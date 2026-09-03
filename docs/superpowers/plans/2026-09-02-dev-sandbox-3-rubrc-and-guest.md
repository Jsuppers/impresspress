# Dev Sandbox Plan 3 — Rubrc in the browser and the `wafer_guest.rs` contract

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An agent scaffolds a Rust backend block from a template, edits it, compiles it in the browser with Rubrc, gets structured diagnostics when it is wrong, and — when it is right — sees it validated, activated, and discoverable as a WebMCP tool, all without leaving `/b/dev`.

**Architecture:** Rubrc's compiler runs in a dedicated module Worker owned by the `/b/dev` document, packaged as versioned static assets under `/__impresspress_dev/compiler/<version>/` (brotli-split parts, as Rubrc's own publish pipeline does) and loaded lazily behind a narrow `BrowserRustCompiler` adapter. Guests are std-only crates carrying a vendored `wafer_guest.rs` that owns the ABI, a small JSON codec, request/response types, a schema builder and host-call helpers over the JSON host codec Plan 0 added. The compile result is staged through Plan 1's existing `builds/stage` endpoint; nothing about activation changes.

**Tech Stack:** Rubrc (pinned SHA, MIT OR Apache-2.0), Vite/Bun for the compiler package, vanilla JS adapter, std-only Rust guest module, wasmi via wafer-run, Playwright.

**Spec:** `docs/superpowers/specs/2026-09-02-dev-sandbox-design.md` — §6, §8, §9.2 (`dev_create_block`, `dev_compile_block`, `dev_read_reference`), §16 scenario step 2, §19, §20 (amendments 1, 2, 4, 6).

**Depends on:** Plans 0–2 merged. Plan 0's `__wafer_host_codec`, schema ops and `json_host_guest` fixture; Plan 1's staging/validation; Plan 2's `/b/dev` page and `dev.js`.

## Global Constraints

