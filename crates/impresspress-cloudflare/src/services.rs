//! The crate's public `make_*` service constructors, plus the Worker var only a
//! service constructor reads.
//!
//! Consumers (`impresspress-cloud`'s worker, the `impresspress build --target
//! cloudflare` shim) construct services through these helpers rather than
//! importing the concrete adapter types, so the adapter internals stay private.
//! `lib.rs` re-exports the public half.

use std::{collections::HashMap, sync::Arc};

use wafer_core::interfaces::{
    config::service::ConfigService, crypto::service::CryptoService,
    database::service::DatabaseService, logger::service::LoggerService,
    network::service::NetworkService, storage::service::StorageService,
};

use crate::{
    config_service, crypto_service, database, kv_cached_db, logger_service, network_service,
    request_services, storage,
};

/// Construct a D1-backed [`DatabaseService`] from a worker `Env` and the D1
/// binding name.
///
/// The binding name must match a `[[d1_databases]]` entry in the consumer's
/// `wrangler.toml` (e.g. `"DB"`).
pub fn make_d1_database_service(
    env: &worker::Env,
    binding: &str,
) -> Result<Arc<dyn DatabaseService>, worker::Error> {
    Ok(make_d1_database_service_concrete(env, binding)?)
}

/// Concrete-typed variant of [`make_d1_database_service`]. Used internally
/// where a caller needs D1-specific capabilities beyond the
/// `DatabaseService` trait object — e.g. the audit-log batch-insert path in
/// `run()`, which needs D1's native `batch()` API via
/// [`database::D1DatabaseService::create_many`].
pub(crate) fn make_d1_database_service_concrete(
    env: &worker::Env,
    binding: &str,
) -> Result<Arc<database::D1DatabaseService>, worker::Error> {
    let db = env.d1(binding)?;
    Ok(Arc::new(database::D1DatabaseService::new(db)))
}

/// Construct a [`DatabaseService`] backed by D1 with a Cloudflare KV cache
/// layered on top of the per-block read paths (`variables WHERE block=?`
/// and `block_settings WHERE block_name=?`).
///
/// The KV binding name must match a `[[kv_namespaces]]` entry in the
/// consumer's `wrangler.toml` (canonical name: `"CONFIG_CACHE"`).
///
/// Fails fast if the KV binding is missing — silent degradation would
/// mask a config-drift outage.
///
/// Spec: `docs/superpowers/specs/2026-05-22-kv-cached-d1-config-source-design.md`.
pub fn make_kv_cached_database_service(
    env: &worker::Env,
    d1_binding: &str,
    kv_binding: &str,
) -> Result<Arc<dyn DatabaseService>, worker::Error> {
    let (db, _backend, _batch_db) = make_kv_cached_database_service_with_backend(
        env,
        d1_binding,
        kv_binding,
        kv_cached_db::CacheMode::default(),
    )?;
    Ok(db)
}

/// Internals of [`make_kv_cached_database_service`], additionally returning
/// the `KvBackend` handle it constructs — the per-isolate runtime cache
/// (task-7) needs the backend itself (not just the `DatabaseService` it's
/// wrapped into) so it can probe the KV config-version stamp without re-deriving
/// a `KvStore` handle from `env` on every request — and the concrete D1
/// handle underneath the KV-cache wrapper, which the audit-log batch-insert
/// path (`run()`'s `waitUntil` drain) needs for D1's native `batch()` API
/// (`request_logs` is never a KV-cached table, so going around the wrapper
/// for this one write path is equivalent to going through it). The
/// `/_deploy/init` endpoint re-derives its own KV handle via
/// `make_kv_backend` for its post-funnel config-version bump.
/// Return type of [`make_kv_cached_database_service_with_backend`]: the
/// wrapped `DatabaseService`, the raw `KvBackend` it was built from, and the
/// concrete D1 handle underneath it.
type KvCachedDbServiceWithBackend = (
    Arc<dyn DatabaseService>,
    Arc<dyn impresspress_core::kv::KvBackend>,
    Arc<database::D1DatabaseService>,
);

pub(crate) fn make_kv_cached_database_service_with_backend(
    env: &worker::Env,
    d1_binding: &str,
    kv_binding: &str,
    mode: kv_cached_db::CacheMode,
) -> Result<KvCachedDbServiceWithBackend, worker::Error> {
    let d1 = make_d1_database_service_concrete(env, d1_binding)?;
    let inner: Arc<dyn DatabaseService> = d1.clone();
    let backend = make_kv_backend(env, kv_binding)?;
    // `DatabaseService` only requires `MaybeSend + MaybeSync` (real
    // `Send + Sync` on native, a no-op marker on wasm32 — see
    // wafer_block::compat), so this `Arc` doesn't promise cross-thread
    // safety; this crate only ever targets wasm32, which is single-threaded.
    #[allow(clippy::arc_with_non_send_sync)]
    let db = Arc::new(kv_cached_db::KvCachedD1DatabaseService::with_mode(
        inner,
        backend.clone(),
        mode,
    ));
    Ok((db, backend, d1))
}

/// Construct a raw [`KvBackend`](impresspress_core::kv::KvBackend) from a worker
/// `Env` and a KV binding name. Single construction path shared by the
/// KV-cached DB factory above and the per-isolate runtime cache's
/// config-version probe (`runtime_cache::get_or_build`), so both derive the
/// `KvStore` handle the same way.
pub(crate) fn make_kv_backend(
    env: &worker::Env,
    binding: &str,
) -> Result<Arc<dyn impresspress_core::kv::KvBackend>, worker::Error> {
    let kv_store = env.kv(binding)?;
    Ok(Arc::new(kv_cached_db::WorkerKvBackend(kv_store)))
}

