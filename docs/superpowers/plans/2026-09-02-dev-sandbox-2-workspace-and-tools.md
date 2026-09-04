# Dev Sandbox Plan 2 — The workspace page, page-scoped tools, the starter site and dev.impresspress.org

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A WebMCP agent on `/b/dev` can build a site and stock a shop through page-scoped `dev_*` and `shop_*` tools while the human watches it happen in a live preview; an anonymous shopper browses the result at `/`; the whole thing is deployed at dev.impresspress.org from `examples/dev-sandbox/`.

**Architecture:** The dev block publishes its own WebMCP manifest at `/b/dev/api/tools.json` through `generate_webmcp_selected` — a curated list of its own endpoints plus twelve products admin endpoints — so every schema is derived from the typed contracts. `dev.js` registers those tools on `/b/dev` only, wraps mutating calls with a status poller that drives the progress panel and reloads the live-site iframe, and re-registers the global manifest when the runtime generation changes. `webmcp.js` learns to wait for the service worker. The starter site is a seed; `examples/dev-sandbox/` is config, seed and deploy files around the sealed web build.

**Tech Stack:** Rust (impresspress-core dev block, maud SSR), vanilla JS (CSP: no eval, no CDN), Playwright with the existing `document.modelContext` polyfill, Cloudflare Workers static assets via `wrangler`.

**Spec:** `docs/superpowers/specs/2026-09-02-dev-sandbox-design.md` — §4, §9, §15, §16 (scenario steps 1, 3–5), §20 (amendments 3, 5).

**Depends on:** Plan 1 merged (the dev block, generations, `browser-devtools`, sealed dev bundle recipe). Plan 0's `generate_webmcp_selected` and `frame_ancestors`.

## Global Constraints

- **Page-scoped means page-scoped.** No dev or shop endpoint carries `agent_tool`; `tools.json` is served only under `/b/dev` (Admin); `dev.js` is loaded only by the `/b/dev` document; a test proves `/` registers none of them.
- **Schemas are derived.** `shop_*` tools are `ToolSelection`s over products endpoints; the manifest test asserts zero refusals so a contract change that breaks a shop tool fails CI. Only the two page-local tools (`dev_compile_block` — a stub until Plan 3 — and `dev_export` — a stub until Plan 4) have inline schemas.
- **Trusted page, sandboxed-by-origin preview.** The live-site iframe is `sandbox="allow-scripts allow-same-origin allow-forms allow-popups"`: without `allow-same-origin` the framed site's fetches to `/b/products/*` would be cross-origin and CORS-blocked, defeating the preview. The framed content is the visitor's own site; the prompt-injection boundary is that no tool is registered from inside the frame and the parent exposes nothing on `window`. Record this in the spec's amendments as #7 when the task lands.
- **No new CSS framework, no CDN.** The page's CSS and JS ship from the block; the browser CSP allows `'self'` and inline.
- Every `/b/dev` response is `no-store`; assets under `/b/dev/static/` are `no-cache`.
- `///` doc comments on contracts become tool descriptions; every `shop_*` description states its side effect.

---

### Task 1: `webmcp.js` — wait for the service worker; expose `refresh()`

**Files:**
- Modify: `crates/impresspress-core/src/ui/assets/webmcp.js` (whole file, 145 lines)
- Modify: `crates/impresspress-web/tests/e2e/webmcp.spec.ts` (polyfill :20-55 gains `unregisterTool`)
- Modify: `crates/impresspress-web/tests/e2e/smoke.spec.ts` (cold-visitor case)

**Interfaces:**
- Produces: `window.__impresspressWebmcp = { refresh(): Promise<void>, generation: () => number }`; on a page where `navigator.serviceWorker` exists and `controller` is `null`, the first manifest fetch waits for `navigator.serviceWorker.ready`; `refresh()` unregisters every tool it registered (via `document.modelContext.unregisterTool(name)` when present) and registers the current manifest.

- [ ] **Step 1: Write the failing tests**

`smoke.spec.ts`:

```ts
test('a cold visitor gets WebMCP tools without a reload', async ({ page }) => {
  // Fresh context: no SW yet. The polyfill must be installed before any script runs.
  await page.addInitScript(MODEL_CONTEXT_POLYFILL);
  await page.goto('/b/auth/login');
  const names = await page.waitForFunction(() => {
    const t = document.modelContext.__tools();
    return t.length > 0 ? t.map((x) => x.name) : null;
  }, null, { timeout: 30_000 });
  expect(await names.jsonValue()).toContain('list_products');
});
```

(export `MODEL_CONTEXT_POLYFILL` from a shared `fixtures/model-context-polyfill.ts` and import it in both specs.) `webmcp.spec.ts`:

```ts
test('refresh() re-registers the manifest and drops stale tools', async ({ page }) => {
  await page.addInitScript(MODEL_CONTEXT_POLYFILL);
  await page.goto('/b/auth/login');
  await registeredTools(page, 1);
  const before = await page.evaluate(() => document.modelContext.__tools().map((t) => t.name).sort());
  await page.evaluate(() => document.modelContext.registerTool({ name: 'stale_tool', description: 'x', inputSchema: { type: 'object' }, execute: async () => ({ content: [] }) }));
  await page.evaluate(() => window.__impresspressWebmcp.refresh());
  const after = await page.evaluate(() => document.modelContext.__tools().map((t) => t.name).sort());
  expect(after).toEqual(before);
});
```

Polyfill: add `unregisterTool(name) { tools.delete(name); }` next to `registerTool`.

- [ ] **Step 2: Run to verify they fail**

