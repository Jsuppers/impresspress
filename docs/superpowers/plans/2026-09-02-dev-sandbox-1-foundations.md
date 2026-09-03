# Dev Sandbox Plan 1 — Foundations: storage paths, replaceable runtime, the dev block's control plane

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A browser build with `browser-devtools` on can, over authenticated HTTP alone, accept site files and a precompiled guest block, validate them, activate them as a new generation (site served by `wafer-run/web`, block executed by wasmi behind its own route), survive a service-worker restart, and roll back — with the feature-off build byte-for-byte unchanged.

**Architecture:** The `impresspress/dev` block's control plane (contracts, blob store, generations ledger, validation rules, activation queue, site publisher, journal recovery) lives in `impresspress-core` behind `block-dev` so it is host-testable with `TestContext`; everything that touches wasmi or the `Rc<Wafer>` swap lives in `impresspress-web` behind `browser-devtools` and reaches the core through one `RuntimeControl` trait. The consumer registers the block with `extra_block` + `add_route` only when the bundle's `[dev] enabled` flag reaches `initialize()`. Hierarchical storage keys and the `Rc<Wafer>` runtime land first because both are useful without the sandbox.

**Tech Stack:** Rust (impresspress-core, impresspress-web, impresspress-browser, impresspress-bundle, impresspress CLI), wasm-bindgen, sql.js/OPFS via `bridge.js`, wasmi (`wafer-run/wasmi`), Playwright.

**Spec:** `docs/superpowers/specs/2026-09-02-dev-sandbox-design.md` — §5, §7, §11, §12, §13, §15, §20 (amendments 2, 3, 6).

**Depends on:** Plan 0 merged and pinned (Task 1). Every task after Task 1 assumes the pin.

**Repo and base:** `impresspress`, branch `feat/dev-sandbox` (worktree `/home/joris/Programs/suppers-ai/impresspress-worktrees/dev-sandbox`, from `origin/main`). A fresh worktree cannot build the `impresspress` CLI crate without `crates/impresspress-web/pkg/*.wasm` — copy `pkg/` from a worktree that has one, or run `wasm-pack build --target web --release --out-dir pkg` in `crates/impresspress-web` first.

## Global Constraints

- **Feature off = nothing.** With `browser-devtools` off, or on but `[dev] enabled = false`, there is no `/b/dev` route, no `impresspress__dev__*` migration, no seed fetch, no change to headers or caching. Task 4 pins this with a test.
- **Every `/b/dev` route is `RouteAccess::Admin`.** The router enforces it; handlers do not re-check. Every response is `Cache-Control: no-store`.
- **Table names:** `pub const` per repo module, `impresspress__dev__generations`, `impresspress__dev__builds`, `impresspress__dev__runtime_state`. Storage folders under the block's own namespace: `blobs`, `artifacts`, `workspace.json`; the only cross-block write is `@wafer-run/web/site`, granted via `ImpresspressBuilder::wrap_grants`.
- **Content is never edited in place.** Files are blobs keyed by SHA-256; workspace and generations are manifests of `path → sha256`.
- **Closed enums** for generation status, cause and activation phase. No free strings in handlers.
- **Typed contracts** (`Serialize, Deserialize, schemars::JsonSchema`, `///` doc = published description, `//` for rationale). The dev block joins the `/openapi.json` snapshot gate; never regenerate a snapshot to get green.
- **No raw SQL** in the block: `wafer_core::clients::database` builders only; migrations are SQL files.
- **`Cargo.lock` is committed** whenever a `Cargo.toml` changes (Task 1 and the feature additions). Re-resolve from outside the tree (`cargo metadata --manifest-path <tree>/Cargo.toml` run from the scratchpad) so the repo-level `[patch]` does not write path sources into it.
- Local `cargo test` failures in `impresspress-web`/`impresspress-cloudflare` on the host are expected (CI excludes them); test core natively and the browser crates with `wasm-pack test --node` / Playwright.

---

### Task 1: Pin Plan 0's wafer-run and prove the new APIs are visible

**Files:**
- Modify: `Cargo.toml` (workspace `[dependencies]` `wafer-*` `rev` lines :38-49 and any others), `Cargo.lock`

**Interfaces:**
- Produces: `wafer_block::wire::database::{TableDef, EnsureTableRequest, …}`, `wafer_core::discovery::{generate_webmcp_selected, ToolSelection}`, `wafer-run/web` `cache_mode`, `wafer-run/security-headers` `frame_ancestors` all resolvable in-tree.

- [ ] **Step 1: Bump every `rev`**

Replace every `rev = "61e68a0252324af7a6b233e2d250ec3d0612ca5c"` in `Cargo.toml` with Plan 0's merge SHA (all `wafer-*` lines must agree; a mixed pin fails to resolve).

- [ ] **Step 2: Re-resolve the lock from outside the tree**

```bash
cd "$SCRATCHPAD"
git -C /home/joris/Programs/suppers-ai/impresspress-worktrees/dev-sandbox checkout -- Cargo.lock
cargo metadata --manifest-path /home/joris/Programs/suppers-ai/impresspress-worktrees/dev-sandbox/Cargo.toml --format-version 1 > /dev/null
git -C /home/joris/Programs/suppers-ai/impresspress-worktrees/dev-sandbox diff --stat Cargo.lock
```

Expected: only `wafer-*` entries changed, all to git sources at the new SHA — no `path` sources.

- [ ] **Step 3: Write the failing compile-visibility test**

`crates/impresspress-core/tests/wafer_pin_surface.rs`:

```rust
//! The dev-sandbox plans use these producer APIs; a wrong pin fails here,
//! naming the missing item, instead of deep inside a block.
#[test]
fn producer_surface_is_pinned() {
    let _ = wafer_core::discovery::generate_webmcp_selected;
    let _: fn() -> wafer_core::discovery::ToolSelection = || unreachable!();
    let _ = wafer_block::wire::database::EnsureTableRequest {
        table: wafer_block::wire::database::TableDef {
            name: String::new(), columns: vec![], indexes: vec![], primary_key: vec![], unique_keys: vec![],
        },
    };
    assert_eq!(wafer_block::abi::HOST_CODEC_JSON, 1);
}
```

- [ ] **Step 4: Build and test**

Run: `cargo check --workspace --exclude impresspress-web --exclude impresspress-cloudflare && cargo test -p impresspress-core --test wafer_pin_surface`
Expected: PASS. Then `cargo check -p impresspress-web --target wasm32-unknown-unknown` — PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/impresspress-core/tests/wafer_pin_surface.rs
git commit -m "chore(deps): pin wafer-run to the sandbox producer merge"
```

---

### Task 2: Hierarchical storage keys in OPFS

**Files:**
- Create: `crates/impresspress-browser/js/storage_paths.mjs`
- Create: `crates/impresspress-browser/js/storage_paths.test.mjs`
- Modify: `crates/impresspress-browser/js/bridge.js` (`storagePut` :160, `storageGet` :188, `storageDelete` :217, `storageList` :248)
- Modify: `.github/workflows/ci.yml` (`browser-wasm-test` job :328-372) and `ci-main.yml` mirror

**Interfaces:**
- Consumes: `getFolderHandle(storageRoot, folder, create)` (bridge.js :145).
- Produces: keys may contain `/` (`assets/app.js`); `storageList(folder, prefix, limit, offset)` returns nested keys (`assets/app.js`) sorted, prefix-matched on the full key, metadata sidecars excluded; `storageDelete` removes the leaf and its sidecar and never a sibling; parents are created only by `put`.

- [ ] **Step 1: Write the failing unit tests** (`storage_paths.test.mjs`, run with `node --test`)

```js
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { splitKey, joinKey, validateSegments } from './storage_paths.mjs';

test('splitKey separates directories from the leaf', () => {
  assert.deepEqual(splitKey('assets/js/app.js'), { dirs: ['assets', 'js'], leaf: 'app.js' });
  assert.deepEqual(splitKey('index.html'), { dirs: [], leaf: 'index.html' });
});

test('splitKey rejects traversal, empty and dot segments', () => {
  for (const bad of ['', '/', 'a//b', '../x', 'a/../b', './a', 'a/.', 'a/']) {
    assert.throws(() => splitKey(bad), TypeError, bad);
  }
});

test('splitKey rejects a metadata sidecar name so a caller cannot forge one', () => {
  assert.throws(() => splitKey('index.html.__meta__'), TypeError);
});

test('joinKey is the inverse of splitKey', () => {
  const key = 'assets/js/app.js';
  const { dirs, leaf } = splitKey(key);
  assert.equal(joinKey(dirs, leaf), key);
});

test('validateSegments accepts unicode file names but not separators', () => {
  assert.doesNotThrow(() => validateSegments(['héllo', 'wörld.txt']));
  assert.throws(() => validateSegments(['a\\b']), TypeError);
});
```

- [ ] **Step 2: Run to verify they fail**

Run: `node --test crates/impresspress-browser/js/storage_paths.test.mjs`
Expected: module not found.

- [ ] **Step 3: Implement `storage_paths.mjs`**

```js
// Pure path helpers for OPFS-backed storage. OPFS names cannot contain `/`,
// so a logical key `assets/app.js` is walked as directories + leaf. Kept
// free of DOM/OPFS APIs so `node --test` covers every rule.

const META_SUFFIX = '.__meta__';

export function validateSegments(segments) {
  if (!Array.isArray(segments) || segments.length === 0) {
    throw new TypeError('storage path must have at least one segment');
  }
  for (const s of segments) {
    if (typeof s !== 'string' || s === '' || s === '.' || s === '..') {
      throw new TypeError(`invalid storage path segment: ${JSON.stringify(s)}`);
    }
    if (s.includes('/') || s.includes('\\') || s.includes(' ')) {
      throw new TypeError(`storage path segment contains a separator: ${JSON.stringify(s)}`);
    }
  }
  return segments;
}

/** @returns {{dirs: string[], leaf: string}} */
export function splitKey(key) {
  if (typeof key !== 'string' || key === '' || key.endsWith('/')) {
    throw new TypeError(`invalid storage key: ${JSON.stringify(key)}`);
  }
  const segments = validateSegments(key.split('/'));
  const leaf = segments[segments.length - 1];
  if (leaf.endsWith(META_SUFFIX)) {
    throw new TypeError(`storage key may not name a metadata sidecar: ${key}`);
  }
  return { dirs: segments.slice(0, -1), leaf };
}

export function joinKey(dirs, leaf) {
  return [...dirs, leaf].join('/');
}

export function metaName(leaf) {
  return `${leaf}${META_SUFFIX}`;
}

export function isMetaName(name) {
  return name.endsWith(META_SUFFIX);
}
```

- [ ] **Step 4: Rewrite the four OPFS functions**

In `bridge.js`, `import { splitKey, joinKey, metaName, isMetaName } from './storage_paths.mjs';` and add:

```js
async function getKeyParent(folderHandle, dirs, create) {
    let handle = folderHandle;
    for (const segment of dirs) {
        handle = await handle.getDirectoryHandle(segment, { create });
    }
    return handle;
}
```

`storagePut`: `const { dirs, leaf } = splitKey(key); const parent = await getKeyParent(folderHandle, dirs, true);` then the existing writes against `parent` with `leaf` / `metaName(leaf)`. `storageGet` and `storageDelete`: same with `create = false`. `storageDelete` removes `leaf` then best-effort `metaName(leaf)`; it never removes directories. `storageList` walks recursively:

```js
export async function storageList(folder, prefix, limit, offset) {
    const storageRoot = await getStorageRoot();
    const folderHandle = await getFolderHandle(storageRoot, folder, false);
    const keys = [];
    async function walk(handle, dirs) {
        for await (const [name, entry] of handle.entries()) {
            if (entry.kind === 'directory') {
                await walk(entry, [...dirs, name]);
            } else if (!isMetaName(name)) {
                const key = joinKey(dirs, name);
                if (!prefix || key.startsWith(prefix)) keys.push(key);
            }
        }
    }
    await walk(folderHandle, []);
    keys.sort();
    const total = keys.length;
    const page = keys.slice(offset, limit > 0 ? offset + limit : undefined);
    return { keys: page, total };
}
```

- [ ] **Step 5: Run the unit tests and the wasm tests**

Run: `node --test crates/impresspress-browser/js/storage_paths.test.mjs && wasm-pack test --node crates/impresspress-browser`
Expected: PASS. Add `node --test crates/impresspress-browser/js/storage_paths.test.mjs` as a step in the `browser-wasm-test` job in both workflows (Node is already set up in that job for `wasm-pack test --node`).

- [ ] **Step 6: Commit**

```bash
git add crates/impresspress-browser/js/ .github/workflows/ci.yml .github/workflows/ci-main.yml
git commit -m "feat(browser): hierarchical storage keys in OPFS with validated segments"
```

---

### Task 3: `Rc<Wafer>` runtime with a replace operation

**Files:**
- Modify: `crates/impresspress-browser/src/runtime.rs` (whole file, 95 lines)
- Modify: `crates/impresspress-browser/src/lib.rs` (re-exports :68)

**Interfaces:**
- Produces:

```rust
pub fn is_initialized() -> bool;
pub fn store_wafer(wafer: wafer_run::Wafer) -> Result<(), StoreError>;          // first install only
pub fn replace_wafer(wafer: wafer_run::Wafer) -> Result<Rc<wafer_run::Wafer>, StoreError>; // returns the previous runtime
pub fn current_wafer() -> Option<Rc<wafer_run::Wafer>>;
pub async fn dispatch_request(request: web_sys::Request) -> Result<web_sys::Response, JsValue>;
```
  `dispatch_request` clones the `Rc` synchronously and holds it across its awaits; the raw pointer and its safety comment are gone.

- [ ] **Step 1: Write the failing tests** (`#[cfg(all(test, target_arch = "wasm32"))] mod tests` in `runtime.rs`, `wasm_bindgen_test`)

