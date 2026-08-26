# WebMCP Surfacing (impresspress) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a running impresspress site agent-operable — serve an auth-filtered WebMCP tool manifest, register those tools in the browser, curate the storefront tool surface, and show in the inspector exactly what an agent sees at each auth level.

**Architecture:** The pipeline serves `GET /b/webmcp/manifest.json`, filtered to the caller's `AuthLevel`, exactly as it already serves `/openapi.json` and `/.well-known/agent.json`. A small static script fetches that manifest and calls `document.modelContext.registerTool()` for each tool, building the HTTP request from the `invocation` metadata. Pages get one `<script src>` tag.

**Tech Stack:** Rust, vanilla JS (no framework — this ships to every page), WebMCP (`document.modelContext`).

**Spec:** `docs/superpowers/specs/2026-08-26-webmcp-design.md`

**Depends on:** Plan 1 (`agent_tool` metadata and `generate_webmcp()` in wafer-run) must be complete. Plan 2 is **not** a hard dependency — this plan works against whatever schemas exist — but the tools are only trustworthy once Plan 2 has made the schemas derived, so land Plan 2 first in practice.

## Design revision from the spec

The spec says to inline the session-filtered manifest into the page rather than serve it from an endpoint. **Implement the endpoint instead.** The reason is structural: `block_infos` is a parameter of `pipeline::handle_request` and is not reachable inside a block's page render, so inlining would mean threading it through `SiteConfig` and every page-rendering call site — a wide change for a saving of one small request.

Serving it mirrors `/openapi.json`, which already works this way at `pipeline.rs:126`, and needs no change to `page()`'s signature. The manifest stays uncacheable and per-session either way, so nothing is lost but a round trip.

## Global Constraints

- **Auth filtering is server-side, always.** The manifest endpoint emits only tools the caller may invoke. Never emit everything and filter in JS — a tool name an agent cannot use is recon surface.
- **`Cache-Control: no-store` on the manifest.** It is per-session by construction; a shared cache serving one user's manifest to another would leak the tool surface.
- **No tool completes a payment.** `start_checkout` returns a Stripe-hosted URL for a human to confirm. This is a property to preserve, not add.
- **The registration script ships to every page.** Keep it small and dependency-free, and make it fail silently on browsers without `document.modelContext`.
- Local `cargo test` failures in unrelated crates may be `[patch]` artifacts; check CI before attributing them to this work.

---

### Task 1: Serve the auth-filtered manifest

**Files:**
- Modify: `crates/impresspress-core/src/pipeline.rs` (the discovery block at lines 113-147)
- Test: `crates/impresspress-core/src/pipeline.rs`, in the existing `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `wafer_core::discovery::generate_webmcp(blocks, caller)` from Plan 1
- Produces: `GET /b/webmcp/manifest.json` returning the manifest for the caller's auth level

- [ ] **Step 1: Write the failing tests**

Add to `pipeline.rs`'s test module, beside the existing discovery tests:

```rust
#[tokio::test]
async fn webmcp_manifest_is_served_and_versioned() {
    let ctx = TestContext::new().await;
    let body = discovery_json(&ctx, "/b/webmcp/manifest.json", "impresspress.example.com").await;

    assert_eq!(body["schema_version"], serde_json::json!(1));
    assert!(
        body["tools"].is_array(),
        "manifest must carry a tools array: {body}"
    );
}

#[tokio::test]
async fn webmcp_manifest_for_anonymous_caller_contains_no_privileged_tools() {
    let ctx = TestContext::new().await;
    let body = discovery_json(&ctx, "/b/webmcp/manifest.json", "impresspress.example.com").await;

    // An unauthenticated request must see Public tools only. Anything
    // requiring a session is recon surface if its name is published here.
    let rendered = body.to_string();
    for forbidden in ["list_users", "list_my_purchases"] {
        assert!(
            !rendered.contains(forbidden),
            "anonymous manifest must not name the privileged tool {forbidden}: {rendered}"
        );
    }
}

#[tokio::test]
async fn webmcp_manifest_is_not_cacheable() {
    let ctx = TestContext::new().await;
    let headers = discovery_headers(&ctx, "/b/webmcp/manifest.json", "impresspress.example.com").await;

    let cache_control = headers
        .get("Cache-Control")
        .map(String::as_str)
        .unwrap_or_default();
    assert!(
        cache_control.contains("no-store"),
        "the manifest is per-session and must not be cached, got: {cache_control:?}"
    );
}
```

`discovery_headers` does not exist yet. Add it beside `discovery_json` in `test_support.rs`, returning the response headers as a `HashMap<String, String>` — model it on `discovery_json`, which already builds the request and calls `handle_request`; return the header map instead of parsing the body.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p impresspress-core webmcp_manifest`
Expected: FAIL — the path is unrouted, so the response is a 404 rather than a manifest.