Run: `cd crates/impresspress-web && npx playwright test --config=tests/playwright.config.ts tests/e2e/smoke.spec.ts -g "cold visitor"` against `python3 -m http.server 8080 -d pkg`
Expected: timeout — zero tools on the first load.

- [ ] **Step 3: Implement**

Restructure `webmcp.js` around a `registered` set and two functions, keeping `buildRequest` and `register` as they are:

```javascript
  var registered = [];
  var generation = 0;

  function unregisterAll() {
    if (typeof document.modelContext.unregisterTool === 'function') {
      registered.forEach(function (name) {
        try { document.modelContext.unregisterTool(name); } catch (e) { /* already gone */ }
      });
    }
    registered = [];
  }

  function load() {
    return fetch('/b/webmcp/manifest.json', { credentials: 'same-origin' })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (manifest) {
        if (!manifest || !Array.isArray(manifest.tools)) { return; }
        manifest.tools.forEach(function (tool) {
          try { register(tool); registered.push(tool.name); } catch (e) { /* keep the rest */ }
        });
        generation += 1;
      })
      .catch(function () { /* degraded, not broken */ });
  }

  function refresh() { unregisterAll(); return load(); }

  window.__impresspressWebmcp = { refresh: refresh, generation: function () { return generation; } };

  // In a service-worker build the first paint beats the worker: the manifest
  // route is served by the worker, so wait for it to control the page.
  var sw = navigator.serviceWorker;
  if (sw && !sw.controller && typeof sw.ready === 'object') {
    sw.ready.then(load, load);
  } else {
    load();
  }
```

`sw.ready` resolves once a registration is active; on a native server `navigator.serviceWorker` exists but `ready` never resolves without a registration — guard with `sw.getRegistration().then(function (r) { return r ? sw.ready : null; })` so the native path loads immediately. Write it that way.

- [ ] **Step 4: Run the tests**

Run: the smoke spec against the SW build and `webmcp.spec.ts` against a native server (see `ci.yml:541-579` for the exact server flags).
Expected: PASS; the existing `every_page_includes_the_webmcp_registration_script` test still passes (`webmcp_js_url` changes with the hash — that is expected).

- [ ] **Step 5: Commit**

```bash
git add crates/impresspress-core/src/ui/assets/webmcp.js crates/impresspress-web/tests/e2e
git commit -m "fix(webmcp): wait for the service worker on a cold visit; refresh() re-registers"
```

---

### Task 2: `tools.json` — the page-scoped manifest, `dev_*` and `shop_*`

**Files:**
- Create: `crates/impresspress-core/src/blocks/dev/tools.rs`
- Modify: `crates/impresspress-core/src/blocks/dev/{mod.rs, contracts.rs}`
- Create: `crates/impresspress-core/tests/dev_tools_manifest.rs`, `crates/impresspress-core/tests/snapshots/dev.tools.json`

**Interfaces:**
- Produces: `GET /b/dev/api/tools.json` (Admin, `no-store`) → the WebMCP manifest `{ schema_version: 1, tools: [...] }` for exactly these selections (`tools::SELECTIONS: &[(block, method, path, name, description)]`):

| name | endpoint |
|---|---|
| `dev_status` | `GET /b/dev/api/status` |
| `dev_list_files` | `GET /b/dev/api/files` |
| `dev_read_file` | `POST /b/dev/api/files/read` |
| `dev_write_file` | `POST /b/dev/api/files/write` |
| `dev_delete_file` | `POST /b/dev/api/files/delete` |
| `dev_list_generations` | `GET /b/dev/api/generations` |
| `dev_get_generation` | `GET /b/dev/api/generations/{id}` |
| `dev_rollback` | `POST /b/dev/api/generations/{id}/rollback` |
| `dev_remove_block` | `POST /b/dev/api/blocks/{name}/remove` |
| `shop_list_products` | `GET /b/products/api/admin/products` |
| `shop_create_product` | `POST /b/products/api/admin/products` |
| `shop_update_product` | `PATCH /b/products/api/admin/products/{id}` |
| `shop_delete_product` | `DELETE /b/products/api/admin/products/{id}` |
| `shop_restore_product` | `POST /b/products/api/admin/products/{id}/restore` |
| `shop_list_groups` | `GET /b/products/api/admin/groups` |
| `shop_create_group` | `POST /b/products/api/admin/groups` |
| `shop_list_offers` | `GET /b/products/api/admin/products/{product_id}/offers` |
| `shop_create_offer` | `POST /b/products/api/admin/products/{product_id}/offers` |
| `shop_update_offer` | `PATCH /b/products/api/admin/products/{product_id}/offers/{offer_id}` |
| `shop_publish_offer` | `POST /b/products/api/admin/products/{product_id}/offers/{offer_id}/publish` |
| `shop_archive_offer` | `DELETE /b/products/api/admin/products/{product_id}/offers/{offer_id}` |

  Descriptions are written in `tools.rs` (each mutating one names its side effect: "Creates a product in `draft` status; call `shop_update_product` with `status: active` to make it visible to shoppers."). The `{id}` path params on dev routes need `path_params_schema` on their endpoints (Plan 1 declared them with hand-written `id_path_schema()`-style schemas — verify).