```rust
    use super::*;
    use wasm_bindgen_test::*;

    fn empty_wafer() -> wafer_run::Wafer {
        let cfg: std::sync::Arc<dyn wafer_run::ConfigSource> =
            std::sync::Arc::new(wafer_run::StaticConfigSource::default());
        wafer_run::Wafer::new(cfg).expect("wafer")
    }

    fn reset() {
        RUNTIME.with(|r| *r.borrow_mut() = None);
    }

    #[wasm_bindgen_test]
    fn first_store_succeeds_and_second_cold_store_fails() {
        reset();
        assert!(!is_initialized());
        store_wafer(empty_wafer()).expect("first store");
        assert!(is_initialized());
        assert!(store_wafer(empty_wafer()).is_err(), "store_wafer is single-shot");
    }

    #[wasm_bindgen_test]
    fn replace_returns_the_previous_runtime_and_keeps_it_alive() {
        reset();
        store_wafer(empty_wafer()).unwrap();
        let held = current_wafer().expect("current");
        let previous = replace_wafer(empty_wafer()).expect("replace");
        assert!(Rc::ptr_eq(&held, &previous), "replace hands back the runtime that was active");
        assert_eq!(Rc::strong_count(&previous), 2, "an in-flight holder keeps the old runtime alive");
        let now = current_wafer().unwrap();
        assert!(!Rc::ptr_eq(&now, &previous));
    }

    #[wasm_bindgen_test]
    fn replace_before_store_is_an_error() {
        reset();
        assert!(replace_wafer(empty_wafer()).is_err());
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `wasm-pack test --node crates/impresspress-browser`
Expected: `replace_wafer`/`current_wafer` undefined.

- [ ] **Step 3: Rewrite `runtime.rs`**

```rust
//! Service-Worker-side Wafer runtime storage and dispatch.
//!
//! The active runtime is an `Rc<Wafer>`. Every dispatch clones the `Rc`
//! before its first `.await`, so a `replace_wafer` that lands while a
//! request is in flight leaves that request on the runtime it started on
//! and routes every later request to the new one. wasm32 is
//! single-threaded, so the thread_local needs no Send/Sync.
use std::{cell::RefCell, rc::Rc};
use wasm_bindgen::prelude::*;
use crate::convert;

thread_local! {
    pub(crate) static RUNTIME: RefCell<Option<Rc<wafer_run::Wafer>>> = const { RefCell::new(None) };
}

#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    AlreadyInitialized,
    NotInitialized,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyInitialized => f.write_str("store_wafer: runtime already initialized"),
            Self::NotInitialized => f.write_str("replace_wafer: runtime not initialized"),
        }
    }
}
impl std::error::Error for StoreError {}

pub fn is_initialized() -> bool {
    RUNTIME.with(|r| r.borrow().is_some())
}

pub fn current_wafer() -> Option<Rc<wafer_run::Wafer>> {
    RUNTIME.with(|r| r.borrow().clone())
}

/// Install the first runtime. Cold initialization only — a second call is an
/// error so an accidental double `initialize()` cannot swap runtimes silently.
pub fn store_wafer(wafer: wafer_run::Wafer) -> Result<(), StoreError> {
    RUNTIME.with(|r| {
        let mut slot = r.borrow_mut();
        if slot.is_some() {
            return Err(StoreError::AlreadyInitialized);
        }
        *slot = Some(Rc::new(wafer));
        Ok(())
    })
}

/// Swap in a rebuilt runtime and hand back the one that was active so the
/// caller can restore it if a later activation step fails.
pub fn replace_wafer(wafer: wafer_run::Wafer) -> Result<Rc<wafer_run::Wafer>, StoreError> {
    RUNTIME.with(|r| {
        let mut slot = r.borrow_mut();
        let previous = slot.take().ok_or(StoreError::NotInitialized)?;
        *slot = Some(Rc::new(wafer));
        Ok(previous)
    })
}

/// Restore a runtime handed back by `replace_wafer`.
pub fn restore_wafer(previous: Rc<wafer_run::Wafer>) {
    RUNTIME.with(|r| *r.borrow_mut() = Some(previous));
}

pub async fn dispatch_request(request: web_sys::Request) -> Result<web_sys::Response, JsValue> {
    let Some(wafer) = current_wafer() else {
        return build_error_response(503, "impresspress-browser: runtime not initialized — call store_wafer() first");
    };
    let (msg, input) = convert::request_to_message(&request).await?;
    let output = wafer.run("site-main", msg, input).await;
    convert::output_to_response(output).await
}

fn build_error_response(status: u16, body: &str) -> Result<web_sys::Response, JsValue> {
    // unchanged from the current file
}
```

Re-export `replace_wafer`, `restore_wafer`, `current_wafer` from `lib.rs`.

- [ ] **Step 4: Run the tests**

Run: `wasm-pack test --node crates/impresspress-browser && cargo check -p impresspress-web -p minimal-browser --target wasm32-unknown-unknown`
Expected: PASS; both consumers still compile (`store_wafer`'s signature is unchanged).

- [ ] **Step 5: Commit**

```bash
git add crates/impresspress-browser/src/runtime.rs crates/impresspress-browser/src/lib.rs
git commit -m "feat(browser): hold the runtime in an Rc so it can be replaced under in-flight requests"
```

---

### Task 4: `browser-devtools`, the `[dev] enabled` flag, and the runtime factory

**Files:**
- Modify: `crates/impresspress-core/Cargo.toml` (features :14-40; add `block-dev = []`, **not** in `default`)
- Modify: `crates/impresspress-web/Cargo.toml` (features :11-15)
- Modify: `crates/impresspress/src/cli/config.rs` (`Config` :4-12; add `DevConfig`)
- Modify: `crates/impresspress/src/cli/flows/embed_web.rs`, `sealed_web.rs` (fill `AppConfig.dev_enabled`)
- Modify: `crates/impresspress-bundle/src/bundle/mod.rs` (`AppConfig`, `build_template_vars` :267)
- Modify: `crates/impresspress-bundle/assets/sw.js.tmpl` (`initialize()` call :55)
- Modify: `crates/impresspress-bundle/tests/bundle_integration.rs` (placeholder coverage)
- Create: `crates/impresspress-web/src/runtime_factory.rs`
- Modify: `crates/impresspress-web/src/lib.rs` (`initialize` :29-151 becomes a thin wrapper), `crates/impresspress-web/src/config.rs` (seed `HAS_LANDING_PAGE` when dev)

**Interfaces:**
- Produces: `impresspress-web` features `dynamic-wasm-blocks = ["wafer-run/wasmi"]` (unchanged) and `browser-devtools = ["dynamic-wasm-blocks", "impresspress-core/block-dev"]`; `impresspress.toml` `[dev] enabled = false` (default); `sw.js` calls `initialize({ dev: __DEV_ENABLED__ })`; `pub async fn initialize(options: JsValue)` reads `options.dev` (missing → false); `RuntimeFactory::build(&self, dynamic: &[DynamicBlockSpec]) -> Result<(Wafer, Arc<ImpresspressStorageBlock>), JsValue>` producing a booted wafer; `RuntimeOptions { dev_enabled: bool }`.
- Consumes: `DynamicBlockSpec` and `RuntimeControl` from Task 5 (define them there; this task compiles against them, so implement Task 5's types first if executing out of order — or land this task with `dynamic` always empty and fill Task 5 in).

- [ ] **Step 1: Write the failing tests**

`crates/impresspress-bundle/tests/bundle_integration.rs` — beside the existing placeholder test, using its fixture directory:

```rust
#[test]
fn sw_passes_the_dev_flag_to_initialize() {
    let dir = fixture_pkg_copy();
    let app = AppConfig { dev_enabled: true, ..AppConfig::default() };
    impresspress_bundle::bundle::run(&dir, &dir, app).unwrap();
    let sw = std::fs::read_to_string(dir.join("sw.js")).unwrap();
    assert!(sw.contains("initialize({ dev: true })"), "{sw}");
}

