//! Every `worker::Env` var and secret this crate reads, captured once.
//!
//! Cloudflare hands a Worker its configuration as a JS object with no
//! enumeration API, so each value has to be asked for by name. Before
//! `CfEnvironment` the crate asked for the same names from five different
//! functions, and the runtime-identity hash — the value that decides whether an
//! isolate may keep serving its cached runtime after the deployment's
//! configuration changed — was a hand-written list of ten of them.

use std::collections::HashMap;

use crate::{request_services, services::cf_log_level_var};

/// Name emitted by the generated Wrangler `[version_metadata]` binding.
pub(crate) const VERSION_METADATA_BINDING: &str = "CF_VERSION_METADATA";

#[derive(Clone)]
pub(crate) struct PreparedRuntimeIdentity {
    pub(crate) application_id: String,
    pub(crate) application_build_sha256: String,
    pub(crate) dependency_lock: impresspress_core::WaferLockIdentity,
    pub(crate) release_assets: impresspress_core::PreparedReleaseAssets,
}

thread_local! {
    /// Verified immutable structure only. No Env, binding, or request I/O
    /// object enters this isolate-local cache.
    ///
    /// [`IdentityCache`] rather than a `RefCell`: this cell is read by every
    /// request that reaches the prepared path, and it is populated by the
    /// single most expensive synchronous step on a cold prepared request
    /// (pull the plan module out of JS, SHA-256 it, parse it, canonicalize
    /// it, re-hash it). Cloudflare can hard-stop a request anywhere in that
    /// window without running a destructor, and a `RefCell` borrow stranded
    /// that way stays set for the life of the isolate — turning every
    /// subsequent request in it into a `panic` → `abort` → wasm trap taken
    /// inside `poll`, whose response promise is never settled. See
    /// `impresspress_core::isolate_cell`'s module documentation for the full
    /// mechanism, and `runtime_cache`'s `BUILD_LEASE_MS` for the same
    /// premise applied to a `Cell<bool>`.
    static PREPARED_PLAN_CACHE: impresspress_core::IdentityCache<impresspress_core::PreparedRuntimePlan> =
        const { impresspress_core::IdentityCache::new() };
}

pub(crate) fn prepared_runtime_identity(
    env: &worker::Env,
) -> Result<PreparedRuntimeIdentity, Box<dyn std::error::Error>> {
    let required_var = |name: &str| -> Result<String, Box<dyn std::error::Error>> {
        let value = env
            .var(name)
            .map_err(|e| format!("required prepared-runtime Worker var {name}: {e}"))?
            .to_string();
        if value.trim().is_empty() {
            return Err(format!("required prepared-runtime Worker var {name} is empty").into());
        }
        Ok(value)
    };
    let application_id = required_var(impresspress_core::PREPARED_APPLICATION_ID_VAR)?;
    let application_build_sha256 =
        required_var(impresspress_core::PREPARED_APPLICATION_BUILD_SHA256_VAR)?;
    let dependency_lock = serde_json::from_str(&required_var(
        impresspress_core::PREPARED_WAFER_LOCK_IDENTITY_JSON_VAR,
    )?)?;

    let release_assets = match env.var(request_services::RELEASE_ASSET_ID_VAR) {
        Ok(asset_id) if !asset_id.to_string().is_empty() => {
            let asset_id = asset_id.to_string();
            let asset_set_sha256 = if asset_id.starts_with("sha256:") {
                asset_id
            } else {
                format!("sha256:{asset_id}")
            };
            impresspress_core::PreparedReleaseAssets::present(
                asset_set_sha256,
                required_var(request_services::RELEASE_ASSET_PREFIX_VAR)?,
                required_var(request_services::RELEASE_ASSET_MANIFEST_VAR)?,
                required_var(impresspress_core::RELEASE_ASSET_MANIFEST_SHA256_VAR)?,
                required_var(impresspress_core::RELEASE_ASSET_KEYS_SHA256_VAR)?,
            )?
        }
        _ => impresspress_core::PreparedReleaseAssets::absent(),
    };

    Ok(PreparedRuntimeIdentity {
        application_id,
        application_build_sha256,
        dependency_lock,
        release_assets,
    })
}