- Consumes: `wafer_core::discovery::{generate_webmcp_selected, ToolSelection}`, `ctx.registered_blocks()`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn tools_json_publishes_every_selection_with_zero_refusals() {
    let ctx = TestContext::with_products().await.with_dev_added(FakeControl::new()).await;   // products + dev
    let doc = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/tools.json")).await).await;
    let mut names: Vec<String> = doc["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    names.sort();
    let mut expected: Vec<String> = impresspress_core::blocks::dev::tools::SELECTIONS.iter().map(|s| s.3.to_string()).collect();
    expected.sort();
    assert_eq!(names, expected, "every curated tool is published — a missing one means a refusal");
    for tool in doc["tools"].as_array().unwrap() {
        assert!(tool["inputSchema"].is_object(), "{}", tool["name"]);
        assert!(!tool["description"].as_str().unwrap().is_empty());
    }
}

#[tokio::test]
async fn shop_offer_tools_keep_the_recursive_condition_under_defs() {
    let ctx = TestContext::with_products().await.with_dev_added(FakeControl::new()).await;
    let doc = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/tools.json")).await).await;
    let create = doc["tools"].as_array().unwrap().iter().find(|t| t["name"] == "shop_create_offer").unwrap();
    assert!(create["inputSchema"]["$defs"].is_object(), "{create}");
}

#[tokio::test]
async fn no_dev_or_shop_tool_leaks_into_the_global_manifest() {
    let ctx = TestContext::with_products().await.with_dev_added(FakeControl::new()).await;
    let doc = impresspress_core::test_support::discovery_json_as(&ctx, "/b/webmcp/manifest.json", &["admin"]).await;
    for tool in doc["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        assert!(!name.starts_with("dev_") && !name.starts_with("shop_"), "{name} leaked");
    }
}

#[tokio::test]
async fn tools_json_matches_its_snapshot() {
    // Same discipline as /openapi.json: UPDATE_DEV_TOOLS_SNAPSHOT=1 regenerates; read every changed line.
    let ctx = TestContext::with_products().await.with_dev_added(FakeControl::new()).await;
    let doc = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/tools.json")).await).await;
    let rendered = serde_json::to_string_pretty(&doc).unwrap() + "\n";
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/dev.tools.json");
    if std::env::var_os("UPDATE_DEV_TOOLS_SNAPSHOT").is_some() {
        std::fs::write(path, &rendered).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("missing snapshot {path}: {e} — run with UPDATE_DEV_TOOLS_SNAPSHOT=1 once, then read it"));
    assert_eq!(rendered, expected, "tools.json changed — every changed line is a decision; regenerate deliberately with UPDATE_DEV_TOOLS_SNAPSHOT=1");
}
```

`with_dev_added` — a `TestContext` method that adds the dev block to an existing context (Plan 1's `with_dev` builds a fresh one). The offer endpoints use hand-written `input_schema`s that already carry `$defs` (`products/mod.rs:386,405,438`), so the second test passes once Plan 0's `$defs` retention is pinned.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p impresspress-core --features block-dev --test dev_tools_manifest`
Expected: 404.

- [ ] **Step 3: Implement `tools.rs`**

```rust
pub const SELECTIONS: &[(&str, HttpMethod, &str, &str, &str)] = &[
    ("impresspress/dev", HttpMethod::Get, "/b/dev/api/status", "dev_status",
     "Read the sandbox state: active generation, runtime generation, active blocks, any activation in progress. Call this first."),
    // ... one row per table entry above ...
    ("impresspress/products", HttpMethod::Post, "/b/products/api/admin/products", "shop_create_product",
     "Create a product. It starts in `draft` status and is invisible to shoppers until `shop_update_product` sets `status: \"active\"`."),
];

pub async fn handle(ctx: &dyn Context) -> OutputStream {
    let blocks = ctx.registered_blocks();
    let selections: Vec<ToolSelection> = SELECTIONS.iter().map(|(b, m, p, n, d)| ToolSelection {
        block: (*b).into(), method: *m, path: (*p).into(), name: (*n).into(), description: (*d).into(),
    }).collect();
    let (doc, refused) = generate_webmcp_selected(&blocks, AuthLevel::Admin, |_b, ep| ep.auth, &selections);
    for r in &refused {
        tracing::error!(block = %r.block, path = %r.path, tool = %r.tool_name, reason = %r.reason, "dev tools.json refusal");
    }
    ResponseBuilder::new().set_header("Cache-Control", "no-store").json(&doc)
}
```

Products may be absent from a build (`block-products` off): selections for a missing block are `SelectionNotFound` refusals — log at `warn` and continue; the manifest test runs with products present.

- [ ] **Step 4: Run and snapshot**

Run: `UPDATE_DEV_TOOLS_SNAPSHOT=1 cargo test -p impresspress-core --features block-dev --test dev_tools_manifest && cargo test -p impresspress-core --features block-dev --test dev_tools_manifest`
Expected: PASS; read the snapshot — 21 tools, every `invocation.path` matches the table.

- [ ] **Step 5: Commit**

```bash
git add crates/impresspress-core
git commit -m "feat(dev): page-scoped tools.json — dev_* and shop_* projected from typed contracts"
```

---

### Task 3: The `/b/dev` document, `dev.js` and `dev.css`

**Files:**
- Create: `crates/impresspress-core/src/blocks/dev/page.rs`
- Create: `crates/impresspress-core/src/blocks/dev/assets/dev.js`, `assets/dev.css`
- Modify: `crates/impresspress-core/src/blocks/dev/mod.rs` (routes `GET /b/dev`, `GET /b/dev/static/dev.js`, `GET /b/dev/static/dev.css`)
- Create: `crates/impresspress-core/tests/dev_page.rs`

**Interfaces:**
- Produces: `GET /b/dev` — an SSR document (via `ui::shell_page` with `NavKind::Admin`, crumb "Workspace") carrying `Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Embedder-Policy: require-corp`, `Cache-Control: no-store`; body sections with stable ids: `#dev-guide` (how it works + suggested prompt + credentials note), `#dev-files` (tree), `#dev-editor` (`<textarea>` + save/delete/new), `#dev-preview` (`<iframe id="dev-preview-frame" src="/" sandbox="allow-scripts allow-same-origin allow-forms allow-popups">`), `#dev-progress` (phase list + log), `#dev-actions` (Compile — disabled until Plan 3; Export — disabled until Plan 4; Refresh tools), `<script src="/b/dev/static/dev.js" defer>`.
- Produces: `dev.js` — registers `tools.json` under an `AbortController` using the `register`/`buildRequest` code lifted from `webmcp.js` into a shared `webmcp-core` (extract the two functions into `crates/impresspress-core/src/ui/assets/webmcp-core.js`, `include_str!`ed into both scripts at build time by concatenation in `assets.rs` so no runtime import is needed); wraps every mutating tool with `withProgress(execute)`; registers the two page-local stubs `dev_compile_block` and `dev_export` returning `isError` "not available yet"; drives the panes over the HTTP API; calls `window.__impresspressWebmcp.refresh()` when `status.runtime_generation` changes; aborts on `pagehide` and on any `401`/`403` from the API.
- Consumes: `/b/dev/api/*` from Plan 1 and Task 2.