#[test]
fn sw_defaults_the_dev_flag_to_false() {
    let dir = fixture_pkg_copy();
    impresspress_bundle::bundle::run(&dir, &dir, AppConfig::default()).unwrap();
    let sw = std::fs::read_to_string(dir.join("sw.js")).unwrap();
    assert!(sw.contains("initialize({ dev: false })"), "{sw}");
}
```

`crates/impresspress/src/cli/config.rs` tests:

```rust
    #[test]
    fn dev_table_parses_and_defaults_off() {
        let cfg = Config::parse(r#"[app]
name = "x"
title = "X"
boot_redirect = "/"
[dev]
enabled = true
"#).unwrap();
        assert!(cfg.dev.enabled);
        let cfg = Config::parse(r#"[app]
name = "x"
title = "X"
boot_redirect = "/"
"#).unwrap();
        assert!(!cfg.dev.enabled);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p impresspress-bundle && cargo test -p impresspress config::`
Expected: `dev_enabled` / `dev` fields unknown.

- [ ] **Step 3: Config and bundle**

`config.rs`:

```rust
/// `[dev]` — the browser development sandbox (`impresspress/dev` block,
/// `/b/dev`, dynamic guest blocks). Off by default; also requires the
/// consumer crate to be built with `impresspress-web/browser-devtools`.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DevConfig {
    #[serde(default)]
    pub enabled: bool,
}
```

with `#[serde(default)] pub dev: DevConfig` on `Config`. `AppConfig` in the bundle gains `pub dev_enabled: bool` (derive `Default` for `AppConfig` if it lacks one; `boot_redirect: None` etc. are already the fallbacks). `build_template_vars` inserts `("DEV_ENABLED", if app.dev_enabled { "true" } else { "false" })`. `sw.js.tmpl:55` becomes `await initialize({ dev: __DEV_ENABLED__ });`. `embed_web.rs`/`sealed_web.rs` pass `dev_enabled: c.dev.enabled` (sealed: `false` when there is no `impresspress.toml`).

- [ ] **Step 4: Features**

`impresspress-core/Cargo.toml`: `block-dev = []` (outside `default`). `impresspress-web/Cargo.toml`:

```toml
[features]
dynamic-wasm-blocks = ["wafer-run/wasmi"]
# The browser development sandbox: the impresspress/dev control plane plus
# wasmi for the guest blocks it activates. Off by default; a bundle built
# with it still needs `[dev] enabled = true` to register anything.
browser-devtools = ["dynamic-wasm-blocks", "impresspress-core/block-dev"]
```

- [ ] **Step 5: Extract `RuntimeFactory`**

`crates/impresspress-web/src/runtime_factory.rs` — everything `initialize()` does between `db_init` and `store_wafer`, parameterised by the dynamic block set:

```rust
//! Builds a booted `Wafer` from the browser services. Cold start builds once
//! with no dynamic blocks; an activation that changes the block set builds
//! again with the new set and swaps the runtime (see `dev_runtime`).

use std::sync::Arc;
use impresspress_core::builder::{self, ImpresspressBuilder};
#[cfg(feature = "browser-devtools")]
use impresspress_core::blocks::dev::{DynamicBlockSpec, DevShared};

pub struct RuntimeOptions {
    pub dev_enabled: bool,
}

pub struct RuntimeFactory {
    pub(crate) options: RuntimeOptions,
    pub(crate) config_svc: Arc<dyn wafer_core::interfaces::config::service::ConfigService>,
    pub(crate) crypto: Arc<impresspress_browser::crypto::BrowserCryptoService>,
    pub(crate) llm: Arc<dyn wafer_core::interfaces::llm::service::LlmService>,
    pub(crate) image: Arc<dyn wafer_core::interfaces::image::service::ImageService>,
    pub(crate) vector: Arc<dyn wafer_core::interfaces::vector::service::VectorService>,
    pub(crate) embedding: Arc<dyn wafer_core::interfaces::vector::service::EmbeddingService>,
    #[cfg(feature = "browser-devtools")]
    pub(crate) dev: Option<Arc<DevShared>>,
}

impl RuntimeFactory {
    /// Construct the browser services once. `BrowserEmbeddingService::new`
    /// is the only fallible one.
    pub fn new(options: RuntimeOptions) -> Result<Self, String> { /* moves lines 62-85 of today's initialize() here */ }

    /// Build + boot one runtime. `dynamic` is empty on cold start.
    pub async fn build(
        &self,
        #[cfg_attr(not(feature = "browser-devtools"), allow(unused_variables))]
        dynamic: &[DynamicBlockSpec],
    ) -> Result<(wafer_run::Wafer, Arc<impresspress_core::blocks::storage::ImpresspressStorageBlock>), JsValue> {
        let initial_block_settings = BlockSettings::from_map(HashMap::new());
        let cfg_source: Arc<dyn wafer_run::ConfigSource> = Arc::new(wafer_run::StaticConfigSource::default());
        let crypto_svc: Arc<dyn CryptoService> = self.crypto.clone();

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
            .block_config("wafer-run/security-headers", serde_json::json!({ "csp": csp_for(self.options.dev_enabled) }))
            .config_source(cfg_source);

        #[cfg(feature = "browser-devtools")]
        if let Some(dev) = &self.dev {
            builder = builder
                .extra_block(impresspress_core::blocks::dev::BLOCK_NAME, Arc::new(impresspress_core::blocks::dev::DevBlock::new(dev.clone())))
                .add_route("/b/dev", impresspress_core::blocks::dev::BLOCK_NAME, wafer_core::RouteAccess::Admin)
                .wrap_grants(impresspress_core::blocks::dev::wrap_grants())
                .block_config("wafer-run/web", serde_json::json!({ "cache_mode": "no-cache" }))
                .block_config("wafer-run/security-headers", serde_json::json!({ "csp": csp_for(true), "frame_ancestors": "self" }));
            for spec in dynamic {
                let block = crate::dev_runtime::load_guest(spec)?;
                builder = builder.extra_block(spec.name.clone(), block);
                for route in &spec.routes {
                    builder = builder.add_route(route.prefix.clone(), spec.name.clone(), route.access);
                }
            }
        }

        let block_settings_handle = builder.block_settings_handle();
        let jwt_secret_handle = builder.jwt_secret_handle();
        let (mut wafer, storage_block) = builder.build().map_err(|e| JsValue::from_str(&e.to_string()))?;
        wafer.set_asset_loader(&impresspress_browser::make_sw_asset_loader());
        let hooks = crate::BrowserBootHooks {
            db: impresspress_browser::make_database_service(),
            config_svc: self.config_svc.clone(),
            block_settings_handle,
            jwt_secret_handle,
            crypto: self.crypto.clone(),
            dev_enabled: self.options.dev_enabled,
        };
        builder::boot(&mut wafer, &storage_block, &hooks).await.map_err(|e| JsValue::from_str(&format!("boot: {e}")))?;
        Ok((wafer, storage_block))
    }
}

fn csp_for(dev: bool) -> String {
    let mut csp = crate::IMPRESSPRESS_CSP.to_string();
    if dev {
        // The compiler worker (same-origin module + blob-spawned subordinate
        // workers) and the live-site iframe on /b/dev.
        csp.push_str("; worker-src 'self' blob:; frame-src 'self'");
    }
    csp
}
```

`BrowserBootHooks` gains `dev_enabled: bool`; `seed_after_admin_init` passes it to `config::seed_and_load_variables(&self.db, dev_enabled)`, which seeds `WAFER_RUN_SHARED__HAS_LANDING_PAGE = "true"` and `IMPRESSPRESS__DEV__ENABLED = "true"` (both `seed_variable_if_absent`, non-sensitive) when `dev_enabled`. `initialize(options: JsValue)`:

```rust
#[wasm_bindgen]
pub async fn initialize(options: JsValue) -> Result<(), JsValue> {
    if impresspress_browser::is_initialized() { return Ok(()); }
    let dev_enabled = js_sys::Reflect::get(&options, &"dev".into()).ok().and_then(|v| v.as_bool()).unwrap_or(false);
    impresspress_browser::db_init().await?;
    let factory = RuntimeFactory::new(RuntimeOptions { dev_enabled }).map_err(|e| JsValue::from_str(&e))?;
    let (wafer, _storage) = factory.build(&[]).await?;
    impresspress_browser::store_wafer(wafer).map_err(|e| JsValue::from_str(&e.to_string()))?;
    #[cfg(feature = "browser-devtools")]
    crate::dev_runtime::install(factory).await?;   // Task 9: convergence + seed
    Ok(())
}
```

Without `browser-devtools`, `dev_enabled = true` is accepted and ignored: the block cannot exist, and `dev_status` cannot be asked. Log one console line saying the flag was set on a build without the feature.

- [ ] **Step 6: Feature-off proof**

Add to `crates/impresspress-web/tests/e2e/smoke.spec.ts` (runs in CI's `e2e-smoke` against the default `pkg/`):

```ts
test('the default bundle has no dev block', async ({ page }) => {
  await page.goto('/');
  await page.waitForFunction(() => navigator.serviceWorker.controller !== null);
  const status = await page.evaluate(async () => (await fetch('/b/dev/api/status')).status);
  expect(status).toBe(404);
  const csp = await page.evaluate(async () => (await fetch('/b/auth/login')).headers.get('content-security-policy'));
  expect(csp).not.toContain('worker-src');
});
```

- [ ] **Step 7: Run**

Run: `cargo test -p impresspress-bundle -p impresspress && cargo check -p impresspress-web --target wasm32-unknown-unknown && cargo check -p impresspress-web --target wasm32-unknown-unknown --features browser-devtools`
Expected: PASS (the second `check` will fail until Task 5 defines `blocks::dev`; do Task 5's Step 3 types first if so).

- [ ] **Step 8: Commit**

```bash
git add crates/impresspress-core/Cargo.toml crates/impresspress-web crates/impresspress/src/cli crates/impresspress-bundle Cargo.lock
git commit -m "feat(web): browser-devtools feature, [dev] enabled flag, RuntimeFactory"
```

---

### Task 5: The `impresspress/dev` block skeleton — contracts, migrations, repo, `RuntimeControl`, status

**Files:**
- Create: `crates/impresspress-core/src/blocks/dev/mod.rs`
- Create: `crates/impresspress-core/src/blocks/dev/contracts.rs`
- Create: `crates/impresspress-core/src/blocks/dev/control.rs`
- Create: `crates/impresspress-core/src/blocks/dev/repo/mod.rs`, `repo/generations.rs`, `repo/builds.rs`, `repo/runtime_state.rs`
- Create: `crates/impresspress-core/src/blocks/dev/migrations/mod.rs`, `migrations/001_dev_schema.sqlite.sql`, `migrations/001_dev_schema.postgres.sql`
- Modify: `crates/impresspress-core/src/blocks/mod.rs` (`#[cfg(feature = "block-dev")] pub mod dev;`)
- Modify: `crates/impresspress-core/src/test_support.rs` (`with_dev`)
- Modify: `crates/impresspress-core/tests/openapi_snapshot.rs` (`("dev", &["/b/dev"])`), `crates/impresspress-core/tests/snapshots/dev.openapi.json` (generated, read)
- Create: `crates/impresspress-core/tests/dev_status.rs`

**Interfaces:**
- Produces (`impresspress_core::blocks::dev`):

```rust
pub const BLOCK_NAME: &str = "impresspress/dev";
pub const ROUTE_PREFIX: &str = "/b/dev";
pub struct DevBlock { shared: Arc<DevShared> }
impl DevBlock { pub fn new(shared: Arc<DevShared>) -> Self }
pub struct DevShared { pub control: Arc<dyn RuntimeControl>, pub(crate) activation: ActivationQueue /* Task 7 */ }
impl DevShared { pub fn new(control: Arc<dyn RuntimeControl>) -> Arc<Self> }
pub fn wrap_grants() -> Vec<wafer_run::ResourceGrant>   // read_write(BLOCK_NAME, "wafer-run/web/site/*").typed(Storage)

// control.rs
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DynamicRoute { pub prefix: String, pub access: RouteAccessKind }   // Public | Authenticated | Admin
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DynamicBlockSpec { pub name: String, pub artifact_sha256: String, pub routes: Vec<DynamicRoute>,
                              pub capabilities: wafer_block::BlockCapabilities, pub wafer_guest_version: u32 }
#[wafer_block::wafer_async_trait]
pub trait RuntimeControl: Send + Sync {
    /// Load the artifact under its declared capabilities and limits, parse
    /// BlockInfo, run Init/Start and one probe request. Static rules are
    /// checked by the caller first (Task 8); this is the executable half.
    async fn validate(&self, spec: &DynamicBlockSpec, artifact: &[u8]) -> Result<ValidatedGuest, ValidationFailure>;
    /// Rebuild the runtime with exactly this block set and swap it in.
    async fn rebuild(&self, blocks: &[DynamicBlockSpec]) -> Result<(), String>;
    /// Monotonic runtime generation counter (bumped by every successful rebuild).
    fn runtime_generation(&self) -> u64;
}
pub struct ValidatedGuest { pub info: wafer_block::BlockInfo }
pub struct ValidationFailure { pub stage: ValidationStage, pub message: String }  // Load | Info | Init | Start | Probe
```
  Tables: `repo::generations::TABLE = "impresspress__dev__generations"`, `repo::builds::TABLE = "impresspress__dev__builds"`, `repo::runtime_state::TABLE = "impresspress__dev__runtime_state"`. Endpoint `GET /b/dev/api/status` → `contracts::StatusResponse`.
- Consumes: `impresspress_feature_block!` (`blocks/feature_block.rs:79`), `migration_helper::lifecycle_init`, `endpoint_match`, `TestContext`.

- [ ] **Step 1: Write the failing tests** (`tests/dev_status.rs`)

```rust
use impresspress_core::test_support::{admin_msg, anon_msg, auth_msg, output_json, output_status, TestContext};
use impresspress_core::blocks::dev::test_support::FakeControl;

#[tokio::test]
async fn status_reports_no_generation_on_a_fresh_instance() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let out = ctx.dispatch(admin_msg("retrieve", "/b/dev/api/status")).await;
    assert_eq!(output_status(out).await, 200);
    let body = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/status")).await).await;
    assert_eq!(body["active_generation"], serde_json::Value::Null);
    assert_eq!(body["runtime_generation"], 0);
    assert_eq!(body["blocks"], serde_json::json!([]));
    assert_eq!(body["activation"], serde_json::Value::Null);
}

#[tokio::test]
async fn status_is_admin_only() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    assert_eq!(output_status(ctx.dispatch(anon_msg("retrieve", "/b/dev/api/status")).await).await, 403);
    assert_eq!(output_status(ctx.dispatch(auth_msg("retrieve", "/b/dev/api/status", "u1")).await).await, 403);
}

#[tokio::test]
async fn status_is_never_cached() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let out = ctx.dispatch(admin_msg("retrieve", "/b/dev/api/status")).await;
    assert_eq!(impresspress_core::test_support::output_header(out, "cache-control").await.as_deref(), Some("no-store"));
}

#[test]
fn routes_and_endpoints_stay_in_lockstep() {
    let info = impresspress_core::blocks::dev::DevBlock::new(impresspress_core::blocks::dev::DevShared::new(FakeControl::new())).info();
    assert_eq!(impresspress_core::blocks::dev::ROUTES.len(), info.endpoints.len());
}
```

`ctx.dispatch(msg)` — use whatever `TestContext` exposes to route a message through `routing::route_to_block` (the pattern in `tests/extra_routes_test.rs:592-639`); if `TestContext` only dispatches to a named block, add a `dispatch_routed` helper in `test_support.rs` that runs `routing::route_to_block` with the context's block infos and extra routes.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p impresspress-core --features block-dev --test dev_status`
Expected: `blocks::dev` missing.

- [ ] **Step 3: Migrations**

`migrations/001_dev_schema.sqlite.sql`:

```sql
CREATE TABLE IF NOT EXISTS impresspress__dev__generations (
    id TEXT PRIMARY KEY,
    parent_id TEXT,
    status TEXT NOT NULL,
    cause TEXT NOT NULL,
    site_manifest_json TEXT NOT NULL,
    block_manifest_json TEXT NOT NULL,
    manifest_sha256 TEXT NOT NULL,
    created_at TEXT NOT NULL,
    activated_at TEXT,
    failure_message TEXT
);
CREATE INDEX IF NOT EXISTS idx_impresspress__dev__generations_created ON impresspress__dev__generations(created_at);

CREATE TABLE IF NOT EXISTS impresspress__dev__builds (
    id TEXT PRIMARY KEY,
    block_name TEXT NOT NULL,
    source_manifest_sha256 TEXT NOT NULL,
    artifact_sha256 TEXT NOT NULL,
    block_info_json TEXT NOT NULL,
    diagnostics_json TEXT NOT NULL,
    compiler_version TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS impresspress__dev__runtime_state (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
    active_generation_id TEXT,
    desired_generation_id TEXT,
    activation_phase TEXT NOT NULL,
    generation INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);
INSERT OR IGNORE INTO impresspress__dev__runtime_state (singleton_id, active_generation_id, desired_generation_id, activation_phase, generation, updated_at)
VALUES (1, NULL, NULL, 'idle', 0, '1970-01-01T00:00:00Z');
```

The Postgres file mirrors it (`INSERT … ON CONFLICT DO NOTHING`, `BIGINT`). `migrations/mod.rs` follows `tickets/migrations/mod.rs:1-12` exactly (`SQLITE_MIGRATIONS = &[("001_dev_schema", SQL_001_SQLITE)]`, `POSTGRES_MIGRATIONS` cfg-gated).

- [ ] **Step 4: Repo modules**

`repo/runtime_state.rs`:

```rust
use wafer_core::clients::database as db;
use wafer_block::db::{Filter, FilterOp};
pub const TABLE: &str = "impresspress__dev__runtime_state";

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActivationPhase { Idle, Validating, BuildingRuntime, Publishing, Active, Failed }   // `Active` appears only in progress lists; the journal rests at `Idle`

impl ActivationPhase {
    pub fn as_str(self) -> &'static str { /* idle | validating | building_runtime | publishing | active | failed */ }
    pub fn parse(s: &str) -> Option<Self> { /* inverse */ }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeState {
    pub active_generation_id: Option<String>,
    pub desired_generation_id: Option<String>,
    pub activation_phase: ActivationPhase,
    pub generation: u64,
}

pub async fn read(ctx: &dyn Context) -> Result<RuntimeState, WaferError> {
    let rec = db::get(ctx, TABLE, "1")?;
    // map columns; an unknown phase string is an error, never Idle
}
pub async fn write(ctx: &dyn Context, state: &RuntimeState) -> Result<(), WaferError> {
    db::update(ctx, TABLE, "1", columns_of(state))
}
```

`repo/generations.rs` with `TABLE`, `GenerationStatus { Staged, Validating, Activating, Active, Failed, Superseded }`, `GenerationCause { SiteWrite, SiteDelete, BlockCompile, BlockRemove, Rollback, Seed }` (both `snake_case`, `as_str`/`parse`), `GenerationRow { id, parent_id, status, cause, site_manifest_json, block_manifest_json, manifest_sha256, created_at, activated_at, failure_message }`, and `insert`, `get`, `set_status(ctx, id, status, failure: Option<&str>, activated_at: Option<&str>)`, `list_recent(ctx, limit)`, `mark_superseded_before(ctx, keep: usize)`. `repo/builds.rs` similarly with `BuildStatus { Staged, Valid, Invalid }`. Ids: `uuid::Uuid::new_v4()` (the crate already depends on `uuid` with `js` on wasm32). Timestamps: `chrono::Utc::now().to_rfc3339()` as the other blocks do.

- [ ] **Step 5: `control.rs`, contracts, block**

`contracts.rs`:

```rust
/// Response of `GET /b/dev/api/status`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusResponse {
    /// The active generation, or null on a fresh instance.
    pub active_generation: Option<GenerationSummary>,
    /// Bumped on every runtime rebuild; the page refreshes tool registrations when it changes.
    pub runtime_generation: u64,
    /// Blocks in the active generation.
    pub blocks: Vec<ActiveBlockView>,
    /// The activation in progress, if any.
    pub activation: Option<ActivationView>,
    /// `wafer_guest.rs` version the block scaffolder currently writes.
    pub wafer_guest_version: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GenerationSummary { pub id: String, pub parent_id: Option<String>, pub cause: GenerationCause,
                               pub status: GenerationStatus, pub created_at: String, pub activated_at: Option<String>,
                               pub site_files: u32, pub blocks: u32 }
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActiveBlockView { pub name: String, pub artifact_sha256: String, pub routes: Vec<DynamicRoute> }
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActivationView { pub generation_id: String, pub phase: ActivationPhase, pub detail: String }
```

`mod.rs` — the `impresspress_feature_block!` form cannot take a constructor argument, so write the block by hand in the same shape (struct + `#[wafer_block::wafer_async_trait] impl Block`), with:

```rust
pub const BLOCK_NAME: &str = "impresspress/dev";
pub const ROUTE_PREFIX: &str = "/b/dev";
pub const WAFER_GUEST_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route { ApiStatus }
pub const ROUTES: &[EndpointRoute<Route>] = &[
    EndpointRoute::new(HttpMethod::Get, "/b/dev/api/status", Route::ApiStatus),
];

fn info(&self) -> BlockInfo {
    BlockInfo::new(BLOCK_NAME, "0.1.0", "http-handler@v1", "Browser development sandbox control plane")
        .instance_mode(InstanceMode::Singleton)
        .requires(vec!["wafer-run/database".into(), "wafer-run/storage".into(), "wafer-run/config".into()])
        .collections(vec![CollectionSchema::new(repo::generations::TABLE), CollectionSchema::new(repo::builds::TABLE), CollectionSchema::new(repo::runtime_state::TABLE)])
        .category(wafer_run::BlockCategory::Feature)
        .endpoints(vec![
            BlockEndpoint::get("/b/dev/api/status").summary("Sandbox status").auth(AuthLevel::Admin).output::<contracts::StatusResponse>(),
        ])
        .admin_url("/b/dev")
        .can_disable(false)
}

async fn handle(&self, ctx: &dyn Context, mut msg: Message, input: InputStream) -> OutputStream {
    let Some(route) = endpoint_match::dispatch(&mut msg, ROUTES) else { return ui::not_found_response(&msg); };
    let out = match route { Route::ApiStatus => status::handle(ctx, &self.shared).await };
    no_store(out)
}
```

`no_store` sets `Cache-Control: no-store` on every response (`ResponseBuilder::set_header` — or the response meta `resp.header.Cache-Control`). `lifecycle` = `migration_helper::lifecycle_init(ctx, &event, BLOCK_NAME, migrations::SQLITE_MIGRATIONS, migrations::POSTGRES_MIGRATIONS)`. `wrap_grants()`:

```rust
pub fn wrap_grants() -> Vec<wafer_run::ResourceGrant> {
    vec![wafer_run::ResourceGrant::read_write(BLOCK_NAME, "wafer-run/web/site/*").typed(wafer_block::ResourceType::Storage)]
}
```

`test_support` submodule (`#[cfg(any(test, feature = "test-support"))] pub mod test_support`) with `FakeControl { pub rebuilt: Mutex<Vec<Vec<DynamicBlockSpec>>>, pub validate_result: Mutex<Result<(), ValidationFailure>>, generation: AtomicU64 }` implementing `RuntimeControl` (records calls, bumps the counter on `rebuild`). `TestContext::with_dev(control: Arc<dyn RuntimeControl>)` registers `DevBlock::new(DevShared::new(control))`, runs its migrations, and adds the `/b/dev` Admin extra route plus `wrap_grants()`.

- [ ] **Step 6: Snapshot gate**

Add `("dev", &["/b/dev"])` to `SNAPSHOTTED_BLOCKS` and include the block in `real_block_infos()` under `#[cfg(feature = "block-dev")]` (construct it with a `FakeControl`). Generate the first snapshot with `UPDATE_OPENAPI_SNAPSHOTS=1 cargo test -p impresspress-core --features block-dev --test openapi_snapshot` and read it: one path, one typed response.

- [ ] **Step 7: Run**

Run: `cargo test -p impresspress-core --features block-dev --test dev_status --test openapi_snapshot && cargo test -p impresspress-core`
Expected: PASS; the second command (default features, no `block-dev`) proves the block is absent from the default build.

- [ ] **Step 8: Commit**

```bash
git add crates/impresspress-core
git commit -m "feat(dev): impresspress/dev block skeleton — status, migrations, RuntimeControl"
```

---

### Task 6: Workspace store — content-addressed blobs, the workspace manifest, and the files API

**Files:**
- Create: `crates/impresspress-core/src/blocks/dev/paths.rs`
- Create: `crates/impresspress-core/src/blocks/dev/blobs.rs`
- Create: `crates/impresspress-core/src/blocks/dev/workspace.rs`
- Create: `crates/impresspress-core/src/blocks/dev/files.rs` (handlers)
- Modify: `crates/impresspress-core/src/blocks/dev/{mod.rs, contracts.rs}`
- Create: `crates/impresspress-core/tests/dev_files.rs`

**Interfaces:**
- Produces:

```rust
// paths.rs
pub const MAX_FILE_BYTES: usize = 512 * 1024;
pub const MAX_FILES: usize = 2_000;
pub const MAX_WORKSPACE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_BLOCKS: usize = 16;
pub enum WorkspaceArea { Site, Block(String) }        // `site/…` or `blocks/<name>/…`
pub fn validate_path(path: &str) -> Result<WorkspaceArea, PathError>;   // relative, no `.`/`..`/empty/backslash/control, ≤ 255 bytes/segment, ≤ 1024 total
pub fn block_name_is_valid(name: &str) -> bool;       // ^[a-z][a-z0-9_]{1,31}$
pub fn content_type_for(path: &str) -> &'static str;  // by extension; `application/octet-stream` fallback

// blobs.rs — storage folder `blobs`, key = sha256 hex
pub fn sha256_hex(bytes: &[u8]) -> String;
pub async fn put(ctx, bytes: &[u8]) -> Result<String /*sha*/, WaferError>;   // idempotent
pub async fn get(ctx, sha: &str) -> Result<Vec<u8>, WaferError>;
pub async fn exists(ctx, sha: &str) -> Result<bool, WaferError>;
pub async fn delete(ctx, sha: &str) -> Result<(), WaferError>;

// workspace.rs — storage key `workspace.json` in the block's namespace
#[derive(Serialize, Deserialize, JsonSchema, Clone, PartialEq, Eq)]
pub struct FileEntry { pub path: String, pub sha256: String, pub size: u64, pub content_type: String }
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Workspace { pub files: BTreeMap<String, FileEntry> }
pub async fn load(ctx) -> Result<Workspace, WaferError>;   // missing = empty
pub async fn save(ctx, ws: &Workspace) -> Result<(), WaferError>;
pub fn site_manifest(ws: &Workspace) -> Vec<FileEntry>;      // entries under `site/`, path with the prefix stripped, sorted
pub fn block_sources(ws: &Workspace, name: &str) -> Vec<FileEntry>;
```
  Endpoints: `GET /b/dev/api/files?prefix=` → `FileListResponse { files: Vec<FileEntry> }`; `POST /b/dev/api/files/read` `{path}` → `FileReadResponse { path, sha256, size, encoding: "utf8"|"base64", content }`; `POST /b/dev/api/files/write` `{path, content, encoding?, expected_sha256: Option<String>}` → `FileWriteResponse { path, sha256, size, generation: Option<GenerationSummary> }`; `POST /b/dev/api/files/delete` `{path, expected_sha256}` → `FileDeleteResponse { path, generation: Option<GenerationSummary> }`. Conflicts: `409` with `FileConflict { path, current_sha256: Option<String>, current_size: Option<u64> }` as the JSON body.
- Consumes: `wafer_core::clients::storage` (`put/get/delete/list`), `sha2`, `base64`.
- Note: in this task `generation` is always `None`; Task 7 wires site writes to activation.

- [ ] **Step 1: Write the failing tests** (`tests/dev_files.rs`, using `TestContext::with_dev` and a `dev_post(path, json)` helper built on `admin_msg("create", …)` with a JSON `InputStream`)

```rust
#[tokio::test]
async fn write_then_list_then_read_round_trips_with_hashes() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let w = output_json(dev_post(&ctx, "/b/dev/api/files/write",
        json!({"path": "site/index.html", "content": "<h1>hi</h1>", "expected_sha256": null})).await).await;
    assert_eq!(w["path"], "site/index.html");
    assert_eq!(w["size"], 11);
    let sha = w["sha256"].as_str().unwrap().to_string();
    assert_eq!(sha.len(), 64);

    let l = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/files?prefix=site/")).await).await;
    assert_eq!(l["files"][0]["path"], "site/index.html");
    assert_eq!(l["files"][0]["sha256"], sha);
    assert_eq!(l["files"][0]["content_type"], "text/html; charset=utf-8");

    let r = output_json(dev_post(&ctx, "/b/dev/api/files/read", json!({"path": "site/index.html"})).await).await;
    assert_eq!(r["encoding"], "utf8");
    assert_eq!(r["content"], "<h1>hi</h1>");
}

#[tokio::test]
async fn write_requires_null_expected_hash_for_a_new_file() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let out = dev_post(&ctx, "/b/dev/api/files/write",
        json!({"path": "site/a.css", "content": "a{}", "expected_sha256": "00"})).await;
    assert_eq!(output_status(out).await, 409);
}

#[tokio::test]
async fn stale_hash_is_a_conflict_that_reports_the_current_hash() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let w = output_json(dev_post(&ctx, "/b/dev/api/files/write",
        json!({"path": "site/a.css", "content": "a{}", "expected_sha256": null})).await).await;
    let current = w["sha256"].as_str().unwrap().to_string();
    let out = dev_post(&ctx, "/b/dev/api/files/write",
        json!({"path": "site/a.css", "content": "b{}", "expected_sha256": "deadbeef"})).await;
    assert_eq!(output_status(out).await, 409);
    // re-dispatch to read the body (dispatch consumes the stream)
    let body = output_json(dev_post(&ctx, "/b/dev/api/files/write",
        json!({"path": "site/a.css", "content": "b{}", "expected_sha256": "deadbeef"})).await).await;
    assert_eq!(body["current_sha256"], current);
}

#[tokio::test]
async fn binary_files_round_trip_as_base64() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let png = base64::engine::general_purpose::STANDARD.encode([0x89, b'P', b'N', b'G', 0, 1, 2]);
    dev_post(&ctx, "/b/dev/api/files/write",
        json!({"path": "site/assets/dot.png", "content": png, "encoding": "base64", "expected_sha256": null})).await;
    let r = output_json(dev_post(&ctx, "/b/dev/api/files/read", json!({"path": "site/assets/dot.png"})).await).await;
    assert_eq!(r["encoding"], "base64");
    assert_eq!(r["content"], png);
}

#[tokio::test]
async fn paths_outside_site_and_blocks_are_rejected() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    for bad in ["../x", "site/../../etc", "sw.js", "site//a", "blocks/Bad Name/src/lib.rs", "site/a\\b", ""] {
        let out = dev_post(&ctx, "/b/dev/api/files/write", json!({"path": bad, "content": "x", "expected_sha256": null})).await;
        assert_eq!(output_status(out).await, 400, "{bad}");
    }
}

#[tokio::test]
async fn delete_with_matching_hash_removes_the_entry_and_keeps_the_blob_for_history() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let w = output_json(dev_post(&ctx, "/b/dev/api/files/write",
        json!({"path": "site/a.css", "content": "a{}", "expected_sha256": null})).await).await;
    let sha = w["sha256"].as_str().unwrap().to_string();
    let out = dev_post(&ctx, "/b/dev/api/files/delete", json!({"path": "site/a.css", "expected_sha256": sha})).await;
    assert_eq!(output_status(out).await, 200);
    let l = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/files")).await).await;
    assert!(l["files"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn file_size_quota_is_enforced() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let big = "x".repeat(512 * 1024 + 1);
    let out = dev_post(&ctx, "/b/dev/api/files/write", json!({"path": "site/big.txt", "content": big, "expected_sha256": null})).await;
    assert_eq!(output_status(out).await, 413);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p impresspress-core --features block-dev --test dev_files`
Expected: 404 on every route.

- [ ] **Step 3: Implement `paths.rs`**

```rust
#[derive(Debug, PartialEq, Eq)]
pub enum PathError { Empty, TooLong, BadSegment(String), OutsideWorkspace, BadBlockName(String) }

pub fn validate_path(path: &str) -> Result<WorkspaceArea, PathError> {
    if path.is_empty() { return Err(PathError::Empty); }
    if path.len() > 1024 { return Err(PathError::TooLong); }
    let segments: Vec<&str> = path.split('/').collect();
    for s in &segments {
        if s.is_empty() || *s == "." || *s == ".." || s.len() > 255
            || s.contains('\\') || s.chars().any(char::is_control) {
            return Err(PathError::BadSegment((*s).to_string()));
        }
    }
    match segments.as_slice() {
        ["site", rest @ ..] if !rest.is_empty() => Ok(WorkspaceArea::Site),
        ["blocks", name, rest @ ..] if !rest.is_empty() => {
            if !block_name_is_valid(name) { return Err(PathError::BadBlockName((*name).to_string())); }
            Ok(WorkspaceArea::Block((*name).to_string()))
        }
        _ => Err(PathError::OutsideWorkspace),
    }
}
```

Spaces are allowed inside a segment. `block_name_is_valid`: first byte `a-z`, then 1–31 of `a-z0-9_`. `content_type_for`: `html→text/html; charset=utf-8`, `css→text/css; charset=utf-8`, `js|mjs→application/javascript; charset=utf-8`, `json→application/json`, `svg→image/svg+xml`, `png`, `jpg|jpeg`, `gif`, `webp`, `ico→image/x-icon`, `txt|md→text/plain; charset=utf-8`, `rs|toml→text/plain; charset=utf-8`, `wasm→application/wasm`, `woff2→font/woff2`, else `application/octet-stream`.

- [ ] **Step 4: Implement `blobs.rs` and `workspace.rs`**

Blobs go through `wafer_core::clients::storage::put(ctx, "blobs", &sha, bytes, "application/octet-stream")` (folder is the block's own `blobs`; the storage block prefixes `impresspress/dev/`). `put` checks `exists` first. `workspace.rs` stores `serde_json::to_vec_pretty(&Workspace)` at folder `""`/key `workspace.json` (own namespace); `load` maps `StorageError::NotFound` → `Workspace::default()`.

- [ ] **Step 5: Implement `files.rs`**

Write handler, in order: parse body (`crud::read_json_body`, 400 on bad JSON); `validate_path` (400 with the `PathError` text); decode content (`encoding` `utf8` default, `base64` via `base64::engine::general_purpose::STANDARD`; 400 on bad base64); size check (413); `workspace::load`; compare `expected_sha256` with the current entry (`None` ↔ absent, `Some(x)` ↔ `entry.sha256 == x`; else 409 `FileConflict`); quota checks (`MAX_FILES`, `MAX_WORKSPACE_BYTES` over all entries, `MAX_BLOCKS` distinct block names — 413/409 respectively); `blobs::put`; insert the `FileEntry`; `workspace::save`; respond `FileWriteResponse { generation: None }`. Read: `validate_path`, look up, `blobs::get`, `utf8` if `std::str::from_utf8` succeeds and the content type is textual, else base64. Delete: same conflict rules; removes the entry only (blobs are reclaimed by Plan 4's GC). List: `?prefix=` filter, sorted by path.

Declare the four endpoints in `info()` (`.input::<FileReadRequest>().output::<FileReadResponse>()` etc.; the list route `.query_params::<FileListQuery>().output::<FileListResponse>()`) and add them to `ROUTES` in the same order. Regenerate and read the openapi snapshot.

- [ ] **Step 6: Run**

Run: `cargo test -p impresspress-core --features block-dev --test dev_files --test dev_status --test openapi_snapshot`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/impresspress-core
git commit -m "feat(dev): content-addressed workspace with hash-checked file writes"
```

---

### Task 7: Generations, the activation queue, the site publisher and journal recovery

**Files:**
- Create: `crates/impresspress-core/src/blocks/dev/generation.rs`
- Create: `crates/impresspress-core/src/blocks/dev/activation.rs`
- Create: `crates/impresspress-core/src/blocks/dev/publisher.rs`
- Modify: `crates/impresspress-core/src/blocks/dev/{mod.rs, contracts.rs, files.rs, control.rs}`
- Create: `crates/impresspress-core/tests/dev_activation.rs`

**Interfaces:**
- Produces:

```rust
// generation.rs
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema)]
pub struct GenerationManifest { pub schema_version: u32, pub generation_id: String, pub parent_id: Option<String>,
                                pub site: SiteManifest, pub blocks: Vec<DynamicBlockSpec> }
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, Default)]
pub struct SiteManifest { pub files: Vec<FileEntry> }     // path relative to site root, sorted
pub fn canonical_json(m: &GenerationManifest) -> Vec<u8>; // sorted keys, no whitespace (serde_json with BTreeMap-backed Value)
pub fn manifest_sha256(m: &GenerationManifest) -> String;
pub fn diff(prev: Option<&GenerationManifest>, next: &GenerationManifest) -> GenerationDiff;   // added/changed/removed paths, added/changed/removed blocks
pub fn block_set_changed(prev: Option<&GenerationManifest>, next: &GenerationManifest) -> bool;

// activation.rs
pub struct ActivationQueue { inner: Mutex<QueueState> }   // in DevShared
pub struct ActivationOutcome { pub generation: GenerationSummary, pub progress: Vec<ProgressStep> }
#[derive(Serialize, Deserialize, Clone, JsonSchema)] pub struct ProgressStep { pub phase: ActivationPhase, pub ms: u64, pub detail: String }
pub async fn request(ctx: &dyn Context, shared: &DevShared, cause: GenerationCause, next: GenerationManifest) -> Result<ActivationOutcome, ActivationError>;
pub async fn converge_on_boot(ctx: &dyn Context, shared: &DevShared) -> Result<(), String>;   // Task 9 calls it after boot
pub enum ActivationError { Validation(Vec<String>), Runtime(String), Storage(String) }   // → 422 / 500 / 500

// publisher.rs
pub async fn publish_site(ctx, prev: Option<&SiteManifest>, next: &SiteManifest) -> Result<(), WaferError>;  // only changed files; deletes removed; index.html last
```
  Endpoints: `GET /b/dev/api/generations?limit=` → `GenerationListResponse { generations: Vec<GenerationSummary> }`; `GET /b/dev/api/generations/{id}` → `GenerationDetail { summary, manifest, diff_from_parent }`; `POST /b/dev/api/generations/{id}/rollback` → `ActivationResponse { generation, progress }`. `files/write` and `files/delete` under `site/` now return `generation: Some(..)`.
- Consumes: `RuntimeControl::rebuild` and `runtime_generation`, repo modules, `wrap_grants()` for `@wafer-run/web/site`.

- [ ] **Step 1: Write the failing tests** (`tests/dev_activation.rs`)

```rust
#[tokio::test]
async fn a_site_write_creates_and_activates_a_generation_without_rebuilding_the_runtime() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;
    let w = output_json(dev_post(&ctx, "/b/dev/api/files/write",
        json!({"path": "site/index.html", "content": "<h1>v1</h1>", "expected_sha256": null})).await).await;
    assert_eq!(w["generation"]["cause"], "site_write");
    assert_eq!(w["generation"]["status"], "active");
    assert!(control.rebuilds().is_empty(), "site-only changes never rebuild the runtime");

    // The published site folder holds exactly the manifest.
    let served = ctx.storage_get("wafer-run/web", "site", "index.html").await.unwrap();
    assert_eq!(served, b"<h1>v1</h1>");
    let status = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/status")).await).await;
    assert_eq!(status["active_generation"]["id"], w["generation"]["id"]);
}

#[tokio::test]
async fn deleting_a_site_file_removes_it_from_the_served_folder() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;
    let w = output_json(dev_post(&ctx, "/b/dev/api/files/write",
        json!({"path": "site/a.css", "content": "a{}", "expected_sha256": null})).await).await;
    let sha = w["sha256"].as_str().unwrap().to_string();
    dev_post(&ctx, "/b/dev/api/files/delete", json!({"path": "site/a.css", "expected_sha256": sha})).await;
    assert!(ctx.storage_get("wafer-run/web", "site", "a.css").await.is_err());
}