/// Read the immutable Text-module payload installed by the final Worker shim.
/// The candidate upload intentionally has no such global and returns `None`.
#[cfg(target_arch = "wasm32")]
pub(crate) fn packaged_prepared_runtime_plan(
    env: &worker::Env,
) -> Result<Option<std::rc::Rc<impresspress_core::PreparedRuntimePlan>>, Box<dyn std::error::Error>>
{
    let value = js_sys::Reflect::get(
        &js_sys::global(),
        &wasm_bindgen::JsValue::from_str("__IMPRESSPRESS_PREPARED_RUNTIME_PLAN"),
    )
    .map_err(|e| format!("read prepared runtime Text module global: {e:?}"))?;
    // Candidate Workers have no Text-module global. Avoid requiring final-only
    // digest vars there, while stable final Workers can check their cheap
    // identity key before allocating the module string.
    if !value.is_string() {
        return Ok(None);
    }
    let expected_module_sha256 = env
        .var(impresspress_core::PREPARED_PLAN_MODULE_SHA256_VAR)
        .map_err(|e| format!("prepared plan module digest Worker var: {e}"))?
        .to_string();
    let expected_plan_hash = env
        .var(impresspress_core::PREPARED_PLAN_HASH_VAR)
        .map_err(|e| format!("prepared plan hash Worker var: {e}"))?
        .to_string();
    let worker_version = env
        .get_binding::<worker::WorkerVersionMetadata>(VERSION_METADATA_BINDING)
        .ok()
        .map(|metadata| metadata.id())
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "no-version-metadata".to_string());
    let cache_key = format!("{worker_version}\n{expected_plan_hash}\n{expected_module_sha256}");
    // Decode OUTSIDE the cache's critical section, exactly as
    // `ReleaseAssetIdentity::from_env` does: look up, release, verify, store.
    // The integrity checks below are unchanged and unconditional — a cache
    // hit is only ever a value that already passed them under this same
    // Worker version, plan hash, and module digest.
    let plan = PREPARED_PLAN_CACHE.with(|cache| {
        cache.get_or_try_insert_with(cache_key, || {
            let json = value
                .as_string()
                .filter(|json| !json.trim().is_empty())
                .ok_or_else(|| "prepared runtime Text module is empty".to_string())?;
            impresspress_core::PreparedRuntimePlan::from_packaged_json(
                json.as_bytes(),
                &expected_plan_hash,
                &expected_module_sha256,
            )
            .map_err(|error| error.to_string())
        })
    })?;
    Ok(Some(plan))
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn packaged_prepared_runtime_plan(
    _env: &worker::Env,
) -> Result<Option<std::rc::Rc<impresspress_core::PreparedRuntimePlan>>, Box<dyn std::error::Error>>
{
    Ok(None)
}

