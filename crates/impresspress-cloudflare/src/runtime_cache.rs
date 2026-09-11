//! Per-isolate runtime cache. Builds the Wafer once per isolate (sealed, no
//! boot funnel — migrations/seeds happen at deploy via `/_deploy/init`),
//! stores it in a thread_local, and rebuilds when the KV config-version
//! stamp moves. Mirrors impresspress-browser/src/runtime.rs's thread_local
//! pattern; `Rc` handles (not raw pointers) keep an in-flight request's
//! runtime alive across a swap. wasm32 is single-threaded, so the RefCell
//! borrows are never contended — but they are still never held across an
//! `.await` (interleaved fetch events resume at await points).
//!
//! Runtime construction is single-flight per isolate. A request that arrives
//! while another request is building waits on its own Workers timer, then
//! re-checks the cache. It deliberately does not await a shared future: the
//! build performs D1/KV I/O, and a future created by one Workers request must
//! not be polled on behalf of another request.

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::Arc,
};

use impresspress_core::{cache_key::CONFIG_VERSION_KEY, metrics::CacheOutcome};

/// Floor of the isolate-local warm-hit probe window (ms) — see
/// [`next_probe_deadline_ms`].
const PROBE_INTERVAL_FLOOR_MS: u64 = 30_000;
/// Width of the jitter added on top of the floor (ms) — see
/// [`next_probe_deadline_ms`].
const PROBE_INTERVAL_JITTER_MS: u64 = 30_000;
/// Maximum time an isolate-local build slot may remain owned.
///
/// Normal Rust cancellation drops [`BuildGuard`], but Cloudflare can hard-stop
/// a request after it exceeds its CPU allowance. That termination does not
/// guarantee Rust destructors run, so a plain boolean can remain set forever.
/// A later request reclaims the slot after this lease and forces a rebuild.
const BUILD_LEASE_MS: u64 = 5_000;

pub(crate) struct ReadyRuntime {
    pub wafer: wafer_run::Wafer,
    // Deliberately no D1/KV/R2/network/config handles here. The Wafer's six
    // service blocks contain stateless request-scoped proxies; concrete
    // services are selected by `request_services::scope` for each dispatch.
    pub version: String,
    /// Config-version observed when a packaged plan was hydrated. `None`
    /// identifies the ordinary dynamic runtime path, whose `version` already
    /// is the config-version itself.
    config_version: Option<String>,
    /// SHA-256 identity of the request-current Worker version and every Env
    /// value captured by runtime construction. Checked before the zero-await
    /// cache hit so binding/secret-only changes cannot retain stale services
    /// in a reused isolate.
    environment_identity: String,
    /// Absolute wall-clock deadline (ms since epoch, `now_millis()`-scale)
    /// after which the next request in this isolate re-probes the KV
    /// config-version stamp instead of trusting this cached runtime
    /// outright. Reset to a fresh jittered window after every probe (hit
    /// or rebuild). See "Remove the KV read from nearly every warm
    /// request" — Cloudflare KV is already eventually consistent (changes
    /// can take 60s+ to propagate), so probing more often than this floor
    /// buys no real freshness.
    probe_deadline_ms: Cell<u64>,
}