#[tokio::test]
async fn block_source_writes_do_not_create_generations() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let w = output_json(dev_post(&ctx, "/b/dev/api/files/write",
        json!({"path": "blocks/hello/src/lib.rs", "content": "// rust", "expected_sha256": null})).await).await;
    assert_eq!(w["generation"], serde_json::Value::Null);
    let l = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/generations")).await).await;
    assert!(l["generations"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn rollback_republishes_an_earlier_generation_as_a_new_one() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;
    let g1 = output_json(dev_post(&ctx, "/b/dev/api/files/write",
        json!({"path": "site/index.html", "content": "v1", "expected_sha256": null})).await).await;
    let sha1 = g1["sha256"].as_str().unwrap().to_string();
    dev_post(&ctx, "/b/dev/api/files/write",
        json!({"path": "site/index.html", "content": "v2", "expected_sha256": sha1})).await;
    assert_eq!(ctx.storage_get("wafer-run/web", "site", "index.html").await.unwrap(), b"v2");

    let id1 = g1["generation"]["id"].as_str().unwrap();
    let r = output_json(dev_post(&ctx, &format!("/b/dev/api/generations/{id1}/rollback"), json!({})).await).await;
    assert_eq!(r["generation"]["cause"], "rollback");
    assert_ne!(r["generation"]["id"], id1, "history is append-only");
    assert_eq!(ctx.storage_get("wafer-run/web", "site", "index.html").await.unwrap(), b"v1");
    // The workspace follows the rollback so the next edit starts from v1.
    let read = output_json(dev_post(&ctx, "/b/dev/api/files/read", json!({"path": "site/index.html"})).await).await;
    assert_eq!(read["content"], "v1");
}

#[tokio::test]
async fn a_failed_runtime_rebuild_leaves_the_previous_generation_active() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;
    dev_post(&ctx, "/b/dev/api/files/write", json!({"path": "site/index.html", "content": "v1", "expected_sha256": null})).await;
    let before = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/status")).await).await;
    control.fail_next_rebuild("wasmi: boom");
    // Task 8 stages a block; here drive the queue directly with a manifest that carries one.
    let err = impresspress_core::blocks::dev::activation::request(
        &ctx.context(), &ctx.dev_shared(), GenerationCause::BlockCompile, manifest_with_block(&ctx).await).await.err().unwrap();
    assert!(matches!(err, ActivationError::Runtime(m) if m.contains("boom")));
    let after = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/status")).await).await;
    assert_eq!(after["active_generation"]["id"], before["active_generation"]["id"]);
    let l = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/generations")).await).await;
    assert_eq!(l["generations"][0]["status"], "failed");
}

