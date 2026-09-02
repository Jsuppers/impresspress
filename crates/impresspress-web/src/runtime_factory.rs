//! Builds a booted [`wafer_run::Wafer`] from the browser platform services.
//!
//! Cold start builds once with no dynamic blocks; an activation that changes
//! the block set builds again with the new set and swaps the runtime (see
//! `dev_runtime`, Task 9). Everything `initialize()` used to do between
//! `db_init()` and `store_wafer()` lives here, parameterised by the dynamic
//! block set — so the second build is the *same* build, not a hand-kept copy
//! of it.
//!
//! The browser services are constructed once, in [`RuntimeFactory::new`], and
//! shared by every runtime the factory builds. That is what keeps a rebuild
//! cheap and, more importantly, what keeps the seeded JWT secret and the
//! loaded config alive across a swap: the crypto service and `ConfigService`
//! the new runtime gets are the same allocations the old one held.

use std::{collections::HashMap, sync::Arc};

#[cfg(feature = "browser-devtools")]
use impresspress_core::blocks::dev::{DevShared, DynamicBlockSpec};
use impresspress_core::builder::{self, ImpresspressBuilder};
use wafer_core::interfaces::{
    config::service::ConfigService,
    crypto::service::CryptoService,
    image::service::ImageService,
    llm::service::LlmService,
    vector::service::{EmbeddingService, VectorService},
};
use wasm_bindgen::prelude::*;

/// One dynamically-registered guest: its manifest entry plus the already-loaded
/// block. Loading (wasmi instantiation over the stored artifact) is the
/// caller's job — the factory only registers what it is handed, so a guest that
/// fails to load never reaches a `Wafer` at all.
#[cfg(feature = "browser-devtools")]
pub type DynamicBlock = (DynamicBlockSpec, Arc<dyn wafer_run::Block>);

/// Placeholder element type for builds without `browser-devtools`.
///
/// Uninhabited on purpose: without the feature the sandbox cannot exist, so the
/// only slice that can be constructed is the empty one. `build(&[])` therefore
/// compiles unchanged in both configurations without the call site growing a
/// `#[cfg]`, and no "the feature is off but we still handled a dynamic block"
/// path can be written by accident.
#[cfg(not(feature = "browser-devtools"))]
pub enum DynamicBlock {}

/// Build-time policy that is fixed for the life of the service worker.
pub struct RuntimeOptions {
    /// `initialize({ dev: … })` — what the *bundle asked for*. This is a
    /// request, not a verdict: [`RuntimeFactory::new`] runs it through
    /// [`resolve_dev_active`] and keeps only the result, because a bundle can
    /// ask for a sandbox that was never compiled in.
    pub dev_enabled: bool,
}

/// Whether the development sandbox is *actually* active.
///
/// `feature_compiled` is `cfg!(feature = "browser-devtools")`; `requested` is
/// the `dev` flag from `initialize({ dev: … })`.
///
/// The rule is AND, not OR, and that is the whole security model: with the
/// feature off the sandbox is **absent**, not merely disabled, so a build
/// without it must produce a runtime that is indistinguishable from one that
/// was never asked for a sandbox — no seeded variables, no widened CSP,
/// nothing but the one console warning `initialize()` emits. "Feature off =
/// nothing" is not an optimisation; a relaxed `worker-src`/`frame-src` on a
/// build with no sandbox to use them is pure attack surface.
///
/// Taken as parameters rather than read from `cfg!` inside, so the rule is one
/// pure expression stated once instead of a `cfg!` repeated at each site that
/// consumes it — every consumer reads [`RuntimeFactory::dev_active`], and the
/// raw request is deliberately not stored on the factory at all.
pub const fn resolve_dev_active(feature_compiled: bool, requested: bool) -> bool {
    feature_compiled && requested
}