thread_local! {
    static RUNTIME: RefCell<Option<Rc<ReadyRuntime>>> = const { RefCell::new(None) };
    /// Per-isolate cumulative count of runtime builds (cold + rebuild).
    /// Surfaced via `Server-Timing` (`CacheOutcome::build_ordinal`) as a
    /// zero-plumbing proxy for "D1 statements per logical request" — see
    /// `impresspress_core::metrics`'s module doc. A plain `Cell<u32>`
    /// increment; costs nothing on the far more common hit/probed-fresh
    /// paths, which never touch it.
    static BUILD_COUNT: Cell<u32> = const { Cell::new(0) };
    /// True while one request in this isolate is probing/rebuilding the
    /// runtime. Workers can interleave fetch events at `.await` points even
    /// though wasm32 is single-threaded, so a plain thread-local `Cell` is the
    /// correct atomicity boundary: check-and-set contains no await/yield.
    static BUILDING: Cell<bool> = const { Cell::new(false) };
    /// Wall-clock acquisition time for [`BUILDING`]. This makes the slot
    /// recoverable when Cloudflare hard-terminates its owning request before
    /// [`BuildGuard::drop`] can clear it.
    static BUILD_STARTED_MS: Cell<u64> = const { Cell::new(0) };
    /// Monotonic ownership token for the build slot. A reclaimed builder gets
    /// a new token so the expired builder's eventual `Drop` or completion
    /// cannot clear or overwrite its successor (the classic ABA race).
    static BUILD_OWNER: Cell<u64> = const { Cell::new(0) };
    static NEXT_BUILD_OWNER: Cell<u64> = const { Cell::new(0) };
    /// Weak liveness token for the request future that owns the slot. A
    /// legitimately suspended build retains the strong token in its guard,
    /// so elapsed wall time alone can never reclaim it. Cancellation drops
    /// the token; if a platform interruption leaves the scalar slot behind,
    /// a later request may reclaim it after the lease grace period.
    static BUILD_LIVENESS: RefCell<Weak<()>> = RefCell::new(Weak::new());
    /// Set by `KvCachedD1DatabaseService::bump_config_version` /
    /// `force_bump_config_version` (kv_cached_db.rs) immediately after a
    /// LOCAL write to a config-version-bumping table (variables /
    /// block_settings / wrap_grants) in THIS isolate. Forces the next
    /// `get_or_build` call to probe (and rebuild) regardless of the
    /// jittered deadline below — a request that just wrote new config must
    /// not keep serving the pre-write runtime for up to a minute just
    /// because the deadline hasn't elapsed yet. Consumed (cleared) by the
    /// next `get_or_build` call, whether or not that call ends up
    /// rebuilding.
    static DIRTY: Cell<bool> = const { Cell::new(false) };
    /// Once mutable admin state changes, a packaged plan is no longer an
    /// authoritative description of this deployment. Keep that exact
    /// plan/environment pair on the dynamic path for the isolate; a new
    /// Worker version or plan hash gets a fresh chance to use its new plan.
    static PREPARED_BYPASS: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// RAII ownership of the isolate's runtime-build slot. Dropping the future on
/// an error/cancellation also drops this guard, allowing the next request to
/// retry instead of leaving the isolate permanently wedged.
struct BuildGuard {
    owner: u64,
    _liveness: Rc<()>,
}

/// A cold request arrived while another request in the same isolate owns the
/// runtime-build slot. Callers turn this into a short, retryable 503 rather
/// than awaiting request-owned work or leaving a response future pending.
#[derive(Debug)]
pub(crate) struct RuntimeBuildBusy;

impl std::fmt::Display for RuntimeBuildBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("runtime build already in progress")
    }
}

impl std::error::Error for RuntimeBuildBusy {}

impl BuildGuard {
    fn try_acquire(now: u64) -> Option<Self> {
        let owner_alive = BUILD_LIVENESS.with(|token| token.borrow().upgrade().is_some());
        let stale = BUILDING.with(|building| {
            building.get()
                && !owner_alive
                && BUILD_STARTED_MS.with(|started| {
                    let acquired_at = started.get();
                    acquired_at == 0 || now.saturating_sub(acquired_at) >= BUILD_LEASE_MS
                })
        });
        if stale {
            tracing::warn!(
                lease_ms = BUILD_LEASE_MS,
                "reclaiming stale runtime-build slot after an interrupted builder"
            );
            BUILDING.with(|building| building.set(false));
            BUILD_STARTED_MS.with(|started| started.set(0));
            BUILD_OWNER.with(|owner| owner.set(0));
            BUILD_LIVENESS.with(|token| *token.borrow_mut() = Weak::new());
            // If a previous runtime exists, its dirty flag may already have
            // been consumed by the interrupted builder. Force the recovered
            // owner to rebuild instead of accepting that old runtime.
            if cached().is_some() {
                DIRTY.with(|dirty| dirty.set(true));
            }
        }

        BUILDING.with(|building| {
            if building.get() {
                return None;
            }
            building.set(true);
            BUILD_STARTED_MS.with(|started| started.set(now));
            let owner = NEXT_BUILD_OWNER.with(|next| {
                let owner = next.get().wrapping_add(1).max(1);
                next.set(owner);
                owner
            });
            BUILD_OWNER.with(|current| current.set(owner));
            let liveness = Rc::new(());
            BUILD_LIVENESS.with(|token| *token.borrow_mut() = Rc::downgrade(&liveness));
            Some(Self {
                owner,
                _liveness: liveness,
            })
        })
    }