- [ ] **Step 3: Add the auth-level helper**

Add above `handle_request` in `pipeline.rs`:

```rust
/// The `AuthLevel` ceiling for this request, used to filter the WebMCP tool
/// manifest.
///
/// Admin is decided by the SAME merged role resolution every other admin
/// check uses — `get_user_roles` merges the inline `users.role` column with
/// `USER_ROLES_TABLE` rows (see `blocks/auth/service.rs` around line 403).
/// Introducing a second notion of "is this caller an admin" is how the two
/// drift apart.
///
/// Fails closed: any resolution error yields `Public`. Under-reporting hides
/// tools a caller could have used, which is a UX problem; over-reporting
/// publishes tool names to someone who cannot invoke them, which is the
/// SEC-073 recon problem.
async fn caller_auth_level(ctx: &dyn Context, msg: &Message) -> AuthLevel {
    let user_id = msg.user_id();
    if user_id.is_empty() {
        return AuthLevel::Public;
    }

    match crate::blocks::auth::helpers::get_user_roles(ctx, user_id).await {
        Ok(roles) if roles.iter().any(|r| r == "admin") => AuthLevel::Admin,
        Ok(_) => AuthLevel::Authenticated,
        Err(_) => AuthLevel::Public,
    }
}
```

- [ ] **Step 4: Serve the manifest — placement is load-bearing**

**Do not put this in the step-0 discovery block at line 113.** `msg.user_id()` is not populated until **step 2** (`pipeline.rs:155-169`), where `extract_auth_meta` / `authenticate_api_key` set the auth metadata. A manifest branch placed at line 113 would see an empty `user_id` for every request, so every caller — including admins — would silently receive the anonymous manifest. The bug is invisible in a smoke test, because the anonymous manifest is a valid document.

Insert **after** the step-2 auth block (after line 170, where `let user_id = msg.user_id().to_string();` proves identity is now resolved) and **before** the step-2a CSRF check:

```rust
    // WebMCP tool manifest. Placed after step 2 because it needs the resolved
    // identity — the discovery documents at step 0 are anonymous by design,
    // this one is not.
    if path == "/b/webmcp/manifest.json" {
        let caller = caller_auth_level(ctx, &msg).await;
        let body = wafer_core::discovery::generate_webmcp(block_infos, caller);

        // Per-session by construction: a shared cache serving one visitor's
        // manifest to another would leak the privileged tool surface.
        return ResponseBuilder::new()
            .set_header("Cache-Control", "no-store")
            .json(&body);
    }
```

Note `path` is already bound at line 174 from `msg.path()`, so no extra binding is needed.

This deliberately does **not** get the `Access-Control-Allow-Origin: *` treatment the step-0 discovery documents receive in development. The manifest is consumed same-origin by the page's own script; there is no reason to advertise it cross-origin.

- [ ] **Step 5: Add an authenticated-caller test to prove the placement is right**

The three tests in Step 1 all pass even with the branch wrongly placed at step 0, because they are anonymous. Add one that fails in that case:

```rust
#[tokio::test]
async fn webmcp_manifest_reflects_an_authenticated_caller() {
    let ctx = TestContext::new().await;

    // Build a request carrying a valid session for a non-admin user, using
    // whatever helper the existing auth tests in this module use to mint one.
    let body = discovery_json_as_user(&ctx, "/b/webmcp/manifest.json", "impresspress.example.com").await;

    let names: Vec<&str> = body["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();

    assert!(
        names.contains(&"list_my_purchases"),
        "an authenticated caller must receive Authenticated-level tools — if this \
         fails with only Public tools present, the manifest branch is running \
         before auth meta is set (step 0 instead of after step 2): {names:?}"
    );
}
```

`discovery_json_as_user` needs adding to `test_support.rs` beside `discovery_json`, differing only in that it attaches a session for a seeded non-admin user. Follow how the existing authenticated tests in `pipeline.rs` construct one.