// The rule, checked by rustc in the configuration actually being built rather
// than by a unit test passing hand-written booleans. `impresspress-web` does
// not compile for the host at all (every module reaches into
// `impresspress_browser`'s `#[cfg(target_arch = "wasm32")]` items), so a
// `#[test]` here would never execute; these `const` assertions do, on every
// `cargo check --target wasm32-unknown-unknown` in both configurations and in
// CI's wasm build. That is also the stronger check: it exercises the real
// `cfg!(feature = …)` wiring below, which a parameterised unit test cannot.
const _: () = assert!(!resolve_dev_active(
    cfg!(feature = "browser-devtools"),
    false
));
#[cfg(not(feature = "browser-devtools"))]
const _: () = assert!(!resolve_dev_active(
    cfg!(feature = "browser-devtools"),
    true
));
#[cfg(feature = "browser-devtools")]
const _: () = assert!(resolve_dev_active(cfg!(feature = "browser-devtools"), true));

/// The browser platform services plus the policy every runtime is built under.
pub struct RuntimeFactory {
    /// The resolved verdict from [`resolve_dev_active`], computed once in
    /// [`RuntimeFactory::new`]. The raw `initialize({ dev })` request is
    /// intentionally *not* retained: keeping only the resolved value is what
    /// makes it impossible for a later consumer to key on "the bundle asked
    /// for it" on a build where the sandbox does not exist.
    pub(crate) dev_active: bool,
    pub(crate) config_svc: Arc<dyn ConfigService>,
    /// Held as the concrete type (not `Arc<dyn CryptoService>`) so
    /// [`crate::BrowserBootHooks`] can rotate the JWT secret through
    /// `set_jwt_secret` after admin's migration seeds it.
    pub(crate) crypto: Arc<impresspress_browser::crypto::BrowserCryptoService>,
    pub(crate) llm: Arc<dyn LlmService>,
    pub(crate) image: Arc<dyn ImageService>,
    pub(crate) vector: Arc<dyn VectorService>,
    pub(crate) embedding: Arc<dyn EmbeddingService>,
    /// The sandbox control plane, once it has been installed. `None` on cold
    /// start — the first runtime is built before there is anything to control.
    #[cfg(feature = "browser-devtools")]
    pub(crate) dev: Option<Arc<DevShared>>,
}

impl RuntimeFactory {
    /// Construct the browser services once.
    /// `BrowserEmbeddingService::new` is the only fallible one.
    pub fn new(options: RuntimeOptions) -> Result<Self, String> {
        let dev_active =
            resolve_dev_active(cfg!(feature = "browser-devtools"), options.dev_enabled);

        let config_svc: Arc<dyn ConfigService> =
            Arc::new(wafer_core::service_blocks::config::EnvConfigService::new());

        // JWT secret can't be loaded yet (the variables table doesn't exist
        // until admin's migration runs). Construct the concrete
        // `BrowserCryptoService` so we keep a typed handle for
        // `set_jwt_secret` in the boot hook; the same allocation is what the
        // builder receives as `Arc<dyn CryptoService>`, so the rotation is
        // observed by every block through the service it already holds.
        let crypto = Arc::new(impresspress_browser::crypto::BrowserCryptoService::new(
            String::new(),
        ));

        let llm: Arc<dyn LlmService> =
            Arc::new(impresspress_browser::llm::BrowserLlmService::new());
        let image: Arc<dyn ImageService> =
            Arc::new(impresspress_browser::image::BrowserImageService::new());
        let vector: Arc<dyn VectorService> =
            Arc::new(impresspress_browser::vector::BrowserVectorService::new());
        // Logged as well as returned: this is the one fallible service, the
        // error surfaces to JS as an opaque `initialize()` rejection, and the
        // console line is what tells an operator *which* service failed.
        let embedding: Arc<dyn EmbeddingService> =
            match impresspress_browser::vector::BrowserEmbeddingService::new() {
                Ok(svc) => Arc::new(svc),
                Err(e) => {
                    web_sys::console::error_1(&format!("BrowserEmbeddingService init: {e}").into());
                    return Err(e);
                }
            };

        Ok(Self {
            dev_active,
            config_svc,
            crypto,
            llm,
            image,
            vector,
            embedding,
            #[cfg(feature = "browser-devtools")]
            dev: None,
        })
    }

    /// Install the sandbox control plane. Every runtime built after this point
    /// carries the `impresspress/dev` block, its `/b/dev` Admin route and the
    /// WRAP grant it needs on the published site.
    #[cfg(feature = "browser-devtools")]
    pub fn with_dev(mut self, dev: Arc<DevShared>) -> Self {
        self.dev = Some(dev);
        self
    }

