//! The one post-`build()` lifecycle for every target: [`boot`], its
//! [`BootHooks`] seam, the [`InitPolicy`] that decides what a failure *does*,
//! the [`GrantSource`] that decides where the runtime's WRAP grants come from,
//! and the native-embedding `register_vector_block` helper.
//!
//! Before this module owned the whole sequence there were four hand-maintained
//! orderings — the tolerant native/browser funnel, the reported deploy funnel,
//! and two Cloudflare request-path copies that differed from each other over
//! whether WRAP grants were applied and from both funnels over whether the
//! seed hook ran at all. Every step below is therefore a *parameter* of one
//! function rather than a line a caller has to remember to copy.

use std::sync::Arc;

use serde::Serialize;
use wafer_core::interfaces::database::service::DatabaseService;
use wafer_run::{RuntimeError, Wafer};

use crate::blocks::storage::ImpresspressStorageBlock;

/// Request-current config flag set only while an authenticated deployment
/// candidate is exporting a prepared runtime plan.
///
/// Consumer blocks whose `Init` lifecycle also publishes mutable derived
/// output (for example a pre-rendered homepage) should still run migrations
/// and seeds, but defer that publication while this value is `"1"`. This
/// keeps the currently promoted Worker and its mutable objects unchanged
/// until the prepared candidate has passed verification and promotion.
pub const PREPARE_RUNTIME_PLAN_KEY: &str = "IMPRESSPRESS_PREPARE_RUNTIME_PLAN";

/// What a block `Init` failure (or a seed-hook failure) *does*. The ordering
/// [`boot`] runs is identical under all three; only the consequence differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitPolicy {
    /// Log and continue. The runtime is published even with a broken block, so
    /// one misconfigured block cannot wedge a whole server. Used by the
    /// long-lived targets (native, browser), which can be inspected and fixed
    /// in place.
    Tolerant,
    /// Fail closed on the first failure. Used by the Cloudflare request path:
    /// publishing a runtime with a failed lazy-init slot would let concurrent
    /// requests wait on one another's init future, which is not a valid
    /// execution model for a request-isolated platform.
    Strict,
    /// Capture every outcome and keep going, so the caller can render the full
    /// picture. Used by `/_deploy/init`, whose whole product is the report.
    Reported,
}