- [ ] **Step 1: Write the failing tests** (`tests/dev_page.rs`)

```rust
#[tokio::test]
async fn dev_page_is_admin_only_cross_origin_isolated_and_uncached() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    assert_eq!(output_status(ctx.dispatch(anon_msg("retrieve", "/b/dev")).await).await, 302 /* login redirect for HTML */);
    let out = ctx.dispatch(admin_msg("retrieve", "/b/dev")).await;
    assert_eq!(output_header(out, "cross-origin-opener-policy").await.as_deref(), Some("same-origin"));
    let out = ctx.dispatch(admin_msg("retrieve", "/b/dev")).await;
    assert_eq!(output_header(out, "cross-origin-embedder-policy").await.as_deref(), Some("require-corp"));
    let out = ctx.dispatch(admin_msg("retrieve", "/b/dev")).await;
    assert_eq!(output_header(out, "cache-control").await.as_deref(), Some("no-store"));
    let html = output_html(ctx.dispatch(admin_msg("retrieve", "/b/dev")).await).await;
    for id in ["dev-guide", "dev-files", "dev-editor", "dev-preview", "dev-progress", "dev-actions"] {
        assert!(html.contains(&format!("id=\"{id}\"")), "{id}");
    }
    assert!(html.contains(r#"<iframe id="dev-preview-frame" src="/""#));
    assert!(html.contains("/b/dev/static/dev.js"));
    assert!(html.contains("admin@example.com"), "the guide shows the local credentials");
}

#[tokio::test]
async fn other_pages_are_not_cross_origin_isolated() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let out = ctx.dispatch(admin_msg("retrieve", "/b/dev/api/status")).await;
    assert_eq!(output_header(out, "cross-origin-opener-policy").await, None);
}

#[test]
fn dev_js_registers_only_from_tools_json_and_never_touches_the_global_manifest() {
    let js = impresspress_core::blocks::dev::assets::dev_js();
    assert!(js.contains("/b/dev/api/tools.json"));
    assert!(!js.contains("/b/webmcp/manifest.json"), "the global manifest is webmcp.js's job");
    assert!(js.contains("AbortController"));
    assert!(js.contains("pagehide"));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p impresspress-core --features block-dev --test dev_page`
Expected: 404.

- [ ] **Step 3: Implement `page.rs`**

```rust
pub async fn workspace(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let body = maud::html! {
        div .dev-workspace {
            section #dev-guide .dev-pane {
                h2 { "How this workspace works" }
                p { "This page is a WebMCP workspace. An agent in your browser sees the tools registered here and can edit the site under " code { "site/" } ", write Rust backend blocks under " code { "blocks/<name>/" } ", stock the shop with the " code { "shop_*" } " tools, and export the result. Every successful change is live at " a href="/" target="_blank" { "/" } " immediately; " code { "dev_rollback" } " undoes a generation." }
                p { "Start with " code { "dev_status" } ". Credentials for this browser-local instance: " code { "admin@example.com" } " / " code { "admin123" } "." }
                details { summary { "Suggested prompt" } pre #dev-suggested-prompt { (SUGGESTED_PROMPT) } }
            }
            section #dev-files .dev-pane { h2 { "Files" } ul #dev-file-list {} button #dev-new-file type="button" { "New file" } }
            section #dev-editor .dev-pane {
                h2 #dev-editor-title { "Editor" }
                textarea #dev-editor-text spellcheck="false" {}
                div .dev-editor-actions { button #dev-save type="button" { "Save" } button #dev-delete type="button" { "Delete" } }
            }
            section #dev-preview .dev-pane {
                h2 { "Live site" }
                iframe #dev-preview-frame src="/" sandbox="allow-scripts allow-same-origin allow-forms allow-popups" title="Live site" {}
            }
            section #dev-progress .dev-pane { h2 { "Progress" } ol #dev-progress-steps {} pre #dev-log {} }
            section #dev-actions .dev-pane {
                button #dev-compile type="button" disabled { "Compile block" }
                button #dev-export type="button" disabled { "Export" }
                button #dev-refresh-tools type="button" { "Refresh tools" }
            }
        }
        link rel="stylesheet" href="/b/dev/static/dev.css";
        script src="/b/dev/static/dev.js" defer {}
    };
    let out = ui::shell_page(ctx, msg, ui::Shell::simple("Workspace", ui::NavKind::Admin, "Workspace"), body).await;
    with_headers(out, &[("Cross-Origin-Opener-Policy", "same-origin"), ("Cross-Origin-Embedder-Policy", "require-corp"), ("Cache-Control", "no-store")])
}
```

