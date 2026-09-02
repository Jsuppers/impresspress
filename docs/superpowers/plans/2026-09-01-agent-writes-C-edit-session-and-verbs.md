# Plan C — Edit session and agent write verbs (impresspress) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an admin who has deliberately opened a time-boxed editing session create, edit, archive and delete products by talking to a browser agent, with nothing the agent does reaching a customer.

**Architecture:** An admin-started session grants the `products:edit` capability. The manifest route resolves held capabilities from a request header and passes them to the capability-aware projection from Plan A, so the write tools are absent until the session exists. The tools point at their own `/b/products/agent/*` routes — `AuthLevel::Admin` **and** the capability, checked independently — which always write `status = 'draft'` and never publish.

**Tech Stack:** Rust, `wafer-core::discovery::CapabilitySet` (Plan A), `webmcp.js`, Playwright.

**Spec:** `docs/superpowers/specs/2026-09-01-agent-product-writes-design.md` (§2, §4, §5)

**Depends on:** Plan A merged and pinned (Task 1 below). Plan B merged — `archive_product` and `delete_product` are meaningless without soft delete.

## Global Constraints

- **The capability never substitutes for the login.** Every agent route is `AuthLevel::Admin` *and* requires `products:edit`. Neither check may satisfy the other.
- **The server is the authority.** Manifest omission is discovery only. Every agent write re-verifies the session, so a stale tool in an agent's list writes nothing.
- **Draft means the field does not exist.** `AgentProductDraftRequest` has no `status` field, and the handler sets the column unconditionally. Never reuse `CreateProductRequest`, which has `status: Option<ProductStatus>` and defaults it with `or_insert` — reusing it would let the agent publish while every schema stayed true.
- **The gate ships off.** `enable_agent_writes()` is opt-in; not a `ConfigVar`, because impresspress#78 would pin it off on Workers.
- `///` becomes the published tool description. Use `//` for rationale.
- Every test verified load-bearing: revert the behaviour, watch it fail, restore.
- **Never regenerate an `/openapi.json` snapshot to get green.**

---

### Task 1: Bump the wafer-run pin

**Files:**
- Modify: `Cargo.toml` (the wafer-run git rev), `Cargo.lock`

**Interfaces:**
- Consumes: Plan A's merge SHA.
- Produces: `wafer_core::discovery::CapabilitySet` available in-tree.

- [ ] **Step 1: Update the rev**

Set the wafer-run `rev` in `Cargo.toml` to Plan A's merge SHA.

- [ ] **Step 2: Re-resolve the lock from outside the tree**

```bash
cd "$SCRATCHPAD"
cargo metadata --manifest-path /path/to/impresspress/Cargo.toml --format-version 1 >/dev/null
```

Cargo finds `.cargo/config.toml` by walking up from the **cwd**, so running this anywhere under `impresspress/` inherits the repo-level `[patch]` and writes path sources into the lock. This is how `e204c98` was produced on #72.

- [ ] **Step 3: Build and commit both files**

Run: `cargo check --workspace`
Expected: PASS. Existing `generate_webmcp_report` call sites now need `&CapabilitySet::none()` — Task 4 replaces that at the manifest route; anywhere else, pass `none()`.

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore(deps): bump wafer-run pin for capability-gated agent tools"
```

`Cargo.toml` changed, so the lock **must** be committed — CI's `--locked` jobs need it. This is the case where the usual "the lock rewrite is an artifact" rule does not apply.

---

### Task 2: The session record

**Files:**
- Create: `crates/impresspress-core/src/blocks/admin/migrations/00X_agent_sessions.sqlite.sql` and `.postgres.sql`
- Create: `crates/impresspress-core/src/blocks/admin/repo/agent_sessions.rs`
- Modify: `crates/impresspress-core/src/blocks/admin/repo/mod.rs`, `.../admin/migrations/mod.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub(crate) const TABLE: &str = "impresspress__admin__agent_sessions";`
  - `pub(crate) struct AgentSession { pub id: String, pub user_id: String, pub capabilities: Vec<String>, pub expires_at: String }`
  - `pub(crate) async fn issue(ctx, user_id: &str, capabilities: &[String], ttl_secs: i64) -> Result<(AgentSession, String), OutputStream>` — returns the session and the **plaintext token, once**
  - `pub(crate) async fn resolve(ctx, token: &str, user_id: &str) -> Option<AgentSession>` — `None` unless live, unexpired, unrevoked **and** owned by `user_id`
  - `pub(crate) async fn revoke(ctx, id: &str, user_id: &str) -> Result<(), OutputStream>`

- [ ] **Step 1: Write the migration**

```sql
CREATE TABLE IF NOT EXISTS impresspress__admin__agent_sessions (
    id            TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL,
    capabilities  TEXT NOT NULL DEFAULT '[]',
    token_hash    TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    expires_at    TEXT NOT NULL,
    revoked_at    TEXT
);
CREATE INDEX IF NOT EXISTS impresspress__admin__agent_sessions_user_idx
    ON impresspress__admin__agent_sessions (user_id);