/// Where this runtime's deployment-owned WRAP grants come from.
///
/// Grants registered after [`Wafer::seal`] are ignored, so this is a step that
/// has to happen inside the funnel and cannot be a follow-up call. Making it a
/// required argument is the point: the Cloudflare prepared-hydration path used
/// to differ from its two sibling paths purely by *omitting* the grant call,
/// and nothing but a comment recorded that the omission was deliberate.
pub enum GrantSource<'a> {
    /// Load admin-created grants from the platform database and register them
    /// before seal. Missing table / read errors degrade to no dynamic grants —
    /// see [`crate::platform_state::wrap_grants::load`].
    Database(&'a Arc<dyn DatabaseService>),
    /// Every grant this runtime gets was already installed by the builder,
    /// via [`super::ImpresspressBuilder::wrap_grants`] or a verified prepared
    /// plan. The `&'static str` is the reason, recorded at the call site
    /// rather than in a comment next to a call that is not there.
    PreInstalled(&'static str),
}

#[derive(Debug, Serialize)]
pub struct StepOutcome {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BlockInitOutcome {
    pub block: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Per-step outcome of one [`boot`] call. Serialized verbatim as the
/// `/_deploy/init` response body, so its field names are a wire contract with
/// `impresspress deploy` (`cli::helpers::cloudflare::prepared`).
///
/// [`InitPolicy::Tolerant`] and [`InitPolicy::Strict`] callers may ignore it:
/// under `Strict` a failure is an `Err` instead, and under `Tolerant` the same
/// failures are logged.
#[derive(Debug, Serialize)]
pub struct BootReport {
    pub sealed: bool,
    pub seed: StepOutcome,
    pub blocks: Vec<BlockInitOutcome>,
    /// True iff every step and every block init succeeded.
    pub ok: bool,
}

/// Call after `wafer.seal()` to inject collected WRAP grants into the storage
/// block for cross-block access control. Private: [`boot`] is the only caller,
/// which is what stops it from being a step a target can forget.
fn post_start(wafer: &Wafer, storage_block: &ImpresspressStorageBlock) {
    storage_block.update_wrap_grants(wafer.wrap_grants());
}

/// Per-target boot I/O for [`boot`]. Implemented by each platform to supply
/// the one step that genuinely differs between them: what to seed and which
/// shared snapshots to publish into once the admin block's `Init` has created
/// the variables / block_settings tables.
///
/// Everything else around it — the invariant `grants → seal → admin-first init
/// → seed → init the rest → post_start` ordering — is owned by [`boot`].
///
/// Deliberately **not** an `Option` argument on [`boot`]: a target with
/// nothing to seed writes a no-op impl and says why (native's
/// `NativeBootHooks`), which is a decision a reader can find. A `None` is a
/// decision nobody has to write down, and skipping the seed by accident on one
/// of several paths is exactly the defect this funnel exists to close.
#[wafer_block::wafer_async_trait]
pub trait BootHooks {
    /// Runs AFTER `init_block(admin)` (so the admin migration has created the
    /// `impresspress__admin__variables` + `block_settings` tables) and BEFORE
    /// the remaining blocks initialize (so a block depending on a seeded
    /// `auto_generate` key can't lose the `HashMap::keys()` race and
    /// permanent-fail on a missing secret — the impresspress #209 regression
    /// class).
    ///
    /// Implementations call [`crate::platform_state::variables::seed_auto_generated`] /
    /// [`crate::platform_state::variables::seed_and_load`] /
    /// [`crate::platform_state::block_settings::load_and_seed`] as appropriate and
    /// publish the results into the shared `ConfigService` / `BlockSettings`
    /// handle / crypto secret the runtime already holds — through
    /// [`super::RuntimeConfig::republish`], so both config surfaces move
    /// together.
    ///
    /// Under [`InitPolicy::Tolerant`] and [`InitPolicy::Strict`] an `Err`
    /// aborts the boot; under [`InitPolicy::Reported`] it is captured in
    /// [`BootReport::seed`]. Return `Err` for a genuinely fatal condition (for
    /// example, structural settings could not be persisted consistently, or a
    /// required secret cannot be read). Optional best-effort per-key seeds
    /// should still log and continue inside the implementation.
    ///
    /// Receives `&mut Wafer` so a target that could not know its settings
    /// before admin migration (browser/Cloudflare first deploy) can publish the
    /// seeded values into the config snapshot before any other block initializes.
    async fn seed_after_admin_init(&self, wafer: &mut Wafer) -> Result<(), String>;
}

/// The one post-`build()` lifecycle, for every target:
///
/// 1. `grants` — register deployment-owned WRAP grants. MUST precede `seal`.
/// 2. `seal()` — finalize composite/uses/capability/snapshot wiring.
/// 3. `init_block(admin)` FIRST — admin's migrations create the variables /
///    block_settings tables before any other block's `Init` writes to them,
///    and before the seed step reads them.
/// 4. `hooks.seed_after_admin_init` — seed + publish (see [`BootHooks`]).
/// 5. every remaining block, in `Wafer::block_names()` order (sorted, so the
///    sequence is identical across processes and platforms).
/// 6. `post_start()` — inject WRAP grants into the storage block.
///
/// `policy` changes none of that ordering; it decides only what a failure at
/// step 3, 4 or 5 does. See [`InitPolicy`].
///
/// The caller must have already wired the pre-seal bits its platform needs
/// (`set_asset_loader`, any post-build block registration) onto `wafer` before
/// calling this; the config surfaces are the builder's (see
/// [`super::RuntimeConfig`]). After it returns, the caller dispatches requests
/// / stores the runtime handle as appropriate.
///
/// Native uses this funnel too, then runs the native-only
/// [`Wafer::run_start_lifecycle`] + [`Wafer::bind_all`] steps afterwards: its
/// HTTP-listener block binds the TCP socket in the `Start`-lifecycle `bind()`
/// pass, which the stateless targets omit (they dispatch per-request via
/// `wafer.run`).
pub async fn boot(
    wafer: &mut Wafer,
    storage_block: &ImpresspressStorageBlock,
    hooks: &dyn BootHooks,
    grants: GrantSource<'_>,
    policy: InitPolicy,
) -> Result<BootReport, RuntimeError> {
    // 1. Deployment-owned WRAP grants, before seal — `add_wrap_grants` after
    //    seal is silently ignored.
    match grants {
        GrantSource::Database(db) => {
            let loaded = crate::platform_state::wrap_grants::load(db).await;
            if !loaded.is_empty() {
                tracing::info!(count = loaded.len(), "registering database WRAP grants");
                wafer.add_wrap_grants(loaded);
            }
        }
        GrantSource::PreInstalled(_because) => {}
    }

    // 2. Seal.
    wafer.seal().await?;

    let admin_id = crate::blocks::admin::ADMIN_BLOCK_ID;
    let names = wafer.block_names();
    let mut blocks = Vec::new();

    // 3. Admin first — its Init creates the variables / block_settings tables
    //    the seed step reads, and migration 002's `block` column the auto-gen
    //    seeder writes.
    if names.iter().any(|name| name == admin_id) {
        blocks.push(init_one(wafer, admin_id, policy).await?);
    }

    // 4. Seed + publish.
    let seed = match hooks.seed_after_admin_init(wafer).await {
        Ok(()) => StepOutcome {
            ok: true,
            error: None,
        },
        Err(e) => match policy {
            InitPolicy::Reported => StepOutcome {
                ok: false,
                error: Some(e),
            },
            InitPolicy::Tolerant | InitPolicy::Strict => return Err(RuntimeError::Config(e)),
        },
    };

    // 5. Every remaining block. Admin is a slot-cached no-op on a second pass,
    //    so it is skipped rather than reported twice. Iterates the registration
    //    keys (`block_names`) — the exact set `init_block` resolves — rather
    //    than each block's self-reported `info().name`.
    for name in &names {
        if name == admin_id {
            continue;
        }
        blocks.push(init_one(wafer, name, policy).await?);
    }

    // 6. WRAP grants into the storage block.
    post_start(wafer, storage_block);

    let ok = seed.ok && blocks.iter().all(|b| b.ok);
    Ok(BootReport {
        sealed: true,
        seed,
        blocks,
        ok,
    })
}

/// Initialize one block and turn the outcome into what `policy` asks for.
/// `Strict` is the only arm that can return `Err`.
async fn init_one(
    wafer: &Wafer,
    name: &str,
    policy: InitPolicy,
) -> Result<BlockInitOutcome, RuntimeError> {
    match wafer.init_block(name).await {
        Ok(_) => Ok(BlockInitOutcome {
            block: name.to_string(),
            ok: true,
            error: None,
        }),
        Err(e) => match policy {
            InitPolicy::Strict => Err(RuntimeError::Config(format!(
                "block `{name}` Init failed: {e}"
            ))),
            InitPolicy::Tolerant => {
                tracing::error!(
                    block = %name,
                    error = %e,
                    "block init lifecycle failed during boot",
                );
                Ok(BlockInitOutcome {
                    block: name.to_string(),
                    ok: false,
                    error: Some(e.to_string()),
                })
            }
            InitPolicy::Reported => Ok(BlockInitOutcome {
                block: name.to_string(),
                ok: false,
                error: Some(e.to_string()),
            }),
        },
    }
}

/// Register the `wafer-run/vector` runtime block backed by native
/// `SqliteVecService` + `FastembedService`.
///
/// - Opens a dedicated `rusqlite::Connection` at `db_path`. SQLite supports
///   multi-connection access with WAL, so sharing the DB file with the
///   platform's `DatabaseService` connection is safe.
/// - `FastembedService::default_model()` triggers an ONNX model download on
///   first run. Failure is logged but does not abort startup — the vector
///   runtime block simply won't be registered, and any attempt to use it
///   will fail via the normal dependency-resolution path.
///
/// This function is only compiled when the `native-embedding` feature is on;
/// the `impresspress/vector` feature block registration in `impresspress-core` is
/// gated by the same feature so the two stay in sync.
#[cfg(feature = "native-embedding")]
pub(super) fn register_vector_block(
    wafer: &mut Wafer,
    db_path: Option<&str>,
) -> Result<(), RuntimeError> {
    use wafer_block_fastembed::FastembedService;
    use wafer_block_sqlite::vector::SqliteVecService;
    use wafer_core::interfaces::vector::service::{EmbeddingService, VectorService};

    let Some(db_path) = db_path else {
        return Err(RuntimeError::Config(
            "native-embedding feature is enabled but no sqlite_db_path was \
             provided to ImpresspressBuilder — call .sqlite_db_path(...) before \
             .build()"
                .to_string(),
        ));
    };

    // Dedicated connection for the vector service — see module docs on
    // `sqlite_db_path` for why a second connection is fine.
    let vec_conn = rusqlite::Connection::open(db_path).map_err(|e| {
        RuntimeError::Config(format!(
            "failed to open SQLite connection at '{db_path}' for vector service: {e}"
        ))
    })?;
    let vec_svc: Arc<dyn VectorService> = Arc::new(SqliteVecService::new(vec_conn));

    let emb_svc: Arc<dyn EmbeddingService> = match FastembedService::default_model() {
        Ok(svc) => Arc::new(svc),
        Err(e) => {
            // Model download can fail offline or on first-run with restricted
            // egress. Log and skip registration so the rest of the runtime
            // boots; `impresspress/vector` registration will fail dep resolution
            // with a clearer error than a half-wired block would.
            tracing::warn!(
                error = ?e,
                "fastembed model unavailable — skipping wafer-run/vector registration"
            );
            return Ok(());
        }
    };

    wafer_core::service_blocks::vector::register_with(wafer, vec_svc, emb_svc)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use wafer_block::{
        core_types::{ErrorCode, LifecycleEvent, LifecycleType, Message, WaferError},
        streams::{input::InputStream, output::OutputStream},
        Block, BlockInfo,
    };
    use wafer_core::interfaces::database::service::DatabaseService;
    use wafer_run::{StaticConfigSource, Wafer};

    use super::*;
    use crate::blocks::storage::ImpresspressStorageBlock;

    struct InitProbeBlock {
        name: &'static str,
        order: Arc<Mutex<Vec<String>>>,
        fail: bool,
    }

    #[wafer_block::wafer_async_trait]
    impl Block for InitProbeBlock {
        fn info(&self) -> BlockInfo {
            BlockInfo::new(self.name, "0.1.0", "test/init@v1", "test")
        }

        async fn lifecycle(
            &self,
            _ctx: &dyn wafer_block::context::Context,
            event: LifecycleEvent,
        ) -> Result<(), WaferError> {
            if event.event_type == LifecycleType::Init {
                push(&self.order, self.name);
                if self.fail {
                    return Err(WaferError::new(
                        ErrorCode::Unknown,
                        "deliberate Init failure",
                    ));
                }
            }
            Ok(())
        }

        async fn handle(
            &self,
            _ctx: &dyn wafer_block::context::Context,
            _msg: Message,
            _input: InputStream,
        ) -> OutputStream {
            OutputStream::respond(Vec::new())
        }
    }

    fn push(order: &Arc<Mutex<Vec<String>>>, entry: &str) {
        order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(entry.to_string());
    }

    fn taken(order: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Records the moment `seed_after_admin_init` ran into the same ordering
    /// log the probe blocks write to, so a test can assert where in the
    /// sequence it landed. `fail` makes the hook return `Err`.
    struct ProbeHooks {
        order: Arc<Mutex<Vec<String>>>,
        fail: bool,
    }

    #[wafer_block::wafer_async_trait]
    impl BootHooks for ProbeHooks {
        async fn seed_after_admin_init(&self, _wafer: &mut Wafer) -> Result<(), String> {
            push(&self.order, "seed");
            if self.fail {
                return Err("deliberate seed failure".to_string());
            }
            Ok(())
        }
    }

    fn register_probe(
        wafer: &mut Wafer,
        name: &'static str,
        order: &Arc<Mutex<Vec<String>>>,
        fail: bool,
    ) {
        wafer
            .register_block(
                name,
                Arc::new(InitProbeBlock {
                    name,
                    order: order.clone(),
                    fail,
                }),
            )
            .unwrap();
    }

    fn empty_wafer() -> Wafer {
        let config: Arc<dyn wafer_run::ConfigSource> = Arc::new(StaticConfigSource::default());
        Wafer::new(config).unwrap()
    }

    /// A storage block to satisfy `post_start`. Its grant list is what
    /// `post_start` writes into, so the WRAP-grant tests read it back.
    fn storage_block() -> Arc<ImpresspressStorageBlock> {
        crate::blocks::storage::create(
            Arc::new(crate::test_support::InMemoryStorageService::new()),
            Arc::from(crate::blocks::admin::ADMIN_BLOCK_ID),
        )
    }

    /// Three probes plus the admin id, one of which optionally fails.
    fn wafer_with_probes(order: &Arc<Mutex<Vec<String>>>, failing: Option<&'static str>) -> Wafer {
        let mut wafer = empty_wafer();
        register_probe(&mut wafer, "test/zeta", order, failing == Some("test/zeta"));
        register_probe(
            &mut wafer,
            crate::blocks::admin::ADMIN_BLOCK_ID,
            order,
            failing == Some(crate::blocks::admin::ADMIN_BLOCK_ID),
        );
        register_probe(
            &mut wafer,
            "test/alpha",
            order,
            failing == Some("test/alpha"),
        );
        wafer
    }

    async fn bare_db() -> Arc<dyn DatabaseService> {
        Arc::new(
            wafer_block_sqlite::service::SQLiteDatabaseService::open_in_memory()
                .expect("open in-memory sqlite"),
        )
    }

    #[tokio::test]
    async fn ordering_is_admin_first_then_seed_then_the_rest_sorted() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut wafer = wafer_with_probes(&order, None);
        let storage = storage_block();

        let report = boot(
            &mut wafer,
            &storage,
            &ProbeHooks {
                order: order.clone(),
                fail: false,
            },
            GrantSource::PreInstalled("test fixture declares none"),
            InitPolicy::Tolerant,
        )
        .await
        .expect("boot");

        assert_eq!(
            taken(&order),
            vec![
                crate::blocks::admin::ADMIN_BLOCK_ID.to_string(),
                "seed".to_string(),
                "test/alpha".to_string(),
                "test/zeta".to_string(),
            ],
        );
        assert!(report.ok);
        assert!(report.sealed);
        assert_eq!(
            report
                .blocks
                .iter()
                .filter(|b| b.block == crate::blocks::admin::ADMIN_BLOCK_ID)
                .count(),
            1,
            "admin initializes first and is reported exactly once, not again in the tail",
        );
    }

    /// The seed hook runs under EVERY policy, including the `Strict` one the
    /// Cloudflare request path uses. Before this funnel existed, that path
    /// called `strict_init_all_blocks` directly and no `BootHooks` value
    /// reached it at all, so `seed_after_admin_init` was skipped on every
    /// Cloudflare request build (ruling 5.5).
    #[tokio::test]
    async fn every_policy_runs_the_seed_hook_after_admin() {
        for policy in [
            InitPolicy::Tolerant,
            InitPolicy::Strict,
            InitPolicy::Reported,
        ] {
            let order = Arc::new(Mutex::new(Vec::new()));
            let mut wafer = wafer_with_probes(&order, None);
            let storage = storage_block();

            boot(
                &mut wafer,
                &storage,
                &ProbeHooks {
                    order: order.clone(),
                    fail: false,
                },
                GrantSource::PreInstalled("test fixture declares none"),
                policy,
            )
            .await
            .unwrap_or_else(|e| panic!("boot under {policy:?}: {e}"));

            let log = taken(&order);
            assert_eq!(
                log.first().map(String::as_str),
                Some(crate::blocks::admin::ADMIN_BLOCK_ID),
                "{policy:?}",
            );
            assert_eq!(log.get(1).map(String::as_str), Some("seed"), "{policy:?}");
        }
    }

    #[tokio::test]
    async fn tolerant_logs_a_failure_and_still_initializes_later_blocks() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut wafer = wafer_with_probes(&order, Some("test/alpha"));
        let storage = storage_block();

        let report = boot(
            &mut wafer,
            &storage,
            &ProbeHooks {
                order: order.clone(),
                fail: false,
            },
            GrantSource::PreInstalled("test fixture declares none"),
            InitPolicy::Tolerant,
        )
        .await
        .expect("tolerant boot returns Ok even with a broken block");

        assert!(!report.ok);
        assert!(
            taken(&order).contains(&"test/zeta".to_string()),
            "the block after the failing one must still initialize",
        );
    }

    #[tokio::test]
    async fn strict_fails_closed_without_initializing_later_blocks() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut wafer = wafer_with_probes(&order, Some("test/alpha"));
        let storage = storage_block();

        let error = boot(
            &mut wafer,
            &storage,
            &ProbeHooks {
                order: order.clone(),
                fail: false,
            },
            GrantSource::PreInstalled("test fixture declares none"),
            InitPolicy::Strict,
        )
        .await
        .expect_err("strict boot must fail closed");

        assert!(error.to_string().contains("test/alpha"), "{error}");
        assert!(
            !taken(&order).contains(&"test/zeta".to_string()),
            "no block after the failing one may initialize",
        );
    }

    #[tokio::test]
    async fn reported_captures_every_block_and_keeps_going() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut wafer = wafer_with_probes(&order, Some("test/alpha"));
        let storage = storage_block();

        let report = boot(
            &mut wafer,
            &storage,
            &ProbeHooks {
                order: order.clone(),
                fail: false,
            },
            GrantSource::PreInstalled("test fixture declares none"),
            InitPolicy::Reported,
        )
        .await
        .expect("reported boot returns Ok");

        assert!(!report.ok);
        let failed: Vec<&str> = report
            .blocks
            .iter()
            .filter(|b| !b.ok)
            .map(|b| b.block.as_str())
            .collect();
        assert_eq!(failed, vec!["test/alpha"]);
        assert!(
            report.blocks.iter().any(|b| b.block == "test/zeta" && b.ok),
            "the block after the failing one is still attempted and reported",
        );
    }

    #[tokio::test]
    async fn seed_failure_is_fatal_except_under_reported() {
        for policy in [InitPolicy::Tolerant, InitPolicy::Strict] {
            let order = Arc::new(Mutex::new(Vec::new()));
            let mut wafer = wafer_with_probes(&order, None);
            let storage = storage_block();

            let error = boot(
                &mut wafer,
                &storage,
                &ProbeHooks {
                    order: order.clone(),
                    fail: true,
                },
                GrantSource::PreInstalled("test fixture declares none"),
                policy,
            )
            .await
            .expect_err(&format!("seed failure must abort under {policy:?}"));
            assert!(error.to_string().contains("deliberate seed failure"));
        }

        let order = Arc::new(Mutex::new(Vec::new()));
        let mut wafer = wafer_with_probes(&order, None);
        let storage = storage_block();
        let report = boot(
            &mut wafer,
            &storage,
            &ProbeHooks {
                order: order.clone(),
                fail: true,
            },
            GrantSource::PreInstalled("test fixture declares none"),
            InitPolicy::Reported,
        )
        .await
        .expect("reported boot captures a seed failure");
        assert!(!report.seed.ok);
        assert!(!report.ok);
        assert_eq!(
            report.seed.error.as_deref(),
            Some("deliberate seed failure")
        );
    }

    /// `GrantSource::Database` and `GrantSource::PreInstalled` (over the same
    /// rows, installed through the builder) must leave the runtime holding the
    /// same grants — otherwise the choice would be a behavioural fork rather
    /// than a statement of provenance.
    #[tokio::test]
    async fn database_and_pre_installed_grants_agree() {
        let db = bare_db().await;
        crate::migration_helper::apply_ddl_via_service(
            &db,
            crate::blocks::admin::migrations::ddl_files("sqlite"),
        )
        .await
        .expect("apply admin migrations");
        crate::platform_state::wrap_grants::seed_fixture_grant(&db).await;

        let order = Arc::new(Mutex::new(Vec::new()));
        let mut from_db = wafer_with_probes(&order, None);
        let storage = storage_block();
        boot(
            &mut from_db,
            &storage,
            &ProbeHooks {
                order: order.clone(),
                fail: false,
            },
            GrantSource::Database(&db),
            InitPolicy::Tolerant,
        )
        .await
        .expect("boot from database grants");

        let expected = crate::platform_state::wrap_grants::load(&db).await;
        assert_eq!(expected.len(), 1, "fixture seeds exactly one grant");

        let order2 = Arc::new(Mutex::new(Vec::new()));
        let mut pre_installed = wafer_with_probes(&order2, None);
        pre_installed.add_wrap_grants(expected.clone());
        let storage2 = storage_block();
        boot(
            &mut pre_installed,
            &storage2,
            &ProbeHooks {
                order: order2,
                fail: false,
            },
            GrantSource::PreInstalled("installed above, standing in for the builder"),
            InitPolicy::Tolerant,
        )
        .await
        .expect("boot with pre-installed grants");

        assert_eq!(
            format!("{:?}", from_db.wrap_grants()),
            format!("{:?}", pre_installed.wrap_grants()),
        );
        assert!(
            format!("{:?}", from_db.wrap_grants())
                .contains(crate::platform_state::wrap_grants::FIXTURE_RESOURCE),
            "the seeded grant must reach the sealed runtime",
        );
        // Step 6: the same grants reach the storage block, which is what makes
        // a cross-block read succeed at request time. Nothing tested this
        // before, because it was a statement each target copied.
        assert!(
            format!("{:?}", storage.installed_wrap_grants())
                .contains(crate::platform_state::wrap_grants::FIXTURE_RESOURCE),
            "boot must close by injecting the sealed grants into the storage block",
        );
    }

    /// `GrantSource::PreInstalled` must not go looking in a database — that is
    /// the whole reason the prepared Cloudflare hydration can stay off D1.
    #[tokio::test]
    async fn pre_installed_does_not_read_the_database() {
        let db = bare_db().await;
        crate::migration_helper::apply_ddl_via_service(
            &db,
            crate::blocks::admin::migrations::ddl_files("sqlite"),
        )
        .await
        .expect("apply admin migrations");
        crate::platform_state::wrap_grants::seed_fixture_grant(&db).await;

        let order = Arc::new(Mutex::new(Vec::new()));
        let mut wafer = wafer_with_probes(&order, None);
        let storage = storage_block();
        boot(
            &mut wafer,
            &storage,
            &ProbeHooks { order, fail: false },
            GrantSource::PreInstalled("the plan is authoritative"),
            InitPolicy::Strict,
        )
        .await
        .expect("boot");

        assert!(
            !format!("{:?}", wafer.wrap_grants())
                .contains(crate::platform_state::wrap_grants::FIXTURE_RESOURCE),
            "PreInstalled must not load the database's grants behind the caller's back",
        );
    }

    /// The `/_deploy/init` response body is a wire contract with
    /// `impresspress deploy`'s `PrepareInitReport`, which is
    /// `deny_unknown_fields`.
    #[tokio::test]
    async fn report_serializes_the_deploy_wire_shape() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut wafer = wafer_with_probes(&order, Some("test/alpha"));
        let storage = storage_block();
        let report = boot(
            &mut wafer,
            &storage,
            &ProbeHooks { order, fail: false },
            GrantSource::PreInstalled("test fixture declares none"),
            InitPolicy::Reported,
        )
        .await
        .expect("boot");

        let json = serde_json::to_value(&report).expect("serialize report");
        let object = json.as_object().expect("report is a JSON object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["blocks", "ok", "sealed", "seed"]);
        assert_eq!(
            json["seed"]
                .as_object()
                .expect("seed is an object")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["ok"],
            "a successful step omits `error` entirely",
        );
        let failed = json["blocks"]
            .as_array()
            .expect("blocks is an array")
            .iter()
            .find(|b| b["block"] == "test/alpha")
            .expect("the failing block is reported");
        let mut block_keys: Vec<&str> = failed
            .as_object()
            .expect("block outcome is an object")
            .keys()
            .map(String::as_str)
            .collect();
        block_keys.sort_unstable();
        assert_eq!(block_keys, vec!["block", "error", "ok"]);
    }
}