#[tokio::test]
async fn boot_converges_an_interrupted_activation() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;
    dev_post(&ctx, "/b/dev/api/files/write", json!({"path": "site/index.html", "content": "v1", "expected_sha256": null})).await;
    // Simulate a crash mid-activation: desired points at a staged generation.
    let staged = insert_staged_generation(&ctx, "v2").await;   // helper: inserts a Staged row + blobs, sets desired_generation_id + phase=publishing
    impresspress_core::blocks::dev::activation::converge_on_boot(&ctx.context(), &ctx.dev_shared()).await.unwrap();
    let state = impresspress_core::blocks::dev::repo::runtime_state::read(&ctx.context()).await.unwrap();
    assert_eq!(state.desired_generation_id, None);
    assert_eq!(state.activation_phase, ActivationPhase::Idle);
    assert_eq!(state.active_generation_id.as_deref(), Some(staged.as_str()));
    assert_eq!(ctx.storage_get("wafer-run/web", "site", "index.html").await.unwrap(), b"v2");
}

#[tokio::test]
async fn only_the_last_twenty_generations_are_retained() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let mut sha: Option<String> = None;
    for i in 0..25 {
        let w = output_json(dev_post(&ctx, "/b/dev/api/files/write",
            json!({"path": "site/index.html", "content": format!("v{i}"), "expected_sha256": sha})).await).await;
        sha = Some(w["sha256"].as_str().unwrap().to_string());
    }
    let l = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/generations?limit=100")).await).await;
    let statuses: Vec<&str> = l["generations"].as_array().unwrap().iter().map(|g| g["status"].as_str().unwrap()).collect();
    assert_eq!(statuses.iter().filter(|s| **s == "active").count(), 1);
    assert_eq!(statuses.iter().filter(|s| **s == "superseded").count(), 5, "25 made, 20 retained: {statuses:?}");
}
```

`ctx.storage_get(block, folder, key)` and `ctx.context()` / `ctx.dev_shared()` are small additions to `TestContext` (direct reads of the in-memory storage service through the impresspress storage block's namespace rules).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p impresspress-core --features block-dev --test dev_activation`
Expected: `generation` is null, storage empty, routes 404.