CREATE UNIQUE INDEX IF NOT EXISTS impresspress__admin__agent_sessions_token_idx
    ON impresspress__admin__agent_sessions (token_hash);
```

- [ ] **Step 2: Write the failing tests**

```rust
#[tokio::test]
async fn resolve_accepts_a_live_session_for_its_owner() {
    let ctx = TestContext::new().await;
    let (session, token) = issue(ctx.ctx(), "admin-1", &["products:edit".into()], 900).await.unwrap();
    let found = resolve(ctx.ctx(), &token, "admin-1").await.expect("must resolve");
    assert_eq!(found.id, session.id);
    assert_eq!(found.capabilities, vec!["products:edit".to_string()]);
}

/// A token is bound to the admin it was issued to. Without this, a leaked
/// token would be a standalone credential rather than an extra restriction
/// on an already-authenticated admin.
#[tokio::test]
async fn resolve_refuses_a_token_presented_by_another_user() {
    let ctx = TestContext::new().await;
    let (_s, token) = issue(ctx.ctx(), "admin-1", &["products:edit".into()], 900).await.unwrap();
    assert!(resolve(ctx.ctx(), &token, "admin-2").await.is_none());
}

#[tokio::test]
async fn resolve_refuses_an_expired_session() {
    let ctx = TestContext::new().await;
    let (_s, token) = issue(ctx.ctx(), "admin-1", &["products:edit".into()], -1).await.unwrap();
    assert!(resolve(ctx.ctx(), &token, "admin-1").await.is_none());
}

#[tokio::test]
async fn resolve_refuses_a_revoked_session() {
    let ctx = TestContext::new().await;
    let (session, token) = issue(ctx.ctx(), "admin-1", &["products:edit".into()], 900).await.unwrap();
    revoke(ctx.ctx(), &session.id, "admin-1").await.unwrap();
    assert!(resolve(ctx.ctx(), &token, "admin-1").await.is_none());
}

/// The plaintext token must never be persisted.
#[tokio::test]
async fn the_plaintext_token_is_not_stored() {
    let ctx = TestContext::new().await;
    let (session, token) = issue(ctx.ctx(), "admin-1", &["products:edit".into()], 900).await.unwrap();
    let row = db::get(ctx.ctx(), TABLE, &session.id).await.unwrap();
    assert_ne!(row.str_field("token_hash"), token);
    assert!(!row.str_field("token_hash").is_empty());
}

#[tokio::test]
async fn resolve_refuses_a_token_that_was_never_issued() {
    let ctx = TestContext::new().await;
    assert!(resolve(ctx.ctx(), "not-a-token", "admin-1").await.is_none());
}
```

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test -p impresspress-core agent_sessions`
Expected: FAIL — module not found.

- [ ] **Step 4: Implement**

`issue` generates 32 random bytes via the crypto service, hex-encodes them as the token, stores `sha256(token)` as `token_hash`, and returns both. `resolve` hashes the presented token, looks the row up by `token_hash`, and returns `None` unless `user_id` matches, `revoked_at` is empty and `expires_at` is in the future. Comparison is on the hash, so it is a single indexed lookup rather than a scan.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test -p impresspress-core agent_sessions`
Expected: PASS (6 tests).

- [ ] **Step 6: Verify load-bearing**

Drop the `user_id` check in `resolve`; confirm `resolve_refuses_a_token_presented_by_another_user` fails. Restore. Drop the expiry check; confirm the expiry test fails. Restore.

- [ ] **Step 7: Commit**

```bash
git add crates/impresspress-core/src/blocks/admin/
git commit -m "feat(admin): agent edit-session records, hashed tokens bound to their admin"
```

---

### Task 3: Start and revoke a session

**Files:**
- Modify: `crates/impresspress-core/src/blocks/admin/mod.rs` (declare two endpoints)
- Create: `crates/impresspress-core/src/blocks/admin/handlers/agent_sessions.rs`
- Modify: `crates/impresspress-core/src/blocks/admin/contracts.rs`

**Interfaces:**
- Consumes: `repo::agent_sessions::{issue, revoke}`.
- Produces: `POST /b/admin/api/agent-sessions` (Admin) → `AgentSessionResponse { id, token, expires_at, capabilities }`; `DELETE /b/admin/api/agent-sessions/{id}` (Admin) → `AgentSessionRevoked { revoked: bool }`. Neither is an agent tool.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn an_admin_can_start_a_session_and_gets_the_token_once() {
    let ctx = TestContext::new().await;
    let body = json_body(handle(ctx.ctx(), &admin_msg("/b/admin/api/agent-sessions"), body_of(json!({"ttl_seconds": 900}))).await).await;
    assert!(body["token"].as_str().unwrap().len() >= 32);
    assert_eq!(body["capabilities"], json!(["products:edit"]));
}

#[tokio::test]
async fn a_non_admin_cannot_start_a_session() {
    let ctx = TestContext::new().await;
    let out = handle(ctx.ctx(), &user_msg("/b/admin/api/agent-sessions"), body_of(json!({}))).await;
    assert!(output_is_error(&out));
}

/// The session endpoints must never become tools: a tool that mints the
/// credential gating the write tools would let the agent open its own gate.
#[test]
fn the_session_endpoints_are_not_agent_tools() {
    let info: BlockInfo = AdminBlock::default().info();
    for ep in &info.endpoints {
        if ep.path.contains("agent-sessions") {
            assert!(!ep.is_agent_tool(), "{} must not be an agent tool", ep.path);
        }
    }
}

#[tokio::test]
async fn ttl_is_clamped_to_the_maximum() {
    let ctx = TestContext::new().await;
    let body = json_body(handle(ctx.ctx(), &admin_msg("/b/admin/api/agent-sessions"), body_of(json!({"ttl_seconds": 86_400}))).await).await;
    // MAX_TTL_SECONDS is 3600; a caller cannot ask for a longer grant.
    let expires: DateTime<Utc> = body["expires_at"].as_str().unwrap().parse().unwrap();
    assert!(expires <= Utc::now() + Duration::seconds(3601));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p impresspress-core agent_session`