    /// True while this guard still owns the slot. Lease age only permits a
    /// competing request to reclaim; it does not invalidate a long-running
    /// build until such a reclaim actually assigns a new owner token.
    fn is_current(&self) -> bool {
        BUILDING.with(Cell::get) && BUILD_OWNER.with(Cell::get) == self.owner
    }
}

impl Drop for BuildGuard {
    fn drop(&mut self) {
        // An expired owner may resume after another request reclaimed the
        // slot. Only the current token is allowed to release ownership.
        if BUILD_OWNER.with(Cell::get) == self.owner {
            BUILDING.with(|building| building.set(false));
            BUILD_STARTED_MS.with(|started| started.set(0));
            BUILD_OWNER.with(|owner| owner.set(0));
            BUILD_LIVENESS.with(|token| *token.borrow_mut() = Weak::new());
        }
    }
}

fn build_slot_active(now: u64) -> bool {
    BUILDING.with(|building| {
        building.get()
            && (BUILD_LIVENESS.with(|token| token.borrow().upgrade().is_some())
                || BUILD_STARTED_MS.with(|started| {
                    let acquired_at = started.get();
                    acquired_at != 0 && now.saturating_sub(acquired_at) < BUILD_LEASE_MS
                }))
    })
}

/// A request must not await a future or timer whose progress depends on a
/// different Workers request. While a healthy builder owns the slot, warm
/// requests can safely use the last complete runtime; cold requests receive a
/// retryable [`RuntimeBuildBusy`]. A stale slot falls through and is reclaimed
/// by [`BuildGuard::try_acquire`].
fn runtime_while_building(
    now: u64,
    environment_identity: &str,
) -> Result<Option<Rc<ReadyRuntime>>, RuntimeBuildBusy> {
    if !build_slot_active(now) {
        return Ok(None);
    }
    if let Some(rt) = cached() {
        if rt.environment_identity == environment_identity && rt.config_version.is_none() {
            return Ok(Some(rt));
        }
    }
    Err(RuntimeBuildBusy)
}

/// Mark the per-isolate runtime dirty: the next [`get_or_build`] call in
/// this isolate probes the KV config-version stamp — and rebuilds
/// unconditionally, regardless of what that probe reads back — rather than
/// trusting the jittered deadline. See the `DIRTY` thread_local's doc.
pub(crate) fn mark_dirty() {
    DIRTY.with(|d| d.set(true));
}

/// Read and clear the dirty flag.
fn take_dirty() -> bool {
    DIRTY.with(|d| d.replace(false))
}

fn prepared_cache_identity(plan_hash: &str, environment_identity: &str) -> String {
    format!("{plan_hash}\n{environment_identity}")
}

/// Return true only for the exact plan/environment pair previously bypassed.
/// Encountering a new identity clears stale isolate-local bypass state.
fn prepared_is_bypassed(identity: &str) -> bool {
    PREPARED_BYPASS.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_deref() == Some(identity) {
            true
        } else {
            *slot = None;
            false
        }
    })
}

fn bypass_prepared(identity: String) {
    PREPARED_BYPASS.with(|slot| *slot.borrow_mut() = Some(identity));
}

fn prepared_probe_requires_fallback(
    dirty: bool,
    cached_config_version: &str,
    observed_config_version: &str,
) -> bool {
    dirty || cached_config_version != observed_config_version
}

fn prepared_generation_matches(plan_generation: &str, observed_generation: &str) -> bool {
    plan_generation != impresspress_core::UNBOUND_CONFIG_GENERATION
        && plan_generation == observed_generation
}