- [ ] **Step 3: Implement `generation.rs`**

`canonical_json`: serialize to `serde_json::Value`, convert every object to a `BTreeMap` recursively (a `fn canonicalize(v: Value) -> Value`), then `serde_json::to_vec`. `manifest_sha256 = sha256_hex(&canonical_json(m))`. `diff` compares `site.files` by path/sha and `blocks` by name/artifact.

- [ ] **Step 4: Implement `publisher.rs`**

```rust
pub async fn publish_site(ctx: &dyn Context, prev: Option<&SiteManifest>, next: &SiteManifest) -> Result<(), WaferError> {
    let prev_by_path: BTreeMap<&str, &FileEntry> = prev.map(|p| p.files.iter().map(|f| (f.path.as_str(), f)).collect()).unwrap_or_default();
    let next_by_path: BTreeMap<&str, &FileEntry> = next.files.iter().map(|f| (f.path.as_str(), f)).collect();
    // 1. every changed non-entrypoint file
    for (path, entry) in &next_by_path {
        if *path == "index.html" { continue; }
        if prev_by_path.get(path).map(|p| p.sha256 == entry.sha256).unwrap_or(false) { continue; }
        let bytes = blobs::get(ctx, &entry.sha256).await?;
        storage::put(ctx, SITE_FOLDER, path, &bytes, &entry.content_type).await?;
    }
    // 2. removed files
    for path in prev_by_path.keys() {
        if !next_by_path.contains_key(path) { storage::delete(ctx, SITE_FOLDER, path).await?; }
    }
    // 3. the entrypoint last
    if let Some(entry) = next_by_path.get("index.html") {
        if !prev_by_path.get("index.html").map(|p| p.sha256 == entry.sha256).unwrap_or(false) {
            let bytes = blobs::get(ctx, &entry.sha256).await?;
            storage::put(ctx, SITE_FOLDER, "index.html", &bytes, &entry.content_type).await?;
        }
    }
    Ok(())
}
pub const SITE_FOLDER: &str = "@wafer-run/web/site";
```

- [ ] **Step 5: Implement `activation.rs`**

The queue holds `QueueState { running: bool, pending: Option<(GenerationCause, GenerationManifest, Vec<oneshot::Sender<…>>)> }`. Because the browser runtime is single-threaded and the dev block may be re-instantiated on rebuild, keep the queue in `DevShared` (`Arc`), not on the block. `request`:

1. Insert the generation row `Staged` (manifest JSON, sha, parent = current active).
2. If `running`, merge into `pending` (latest manifest wins; every waiter gets the final outcome) and await; else mark `running` and proceed with this manifest.
3. `runtime_state::write { desired = id, phase = Validating }`.
4. Verify every `site.files[].sha256` and `blocks[].artifact_sha256` exists in blobs/artifacts (else `Validation`).
5. If `block_set_changed(prev, next)`: `phase = BuildingRuntime`; `shared.control.rebuild(&next.blocks).await` (`Runtime` error → mark `Failed`, restore state `desired = None, phase = Idle`, return).
6. `phase = Publishing`; `publish_site(prev_site, &next.site)`; on error, if the runtime was rebuilt, `control.rebuild(prev_blocks)` to restore, then `Failed`.
7. `generations::set_status(id, Active, activated_at)`, previous active → `Superseded`, `runtime_state::write { active = id, desired = None, phase = Idle, generation += 1 }`, `mark_superseded_before(keep = 20)`.
8. Record `ProgressStep`s with elapsed ms per phase; release `running`; if `pending` exists, loop with it.

`converge_on_boot`: read state; if `desired.is_some()` → load that generation's manifest and re-run steps 4–7 (`Rollback`-style, cause preserved); if it fails, restore `active`'s site files from its manifest and clear `desired`. If `desired.is_none()` and `active` has blocks, the browser side rebuilds with them (Task 9) — this function only returns the active block set: make it `-> Result<Vec<DynamicBlockSpec>, String>`.

Wire `files.rs`: after a successful `site/` write or delete, build `next = GenerationManifest { site: workspace::site_manifest(&ws), blocks: current active blocks }` and call `request(..)`; map `ActivationError` to 422/500 with the message; put the `GenerationSummary` in the response. Rollback handler: load target manifest, copy into a new manifest with `parent = active`, **also** rewrite the workspace's `site/` entries to match the target (so later edits start from the rolled-back content), then `request(Rollback, ..)`.

- [ ] **Step 6: Run**

Run: `cargo test -p impresspress-core --features block-dev --test dev_activation --test dev_files --test openapi_snapshot`
Expected: PASS after adding the three generation endpoints to `info()`/`ROUTES` and reading the snapshot diff.

- [ ] **Step 7: Verify load-bearing**

Swap steps 1 and 3 of `publish_site` (entrypoint first); `deleting_a_site_file…` still passes but add an assertion-free ordering test: a `ScriptedStorage` that records `put` order for a manifest with `index.html` + `a.css` must see `a.css` before `index.html`. Keep that test.

- [ ] **Step 8: Commit**

```bash
git add crates/impresspress-core
git commit -m "feat(dev): generations, coalescing auto-activation, site publisher, boot convergence"
```

---

### Task 8: Block staging, static validation, removal

**Files:**
- Create: `crates/impresspress-core/src/blocks/dev/validation.rs`
- Create: `crates/impresspress-core/src/blocks/dev/blocks_api.rs` (handlers)
- Modify: `crates/impresspress-core/src/blocks/dev/{mod.rs, contracts.rs}`
- Create: `crates/impresspress-core/tests/dev_blocks.rs`

**Interfaces:**
- Produces:

```rust
// validation.rs
pub const MAX_ARTIFACT_BYTES: usize = 4 * 1024 * 1024;
pub const RESERVED_NAME_PREFIXES: &[&str] = &["wafer-run/", "impresspress/"];
pub struct StaticValidation { pub spec: DynamicBlockSpec }
pub fn validate_static(
    name: &str, info: &wafer_block::BlockInfo, artifact_sha256: &str,
    builtin_routes: &[&str],                 // routing::ROUTES prefixes + existing extra routes
    active: &[DynamicBlockSpec],            // other blocks in the target generation
) -> Result<DynamicBlockSpec, Vec<Diagnostic>>;
#[derive(Serialize, Deserialize, Clone, JsonSchema)]
pub struct Diagnostic { pub severity: Severity, pub code: String, pub message: String, pub file: Option<String>, pub line: Option<u32>, pub column: Option<u32> }
```
  Rules (each a `code`): `name-format` (`site/<name>` with `block_name_is_valid`), `name-reserved`, `name-mismatch` (BlockInfo.name ≠ manifest), `route-prefix` (every route under `/b/<name>/`, normalized, trailing slash), `route-collision` (built-in or another dynamic block), `endpoint-outside-routes`, `tool-name-duplicate` (against built-ins' and other dynamic blocks' `agent_tool` names — the producer's `seal()` is the last line, this is the first), `cap-collection`, `cap-folder`, `cap-config`, `cap-raw-sql`, `cap-ddl` (raw `ddl` must be false; the structured `schema` capability may be true — spec amendment #10), `cap-network`, `cap-crypto`, `cap-vector`, `cap-callable`, `cap-requires-mismatch`, `artifact-too-large`, `wafer-guest-version` (must equal `WAFER_GUEST_VERSION` when the guest declares one in `BlockInfo.description` — omit this rule in Plan 1; Plan 3 adds the declaration).
  Endpoints: `POST /b/dev/api/builds/stage` `StageBuildRequest { block_name, artifact_base64, source_manifest_sha256: Option<String>, compiler_version: String, diagnostics: Vec<Diagnostic> }` → `StageBuildResponse { build_id, success, diagnostics, generation: Option<GenerationSummary>, progress }` (`success:false` carries diagnostics with HTTP 200 — a refused block is a result, not a transport failure; only malformed requests are 4xx); `POST /b/dev/api/blocks/{name}/remove` → `ActivationResponse`.
- Consumes: `RuntimeControl::validate` for the executable half, `activation::request`.

- [ ] **Step 1: Write the failing tests** (`tests/dev_blocks.rs`)

Use the proof guest's bytes: build `experiments/browser-service-worker-blocks/guest` for `wasm32-wasip1` in a `build.rs`-free way — the test reads `EXPERIMENT_GUEST_WASM` env var or the default path `experiments/browser-service-worker-blocks/guest/target/wasm32-wasip1/release/browser_compiled_wafer_block.wasm` and `skip`s with a clear message when absent. Its BlockInfo names itself `browser/hello`; for the tests below, `FakeControl::validate` returns a `ValidatedGuest { info }` whose `info` the test supplies (`control.set_validated_info(info)`), so the static rules are exercised without wasmi.

```rust
fn hello_info(name: &str) -> BlockInfo {
    BlockInfo::new(name, "0.1.0", "handler@v1", "hello")
        .endpoints(vec![BlockEndpoint::get("/b/hello/").auth(AuthLevel::Public).summary("hello")])
}

#[tokio::test]
async fn staging_a_valid_block_activates_a_generation_and_rebuilds_the_runtime() {
    let control = FakeControl::new();
    control.set_validated_info(hello_info("site/hello"));
    let ctx = TestContext::with_dev(control.clone()).await;
    let r = output_json(dev_post(&ctx, "/b/dev/api/builds/stage", json!({
        "block_name": "hello", "artifact_base64": b64(b"\0asm\x01\0\0\0"), "compiler_version": "test", "diagnostics": []
    })).await).await;
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(r["generation"]["cause"], "block_compile");
    let rebuilds = control.rebuilds();
    assert_eq!(rebuilds.len(), 1);
    assert_eq!(rebuilds[0][0].name, "site/hello");
    assert_eq!(rebuilds[0][0].routes[0].prefix, "/b/hello/");
    let status = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/status")).await).await;
    assert_eq!(status["blocks"][0]["name"], "site/hello");
}

#[tokio::test]
async fn a_block_naming_itself_outside_its_namespace_is_refused_with_a_diagnostic() {
    let control = FakeControl::new();
    control.set_validated_info(hello_info("impresspress/admin"));
    let ctx = TestContext::with_dev(control.clone()).await;
    let r = output_json(dev_post(&ctx, "/b/dev/api/builds/stage", json!({
        "block_name": "hello", "artifact_base64": b64(b"\0asm"), "compiler_version": "test", "diagnostics": []
    })).await).await;
    assert_eq!(r["success"], false);
    let codes: Vec<&str> = r["diagnostics"].as_array().unwrap().iter().map(|d| d["code"].as_str().unwrap()).collect();
    assert!(codes.contains(&"name-mismatch"), "{codes:?}");
    assert!(control.rebuilds().is_empty());
}

#[tokio::test]
async fn a_route_that_shadows_a_builtin_is_refused() {
    let control = FakeControl::new();
    control.set_validated_info(BlockInfo::new("site/hello", "0.1.0", "handler@v1", "x")
        .endpoints(vec![BlockEndpoint::get("/b/auth/login").auth(AuthLevel::Public).summary("x")]));
    let ctx = TestContext::with_dev(control.clone()).await;
    let r = output_json(dev_post(&ctx, "/b/dev/api/builds/stage", json!({
        "block_name": "hello", "artifact_base64": b64(b"\0asm"), "compiler_version": "test", "diagnostics": []
    })).await).await;
    assert_eq!(r["success"], false);
    assert!(r["diagnostics"].to_string().contains("endpoint-outside-routes"));
}

#[tokio::test]
async fn declared_capabilities_outside_the_namespace_are_refused() {
    let control = FakeControl::new();
    let mut info = hello_info("site/hello");
    info.capabilities = Some(wafer_block::BlockCapabilities {
        collections: wafer_block::Allowlist::Only(vec!["impresspress__products__products".into()]),
        ..wafer_block::BlockCapabilities::none()
    });
    control.set_validated_info(info);
    let ctx = TestContext::with_dev(control.clone()).await;
    let r = output_json(dev_post(&ctx, "/b/dev/api/builds/stage", json!({
        "block_name": "hello", "artifact_base64": b64(b"\0asm"), "compiler_version": "test", "diagnostics": []
    })).await).await;
    assert_eq!(r["success"], false);
    assert!(r["diagnostics"].to_string().contains("cap-collection"));
}

#[tokio::test]
async fn an_executable_validation_failure_is_a_diagnostic_not_a_transport_error() {
    let control = FakeControl::new();
    control.fail_next_validate(ValidationStage::Init, "trap: unreachable");
    let ctx = TestContext::with_dev(control.clone()).await;
    let out = dev_post(&ctx, "/b/dev/api/builds/stage", json!({
        "block_name": "hello", "artifact_base64": b64(b"\0asm"), "compiler_version": "test", "diagnostics": []
    })).await;
    assert_eq!(output_status(out).await, 200);
    let r = output_json(dev_post(&ctx, "/b/dev/api/builds/stage", json!({
        "block_name": "hello", "artifact_base64": b64(b"\0asm"), "compiler_version": "test", "diagnostics": []
    })).await).await;
    assert_eq!(r["success"], false);
    assert_eq!(r["diagnostics"][0]["code"], "guest-init");
}

#[tokio::test]
async fn removing_a_block_rebuilds_without_it_and_keeps_its_source() {
    let control = FakeControl::new();
    control.set_validated_info(hello_info("site/hello"));
    let ctx = TestContext::with_dev(control.clone()).await;
    dev_post(&ctx, "/b/dev/api/files/write", json!({"path": "blocks/hello/src/lib.rs", "content": "//", "expected_sha256": null})).await;
    dev_post(&ctx, "/b/dev/api/builds/stage", json!({"block_name": "hello", "artifact_base64": b64(b"\0asm"), "compiler_version": "test", "diagnostics": []})).await;
    let r = output_json(dev_post(&ctx, "/b/dev/api/blocks/hello/remove", json!({})).await).await;
    assert_eq!(r["generation"]["cause"], "block_remove");
    assert!(control.rebuilds().last().unwrap().is_empty());
    let l = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/files?prefix=blocks/hello/")).await).await;
    assert_eq!(l["files"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn oversized_artifacts_are_refused_before_validation() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;
    let r = output_json(dev_post(&ctx, "/b/dev/api/builds/stage", json!({
        "block_name": "hello", "artifact_base64": b64(&vec![0u8; 4 * 1024 * 1024 + 1]), "compiler_version": "test", "diagnostics": []
    })).await).await;
    assert_eq!(r["success"], false);
    assert_eq!(r["diagnostics"][0]["code"], "artifact-too-large");
    assert_eq!(control.validations(), 0);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p impresspress-core --features block-dev --test dev_blocks`
Expected: 404s.

- [ ] **Step 3: Implement `validation.rs`**

Straightforward rule checks producing `Vec<Diagnostic>`; on success build the `DynamicBlockSpec` with `routes = [DynamicRoute { prefix: format!("/b/{name}/"), access: Public }]` (the router still applies each endpoint's declared `auth` on top, via `declared_access`), `capabilities = info.capabilities.clone().unwrap_or_else(BlockCapabilities::none)`, `wafer_guest_version = 0` (Plan 3 fills it). Built-in prefixes come from `crate::routing::ROUTES` plus `ROUTE_PREFIX`; the caller passes the target generation's other blocks. Tool names: collect `ep.agent_tool.name` across `ctx.registered_blocks()` and the other dynamic blocks; a duplicate is `tool-name-duplicate`.

- [ ] **Step 4: Implement `blocks_api.rs`**

Stage handler: parse; decode base64 (400); size (diagnostic `artifact-too-large`, `success:false`); `sha256`; store in folder `artifacts`, key `<sha>.wasm`; insert `builds` row `Staged`; `shared.control.validate(&provisional_spec, &bytes).await` → on `ValidationFailure { stage, message }` diagnostic `guest-<stage>` (`guest-load`, `guest-info`, `guest-init`, `guest-start`, `guest-probe`), build `Invalid`, return `success:false`; `validate_static(...)` → diagnostics or spec; build `Valid`; compose the next manifest = active site manifest + active blocks with this block replaced/added; `activation::request(BlockCompile, next)`; respond. Remove handler: next manifest without the block, `request(BlockRemove, next)`. Add the two endpoints to `info()`/`ROUTES`; regenerate and read the snapshot.

- [ ] **Step 5: Run**

Run: `cargo test -p impresspress-core --features block-dev`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/impresspress-core
git commit -m "feat(dev): stage, validate and activate precompiled guest blocks; remove blocks"
```

---

### Task 9: The browser `RuntimeControl` — wasmi validation, runtime rebuild, boot convergence, seed-on-boot

**Files:**
- Create: `crates/impresspress-web/src/dev_runtime.rs`
- Modify: `crates/impresspress-web/src/lib.rs` (`mod dev_runtime;` under `browser-devtools`; `initialize` calls `dev_runtime::install`)
- Modify: `crates/impresspress-web/src/runtime_factory.rs` (`load_guest` lives in `dev_runtime`)
- Create: `crates/impresspress-core/src/blocks/dev/seed.rs` (manifest type + `import_files`)
- Modify: `crates/impresspress-bundle/src/bundle/mod.rs` (`/seed/` on the default bypass list when `dev_enabled`)

**Interfaces:**
- Produces (`impresspress-web`, `browser-devtools` only):

```rust
pub struct BrowserRuntimeControl { factory: Rc<RuntimeFactory>, generation: Cell<u64> }
unsafe impl Send/Sync (single-threaded wasm32, same pattern as BrowserStorageService)
impl RuntimeControl for BrowserRuntimeControl { validate, rebuild, runtime_generation }
pub fn load_guest(spec: &DynamicBlockSpec, artifact: &[u8]) -> Result<Arc<dyn Block>, JsValue>;
pub async fn install(factory: RuntimeFactory) -> Result<(), JsValue>;   // called once from initialize()
```
  `load_guest` = `WasmiBlock::load_with_capabilities_and_limits(artifact, spec.capabilities.clone(), ResourceLimits { fuel: Metered(100_000_000), memory_pages: 256, ..Default::default() })`. `rebuild` reads each artifact from storage (`artifacts/<sha>.wasm`), calls `factory.build(blocks)`, `replace_wafer`, bumps `generation`; on `build` error nothing is swapped. `validate`: `load_guest` → `block.info()` (must parse as BlockInfo; compare to the provisional name) → `lifecycle(Init)`/`lifecycle(Start)` with a deny-all `Context` → one `handle` of `GET <first route prefix>` under a 100 M fuel budget; any trap → `ValidationFailure`. `install`: `seed::import_if_fresh` then `activation::converge_on_boot` → if the active generation has blocks, `rebuild(&blocks)` before returning (requests arrive only after `initialize()` resolves, so nothing serves from the base runtime while blocks are pending).
- Produces (`impresspress-core::blocks::dev::seed`):

```rust
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SeedManifest { pub schema_version: u32, pub source_generation: Option<String>,
                          pub site: Vec<SeedFile>, pub blocks: Vec<SeedBlock> }
pub struct SeedFile { pub path: String, pub sha256: String, pub size: u64, pub content_type: String }
pub struct SeedBlock { pub spec: DynamicBlockSpec, pub sources: Vec<SeedFile> }
pub async fn import(ctx, manifest: &SeedManifest, fetch: &dyn Fn(&str) -> BoxFuture<Result<Vec<u8>, String>>) -> Result<GenerationManifest, String>;
```
  `import` verifies every fetched file's sha256, writes blobs/artifacts, writes the workspace, and returns the generation-0 manifest for `activation::request(Seed, ..)`. It runs only when `runtime_state.active_generation_id` is `None` and `generations` is empty.

- [ ] **Step 1: Write the failing tests**

Core (`tests/dev_seed.rs`): `import` with an in-memory fetch closure serving a two-file site + one block → workspace has `site/index.html`, `site/assets/app.js`, `blocks/hello/src/lib.rs`; the returned manifest has one block; a fetch whose bytes do not match `sha256` fails with a message naming the path; a second `import` on a non-fresh instance is a no-op (`Ok(None)` — make the return `Result<Option<GenerationManifest>, String>`).

Browser (`crates/impresspress-web/tests/e2e/dev-foundations.spec.ts`, Task 10 runs it) — this task only needs the crate to compile: `cargo check -p impresspress-web --target wasm32-unknown-unknown --features browser-devtools`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p impresspress-core --features block-dev --test dev_seed`
Expected: `seed` module missing.

- [ ] **Step 3: Implement `seed.rs`** as specified; the fetch closure abstraction keeps it host-testable. In `dev_runtime::install`, the fetch is `web_sys::window`-less `js_sys::global()` `fetch` (the SW's `self.fetch`) of `/seed/manifest.json`; a 404 means "no seed" and is not an error; any other failure logs and continues without a seed (the instance simply has no generation 0 — the welcome site is Plan 2's concern).

- [ ] **Step 4: Implement `dev_runtime.rs`**

`BrowserRuntimeControl::validate` builds a `DenyAllContext` (every `check_resource_access` denies; `call_block` returns `PermissionDenied`) — copy the shape of the `MockContext` in wafer-run's `tests/abi_compat.rs:20-48`. `rebuild` must not hold a `RefCell` borrow across awaits: read artifacts first (`storage::get` through the block's own context is unavailable here — use `impresspress_browser::make_storage_service()` directly with the namespaced folder `impresspress/dev/artifacts`, exactly as boot code uses services directly), then `factory.build(blocks).await`, then `replace_wafer`.

- [ ] **Step 5: Bundle bypass**

In `build_template_vars`, when `app.dev_enabled`, append `/seed/` to the extra-bypass prefixes (so the SW lets `fetch('/seed/…')` reach the static host). The bundle integration test asserts `url.pathname.startsWith('/seed/')` appears in `sw.js` only when the flag is on.

- [ ] **Step 6: Run**

Run: `cargo test -p impresspress-core --features block-dev --test dev_seed && cargo test -p impresspress-bundle && cargo check -p impresspress-web --target wasm32-unknown-unknown --features browser-devtools`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/impresspress-web crates/impresspress-core crates/impresspress-bundle
git commit -m "feat(web): wasmi-backed RuntimeControl, boot convergence and seed import"
```

---

### Task 10: Checkpoint — a browser e2e that stages a real guest and survives a restart

**Files:**
- Create: `crates/impresspress-web/tests/e2e/dev-foundations.spec.ts`
- Create: `crates/impresspress-web/tests/e2e/fixtures/dev-sandbox-seed/seed/manifest.json`, `seed/site/index.html` (a one-line welcome placeholder; Plan 2 replaces it)
- Modify: `.github/workflows/ci.yml` (new job `e2e-dev-sandbox` modelled on `e2e-smoke` :445-506), `ci-main.yml` mirror
- Modify: `crates/impresspress-web/package.json` (`"e2e:dev": …`)

**Interfaces:**
- Consumes: a dev bundle built through the **sealed** web flow with the wasm/JS overrides `sealed_web.rs` already honours (`helpers/wasm.rs:10-26`): build the feature-on pkg once, then bundle it with a throwaway `impresspress.toml` that turns the flag on:

```bash
(cd crates/impresspress-web && wasm-pack build --target web --release --out-dir pkg-dev -- --features browser-devtools)
mkdir -p "$SCRATCH/dev-bundle" && cd "$SCRATCH/dev-bundle"
cat > impresspress.toml <<'TOML'
[app]
name = "dev-sandbox-e2e"
title = "Dev sandbox e2e"
boot_redirect = "/"
[dev]
enabled = true
TOML
IMPRESSPRESS_WEB_WASM="$REPO/crates/impresspress-web/pkg-dev/impresspress_web_bg.wasm" \
IMPRESSPRESS_WEB_JS="$REPO/crates/impresspress-web/pkg-dev/impresspress_web.js" \
impresspress build --target web --release          # sealed mode: no Cargo.toml here → dist/
cp -r "$REPO/crates/impresspress-web/tests/e2e/fixtures/dev-sandbox-seed/seed" dist/
python3 -m http.server 8082 -d dist
```
  (Plan 2 turns this into `examples/dev-sandbox/`.)
- Consumes: the proof guest wasm, built in CI with `cargo build --release --target wasm32-wasip1 --manifest-path experiments/browser-service-worker-blocks/guest/Cargo.toml`.

- [ ] **Step 1: Write the spec**

```ts
import { test, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';

const GUEST = readFileSync(process.env.PROOF_GUEST_WASM!);   // set by CI; the guest names itself browser/hello

async function loginAdmin(page) {
  await page.goto('/b/auth/login?redirect=/b/dev/api/status');
  await page.locator('input#email').fill('admin@example.com');
  await page.locator('input#password').fill('admin123');
  await page.getByRole('button', { name: /sign in/i }).click();
  await page.waitForURL(/\/b\/dev\/api\/status/);
}

test('feature on: status answers, site write serves, block stages, restart persists', async ({ page }) => {
  await page.goto('/');
  await page.waitForFunction(() => navigator.serviceWorker.controller !== null);
  await loginAdmin(page);

  const status = await page.evaluate(async () => (await fetch('/b/dev/api/status')).json());
  expect(status.runtime_generation).toBe(0);

  const w = await page.evaluate(async () => (await fetch('/b/dev/api/files/write', {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ path: 'site/index.html', content: '<h1>sandbox v1</h1>', expected_sha256: null }),
  })).json());
  expect(w.generation.status).toBe('active');
  await page.goto('/');
  await expect(page.locator('h1')).toHaveText('sandbox v1');
  const cc = await page.evaluate(async () => (await fetch('/')).headers.get('cache-control'));
  expect(cc).toBe('no-cache');

  // The proof guest declares `browser/hello`; the sandbox requires `site/<name>`.
  // Plan 3's template fixes the namespace; here we assert the refusal is a diagnostic.
  const staged = await page.evaluate(async (bytes) => (await fetch('/b/dev/api/builds/stage', {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ block_name: 'hello', artifact_base64: btoa(String.fromCharCode(...bytes)), compiler_version: 'proof', diagnostics: [] }),
  })).json(), Array.from(GUEST));
  expect(staged.success).toBe(false);
  expect(staged.diagnostics.map((d) => d.code)).toContain('name-mismatch');

  // A guest whose info says site/hello: rewrite the INFO bytes in the proof
  // wasm (the name is a plain string in the data section) and stage again.
  const patched = Buffer.from(GUEST).toString('latin1').replace('"name":"browser/hello"', '"name":"site/hello"   ');
  const ok = await page.evaluate(async (b64) => (await fetch('/b/dev/api/builds/stage', {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ block_name: 'hello', artifact_base64: b64, compiler_version: 'proof', diagnostics: [] }),
  })).json(), Buffer.from(patched, 'latin1').toString('base64'));
  expect(ok.success, JSON.stringify(ok.diagnostics)).toBe(true);
  expect(ok.generation.cause).toBe('block_compile');

  const hello = await page.evaluate(async () => (await fetch('/b/hello/')).text());
  expect(hello).toContain('Hello from a browser-compiled WAFER block!');
  const after = await page.evaluate(async () => (await fetch('/b/dev/api/status')).json());
  expect(after.runtime_generation).toBe(1);
  expect(after.blocks[0].name).toBe('site/hello');

  // Restart the service worker: unregister, reload, everything comes back.
  await page.evaluate(async () => { for (const r of await navigator.serviceWorker.getRegistrations()) await r.unregister(); });
  await page.goto('/');
  await page.waitForFunction(() => navigator.serviceWorker.controller !== null);
  await expect(page.locator('h1')).toHaveText('sandbox v1');
  const helloAgain = await page.evaluate(async () => (await fetch('/b/hello/')).text());
  expect(helloAgain).toContain('Hello from a browser-compiled WAFER block!');

  // Rollback to the first generation removes the block and keeps the site.
  const gens = await page.evaluate(async () => (await fetch('/b/dev/api/generations')).json());
  const first = gens.generations.at(-1);
  const rb = await page.evaluate(async (id) => (await fetch(`/b/dev/api/generations/${id}/rollback`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}' })).json(), first.id);
  expect(rb.generation.cause).toBe('rollback');
  const gone = await page.evaluate(async () => (await fetch('/b/hello/')).status);
  expect(gone).toBe(404);
});