`with_headers` sets `resp.header.*` meta on the buffered response (use the same mechanism `ResponseBuilder::set_header` uses; if `shell_page` returns an `OutputStream` whose headers cannot be amended afterwards, render through `Page::render()` and build the response yourself with `ResponseBuilder`). `SUGGESTED_PROMPT`:

```
Build me a small online shop for handmade ceramics. Create a home page at site/index.html that lists products from /b/products/catalog and lets a visitor open one, using the storefront widget from /b/products/storefront.js. Then create three products with shop_create_product, give each a published offer with shop_create_offer and shop_publish_offer, and set their status to active with shop_update_product. Show me the live site when you are done.
```

- [ ] **Step 4: Implement `dev.js`**

Structure (vanilla, IIFE, `'use strict'`):

```javascript
(function () {
  'use strict';
  var api = {
    get: function (path) { return fetch(path, { credentials: 'same-origin' }).then(check); },
    post: function (path, body) { return fetch(path, { method: 'POST', credentials: 'same-origin', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body || {}) }).then(check); }
  };
  function check(r) { if (r.status === 401 || r.status === 403) { abort.abort(); } return r; }

  var abort = new AbortController();
  var lastRuntimeGeneration = null;
  var polling = null;

  // ---- progress ---------------------------------------------------------
  function renderStatus(status) { /* fills #dev-progress-steps from status.activation, #dev-log lines */ }
  function startPolling() {
    if (polling) return;
    polling = setInterval(function () {
      api.get('/b/dev/api/status').then(function (r) { return r.json(); }).then(function (s) {
        renderStatus(s);
        if (lastRuntimeGeneration !== null && s.runtime_generation !== lastRuntimeGeneration) { window.__impresspressWebmcp.refresh(); }
        lastRuntimeGeneration = s.runtime_generation;
      });
    }, 300);
  }
  function stopPolling() { clearInterval(polling); polling = null; }
  function withProgress(execute) {
    return async function (args) {
      startPolling();
      try { var result = await execute(args); }
      finally { stopPolling(); await refreshAfterChange(); }
      return result;
    };
  }
  async function refreshAfterChange() {
    var s = await (await api.get('/b/dev/api/status')).json();
    renderStatus(s);
    document.getElementById('dev-preview-frame').contentWindow.location.reload();
    await loadFiles();
  }

  // ---- tools ------------------------------------------------------------
  var MUTATING = /^(dev_write_file|dev_delete_file|dev_rollback|dev_remove_block|shop_)/;
  function registerFromManifest(manifest) {
    manifest.tools.forEach(function (tool) {
      var options = toolOptions(tool);              // from webmcp-core.js: name/description/inputSchema/outputSchema/execute
      if (MUTATING.test(tool.name)) { options.execute = withProgress(options.execute); }
      document.modelContext.registerTool(options, { signal: abort.signal });
    });
  }
  function registerPageLocal() {
    document.modelContext.registerTool({ name: 'dev_compile_block', description: 'Compile a Rust block in the browser. Not available in this build yet.',
      inputSchema: { type: 'object', properties: { name: { type: 'string' } }, required: ['name'] },
      execute: async function () { return { isError: true, content: [{ type: 'text', text: 'dev_compile_block is not available in this build.' }] }; } }, { signal: abort.signal });
    document.modelContext.registerTool({ name: 'dev_export', description: 'Export the site as a runnable static bundle. Not available in this build yet.',
      inputSchema: { type: 'object', properties: {} },
      execute: async function () { return { isError: true, content: [{ type: 'text', text: 'dev_export is not available in this build.' }] }; } }, { signal: abort.signal });
  }
  if ('modelContext' in document && typeof document.modelContext.registerTool === 'function') {
    api.get('/b/dev/api/tools.json').then(function (r) { return r.json(); }).then(function (m) { registerFromManifest(m); registerPageLocal(); });
  }
  window.addEventListener('pagehide', function () { abort.abort(); });

  // ---- editor -----------------------------------------------------------
  var current = null;   // { path, sha256 } of the file in the textarea
  var list = document.getElementById('dev-file-list');
  var text = document.getElementById('dev-editor-text');
  var title = document.getElementById('dev-editor-title');

  async function loadFiles() {
    var files = (await (await api.get('/b/dev/api/files')).json()).files;
    list.innerHTML = '';
    files.forEach(function (f) {
      var li = document.createElement('li');
      var a = document.createElement('a'); a.href = '#'; a.textContent = f.path;
      a.addEventListener('click', function (ev) { ev.preventDefault(); openFile(f.path); });
      li.appendChild(a); list.appendChild(li);
    });
  }
  async function openFile(path) {
    var r = await (await api.post('/b/dev/api/files/read', { path: path })).json();
    current = { path: r.path, sha256: r.sha256 };
    title.textContent = r.path;
    text.value = r.encoding === 'utf8' ? r.content : '(binary file, ' + r.size + ' bytes)';
    text.disabled = r.encoding !== 'utf8';
  }
  var save = withProgress(async function () {
    if (!current) return;
    var r = await api.post('/b/dev/api/files/write', { path: current.path, content: text.value, expected_sha256: current.sha256 });
    if (r.status === 409) { var c = await r.json(); alert('Changed elsewhere (now ' + c.current_sha256 + '). Reopen the file.'); return; }
    current.sha256 = (await r.json()).sha256;
  });
  var remove = withProgress(async function () {
    if (!current || !confirm('Delete ' + current.path + '?')) return;
    await api.post('/b/dev/api/files/delete', { path: current.path, expected_sha256: current.sha256 });
    current = null; title.textContent = 'Editor'; text.value = '';
  });
  var create = withProgress(async function () {
    var path = prompt('New file path (site/... or blocks/<name>/...)');
    if (!path) return;
    await api.post('/b/dev/api/files/write', { path: path, content: '', expected_sha256: null });
    await openFile(path);
  });
  document.getElementById('dev-save').addEventListener('click', function () { save(); });
  document.getElementById('dev-delete').addEventListener('click', function () { remove(); });
  document.getElementById('dev-new-file').addEventListener('click', function () { create(); });
  document.getElementById('dev-refresh-tools').addEventListener('click', function () { window.__impresspressWebmcp.refresh(); });

  loadFiles();
  api.get('/b/dev/api/status').then(function (r) { return r.json(); }).then(function (s) { renderStatus(s); lastRuntimeGeneration = s.runtime_generation; });
})();
```