This test depends on `list_my_purchases` existing, which Task 3 adds. Until then, assert against any `AuthLevel::Authenticated` tool the build actually has, or mark it `#[ignore]` with a note and enable it in Task 3.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p impresspress-core webmcp_manifest`
Expected: PASS, all three.

- [ ] **Step 7: Verify the route is publicly reachable but auth-aware**

Run: `cargo test -p impresspress-core`
Expected: PASS. In particular the CSRF and routing tests must still pass — the new path is a GET and should not require a session, but must still resolve one when present.

- [ ] **Step 8: Commit**

```bash
git add crates/impresspress-core/src/pipeline.rs crates/impresspress-core/src/test_support.rs
git commit -m "feat: serve auth-filtered WebMCP manifest at /b/webmcp/manifest.json

Third discovery document beside /openapi.json and the agent card, but
filtered to the caller's AuthLevel and marked no-store — the tool surface
is per-session, and a privileged tool name is recon surface if published
to someone who cannot invoke it."
```

---

### Task 2: Register the tools in the browser

**Files:**
- Create: `crates/impresspress-core/src/ui/assets/webmcp.js`
- Modify: `crates/impresspress-core/src/ui/assets.rs`
- Modify: `crates/impresspress-core/src/blocks/system.rs` (serve the new asset)
- Modify: `crates/impresspress-core/src/ui/layout.rs:47`
- Modify: `crates/impresspress-core/src/ui/templates.rs:449`

**Interfaces:**
- Consumes: `GET /b/webmcp/manifest.json` from Task 1
- Produces: `assets::webmcp_js()` and `assets::webmcp_js_url()`, and a `<script>` tag on every rendered page

- [ ] **Step 1: Write the registration script**

Create `crates/impresspress-core/src/ui/assets/webmcp.js`:

```js
// Registers this site's WebMCP tools with the browser agent.
//
// The manifest is generated server-side from each block's endpoint
// declarations and filtered to this session's auth level, so whatever
// arrives here is exactly what this visitor is allowed to invoke. The
// script's only job is translating that into registerTool calls.
(function () {
  'use strict';

  // Browsers without WebMCP get nothing. This ships on every page, so it
  // must never throw on an unsupported browser.
  if (!('modelContext' in document) || typeof document.modelContext.registerTool !== 'function') {
    return;
  }

  // Substitute {name} path segments, and collect the rest into query or body
  // according to the provenance the server recorded.
  function buildRequest(invocation, args) {
    var path = invocation.path;
    (invocation.path_params || []).forEach(function (name) {
      path = path.replace('{' + name + '}', encodeURIComponent(args[name]));
    });

    var query = new URLSearchParams();
    (invocation.query_params || []).forEach(function (name) {
      if (args[name] !== undefined && args[name] !== null) {
        query.append(name, args[name]);
      }
    });
    var qs = query.toString();
    if (qs) {
      path += '?' + qs;
    }

    var init = { method: invocation.method.toUpperCase(), headers: {} };

    var bodyNames = invocation.body_params || [];
    if (bodyNames.length > 0) {
      var body = {};
      bodyNames.forEach(function (name) {
        if (args[name] !== undefined) {
          body[name] = args[name];
        }
      });
      init.headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(body);
    }

    // Same-origin, so the session cookie rides along and the server applies
    // the same authorization it would to any other request. The manifest
    // filter is a UX affordance; the endpoint is still the real gate.
    init.credentials = 'same-origin';

    return { url: path, init: init };
  }

  function register(tool) {
    document.modelContext.registerTool({
      name: tool.name,
      description: tool.description,
      inputSchema: tool.inputSchema,
      execute: async function (args) {
        var req = buildRequest(tool.invocation, args || {});
        var response = await fetch(req.url, req.init);
        var text = await response.text();

        if (!response.ok) {
          return {
            content: [{
              type: 'text',
              text: 'Request failed (' + response.status + '): ' + text
            }]
          };
        }

        return { content: [{ type: 'text', text: text }] };
      }
    });
  }

  fetch('/b/webmcp/manifest.json', { credentials: 'same-origin' })
    .then(function (r) { return r.ok ? r.json() : null; })
    .then(function (manifest) {
      if (!manifest || !Array.isArray(manifest.tools)) {
        return;
      }
      manifest.tools.forEach(register);
    })
    .catch(function () {
      // A failed manifest fetch means no tools. That is a degraded page,
      // not a broken one — never surface it to the visitor.
    });
})();
```

- [ ] **Step 2: Add the asset accessors**

In `crates/impresspress-core/src/ui/assets.rs`, follow the existing htmx pattern exactly (`htmx_js()` at line 138 and `htmx_js_url()` at line 156):

```rust
/// WebMCP tool-registration script, served on every page.
pub fn webmcp_js() -> &'static str {
    include_str!("assets/webmcp.js")
}