test('a non-admin sees no sandbox', async ({ page }) => {
  await page.goto('/');
  await page.waitForFunction(() => navigator.serviceWorker.controller !== null);
  const s = await page.evaluate(async () => (await fetch('/b/dev/api/status')).status);
  expect(s).toBe(403);
});
```

The INFO patch keeps the byte length identical (three trailing spaces) so the packed `(ptr, len)` in the proof guest stays valid; JSON tolerates the spaces. Playwright's `serviceWorkers: 'allow'` is already in the base config; the `/b/auth/login` flow sets the `auth_token` cookie client-side on wasm32 (see `pages/mod.rs:~186-190`), which `fetch` sends same-origin.

- [ ] **Step 2: CI job**

Copy `e2e-smoke` (:445-506) as `e2e-dev-sandbox`: install `wasm32-wasip1`, build the proof guest, build the CLI (`cargo install --path crates/impresspress --locked --debug --root out`, as `e2e-build` does), run the bundle recipe above, serve on 8082 with `python3 -m http.server`, run `npx playwright test --config=tests/playwright.config.ts tests/e2e/dev-foundations.spec.ts` with `TEST_PORT=8082 PROOF_GUEST_WASM=…`. `rust-cache` `prefix-key: e2e-dev`.

- [ ] **Step 3: Run locally**

Run the same steps locally; expected: both tests PASS. Record the cold-start time with one block and the rebuild time in the PR description (the spec's §16 size/time line).

- [ ] **Step 4: Commit and open the PR**

```bash
git add crates/impresspress-web/tests crates/impresspress-web/package.json .github/workflows
git commit -m "test(e2e): dev sandbox foundations — stage a guest, restart, roll back"
git push -u origin feat/dev-sandbox
gh pr create --title "Dev sandbox foundations: hierarchical OPFS keys, replaceable runtime, impresspress/dev control plane" --body-file - <<'EOF'
Plan 1 of the dev.impresspress.org sandbox (spec: docs/superpowers/specs/2026-09-02-dev-sandbox-design.md).

- OPFS storage keys may be hierarchical (`assets/app.js`), validated per segment.
- The browser runtime is an `Rc<Wafer>`; `replace_wafer` swaps under in-flight requests.
- `browser-devtools` feature + `[dev] enabled` bundle flag; off by default and proven absent.
- `impresspress/dev` (core, `block-dev`): content-addressed workspace, hash-checked writes, generations, coalescing auto-activation, site publisher, boot convergence, precompiled-block staging with static + executable validation, rollback, seed import.
- e2e: a real guest staged over HTTP answers on its route, survives an SW restart, and rolls back.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
```

## Self-review notes

- Spec coverage: §7.1–7.4 (Tasks 6–8), §7.3 recovery (Task 7 `converge_on_boot`, Task 9 `install`), §11 (Tasks 5–6), §12 endpoints except `tools.json`/`reference`/`blocks` create/`export` (Plans 2–4), §13 feature-off (Task 4), §15 bypass `/seed/` (Task 9), §20.2 (`DDL` is not used here — the dev block writes no guest tables), §20.3 `frame_ancestors`/CSP (Task 4), §20.6 CSP (Task 4). `wafer_guest_version` is carried but always 0 until Plan 3.
- Names other plans rely on: `impresspress_core::blocks::dev::{BLOCK_NAME, ROUTE_PREFIX, DevBlock, DevShared, RuntimeControl, DynamicBlockSpec, DynamicRoute, ValidatedGuest, ValidationFailure, ValidationStage, contracts::*, generation::{GenerationManifest, SiteManifest}, activation::{request, ActivationOutcome, ProgressStep}, workspace::{Workspace, FileEntry, load, save}, blobs::{put, get}, paths::{validate_path, WorkspaceArea, MAX_*}, validation::{Diagnostic, Severity, validate_static}, seed::{SeedManifest, import}, repo::{generations, builds, runtime_state}::TABLE, test_support::FakeControl}`; `impresspress_web::{RuntimeFactory, RuntimeOptions, dev_runtime::{BrowserRuntimeControl, install, load_guest}}`; `impresspress_browser::{replace_wafer, restore_wafer, current_wafer}`; bundle `AppConfig.dev_enabled`, `__DEV_ENABLED__`; CLI `[dev] enabled`.