/// Request-current identity for every environment value captured while
/// constructing an isolate-cached runtime.
///
/// The Worker version ID is the deployed fast path: Cloudflare versions
/// capture bindings, secrets/config, code, and compatibility settings, so no
/// secret reads or hashing are needed on an ordinary warm request. The
/// explicit value hash is only a fallback for local development and
/// hand-written Wrangler configs that have not adopted the metadata binding
/// yet. Raw secret material never leaves this function.
pub(crate) fn runtime_environment_identity(
    env: &worker::Env,
    request_config: &HashMap<String, String>,
) -> String {
    let request_config_hash = config_identity_hash(request_config);
    if let Ok(metadata) = env.get_binding::<worker::WorkerVersionMetadata>(VERSION_METADATA_BINDING)
    {
        let version_id = metadata.id();
        if !version_id.is_empty() {
            return format!("worker-version:{version_id}:request-config:{request_config_hash}");
        }
    }

    let jwt_secret = env
        .secret(impresspress_core::blocks::auth::JWT_SECRET_KEY)
        .map(|value| value.to_string())
        .unwrap_or_default();
    let strict_schema = env
        .var(wafer_core::interfaces::database::handler::STRICT_SCHEMA_CONFIG_KEY)
        .map(|value| value.to_string())
        .unwrap_or_default();
    let log_level = cf_log_level_var(env).unwrap_or_default();
    let cors = env
        .var(impresspress_core::config_vars::CORS_ALLOWED_ORIGINS_KEY)
        .map(|value| value.to_string())
        .unwrap_or_default();
    let csp = env
        .var(impresspress_core::config_vars::CSP_DIRECTIVES_KEY)
        .map(|value| value.to_string())
        .unwrap_or_default();
    let release_id = env
        .var(request_services::RELEASE_ASSET_ID_VAR)
        .map(|value| value.to_string())
        .unwrap_or_default();
    let release_prefix = env
        .var(request_services::RELEASE_ASSET_PREFIX_VAR)
        .map(|value| value.to_string())
        .unwrap_or_default();
    let release_manifest = env
        .var(request_services::RELEASE_ASSET_MANIFEST_VAR)
        .map(|value| value.to_string())
        .unwrap_or_default();
    let release_manifest_hash = env
        .var(impresspress_core::RELEASE_ASSET_MANIFEST_SHA256_VAR)
        .map(|value| value.to_string())
        .unwrap_or_default();
    let release_keys_hash = env
        .var(impresspress_core::RELEASE_ASSET_KEYS_SHA256_VAR)
        .map(|value| value.to_string())
        .unwrap_or_default();

    // Length prefixes make the encoding unambiguous even if values contain
    // separators. Hashing also keeps the JWT secret out of ReadyRuntime and
    // diagnostics.
    let components = [
        ("jwt-secret", jwt_secret.as_str()),
        ("strict-schema", strict_schema.as_str()),
        ("log-level", log_level.as_str()),
        ("cors", cors.as_str()),
        ("csp", csp.as_str()),
        ("release-id", release_id.as_str()),
        ("release-prefix", release_prefix.as_str()),
        ("release-manifest", release_manifest.as_str()),
        ("release-manifest-hash", release_manifest_hash.as_str()),
        ("release-keys-hash", release_keys_hash.as_str()),
        ("request-config", request_config_hash.as_str()),
    ];
    let mut encoded = Vec::new();
    for (name, value) in components {
        encoded.extend_from_slice(&(name.len() as u64).to_le_bytes());
        encoded.extend_from_slice(name.as_bytes());
        encoded.extend_from_slice(&(value.len() as u64).to_le_bytes());
        encoded.extend_from_slice(value.as_bytes());
    }
    impresspress_core::util::sha256_hex(&encoded)
}

fn config_identity_hash(config: &HashMap<String, String>) -> String {
    let mut entries: Vec<_> = config.iter().collect();
    entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
    let mut encoded = Vec::new();
    for (key, value) in entries {
        encoded.extend_from_slice(&(key.len() as u64).to_le_bytes());
        encoded.extend_from_slice(key.as_bytes());
        encoded.extend_from_slice(&(value.len() as u64).to_le_bytes());
        encoded.extend_from_slice(value.as_bytes());
    }
    impresspress_core::util::sha256_hex(&encoded)
}

#[cfg(test)]
mod tests {
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::*;

    #[wasm_bindgen_test]
    fn verified_identity_cache_loads_once_and_invalidates_on_identity_change() {
        let cache: impresspress_core::IdentityCache<u32> = impresspress_core::IdentityCache::new();
        let loads = std::cell::Cell::new(0);
        let first = cache
            .get_or_try_insert_with("worker-a/plan-a".to_string(), || {
                loads.set(loads.get() + 1);
                Ok::<_, ()>(11)
            })
            .unwrap();
        let second = cache
            .get_or_try_insert_with("worker-a/plan-a".to_string(), || {
                loads.set(loads.get() + 1);
                Ok::<_, ()>(22)
            })
            .unwrap();
        assert!(std::rc::Rc::ptr_eq(&first, &second));
        assert_eq!(loads.get(), 1);

        let replacement = cache
            .get_or_try_insert_with("worker-b/plan-a".to_string(), || {
                loads.set(loads.get() + 1);
                Ok::<_, ()>(33)
            })
            .unwrap();
        assert_eq!(*replacement, 33);
        assert_eq!(loads.get(), 2);
    }

    #[wasm_bindgen_test]
    fn request_config_identity_is_order_independent_and_value_sensitive() {
        let left = HashMap::from([
            ("B".to_string(), "2".to_string()),
            ("A".to_string(), "1".to_string()),
        ]);
        let right = HashMap::from([
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "2".to_string()),
        ]);
        let changed = HashMap::from([
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "3".to_string()),
        ]);
        assert_eq!(config_identity_hash(&left), config_identity_hash(&right));
        assert_ne!(config_identity_hash(&left), config_identity_hash(&changed));
    }
}