- **Rubrc stays behind the adapter.** Nothing in impresspress imports Rubrc's UI, Monaco, or app code; the adapter speaks one message protocol to one worker script we build.
- **Pinned and recorded.** The Rubrc SHA, the license, and the SHA-256 of every packaged asset live in `examples/dev-sandbox/compiler/PIN.json`; CI fails if the packaged tree drifts from it.
- **No asset larger than 24 MiB** in `dist/` (`verify-compiler-assets.mjs` enforces it, same rule as Rubrc's `verify-vfs-asset.mjs`).
- **`wafer_guest.rs` is vendored and versioned.** `dev_create_block` writes it; nothing rewrites it silently. Its public API is documented in `reference.md`, which `dev_read_reference` returns verbatim.
- **Std only.** The template crates have an empty `[dependencies]`; a golden test compiles both templates with plain `cargo` for `wasm32-wasip1` and runs them through wafer-run's wasmi against a real SQLite service.
- **Compiler errors are results.** `dev_compile_block` returns `success:false` with diagnostics; the only `isError` results are for "no compiler in this build" and adapter crashes.
- One compile at a time; 120 s timeout; the worker is terminated and recreated after any unrecoverable failure.

---

### Task 1: Pin, extract and package the Rubrc compiler (verification task)

**Files:**
- Create: `examples/dev-sandbox/compiler/README.md`, `compiler/PIN.json`, `compiler/build-compiler.sh`, `compiler/package.json`, `compiler/vite.config.ts`, `compiler/src/worker-entry.ts`, `compiler/src/probe.html`, `compiler/scripts/verify-compiler-assets.mjs`
- Modify: `examples/dev-sandbox/impresspress.toml` (overlay `compiler/dist` → `__impresspress_dev/compiler`)

**Interfaces:**
- Produces: `examples/dev-sandbox/compiler/dist/<version>/` containing `worker.js` (module worker entry: our `worker-entry.ts` bundled with Rubrc's `page/src/worker_process/{worker.ts, thread_spawn.ts, util_cmd.ts}` and `vfs_bindings/`), the subordinate worker chunks, `vfs.core-<hash>.wasm.br.part-NNN` + `.br.json`, and `dist/manifest.json`:

```json
{ "schema_version": 1, "version": "<rubrc sha8>", "entry": "/__impresspress_dev/compiler/<sha8>/worker.js",
  "total_bytes": 0, "assets": [{ "path": "...", "bytes": 0, "sha256": "..." }], "license": "MIT OR Apache-2.0",
  "rubrc": { "repo": "https://github.com/oligamiq/rubrc", "sha": "<full sha>" }, "target": "wasm32-wasip1" }
```
- Produces: the **message protocol** the adapter (Task 3) speaks, documented in `README.md` and implemented by `worker-entry.ts`:

```
page → worker : { type: 'init', id }                                   → worker: { type: 'progress', id, stage: 'download'|'initializing', loaded, total } … { type: 'ready', id, rustcVersion }
page → worker : { type: 'compile', id, crateName, files: { 'Cargo.toml': '...', 'src/lib.rs': '...', ... }, target: 'wasm32-wasip1', release: true }
worker → page : { type: 'progress', id, stage: 'compiling', detail }
worker → page : { type: 'result', id, success, artifact?: ArrayBuffer (transferred), stdout, stderr, diagnostics: [{file,line,column,severity,message,code?}], elapsedMs }
page → worker : { type: 'cancel', id }                                  → worker: { type: 'result', id, success: false, cancelled: true, ... }
```

This task is where §19's Rubrc assumptions are verified. Its output is the packaged tree plus a `README.md` section recording what was confirmed.

- [ ] **Step 1: Pin**

`build-compiler.sh` clones `https://github.com/oligamiq/rubrc` at the SHA in `PIN.json` (start with the current `main` head; record it), runs `bun install --frozen-lockfile` and `bun run vfs:build:prod` (the composed `vfs.core-*.wasm`), then builds **our** Vite project in `compiler/` whose only inputs are `src/worker-entry.ts` and, via aliases, Rubrc's `page/src/worker_process/*` and `vfs_bindings/*`. Run `node scripts/prepare-vfs-asset.mjs` from the Rubrc checkout (or a copy of it under `compiler/scripts/`) against our `dist/<version>/` to brotli-split the composed wasm into 24 MiB parts. Write `manifest.json` with sizes and hashes.

- [ ] **Step 2: The probe page**

`src/probe.html` — a standalone page (served with COOP/COEP by `python3 -m http.server` plus a `_headers`-equivalent: use a tiny Node static server in `compiler/scripts/serve-probe.mjs` that sets the two headers) that creates `new Worker('./<version>/worker.js', { type: 'module' })`, sends `init`, then `compile` with the `hello` template's two files (Task 4's `Cargo.toml` and a `src/lib.rs` that includes `wafer_guest.rs` inline — for the probe, paste Plan 0's `json_host_guest/src/lib.rs`, which is already std-only), and prints: `ready` time, compile time, artifact bytes, and whether the artifact instantiates under `WebAssembly.instantiate` with stub imports. Verify with a **deliberate error** too (a missing semicolon): the `result` must carry a diagnostic with `file: 'src/lib.rs'`, a `line`, and `severity: 'error'`.

Confirmed-or-not list to write into `README.md` after running the probe:
1. The worker starts from a same-origin module URL and its subordinate workers resolve their URLs after bundling.
2. `crossOriginIsolated === true` in the probe page and the worker.
3. `--error-format=json` (or Rubrc's equivalent) yields machine-readable diagnostics; if only text is available, the `diagnostics` array is derived by the regex `^(error|warning)(\[(E\d+)\])?: (.*)\n\s+--> ([^:]+):(\d+):(\d+)` in `worker-entry.ts`.
4. Release build of the std-only guest is < 200 KB and instantiates.
5. The largest file in `dist/` and the total download (record both).
6. Cold `init` time on a warm cache and cold compile time (record both).

- [ ] **Step 3: `verify-compiler-assets.mjs`**

Fails when: any file > 25 165 824 bytes; `manifest.json` hashes disagree with the files; the raw composed `.wasm` is present next to its parts; `PIN.json`'s `sha` differs from `manifest.json`'s.

- [ ] **Step 4: Overlay and bypass**

`impresspress.toml`: add `{ from = "compiler/dist", to = "__impresspress_dev/compiler" }` to `overlay`; `/__impresspress_dev/compiler/` is already on `extra_bypass_prefix` (Plan 2). `build.sh` runs `compiler/build-compiler.sh` only when `compiler/dist/manifest.json` is missing or `PIN.json` changed (it is slow); `--check` runs `verify-compiler-assets.mjs`.

- [ ] **Step 5: Commit**

```bash
git add examples/dev-sandbox
git commit -m "feat(dev-sandbox): pin and package the Rubrc compiler as versioned static assets"
```

`compiler/dist/` is gitignored; CI caches it keyed on `PIN.json` (Task 8).

---

### Task 2: `/b/dev` is cross-origin isolated, and the compiler manifest is discoverable

**Files:**
- Modify: `crates/impresspress-web/tests/e2e/dev-workspace.spec.ts`
- Modify: `crates/impresspress-core/src/blocks/dev/assets/dev.js` (fetch `/__impresspress_dev/compiler/manifest.json` on load; enable the Compile button when present)

- [ ] **Step 1: Write the failing test**

```ts
test('/b/dev is cross-origin isolated and the compiler is discoverable; / is not isolated', async ({ page }) => {
  await loginToWorkspace(page);            // helper from Task 6 of Plan 2, lifted into fixtures/dev-helpers.ts
  expect(await page.evaluate(() => crossOriginIsolated)).toBe(true);
  expect(await page.evaluate(() => typeof SharedArrayBuffer)).toBe('function');
  await expect(page.locator('#dev-compile')).toBeEnabled();
  const manifest = await page.evaluate(async () => (await fetch('/__impresspress_dev/compiler/manifest.json')).json());
  expect(manifest.schema_version).toBe(1);
  await page.goto('/');
  expect(await page.evaluate(() => crossOriginIsolated)).toBe(false);
});
```

- [ ] **Step 2: Run to verify it fails** — the Compile button is disabled (Plan 2 stub).

- [ ] **Step 3: Implement** — `dev.js` fetches the manifest with `{ cache: 'no-store' }`; on 200 stores `compilerManifest`, enables `#dev-compile`, and shows "Compiler v<version> · <total MB>" in `#dev-actions`; on 404 leaves the button disabled with title "No compiler in this build".

- [ ] **Step 4: Run** the spec against `examples/dev-sandbox/dist` (built with the compiler present). Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/impresspress-core/src/blocks/dev/assets/dev.js crates/impresspress-web/tests
git commit -m "feat(dev): discover the packaged compiler; prove cross-origin isolation on /b/dev"
```

---

### Task 3: The `BrowserRustCompiler` adapter

**Files:**
- Create: `crates/impresspress-core/src/blocks/dev/assets/compiler-adapter.js`
- Modify: `crates/impresspress-core/src/blocks/dev/assets.rs` (serve `/b/dev/static/compiler-adapter.js`), `page.rs` (script tag), `dev.js` (use it)
- Create: `crates/impresspress-web/tests/e2e/fixtures/fake-compiler-worker.js` (a stub worker speaking the protocol)
- Modify: `crates/impresspress-web/tests/e2e/dev-workspace.spec.ts`

**Interfaces:**
- Produces (`window.ImpresspressCompiler`):

```javascript
class BrowserRustCompiler {
  constructor(manifest)                                  // from /__impresspress_dev/compiler/manifest.json
  async initialize(onProgress)                           // idempotent; resolves when 'ready'; rejects → worker recreated on next call
  async compile({ crateName, files, onProgress })        // one at a time (queued); resolves CompileResult; never rejects on compiler errors
  async cancel()                                         // resolves the in-flight compile with { success:false, cancelled:true }
  async dispose()                                        // terminates the worker
  get version()
}
// CompileResult = { buildId, success, cancelled?, artifact: Uint8Array|null, artifactSha256: string|null, stdout, stderr,
//                   diagnostics: [{file,line,column,severity,message,code}], elapsedMs, compilerVersion }
```
  `artifactSha256` is computed with `crypto.subtle.digest('SHA-256', …)` before returning; a `result` that arrives with an unexpected `id`, a non-ArrayBuffer artifact, or an artifact > 4 MiB is treated as an adapter failure (worker terminated, promise rejected). A compile exceeding 120 s is cancelled and the worker recreated.

- [ ] **Step 1: Write the failing test** — `dev-workspace.spec.ts`, using the stub worker (served from the test's static dir at `/__impresspress_dev/compiler/test/worker.js` by pointing `manifest.json`'s `entry` there in a test-only manifest the spec writes into `dist/` before serving):

```ts
test('the compiler adapter reports progress, results and diagnostics', async ({ page }) => {
  await loginToWorkspace(page);
  const r = await page.evaluate(async () => {
    const manifest = await (await fetch('/__impresspress_dev/compiler/manifest.json')).json();
    const c = new window.ImpresspressCompiler(manifest);
    const stages = [];
    await c.initialize((p) => stages.push(p.stage));
    const ok = await c.compile({ crateName: 'hello', files: { 'src/lib.rs': '// ok' }, onProgress: (p) => stages.push(p.stage) });
    const bad = await c.compile({ crateName: 'hello', files: { 'src/lib.rs': '// FAIL' } });
    await c.dispose();
    return { stages, ok: { success: ok.success, bytes: ok.artifact.length, sha: ok.artifactSha256 }, bad: { success: bad.success, diagnostics: bad.diagnostics } };
  });
  expect(r.stages).toEqual(['download', 'initializing', 'compiling']);
  expect(r.ok.success).toBe(true);
  expect(r.ok.sha).toMatch(/^[0-9a-f]{64}$/);
  expect(r.bad.success).toBe(false);
  expect(r.bad.diagnostics[0]).toMatchObject({ file: 'src/lib.rs', line: 1, severity: 'error' });
});
```

`fake-compiler-worker.js`: answers `init` with two `progress` messages then `ready`; answers `compile` with `progress: compiling` then `result` — `success:true` with a 16-byte ArrayBuffer when no file contains `FAIL`, else `success:false` with one diagnostic.

- [ ] **Step 2: Run to verify it fails** — `ImpresspressCompiler` undefined.

- [ ] **Step 3: Implement `compiler-adapter.js`** — a class over `new Worker(manifest.entry, { type: 'module' })`; a `pending` map `id → {resolve, reject, onProgress, timer}`; `postMessage` with transfer for results; `queue` (promise chain) so `compile` calls serialize; `terminate()` + `this.worker = null` on any protocol violation so the next `initialize` recreates it. Expose as `window.ImpresspressCompiler`.

- [ ] **Step 4: Run** the spec. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/impresspress-core/src/blocks/dev crates/impresspress-web/tests
git commit -m "feat(dev): BrowserRustCompiler adapter over the packaged Rubrc worker"
```

---

### Task 4: `wafer_guest.rs`, the two templates, `dev_create_block`, `dev_read_reference`

**Files:**
- Create: `crates/impresspress-core/src/blocks/dev/templates/wafer_guest.rs` (canonical)
- Create: `crates/impresspress-core/src/blocks/dev/templates/hello/{Cargo.toml, src/lib.rs, src/wafer_guest.rs → ../../wafer_guest.rs (symlink)}`
- Create: `crates/impresspress-core/src/blocks/dev/templates/table/{Cargo.toml, src/lib.rs, src/wafer_guest.rs (symlink)}`
- Create: `crates/impresspress-core/src/blocks/dev/templates/reference.md`
- Create: `crates/impresspress-core/src/blocks/dev/scaffold.rs` (handlers + `Template` enum)
- Modify: `crates/impresspress-core/src/blocks/dev/{mod.rs, contracts.rs, tools.rs}`; `crates/impresspress-core/Cargo.toml` (dev-deps: `wafer-run` with `wasmi`, `wafer-block-sqlite`)
- Create: `crates/impresspress-core/tests/dev_scaffold.rs`, `crates/impresspress-core/tests/wafer_guest_parity.rs`, `crates/impresspress-core/tests/wafer_guest_golden.rs`

**Interfaces:**
- Produces: `POST /b/dev/api/blocks` `CreateBlockRequest { name, template: "hello"|"table" }` → `CreateBlockResponse { name, files: Vec<FileEntry> }` (409 if `blocks/<name>/` exists; 400 on a bad name); `GET /b/dev/api/reference` → `ReferenceResponse { wafer_guest_version, markdown }`; tools `dev_create_block`, `dev_read_reference` added to `tools::SELECTIONS`; `StageBuildRequest.wafer_guest_version: Option<u32>` (diagnostic `wafer-guest-version` when it differs from `WAFER_GUEST_VERSION`).
- Produces: the guest API (public surface of `wafer_guest.rs`, `WAFER_GUEST_VERSION = 1`):

```rust
pub mod json;                     // Json enum + parse/render
pub struct Schema;                // object(), string(), integer(), number(), boolean(), array(Schema), enum_of(&[&str]),
                                  // .prop(name, Schema), .required(&[&str]), .describe(text)
pub enum Method { Get, Post, Put, Patch, Delete }
pub enum Auth { Public, Authenticated, Admin }
pub struct Request  { pub method: String, pub path: String, pub params: Vec<(String,String)>, pub query: Vec<(String,String)>,
                      pub headers: Vec<(String,String)>, pub body: Vec<u8>, pub user_id: Option<String>, pub user_email: Option<String>, pub roles: Vec<String> }
impl Request { pub fn json(&self) -> Result<Json, String>; pub fn param(&self, name) -> Option<&str>; pub fn query(&self, name) -> Option<&str> }
pub struct Response;              // Response::json(status, &Json), Response::text(status, &str), Response::bytes(status, ct, Vec<u8>), .header(k, v)
pub struct Ctx;                   // handle passed to handlers and init
pub type Handler = fn(&Request, &Ctx) -> Response;
pub struct Endpoint;              // Endpoint::new(Method, "/b/<name>/path/{param}", handler).auth(Auth).summary(text)
                                  //   .input(Schema).output(Schema).agent_tool(name, description)
pub struct Block;                 // Block::new("site/<name>", summary).requires(&["wafer-run/database", ...])
                                  //   .collection("site__<name>__t").storage_folder("site/<name>/f").config_key("SITE__<NAME>__K").endpoint(Endpoint)
pub struct HostError { pub code: String, pub message: String }
pub mod db      { ensure_table(ctx, TableDef) ; create(ctx, collection, Json) -> Json(record) ; get(ctx, collection, id) ; list(ctx, collection, ListOptions) -> Vec<Json>
                  update(ctx, collection, id, Json) ; delete(ctx, collection, id) ; count(ctx, collection, filters) }
pub struct TableDef; pub struct Column;   // TableDef::new("site__x__t").column(Column::text("id").primary_key()).column(Column::text("email").not_null()).index(&["email"], false)
pub struct ListOptions;           // ListOptions::new().filter(field, op, Json).sort(field, desc).limit(n).offset(n)
pub mod storage { put(ctx, folder, key, bytes, content_type) ; get(ctx, folder, key) -> (Vec<u8>, String) ; delete(ctx, folder, key) ; list(ctx, folder, prefix) -> Vec<String> }
pub mod config  { get(ctx, key) -> Option<String> }
pub mod log     { error(msg) ; warn(msg) ; info(msg) ; debug(msg) }
// Required in the user's lib.rs:
//   pub fn block() -> Block;  pub fn init(ctx: &Ctx) -> Result<(), String>;
```

- [ ] **Step 1: Write the failing tests**

`tests/wafer_guest_parity.rs` — compiles the canonical module natively (its `extern "C"` block is `#[cfg(target_arch = "wasm32")]`; a `#[cfg(not(...))]` shim makes every host import panic with "host calls need wasm32") and checks that what the guest renders is what `wafer-block` parses:

```rust
#[path = "../src/blocks/dev/templates/wafer_guest.rs"]
#[allow(dead_code)]
mod wafer_guest;
mod table_template {
    #[path = "../../src/blocks/dev/templates/table/src/lib.rs"]
    pub mod lib;         // its `mod wafer_guest;` line is `#[cfg(target_arch = "wasm32")]`-gated inside the template; on the host it uses `use super::super::wafer_guest`
}

#[test]
fn rendered_block_info_parses_and_matches_the_typed_builder() {
    let rendered = wafer_guest::render_block_info(&table_template::lib::block());
    let parsed: wafer_block::BlockInfo = serde_json::from_str(&rendered).expect("BlockInfo JSON");
    assert_eq!(parsed.name, "site/newsletter");
    parsed.validate().expect("valid");
    let subscribe = parsed.endpoints.iter().find(|e| e.path == "/b/newsletter/subscribe").unwrap();
    assert_eq!(subscribe.method, wafer_block::HttpMethod::Post);
    assert_eq!(subscribe.agent_tool.as_ref().unwrap().name, "subscribe_newsletter");
    assert_eq!(subscribe.input_schema.as_ref().unwrap()["properties"]["email"]["type"], "string");
    let caps = parsed.capabilities.as_ref().unwrap();
    assert!(caps.allows_collection("site__newsletter__subscribers"));
    assert!(caps.schema);
    assert!(!caps.ddl);
    assert!(!caps.raw_sql);
}

#[test]
fn json_codec_round_trips_wire_shapes() {
    use wafer_guest::json::Json;
    let text = r#"{"id":"n1","data":{"email":"a@b.c","n":3,"ok":true,"none":null,"bytes":[104,105],"nested":{"x":[1.5,"y"]}}}"#;
    let parsed = Json::parse(text).unwrap();
    assert_eq!(parsed.get("data").unwrap().get("email").unwrap().as_str(), Some("a@b.c"));
    let rendered = parsed.render();
    assert_eq!(Json::parse(&rendered).unwrap(), parsed);
    assert!(Json::parse("{bad").is_err());
    assert_eq!(Json::parse(r#""a\"b\\c\né""#).unwrap().as_str(), Some("a\"b\\c\né"));
}

#[test]
fn request_frame_is_decoded_and_routed_with_path_params() {
    let frame = r#"[{"kind":"POST:/b/newsletter/subscribe","meta":[{"key":"http.method","value":"POST"},{"key":"http.path","value":"/b/newsletter/subscribe"},{"key":"http.query.src","value":"footer"},{"key":"auth.user_id","value":"u1"},{"key":"auth.user_roles","value":"admin,editor"}]},[123,34,101,109,97,105,108,34,58,34,97,64,98,46,99,34,125]]"#;
    let req = wafer_guest::Request::from_frame(frame.as_bytes()).unwrap();
    assert_eq!(req.method, "POST");
    assert_eq!(req.query("src"), Some("footer"));
    assert_eq!(req.roles, vec!["admin", "editor"]);
    assert_eq!(req.json().unwrap().get("email").unwrap().as_str(), Some("a@b.c"));
    let block = table_template::lib::block();
    let (ep, params) = block.route("GET", "/b/newsletter/subscribers/abc").expect("route with param");
    assert_eq!(ep.path, "/b/newsletter/subscribers/{id}");
    assert_eq!(params, vec![("id".to_string(), "abc".to_string())]);
    assert!(block.route("GET", "/b/other").is_none());
}

#[test]
fn response_renders_the_guest_result_shape() {
    let resp = wafer_guest::Response::json(201, &wafer_guest::json::Json::parse(r#"{"ok":true}"#).unwrap()).header("x-a", "b");
    let text = wafer_guest::render_result(&resp);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["action"], "Respond");
    assert_eq!(v["response"]["data"], serde_json::json!([123, 34, 111, 107, 34, 58, 116, 114, 117, 101, 125]));
    let meta = v["response"]["meta"].as_array().unwrap();
    assert!(meta.iter().any(|m| m["key"] == "resp.status" && m["value"] == "201"));
    assert!(meta.iter().any(|m| m["key"] == "resp.content_type" && m["value"] == "application/json"));
    assert!(meta.iter().any(|m| m["key"] == "resp.header.x-a" && m["value"] == "b"));
    // The host decodes it as the real type:
    let parsed: wafer_block::abi::GuestResult = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed.action, wafer_block::abi::GuestAction::Respond);
}

#[test]
fn templates_carry_the_canonical_module_byte_for_byte() {
    let canonical = include_str!("../src/blocks/dev/templates/wafer_guest.rs");
    assert_eq!(include_str!("../src/blocks/dev/templates/hello/src/wafer_guest.rs"), canonical);
    assert_eq!(include_str!("../src/blocks/dev/templates/table/src/wafer_guest.rs"), canonical);
    assert!(canonical.contains(&format!("pub const WAFER_GUEST_VERSION: u32 = {};", impresspress_core::blocks::dev::WAFER_GUEST_VERSION)));
}
```

`tests/dev_scaffold.rs`:

```rust
#[tokio::test]
async fn create_block_writes_the_template_and_the_module() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let r = output_json(dev_post(&ctx, "/b/dev/api/blocks", json!({"name": "newsletter", "template": "table"})).await).await;
    let paths: Vec<&str> = r["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(paths, vec!["blocks/newsletter/Cargo.toml", "blocks/newsletter/src/lib.rs", "blocks/newsletter/src/wafer_guest.rs"]);
    let lib = output_json(dev_post(&ctx, "/b/dev/api/files/read", json!({"path": "blocks/newsletter/src/lib.rs"})).await).await;
    assert!(lib["content"].as_str().unwrap().contains("site/newsletter"), "the template is instantiated with the block name");
    assert!(lib["content"].as_str().unwrap().contains("site__newsletter__subscribers"));
    let again = dev_post(&ctx, "/b/dev/api/blocks", json!({"name": "newsletter", "template": "hello"})).await;
    assert_eq!(output_status(again).await, 409);
    let bad = dev_post(&ctx, "/b/dev/api/blocks", json!({"name": "Bad-Name", "template": "hello"})).await;
    assert_eq!(output_status(bad).await, 400);
}

#[tokio::test]
async fn reference_returns_the_authoring_guide() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let r = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/reference")).await).await;
    assert_eq!(r["wafer_guest_version"], 1);
    let md = r["markdown"].as_str().unwrap();
    for needle in ["Block::new", "db::ensure_table", "agent_tool", "site__<name>__", "wasm32-wasip1", "no dependencies"] {
        assert!(md.contains(needle), "{needle}");
    }
}

#[tokio::test]
async fn staging_with_a_stale_module_version_is_a_diagnostic() {
    let control = FakeControl::new();
    control.set_validated_info(hello_info("site/hello"));
    let ctx = TestContext::with_dev(control.clone()).await;
    let r = output_json(dev_post(&ctx, "/b/dev/api/builds/stage", json!({
        "block_name": "hello", "artifact_base64": b64(b"\0asm"), "compiler_version": "t", "diagnostics": [], "wafer_guest_version": 0
    })).await).await;
    assert_eq!(r["success"], false);
    assert_eq!(r["diagnostics"][0]["code"], "wafer-guest-version");
}
```

`tests/wafer_guest_golden.rs` (gated: runs when `IMPRESSPRESS_GUEST_GOLDEN=1`, CI sets it):

```rust
fn build_template(name: &str) -> Vec<u8> {
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/src/blocks/dev/templates/").to_string() + name;
    let out = tempfile::tempdir().unwrap();
    copy_dir_all(&src, out.path()).unwrap();          // follows the symlink → a real file in the copy
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-wasip1", "--offline"])
        .current_dir(out.path()).status().unwrap();
    assert!(status.success(), "template {name} must build with plain cargo and no dependencies");
    std::fs::read(out.path().join(format!("target/wasm32-wasip1/release/{}.wasm", name.replace('-', "_")))).unwrap()
}

#[tokio::test]
async fn table_template_creates_its_table_and_serves_its_endpoints_over_json() {
    if std::env::var_os("IMPRESSPRESS_GUEST_GOLDEN").is_none() { eprintln!("skipped: set IMPRESSPRESS_GUEST_GOLDEN=1"); return; }
    let wasm = build_template("table");
    let mut wafer = golden_wafer().await;   // real SQLite DatabaseBlock + in-memory storage + static config, as wafer-run's wrap_hostile_guest_e2e builds
    let caps = { let info: wafer_block::BlockInfo = /* parse __wafer_info via a first load */ ; info.capabilities.unwrap() };
    let block = wafer_run::WasmiBlock::load_with_capabilities_and_limits(&wasm, caps, wafer_run::ResourceLimits::default()).unwrap();
    wafer.register_block("site/newsletter", std::sync::Arc::new(block)).unwrap();
    let wafer = wafer.start().await.unwrap();      // runs Init → ensure_table
    let post = http_msg("POST", "/b/newsletter/subscribe", &[("auth.user_id", "")]);
    let out = wafer.run_block("site/newsletter", post, InputStream::from_bytes(br#"{"email":"a@b.c"}"#.to_vec())).await.collect_buffered().await.unwrap();
    assert_eq!(out.status(), 200);
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&out.body).unwrap()["ok"], true);
    let get = http_msg("GET", "/b/newsletter/subscribers", &[("auth.user_id", "admin_1"), ("auth.user_roles", "admin")]);
    let out = wafer.run_block("site/newsletter", get, InputStream::empty()).await.collect_buffered().await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.body).unwrap();
    assert_eq!(v["subscribers"][0]["email"], "a@b.c");
}

#[tokio::test]
async fn hello_template_answers() {
    if std::env::var_os("IMPRESSPRESS_GUEST_GOLDEN").is_none() { return; }
    let wasm = build_template("hello");
    let block = wafer_run::WasmiBlock::load_with_capabilities(&wasm, wafer_block::BlockCapabilities::none()).unwrap();
    let mut wafer = golden_wafer().await;
    wafer.register_block("site/hello", std::sync::Arc::new(block)).unwrap();
    let wafer = wafer.start().await.unwrap();
    let out = wafer.run_block("site/hello", http_msg("GET", "/b/hello/", &[]), InputStream::empty()).await.collect_buffered().await.unwrap();
    assert!(String::from_utf8_lossy(&out.body).contains("Hello from site/hello"));
}
```

`http_msg` builds a `Message` with `kind = "GET:/path"` and the `http.method`/`http.path` meta (`wafer_block::http_codec::build_http_message` if reachable, else set the meta directly).

- [ ] **Step 2: Run to verify they fail** — the module and templates do not exist.

- [ ] **Step 3: Write `wafer_guest.rs`**

The file is ~900 lines. The load-bearing pieces, verbatim:

```rust
//! wafer_guest.rs — the ImpressPress guest runtime for std-only blocks.
//! VENDORED: version WAFER_GUEST_VERSION. Do not edit; `dev_create_block`
//! writes it and the reference documents its API.
#![allow(dead_code)]
pub const WAFER_GUEST_VERSION: u32 = 1;

// ---- host imports -------------------------------------------------------
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wafer")]
extern "C" {
    fn __wafer_host_log(level_ptr: i32, level_len: i32, msg_ptr: i32, msg_len: i32);
    fn __wafer_host_stream_init(name_ptr: i32, name_len: i32, msg_ptr: i32, msg_len: i32) -> i64;
    fn __wafer_host_stream_write_chunk(handle: i64, ptr: i32, len: i32) -> i32;
    fn __wafer_host_stream_finish(handle: i64) -> i32;
    fn __wafer_host_stream_read_chunk(handle: i64) -> i64;
    fn __wafer_host_stream_take_error(handle: i64) -> i64;
    fn __wafer_host_stream_close(handle: i64);
}
#[cfg(not(target_arch = "wasm32"))]
mod host_shim {   // parity tests compile this file natively; host calls are not available there
    pub unsafe fn __wafer_host_log(_: i32, _: i32, _: i32, _: i32) {}
    pub unsafe fn __wafer_host_stream_init(_: i32, _: i32, _: i32, _: i32) -> i64 { panic!("host calls need wasm32") }
    pub unsafe fn __wafer_host_stream_write_chunk(_: i64, _: i32, _: i32) -> i32 { panic!("host calls need wasm32") }
    pub unsafe fn __wafer_host_stream_finish(_: i64) -> i32 { panic!("host calls need wasm32") }
    pub unsafe fn __wafer_host_stream_read_chunk(_: i64) -> i64 { panic!("host calls need wasm32") }
    pub unsafe fn __wafer_host_stream_take_error(_: i64) -> i64 { panic!("host calls need wasm32") }
    pub unsafe fn __wafer_host_stream_close(_: i64) {}
}
#[cfg(not(target_arch = "wasm32"))]
use host_shim::*;

// ---- ABI exports (wasm32 only; the crate's lib.rs supplies `block()` and `init()`) ----
#[cfg(target_arch = "wasm32")]
mod abi {
    use super::*;
    fn pack(bytes: &[u8]) -> i64 { ((bytes.as_ptr() as u32 as i64) << 32) | bytes.len() as i64 }
    fn leak(s: String) -> &'static [u8] { Box::leak(s.into_boxed_str()).as_bytes() }
    #[no_mangle] pub extern "C" fn __wafer_alloc(size: i32) -> i32 {
        Box::leak(vec![0u8; size.max(0) as usize].into_boxed_slice()).as_mut_ptr() as i32
    }
    #[no_mangle] pub extern "C" fn __wafer_host_codec() -> i32 { 1 }
    #[no_mangle] pub extern "C" fn __wafer_info() -> i64 { pack(leak(render_block_info(&crate::block()))) }
    #[no_mangle] pub extern "C" fn __wafer_handle(ptr: i32, len: i32) -> i64 {
        let frame = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
        let resp = match Request::from_frame(frame) {
            Ok(req) => dispatch(&crate::block(), &req),
            Err(e) => Response::text(400, &format!("bad frame: {e}")),
        };
        pack(leak(render_result(&resp)))
    }
    #[no_mangle] pub extern "C" fn __wafer_lifecycle(ptr: i32, len: i32) -> i64 {
        let ev = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
        let is_init = json::Json::parse(&String::from_utf8_lossy(ev)).ok()
            .and_then(|j| j.get("event_type").and_then(|t| t.as_str().map(|s| s == "Init"))).unwrap_or(false);
        let out = if is_init {
            match crate::init(&Ctx) { Ok(()) => r#"{"Ok":null}"#.to_string(),
                Err(m) => format!(r#"{{"Err":{{"code":"Internal","message":{},"meta":[]}}}}"#, json::escape(&m)) }
        } else { r#"{"Ok":null}"#.to_string() };
        pack(leak(out))
    }
}
```

Pin the `Result<(), WaferError>` JSON shape (`{"Ok":null}` / `{"Err":{...}}`) and `ErrorCode` spelling against `wafer_block` in the parity test (`serde_json::from_str::<Result<(), wafer_block::WaferError>>`). `json` module — a recursive-descent parser and renderer:

```rust
pub mod json {
    #[derive(Clone, Debug, PartialEq)]
    pub enum Json { Null, Bool(bool), Num(f64), Str(String), Arr(Vec<Json>), Obj(Vec<(String, Json)>) }
    impl Json {
        pub fn obj() -> Json { Json::Obj(Vec::new()) }
        pub fn set(mut self, k: &str, v: Json) -> Json { if let Json::Obj(m) = &mut self { m.retain(|(kk, _)| kk != k); m.push((k.into(), v)); } self }
        pub fn get(&self, k: &str) -> Option<&Json> { if let Json::Obj(m) = self { m.iter().find(|(kk, _)| kk == k).map(|(_, v)| v) } else { None } }
        pub fn as_str(&self) -> Option<&str> { if let Json::Str(s) = self { Some(s) } else { None } }
        pub fn as_f64(&self) -> Option<f64> { if let Json::Num(n) = self { Some(*n) } else { None } }
        pub fn as_i64(&self) -> Option<i64> { self.as_f64().filter(|n| n.fract() == 0.0).map(|n| n as i64) }
        pub fn as_bool(&self) -> Option<bool> { if let Json::Bool(b) = self { Some(*b) } else { None } }
        pub fn as_array(&self) -> Option<&[Json]> { if let Json::Arr(a) = self { Some(a) } else { None } }
        pub fn parse(text: &str) -> Result<Json, String> { let mut p = Parser { s: text.as_bytes(), i: 0 }; let v = p.value()?; p.ws(); if p.i != p.s.len() { return Err(format!("trailing data at {}", p.i)); } Ok(v) }
        pub fn render(&self) -> String { let mut out = String::new(); render_into(self, &mut out); out }
    }
    pub fn escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""), '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"), '\r' => out.push_str("\\r"), '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }
    fn render_into(v: &Json, out: &mut String) {
        match v {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Num(n) => { if n.fract() == 0.0 && n.abs() < 1e15 { out.push_str(&format!("{}", *n as i64)); } else { out.push_str(&format!("{n}")); } }
            Json::Str(s) => out.push_str(&escape(s)),
            Json::Arr(a) => { out.push('['); for (i, x) in a.iter().enumerate() { if i > 0 { out.push(','); } render_into(x, out); } out.push(']'); }
            Json::Obj(m) => { out.push('{'); for (i, (k, x)) in m.iter().enumerate() { if i > 0 { out.push(','); } out.push_str(&escape(k)); out.push(':'); render_into(x, out); } out.push('}'); }
        }
    }
    struct Parser<'a> { s: &'a [u8], i: usize }
    impl Parser<'_> {
        fn err<T>(&self, what: &str) -> Result<T, String> { Err(format!("{what} at byte {}", self.i)) }
        fn ws(&mut self) { while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\n' | b'\r' | b'\t') { self.i += 1; } }
        fn value(&mut self) -> Result<Json, String> {
            self.ws();
            match self.s.get(self.i) {
                Some(b'{') => self.object(), Some(b'[') => self.array(), Some(b'"') => Ok(Json::Str(self.string()?)),
                Some(b't') => self.lit("true", Json::Bool(true)), Some(b'f') => self.lit("false", Json::Bool(false)),
                Some(b'n') => self.lit("null", Json::Null), Some(_) => self.number(), None => self.err("unexpected end"),
            }
        }
        fn lit(&mut self, word: &str, v: Json) -> Result<Json, String> {
            if self.s[self.i..].starts_with(word.as_bytes()) { self.i += word.len(); Ok(v) } else { self.err("bad literal") }
        }
        fn number(&mut self) -> Result<Json, String> {
            let start = self.i;
            while self.i < self.s.len() && matches!(self.s[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') { self.i += 1; }
            std::str::from_utf8(&self.s[start..self.i]).ok().and_then(|t| t.parse::<f64>().ok()).map(Json::Num).ok_or_else(|| format!("bad number at byte {start}"))
        }
        fn string(&mut self) -> Result<String, String> {
            self.i += 1; // opening quote
            let mut out = String::new();
            loop {
                let Some(&b) = self.s.get(self.i) else { return self.err("unterminated string"); };
                self.i += 1;
                match b {
                    b'"' => return Ok(out),
                    b'\\' => {
                        let Some(&e) = self.s.get(self.i) else { return self.err("bad escape"); };
                        self.i += 1;
                        match e {
                            b'"' => out.push('"'), b'\\' => out.push('\\'), b'/' => out.push('/'),
                            b'b' => out.push('\u{8}'), b'f' => out.push('\u{c}'), b'n' => out.push('\n'), b'r' => out.push('\r'), b't' => out.push('\t'),
                            b'u' => {
                                let mut cp = self.hex4()?;
                                if (0xD800..0xDC00).contains(&cp) {
                                    if self.s.get(self.i..self.i + 2) != Some(b"\\u") { return self.err("lone surrogate"); }
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    cp = 0x10000 + ((cp - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF);
                                }
                                out.push(char::from_u32(cp).ok_or_else(|| format!("bad code point at byte {}", self.i))?);
                            }
                            _ => return self.err("bad escape"),
                        }
                    }
                    _ => {
                        // copy one UTF-8 sequence starting at b
                        let len = match b { 0x00..=0x7F => 1, 0xC0..=0xDF => 2, 0xE0..=0xEF => 3, _ => 4 };
                        let start = self.i - 1;
                        self.i = start + len;
                        out.push_str(std::str::from_utf8(self.s.get(start..self.i).ok_or("truncated utf-8")?).map_err(|_| "bad utf-8")?);
                    }
                }
            }
        }
        fn hex4(&mut self) -> Result<u32, String> {
            let t = std::str::from_utf8(self.s.get(self.i..self.i + 4).ok_or("short \\u escape")?).map_err(|_| "bad \\u escape")?;
            self.i += 4;
            u32::from_str_radix(t, 16).map_err(|_| "bad \\u escape".to_string())
        }
        fn array(&mut self) -> Result<Json, String> {
            self.i += 1; let mut items = Vec::new(); self.ws();
            if self.s.get(self.i) == Some(&b']') { self.i += 1; return Ok(Json::Arr(items)); }
            loop {
                items.push(self.value()?); self.ws();
                match self.s.get(self.i) { Some(b',') => { self.i += 1; } Some(b']') => { self.i += 1; return Ok(Json::Arr(items)); } _ => return self.err("expected , or ]") }
            }
        }
        fn object(&mut self) -> Result<Json, String> {
            self.i += 1; let mut members = Vec::new(); self.ws();
            if self.s.get(self.i) == Some(&b'}') { self.i += 1; return Ok(Json::Obj(members)); }
            loop {
                self.ws();
                if self.s.get(self.i) != Some(&b'"') { return self.err("expected key"); }
                let k = self.string()?; self.ws();
                if self.s.get(self.i) != Some(&b':') { return self.err("expected :"); }
                self.i += 1;
                let v = self.value()?; members.push((k, v)); self.ws();
                match self.s.get(self.i) { Some(b',') => { self.i += 1; } Some(b'}') => { self.i += 1; return Ok(Json::Obj(members)); } _ => return self.err("expected , or }") }
            }
        }
    }
}
```

`Schema`, `Block`, `Endpoint`, `Request::from_frame` (frame = `[message, bytes]`: `kind`, `meta` → fields; `http.query.*` → `query`; `http.header.*` → `headers`; `auth.user_roles` split on `,`), `Block::route(method, path)` (exact segment match with `{param}` capture, first declared wins), `dispatch` (route → handler; `None` → `Response::text(404, "not found")`; a handler that panics aborts — `panic = "abort"` — so the host sees a trap and the request fails, as the spec's §6.6 states), `render_result` (the `GuestResult` JSON with `data` as an integer array and `resp.status` / `resp.content_type` / `resp.header.*` meta), `render_block_info` (mirrors `wafer_block::BlockInfo`'s serde form — every field the parity test reads: `name`, `version`, `interface`, `summary`, `requires`, `capabilities` (the `BlockCapabilities` serde form with `Allowlist::Only([...])`, `schema: true` whenever a collection is declared, `ddl: false` always — spec amendment #10), `endpoints[]` with `method`, `path`, `summary`, `auth`, `input_schema`, `output_schema`, `agent_tool`). Host calls:

```rust
pub fn call(target: &str, kind: &str, body: &json::Json) -> Result<json::Json, HostError> {
    let msg = format!(r#"{{"kind":"{kind}","meta":[]}}"#);
    let body = body.render();
    unsafe {
        let h = __wafer_host_stream_init(target.as_ptr() as i32, target.len() as i32, msg.as_ptr() as i32, msg.len() as i32);
        if h < 0 { return Err(HostError::code(h as i32, "stream_init")); }
        __wafer_host_stream_write_chunk(h, body.as_ptr() as i32, body.len() as i32);
        let status = __wafer_host_stream_finish(h);
        let mut frames = Vec::new();
        if status == 0 { loop { let p = __wafer_host_stream_read_chunk(h); if p <= 0 { break; } frames.extend_from_slice(unpack(p)); } }
        let err_p = __wafer_host_stream_take_error(h);
        let err = if err_p > 0 { Some(String::from_utf8_lossy(unpack(err_p)).into_owned()) } else { None };
        __wafer_host_stream_close(h);
        match err {
            Some(text) => Err(HostError::from_json(&text)),
            None if status != 0 => Err(HostError::code(status, "stream_finish")),
            None => json::Json::parse(&String::from_utf8_lossy(&frames)).map_err(|m| HostError { code: "Internal".into(), message: m }),
        }
    }
}
```

`db::*`, `storage::*`, `config::get` build the exact wire request objects (`database.ensure_table {table:{name,columns:[{name,kind,nullable,primary_key,...}],indexes:[...]}}`, `database.create {collection,data}`, `database.get {collection,id}`, `database.list {collection,filters:[{field,operator,value}],sort:[{field,descending}],limit,offset}`, `database.update {collection,id,data}`, `database.delete {collection,id}`, `database.count {collection,filters}`, `storage.put {folder,key,data:[..],content_type}`, `storage.get {folder,key}`, `storage.delete`, `storage.list {folder,prefix,limit,offset}`, `config.get {key}`) — copy the field names from `wafer-block/src/wire/{database,storage,config}.rs` at the pinned rev; `FilterDef.operator` values from `wire/database.rs:20-60`. Targets are `wafer-run/database`, `wafer-run/storage`, `wafer-run/config`.

- [ ] **Step 4: Templates**

`hello/Cargo.toml` (and `table/`):

```toml
[package]
name = "hello"            # replaced with the block name by the scaffolder
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]

[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
panic = "abort"
strip = true
```

`hello/src/lib.rs`:

```rust
#[cfg(target_arch = "wasm32")]
mod wafer_guest;
#[cfg(not(target_arch = "wasm32"))]
use super::super::wafer_guest;     // host-side parity tests only
use wafer_guest::*;

pub fn block() -> Block {
    Block::new("site/hello", "Says hello")
        .endpoint(Endpoint::new(Method::Get, "/b/hello/", hello).auth(Auth::Public).summary("Say hello"))
}

pub fn init(_ctx: &Ctx) -> Result<(), String> { Ok(()) }

fn hello(_req: &Request, _ctx: &Ctx) -> Response {
    Response::text(200, "Hello from site/hello — a block compiled in your browser.")
}
```

`table/src/lib.rs` — the newsletter block: `Block::new("site/newsletter", "Newsletter signups").requires(&["wafer-run/database"]).collection("site__newsletter__subscribers")` with `POST /b/newsletter/subscribe` (Public, `agent_tool("subscribe_newsletter", "Subscribe an email address to the newsletter. Creates a subscriber row; duplicates are rejected.", Schema::object().prop("email", Schema::string().describe("Email address")).required(&["email"]), Schema::object().prop("ok", Schema::boolean()))`), `GET /b/newsletter/subscribers` (Admin, output `{subscribers:[{id,email,created_at}]}`), `GET /b/newsletter/subscribers/{id}` (Admin); `init` calls `db::ensure_table(ctx, TableDef::new("site__newsletter__subscribers").column(Column::text("id").primary_key()).column(Column::text("email").not_null().unique()).column(Column::datetime("created_at").default_now()))`. `subscribe` validates the email contains `@`, generates an id from the email's bytes (a simple FNV-1a hex — no `uuid` crate), inserts via `db::create`, and returns `{ok:true}` or 409 on a `Conflict` `HostError`.

The scaffolder replaces `hello`/`newsletter` in `name = "…"`, `site/…`, `/b/…/`, `site__…__` with the requested name (a `Template::instantiate(name)` that does exact-string substitution on the template's own identifiers, tested).

- [ ] **Step 5: `reference.md`** — sections: Layout · The `block()` and `init()` functions · Requests and responses · Routes and path params · Agent tools and schemas · Database (`ensure_table`, `create`, `get`, `list`, `update`, `delete`, `count` — with the namespace rule `site__<name>__*`) · Storage (`site/<name>/…`) · Config (`SITE__<NAME>__*`) · Logging · Limits (fuel, memory, artifact ≤ 4 MiB, no dependencies, no network, no cross-block calls) · What a compile error looks like · Both templates in full. Every code sample must compile against the module (the parity test can `include_str!` the samples? keep it simple: the samples are the templates themselves, included by `include_str!` at render time so they cannot drift).

- [ ] **Step 6: Handlers, endpoints, tools** — `scaffold.rs` (`POST /b/dev/api/blocks` writes the three files through `workspace`/`blobs` with `expected_sha256: null` semantics, 409 if any `blocks/<name>/` entry exists; `GET /b/dev/api/reference`), `wafer_guest_version` in `StageBuildRequest` + the diagnostic, two new `SELECTIONS` rows (`dev_create_block`: "Scaffold a new backend block from a template (`hello` or `table`) under blocks/<name>/. Compile it with dev_compile_block."; `dev_read_reference`: "The authoring reference for backend blocks: API, host services, limits, and the two templates. Read it before writing Rust."), snapshot updates (openapi + tools.json — read them).

- [ ] **Step 7: Run**

Run: `cargo test -p impresspress-core --features block-dev` then `rustup target add wasm32-wasip1 && IMPRESSPRESS_GUEST_GOLDEN=1 cargo test -p impresspress-core --features block-dev --test wafer_guest_golden`
Expected: PASS. Add the golden run to CI's `test` job (it needs `wasm32-wasip1` and the wafer-run `wasmi` dev-feature; `cargo build --offline` inside the temp copy works because the template has no dependencies).

- [ ] **Step 8: Commit**

```bash
git add crates/impresspress-core Cargo.lock
git commit -m "feat(dev): wafer_guest.rs, hello/table templates, dev_create_block, dev_read_reference"
```

---

### Task 5: `dev_compile_block` — snapshot, compile, stage, report

**Files:**
- Modify: `crates/impresspress-core/src/blocks/dev/assets/dev.js` (replace the Plan 2 stub; wire `#dev-compile`)
- Modify: `crates/impresspress-core/src/blocks/dev/page.rs` (a `<select id="dev-compile-block">` of block names next to the button)
- Modify: `crates/impresspress-web/tests/e2e/dev-workspace.spec.ts` (with the fake worker)

**Interfaces:**
- Produces: the page-local tool

```
dev_compile_block { name: string }  →
  { success, build_id?, generation?, diagnostics: [...], stdout, stderr, elapsed_ms, compiler_version, progress: [{phase, ms}] }
```
  Flow: `dev_list_files?prefix=blocks/<name>/` → `dev_read_file` each (utf8 only; a binary file under `blocks/` is a diagnostic `binary-source`) → `compiler.initialize(onProgress)` (progress into the panel) → `compiler.compile({ crateName: name, files, onProgress })` → on `success:false` return the result as-is (`isError` **not** set) → on success `POST /b/dev/api/builds/stage` with `artifact_base64` (from the `Uint8Array`), `source_manifest_sha256` (sha256 of the sorted `path\0sha256\n` lines), `compiler_version`, `diagnostics` (warnings), `wafer_guest_version` (parsed from the `WAFER_GUEST_VERSION` line of `blocks/<name>/src/wafer_guest.rs`) → merge the stage response (`success`, `build_id`, `generation`, its `diagnostics`, `progress`) into the tool result. Any adapter crash → `isError:true` with the message. The `#dev-compile` button runs the same function for the selected block and streams the same progress.

- [ ] **Step 1: Write the failing test** (fake worker; the artifact it returns is the proof guest's bytes patched to `site/hello`, which the test serves as `/__impresspress_dev/compiler/test/hello.wasm` and the fake worker fetches when the crate name is `hello`)

```ts
test('dev_compile_block compiles, stages and activates; errors are results', async ({ page }) => {
  await loginToWorkspace(page);
  await execute(page, 'dev_create_block', { name: 'hello', template: 'hello' });
  const ok = await execute(page, 'dev_compile_block', { name: 'hello' });
  expect(ok.isError).toBeUndefined();
  expect(ok.structuredContent.success).toBe(true);
  expect(ok.structuredContent.generation.cause).toBe('block_compile');
  expect(ok.structuredContent.progress.map((p) => p.phase)).toEqual(expect.arrayContaining(['validating', 'building_runtime', 'publishing', 'active']));
  await expect(page.locator('#dev-progress-steps')).toContainText('active');
  const hello = await page.evaluate(async () => (await fetch('/b/hello/')).text());
  expect(hello).toContain('Hello from a browser-compiled WAFER block!');

  const lib = await execute(page, 'dev_read_file', { path: 'blocks/hello/src/lib.rs' });
  await execute(page, 'dev_write_file', { path: 'blocks/hello/src/lib.rs', content: lib.structuredContent.content + '\nFAIL', expected_sha256: lib.structuredContent.sha256 });
  const bad = await execute(page, 'dev_compile_block', { name: 'hello' });
  expect(bad.isError).toBeUndefined();
  expect(bad.structuredContent.success).toBe(false);
  expect(bad.structuredContent.diagnostics[0]).toMatchObject({ file: 'src/lib.rs', severity: 'error' });
  const still = await page.evaluate(async () => (await fetch('/b/hello/')).status);
  expect(still).toBe(200);                                   // the previous generation stays live
});
```

- [ ] **Step 2: Run to verify it fails** — the stub returns `isError`.

- [ ] **Step 3: Implement** in `dev.js`:

```javascript
  var compiler = null;
  async function ensureCompiler(onProgress) {
    if (!compilerManifest) throw new Error('No compiler in this build.');
    if (!compiler) compiler = new window.ImpresspressCompiler(compilerManifest);
    await compiler.initialize(onProgress);
    return compiler;
  }
  async function snapshotBlock(name) {
    var list = await (await api.get('/b/dev/api/files?prefix=blocks/' + encodeURIComponent(name) + '/')).json();
    var files = {}, diagnostics = [], guestVersion = null;
    for (var i = 0; i < list.files.length; i++) {
      var f = list.files[i];
      var r = await (await api.post('/b/dev/api/files/read', { path: f.path })).json();
      var rel = f.path.slice(('blocks/' + name + '/').length);
      if (r.encoding !== 'utf8') { diagnostics.push({ severity: 'error', code: 'binary-source', message: 'binary file in block source', file: rel }); continue; }
      files[rel] = r.content;
      if (rel === 'src/wafer_guest.rs') { var m = /WAFER_GUEST_VERSION: u32 = (\d+)/.exec(r.content); if (m) guestVersion = Number(m[1]); }
    }
    var lines = Object.keys(files).sort().map(function (p) { return p + '\0' + list.files.find(function (f) { return f.path === 'blocks/' + name + '/' + p; }).sha256; });
    return { files: files, diagnostics: diagnostics, guestVersion: guestVersion, sourceSha: await sha256Hex(lines.join('\n')) };
  }
  async function compileBlock(name) {
    var log = function (p) { appendProgress(p.stage, p.detail || ''); };
    var snap = await snapshotBlock(name);
    if (snap.diagnostics.length) return { success: false, diagnostics: snap.diagnostics, stdout: '', stderr: '', elapsed_ms: 0, compiler_version: null, progress: [] };
    var c = await ensureCompiler(log);
    var result = await c.compile({ crateName: name, files: snap.files, onProgress: log });
    if (!result.success) return { success: false, diagnostics: result.diagnostics, stdout: result.stdout, stderr: result.stderr, elapsed_ms: result.elapsedMs, compiler_version: result.compilerVersion, progress: [] };
    var staged = await (await api.post('/b/dev/api/builds/stage', {
      block_name: name, artifact_base64: toBase64(result.artifact), source_manifest_sha256: snap.sourceSha,
      compiler_version: result.compilerVersion, diagnostics: result.diagnostics, wafer_guest_version: snap.guestVersion,
    })).json();
    return { success: staged.success, build_id: staged.build_id, generation: staged.generation, diagnostics: result.diagnostics.concat(staged.diagnostics || []),
             stdout: result.stdout, stderr: result.stderr, elapsed_ms: result.elapsedMs, compiler_version: result.compilerVersion, progress: staged.progress || [] };
  }
  function registerCompileTool() {
    document.modelContext.registerTool({
      name: 'dev_compile_block',
      description: 'Compile blocks/<name>/ with the in-browser Rust toolchain (wasm32-wasip1, no dependencies). On success the block is validated and activated immediately and its routes are live; on failure the result carries structured compiler diagnostics. Only one compile runs at a time.',
      inputSchema: { type: 'object', properties: { name: { type: 'string', description: 'Block name, as used in blocks/<name>/' } }, required: ['name'], additionalProperties: false },
      outputSchema: { type: 'object', properties: { success: { type: 'boolean' }, build_id: { type: 'string' }, diagnostics: { type: 'array' }, generation: { type: 'object' }, stdout: { type: 'string' }, stderr: { type: 'string' }, elapsed_ms: { type: 'integer' }, compiler_version: { type: ['string', 'null'] }, progress: { type: 'array' } }, required: ['success', 'diagnostics'] },
      execute: withProgress(async function (args) {
        try { var r = await compileBlock(String(args.name)); return { content: [{ type: 'text', text: JSON.stringify(r) }], structuredContent: r }; }
        catch (e) { return { isError: true, content: [{ type: 'text', text: 'compile failed: ' + (e && e.message || e) }] }; }
      })
    }, { signal: abort.signal });
  }
```

`toBase64` chunks the `Uint8Array` through `btoa` (64 KiB slices); `sha256Hex` uses `crypto.subtle`. `#dev-compile` click → `compileBlock(select.value)` under the same progress wrapper.

- [ ] **Step 4: Run** the spec (fake worker). Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/impresspress-core/src/blocks/dev crates/impresspress-web/tests
git commit -m "feat(dev): dev_compile_block — snapshot, compile in the worker, stage, activate"
```

---

### Task 6: Checkpoint — the newsletter block compiled by Rubrc in the browser

**Files:**
- Create: `crates/impresspress-web/tests/e2e/dev-compile.spec.ts`
- Modify: `.github/workflows/ci.yml`, `ci-main.yml` (new job `e2e-dev-compile`)

- [ ] **Step 1: Write the spec** (real compiler; the slow suite)

```ts
test('an agent scaffolds, compiles and uses a Rust block end to end', async ({ browser, page }) => {
  test.setTimeout(10 * 60 * 1000);
  await loginToWorkspace(page);
  const ref = await execute(page, 'dev_read_reference', {});
  expect(ref.structuredContent.markdown).toContain('db::ensure_table');

  await execute(page, 'dev_create_block', { name: 'newsletter', template: 'table' });
  const compiled = await execute(page, 'dev_compile_block', { name: 'newsletter' });
  expect(compiled.structuredContent.success, JSON.stringify(compiled.structuredContent.diagnostics)).toBe(true);
  expect(compiled.structuredContent.elapsed_ms).toBeGreaterThan(0);

  const sub = await page.evaluate(async () => (await fetch('/b/newsletter/subscribe', { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{"email":"a@b.c"}' })).json());
  expect(sub.ok).toBe(true);
  const list = await page.evaluate(async () => (await fetch('/b/newsletter/subscribers')).json());
  expect(list.subscribers[0].email).toBe('a@b.c');

  // The new block's curated tool is discoverable by an anonymous visitor.
  const shopper = await (await browser.newContext()).newPage();
  await shopper.addInitScript(MODEL_CONTEXT_POLYFILL);
  await shopper.goto('/');
  const tools = (await registeredTools(shopper, 1)).map((t) => t.name);
  expect(tools).toContain('subscribe_newsletter');
  const viaTool = await execute(shopper, 'subscribe_newsletter', { email: 'b@c.d' });
  expect(viaTool.structuredContent.ok).toBe(true);

  // A broken edit yields a diagnostic with a line number; the working block stays live.
  const lib = await execute(page, 'dev_read_file', { path: 'blocks/newsletter/src/lib.rs' });
  await execute(page, 'dev_write_file', { path: 'blocks/newsletter/src/lib.rs', content: lib.structuredContent.content.replace('Ok(())', 'Ok(()'), expected_sha256: lib.structuredContent.sha256 });
  const broken = await execute(page, 'dev_compile_block', { name: 'newsletter' });
  expect(broken.structuredContent.success).toBe(false);
  expect(broken.structuredContent.diagnostics[0].line).toBeGreaterThan(0);
  expect((await page.evaluate(async () => (await fetch('/b/newsletter/subscribers')).json())).subscribers.length).toBe(2);

  // Rollback removes the block and its tool.
  const gens = await execute(page, 'dev_list_generations', {});
  const beforeBlock = gens.structuredContent.generations.find((g) => g.blocks === 0);
  await execute(page, 'dev_rollback', { id: beforeBlock.id });
  expect(await page.evaluate(async () => (await fetch('/b/newsletter/subscribers')).status)).toBe(404);
  await shopper.reload();
  await shopper.evaluate(() => window.__impresspressWebmcp.refresh());
  expect((await registeredTools(shopper, 1)).map((t) => t.name)).not.toContain('subscribe_newsletter');
});
```

- [ ] **Step 2: CI job `e2e-dev-compile`** — `needs: e2e-build`; `actions/cache` on `examples/dev-sandbox/compiler/dist` keyed by `hashFiles('examples/dev-sandbox/compiler/PIN.json', 'examples/dev-sandbox/compiler/src/**')`; on a miss run `compiler/build-compiler.sh` (Bun via `oven-sh/setup-bun`, nightly Rust for the VFS crate per Rubrc's README); then `examples/dev-sandbox/build.sh --check`, serve `dist/` on 8083 with the COOP/COEP-aware Node server from Task 1 (the SW sets the headers for `/b/dev`, but the *probe* and the initial static load of the compiler assets are plain files — `python3 -m http.server` is enough for the SW path; use it), run `dev-compile.spec.ts` with `TEST_PORT=8083`. Record `elapsed_ms` from the test output into the job summary.

- [ ] **Step 3: Run locally**, then commit and PR:

```bash
git add crates/impresspress-web/tests .github/workflows
git commit -m "test(e2e): compile the newsletter block with Rubrc in the browser"
git push && gh pr create --title "Dev sandbox: Rubrc in the browser and the wafer_guest.rs contract" --body-file - <<'EOF'
Plan 3 of the dev.impresspress.org sandbox (spec §6, §8, §9.2, §20.1/2/4/6).

- Rubrc pinned and packaged as versioned static assets (brotli-split parts ≤ 24 MiB, hashed manifest, license recorded); the compiler runs in a page-owned module worker behind `BrowserRustCompiler`.
- `wafer_guest.rs`: std-only guest runtime (ABI, JSON codec, request/response, schema builder, JSON host calls to database/storage/config); parity tests against wafer-block's types; golden tests build both templates with plain cargo and run them under wasmi.
- `dev_create_block`, `dev_read_reference`, `dev_compile_block`; structured diagnostics; the previous generation stays live on a failed compile.
- e2e: newsletter block compiled in the browser, its table created through `database.ensure_table`, its tool discovered and invoked anonymously, rollback removes it.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
```

## Self-review notes

- Spec coverage: §6.1–6.2 (Task 4), §6.3–6.5 (Plan 0 + Task 4's `capabilities` rendering; the validation rules are Plan 1 Task 8, exercised here by the golden guest), §6.6 limits (`load_guest` in Plan 1; `panic = "abort"` in the templates), §8 (Tasks 1, 3, 5), §9.2 the three tools (Tasks 4–5), §16 scenario step 2 (Task 6), §19 verification items 1–3 (Task 1 README), §20.1 (`__wafer_host_codec` export in `wafer_guest.rs`), §20.4 (Task 1 packaging), §20.6 (Plan 1's CSP; Task 2 proves isolation).
- Names other plans rely on: `blocks::dev::templates::{wafer_guest.rs, hello, table, reference.md}`, `blocks::dev::scaffold`, `WAFER_GUEST_VERSION`, the `BrowserRustCompiler` ES-module export of `/b/dev/static/compiler-adapter.js` (`blocks/dev/assets/compiler-adapter.js`; there is deliberately NO window global — `dev.js` imports it), `/__impresspress_dev/compiler/manifest.json`, `examples/dev-sandbox/compiler/{PIN.json, build-compiler.sh, fetch-dist.sh, pack-dist.sh, dist/}`, e2e `fixtures/{dev-sandbox.ts, fake-compiler-worker.js}`, tools `dev_create_block`, `dev_read_reference`, `dev_compile_block`.
- Spec amendment to record when Task 4 lands: #8 — `StageBuildRequest.wafer_guest_version` comes from the page (read from the vendored file), not from `BlockInfo`, which has no such field.
