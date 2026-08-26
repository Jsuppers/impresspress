# Derive Migration (impresspress) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace every hand-written JSON schema in impresspress with schemas derived from the Rust types the handlers already deserialize into, so a schema can never drift from the contract it describes.

**Architecture:** Enable `wafer-block`'s existing `json-schema` feature, derive `schemars::JsonSchema` on the contract types, and swap each `.input_schema(json!(...))` call site for `.input::<T>()`. This deletes a duplicated layer rather than adding one. Every block is gated by a before/after `/openapi.json` snapshot diff, because derive changes what the public contract says.

**Tech Stack:** Rust, `schemars` v1.2.2, `serde_json`, `insta` or plain committed JSON fixtures for snapshots.

**Spec:** `docs/superpowers/specs/2026-08-26-webmcp-design.md`

**Repo:** All work is in `impresspress`.

**Blocked by Plan 1 Task 5.** The typed builders as they ship today emit `schema_for!` output — a root schema plus a `$defs` table with internal `#/$defs/X` refs. `generate_openapi` embeds that verbatim (`wafer-core/src/discovery.rs:94-133`) and `extract_params` lifts properties out while discarding `$defs`, so every migrated schema would put dangling references into `/openapi.json`. Plan 1 Task 5 fixes this in the builders. Starting here first means the snapshot reviews below would be approving structurally broken schemas.

Plan 1's other four tasks are independent and may run in parallel with this one.

## Global Constraints

- **schemars version is `1`**, matching `wafer-block`'s optional dep. Do not add a second schemars version to the tree.
- **The snapshot gate is mandatory and per block.** Never migrate two blocks under one snapshot diff — a widening in one hides inside the other's churn.
- **A widening is a decision, not noise.** If derive exposes a field the hand-written schema omitted, resolve it explicitly: `#[serde(skip)]`, a dedicated view type, or a recorded decision to accept. Never accept silently to make a diff pass.
- **Descriptions must survive.** Editorial text in a hand-written schema becomes a `///` doc comment on the corresponding field, which schemars emits as `description`.
- **`/openapi.json` and `/.well-known/agent.json` must keep working throughout.** They are generated at runtime from these same schemas (`pipeline.rs:126`); this migration changes their input, so every step must leave both valid.
- **Do not touch `vector` or `llm`.** They are excluded from the Worker build (`impresspress-cloudflare/Cargo.toml:74`) and are out of scope here. Do not touch `auth` either — it declares zero endpoints.
- Local `cargo test` failures in unrelated crates may be artifacts of the `[patch]` wiring in this workspace; check CI before treating them as caused by this work.

---

### Task 1: Enable `json-schema` and build the snapshot harness

Nothing can be safely migrated until there is a way to see what a migration changed. This task ships that instrument and the dependency it needs, and migrates nothing.

**Files:**
- Modify: `crates/impresspress-core/Cargo.toml`
- Create: `crates/impresspress-core/tests/openapi_snapshot.rs`
- Create: `crates/impresspress-core/tests/snapshots/` (directory, populated by the test)

**Interfaces:**
- Consumes: nothing
- Produces:
  - `schemars` available as a direct dependency of `impresspress-core`
  - `cargo test -p impresspress-core --test openapi_snapshot` writes/compares per-block snapshots
  - `UPDATE_OPENAPI_SNAPSHOTS=1` env var regenerates them

- [ ] **Step 1: Add the dependency and feature**

In `crates/impresspress-core/Cargo.toml`, change the `wafer-block` line (currently `wafer-block = { workspace = true }`, around line 125) and add `schemars` beside it:

```toml
wafer-block = { workspace = true, features = ["json-schema"] }
schemars = "1"
```

- [ ] **Step 2: Verify the feature compiles before writing anything against it**

Run: `cargo check -p impresspress-core`
Expected: clean. `schemars` v1.2.2 appears in `Cargo.lock` where it previously appeared zero times.

- [ ] **Step 3: Write the snapshot harness**

Create `crates/impresspress-core/tests/openapi_snapshot.rs`:

```rust
//! Per-block `/openapi.json` snapshots.
//!
//! This is the gate for the derive migration. Replacing a hand-written
//! schema with a derived one can change the *public contract* in two ways
//! that are invisible at the call site:
//!
//! 1. **Widening** — derive exposes every field not marked `serde(skip)`,
//!    including any a hand-written schema deliberately omitted.
//! 2. **Description loss** — editorial text vanishes unless it is
//!    reintroduced as a doc comment.
//!
//! So: snapshot a block before migrating it, migrate, then read the diff.
//! Every changed line is a decision. Regenerate with
//! `UPDATE_OPENAPI_SNAPSHOTS=1 cargo test -p impresspress-core --test openapi_snapshot`.

use std::path::PathBuf;

/// Blocks under migration, mapped to the URL prefixes they actually serve.
///
/// **The prefix is NOT derivable from the block name**, and assuming it is
/// makes this whole gate silently vacuous:
///
/// * `auth_ui` serves `/b/auth/*` — nothing is under `/b/auth_ui/`.
/// * `files` serves TWO prefixes, `/b/storage/*` and `/b/cloudstorage/*`.
///
/// A name-derived prefix would produce a permanently empty snapshot for both,
/// which passes forever and reviews nothing.
const SNAPSHOTTED_BLOCKS: &[(&str, &[&str])] = &[
    ("products", &["/b/products"]),
    ("auth_ui", &["/b/auth"]),
    ("files", &["/b/storage", "/b/cloudstorage"]),
    ("messages", &["/b/messages"]),
    ("admin", &["/b/admin"]),
];

fn snapshot_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots")
}

/// Every path in the generated OpenAPI document belonging to `block`,
/// pretty-printed with sorted keys so the output is diff-stable.
fn block_openapi(doc: &serde_json::Value, prefixes: &[&str]) -> String {
    let paths = doc["paths"].as_object().expect("openapi paths object");

    // BTreeMap gives deterministic key ordering regardless of how the
    // generator happened to insert them.
    let filtered: std::collections::BTreeMap<&String, &serde_json::Value> = paths
        .iter()
        .filter(|(path, _)| prefixes.iter().any(|p| path.starts_with(p)))
        .collect();

    serde_json::to_string_pretty(&filtered).expect("serialize block paths")
}

#[tokio::test]
async fn openapi_matches_committed_snapshots() {
    let ctx = impresspress_core::test_support::TestContext::new().await;
    let doc = impresspress_core::test_support::openapi_document(&ctx).await;

    let updating = std::env::var("UPDATE_OPENAPI_SNAPSHOTS").is_ok();
    std::fs::create_dir_all(snapshot_dir()).expect("create snapshot dir");

    let mut failures = Vec::new();

    for (block, prefixes) in SNAPSHOTTED_BLOCKS {
        let actual = block_openapi(&doc, prefixes);
        let path = snapshot_dir().join(format!("{block}.openapi.json"));

        // An empty snapshot for a block that has schema-carrying endpoints
        // means the prefix map is wrong and this block is being "guarded" by
        // a diff that can never change. Only admin is legitimately empty
        // before Task 5.
        if *block != "admin" && actual.trim() == "{}" {
            failures.push(format!(
                "\n=== {block} ===\nEMPTY snapshot. This block's prefixes {prefixes:?} matched no \
                 OpenAPI paths, so its gate is vacuous. Either the prefix map is wrong or the \
                 block is missing from the document's block list."
            ));
            continue;
        }

        if updating || !path.exists() {
            std::fs::write(&path, &actual).expect("write snapshot");
            continue;
        }

        let expected = std::fs::read_to_string(&path).expect("read snapshot");
        if expected != actual {
            failures.push(format!(
                "\n=== {block} ===\nSnapshot differs. Review EVERY changed line:\n\
                 - a new property = the contract widened; decide serde(skip), a view type, or accept\n\
                 - a removed description = editorial text lost; restore it as a /// doc comment\n\
                 Accept with: UPDATE_OPENAPI_SNAPSHOTS=1 cargo test -p impresspress-core --test openapi_snapshot\n\
                 Snapshot: {}",
                path.display()
            ));
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
```

- [ ] **Step 4: Add the test-support helpers — and widen the block list**

The harness calls `test_support::openapi_document(&ctx)`. `TestContext` already exists in `crates/impresspress-core/src/test_support.rs`; the OpenAPI helper does not. Read `pipeline.rs`'s existing test module: there is a private `discovery_json(&ctx, path, host)` helper, and it builds the document from `real_block_infos()` at `pipeline.rs:392-398`.

**`real_block_infos()` currently returns only three blocks:**

```rust
    fn real_block_infos() -> Vec<BlockInfo> {
        vec![
            AuthUiBlock::new().info(),
            FilesBlock::new().info(),
            ProductsBlock::new().info(),
        ]
    }
```

Admin and messages are absent, so they never appear in the generated document at all. Left unchanged, their snapshots are empty for a reason that has nothing to do with their schemas, and **Task 5's `admin_json_api_appears_in_openapi` test can never pass no matter how correct the migration is.**

Extend it to every Worker-shipping block:

```rust
    fn real_block_infos() -> Vec<BlockInfo> {
        vec![
            AuthUiBlock::new().info(),
            FilesBlock::new().info(),
            ProductsBlock::new().info(),
            AdminBlock::new().info(),
            MessagesBlock::new().info(),
        ]
    }
```

Match the real constructor for each block — read how each is built elsewhere in the crate rather than assuming `::new()` for all of them. Update the stale doc comment above it, which still says "the three blocks this PR added schemas to".

Then promote the helpers into `test_support.rs`:

```rust
/// Fetch the generated `/openapi.json` document. Shared by pipeline tests
/// and the per-block snapshot gate.
#[cfg(feature = "test-support")]
pub async fn openapi_document(ctx: &TestContext) -> serde_json::Value {
    discovery_json(ctx, "/openapi.json", "impresspress.example.com").await
}
```

Move `discovery_json` itself into `test_support.rs` and have `pipeline.rs`'s tests import it, so there is one implementation rather than two.

- [ ] **Step 5: Generate the baseline snapshots**

Run: `UPDATE_OPENAPI_SNAPSHOTS=1 cargo test -p impresspress-core --test openapi_snapshot`
Expected: PASS, and `crates/impresspress-core/tests/snapshots/` now contains five `.openapi.json` files.

Sanity-check them by eye: `products.openapi.json` should be substantial, `admin.openapi.json` should be nearly empty (admin has 17 endpoints and zero schemas, so `has_schema()` filters them all out — an empty snapshot here is correct, not a bug).

- [ ] **Step 6: Verify the gate catches a change in EVERY block, not just products**

This is the step that decides whether the rest of the plan is safe. Probing only products would confirm the one block whose prefix happens to match its name, and miss that auth_ui and files were silently unguarded.

For **each** of products, auth_ui, files, and messages in turn:

1. Add a junk property to one of that block's hand-written schemas — e.g. `"zzz_probe": {"type": "string"}` into a `properties` object.
2. Run: `cargo test -p impresspress-core --test openapi_snapshot`
3. Expected: FAIL, naming that specific block.
4. Revert the junk property; re-run and confirm PASS.

**If any block's probe does not produce a failure naming that block, its gate is vacuous.** The most likely cause is a wrong prefix in `SNAPSHOTTED_BLOCKS` or the block missing from `real_block_infos()`. Fix it before migrating anything — a green gate that cannot fail is worse than no gate, because it manufactures false confidence in every diff review downstream.

Admin is exempt from this probe: it has no schemas until Task 5, so its snapshot is legitimately empty and the harness's empty-snapshot check allows only admin.

- [ ] **Step 7: Commit**

```bash
git add crates/impresspress-core/Cargo.toml crates/impresspress-core/tests/openapi_snapshot.rs crates/impresspress-core/tests/snapshots/ crates/impresspress-core/src/test_support.rs crates/impresspress-core/src/pipeline.rs
git commit -m "test: add per-block openapi snapshot gate for the derive migration

Enables wafer-block's json-schema feature and commits the pre-migration
shape of every block's public contract. Derive can widen contracts and
drop descriptions silently; this makes both visible as a diff."
```

---

### Task 2: Migrate `products` — the largest and best-typed block

**Files:**
- Modify: `crates/impresspress-core/src/blocks/products/contracts.rs`
- Modify: `crates/impresspress-core/src/blocks/products/mod.rs` — **48** `let *_schema = json!(...)` bindings (lines 308-800) and **119** `BlockEndpoint::` declarations from line 1055 onward
- Test: `crates/impresspress-core/tests/snapshots/products.openapi.json` (the gate)

**Scale warning:** this is the largest task in the plan by a wide margin — 119 endpoint declarations, of which roughly 95 carry schemas, fed by 48 hand-written schema bindings. Budget accordingly; it is not a single sitting.

**Interfaces:**
- Consumes: the snapshot gate from Task 1
- Produces: `products` endpoints declaring schemas via `.input::<T>()` / `.output::<T>()` / `.path_params::<T>()` / `.query_params::<T>()`, with the hand-written schema variables deleted

**Why products first:** it has 1,115 lines of existing typed contracts, and I verified all 71 of its types derive `JsonSchema` cleanly under schemars v1.2.2. It is the block where derive has the most to work with, so it surfaces migration problems earliest.

- [ ] **Step 1: Derive `JsonSchema` on the contract types**

In `crates/impresspress-core/src/blocks/products/contracts.rs`, add `schemars::JsonSchema` to every derive list that already contains `Deserialize`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TemplateKind {
```

This applies to 71 derive attributes. A mechanical pass that works:

```bash
sed -i -E '/#\[derive\(.*Deserialize/ s/\)\]$/, schemars::JsonSchema)]/' \
  crates/impresspress-core/src/blocks/products/contracts.rs
```

Verify the count afterwards: `grep -c 'schemars::JsonSchema' crates/impresspress-core/src/blocks/products/contracts.rs` should print `71`.

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p impresspress-core`
Expected: clean. (This exact change was verified to compile during design.)

- [ ] **Step 3: Confirm the gate is still green**

Run: `cargo test -p impresspress-core --test openapi_snapshot`
Expected: PASS. Deriving `JsonSchema` alone changes no endpoint declaration, so the public contract must be byte-identical. **A failure here means something else moved** — investigate before continuing.

- [ ] **Step 4: Commit the derives separately**

Keeping derives and call-site swaps in separate commits means the snapshot diff in the next step is attributable to exactly one cause.

```bash
git add crates/impresspress-core/src/blocks/products/contracts.rs
git commit -m "refactor(products): derive JsonSchema on contract types

No behavior change — schemas are still hand-written at the call sites.
This is the prerequisite for replacing them."
```

- [ ] **Step 5: Swap one endpoint to the typed builder**

Start with a single simple endpoint to learn what the diff looks like before doing 33 of them. In `products/mod.rs`, find the `POST /b/products/api/admin/products` declaration (around line 1131):

```rust
BlockEndpoint::post("/b/products/api/admin/products")
    .summary("Create product")
    .auth(AuthLevel::Admin)
    .input_schema(product_write_schema.clone())
    .output_schema(record_schema(product_schema.clone()))
    .tags(&["products", "admin"]),
```

Identify the contract type in `contracts.rs` that `product_write_schema` describes (the type the handler deserializes the request body into — check the handler in `products/handlers/`), then replace:

```rust
BlockEndpoint::post("/b/products/api/admin/products")
    .summary("Create product")
    .auth(AuthLevel::Admin)
    .input::<ProductWrite>()
    .output_schema(record_schema(product_schema.clone()))
    .tags(&["products", "admin"]),
```

Substitute the real type name for `ProductWrite`. Leave `output_schema` alone for now — the `record_schema(...)` envelope wrapper is handled in Step 8.

- [ ] **Step 6: Read the diff — this is the real work**

Run: `cargo test -p impresspress-core --test openapi_snapshot`
Expected: FAIL, showing the `products` diff.

Read every changed line and classify it:

- **New property present that was not there before** → the contract widened. Decide: `#[serde(skip)]` on the field if it is internal, a dedicated write-view struct if the API genuinely differs from the storage type, or accept and note why.
- **`description` disappeared** → restore the hand-written text as a `///` doc comment on the corresponding field in `contracts.rs`.
- **`required` list changed** → schemars marks non-`Option` fields required. If the hand-written schema disagreed, one of the two was wrong; decide which.
- **Type narrowed or widened** (e.g. `string` became `string | null`) → an `Option<T>` in Rust that the hand-written schema treated as mandatory. Usually the Rust type is right.

- [ ] **Step 7: Resolve every diff line, then accept the snapshot**

Once every changed line is either fixed or a recorded decision:

Run: `UPDATE_OPENAPI_SNAPSHOTS=1 cargo test -p impresspress-core --test openapi_snapshot`
Then: `git diff crates/impresspress-core/tests/snapshots/products.openapi.json`
Expected: the remaining diff contains only changes you decided to accept.

- [ ] **Step 8: Handle the envelope wrappers**

`record_schema(data)` and `record_list_schema(data)` (lines 377-397) wrap a payload schema in a `{id, data}` or paginated envelope. These are structural, not contract types, and there are two clean options:

**Preferred:** introduce real generic envelope types in `contracts.rs` and derive on them:

```rust
/// A single record as returned by the JSON API.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Record<T> {
    /// Stable identifier for this record.
    pub id: String,
    /// The record payload.
    pub data: T,
}

/// A page of records.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecordList<T> {
    /// The records on this page.
    pub records: Vec<Record<T>>,
    /// Total number of records across all pages.
    pub total: i64,
}
```

Match the field names to what `record_schema` / `record_list_schema` actually emit at lines 377-397 — read them rather than trusting the names above. Then `.output::<Record<Product>>()` replaces `record_schema(product_schema.clone())`.

**Fallback:** if the envelope shape varies per endpoint in ways a generic cannot express, keep the closures for `output_schema` only and migrate inputs first. Record the reason in the commit message.

- [ ] **Step 9: Migrate the remaining endpoints in batches**

Work through the remaining ~95 schema-carrying products endpoints in groups of roughly five, running the gate after each group and committing per group. Small batches keep every diff attributable; at this volume a large batch produces a diff nobody can meaningfully review, which defeats the gate.

Expect this step to span many commits.

For each: find the type the handler actually uses, swap the builder, read the diff, resolve it.

The public storefront chain matters most — it is the surface Plan 3 exposes to agents — so migrate these carefully and last, when the pattern is well understood:
- `GET /b/products/storefront/config`
- `GET /b/products/storefront/{product_id}`
- `POST /b/products/pricing/preview`
- `POST /b/products/checkout`
- `GET /b/products/orders/{id}/status`

- [ ] **Step 10: Delete the dead schema variables**

Every `let *_schema = serde_json::json!(...)` in `products/mod.rs` (lines 308-800) with no remaining reference is now dead. Delete them.

Run: `cargo check -p impresspress-core`
Expected: clean, with no `unused variable` warnings for any `*_schema` binding. Warnings here mean a variable survived that should have been deleted, or an endpoint was missed.

- [ ] **Step 11: Full verification**

Run: `cargo test -p impresspress-core`
Expected: PASS — including `pipeline.rs`'s existing OpenAPI assertions, which check real schema content and are an independent check on the migration.

- [ ] **Step 12: Commit**

```bash
git add crates/impresspress-core/src/blocks/products/ crates/impresspress-core/tests/snapshots/products.openapi.json
git commit -m "refactor(products): derive endpoint schemas from contract types

Replaces ~30 hand-written json! schema variables with .input::<T>() /
.output::<T>() derived from the types the handlers already deserialize
into. Snapshot diff reviewed line by line; contract changes are recorded
in the snapshot rather than silent."
```

---

### Task 3: Migrate `auth_ui`

**Files:**
- Modify: `crates/impresspress-core/src/blocks/auth_ui/mod.rs` (8 schema call sites, 18 endpoints)
- Modify: the request/response types behind `crates/impresspress-core/src/blocks/auth_ui/api/` handlers
- Test: `crates/impresspress-core/tests/snapshots/auth_ui.openapi.json`

**Interfaces:**
- Consumes: the snapshot gate (Task 1); the migration pattern established in Task 2
- Produces: auth_ui endpoints declaring derived schemas

**Note:** this block owns the auth HTTP surface — `/b/auth/api/login`, `/b/auth/api/me`, `/b/auth/api/refresh` — despite living under `auth_ui`. The `auth` block itself declares zero endpoints and is not touched by this plan.

**Extra care:** `pipeline.rs:459-507` asserts specific content about these exact endpoints — that login's required list is `["email", "password"]`, that login carries no `security` field because it is `Public`, and that `me` does carry `bearerAuth`. Those tests are a second gate on this task and must keep passing without being edited to match new output. If they fail, the migration changed the contract; fix the contract, not the test.

- [ ] **Step 1: Find the types behind the login/me/refresh handlers**

Read `crates/impresspress-core/src/blocks/auth_ui/api/login.rs` and its siblings. Identify the struct each handler deserializes its request body into and serializes its response from. If a handler parses fields ad hoc out of a `serde_json::Value` rather than into a struct, **write the struct** — that is the root-cause fix, and it is why this block's schemas were hand-written.

- [ ] **Step 2: Derive `JsonSchema` on those types**

Add `schemars::JsonSchema` to their derive lists, exactly as in Task 2 Step 1.

- [ ] **Step 3: Verify compile and green gate**

Run: `cargo check -p impresspress-core && cargo test -p impresspress-core --test openapi_snapshot`
Expected: both clean. Derives alone change nothing.

- [ ] **Step 4: Swap the 8 call sites**

Replace each `.input_schema(json!(...))` / `.output_schema(json!(...))` in `auth_ui/mod.rs` with `.input::<T>()` / `.output::<T>()`.

Start with `/b/auth/api/login` at line 213, since `pipeline.rs` asserts its exact shape and will tell you immediately if the derived schema disagrees.

- [ ] **Step 5: Read and resolve the diff**

Run: `cargo test -p impresspress-core --test openapi_snapshot`
Classify every changed line using the same four categories as Task 2 Step 6.

**Watch specifically for credential leakage into the response schema.** If a response type contains a password hash, token secret, or session id that the hand-written schema omitted, that is a widening you must fix with `#[serde(skip)]` or a view type — never accept it into the snapshot.

- [ ] **Step 6: Verify the independent assertions still pass**

Run: `cargo test -p impresspress-core pipeline`
Expected: PASS, unmodified. Specifically `openapi_documents_core_auth_endpoints_with_schemas`.

- [ ] **Step 7: Accept the snapshot and commit**

```bash
UPDATE_OPENAPI_SNAPSHOTS=1 cargo test -p impresspress-core --test openapi_snapshot
git add crates/impresspress-core/src/blocks/auth_ui/ crates/impresspress-core/tests/snapshots/auth_ui.openapi.json
git commit -m "refactor(auth_ui): derive endpoint schemas from handler types

Covers the auth HTTP surface (login, me, refresh). pipeline.rs's existing
assertions on login's required fields and security requirement pass
unmodified, confirming the derived schemas match the real handlers."
```

---

### Task 4: Migrate `files` and `messages`

**Files:**
- Modify: `crates/impresspress-core/src/blocks/files/mod.rs` (4 schema call sites)
- Modify: `crates/impresspress-core/src/blocks/messages/mod.rs` (3-7 schema call sites)
- Modify: the corresponding handler types in each block
- Test: `crates/impresspress-core/tests/snapshots/files.openapi.json`, `messages.openapi.json`

**Interfaces:**
- Consumes: the pattern from Tasks 2 and 3
- Produces: files and messages endpoints declaring derived schemas

These are the two smallest blocks. Migrate them one at a time — **separate commits and separate snapshot diffs**, never both under one gate run.

- [ ] **Step 1: Migrate `files`**

Apply the Task 2 sequence to `files/mod.rs`: locate the handler types, derive `JsonSchema`, verify the gate is green, swap the 4 call sites, read the diff, resolve it.

`files` handles uploads, so check whether any endpoint takes `multipart/form-data` rather than JSON. If so its request shape is not a JSON body schema at all — leave `input_schema` alone there and note why in the commit. `crates/impresspress-core/src/multipart.rs` is the relevant code.

- [ ] **Step 2: Verify and commit `files`**

```bash
cargo test -p impresspress-core
UPDATE_OPENAPI_SNAPSHOTS=1 cargo test -p impresspress-core --test openapi_snapshot
git add crates/impresspress-core/src/blocks/files/ crates/impresspress-core/tests/snapshots/files.openapi.json
git commit -m "refactor(files): derive endpoint schemas from handler types"
```

- [ ] **Step 3: Migrate `messages`**

Same sequence against `messages/mod.rs`. `crates/impresspress-core/src/messages_schema.rs` may already hold the relevant types — read it first rather than assuming they live in the block.

- [ ] **Step 4: Verify and commit `messages`**

```bash
cargo test -p impresspress-core
UPDATE_OPENAPI_SNAPSHOTS=1 cargo test -p impresspress-core --test openapi_snapshot
git add crates/impresspress-core/src/blocks/messages/ crates/impresspress-core/tests/snapshots/messages.openapi.json
git commit -m "refactor(messages): derive endpoint schemas from handler types"
```

---

### Task 5: Type the `admin` block

`admin` has 17 endpoints and zero schemas because its handlers were never typed — `admin/users.rs` and `admin/settings.rs` contain no `pub struct` at all. This is the one Worker-shipping block where contracts must be written rather than migrated.

**Files:**
- Create: `crates/impresspress-core/src/blocks/admin/contracts.rs`
- Modify: `crates/impresspress-core/src/blocks/admin/mod.rs` (add `mod contracts;`, add schemas to the 4 JSON API endpoints)
- Modify: `crates/impresspress-core/src/blocks/admin/users.rs`, `iam.rs`, `settings.rs`, `logs.rs` (deserialize into the new types)
- Test: `crates/impresspress-core/tests/snapshots/admin.openapi.json`

**Interfaces:**
- Consumes: the snapshot gate (Task 1)
- Produces: typed contracts for admin's four JSON API endpoints, exposed in `/openapi.json` for the first time

**Scope boundary:** type the four **read** endpoints only:
- `GET /b/admin/api/users` (`mod.rs:124`)
- `GET /b/admin/api/iam/roles` (`mod.rs:125`)
- `GET /b/admin/api/settings` (`mod.rs:126`)
- `GET /b/admin/api/logs` (`mod.rs:127`)

The 13 SSR page endpoints return HTML, not JSON — they get no schemas and must never become tools. New admin **write** endpoints are explicitly out of scope for this plan.

- [ ] **Step 1: Write the failing snapshot expectation**

`admin.openapi.json` is currently near-empty because `has_schema()` filters everything out. Add the expectation that it will not be, so the task has a red state to work against. In `crates/impresspress-core/tests/openapi_snapshot.rs`, add:

```rust
#[tokio::test]
async fn admin_json_api_appears_in_openapi() {
    let ctx = impresspress_core::test_support::TestContext::new().await;
    let doc = impresspress_core::test_support::openapi_document(&ctx).await;

    for path in [
        "/b/admin/api/users",
        "/b/admin/api/iam/roles",
        "/b/admin/api/settings",
        "/b/admin/api/logs",
    ] {
        assert!(
            !doc["paths"][path]["get"].is_null(),
            "{path} must carry a schema and appear in /openapi.json — admin's \
             JSON API was previously invisible because its handlers were untyped"
        );
        assert_eq!(
            doc["paths"][path]["get"]["security"],
            serde_json::json!([{ "bearerAuth": [] }]),
            "{path} is AuthLevel::Admin and must carry a security requirement"
        );
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p impresspress-core --test openapi_snapshot admin_json_api`
Expected: FAIL — all four paths null.

- [ ] **Step 3: Write the contracts**

Create `crates/impresspress-core/src/blocks/admin/contracts.rs`. Read each handler first to learn the shape it actually returns, then write types that match. For example, for `GET /b/admin/api/users`:

```rust
//! Typed request/response contracts for the admin JSON API.
//!
//! These did not exist before: admin's handlers built responses ad hoc, so
//! the block declared no schemas and was invisible in `/openapi.json`.

use serde::{Deserialize, Serialize};

/// A user account as returned by the admin API.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AdminUserView {
    /// Stable user identifier.
    pub id: String,
    /// Login email address.
    pub email: String,
    /// Role names granted to this user.
    pub roles: Vec<String>,
    /// RFC 3339 creation timestamp.
    pub created_at: String,
}

/// Query parameters accepted by `GET /b/admin/api/users`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct AdminUserListQuery {
    /// 1-based page number.
    pub page: Option<u32>,
    /// Results per page.
    pub page_size: Option<u32>,
    /// Case-insensitive email substring filter.
    pub search: Option<String>,
}

/// Response body of `GET /b/admin/api/users`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AdminUserListResponse {
    /// Users on this page.
    pub users: Vec<AdminUserView>,
    /// Total users matching the filter, across all pages.
    pub total: i64,
}
```

Write the equivalent for roles, settings, and logs. **Match the handlers' real output** — read `admin/users.rs`, `admin/iam.rs`, `admin/settings.rs`, and `admin/logs.rs` and mirror what they emit. Inventing a shape the handler does not produce is worse than no schema, because it is a schema that lies.

**Never expose a password hash, session token, or secret value.** `GET /b/admin/api/settings` in particular lists config variables — the codebase treats a `_SECRET`/`_KEY` suffix as sensitive (see `config_vars.rs`). Confirm how the handler masks those values today and make the contract reflect the masked shape, not the raw one.

- [ ] **Step 4: Register the module**

Add to `crates/impresspress-core/src/blocks/admin/mod.rs`, beside the other `mod` declarations:

```rust
mod contracts;
```

- [ ] **Step 5: Make the handlers use the types**

Update `admin/users.rs`, `iam.rs`, `settings.rs`, and `logs.rs` to build and serialize the new structs rather than assembling `serde_json::Value` inline. This is what makes the schema true rather than merely declared — without it, the contract is still hand-maintained, just in Rust instead of JSON.

- [ ] **Step 6: Declare the schemas on the endpoints**

In `admin/mod.rs`, extend the four JSON API declarations (lines 124-127):

```rust
BlockEndpoint::get("/b/admin/api/users")
    .summary("List users API")
    .auth(AuthLevel::Admin)
    .query_params::<contracts::AdminUserListQuery>()
    .output::<contracts::AdminUserListResponse>(),
```

Apply the equivalent to roles, settings, and logs.

- [ ] **Step 7: Run to verify the tests pass**

Run: `cargo test -p impresspress-core --test openapi_snapshot admin_json_api`
Expected: PASS.

- [ ] **Step 8: Review the new admin snapshot closely**

Run: `UPDATE_OPENAPI_SNAPSHOTS=1 cargo test -p impresspress-core --test openapi_snapshot`
Then: `git diff crates/impresspress-core/tests/snapshots/admin.openapi.json`

This diff is pure addition — admin had nothing before. Read all of it and confirm no field exposes a secret, hash, or token. This is the block with the most sensitive surface in the migration.

- [ ] **Step 9: Full verification and commit**

```bash
cargo test -p impresspress-core
git add crates/impresspress-core/src/blocks/admin/ crates/impresspress-core/tests/openapi_snapshot.rs crates/impresspress-core/tests/snapshots/admin.openapi.json
git commit -m "feat(admin): add typed contracts for the JSON API

admin's 17 endpoints declared no schemas because its handlers were never
typed — its JSON API was invisible in /openapi.json. Adds contracts for
the four read endpoints and makes the handlers build them, so the schema
describes what the handler actually returns.

Read endpoints only; SSR pages return HTML and stay schema-less."
```

---

### Task 6: Measure the real wasm cost

The spec's `+94 KB raw / +21 KB gzip` estimate came from a synthetic benchmark. This task replaces the estimate with the real number, and is the checkpoint where the design's one open risk is closed.

**Files:**
- Create: `docs/2026-08-26-derive-migration-wasm-measurement.md`

**Interfaces:**
- Consumes: the completed migration (Tasks 2-5)
- Produces: a recorded measurement for future size decisions

- [ ] **Step 1: Measure the pre-migration baseline**

```bash
git stash list  # note: this workspace shares a stash stack — do not use bare git stash
git log --oneline  # find the commit immediately before Task 1
git worktree add /tmp/impresspress-baseline <commit-before-task-1>
```

Build the Cloudflare consumer wasm at that commit and record raw and gzip-9 sizes. Use the same build path the deploy CLI uses — see `crates/impresspress/src/cli/helpers/cloudflare/build.rs` for the exact wasm-pack invocation and feature set, and match it exactly. A different feature set produces a number that cannot be compared.

- [ ] **Step 2: Measure post-migration**

Build the same target at `HEAD` with the identical command and feature set. Record raw and gzip-9.

- [ ] **Step 3: Compare against the budget**

The relevant thresholds: 8 MB raw is the repo's own warn line (`profile_check.rs:44`); Cloudflare's hard limits are 3 MB compressed on Free and 10 MB compressed on Paid.

- [ ] **Step 4: Write it down**

Create `docs/2026-08-26-derive-migration-wasm-measurement.md` recording: both raw and gzip sizes, the delta, the exact build command and feature set, and how it compares to the synthetic `+94 KB / +21 KB` estimate. State plainly whether the estimate held.

- [ ] **Step 5: If the delta is larger than expected**

`worker-build` 0.7 defaults to `wasm-opt -O` rather than `-Oz`, worth roughly −215 KB raw per `docs/CODE_REVIEW_2026-07-16_FINDINGS.md`. That is available headroom, not a required change — apply it only if the measurement calls for it, and measure again if you do.

- [ ] **Step 6: Commit**

```bash
git worktree remove /tmp/impresspress-baseline
git add docs/2026-08-26-derive-migration-wasm-measurement.md
git commit -m "docs: record real wasm cost of the derive migration"
```

---

## Done criteria

- [ ] `cargo test -p impresspress-core` passes
- [ ] `cargo test --workspace` passes in impresspress
- [ ] No `serde_json::json!` schema literal remains in any migrated block's endpoint declarations
- [ ] Every snapshot change is either an intentional recorded decision or absent
- [ ] `/openapi.json` and `/.well-known/agent.json` both still return valid documents
- [ ] No secret, hash, or credential appears in any snapshot. Grep for `secret`, `password`, `hash`, `token` — then apply judgement: `access_token` and `refresh_token` in login responses and `receipt_token` in guest order status are **legitimate** parts of those contracts and already public. What must never appear is a password hash, a session-signing secret, or an unmasked `*_SECRET`/`*_KEY` config value. A blanket grep-must-be-empty criterion is unsatisfiable and would be waved through; review the hits instead.
- [ ] Real wasm delta measured and recorded

## What this plan deliberately does not do

- **No `agent_tool` annotations and no WebMCP anything.** This plan makes the schemas trustworthy; Plan 3 exposes them.
- **No `vector` or `llm` work.** Excluded from the Worker build; native-only, and must not gate the Worker milestone.
- **No `auth` block work.** It declares zero endpoints.
- **No new admin write endpoints.** Out of scope by explicit decision in the spec.
