//! Impresspress app compiled to WASM for running in the browser via Service Worker.
//!
//! Thin wasm-bindgen wrapper around the `impresspress-browser` framework. Uses
//! `ImpresspressBuilder` (from `impresspress-core`) to wire up the full Impresspress
//! block suite + the app-specific `BrowserLlmService`.

use std::sync::Arc;

use impresspress_core::builder;
use wafer_core::interfaces::config::service::ConfigService;
use wasm_bindgen::prelude::*;

pub mod config;
// The module documents itself (`//!` in `dev_runtime.rs`). Deliberately no
// `///` here: rustdoc merges an outer doc comment on the `mod` item with the
// module's own inner docs and then resolves the whole block in *this* scope,
// so every intra-doc link the module makes to its own items goes unresolved.
#[cfg(feature = "browser-devtools")]
pub mod dev_runtime;
pub mod runtime_factory;

pub use runtime_factory::{RuntimeFactory, RuntimeOptions};

const IMPRESSPRESS_CSP: &str = concat!(
    "default-src 'self'; ",
    "script-src 'self' 'unsafe-inline' 'unsafe-eval' 'wasm-unsafe-eval' https://cdn.jsdelivr.net; ",
    "style-src 'self' 'unsafe-inline'; ",
    "img-src 'self' data: blob: https:; ",
    "font-src 'self' https:; ",
    "connect-src 'self' https://cdn.jsdelivr.net https://esm.run https://huggingface.co ",
        "https://raw.githubusercontent.com https://*.huggingface.co https://*.hf.co https://*.xethub.hf.co; ",
    "frame-ancestors 'none'; ",
    "base-uri 'self'; ",
    "form-action 'self'",
);

/// Boot the runtime inside the Service Worker.
///
/// `options` is the object `sw.js` passes: `{ dev: <bool> }`, rendered from
/// the bundle's `__DEV_ENABLED__` placeholder. A missing or non-boolean `dev`
/// reads as `false` — the sandbox is never enabled by an unparseable value.
///
/// The flag is a *request*. On a build without `browser-devtools` it resolves
/// to inactive (see `runtime_factory::resolve_dev_active`), and `{ dev: true }`
/// is then a no-op apart from the single console warning below: same seeded
/// variables, same CSP, same routes as `{ dev: false }`.
#[wasm_bindgen]
pub async fn initialize(options: JsValue) -> Result<(), JsValue> {
    if impresspress_browser::is_initialized() {
        return Ok(());
    }

    let dev_requested = js_sys::Reflect::get(&options, &JsValue::from_str("dev"))
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // A bundle can ask for the sandbox on a build that never compiled it. That
    // is accepted rather than fatal — the block cannot exist, so there is
    // nothing to disable — but it is the difference between "the sandbox is
    // off" and "the sandbox is missing", so say so once rather than leaving an
    // operator to wonder why `/b/dev` 404s.
    #[cfg(not(feature = "browser-devtools"))]
    if dev_requested {
        web_sys::console::warn_1(
            &"impresspress: initialize({ dev: true }) on a build without the \
              `browser-devtools` feature — the sandbox is not compiled in and \
              /b/dev will not exist"
                .into(),
        );
    }

    impresspress_browser::db_init().await?;

    let factory = RuntimeFactory::new(RuntimeOptions {
        dev_enabled: dev_requested,
    })
    .map_err(|e| JsValue::from_str(&e))?;

    // The sandbox control plane is attached BEFORE the first build, so the
    // cold-start runtime already carries `/b/dev`. Doing it afterwards would
    // mean the page did not exist until a rebuild — and an instance with no
    // guest blocks has no reason to rebuild, so on the common path it would
    // never exist at all. `attach` answers `None` whenever the sandbox is not
    // active, and the factory is then untouched.
    #[cfg(feature = "browser-devtools")]
    let (factory, dev_shared) = dev_runtime::attach(factory);
    #[cfg(not(feature = "browser-devtools"))]
    let factory = factory;

    let (wafer, _storage_block) = factory.build(&[]).await?;

    web_sys::console::log_1(&"impresspress: WAFER runtime started".into());

    impresspress_browser::store_wafer(wafer).map_err(|e| JsValue::from_str(&e.to_string()))?;

    // Seed on a fresh instance, converge on whatever the activation journal
    // was in the middle of, and rebuild with the active block set — all before
    // returning, because requests only start once `initialize()` resolves.
    #[cfg(feature = "browser-devtools")]
    if let Some(shared) = &dev_shared {
        dev_runtime::install(shared).await;
    }

    Ok(())
}