Expected: FAIL — route not found.

- [ ] **Step 3: Implement**

```rust
// The capability set a session may grant is fixed here rather than taken
// from the request. A caller-chosen list would let an admin (or anything
// steering one) mint a grant for a capability the UI never offered.
const GRANTABLE: &[&str] = &["products:edit"];
const DEFAULT_TTL_SECONDS: i64 = 900;
const MAX_TTL_SECONDS: i64 = 3600;
```

Declare both endpoints in `admin/mod.rs` **without** `.agent_tool(...)`, with `AuthLevel::Admin`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p impresspress-core agent_session`
Expected: PASS.

- [ ] **Step 5: Snapshot**

Run: `cargo test -p impresspress openapi_snapshot`
Expected: two added paths. Read every line of the diff.

- [ ] **Step 6: Commit**

```bash
git add crates/impresspress-core/src/blocks/admin/
git commit -m "feat(admin): start and revoke an agent edit session"
```

---

### Task 4: Resolve capabilities at the manifest route

**Files:**
- Modify: `crates/impresspress-core/src/pipeline.rs:273-333` (the manifest branch)
- Modify: `crates/impresspress-core/src/builder/registration.rs` (the boot-time refusal census call)
- Test: `crates/impresspress-core/src/pipeline.rs` (inline tests, beside `webmcp_manifest_*`)

**Interfaces:**
- Consumes: `CapabilitySet` (Plan A), `repo::agent_sessions::resolve`.
- Produces: `pub(crate) const AGENT_SESSION_HEADER: &str = "x-impresspress-agent-session";`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn the_manifest_has_no_write_tools_without_a_session() {
    let ctx = TestContext::new().await;
    let body = webmcp_manifest_as_admin(&ctx, None).await;
    assert!(!tool_names(&body).contains(&"create_product_draft".to_string()));
}

#[tokio::test]
async fn the_manifest_gains_the_write_tools_with_a_live_session() {
    let ctx = TestContext::new().await;
    let (_s, token) = issue_session(&ctx, "admin-1").await;
    let body = webmcp_manifest_as_admin(&ctx, Some(&token)).await;
    assert!(tool_names(&body).contains(&"create_product_draft".to_string()));
}

/// A session token cannot lift a non-admin caller. The capability is an
/// additional restriction on an authenticated admin, never a credential.
#[tokio::test]
async fn a_session_token_does_not_give_an_anonymous_caller_write_tools() {
    let ctx = TestContext::new().await;
    let (_s, token) = issue_session(&ctx, "admin-1").await;
    let body = webmcp_manifest_anonymous(&ctx, Some(&token)).await;
    let names = tool_names(&body);
    assert!(!names.contains(&"create_product_draft".to_string()));
    assert!(!names.contains(&"list_users".to_string()));
}

#[tokio::test]
async fn an_expired_session_leaves_the_manifest_unchanged() {
    let ctx = TestContext::new().await;
    let (_s, token) = issue_expired_session(&ctx, "admin-1").await;
    let with = webmcp_manifest_as_admin(&ctx, Some(&token)).await;
    let without = webmcp_manifest_as_admin(&ctx, None).await;
    assert_eq!(with, without);
}

/// Every capability any block gates a tool on must be one a session can
/// actually grant. Otherwise a typo produces a tool that is invisible
/// forever, with nothing to tell the operator why.
#[test]
fn every_declared_capability_is_grantable() {
    for block in real_block_infos() {
        for ep in &block.endpoints {
            let Some(tool) = ep.agent_tool.as_ref() else { continue };
            let Some(cap) = tool.requires_capability.as_ref() else { continue };
            assert!(
                GRANTABLE.contains(&cap.as_str()),
                "{} gates {} on {cap}, which no session can grant",
                ep.path, tool.name
            );
        }
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p impresspress-core manifest`
Expected: FAIL — the manifest call takes three arguments.