/// A fresh probe deadline: `now` plus a jittered 30-60s window. Jitter
/// avoids every isolate that warmed at the same instant re-probing KV in
/// lockstep after exactly the same interval.
fn next_probe_deadline_ms(now: u64) -> u64 {
    let mut buf = [0u8; 2];
    let jitter_ms = if getrandom::getrandom(&mut buf).is_ok() {
        u64::from(u16::from_le_bytes(buf)) % PROBE_INTERVAL_JITTER_MS
    } else {
        0
    };
    now + PROBE_INTERVAL_FLOOR_MS + jitter_ms
}

fn cached() -> Option<Rc<ReadyRuntime>> {
    RUNTIME.with(|r| r.borrow().clone())
}

fn store(rt: Rc<ReadyRuntime>) {
    RUNTIME.with(|r| *r.borrow_mut() = Some(rt));
}

fn store_if_current(guard: &BuildGuard, rt: Rc<ReadyRuntime>) -> bool {
    if !guard.is_current() {
        return false;
    }
    store(rt);
    true
}

/// Current KV config-version stamp. Missing key ⇒ stamp a fresh one so all
/// isolates converge on the same generation.
async fn current_version(kv: &Arc<dyn impresspress_core::kv::KvBackend>) -> String {
    match kv.get(CONFIG_VERSION_KEY).await {
        Ok(Some(v)) => v,
        _ => {
            let v = crate::kv_cached_db::new_version_stamp();
            if let Err(e) =
                impresspress_core::kv::put_version_stamp_with_retry(kv.as_ref(), &v).await
            {
                tracing::warn!(error = %e, "config-version stamp persist failed; runtime tagged with local stamp only (KV unstamped; re-mints until a put lands)");
            }
            v
        }
    }
}