/// WebMCP script URL with content hash, e.g. `/b/static/webmcp-a1b2c3d4.js`
pub fn webmcp_js_url() -> &'static str {
    // Mirror htmx_js_url's construction — same OnceLock + hash helper, same
    // STATIC_PREFIX. Read lines 150-165 and follow that shape.
}
```

- [ ] **Step 3: Serve it**

`crates/impresspress-core/src/blocks/system.rs` serves everything under `STATIC_PREFIX`. Find where it matches the hashed htmx URL and add a matching arm for the webmcp script, returning `assets::webmcp_js()` with `Content-Type: application/javascript` and the same long-lived cache headers the other hashed assets use. The content hash in the URL makes immutable caching safe.

- [ ] **Step 4: Inject the tag on every page**

In `crates/impresspress-core/src/ui/layout.rs`, inside the `body` block, immediately after the existing `script { (PreEscaped(assets::modal_js())) }` line (around line 46):

```rust
                script src=(assets::webmcp_js_url()) defer {}
```

Then make the identical addition in `crates/impresspress-core/src/ui/templates.rs` at the equivalent point near line 449, where `opts.config.embedded_scripts` is rendered. **Both render paths need it** — a page rendered through `templates.rs` that lacks the tag is a page where agents silently have no tools.

- [ ] **Step 5: Write the failing test**

Add to `layout.rs`'s test module (or create one following `shell.rs`'s pattern):

```rust
#[test]
fn every_page_includes_the_webmcp_registration_script() {
    let config = SiteConfig {
        app_name: "Test".into(),
        logo_url: String::new(),
        logo_icon_url: String::new(),
        favicon_url: String::new(),
        primary_color: String::new(),
        embedded_scripts: Vec::new(),
    };
    let rendered = page("Title", &config, maud::html! { p { "body" } }).into_string();
    assert!(
        rendered.contains(assets::webmcp_js_url()),
        "the WebMCP script must be on every page: {rendered}"
    );
}
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p impresspress-core webmcp`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/impresspress-core/src/ui/ crates/impresspress-core/src/blocks/system.rs
git commit -m "feat(ui): register WebMCP tools from the served manifest

Dependency-free script on every page: fetches the auth-filtered manifest
and translates each tool into a registerTool call, rebuilding the HTTP
request from the invocation provenance. No-ops on browsers without
document.modelContext."
```

---

### Task 3: Curate the storefront tool surface

**Files:**
- Modify: `crates/impresspress-core/src/blocks/products/mod.rs` (the storefront endpoint declarations, lines 1786-1950)
- Test: `crates/impresspress-core/src/blocks/products/tests/`

**Interfaces:**
- Consumes: `.agent_tool(name, description)` from Plan 1
- Produces: five annotated storefront endpoints, the demonstrable agent-shoppable surface

**Why these five:** they are already `AuthLevel::Public`, already fully schema'd, and form a complete purchase path — and `checkout` returns a Stripe-hosted URL, so the agent structurally cannot complete a payment.

- [ ] **Step 1: Write the failing test**

Create or extend a test in the products block:

```rust
#[test]
fn storefront_endpoints_are_exposed_as_curated_agent_tools() {
    let info = ProductsBlock::default().info();
    let named: std::collections::HashMap<&str, &str> = info
        .endpoints
        .iter()
        .filter_map(|ep| ep.agent_tool.as_ref().map(|t| (t.name.as_str(), ep.path.as_str())))
        .collect();

    assert_eq!(named.get("search_products"), Some(&"/b/products/storefront"));
    assert_eq!(
        named.get("get_product"),
        Some(&"/b/products/storefront/{product_id}")
    );
    assert_eq!(
        named.get("preview_price"),
        Some(&"/b/products/pricing/preview")
    );
    assert_eq!(named.get("start_checkout"), Some(&"/b/products/checkout"));
    assert_eq!(
        named.get("get_order_status"),
        Some(&"/b/products/orders/{id}/status")
    );
}

#[test]
fn stripe_webhook_is_never_an_agent_tool() {
    let info = ProductsBlock::default().info();
    let webhook = info
        .endpoints
        .iter()
        .find(|ep| ep.path == "/b/products/webhooks")
        .expect("webhook endpoint exists");
    assert!(
        !webhook.is_agent_tool(),
        "the Stripe webhook is a machine-to-machine transport endpoint \
         authenticated by HMAC — never an agent tool"
    );
}
```