/// [`BootHooks`](impresspress_core::builder::BootHooks) impl for the browser
/// target. After `init_block(admin)` has created the variables /
/// block_settings tables, this seeds them (auto-gen + JWT + browser-only
/// defaults) and the #222 block-settings hash-gate, then publishes the loaded
/// state into the services the wafer already holds:
///  - `config_svc` — the same `Arc<dyn ConfigService>` (mutated via `.set()`).
///  - `block_settings_handle` — the same `Arc<RwLock<BlockSettings>>` the
///    router's `FeatureConfig` reads, so the write is visible to the
///    subsequent `init_all_blocks()` and every later request.
///  - `crypto` — the concrete `BrowserCryptoService`, rotated to the real JWT
///    secret so any not-yet-initialised block signs/verifies with it.
///
/// `db` is a fresh `BrowserDatabaseService` handle; the service is a stateless
/// unit struct over global OPFS, so it points at the same database the wafer
/// uses.
struct BrowserBootHooks {
    db: Arc<dyn wafer_core::interfaces::database::service::DatabaseService>,
    config_svc: Arc<dyn ConfigService>,
    block_settings_handle: Arc<std::sync::RwLock<impresspress_core::features::BlockSettings>>,
    jwt_secret_handle: Arc<std::sync::RwLock<String>>,
    crypto: Arc<impresspress_browser::crypto::BrowserCryptoService>,
    /// Whether this bundle was booted with the development sandbox requested.
    /// Seeds the sandbox's own variables — see `config::seed_and_load_variables`.
    dev_active: bool,
}

#[wafer_block::wafer_async_trait]
impl builder::BootHooks for BrowserBootHooks {
    async fn seed_after_admin_init(&self, wafer: &mut wafer_run::Wafer) -> Result<(), String> {
        let vars = config::seed_and_load_variables(&self.db, self.dev_active).await?;
        web_sys::console::log_1(
            &format!(
                "impresspress: {} variables loaded from database",
                vars.len()
            )
            .into(),
        );
        let features = config::load_block_settings(&self.db).await?;

        for (key, value) in &vars {
            self.config_svc.set(key, value);
        }
        // This adapter executes inside an end user's browser. Set the marker
        // after persisted variables are published so a database/admin value
        // cannot accidentally enable Stripe secret-key operations locally.
        // Static pages may still use a remote trusted commerce API or
        // pre-created Payment Links.
        self.config_svc.set(
            impresspress_core::blocks::products::RUNTIME_KIND_CONFIG_KEY,
            "browser",
        );
        self.config_svc.set(
            impresspress_core::features::BLOCK_SETTINGS_CONFIG_KEY,
            &features.to_config_json(),
        );

        // `ctx.config_get` reads Wafer's synchronous snapshot, not the config
        // service block. Publish the same post-migration values there before
        // `init_all_blocks()` so migration/feature gates observe the seeded
        // browser state rather than the empty pre-admin snapshot.
        let mut snapshot = (**wafer.config_snapshot()).clone();
        snapshot.extend(vars.iter().map(|(k, v)| (k.clone(), v.clone())));
        snapshot.insert(
            impresspress_core::blocks::products::RUNTIME_KIND_CONFIG_KEY.to_string(),
            "browser".to_string(),
        );
        snapshot.insert(
            impresspress_core::features::BLOCK_SETTINGS_CONFIG_KEY.to_string(),
            features.to_config_json(),
        );
        wafer.set_config_snapshot(snapshot);

        *self
            .block_settings_handle
            .write()
            .expect("BlockSettings RwLock poisoned") = features;
        if let Some(secret) = vars.get(impresspress_core::blocks::auth::JWT_SECRET_KEY) {
            // Rotate BOTH holders of the secret to the seeded value: the crypto
            // service that SIGNS tokens and the router lock the pipeline VERIFIES
            // against. Rotating only the crypto service (the old bug) left the
            // router verifying with the empty build-time secret, so every
            // authenticated request 403'd after a successful login.
            self.crypto.set_jwt_secret(secret.clone());
            *self
                .jwt_secret_handle
                .write()
                .expect("jwt_secret handle RwLock poisoned") = secret.clone();
        }
        Ok(())
    }
}

#[wasm_bindgen]
pub async fn handle_request(request: web_sys::Request) -> Result<web_sys::Response, JsValue> {
    impresspress_browser::dispatch_request(request).await
}