- [ ] **Step 3: Implement**

In the manifest branch, before the `generate_webmcp_report` call:

```rust
        // Capabilities are resolved only for an authenticated admin. Reading
        // the header first and checking the tier second would let a token
        // decide the manifest on its own, which is exactly the standalone
        // credential this design does not want it to be.
        let held = if caller == AuthLevel::Admin {
            match msg.header(AGENT_SESSION_HEADER) {
                Some(token) if !token.is_empty() => {
                    match agent_sessions::resolve(ctx, token, msg.user_id()).await {
                        Some(session) => CapabilitySet::from_iter(session.capabilities),
                        None => CapabilitySet::none(),
                    }
                }
                _ => CapabilitySet::none(),
            }
        } else {
            CapabilitySet::none()
        };
```

and pass `&held` as the final argument. Pass `&CapabilitySet::none()` at the boot-time census in `registration.rs` — it reports defects in declarations, which are caller-independent.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p impresspress-core -- webmcp manifest`
Expected: PASS.

- [ ] **Step 5: Verify load-bearing**

Resolve `held` before the `caller == AuthLevel::Admin` check; confirm `a_session_token_does_not_give_an_anonymous_caller_write_tools` fails. Restore.

- [ ] **Step 6: Commit**

```bash
git add crates/impresspress-core/src/pipeline.rs crates/impresspress-core/src/builder/registration.rs
git commit -m "feat(webmcp): resolve edit-session capabilities for the manifest"
```

---

### Task 5: The opt-in gate

**Files:**
- Modify: `crates/impresspress-core/src/builder/mod.rs` (add the flag beside the other builder methods, ~:108-340)
- Modify: `crates/impresspress-core/src/builder/registration.rs` (skip the agent routes when off)
- Test: `crates/impresspress-core/src/builder/mod.rs` (inline tests)

**Interfaces:**
- Consumes: nothing.
- Produces: `ImpresspressBuilder::enable_agent_writes(self) -> Self`; `BuiltRuntime::agent_writes_enabled() -> bool`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn agent_writes_are_off_by_default() {
    let built = ImpresspressBuilder::new().build_for_test();
    assert!(!built.agent_writes_enabled());
    let products = built.block_info("impresspress/products");
    assert!(
        !products.endpoints.iter().any(|ep| ep.path.starts_with("/b/products/agent/")),
        "the agent routes must not exist when the feature is off"
    );
}

#[test]
fn enable_agent_writes_registers_the_agent_routes() {
    let built = ImpresspressBuilder::new().enable_agent_writes().build_for_test();
    assert!(built.agent_writes_enabled());
    let products = built.block_info("impresspress/products");
    assert!(products.endpoints.iter().any(|ep| ep.path == "/b/products/agent/products"));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p impresspress-core agent_writes`
Expected: FAIL — no method `enable_agent_writes`.

- [ ] **Step 3: Implement**

```rust
    /// Allow an admin to grant a browser agent write access to products
    /// through an explicit, time-boxed edit session. Off by default.
    ///
    /// Deliberately a builder call rather than a config variable: shared
    /// config has no read path on Workers (impresspress#78), so a variable
    /// defaulting to off would be permanently off on Cloudflare — the one
    /// deployment where enabling it matters most.
    pub fn enable_agent_writes(mut self) -> Self {
        self.agent_writes = true;
        self
    }
```

Thread the flag into products' `info()` so the `/b/products/agent/*` endpoints are only pushed when it is set.

- [ ] **Step 4: Run to verify they pass; Step 5: verify load-bearing** (register the routes unconditionally, confirm `agent_writes_are_off_by_default` fails, restore)

- [ ] **Step 6: Commit**

```bash
git add crates/impresspress-core/src/builder/
git commit -m "feat: enable_agent_writes builder opt-in, off by default"
```

---

### Task 6: `create_product_draft`

**Files:**
- Modify: `crates/impresspress-core/src/blocks/products/contracts.rs` (add `AgentProductDraftRequest`, `AgentProductView`)
- Create: `crates/impresspress-core/src/blocks/products/handlers/agent.rs`
- Modify: `crates/impresspress-core/src/blocks/products/mod.rs`, `handlers/dispatch.rs`
- Test: `crates/impresspress-core/src/blocks/products/tests/agent_tests.rs`

**Interfaces:**
- Consumes: `repo::products::create` (Plan B), the capability gate (Task 4), the opt-in (Task 5).
- Produces: `POST /b/products/agent/products` → `AgentProductView`, tool `create_product_draft`, capability `products:edit`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn create_product_draft_writes_a_draft() {
    let ctx = agent_ctx().await;
    let body = json_body(agent_create(&ctx, json!({"name": "Jacket", "currency": "NZD"})).await).await;
    assert_eq!(body["status"], json!("draft"));
    assert_eq!(body["name"], json!("Jacket"));
}