If the browser's `registerTool` does not accept an options bag with `signal`, keep the names in an array and call `unregisterTool` on abort (the polyfill supports both). The editor half is ordinary DOM code; write it fully — the test in Step 1 checks the markers, the e2e in Task 6 drives it. `dev.css`: a two-column grid (`files | editor`, `preview` full width, `progress | actions`), `textarea` monospace 100%/40vh, iframe 100%/60vh, pane borders using the admin theme variables already present in the shared stylesheet (`ui/assets/app.css` — reuse its custom properties rather than new colours).

- [ ] **Step 5: Serve the assets**

Add `assets.rs` under `blocks/dev` with `include_str!("assets/dev.js")` (concatenated after `ui::assets::webmcp_core_js()`) and `dev.css`; routes `GET /b/dev/static/dev.js` and `/dev.css` (Admin — the page is Admin-only anyway) with `Cache-Control: no-cache`; declare them as endpoints without schemas.

- [ ] **Step 6: Run**

Run: `cargo test -p impresspress-core --features block-dev`
Expected: PASS, snapshot updated for the three new page routes (no schemas — read the diff).

- [ ] **Step 7: Commit**

```bash
git add crates/impresspress-core
git commit -m "feat(dev): the /b/dev workspace page with page-scoped tool registration and live preview"
```

---

### Task 4: The welcome starter site and `examples/dev-sandbox/`

**Files:**
- Create: `examples/dev-sandbox/impresspress.toml`, `examples/dev-sandbox/README.md`, `examples/dev-sandbox/build.sh`
- Create: `examples/dev-sandbox/seed/manifest.json`, `examples/dev-sandbox/seed/site/index.html`, `examples/dev-sandbox/seed/site/styles.css`
- Modify: `crates/impresspress/src/cli/helpers/overlays.rs` (directory overlays)
- Delete: the Plan 1 placeholder seed fixture directory under `crates/impresspress-web/tests/e2e/fixtures/` (single source: `dev-foundations.spec.ts` now reads `examples/dev-sandbox/seed/manifest.json` directly, per the task-4 controller ruling — no symlink/copy)

**Interfaces:**
- Produces: `examples/dev-sandbox/build.sh` that emits `examples/dev-sandbox/dist/` — the sealed web bundle with `dev_enabled: true`, `/seed/` + `/__impresspress_dev/compiler/` bypassed, the seed overlaid — from a feature-on `pkg-dev`. `impresspress.toml`:

```toml
[app]
name = "dev-sandbox"
title = "ImpressPress dev sandbox"
boot_redirect = "/"

[assets]
extra_bypass_prefix = ["/seed/", "/__impresspress_dev/compiler/"]
opfs_wipe_on_recovery = false
overlay = [{ from = "seed", to = "seed" }]

[dev]
enabled = true
```
- Produces: the welcome site — `index.html` explains the sandbox, shows the credentials, links **Open workspace → `/b/auth/login?redirect=/b/dev`**, and carries the seed hashes in `manifest.json` (`schema_version: 1`, two site files, no blocks).

- [ ] **Step 1: Directory overlays** — in `apply_overlays`, if `src.is_dir()` copy recursively (a small `copy_dir_all`), else `fs::copy`. Test in `crates/impresspress/src/cli/helpers/overlays.rs`'s `mod tests`: a temp dir with `seed/a/b.txt` overlays to `dist/seed/a/b.txt`.

- [ ] **Step 2: The seed** — write `index.html` (plain HTML, inline CSS link to `styles.css`, no scripts required) and generate `manifest.json` with a `build.sh` step: `sha256sum` of each site file → entries `{ "path", "sha256", "size", "content_type" }`. Keep the manifest checked in and add a CI check (`build.sh --check`) that fails when the hashes drift from the files.

- [ ] **Step 3: `build.sh`**

```bash
#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; REPO="$(cd "$HERE/../.." && pwd)"
(cd "$REPO/crates/impresspress-web" && wasm-pack build --target web --release --out-dir pkg-dev -- --features browser-devtools)
cd "$HERE"
IMPRESSPRESS_WEB_WASM="$REPO/crates/impresspress-web/pkg-dev/impresspress_web_bg.wasm" \
IMPRESSPRESS_WEB_JS="$REPO/crates/impresspress-web/pkg-dev/impresspress_web.js" \
impresspress build --target web --release
echo "dist/ ready: $(du -sh dist | cut -f1)"
```