    /// Build + boot one runtime. `dynamic` is empty on cold start.
    ///
    /// Returns the booted `Wafer` and the storage block the caller needs for
    /// later WRAP-grant republication; the caller decides whether this runtime
    /// becomes the live one (`store_wafer`) or replaces one (`replace_wafer`).
    pub async fn build(
        &self,
        dynamic: &[DynamicBlock],
    ) -> Result<
        (
            wafer_run::Wafer,
            Arc<impresspress_core::blocks::storage::ImpresspressStorageBlock>,
        ),
        JsValue,
    > {
        #[cfg(not(feature = "browser-devtools"))]
        let _ = dynamic;

        // ── Phase 1 ─────────────────────────────────────────────────────────
        // Build with EMPTY config + EMPTY block_settings + EMPTY ConfigSource.
        // None of these can be filled from OPFS yet: the
        // `impresspress__admin__variables` / `impresspress__admin__block_settings`
        // tables only exist after admin's lazy `lifecycle(Init)` runs its
        // migrations — and admin can't run until the wafer is built and sealed.
        //
        // The schema-drift class of bug (#210/#211) came from this crate trying
        // to short-cut that chicken-and-egg with `CREATE TABLE IF NOT EXISTS`
        // pre-creates that duplicated the admin migration schema by hand. Any
        // drift between the two schemas was silent until the first per-block
        // `migration_helper::write_state` upserted into the stale table and
        // failed on a missing column, taking the whole runtime with it.
        //
        // The proper fix is what the native CLI and Cloudflare runner already
        // do: defer seeding until *after* `init_block(admin)`. Admin's
        // migration is the single source of schema truth; this crate just reads
        // back what it created.

        // Empty initial BlockSettings — every block defaults to enabled. The
        // boot hook rewrites this through the handle below once the real
        // settings are loaded.
        let initial_block_settings =
            impresspress_core::features::BlockSettings::from_map(HashMap::new());
        // Empty StaticConfigSource: blocks that look up their declared keys via
        // the runtime's ConfigSource at lifecycle(Init) payload-build time will
        // see nothing. That's fine because impresspress blocks read their keys
        // via `config_client::get` (which hits `wafer-run/config` →
        // ConfigService) rather than the Init payload, and the boot hook
        // populates `config_svc` before triggering any block's Init.
        let cfg_source: Arc<dyn wafer_run::ConfigSource> =
            Arc::new(wafer_run::StaticConfigSource::default());
        let crypto_svc: Arc<dyn CryptoService> = self.crypto.clone();

        // `add_block_config` is a map insert, not a merge — the last
        // declaration for a block name wins outright. So the sandbox's
        // `frame_ancestors` relaxation is folded into this one value and
        // declared once, below; a second `block_config` for the same block
        // would silently drop the CSP.
        //
        // The `mut` on this and on `builder` only bites under
        // `browser-devtools`; without the feature there is no sandbox branch
        // to widen either of them.
        #[cfg_attr(not(feature = "browser-devtools"), allow(unused_mut))]
        let mut security_headers = serde_json::json!({ "csp": self.csp() });

        #[cfg_attr(not(feature = "browser-devtools"), allow(unused_mut))]
        let mut builder = ImpresspressBuilder::new()
            .database(impresspress_browser::make_database_service())
            .storage(impresspress_browser::make_storage_service())
            .config(self.config_svc.clone())
            .crypto(crypto_svc)
            .network(impresspress_browser::make_network_service())
            .logger(impresspress_browser::make_console_logger())
            .llm_service("browser", self.llm.clone())
            .image_service("browser", self.image.clone())
            .vector_service(self.vector.clone())
            .embedding_service(self.embedding.clone())
            .block_settings(initial_block_settings)
            .config_source(cfg_source);

        #[cfg(feature = "browser-devtools")]
        if let Some(dev) = &self.dev {
            use impresspress_core::blocks::dev;

            // `/b/dev` is registered at `RouteAccess::Admin` here, at the
            // router — not by a check inside any handler. That is the single
            // gate keeping the sandbox admin-only.
            //
            // `arc_with_non_send_sync`: `DevShared` holds an
            // `Arc<dyn RuntimeControl>`, whose `MaybeSend + MaybeSync` bound is
            // unbounded on wasm32 so the browser control can hold the live
            // `Rc<Wafer>`. `extra_block` takes an `Arc<dyn Block>` regardless,
            // and wasm32 is single-threaded — the same allowance the rest of
            // the block registration path carries.
            #[allow(clippy::arc_with_non_send_sync)]
            let dev_block: Arc<dyn wafer_run::Block> = Arc::new(dev::DevBlock::new(dev.clone()));
            builder = builder
                .extra_block(dev::BLOCK_NAME, dev_block)
                .add_route(
                    dev::ROUTE_PREFIX,
                    dev::BLOCK_NAME,
                    impresspress_core::routing::RouteAccess::Admin,
                )
                // The published site is owned by `wafer-run/web`, so the dev
                // block cannot declare this grant itself — a block may only
                // grant what it owns. Whoever registers it hands it over.
                .wrap_grants(dev::wrap_grants())
                // A sandbox iteration republishes the site under the same
                // URLs; a cached page would show the previous generation.
                .block_config(
                    "wafer-run/web",
                    serde_json::json!({ "cache_mode": "no-cache" }),
                );
            // The `/b/dev` page previews the live site in a same-origin iframe.
            security_headers["frame_ancestors"] = serde_json::json!("self");

            for (spec, block) in dynamic {
                builder = builder.extra_block(spec.name.clone(), Arc::clone(block));
                for route in &spec.routes {
                    builder = builder.add_route(
                        route.prefix.clone(),
                        spec.name.clone(),
                        route.access.to_route_access(),
                    );
                }
            }
        }

        let builder = builder.block_config("wafer-run/security-headers", security_headers);

        let block_settings_handle = builder.block_settings_handle();
        // The router verifies JWTs against this secret. It's empty at build
        // time (the variables table doesn't exist yet), so grab the handle and
        // rotate it to the seeded value in `seed_after_admin_init`, alongside
        // the crypto service that signs with it.
        let jwt_secret_handle = builder.jwt_secret_handle();

        let (mut wafer, storage_block) = builder
            .build()
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        wafer.set_asset_loader(&impresspress_browser::make_sw_asset_loader());

        // ── Phase 2 ─────────────────────────────────────────────────────────
        // Run the shared boot funnel: seal → init_block(admin) →
        // seed_after_admin_init → init_all_blocks → post_start.
        //
        // admin's `lifecycle(Init)` runs FIRST so its migrations create the
        // canonical `impresspress__admin__variables` + `block_settings` tables
        // before the seed hook reads them — admin's migration is the single
        // source of schema truth (the #210/#211 schema-drift lesson). The hook
        // then seeds + publishes into the services the wafer already holds (see
        // `BrowserBootHooks`), all over `BrowserDatabaseService` rather than
        // the old bridge raw-SQL strings.
        let hooks = crate::BrowserBootHooks {
            db: impresspress_browser::make_database_service(),
            config_svc: self.config_svc.clone(),
            block_settings_handle,
            jwt_secret_handle,
            crypto: self.crypto.clone(),
            dev_active: self.dev_active,
        };
        builder::boot(&mut wafer, &storage_block, &hooks)
            .await
            .map_err(|e| JsValue::from_str(&format!("boot: {e}")))?;

        Ok((wafer, storage_block))
    }

    /// The `Content-Security-Policy` every response is served under.
    ///
    /// Keyed off `dev_active` rather than `dev.is_some()`: the policy is
    /// resolved once per runtime build, and a sandbox activation must not have
    /// to widen headers on a runtime that is already answering requests. A
    /// bundle that was never booted with `{ dev: true }` — or that was, on a
    /// build without `browser-devtools` — therefore carries the unrelaxed
    /// policy, which is what the feature-off smoke asserts.
    fn csp(&self) -> String {
        let mut csp = crate::IMPRESSPRESS_CSP.to_string();
        if self.dev_active {
            // The compiler worker (a same-origin module worker that spawns
            // blob-URL subordinate workers) and the live-site preview iframe
            // on `/b/dev`.
            csp.push_str("; worker-src 'self' blob:; frame-src 'self'");
        }
        csp
    }
}