/// The escape hatch this contract exists to close: `CreateProductRequest`
/// has an optional `status` that `handle_create_product` fills with
/// `or_insert`, so reusing it would let the agent publish directly.
#[tokio::test]
async fn a_smuggled_status_cannot_publish() {
    let ctx = agent_ctx().await;
    let body = json_body(agent_create(&ctx, json!({"name": "Sneaky", "status": "active"})).await).await;
    assert_eq!(body["status"], json!("draft"));

    let catalog = json_body(handle_catalog(ctx.ctx(), &catalog_msg()).await).await;
    assert!(catalog["items"].as_array().unwrap().is_empty());
}

/// And the schema must not advertise the field either, or an agent will
/// reasonably try to set it.
#[test]
fn the_published_input_schema_names_no_status() {
    let info: BlockInfo = products_info_with_agent_writes();
    let ep = info.endpoints.iter().find(|e| e.path == "/b/products/agent/products").unwrap();
    let schema = serde_json::to_value(ep.input_schema.as_ref().unwrap()).unwrap();
    assert!(schema["properties"].get("status").is_none(), "schema: {schema}");
}

#[tokio::test]
async fn create_is_refused_without_a_session() {
    let ctx = agent_ctx().await;
    let out = agent_create_without_token(&ctx, json!({"name": "Nope"})).await;
    assert!(output_is_error(&out));
    assert_eq!(count_products(&ctx).await, 0);
}

#[tokio::test]
async fn create_is_refused_for_a_non_admin_holding_a_valid_token() {
    let ctx = agent_ctx().await;
    let (_s, token) = issue_session(&ctx, "admin-1").await;
    let out = agent_create_as_user(&ctx, "user-2", &token, json!({"name": "Nope"})).await;
    assert!(output_is_error(&out));
    assert_eq!(count_products(&ctx).await, 0);
}

#[tokio::test]
async fn create_writes_an_audit_row_attributing_the_agent() {
    let ctx = agent_ctx().await;
    agent_create(&ctx, json!({"name": "Jacket"})).await;
    let row = latest_audit_row(&ctx, "product.create").await;
    assert_eq!(row.str_field("user_id"), "admin-1");
    assert_eq!(row.str_field("via"), "agent");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p impresspress-core create_product_draft`
Expected: FAIL — route not found.

- [ ] **Step 3: Implement the contract and handler**

```rust
// No `status` field, deliberately. `CreateProductRequest` carries
// `status: Option<ProductStatus>` and `handle_create_product` defaults it
// with `or_insert`, which is right for a human admin and wrong here: an
// agent that can set `status` can publish to customers. Not a type alias,
// not a `#[serde(flatten)]` of that request, not a `From` — a separate type
// whose published schema simply has no such property.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentProductDraftRequest {
    /// Customer-visible product name.
    pub name: String,
    /// Longer description shown on the product page.
    #[serde(default)]
    pub description: Option<String>,
    /// ISO 4217 currency code.
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub image_url: Option<String>,
}
```

The handler resolves the session, builds the column map, then sets the invariants **unconditionally** (`insert`, never `entry().or_insert()`):

```rust
    let mut data = request.into_columns();
    // Unconditional: `or_insert` here would reintroduce exactly the escape
    // hatch this type exists to remove.
    data.insert("status".to_string(), json!("draft"));
    data.insert("created_by".to_string(), json!(msg.user_id()));
    data.insert("owner_kind".to_string(), json!("platform"));
    data.insert("owner_id".to_string(), json!(""));
    data.insert("approval_status".to_string(), json!("approved"));
```

Declare the endpoint with `AuthLevel::Admin`, `.input::<AgentProductDraftRequest>()`, `.output::<AgentProductView>()`, `.agent_tool("create_product_draft", …)`, `.requires_capability("products:edit")`. The description must state that the product is created as a draft and is not visible to customers until a person publishes it.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p impresspress-core -- agent`
Expected: PASS.

- [ ] **Step 5: Verify load-bearing**

Change the `data.insert("status", …)` to `data.entry(...).or_insert(...)`; confirm `a_smuggled_status_cannot_publish` fails. Restore.

- [ ] **Step 6: Snapshot, then commit**

```bash
cargo test -p impresspress openapi_snapshot   # read every added line
git add crates/impresspress-core/src/blocks/products/
git commit -m "feat(products): create_product_draft, draft-only by construction"
```

---

### Task 7: `update_product_draft`

**Files:** as Task 6, plus `handlers/agent.rs`

**Interfaces:**
- Consumes: Task 6's contract module.
- Produces: `PATCH /b/products/agent/products/{id}` → `AgentProductView`, tool `update_product_draft`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn update_edits_a_draft() {
    let ctx = agent_ctx().await;
    let id = seed_draft(&ctx, "Old").await;
    let body = json_body(agent_update(&ctx, &id, json!({"name": "New"})).await).await;
    assert_eq!(body["name"], json!("New"));
}