Match `ProductsBlock::default().info()` to how the block is actually constructed in this block's existing tests; read them first.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p impresspress-core storefront_endpoints_are_exposed`
Expected: FAIL — no endpoint carries `agent_tool` yet.

- [ ] **Step 3: Annotate the five endpoints**

In `products/mod.rs`, add `.agent_tool(...)` to each. Descriptions are written for an agent deciding whether to call, not for a developer reading docs:

```rust
BlockEndpoint::get("/b/products/storefront/{product_id}")
    // ... existing summary/auth/schemas unchanged ...
    .agent_tool(
        "get_product",
        "Get one product's full details and its purchasable offers, including \
         pricing inputs. Call this before previewing a price or starting checkout.",
    ),
```

```rust
BlockEndpoint::post("/b/products/pricing/preview")
    // ...
    .agent_tool(
        "preview_price",
        "Calculate the exact total for an offer given the customer's chosen \
         options, before any payment. Returns amounts in integer minor units. \
         Use this to answer 'how much would X cost' without starting checkout.",
    ),
```

```rust
BlockEndpoint::post("/b/products/checkout")
    // ...
    .agent_tool(
        "start_checkout",
        "Begin a purchase and return a Stripe checkout URL for the customer to \
         complete. This does NOT complete the payment — always give the returned \
         URL to the customer so they can confirm and pay themselves.",
    ),
```

```rust
BlockEndpoint::get("/b/products/orders/{id}/status")
    // ...
    .agent_tool(
        "get_order_status",
        "Check whether an order has been paid, using the receipt token issued \
         when checkout started. Use this after the customer says they have paid.",
    ),
```

