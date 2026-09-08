//! Cloudflare Workers adapter for impresspress: D1 database service, R2 storage
//! service, wasm-compatible crypto/network services, and worker entry helpers.
//!
//! Consumed by:
//! - `impresspress-cloud`'s `impresspress-worker` (multi-tenant dispatch user worker).
//! - The `impresspress build --target cloudflare` flow (single-worker consumers
//!   like wafer-site).
//!
//! This crate is wasm-only; building for native targets is not supported.
//!
//! # Where things are
//!
//! This file is the Worker entry surface and nothing else — both entry
//! points. [`run`] / [`run_with_config`] are the `fetch` shim (`run_inner`,
//! `dispatch`, the `/b/static/` R2 read-through, and the error mapping that
//! turns a failed dispatch into a response); [`run_scheduled`] /
//! [`run_scheduled_with_config`] are the `scheduled` shim, which hydrates a
//! runtime through the same cache and runs the auth retention sweep on it.
//! Everything either funnel *calls* lives beside them:
//!
//! | module | what it owns |
//! |---|---|
//! | [`services`] | the public `make_*` service constructors |
//! | [`environment`] | the runtime-identity hash and the prepared-plan identity reads |
//! | [`runtime_build`] | `build_runtime`, the two config surfaces, the three boot funnels |
//! | [`boot_hooks`] | the three `BootHooks` impls those funnels pick between |
//! | [`runtime_cache`] | the per-isolate runtime cache and its probe policy |
//! | [`deploy_endpoints`] | `/_deploy/init`, `/_deploy/prepare`, `/_deploy/prepared`, `/_deploy/verify` |
//! | [`host_policy`] | the `*.workers.dev` preview lockdown |
//!
//! The release manifest `/_deploy/verify` re-reads is not one of them: it is
//! `impresspress_core::release_inventory::ReleaseManifest`, the same type
//! `impresspress deploy` writes.

mod boot_hooks;
pub mod config_service;
pub mod config_source;
// Compile-time `DatabaseService` conformance assertions for the D1 and
// KV-cached adapters (wafer-run #319 shared suite). Gated behind the
// off-by-default `conformance-check` feature so the suite never enters the
// production Worker wasm; CI checks it explicitly. See the module doc.
#[cfg(feature = "conformance-check")]
mod conformance;
pub mod convert;
pub mod crypto_service;
pub mod database;
mod deploy_endpoints;
mod environment;
pub mod helpers;
mod host_policy;
pub mod kv_cached_db;
pub mod logger_service;
pub mod network_service;
mod request_services;
mod runner;
mod runtime_build;
mod runtime_cache;
mod services;
pub mod storage;

// The `make_*` constructors are the crate's public service-construction API
// (`impresspress-cloud`'s worker and the `impresspress build --target
// cloudflare` shim both call them by these paths); `services` is private so
// that surface is exactly this list.
use std::{collections::HashMap, sync::Arc};

use impresspress_core::builder::ImpresspressBuilder;
pub use services::{
    make_config_service, make_console_logger, make_d1_database_service, make_fetch_network_service,
    make_jwt_crypto_service, make_kv_cached_database_service, make_r2_storage_service,
    release_asset_object_key,
};
use wafer_core::interfaces::storage::service::StorageService;

use crate::{
    deploy_endpoints::{
        deploy_init_endpoint, deploy_token_authorized, prepared_status_endpoint,
        prepared_verify_endpoint,
    },
    environment::CfEnvironment,
    host_policy::{host_is_version_preview, host_is_workers_dev},
    runtime_build::warm_request_services,
    services::{make_d1_database_service_concrete, make_kv_backend, resolved_log_level},
};