/// An active product is customer-facing. Editing one silently changes what a
/// shopper sees, so the agent may not — mirroring `repo/offers.rs:624`,
/// where active and archived offers are already immutable.
#[tokio::test]
async fn update_refuses_an_active_product() {
    let ctx = agent_ctx().await;
    let id = seed_active_product(&ctx, "Live").await;
    let out = agent_update(&ctx, &id, json!({"name": "Hijacked"})).await;
    assert!(output_is_error(&out));
    assert_eq!(product_name(&ctx, &id).await, "Live");
}

#[tokio::test]
async fn update_refuses_an_archived_product() {
    let ctx = agent_ctx().await;
    let id = seed_archived_product(&ctx, "Old").await;
    assert!(output_is_error(&agent_update(&ctx, &id, json!({"name": "X"})).await));
}

#[tokio::test]
async fn update_refuses_a_soft_deleted_product() {
    let ctx = agent_ctx().await;
    let id = seed_draft(&ctx, "Gone").await;
    soft_delete_product(&ctx, &id).await;
    assert!(output_is_error(&agent_update(&ctx, &id, json!({"name": "X"})).await));
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p impresspress-core update_product_draft`

- [ ] **Step 3: Implement** — load through `repo::products::get` (which already excludes soft-deleted), refuse unless `status == "draft"`, then `repo::products::update`. The request type reuses `AgentProductDraftRequest`'s fields with all of them optional; it likewise has no `status`.

- [ ] **Step 4: Run to verify they pass; Step 5: verify load-bearing** (drop the `status == "draft"` guard, confirm `update_refuses_an_active_product` fails, restore)

- [ ] **Step 6: Commit**

```bash
git commit -am "feat(products): update_product_draft, drafts only"
```

---

### Task 8: `archive_product` and `delete_product`

**Files:** as Task 7

**Interfaces:**
- Consumes: `repo::products::{soft_delete, update}` (Plan B).
- Produces: `POST /b/products/agent/products/{id}/archive`, `DELETE /b/products/agent/products/{id}`, tools `archive_product` and `delete_product`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn archive_takes_a_product_out_of_the_catalog() {
    let ctx = agent_ctx().await;
    let id = seed_active_product(&ctx, "Live").await;
    assert!(!output_is_error(&agent_archive(&ctx, &id).await));

    let catalog = json_body(handle_catalog(ctx.ctx(), &catalog_msg()).await).await;
    assert!(catalog["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn archive_is_reversible() {
    let ctx = agent_ctx().await;
    let id = seed_active_product(&ctx, "Live").await;
    agent_archive(&ctx, &id).await;
    assert_eq!(product_status(&ctx, &id).await, "archived");
}

#[tokio::test]
async fn delete_is_soft_and_keeps_order_history() {
    let ctx = agent_ctx().await;
    let id = seed_active_product(&ctx, "Sold").await;
    let order = seed_purchase(&ctx, &id).await;
    assert!(!output_is_error(&agent_delete(&ctx, &id).await));

    let row = db::get(ctx.ctx(), repo::products::TABLE, &id).await.unwrap();
    assert!(!row.str_field("deleted_at").is_empty());
    let purchase = db::get(ctx.ctx(), PURCHASES_TABLE, &order.id).await.unwrap();
    assert_eq!(purchase.str_field("product_id"), id);
}

#[tokio::test]
async fn both_destructive_verbs_are_refused_without_a_session() {
    let ctx = agent_ctx().await;
    let id = seed_active_product(&ctx, "Live").await;
    assert!(output_is_error(&agent_archive_without_token(&ctx, &id).await));
    assert!(output_is_error(&agent_delete_without_token(&ctx, &id).await));
    assert_eq!(product_status(&ctx, &id).await, "active");
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p impresspress-core archive_product delete_is_soft`

- [ ] **Step 3: Implement** — `archive` writes `status = "archived"` via `repo::products::update`; `delete` calls `repo::products::soft_delete`. Both audit with `via = agent`. Both tool descriptions state that the operation asks the site owner to confirm in their browser before it happens.

- [ ] **Step 4: Run to verify they pass; Step 5: verify load-bearing**

- [ ] **Step 6: Commit**

```bash
git commit -am "feat(products): archive_product and delete_product for agents"
```

---

### Task 9: `webmcp.js` — token, re-registration, confirmation

**Files:**
- Modify: `crates/impresspress-core/src/ui/assets/webmcp.js` (145 lines; the load-time fetch at :124, `register` at :63)
- Test: `crates/impresspress-web/tests/e2e/webmcp.spec.ts`

**Interfaces:**
- Consumes: the manifest's `requiresConfirmation` flag on gated tools.
- Produces: `window.impresspressAgentSession.start(token, expiresAt)` and `.stop()`.

- [ ] **Step 1: Write the failing e2e tests**

```ts
test('write tools appear only after a session starts', async ({ page }) => {
  await signInAsAdmin(page);
  await installModelContextPolyfill(page);
  expect(await toolNames(page)).not.toContain('create_product_draft');

  await startAgentSession(page);           // clicks the admin control
  expect(await toolNames(page)).toContain('create_product_draft');

  await stopAgentSession(page);
  expect(await toolNames(page)).not.toContain('create_product_draft');
});

test('a destructive tool waits for the operator to confirm', async ({ page }) => {
  await signInAsAdmin(page);
  await installModelContextPolyfill(page);
  await startAgentSession(page);

  const pending = invokeTool(page, 'delete_product', { id: seededProductId });
  await expect(page.getByRole('dialog', { name: /delete/i })).toContainText(seededProductName);
  await page.getByRole('button', { name: 'Cancel' }).click();

  const result = await pending;
  expect(result.isError).toBe(true);
  await expect(page.locator(`[data-product-id="${seededProductId}"]`)).toBeVisible();
});
```

- [ ] **Step 2: Run to verify they fail**

Run: `npx playwright test webmcp.spec.ts -g "session"`
Expected: FAIL — no session control, and write tools never register.

- [ ] **Step 3: Implement**

Refactor the load-time fetch into `loadAndRegister(token, signal)`. Changes to `register`:

```js
    // The session token is attached here, by the page, and is never a tool
    // argument: it must not appear in the manifest, in the agent's context,
    // or in anything the model can echo back.
    if (agentSessionToken) {
      req.init.headers = Object.assign({}, req.init.headers, {
        'X-Impresspress-Agent-Session': agentSessionToken,
      });
    }
```

and, for a tool the manifest marks `requiresConfirmation`, `execute` awaits a modal before fetching, returning `{isError: true, content: [{type: 'text', text: 'The site owner declined this action.'}]}` on cancel. The modal is rendered by the page — an agent can read the page but cannot click it, which is the whole defence.

`start(token, expiresAt)` stores the token in a closure variable (never `localStorage`), re-fetches the manifest and registers the new tools under a fresh `AbortController`. `stop()` aborts it, which unregisters and fires `toolchange`. A timer calls `stop()` at `expiresAt`.

- [ ] **Step 4: Run to verify they pass**

Run: `npx playwright test webmcp.spec.ts`
Expected: PASS.

- [ ] **Step 5: Verify load-bearing**

Make the confirmation modal auto-accept; confirm the cancel test fails. Restore.

- [ ] **Step 6: Commit**

```bash
git add crates/impresspress-core/src/ui/assets/webmcp.js crates/impresspress-web/tests/e2e/webmcp.spec.ts
git commit -m "feat(ui): register write tools per edit session; confirm destructive ones"
```

---

### Task 10: The admin control

**Files:**
- Modify: the admin shell template that renders the products admin page
- Test: `crates/impresspress-web/tests/e2e/webmcp.spec.ts`

**Interfaces:**
- Consumes: Task 3's endpoints, Task 9's `window.impresspressAgentSession`.
- Produces: the "Allow agent editing" control.

- [ ] **Step 1: Write the failing test**

```ts
test('the control is absent when agent writes are disabled', async ({ page }) => {
  await signInAsAdmin(page);            // fixture built WITHOUT enable_agent_writes()
  await page.goto('/b/admin/');
  await expect(page.getByRole('button', { name: /allow agent editing/i })).toHaveCount(0);
});
```

- [ ] **Step 2: Run to verify it fails** — the control renders unconditionally.

- [ ] **Step 3: Implement** — render the control only when the runtime reports `agent_writes_enabled()`. On click, POST to `/b/admin/api/agent-sessions`, hand the token to `window.impresspressAgentSession.start(...)`, and show a countdown with a Stop button that calls `.stop()` and DELETEs the session.

- [ ] **Step 4-5: Run; verify load-bearing** (render unconditionally, confirm the test fails, restore)

- [ ] **Step 6: Commit**

```bash
git commit -am "feat(admin): control to open and close an agent edit session"
```

---

### Task 11: Replace the read-only policy test

**Files:**
- Modify: `crates/impresspress-core/src/blocks/admin/mod.rs:536` (`no_admin_write_is_an_agent_tool`)
- Create: `crates/impresspress-core/src/blocks/agent_tool_policy_tests.rs`

**Interfaces:**
- Consumes: every block's `BlockInfo`.
- Produces: the invariant that replaces the old policy.

**Why:** `no_admin_write_is_an_agent_tool` would still pass after this work, because the new writes are on the products block. Passing for an irrelevant reason is the failure mode this project has hit repeatedly, so it is replaced by something strictly stronger across all blocks.

- [ ] **Step 1: Write the failing tests**

```rust
/// Any agent tool that mutates must be gated. Without this, adding a write
/// tool and forgetting `.requires_capability(..)` publishes it to every
/// admin with no session at all.
#[test]
fn every_non_get_agent_tool_declares_a_capability() {
    for block in all_block_infos_with_agent_writes() {
        for ep in &block.endpoints {
            let Some(tool) = ep.agent_tool.as_ref() else { continue };
            if ep.method != HttpMethod::Get {
                assert!(
                    tool.requires_capability.is_some(),
                    "{} {} publishes {} with no capability gate",
                    ep.method, ep.path, tool.name
                );
            }
        }
    }
}

/// And the gate must actually be closed by default.
#[test]
fn no_gated_tool_appears_with_no_capabilities_held() {
    let manifest = generate_webmcp_declared_auth(
        &all_block_infos_with_agent_writes(),
        AuthLevel::Admin,
        &CapabilitySet::none(),
    );
    for name in tool_names(&manifest) {
        assert!(
            !gated_tool_names().contains(&name),
            "{name} is capability-gated but published to a caller holding nothing"
        );
    }
}

/// The admin block itself stays read-only: its writes are still not tools.
#[test]
fn no_admin_block_write_is_an_agent_tool() {
    let info: BlockInfo = AdminBlock::default().info();
    for ep in &info.endpoints {
        if ep.is_agent_tool() {
            assert_eq!(ep.method, HttpMethod::Get, "{} must stay a read", ep.path);
        }
    }
}
```

- [ ] **Step 2: Run** — `cargo test -p impresspress-core agent_tool_policy`
Expected: PASS given Tasks 6-8. A failure names a real ungated write.

- [ ] **Step 3: Verify load-bearing**

Remove `.requires_capability("products:edit")` from `create_product_draft`; confirm both of the first two tests fail. Restore.

- [ ] **Step 4: Delete the superseded test and commit**

Remove `no_admin_write_is_an_agent_tool` (replaced by `no_admin_block_write_is_an_agent_tool` plus the two stronger invariants), leaving a comment pointing at the new module.

```bash
git add crates/impresspress-core/src/blocks/
git commit -m "test: replace the admin read-only policy with a capability invariant across all blocks"
```

---

### Task 12: End-to-end and merge-debt reconciliation

**Files:**
- Modify: `crates/impresspress-web/tests/e2e/webmcp.spec.ts` (the exact tool-set assertions at `:198`, `:304`, `:329`)

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Reconcile the three tool-set assertions**

Those assertions already carry unresolved debt between #76 and #81 (`list_products` plus four admin tools). This work adds a fourth axis: admin-with-session. Update all three to assert the no-session sets, and add a fourth assertion for the session set. If the #76/#81 debt is still open when this lands, carry `test/integration-local`'s commit (`a19b7bf`) first, then layer this on top.

- [ ] **Step 2: Add the full-journey test**

```ts
test('an agent can create a draft that no shopper can see', async ({ page }) => {
  await signInAsAdmin(page);
  await installModelContextPolyfill(page);
  await startAgentSession(page);

  const created = await invokeTool(page, 'create_product_draft', {
    name: 'Agent Jacket', currency: 'NZD',
  });
  expect(created.isError).toBeFalsy();

  const anon = await page.context().browser().newContext();
  const anonPage = await anon.newPage();
  await installModelContextPolyfill(anonPage);
  const catalog = await invokeTool(anonPage, 'list_products', {});
  expect(JSON.stringify(catalog)).not.toContain('Agent Jacket');
});
```

- [ ] **Step 3: Run the whole suite**

Run: `npx playwright test webmcp.spec.ts` and `cargo test --workspace`
Expected: PASS.

- [ ] **Step 4: Commit and open the PR**

```bash
git add -A
git commit -m "test(e2e): drive the agent write tools through a real session"
git push -u origin feat/agent-product-writes
gh pr create --title "feat(webmcp): agent-driven product writes behind an admin edit session" --body "$(cat <<'BODY'
Adds four write tools — create_product_draft, update_product_draft,
archive_product, delete_product — behind three independent layers:

- draft-only by construction (the request type has no `status` field, and
  the handler sets the column unconditionally rather than with `or_insert`);
- a capability gate, so the tools are absent from the manifest until an admin
  deliberately opens a time-boxed edit session;
- a confirmation modal on the destructive verbs, which defends against prompt
  injection — the agent can read the page but cannot click.

Every agent route is `AuthLevel::Admin` AND requires the capability. Neither
check satisfies the other, so a leaked token is not a standalone credential.

Ships off by default (`enable_agent_writes()`), a builder call rather than a
config variable because impresspress#78 would pin a variable off on Workers.

The old `no_admin_write_is_an_agent_tool` is replaced by a stronger invariant
across every block: any non-GET agent tool must declare a capability, and no
gated tool may appear to a caller holding none.

Spec: docs/superpowers/specs/2026-09-01-agent-product-writes-design.md

🤖 Generated with [Claude Code](https://claude.com/claude-code)

https://claude.ai/code/session_01WMJ8nQz9HTrc6CsSesAXUk
BODY
)"
```