/// Return the per-isolate cached runtime, rebuilding it if the KV
/// config-version stamp has moved (or if nothing is cached yet), alongside
/// the [`CacheOutcome`] this call resolved to — a free byproduct of the
/// branches below, consumed by `lib.rs::run` to build the `Server-Timing`
/// response header.
///
/// The `register_blocks` / `register_post_build` hooks are `FnOnce` and are
/// consumed only on the build path; on a cache hit they are dropped unused.
pub(crate) async fn get_or_build<F, G>(
    env: &worker::Env,
    request_config: &std::collections::HashMap<String, String>,
    register_blocks: F,
    register_post_build: G,
) -> Result<(Rc<ReadyRuntime>, CacheOutcome), Box<dyn std::error::Error>>
where
    F: FnOnce(
        crate::ImpresspressBuilder,
    ) -> Result<crate::ImpresspressBuilder, Box<dyn std::error::Error>>,
    G: FnOnce(
        &mut wafer_run::Wafer,
        Arc<dyn wafer_core::interfaces::storage::service::StorageService>,
    ) -> Result<(), Box<dyn std::error::Error>>,
{
    if let Some(plan) = crate::packaged_prepared_runtime_plan(env)? {
        let environment_identity = crate::runtime_environment_identity(env, request_config);
        let prepared_identity = prepared_cache_identity(&plan.plan_hash, &environment_identity);
        if !prepared_is_bypassed(&prepared_identity) {
            return get_or_build_prepared(
                env,
                request_config,
                plan,
                register_blocks,
                register_post_build,
            )
            .await;
        }
    }

    let environment_identity = crate::runtime_environment_identity(env, request_config);

    // Hooks are FnOnce because only the request that acquires the build slot
    // consumes them. Waiters retain their own hooks while sleeping, then drop
    // them unused when the completed runtime is visible.
    let mut register_blocks = Some(register_blocks);
    let mut register_post_build = Some(register_post_build);

    let (probed_version, read_through, is_cold, built_at, build_guard) = loop {
        let now = impresspress_core::util::now_millis();

        // Preserve the zero-await warm path. A dirty or probe-due runtime
        // falls through to the build slot so only one request probes KV and,
        // if needed, rebuilds.
        // Do not take the ordinary hit while any slot is marked owned. A
        // healthy owner is handled as stale-while-revalidate below; an
        // expired owner must reach `try_acquire` so it can be reclaimed and
        // force a rebuild even if the interrupted builder consumed DIRTY.
        if !BUILDING.with(Cell::get) {
            if let Some(rt) = cached() {
                let dirty = DIRTY.with(Cell::get);
                if !dirty
                    && rt.config_version.is_none()
                    && rt.environment_identity == environment_identity
                    && now < rt.probe_deadline_ms.get()
                {
                    return Ok((rt, CacheOutcome::Hit));
                }
            }
        }

        if let Some(rt) = runtime_while_building(now, &environment_identity)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?
        {
            return Ok((rt, CacheOutcome::Hit));
        }

        let build_guard = BuildGuard::try_acquire(now)
            .ok_or_else(|| Box::new(RuntimeBuildBusy) as Box<dyn std::error::Error>)?;

        // Re-check under ownership of the slot. Another request may have
        // completed a build while this one was waiting on its timer.
        let resolution = if let Some(rt) = cached() {
            let dirty = take_dirty();
            let environment_changed = rt.environment_identity != environment_identity;

            if !dirty
                && rt.config_version.is_none()
                && !environment_changed
                && now < rt.probe_deadline_ms.get()
            {
                return Ok((rt, CacheOutcome::Hit));
            }

            // Always derive the probe handle from THIS request's Env. The
            // immutable cached runtime intentionally retains no KV binding.
            let probe_kv = crate::make_kv_backend(env, crate::runner::KV_BINDING)?;
            let version = current_version(&probe_kv).await;

            // A pure deadline-elapsed probe (not dirty) that finds the
            // version unchanged just extends the window — no rebuild needed.
            // A LOCAL write (`dirty`) always rebuilds even if KV still reports
            // the old version because KV is eventually consistent.
            if !dirty
                && rt.config_version.is_none()
                && !environment_changed
                && rt.version == version
            {
                rt.probe_deadline_ms.set(next_probe_deadline_ms(now));
                return Ok((rt, CacheOutcome::ProbedFresh));
            }
            tracing::info!(old = %rt.version, new = %version, dirty, environment_changed, "config version, Worker environment, or local state changed; rebuilding runtime");
            (version, true, false, now, build_guard)
        } else {
            // Cold isolate: probe before build so the finished runtime is
            // tagged with a version no newer than the config it loaded.
            let kv = crate::make_kv_backend(env, crate::runner::KV_BINDING)?;
            (current_version(&kv).await, false, true, now, build_guard)
        };
        break resolution;
    };

    let mut built = crate::build_runtime(
        env,
        request_config,
        None,
        register_blocks
            .take()
            .expect("build hooks are consumed by at most one build attempt"),
        register_post_build
            .take()
            .expect("build hooks are consumed by at most one build attempt"),
        false,
        crate::kv_cached_db::CacheMode {
            read_through,
            bump_on_write: true,
        },
    )
    .await?;

    // Dynamic WRAP grants must be registered before seal. Strictly initialize
    // every slot under the build owner's concrete services before publishing
    // the Wafer: Workers requests must never wait on another request's shared
    // lazy-init mutex/future. The concrete services are dropped instead of
    // entering ReadyRuntime.
    crate::request_services::scope(built.services.clone(), async {
        crate::apply_db_wrap_grants(&mut built).await;
        built.wafer.seal().await.map_err(|e| format!("seal: {e}"))?;
        impresspress_core::builder::strict_init_all_blocks(&built.wafer)
            .await
            .map_err(|error| format!("strict cached-runtime Init: {error}"))
    })
    .await?;
    crate::request_services::scope_sync(built.services.clone(), || {
        impresspress_core::builder::post_start(&built.wafer, &built.storage_block);
    });

    let build_ordinal = BUILD_COUNT.with(|c| {
        let n = c.get() + 1;
        c.set(n);
        n
    });
    let duration_ms = impresspress_core::util::now_millis().saturating_sub(built_at);

    let rt = Rc::new(ReadyRuntime {
        wafer: built.wafer,
        version: probed_version,
        config_version: None,
        environment_identity,
        probe_deadline_ms: Cell::new(next_probe_deadline_ms(built_at)),
    });
    if !store_if_current(&build_guard, rt.clone()) {
        tracing::warn!("discarding runtime built by an expired or superseded owner");
        return Err(Box::new(RuntimeBuildBusy));
    }
    tracing::info!(
        build_ordinal,
        duration_ms,
        cold = is_cold,
        "runtime build complete"
    );
    // `build_guard` remains alive through `store`, so waiters cannot observe
    // BUILDING=false before the completed runtime is visible.
    let outcome = if is_cold {
        CacheOutcome::ColdBuilt {
            build_ordinal,
            duration_ms,
        }
    } else {
        CacheOutcome::Rebuilt {
            build_ordinal,
            duration_ms,
        }
    };
    Ok((rt, outcome))
}