/// Construct an R2-backed [`StorageService`] from a worker `Env` and the R2
/// bucket binding name.
///
/// The binding name must match a `[[r2_buckets]]` entry in the consumer's
/// `wrangler.toml` (e.g. `"STORAGE"`).
pub fn make_r2_storage_service(
    env: &worker::Env,
    binding: &str,
) -> Result<Arc<dyn StorageService>, worker::Error> {
    let bucket = env.bucket(binding)?;
    Ok(Arc::new(storage::R2StorageService::new(bucket)))
}

/// Resolve a logical release-managed asset to its immutable R2 object key.
///
/// Fetches and digest-verifies the release key inventory from R2 on the
/// isolate's first call (cached thereafter). Returns `Ok(None)` only when no
/// release contract is configured or the key is not an inventory member. A
/// partial, malformed, or digest-mismatched contract fails closed so direct
/// R2 fast paths cannot silently downgrade to mutable logical objects.
pub async fn release_asset_object_key(
    env: &worker::Env,
    r2_binding: &str,
    logical_key: &str,
) -> worker::Result<Option<String>> {
    let Some(identity) =
        request_services::ReleaseAssetIdentity::from_env(env).map_err(worker::Error::RustError)?
    else {
        return Ok(None);
    };
    let storage = make_r2_storage_service(env, r2_binding)?;
    let (keys_folder, keys_name) = identity.keys_location();
    let inventory = impresspress_core::release_inventory::load_release_inventory(
        keys_folder,
        keys_name,
        identity.keys_sha256(),
        storage.as_ref(),
    )
    .await
    .map_err(|error| worker::Error::RustError(error.to_string()))?;
    let release = request_services::LoadedRelease {
        identity,
        inventory,
    };
    Ok(release.physical_object_key(logical_key))
}

/// Construct a wasm-compatible [`CryptoService`]: the wafer-block-crypto
/// HS256 JWT engine (exp-required, per-block HKDF-derived keys — same
/// policy as native) with Workers-constrained argon2id password hashing.
///
/// `jwt_secret` is the HMAC master secret used to sign and verify JWTs.
/// It must be at least `wafer_block_crypto::primitives::MIN_JWT_SECRET_LEN`
/// bytes; a missing/short secret surfaces as an error on each sign/verify
/// rather than failing worker boot.
pub fn make_jwt_crypto_service(jwt_secret: String) -> Arc<dyn CryptoService> {
    Arc::new(crypto_service::ImpresspressCryptoService::new(jwt_secret))
}

/// Construct a [`NetworkService`] backed by the CF Worker global `fetch` API.
pub fn make_fetch_network_service() -> Arc<dyn NetworkService> {
    Arc::new(network_service::WorkerFetchService)
}

/// Construct a [`LoggerService`] that writes to `worker::console_log`.
///
/// The minimum emitted level is read at construction from the
/// `IMPRESSPRESS_CF_LOG_LEVEL` worker var (set via `wrangler.toml [vars]` or
/// the dashboard) — a runtime knob, unlike the previous `option_env!`
/// compile-time read, so an operator can raise/lower verbosity per
/// deployment without rebuilding. Falls back to the compile-time default
/// (Debug in dev builds, Info in release) when the var is unset or
/// unparseable. Resolved once per logger construction, which happens at
/// most once per per-isolate runtime build
/// (`runtime_cache::get_or_build`) — never on the request hot path.
pub fn make_console_logger(env: &worker::Env) -> Arc<dyn LoggerService> {
    Arc::new(logger_service::ConsoleLoggerService::new(
        cf_log_level_var(env).as_deref(),
    ))
}

/// Raw `IMPRESSPRESS_CF_LOG_LEVEL` worker var value, if set. Shared by
/// [`make_console_logger`] (logger construction) and [`resolved_log_level`]
/// (Server-Timing gating in `run_inner`) so both resolve from the same read
/// instead of two independent env lookups that could disagree.
pub(crate) fn cf_log_level_var(env: &worker::Env) -> Option<String> {
    env.var(CF_LOG_LEVEL_KEY).ok().map(|v| v.to_string())
}

/// Resolve the Cloudflare console logger's minimum level without needing to
/// downcast the type-erased `Arc<dyn LoggerService>` the runtime holds.
///
/// Used by `run_inner` to gate the `Server-Timing` response header: only
/// attached when this resolves to `Debug` (dev). An unconditional header
/// would disclose per-request cache/rebuild state — including the isolate
/// build counter, a signal for when a config bump landed — to every
/// anonymous client, which is a production fingerprinting concern, not a
/// dev debugging aid.
pub(crate) fn resolved_log_level(env: &worker::Env) -> impresspress_core::log_level::LogLevel {
    logger_service::resolve_level(cf_log_level_var(env).as_deref())
}

/// Worker var (`env.var`) that sets the Cloudflare console logger's minimum
/// emitted level at runtime (`debug`/`info`/`warn`/`error`, case-insensitive
/// — see [`impresspress_core::log_level::LogLevel::parse`]). Unset or
/// unparseable falls back to the compile-time default. See
/// [`make_console_logger`].
pub(crate) const CF_LOG_LEVEL_KEY: &str = "IMPRESSPRESS_CF_LOG_LEVEL";

/// Construct a [`ConfigService`] from a pre-loaded key/value map.
///
/// In a CF Worker, callers typically load variables from the D1 `variables`
/// table (and merge any protected worker env bindings) before calling this
/// function.  The returned service is read-only; `set()` is a no-op because
/// CF Workers are stateless.
pub fn make_config_service(vars: HashMap<String, String>) -> Arc<dyn ConfigService> {
    Arc::new(config_service::HashMapConfigService::new(vars))
}