`impresspress build` in a directory with `impresspress.toml` but no `Cargo.toml` takes the sealed path (`mode.rs:45`), which is exactly what we want. `README.md` documents the build, the local serve (`python3 -m http.server 8080 -d dist`), the credentials, and that every visitor gets their own instance.

- [ ] **Step 4: Run** — `examples/dev-sandbox/build.sh && python3 -m http.server 8080 -d examples/dev-sandbox/dist` then open `/`: the welcome page shows (generation 0 seeded), `/b/auth/login?redirect=/b/dev` → `/b/dev` renders. Add a `build.sh --check` invocation to CI's `e2e-dev-sandbox` job and repoint that job at this example (replacing the Plan 1 scratch recipe).

- [ ] **Step 5: Commit**

```bash
git add examples/dev-sandbox crates/impresspress/src/cli/helpers/overlays.rs crates/impresspress-web/tests .github/workflows
git commit -m "feat(examples): dev-sandbox bundle with the welcome starter site"
```

---

### Task 5: Deploying dev.impresspress.org

**Files:**
- Create: `examples/dev-sandbox/wrangler.toml`
- Create: `.github/workflows/deploy-dev-sandbox.yml`
- Modify: `examples/dev-sandbox/README.md`

**Interfaces:**
- Produces: a Cloudflare Worker `impresspress-dev-sandbox` serving `dist/` as static assets with SPA fallback on the custom domain `dev.impresspress.org`.

- [ ] **Step 1: `wrangler.toml`**

```toml
name = "impresspress-dev-sandbox"
compatibility_date = "2026-05-01"
main = "worker.js"
routes = [{ pattern = "dev.impresspress.org", custom_domain = true }]

[assets]
directory = "./dist"
binding = "ASSETS"
not_found_handling = "single-page-application"
```

`worker.js` is a three-line pass-through (`export default { fetch: (req, env) => env.ASSETS.fetch(req) }`) so headers can be added later without moving off static assets. The 25 MiB per-file cap is respected by Plan 3's compiler packaging.

- [ ] **Step 2: Workflow** — model on `deploy-demo.yml`: on push to `main` touching `examples/dev-sandbox/**`, `crates/impresspress-web/**`, `crates/impresspress-core/**`, or `workflow_dispatch`; steps: checkout, `wasm32-unknown-unknown`, `wasm-pack` (pinned `v0.15.0`), `cargo install --path crates/impresspress --locked` (after a plain `pkg` pre-build for its `build.rs`, as `deploy-demo.yml:32-38` does), `examples/dev-sandbox/build.sh`, then `cloudflare/wrangler-action@v3` with `workingDirectory: examples/dev-sandbox`, `command: deploy`, secrets `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`.

- [ ] **Step 3: Document** the one-time setup (zone, custom domain, the two secrets) in the README and record the live URL.

- [ ] **Step 4: Commit**

```bash
git add examples/dev-sandbox .github/workflows/deploy-dev-sandbox.yml
git commit -m "ci: deploy examples/dev-sandbox to dev.impresspress.org"
```

---

### Task 6: Checkpoint — an agent builds the site and stocks the shop; a shopper browses

**Files:**
- Create: `crates/impresspress-web/tests/e2e/dev-workspace.spec.ts`
- Modify: `.github/workflows/ci.yml` (`e2e-dev-sandbox` runs both dev specs against `examples/dev-sandbox/dist`)

**Interfaces:**
- Consumes: the polyfill (`fixtures/model-context-polyfill.ts`), `registeredTools`/`execute` helpers from `webmcp.spec.ts` (move them into `fixtures/webmcp-helpers.ts`), the product + offer payload `global-setup.ts` seeds (lift it into `fixtures/shop-fixture.ts` as `SHOP_PRODUCT`, `SHOP_OFFER`).

- [ ] **Step 1: Write the spec**