/// Hydrate a packaged immutable plan without D1 settings or WRAP-grant reads.
/// One KV config-version read tags the cold hydration, followed by the same
/// bounded 30–60s probes used by dynamic runtimes. A local dirty signal or a
/// moved version permanently bypasses this plan/environment pair in the
/// isolate and restores dynamic hydration, preserving admin mutation
/// semantics without putting D1 structural reads back on the prepared cold
/// path.
async fn get_or_build_prepared<F, G>(
    env: &worker::Env,
    request_config: &std::collections::HashMap<String, String>,
    plan: Rc<impresspress_core::PreparedRuntimePlan>,
    register_blocks: F,
    register_post_build: G,
) -> Result<(Rc<ReadyRuntime>, CacheOutcome), Box<dyn std::error::Error>>
where
    F: FnOnce(
        crate::ImpresspressBuilder,
    ) -> Result<crate::ImpresspressBuilder, Box<dyn std::error::Error>>,
    G: FnOnce(
        &mut wafer_run::Wafer,
        Arc<dyn wafer_core::interfaces::storage::service::StorageService>,
    ) -> Result<(), Box<dyn std::error::Error>>,
{
    let environment_identity = crate::runtime_environment_identity(env, request_config);
    let plan_generation = plan.plan_hash.clone();
    let prepared_identity = prepared_cache_identity(&plan_generation, &environment_identity);
    let now = impresspress_core::util::now_millis();

    if !BUILDING.with(Cell::get) {
        if let Some(rt) = cached() {
            let dirty = DIRTY.with(Cell::get);
            if !dirty
                && rt.environment_identity == environment_identity
                && rt.version == plan_generation
                && rt.config_version.is_some()
                && now < rt.probe_deadline_ms.get()
            {
                return Ok((rt, CacheOutcome::Hit));
            }
        }
    }

    if build_slot_active(now) {
        if let Some(rt) = cached() {
            if rt.environment_identity == environment_identity
                && rt.version == plan_generation
                && rt.config_version.is_some()
            {
                return Ok((rt, CacheOutcome::Hit));
            }
        }
        return Err(Box::new(RuntimeBuildBusy));
    }

    let build_guard = BuildGuard::try_acquire(now)
        .ok_or_else(|| Box::new(RuntimeBuildBusy) as Box<dyn std::error::Error>)?;
    if let Some(rt) = cached() {
        if rt.environment_identity == environment_identity && rt.version == plan_generation {
            if let Some(cached_config_version) = rt.config_version.as_deref() {
                let dirty = take_dirty();
                if !dirty && now < rt.probe_deadline_ms.get() {
                    return Ok((rt, CacheOutcome::Hit));
                }

                let probe_kv = crate::make_kv_backend(env, crate::runner::KV_BINDING)?;
                let observed_config_version = current_version(&probe_kv).await;
                if !prepared_probe_requires_fallback(
                    dirty,
                    cached_config_version,
                    &observed_config_version,
                ) {
                    rt.probe_deadline_ms.set(next_probe_deadline_ms(now));
                    return Ok((rt, CacheOutcome::ProbedFresh));
                }

                tracing::info!(
                    plan_hash = %plan_generation,
                    old_config_version = %cached_config_version,
                    new_config_version = %observed_config_version,
                    dirty,
                    "mutable admin state changed; bypassing packaged plan for this isolate"
                );
                bypass_prepared(prepared_identity);
                drop(build_guard);
                // Force the dynamic path to replace the currently cached prepared
                // runtime even when an eventually-consistent KV read still
                // returns the old stamp after a local write.
                mark_dirty();
                return Box::pin(get_or_build(
                    env,
                    request_config,
                    register_blocks,
                    register_post_build,
                ))
                .await;
            }
        }
    }
    let is_cold = cached().is_none();
    // A plan/Worker identity change supersedes dirty state belonging to the
    // prior runtime. The new plan is tagged with the current KV generation.
    let _ = take_dirty();
    let kv = crate::make_kv_backend(env, crate::runner::KV_BINDING)?;
    let config_version = current_version(&kv).await;
    if !prepared_generation_matches(&plan.config_generation, &config_version) {
        tracing::info!(
            plan_hash = %plan_generation,
            plan_config_generation = %plan.config_generation,
            observed_config_generation = %config_version,
            "prepared plan generation is stale; bypassing it for this isolate"
        );
        bypass_prepared(prepared_identity);
        drop(build_guard);
        return Box::pin(get_or_build(
            env,
            request_config,
            register_blocks,
            register_post_build,
        ))
        .await;
    }

    let mut built = crate::build_runtime(
        env,
        request_config,
        Some(plan.as_ref()),
        register_blocks,
        register_post_build,
        false,
        crate::kv_cached_db::CacheMode::default(),
    )
    .await?;

    // Grants and settings were imported from the verified plan. Seal and
    // strictly initialize every slot under the build owner's request services
    // before publishing the Wafer. ConfigSource may still perform per-block
    // reads; keeping them here prevents cross-request lazy-init waiters.
    crate::request_services::scope(built.services.clone(), async {
        built.wafer.seal().await.map_err(|e| format!("seal: {e}"))?;
        impresspress_core::builder::strict_init_all_blocks(&built.wafer)
            .await
            .map_err(|error| format!("strict prepared-runtime Init: {error}"))
    })
    .await?;
    crate::request_services::scope_sync(built.services.clone(), || {
        impresspress_core::builder::post_start(&built.wafer, &built.storage_block);
    });

    let build_ordinal = BUILD_COUNT.with(|count| {
        let next = count.get() + 1;
        count.set(next);
        next
    });
    let duration_ms = impresspress_core::util::now_millis().saturating_sub(now);
    let rt = Rc::new(ReadyRuntime {
        wafer: built.wafer,
        version: plan_generation,
        config_version: Some(plan.config_generation.clone()),
        environment_identity,
        probe_deadline_ms: Cell::new(next_probe_deadline_ms(now)),
    });
    if !store_if_current(&build_guard, rt.clone()) {
        tracing::warn!(
            prepared = true,
            "discarding runtime built by an expired or superseded owner"
        );
        return Err(Box::new(RuntimeBuildBusy));
    }
    tracing::info!(
        build_ordinal,
        duration_ms,
        cold = is_cold,
        prepared = true,
        "prepared runtime hydration complete"
    );
    let outcome = if is_cold {
        CacheOutcome::ColdBuilt {
            build_ordinal,
            duration_ms,
        }
    } else {
        CacheOutcome::Rebuilt {
            build_ordinal,
            duration_ms,
        }
    };
    Ok((rt, outcome))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset_build_slot() {
        BUILDING.with(|value| value.set(false));
        BUILD_STARTED_MS.with(|value| value.set(0));
        BUILD_OWNER.with(|value| value.set(0));
        NEXT_BUILD_OWNER.with(|value| value.set(0));
        BUILD_LIVENESS.with(|value| *value.borrow_mut() = Weak::new());
        DIRTY.with(|value| value.set(false));
    }

    #[test]
    fn expired_owner_cannot_clear_reclaimer_slot_or_store() {
        reset_build_slot();
        let owner_a = BuildGuard::try_acquire(100).unwrap();
        // Simulate a platform interruption that orphaned scalar slot state,
        // while retaining the old guard to exercise an eventual late resume.
        BUILD_LIVENESS.with(|value| *value.borrow_mut() = Weak::new());
        let owner_b = BuildGuard::try_acquire(100 + BUILD_LEASE_MS).unwrap();

        assert!(!owner_a.is_current());
        assert!(owner_b.is_current());
        drop(owner_a);
        assert!(BUILDING.with(Cell::get));
        assert!(owner_b.is_current());

        drop(owner_b);
        assert!(!BUILDING.with(Cell::get));
    }

    #[test]
    fn live_long_build_is_never_reclaimed_by_repeated_arrivals() {
        reset_build_slot();
        let owner = BuildGuard::try_acquire(100).unwrap();
        // The old five-second age threshold may pass many times. Every
        // arrival still sees the live request token and leaves it alone.
        assert!(owner.is_current());
        assert!(build_slot_active(100 + BUILD_LEASE_MS - 1));
        assert!(build_slot_active(100 + BUILD_LEASE_MS));
        assert!(BuildGuard::try_acquire(100 + BUILD_LEASE_MS).is_none());
        assert!(BuildGuard::try_acquire(100 + BUILD_LEASE_MS * 10).is_none());
        assert!(owner.is_current());
        drop(owner);
        assert!(!BUILDING.with(Cell::get));
    }

    #[test]
    fn orphaned_slot_waits_for_grace_then_recovers() {
        reset_build_slot();
        BUILDING.with(|value| value.set(true));
        BUILD_STARTED_MS.with(|value| value.set(100));
        BUILD_OWNER.with(|value| value.set(7));
        BUILD_LIVENESS.with(|value| *value.borrow_mut() = Weak::new());

        assert!(BuildGuard::try_acquire(100 + BUILD_LEASE_MS - 1).is_none());
        let recovered = BuildGuard::try_acquire(100 + BUILD_LEASE_MS).unwrap();
        assert!(recovered.is_current());
    }

    #[test]
    fn local_dirty_forces_prepared_fallback_even_before_kv_converges() {
        assert!(prepared_probe_requires_fallback(true, "v1", "v1"));
        assert!(!prepared_probe_requires_fallback(false, "v1", "v1"));
    }

    #[test]
    fn moved_config_version_forces_prepared_fallback() {
        assert!(prepared_probe_requires_fallback(false, "v1", "v2"));
    }

    #[test]
    fn second_isolate_rejects_v1_plan_after_generation_moves_to_v2() {
        let v1 = "1".repeat(32);
        let v2 = "2".repeat(32);
        // Isolate A started while the candidate's generation was current.
        assert!(prepared_generation_matches(&v1, &v1));
        // A later admin/deploy mutation moves KV. A fresh isolate must not
        // hydrate the older packaged v1 structure.
        assert!(!prepared_generation_matches(&v1, &v2));
        // The replacement Worker plan is accepted by another fresh isolate.
        assert!(prepared_generation_matches(&v2, &v2));
        assert!(!prepared_generation_matches(
            impresspress_core::UNBOUND_CONFIG_GENERATION,
            impresspress_core::UNBOUND_CONFIG_GENERATION,
        ));
    }

    #[test]
    fn bypass_is_scoped_to_exact_plan_and_environment_identity() {
        PREPARED_BYPASS.with(|slot| *slot.borrow_mut() = None);
        let old = prepared_cache_identity("plan-a", "worker-a");
        bypass_prepared(old.clone());
        assert!(prepared_is_bypassed(&old));

        let replacement = prepared_cache_identity("plan-b", "worker-a");
        assert!(!prepared_is_bypassed(&replacement));
        assert!(!prepared_is_bypassed(&old));
    }

    #[test]
    fn environment_change_produces_a_distinct_prepared_cache_identity() {
        let original = prepared_cache_identity("plan-a", "worker-env-v1");
        let changed = prepared_cache_identity("plan-a", "worker-env-v2");
        assert_ne!(original, changed);

        PREPARED_BYPASS.with(|slot| *slot.borrow_mut() = None);
        bypass_prepared(original.clone());
        assert!(prepared_is_bypassed(&original));
        assert!(!prepared_is_bypassed(&changed));
    }
}