Add `search_products` to whichever endpoint lists storefront products. If no public list endpoint exists, **do not invent one in this plan** — drop `search_products` from the test in Step 1 and note the gap, since adding a new endpoint is a scope change rather than an annotation.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p impresspress-core storefront`
Expected: PASS.

- [ ] **Step 5: Verify the anonymous manifest now carries exactly these tools**

Add to `pipeline.rs`'s tests:

```rust
#[tokio::test]
async fn anonymous_manifest_exposes_the_storefront_purchase_path() {
    let ctx = TestContext::new().await;
    let body = discovery_json(&ctx, "/b/webmcp/manifest.json", "impresspress.example.com").await;

    let names: Vec<&str> = body["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();

    for expected in ["get_product", "preview_price", "start_checkout", "get_order_status"] {
        assert!(
            names.contains(&expected),
            "anonymous visitors must get the public purchase path; missing {expected}: {names:?}"
        );
    }
}
```

Run: `cargo test -p impresspress-core anonymous_manifest`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/impresspress-core/src/blocks/products/
git commit -m "feat(products): expose the storefront purchase path as agent tools

Five curated tools covering browse, price, checkout, and order status.
start_checkout returns a Stripe-hosted URL rather than completing payment,
so a human confirms every purchase. The HMAC-authenticated Stripe webhook
is explicitly not a tool."
```

---

### Task 4: Show the agent's view in the inspector

**Files:**
- Modify: `wafer-run/crates/wafer-block-inspector/src/lib.rs` and its `inspector.html`

**Interfaces:**
- Consumes: `generate_webmcp()` (Plan 1)
- Produces: an inspector view rendering the tool manifest at each of the three auth levels

**This task is in the wafer-run repo,** like Plan 1. It ships last because it is the demonstration, not the mechanism.

- [ ] **Step 1: Add a manifest route to the inspector**

`wafer-block-inspector/src/lib.rs` (376 lines) serves a static `inspector.html` at line 184 and handles JSON sub-routes above it. Add a route — follow the existing sub-route pattern in `handle()` — that returns, for each of `Public`, `Authenticated`, and `Admin`, the output of `generate_webmcp(blocks, level)`:

```rust
{
  "public": { "schema_version": 1, "tools": [ ... ] },
  "authenticated": { "schema_version": 1, "tools": [ ... ] },
  "admin": { "schema_version": 1, "tools": [ ... ] }
}
```

The inspector already receives block info for its routes view — reuse that source rather than introducing a second one.

- [ ] **Step 2: Write the failing test**

Add to `wafer-block-inspector`'s test module:

```rust
#[tokio::test]
async fn inspector_reports_tools_per_auth_level() {
    // Construct the inspector with fixture blocks the same way the existing
    // routes-view tests do, then request the new sub-route.
    let body = inspector_json("/b/inspector/webmcp").await;

    let public = body["public"]["tools"].as_array().expect("public tools");
    let admin = body["admin"]["tools"].as_array().expect("admin tools");

    assert!(
        admin.len() >= public.len(),
        "an admin sees at least everything an anonymous visitor sees"
    );
}
```

- [ ] **Step 3: Run to verify it fails, then implement**

Run: `cargo test -p wafer-block-inspector webmcp`
Expected: FAIL, then PASS once Step 1 is implemented.

- [ ] **Step 4: Render it in `inspector.html`**

Add a panel showing the three manifests side by side, each tool as name, description, and collapsible input schema. The point it must make visible at a glance: **the same site presents a different tool surface depending on who is asking.** Plain HTML/CSS — the inspector has no framework and should not gain one.

- [ ] **Step 5: Commit**

```bash
git add crates/wafer-block-inspector/
git commit -m "feat(inspector): show the WebMCP tool manifest per auth level

Makes the auth-filtered tool surface directly inspectable: the same site
presents different tools to anonymous, authenticated, and admin callers."
```

---

### Task 5: End-to-end verification against a real agent

**Files:**
- Create: `docs/2026-08-26-webmcp-e2e-verification.md`

**Interfaces:**
- Consumes: everything above
- Produces: evidence the tools actually work in a browser agent, not just in tests

Unit tests prove the manifest is correct. They cannot prove a browser agent can use it. This task closes that gap.

- [ ] **Step 1: Run a local instance with products enabled**

```bash
just build-debug
WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_EMAIL=admin@example.com \
WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_PASSWORD=admin123 \
IMPRESSPRESS_LISTEN=127.0.0.1:8090 \
./target/debug/impresspress serve --target native --run-migrations
```

Sign in at `http://127.0.0.1:8090/b/auth/login`, enable the products block, and create at least one product with an active offer and a test-mode Stripe key.

- [ ] **Step 2: Verify the manifest by hand**

```bash
curl -s http://127.0.0.1:8090/b/webmcp/manifest.json | jq '.tools[].name'
```
Expected: the four public storefront tool names, and nothing privileged.

Then with an admin session cookie, confirm the same URL returns a strictly larger set.

- [ ] **Step 3: Verify registration in a WebMCP browser**

Open a storefront page in the ChatGPT browser or Chrome with WebMCP enabled. In the devtools console:

```js
await document.modelContext.getTools()
```
Expected: the storefront tools, each with a populated `inputSchema`.

- [ ] **Step 4: Drive the purchase path with an agent**

Ask the agent, in the browser, to find the product, price it with specific options, and start a checkout. Confirm:

- it calls `get_product` then `preview_price` before `start_checkout`
- the returned Stripe URL is handed to you rather than followed
- `get_order_status` reflects payment after you complete it yourself

- [ ] **Step 5: Record what actually happened**

Create `docs/2026-08-26-webmcp-e2e-verification.md` with the exact prompts used, which tools the agent selected in which order, anything it got wrong, and any tool description that needed rewording because the agent misread it.

**Tool descriptions are the main thing this exercise tests.** If the agent picks the wrong tool or supplies wrong arguments, that is a description bug — fix it in the `agent_tool` annotation and re-run.

- [ ] **Step 6: Commit**

```bash
git add docs/2026-08-26-webmcp-e2e-verification.md crates/impresspress-core/src/blocks/products/
git commit -m "docs: record end-to-end WebMCP agent verification"
```

---

## Done criteria

- [ ] `cargo test --workspace` passes in both repos
- [ ] `/b/webmcp/manifest.json` returns Public-only tools anonymously and a superset when admin
- [ ] The manifest carries `Cache-Control: no-store`
- [ ] `document.modelContext.getTools()` lists the storefront tools in a real WebMCP browser
- [ ] An agent completes browse → price → checkout, and the payment is confirmed by a human at Stripe
- [ ] The inspector shows all three auth-level manifests
- [ ] E2E findings recorded, with any description fixes applied

## What this plan deliberately does not do

- **No admin write tools.** Admin gets read tools at most; agent-driven site configuration is a separate decision.
- **No new endpoints.** This plan annotates and exposes what exists. If `search_products` has no backing endpoint, it is dropped rather than invented.
- **No browser-runtime work.** Running this in the fully in-browser build (`impresspress-browser`) is attractive and mostly free, but the service worker boots after first paint, so tools would need `toolchange` re-registration. That is unproven and belongs in its own plan.