```ts
const DEV_TOOLS = ['dev_status','dev_list_files','dev_read_file','dev_write_file','dev_delete_file','dev_list_generations','dev_get_generation','dev_rollback','dev_remove_block','dev_compile_block','dev_export'];
const SHOP_TOOLS = ['shop_list_products','shop_create_product','shop_update_product','shop_delete_product','shop_restore_product','shop_list_groups','shop_create_group','shop_list_offers','shop_create_offer','shop_update_offer','shop_publish_offer','shop_archive_offer'];

const SHOP_PAGE = `<!doctype html><html><head><meta charset="utf-8"><title>Ceramics</title>
<script src="/b/products/storefront.js" defer></script></head>
<body><h1>Ceramics</h1><ul id="products"></ul>
<script>
fetch('/b/products/catalog').then(r => r.json()).then(page => {
  for (const p of page.items) {   // use the list field name `ProductListResponse`/the catalog view actually publishes (read products/contracts.rs) — the test must not guess
    const li = document.createElement('li');
    li.innerHTML = '<h2>' + p.name + '</h2><impresspress-product product-id="' + p.id + '"></impresspress-product>';
    document.getElementById('products').appendChild(li);
  }
});
</script></body></html>`;

test('an agent builds the shop on /b/dev and a shopper sees it at /', async ({ browser, page }) => {
  await page.addInitScript(MODEL_CONTEXT_POLYFILL);
  await page.goto('/');
  await page.waitForFunction(() => navigator.serviceWorker.controller !== null);
  await expect(page.locator('body')).toContainText('admin@example.com');          // the welcome page
  await page.getByRole('link', { name: /open workspace/i }).click();
  await page.locator('input#email').fill('admin@example.com');
  await page.locator('input#password').fill('admin123');
  await page.getByRole('button', { name: /sign in/i }).click();
  await page.waitForURL(/\/b\/dev$/);

  const tools = (await registeredTools(page, DEV_TOOLS.length + SHOP_TOOLS.length)).map((t) => t.name).sort();
  const globalNames = tools.filter((n) => !n.startsWith('dev_') && !n.startsWith('shop_'));
  expect(tools.filter((n) => n.startsWith('dev_') || n.startsWith('shop_')).sort()).toEqual([...DEV_TOOLS, ...SHOP_TOOLS].sort());
  expect(globalNames).toContain('list_products');                                  // webmcp.js still registers the global set here

  const status = await execute(page, 'dev_status', {});
  expect(status.structuredContent.active_generation.cause).toBe('seed');

  const wrote = await execute(page, 'dev_write_file', { path: 'site/index.html', content: SHOP_PAGE, expected_sha256: (await execute(page, 'dev_read_file', { path: 'site/index.html' })).structuredContent.sha256 });
  expect(wrote.structuredContent.generation.cause).toBe('site_write');
  await expect(page.frameLocator('#dev-preview-frame').locator('h1')).toHaveText('Ceramics');

  const product = await execute(page, 'shop_create_product', SHOP_PRODUCT);
  expect(product.isError).toBeUndefined();
  const id = product.structuredContent.id;
  const offer = await execute(page, 'shop_create_offer', { product_id: id, ...SHOP_OFFER });
  await execute(page, 'shop_publish_offer', { product_id: id, offer_id: offer.structuredContent.id });
  const published = await execute(page, 'shop_update_product', { id, status: 'active' });
  expect(published.structuredContent.status).toBe('active');
  await expect(page.locator('#dev-progress-steps')).toContainText('active');

  const shopper = await browser.newContext();                                      // anonymous
  const shop = await shopper.newPage();
  await shop.addInitScript(MODEL_CONTEXT_POLYFILL);
  await shop.goto('/');
  await shop.waitForFunction(() => navigator.serviceWorker.controller !== null);
  await expect(shop.locator('h2')).toHaveText(SHOP_PRODUCT.name);
  await expect(shop.locator('impresspress-product')).toBeVisible();
  const shopperTools = (await registeredTools(shop, 1)).map((t) => t.name);
  expect(shopperTools).toContain('list_products');
  expect(shopperTools.some((n) => n.startsWith('dev_') || n.startsWith('shop_'))).toBe(false);
  await shopper.close();
});
```

`registeredTools(page, n)` waits until at least `n` tools exist; the dev page registers global + dev + shop, so wait for the dev/shop count and then read all. The second context shares the origin's OPFS/service worker in Chromium (same profile) — that is what makes the shopper see the admin's products; assert it explicitly with a comment.

- [ ] **Step 2: CI** — the `e2e-dev-sandbox` job builds `examples/dev-sandbox` and runs `tests/e2e/dev-foundations.spec.ts tests/e2e/dev-workspace.spec.ts` with `TEST_PORT=8082`.

- [ ] **Step 3: Run** locally against `python3 -m http.server 8082 -d examples/dev-sandbox/dist`.
Expected: PASS.

- [ ] **Step 4: Commit and PR**

```bash
git add crates/impresspress-web/tests .github/workflows/ci.yml .github/workflows/ci-main.yml
git commit -m "test(e2e): an agent builds the shop on /b/dev; a shopper browses it"
git push && gh pr create --title "Dev sandbox workspace: /b/dev, page-scoped dev_*/shop_* tools, starter site, dev.impresspress.org" --body-file - <<'EOF'
Plan 2 of the dev.impresspress.org sandbox (spec: docs/superpowers/specs/2026-09-02-dev-sandbox-design.md §4, §9, §15).

- `webmcp.js` waits for the service worker on a cold visit and exposes `refresh()`.
- `/b/dev/api/tools.json`: 21 tools projected from typed contracts with `generate_webmcp_selected`; zero refusals is a test.
- `/b/dev`: editor, live-site iframe, progress panel, agent guide; `dev.js` registers tools only there and refreshes the global set when the runtime generation changes.
- `examples/dev-sandbox/`: welcome starter seed, sealed bundle build, wrangler deploy to dev.impresspress.org.
- e2e: polyfilled agent writes the shop page, creates/publishes a product; an anonymous context sees it.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
```

## Self-review notes

- Spec coverage: §4.1 (Task 4), §4.2 (Task 3), §4.3 progress channel 1 (Task 3 `withProgress`), §9.1–9.4 (Tasks 2–3; `dev_create_block`/`dev_read_reference` come with Plan 3, `dev_compile_block`/`dev_export` are stubs until Plans 3/4), §15 (Tasks 4–5), §16 scenario 1, 3, 4, 5 (Task 6), §20.3 CSP/COOP/COEP (Task 3), §20.5 (Task 2).
- Names other plans rely on: `blocks::dev::tools::{SELECTIONS, handle}`, `blocks::dev::page::workspace`, `blocks::dev::assets::{dev_js, dev_css}`, `ui::assets::webmcp_core_js`, `window.__impresspressWebmcp.refresh()`, DOM ids `dev-*`, `examples/dev-sandbox/{build.sh, seed/, wrangler.toml}`, e2e fixtures `model-context-polyfill.ts`, `webmcp-helpers.ts`, `shop-fixture.ts`.
- Amendment to record in the spec when Task 3 lands: #7 — the preview iframe is same-origin (`allow-same-origin`) so the framed site's API calls work; isolation is "no tool registered inside the frame, nothing exposed on the parent window".