thread_local! {
    static ISOLATE_INITIALIZED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// One-time isolate initialization: selects [`RequestLogMode::Queued`]
/// (audit rows drain into `ctx.wait_until` off the response path — see
/// `run`). Consumers should call this from their worker's
/// `#[event(start)]` handler; `run()` also invokes it behind a
/// once-per-isolate guard, so isolates stay correct either way and repeat
/// calls are no-ops.
///
/// [`RequestLogMode::Queued`]: impresspress_core::pipeline::RequestLogMode::Queued
pub fn init_isolate() {
    ISOLATE_INITIALIZED.with(|done| {
        if !done.get() {
            impresspress_core::pipeline::set_request_log_mode(
                impresspress_core::pipeline::RequestLogMode::Queued,
            );
            done.set(true);
        }
    });
}

/// Worker entry shim: load D1 vars, wire services, run the consumer's
/// block registrations, dispatch the request through WAFER.
///
/// Two consumer hooks:
/// - `register_blocks` runs against the `ImpresspressBuilder` after the 6
///   services are attached and before `builder.build()`. Use builder
///   methods (`extra_block`, `add_route`, `block_config`).
/// - `register_post_build` runs against `&mut Wafer` after build and
///   before start, and additionally receives the configured R2-backed
///   `StorageService` so consumers can register blocks that need direct
///   (un-namespaced) access to the bucket — for example, a static
///   asset-serving block that reads a fixed key prefix uploaded by
///   `impresspress deploy --target cloudflare`.
///
/// Binding names are hardcoded: D1 = `"DB"`, R2 = `"STORAGE"`. Consumers'
/// `wrangler.toml` must use these names.
///
/// On error in any step, returns a 500 response with the error message.
/// The error is also logged via `worker::console_log!`.
pub async fn run<F, G>(
    req: worker::Request,
    env: worker::Env,
    ctx: worker::Context,
    register_blocks: F,
    register_post_build: G,
) -> worker::Result<worker::Response>
where
    F: FnOnce(ImpresspressBuilder) -> Result<ImpresspressBuilder, Box<dyn std::error::Error>>,
    G: FnOnce(
        &mut wafer_run::Wafer,
        Arc<dyn StorageService>,
    ) -> Result<(), Box<dyn std::error::Error>>,
{
    run_with_config(
        req,
        env,
        ctx,
        HashMap::new(),
        register_blocks,
        register_post_build,
    )
    .await
}

/// Resolve a `/b/static/…` request path to the R2 object key and content
/// type for that asset, or `None` if the path is not a known asset.
///
/// The manifest lookup IS the security boundary: a filename absent from
/// `ASSETS` returns `None` before any key is constructed, so no request
/// input ever reaches a storage key — mirrors the exact-match discipline of
/// `impresspress_core::blocks::system`'s embedded-path lookup (no
/// prefix/suffix scanning), just resolved to an R2 object key instead of
/// `'static` bytes. `ASSETS` itself is unconditionally available (`build.rs`
/// always generates the manifest; only the bytes behind it are feature-gated
/// — see `impresspress_core::ui::assets`), so this works even though this
/// crate builds `impresspress-core` with `embed-assets` off.
#[cfg(not(feature = "embed-assets"))]
pub(crate) fn static_asset_target(path: &str) -> Option<(&'static str, &'static str)> {
    let filename = path.strip_prefix(impresspress_core::routing::STATIC_PREFIX)?;
    let e = impresspress_core::ui::assets::ASSETS
        .iter()
        .find(|e| e.filename == filename)?;
    Some((e.filename, e.content_type))
}

/// Stream a resolved `/b/static/` asset straight off the R2 bucket binding.
/// `key` and `content_type` come only from [`static_asset_target`]'s
/// manifest lookup — this never constructs a storage key from raw request
/// input. Headers match the embedded path (`impresspress-core`'s
/// `blocks::system`) exactly: the manifest's content type plus a one-year
/// immutable cache lifetime, safe because every filename carries a content
/// hash.
///
/// A miss in R2 (object absent) is a 404 — that should not happen for a key
/// straight from the manifest on a correctly deployed bucket, but the
/// request must not 500 if a deploy's R2 upload and Worker version somehow
/// drift.
#[cfg(not(feature = "embed-assets"))]
async fn serve_static_asset_from_r2(
    env: &worker::Env,
    key: &str,
    content_type: &str,
) -> worker::Result<worker::Response> {
    let bucket = env.bucket(runner::R2_BINDING)?;
    let Some(object) = bucket.get(key).execute().await? else {
        return worker::Response::error("not found", 404);
    };
    let body = object
        .body()
        .ok_or_else(|| worker::Error::RustError(format!("R2 object {key} has no body")))?;
    let bytes = body.bytes().await?;

    let mut response = worker::Response::from_bytes(bytes)?;
    let headers = response.headers_mut();
    headers.set("Content-Type", content_type)?;
    headers.set("Cache-Control", "public, max-age=31536000, immutable")?;
    Ok(response)
}

/// Variant of [`run`] with explicit request-current Worker configuration.
///
/// Workers cannot enumerate `Env`, so consumers pass the small allowlist of
/// application vars/secrets their blocks resolve through `wafer-run/config`.
/// These values enter only this request's ConfigService and lazy ConfigSource;
/// they are not copied into the isolate-cached Wafer snapshot. Their hash is
/// nevertheless part of runtime identity because a builder may consume one
/// structurally while registering middleware/routes.
pub async fn run_with_config<F, G>(
    req: worker::Request,
    env: worker::Env,
    ctx: worker::Context,
    request_config: HashMap<String, String>,
    register_blocks: F,
    register_post_build: G,
) -> worker::Result<worker::Response>
where
    F: FnOnce(ImpresspressBuilder) -> Result<ImpresspressBuilder, Box<dyn std::error::Error>>,
    G: FnOnce(
        &mut wafer_run::Wafer,
        Arc<dyn StorageService>,
    ) -> Result<(), Box<dyn std::error::Error>>,
{
    // Every `worker::Env` var and secret this request needs, read once, here.
    // `worker::Env` keeps travelling alongside it for the D1/KV/R2 *bindings*,
    // which are not var reads. See `environment`'s module doc.
    let environment = CfEnvironment::capture(&env);

    // `std::env` is stubbed to always-empty on `wasm32-unknown-unknown`, so
    // `impresspress_core::ui::assets::base_url()` can never observe
    // `IMPRESSPRESS_ASSET_BASE_URL` through it here. `worker::Env::var` is
    // the one channel that does carry a Worker `[vars]` entry, so push the
    // captured value into `base_url()`'s platform override before any code
    // path below can render a page (and therefore call `base_url()`).
    // Idempotent (see `set_base_url_override`'s doc) — safe to call on
    // every request, including the fresh-runtime `/_deploy/*` funnels.
    impresspress_core::ui::assets::set_base_url_override(environment.asset_base_url());

    if req.path() == "/_deploy/verify" {
        return prepared_verify_endpoint(&req, &env, &environment).await;
    }
    if req.path() == "/_deploy/prepared" {
        return prepared_status_endpoint(&req, &environment);
    }
    if req.path() == "/_deploy/init" || req.path() == "/_deploy/prepare" {
        let prepare_plan = req.path() == "/_deploy/prepare";
        return deploy_init_endpoint(
            req,
            env,
            environment,
            request_config,
            prepare_plan,
            register_blocks,
            register_post_build,
        )
        .await;
    }

    // Lock down `*.workers.dev` preview hosts. Version preview URLs
    // (`https://<hash>-<worker>.<subdomain>.workers.dev`) expose the full app
    // on a public workers.dev host during the atomic deploy window; this guard
    // returns a plain 404 there so only the deploy endpoint is reachable.
    // Runs AFTER the `/_deploy/init` intercept above, so `impresspress deploy`'s
    // init gate still works on the preview host — that's the whole deploy flow.
    //
    // Consumers that legitimately serve on workers.dev — no custom domain —
    // opt in with the `IMPRESSPRESS_ALLOW_WORKERS_DEV=1` worker var. The opt-in
    // admits the worker's canonical host only; a *version preview* host stays
    // locked regardless, because `impresspress deploy` proves an unpromoted
    // candidate is unreachable before it promotes anything
    // (`smoke_preview_lockdown`), and an opt-in that opened previews would
    // make the atomic deploy impossible for exactly the consumers it exists
    // for. `wrangler dev` (localhost) is unaffected.
    if host_is_workers_dev(&req)?
        && !deploy_token_authorized(&req, &environment)
        && (!environment.allows_workers_dev() || host_is_version_preview(&req, &environment)?)
    {
        return worker::Response::error("not found", 404);
    }

    // Serve `/b/static/` assets straight from the R2 bucket binding,
    // bypassing Wafer block dispatch entirely — same shape as the
    // `/_deploy/*` special cases above, because this is the one place with a
    // live `env` and its R2 binding; `impresspress-core`'s `Context` has no
    // storage capability (see `static_asset_target`'s doc and this crate's
    // `embed-assets` feature comment in `Cargo.toml`). Runs AFTER the
    // workers.dev lockdown so a preview host keeps hiding CSS/JS along with
    // everything else, matching how the embedded path behaves on that host.
    #[cfg(not(feature = "embed-assets"))]
    if let Some((key, content_type)) = static_asset_target(&req.path()) {
        return serve_static_asset_from_r2(&env, key, content_type).await;
    }

    // Isolate-scoped init (request-log mode) — no-op after the first call;
    // consumers with an #[event(start)] handler have already run it.
    init_isolate();
    let result = run_inner(
        req,
        &env,
        &environment,
        &request_config,
        register_blocks,
        register_post_build,
    )
    .await;

    // Persist any audit rows queued during this dispatch off the response
    // path. Derive the D1 handle from THIS request's Env instead of borrowing
    // one retained by the isolate-cached runtime.
    let rows = impresspress_core::pipeline::drain_queued_request_logs();
    if !rows.is_empty() {
        match make_d1_database_service_concrete(&env, runner::D1_BINDING) {
            Ok(batch_db) => {
                ctx.wait_until(async move {
                    let mut by_table: std::collections::HashMap<
                        &'static str,
                        Vec<std::collections::HashMap<String, serde_json::Value>>,
                    > = std::collections::HashMap::new();
                    for row in rows {
                        by_table.entry(row.table).or_default().push(row.data);
                    }
                    for (table, rows) in by_table {
                        let n = rows.len();
                        if let Err(e) = batch_db.create_many(table, rows).await {
                            // Structured metric line (not a Server-Timing header:
                            // this closure runs in `ctx.wait_until`, after the
                            // response has already been sent). See
                            // `impresspress_core::metrics`'s module doc.
                            let rows_str = n.to_string();
                            let err_str = e.to_string();
                            worker::console_log!(
                                "{}",
                                impresspress_core::metrics::metric_line(
                                    "audit_log_persist_failed",
                                    &[("table", table), ("rows", &rows_str), ("error", &err_str)],
                                )
                            );
                        }
                    }
                });
            }
            Err(e) => worker::console_log!(
                "{}",
                impresspress_core::metrics::metric_line(
                    "audit_log_persist_failed",
                    &[("error", &e.to_string())],
                )
            ),
        }
    }

    retry_pending_config_version(&env, |task| ctx.wait_until(task));

    match result {
        Ok(response) => Ok(response),
        Err(e)
            if e.downcast_ref::<runtime_cache::RuntimeBuildBusy>()
                .is_some() =>
        {
            worker::console_log!("impresspress-cloudflare runtime build busy; retrying is safe");
            let mut response = worker::Response::error("service temporarily unavailable", 503)?;
            response.headers_mut().set("Retry-After", "1")?;
            Ok(response)
        }
        Err(e) => {
            // Never return the real cause to the client — `e` can carry
            // SQL, binding, schema, or other configuration detail. Log it
            // (with a correlation id) and return an opaque 500; an operator
            // greps the isolate's console log for the same id to find the
            // real error.
            let correlation_id = uuid::Uuid::new_v4();
            worker::console_log!("impresspress-cloudflare run error [{correlation_id}]: {e}");
            worker::Response::error(
                format!("internal server error (reference: {correlation_id})"),
                500,
            )
        }
    }
}

/// Worker `scheduled` entry shim: the cron counterpart of [`run`].
///
/// Consumers call this from their `#[event(scheduled)]` handler, passing the
/// **same two registration hooks they pass to [`run`]**. That is not a style
/// preference: both entry points share one per-isolate runtime cache, so a
/// `scheduled` handler that registered a different block set would build a
/// runtime under this deployment's own identity and publish it for the next
/// fetch to serve.
///
/// One thing runs here — the auth retention sweep
/// (`impresspress_core::blocks::auth_ui::MAINTENANCE_MESSAGE_KIND`), a message
/// kind auth-ui already routes, so this adds an entry point rather than a code
/// path. Its counts are logged. Nothing else runs on the schedule.
///
/// The schedule itself comes from `[triggers] crons` in the generated
/// `wrangler.toml` (`impresspress deploy`'s `DEFAULT_CRONS`). A deployment
/// that sets `crons = []` never reaches this function.
pub async fn run_scheduled<F, G>(
    event: worker::ScheduledEvent,
    env: worker::Env,
    ctx: worker::ScheduleContext,
    register_blocks: F,
    register_post_build: G,
) where
    F: FnOnce(ImpresspressBuilder) -> Result<ImpresspressBuilder, Box<dyn std::error::Error>>,
    G: FnOnce(
        &mut wafer_run::Wafer,
        Arc<dyn StorageService>,
    ) -> Result<(), Box<dyn std::error::Error>>,
{
    run_scheduled_with_config(
        event,
        env,
        ctx,
        HashMap::new(),
        register_blocks,
        register_post_build,
    )
    .await
}

/// Variant of [`run_scheduled`] with explicit request-current Worker
/// configuration, for consumers that use [`run_with_config`] on the fetch side.
///
/// Pass the same map: `request_config` is part of runtime identity
/// (`CfEnvironment::identity`), so a cron passing an empty map into an isolate
/// whose fetches pass a populated one would rebuild the runtime on every
/// invocation and leave the wrong one cached behind it.
pub async fn run_scheduled_with_config<F, G>(
    event: worker::ScheduledEvent,
    env: worker::Env,
    ctx: worker::ScheduleContext,
    request_config: HashMap<String, String>,
    register_blocks: F,
    register_post_build: G,
) where
    F: FnOnce(ImpresspressBuilder) -> Result<ImpresspressBuilder, Box<dyn std::error::Error>>,
    G: FnOnce(
        &mut wafer_run::Wafer,
        Arc<dyn StorageService>,
    ) -> Result<(), Box<dyn std::error::Error>>,
{
    let environment = CfEnvironment::capture(&env);
    // Same reason as `run_with_config`: `std::env` is stubbed empty on wasm32,
    // so the Worker var is the only channel carrying an asset base URL. The
    // sweep renders no page, but the runtime this builds is published into the
    // isolate cache for the next fetch, which does.
    impresspress_core::ui::assets::set_base_url_override(environment.asset_base_url());
    init_isolate();

    let cron = event.cron();
    match run_scheduled_inner(
        &env,
        &environment,
        &request_config,
        register_blocks,
        register_post_build,
    )
    .await
    {
        Ok(sweep) => worker::console_log!(
            "{}",
            impresspress_core::metrics::metric_line(
                "auth_maintenance_sweep",
                &[
                    ("cron", &cron),
                    ("complete", &sweep.complete.to_string()),
                    ("sessions_deleted", &sweep.sessions_deleted.to_string()),
                    ("tokens_deleted", &sweep.tokens_deleted.to_string()),
                    (
                        "jwt_blocklist_deleted",
                        &sweep.jwt_blocklist_deleted.to_string()
                    ),
                    ("oauth_pkce_deleted", &sweep.oauth_pkce_deleted.to_string()),
                    ("errors", &sweep.errors.join(",")),
                ],
            )
        ),
        // A cron has no client to answer, so a failure that a fetch would turn
        // into a 500 can only be logged. It is logged in full rather than
        // behind a correlation id: nobody is receiving this text but the
        // operator reading the isolate's own log.
        Err(error) => worker::console_log!(
            "{}",
            impresspress_core::metrics::metric_line(
                "auth_maintenance_sweep_failed",
                &[("cron", &cron), ("error", &error.to_string())],
            )
        ),
    }

    retry_pending_config_version(&env, |task| ctx.wait_until(task));
}

/// Hydrate a runtime and run one retention pass on it.
async fn run_scheduled_inner<F, G>(
    env: &worker::Env,
    environment: &CfEnvironment,
    request_config: &HashMap<String, String>,
    register_blocks: F,
    register_post_build: G,
) -> Result<impresspress_core::blocks::auth::maintenance::SweepResult, Box<dyn std::error::Error>>
where
    F: FnOnce(ImpresspressBuilder) -> Result<ImpresspressBuilder, Box<dyn std::error::Error>>,
    G: FnOnce(
        &mut wafer_run::Wafer,
        Arc<dyn StorageService>,
    ) -> Result<(), Box<dyn std::error::Error>>,
{
    // WHICH BOOT FUNNEL A CRON TAKES, and why it is this one.
    //
    // `get_or_build` is the request path's entry, and it picks between two of
    // the three funnels `runtime_build` declares: `boot_prepared_runtime` when
    // this Worker version carries a packaged plan, `boot_dynamic_request_
    // runtime` otherwise. Going through it puts a scheduled invocation on
    // whichever of those two this deployment's fetches already take. That is
    // the decision, made here, not a side effect of reusing a convenient
    // function — and it is also why the build is not wasted: a cold cron warms
    // the very cache the next fetch reads.
    //
    // The third funnel, `boot_deploy_runtime`, is the one a cron must NOT
    // take, and it is the one that looks affordable — no client is waiting, so
    // `InitPolicy::Reported` and the seeding hook seem free. They are not.
    // Seeding is a deploy-time mutation performed with an operator present. On
    // a schedule it would run migrations without consent on any database that
    // has not seen `/_deploy/init`, bump the KV config generation whenever it
    // did seed and so force a full dynamic rebuild across the fleet — daily —
    // and race a UNIQUE insert against whatever isolates are serving. Those
    // are exactly the three failure modes amended ruling 5.5 keeps off the
    // request path, and a cron has all three plus nobody watching.
    // `InitPolicy::Reported` is wrong for the same reason: nothing reads the
    // report, so a "reported" failure would publish a half-initialized runtime
    // into the isolate cache for the next fetch to serve. `Strict`, which is
    // what the request funnels use, refuses instead.
    //
    // A cron is a serving-time invocation that happens to have no client. It
    // belongs on the serving funnels.
    let (rt, _cache_outcome) = runtime_cache::get_or_build(
        env,
        environment,
        request_config,
        register_blocks,
        register_post_build,
    )
    .await?;
    let services =
        warm_request_services(env, environment, rt.wafer.config_snapshot(), request_config)?;

    let output = request_services::scope(services, async {
        rt.wafer
            .run_block(
                impresspress_core::blocks::auth_ui::AUTH_UI_BLOCK_ID,
                impresspress_core::blocks::auth_ui::maintenance_message(),
                wafer_run::InputStream::empty(),
            )
            .await
    })
    .await;

    Ok(impresspress_core::blocks::auth_ui::sweep_result_from_output(output).await?)
}

/// Re-attempt a config-version KV PUT that failed earlier in this invocation
/// (most likely KV's 1-write/sec/key throttle), through this invocation's own
/// `Env`.
///
/// Both entry points need it and neither can hold the other's context type:
/// `fetch` has a `worker::Context`, `scheduled` a `worker::ScheduleContext`,
/// and the two `wait_until` methods are inherent, not a shared trait. Hence
/// the closure — the alternative was a second verbatim copy of the retry in
/// [`run_scheduled_with_config`], which is exactly the shape that lets one
/// path silently stop retrying.
fn retry_pending_config_version(env: &worker::Env, defer: impl FnOnce(BoxedTask)) {
    let Some(stamp) = kv_cached_db::take_pending_version_retry() else {
        return;
    };
    match make_kv_backend(env, runner::KV_BINDING) {
        Ok(kv) => defer(Box::pin(async move {
            worker::Delay::from(std::time::Duration::from_millis(1_100)).await;
            if let Err(e) = kv
                .put(impresspress_core::cache_key::CONFIG_VERSION_KEY, &stamp)
                .await
            {
                // `e` here is already a `String` (KvBackend::put's error
                // type) — no `.to_string()` clone needed.
                worker::console_log!(
                    "{}",
                    impresspress_core::metrics::metric_line(
                        "config_version_retry_failed",
                        &[("error", &e)],
                    )
                );
            }
        })),
        Err(e) => worker::console_log!(
            "{}",
            impresspress_core::metrics::metric_line(
                "config_version_retry_failed",
                &[("error", &e.to_string())],
            )
        ),
    }
}

/// Work handed to a `wait_until`. Boxed because the two entry points' contexts
/// take it by different inherent methods and it has to cross a closure.
type BoxedTask = std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;

/// Convert a worker request into a WAFER message (preserving the auth header)
/// and dispatch it through the `"site-main"` flow.
async fn dispatch(
    wafer: &wafer_run::Wafer,
    req: worker::Request,
    services: std::rc::Rc<request_services::RequestServices>,
) -> Result<worker::Response, Box<dyn std::error::Error>> {
    request_services::scope(services, async move {
        // 7. Convert request → message; preserve auth header in meta.
        let auth_header = req.headers().get("authorization")?;
        let (mut msg, input) = convert::worker_request_to_message(&req).await?;
        if let Some(ref auth) = auth_header {
            msg.set_meta("http.header.authorization", auth);
        }

        // 8. Dispatch and convert response. Keeping conversion inside the
        // poll scope also covers lazily consumed service-backed streams.
        let output = wafer.run("site-main", msg, input).await;
        Ok(convert::output_to_response(output).await?)
    })
    .await
}

async fn run_inner<F, G>(
    req: worker::Request,
    env: &worker::Env,
    environment: &CfEnvironment,
    request_config: &HashMap<String, String>,
    register_blocks: F,
    register_post_build: G,
) -> Result<worker::Response, Box<dyn std::error::Error>>
where
    F: FnOnce(ImpresspressBuilder) -> Result<ImpresspressBuilder, Box<dyn std::error::Error>>,
    G: FnOnce(
        &mut wafer_run::Wafer,
        Arc<dyn StorageService>,
    ) -> Result<(), Box<dyn std::error::Error>>,
{
    // Reuse the per-isolate runtime; rebuild only when the KV config-version
    // stamp has moved. No boot funnel here — migrations/seeds run at deploy
    // time via `/_deploy/init`, not on the request path.
    let (rt, cache_outcome) = runtime_cache::get_or_build(
        env,
        environment,
        request_config,
        register_blocks,
        register_post_build,
    )
    .await?;
    let services =
        warm_request_services(env, environment, rt.wafer.config_snapshot(), request_config)?;
    let mut response = dispatch(&rt.wafer, req, services).await?;

    // Cheap observability signal (2026-07-16 audit follow-up): one header
    // assembly from a value already computed by `get_or_build`. Gated to
    // Debug (dev) level — see `resolved_log_level`'s doc — so an
    // unconditional header doesn't disclose per-request cache/rebuild state
    // to anonymous clients on production deployments (which default to
    // Info). A failure to set it never fails the request.
    if resolved_log_level(environment) == impresspress_core::log_level::LogLevel::Debug {
        let server_timing = impresspress_core::metrics::server_timing_header(cache_outcome);
        if let Err(e) = response.headers_mut().set("Server-Timing", &server_timing) {
            worker::console_log!(
                "{}",
                impresspress_core::metrics::metric_line(
                    "server_timing_header_failed",
                    &[("error", &e.to_string())],
                )
            );
        }
    }

    Ok(response)
}
/// Tests for [`static_asset_target`] — the pure decision seam behind the
/// `/b/static/` R2 read-through in `run_with_config`. A worker `fetch`
/// handler is awkward to unit-test, so this covers only the manifest-lookup
/// security boundary; the R2 fetch itself is validated end-to-end by a real
/// Cloudflare deploy (same posture as this crate's other `worker::Env`-driven
/// paths — see `database.rs`'s module note).
#[cfg(all(test, not(feature = "embed-assets")))]
mod static_asset_target_tests {
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::*;

    #[wasm_bindgen_test]
    fn static_asset_target_resolves_a_known_asset() {
        let e = impresspress_core::ui::assets::entry("app.css");
        let path = format!(
            "{}{}",
            impresspress_core::routing::STATIC_PREFIX,
            e.filename
        );
        let (key, ct) = static_asset_target(&path).expect("known asset must resolve");
        assert_eq!(
            key, e.filename,
            "R2 key is the flat hashed filename Task 4 uploads"
        );
        assert_eq!(ct, e.content_type);
    }

    #[wasm_bindgen_test]
    fn static_asset_target_rejects_unknown_and_traversal_before_building_a_key() {
        for p in [
            "/b/static/app-deadbeef.css",
            "/b/static/../../etc/passwd",
            "/b/static/",
            "/not-static/app.css",
        ] {
            assert!(static_asset_target(p).is_none(), "must not resolve: {p}");
        }
    }
}

/// The wasm32 half of the middleware-block invariant.
///
/// `impresspress-core`'s `use_static_blocks!` anchor list is the ONE place
/// the six `wafer-run/*` middleware blocks are named. Off wasm32 linkme
/// collects them and `WAFER_STATIC_BLOCKS` is empty; on wasm32 linkme writes
/// into a link section that does not exist, so the by-value list is the only
/// thing that registers them — and it is the half a hand-written second list
/// used to cover, with nothing keeping the two in step.
///
/// That is asserted here rather than beside the list because
/// `impresspress-core` cannot compile test code for wasm32 at all
/// (`--all-targets` pulls its tokio/mio dev-dependencies, which do not build
/// for that target), so a `cfg(target_arch = "wasm32")` assertion written
/// there is compiled by nothing. This crate has an executable wasm lane and a
/// CI job whose path filter covers the manifests that turn these blocks on.
#[cfg(test)]
mod middleware_blocks_tests {
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn a_runtime_built_on_wasm32_carries_every_middleware_block() {
        let mut wafer =
            wafer_run::Wafer::new(std::sync::Arc::new(wafer_run::StaticConfigSource::default()))
                .expect("Wafer::new with no lockfile");

        // Self-guard: on wasm32 linkme collects nothing, so a bare `Wafer` has
        // none of the six. If that ever stopped being true the assertions
        // below would pass without `register_middleware_blocks` doing anything.
        for name in impresspress_core::builder::MIDDLEWARE_BLOCKS {
            assert!(
                !wafer.has_block(name),
                "{name} was already registered before                  `register_middleware_blocks` ran — this test would be vacuous"
            );
        }

        impresspress_core::builder::register_middleware_blocks(&mut wafer)
            .expect("register the middleware blocks");

        for name in impresspress_core::builder::MIDDLEWARE_BLOCKS {
            assert!(
                wafer.has_block(name),
                "{name} is not registered on wasm32 — is its crate still in \
                 `impresspress-core`'s `use_static_blocks!` anchor list, and \
                 does it still invoke `register_static_block!` under that name?"
            );
        }
    }
}
