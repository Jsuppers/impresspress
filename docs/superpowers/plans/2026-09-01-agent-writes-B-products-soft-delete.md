# Plan B — Products soft delete (impresspress) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make deleting a product a soft delete, so order history stops being orphaned, and route every products-table read through one module that filters `deleted_at IS NULL`.

**Architecture:** A new `repo/products.rs` becomes the sole owner of the products table, following the convention its sibling modules already document. Every read carries the soft-delete filter. Only once every read is migrated does `handle_delete_product` switch from `db::delete` to writing `deleted_at` — the reverse order would ship a window in which a deleted product stays publicly purchasable.

**Tech Stack:** Rust, `wafer-core::clients::database`, `wafer_block::db::{Filter, FilterOp}`.

**Spec:** `docs/superpowers/specs/2026-09-01-agent-product-writes-design.md` (§3)

**Repo:** `impresspress`. Independent of Plan A and Plan C — this ships on its own as a bug fix.

## Global Constraints

- **Task order is load-bearing.** The write of `deleted_at` (Task 5) comes last. `handle_catalog` filters only `status = 'active'` (`catalog.rs:25-29`) and the single-product route only checks `status` (`catalog.rs:67`), so soft-deleting before those are migrated leaves a deleted, active product in the public catalog and purchasable.
- **No raw SQL.** Use `wafer_block::db::{Filter, FilterOp}`; `FilterOp::IsNull` already exists.
- **No sync bridges.** Everything here is async; callers stay async.
- `///` becomes published API documentation. Use `//` for rationale.
- Every test must be verified load-bearing: revert the behaviour, watch the test fail, restore.
- **Never regenerate an `/openapi.json` snapshot to get green.** Read every changed line.
- Do not pass `--locked` locally; the local `[patch]` rewrites `Cargo.lock` and that rewrite is an artifact — `git checkout Cargo.lock` before committing unless a `Cargo.toml` actually changed.

---

### Task 1: The repo module that owns the table

**Files:**
- Create: `crates/impresspress-core/src/blocks/products/repo/products.rs`
- Modify: `crates/impresspress-core/src/blocks/products/repo/mod.rs` (add `pub(crate) mod products;` to the list at :8-21)
- Test: `crates/impresspress-core/src/blocks/products/repo/products.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub(crate) const TABLE: &str = "impresspress__products__products";`
  - `pub(crate) async fn get(ctx: &dyn Context, id: &str) -> Result<Record, wafer_run::Error>` — `NotFound` for a soft-deleted row
  - `pub(crate) async fn list_page(ctx: &dyn Context, page: i64, page_size: i64, filters: Vec<Filter>, sort: Option<Vec<SortField>>) -> Result<RecordList, OutputStream>`
  - `pub(crate) async fn count(ctx: &dyn Context, filters: &[Filter]) -> Result<i64, wafer_run::Error>`
  - `pub(crate) async fn create(ctx: &dyn Context, data: HashMap<String, Value>) -> Result<Record, OutputStream>`
  - `pub(crate) async fn update(ctx: &dyn Context, id: &str, data: HashMap<String, Value>) -> Result<Record, OutputStream>`
  - `pub(crate) async fn purge(ctx: &dyn Context, id: &str) -> Result<(), wafer_run::Error>` — a genuine hard delete, for rolling back a row that was never visible
  - `pub(crate) fn live_filter() -> Filter`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestContext;

    async fn seed(ctx: &TestContext, id: &str, deleted_at: Option<&str>) {
        let mut data = HashMap::from([
            ("id".to_string(), json!(id)),
            ("name".to_string(), json!(id)),
            ("status".to_string(), json!("active")),
        ]);
        if let Some(ts) = deleted_at {
            data.insert("deleted_at".to_string(), json!(ts));
        }
        db::create(ctx.ctx(), TABLE, data).await.expect("seed");
    }

    #[tokio::test]
    async fn get_refuses_a_soft_deleted_row() {
        let ctx = TestContext::new().await;
        seed(&ctx, "gone", Some("2026-09-01T00:00:00Z")).await;
        let err = get(ctx.ctx(), "gone").await.expect_err("must not resolve");
        assert_eq!(err.code, wafer_run::ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn get_returns_a_live_row() {
        let ctx = TestContext::new().await;
        seed(&ctx, "here", None).await;
        let row = get(ctx.ctx(), "here").await.expect("must resolve");
        assert_eq!(row.str_field("name"), "here");
    }

    #[tokio::test]
    async fn list_page_excludes_soft_deleted_rows() {
        let ctx = TestContext::new().await;
        seed(&ctx, "live", None).await;
        seed(&ctx, "gone", Some("2026-09-01T00:00:00Z")).await;
        let list = list_page(ctx.ctx(), 1, 50, vec![], None).await.expect("list");
        let ids: Vec<&str> = list.items.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["live"]);
    }

    // A caller-supplied filter must narrow the live set, never replace the
    // soft-delete filter. Appending rather than replacing is the whole point
    // of routing reads through here.
    #[tokio::test]
    async fn caller_filters_are_added_to_the_soft_delete_filter() {
        let ctx = TestContext::new().await;
        seed(&ctx, "gone", Some("2026-09-01T00:00:00Z")).await;
        let status_active = Filter {
            field: "status".to_string(),
            operator: FilterOp::Equal,
            value: json!("active"),
        };
        let list = list_page(ctx.ctx(), 1, 50, vec![status_active], None)
            .await
            .expect("list");
        assert!(list.items.is_empty(), "a soft-deleted active row must not list");
    }

    #[tokio::test]
    async fn count_excludes_soft_deleted_rows() {
        let ctx = TestContext::new().await;
        seed(&ctx, "live", None).await;
        seed(&ctx, "gone", Some("2026-09-01T00:00:00Z")).await;
        assert_eq!(count(ctx.ctx(), &[]).await.expect("count"), 1);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p impresspress-core repo::products`
Expected: FAIL — module `products` not found in `repo`.

- [ ] **Step 3: Write the module**

```rust
//! Row-level access over `impresspress__products__products`.
//!
//! This module is the sole place that issues `db::*` against the products
//! table, per the `repo`-module-owns-its-`TABLE` convention documented in
//! `repo/mod.rs`. Every read here carries the soft-delete filter, which is
//! why routing reads through it is a correctness requirement and not tidying:
//! the products table has carried `deleted_at` since migration 001 and the
//! partial unique slug index in 005 is defined on `deleted_at IS NULL`, but
//! before this module nothing ever wrote the column and the public catalog
//! filtered on `status` alone.

use std::collections::HashMap;

use serde_json::Value;
use wafer_block::db::{Filter, FilterOp, SortField};
use wafer_core::clients::database::{self as db, Record, RecordList};
use wafer_run::context::Context;

use crate::blocks::crud;
use crate::error::OutputStream;

pub(crate) const TABLE: &str = "impresspress__products__products";

/// `deleted_at IS NULL` — the predicate that distinguishes a live product
/// from a soft-deleted one.
pub(crate) fn live_filter() -> Filter {
    Filter {
        field: "deleted_at".to_string(),
        operator: FilterOp::IsNull,
        value: Value::Null,
    }
}

/// Fetch one live product. A soft-deleted row answers `NotFound`, so callers
/// need no extra check and cannot forget one.
pub(crate) async fn get(ctx: &dyn Context, id: &str) -> Result<Record, wafer_run::Error> {
    let record = db::get(ctx, TABLE, id).await?;
    if is_deleted(&record) {
        return Err(wafer_run::Error::new(
            wafer_run::ErrorCode::NotFound,
            "Product not found",
        ));
    }
    Ok(record)
}

/// List one page of live products. `filters` narrows the live set; it cannot
/// widen it.
pub(crate) async fn list_page(
    ctx: &dyn Context,
    page: i64,
    page_size: i64,
    mut filters: Vec<Filter>,
    sort: Option<Vec<SortField>>,
) -> Result<RecordList, OutputStream> {
    filters.push(live_filter());
    crud::list_page(ctx, TABLE, page, page_size, filters, sort).await
}

/// Count live products matching `filters`.
pub(crate) async fn count(ctx: &dyn Context, filters: &[Filter]) -> Result<i64, wafer_run::Error> {
    let mut all = filters.to_vec();
    all.push(live_filter());
    db::count(ctx, TABLE, &all).await
}

pub(crate) async fn create(
    ctx: &dyn Context,
    data: HashMap<String, Value>,
) -> Result<Record, OutputStream> {
    crud::create_record(ctx, TABLE, data).await
}

pub(crate) async fn update(
    ctx: &dyn Context,
    id: &str,
    data: HashMap<String, Value>,
) -> Result<Record, OutputStream> {
    crud::update_record(ctx, TABLE, id, data, "Product").await
}

/// Hard-delete a row. Reserved for rolling back a product that failed
/// mid-creation and was never visible to anyone — see the cleanup path in
/// `handlers/product.rs`. Not the delete a user's action reaches.
pub(crate) async fn purge(ctx: &dyn Context, id: &str) -> Result<(), wafer_run::Error> {
    db::delete(ctx, TABLE, id).await
}

// The DB layer maps a NULL text column to the empty string on read, so a
// live row is either absent or empty here — checking both keeps this correct
// on SQLite/D1 and Postgres alike.
fn is_deleted(record: &Record) -> bool {
    !record.str_field("deleted_at").is_empty()
}
```

Add `pub(crate) mod products;` to `repo/mod.rs` in alphabetical position.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p impresspress-core repo::products`
Expected: PASS (5 tests).

- [ ] **Step 5: Verify the tests are load-bearing**

Remove the `filters.push(live_filter())` line in `list_page`; confirm `list_page_excludes_soft_deleted_rows` and `caller_filters_are_added_to_the_soft_delete_filter` fail. Restore. Remove the `is_deleted` branch in `get`; confirm `get_refuses_a_soft_deleted_row` fails. Restore.

- [ ] **Step 6: Commit**

```bash
git add crates/impresspress-core/src/blocks/products/repo/products.rs \
        crates/impresspress-core/src/blocks/products/repo/mod.rs
git commit -m "feat(products): repo module owning the products table, with the soft-delete filter"
```

---

### Task 2: Migrate the customer-facing reads

**Files:**
- Modify: `crates/impresspress-core/src/blocks/products/handlers/catalog.rs:13,36,64`
- Modify: `crates/impresspress-core/src/blocks/products/handlers/commerce.rs:20,191`
- Modify: `crates/impresspress-core/src/blocks/products/handlers/offers.rs:12,41`
- Modify: `crates/impresspress-core/src/blocks/products/repo/offers.rs:22,510,867`
- Test: `crates/impresspress-core/src/blocks/products/tests/handler_tests.rs`

**Interfaces:**
- Consumes: `repo::products::{get, list_page}` from Task 1.
- Produces: nothing new.

**Why these first:** they are the paths a customer reaches. Everything else is admin or seller surface.

- [ ] **Step 1: Write the failing tests**

```rust
/// The catalog filtered on `status` alone, so a soft-deleted product that
/// was still `active` stayed listed and purchasable. This is the hole soft
/// delete would otherwise open.
#[tokio::test]
async fn catalog_list_omits_a_soft_deleted_active_product() {
    let ctx = TestContext::new().await;
    seed_active_product(&ctx, "keep").await;
    seed_active_product(&ctx, "gone").await;
    soft_delete_product(&ctx, "gone").await;

    let body = json_body(handle_catalog(ctx.ctx(), &catalog_msg()).await).await;
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["keep"]);
}

#[tokio::test]
async fn catalog_detail_404s_for_a_soft_deleted_active_product() {
    let ctx = TestContext::new().await;
    seed_active_product(&ctx, "gone").await;
    soft_delete_product(&ctx, "gone").await;

    let out = handle_catalog_detail(ctx.ctx(), &catalog_detail_msg("gone")).await;
    assert!(output_is_error(&out), "a soft-deleted product must not resolve");
}

#[tokio::test]
async fn checkout_refuses_a_soft_deleted_product() {
    let ctx = TestContext::new().await;
    let offer = seed_published_offer(&ctx, "gone").await;
    soft_delete_product(&ctx, "gone").await;

    let out = handle_checkout(ctx.ctx(), &checkout_msg(&offer), checkout_body(&offer)).await;
    assert!(output_is_error(&out));
}
```

Add the two helpers beside the existing fixtures in that file:

```rust
async fn soft_delete_product(ctx: &TestContext, id: &str) {
    db::update(
        ctx.ctx(),
        repo::products::TABLE,
        id,
        HashMap::from([("deleted_at".to_string(), json!("2026-09-01T00:00:00Z"))]),
    )
    .await
    .expect("soft delete");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p impresspress-core soft_deleted`
Expected: the two catalog tests FAIL (the soft-deleted product is still listed and still resolves). `checkout_refuses_a_soft_deleted_product` PASSES already — `commerce.rs:200-203` gates on `deleted_at` today. Keep it: it pins behaviour the migration must not lose.

- [ ] **Step 3: Migrate the reads**

In `catalog.rs`, drop `use super::PRODUCTS_TABLE;` and call the repo:

```rust
    match repo::products::list_page(
        ctx,
        i64::from(query.page),
        i64::from(query.page_size),
        filters,
        Some(sort),
    )
    .await
```

and at :64 replace `db::get(ctx, PRODUCTS_TABLE, id)` with `repo::products::get(ctx, id)`. The `status != "active"` check at :67 stays — it is a different rule.

In `commerce.rs:191`, `offers.rs:41`, `repo/offers.rs:510,867`: replace each `db::get(ctx, PRODUCTS_TABLE, …)` with `repo::products::get(ctx, …)`. In `commerce.rs`, the now-redundant `deleted_at` check at :200 can go — `repo::products::get` answers `NotFound` first — but leave the `status`/`approval_status` checks alone.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p impresspress-core -- products`
Expected: PASS.

- [ ] **Step 5: Verify the tests are load-bearing**

Point `catalog.rs`'s list back at `crud::list_page(ctx, repo::products::TABLE, …)`; confirm `catalog_list_omits_a_soft_deleted_active_product` fails. Restore.

- [ ] **Step 6: Commit**

```bash
git add crates/impresspress-core/src/blocks/products/handlers/catalog.rs \
        crates/impresspress-core/src/blocks/products/handlers/commerce.rs \
        crates/impresspress-core/src/blocks/products/handlers/offers.rs \
        crates/impresspress-core/src/blocks/products/repo/offers.rs \
        crates/impresspress-core/src/blocks/products/tests/handler_tests.rs
git commit -m "fix(products): route every customer-facing product read through the repo door"
```

---

### Task 3: Migrate the admin, seller and Stripe reads

**Files:**
- Modify: `handlers/product.rs` (:15,99,114,135,160,179,214,226,315,319,336,474,491,562)
- Modify: `handlers/sellers.rs` (:13,39,89,135,203)
- Modify: `handlers/group.rs` (:14,238)
- Modify: `handlers/seller_policy.rs` (:10,152)
- Modify: `handlers/stats.rs` (:7,28,29)
- Modify: `pages.rs` (:16,306,441,523,642,1894,2837,3457)
- Modify: `stripe.rs` (:24,968,1670,1859,1942,2673)
- Modify: `mod.rs` (:17,462), `handlers/mod.rs` (:48 — delete the const)
- Test: `crates/impresspress-core/src/blocks/products/tests/handler_tests.rs`

**Interfaces:**
- Consumes: all of `repo::products` from Task 1.
- Produces: `PRODUCTS_TABLE` no longer exists; `repo::products::TABLE` replaces it.

- [ ] **Step 1: Write the failing tests**

```rust
/// Two counters read the table with no filter at all, so a soft-deleted
/// product would still be counted — the admin dashboard and the stats
/// endpoint would both overstate the catalog.
#[tokio::test]
async fn stats_do_not_count_soft_deleted_products() {
    let ctx = TestContext::new().await;
    seed_active_product(&ctx, "live").await;
    seed_active_product(&ctx, "gone").await;
    soft_delete_product(&ctx, "gone").await;

    let body = json_body(handle_stats(ctx.ctx(), &admin_msg("/b/products/api/admin/stats")).await).await;
    assert_eq!(body["products"], json!(1));
}

#[tokio::test]
async fn admin_product_detail_404s_for_a_soft_deleted_product() {
    let ctx = TestContext::new().await;
    seed_active_product(&ctx, "gone").await;
    soft_delete_product(&ctx, "gone").await;

    let out = handle_get_product(ctx.ctx(), &admin_msg("/b/products/api/products/gone")).await;
    assert!(output_is_error(&out));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p impresspress-core soft_deleted_products`
Expected: FAIL — the count is 2 and the detail resolves.

- [ ] **Step 3: Migrate the remaining sites**

Mechanical, one file at a time, compiling between each:

- `db::get(ctx, PRODUCTS_TABLE, id)` → `repo::products::get(ctx, id)`
- `db::count(ctx, PRODUCTS_TABLE, &f)` → `repo::products::count(ctx, &f)`
- `crud::get_record(ctx, PRODUCTS_TABLE, id, "Product")` → `repo::products::get(ctx, id)`, mapping the error with the existing `err_not_found`/`err_internal` helpers at the call site
- `crud::create_record(ctx, PRODUCTS_TABLE, data)` → `repo::products::create(ctx, data)`
- `crud::update_record(ctx, PRODUCTS_TABLE, id, data, "Product")` → `repo::products::update(ctx, id, data)`
- `crud::list_page(ctx, PRODUCTS_TABLE, …)` → `repo::products::list_page(ctx, …)`, dropping any hand-written `deleted_at` filter the call site already had (`pages.rs:441,2837,3457`, `seller_policy.rs:152`) — the repo now supplies it, and two copies would be two things to keep in step
- `product.rs:336` (`db::delete` cleaning up a product whose creation failed) → `repo::products::purge(ctx, &created.id)`. This one stays a hard delete: the row was never visible to anyone, and leaving a soft-deleted husk behind would consume its slug against the partial unique index.
- `mod.rs:462` → `CollectionSchema::new(repo::products::TABLE)`, matching the `repo::offers::TABLE` entries four lines below
- Delete `pub(crate) const PRODUCTS_TABLE` from `handlers/mod.rs:48` and every `PRODUCTS_TABLE` import.

- [ ] **Step 4: Run the full block suite**

Run: `cargo test -p impresspress-core -- products`
Expected: PASS. Then `cargo check --workspace --all-targets` to catch any remaining import.

- [ ] **Step 5: Verify the tests are load-bearing**

Point `stats.rs` back at `db::count(ctx, repo::products::TABLE, &[])`; confirm `stats_do_not_count_soft_deleted_products` fails. Restore.

- [ ] **Step 6: Commit**

```bash
git add -A crates/impresspress-core/src/blocks/products/
git commit -m "refactor(products): route every remaining products-table read through the repo door"
```

---

### Task 4: Gate the door shut

**Files:**
- Create: `crates/impresspress-core/src/blocks/products/tests/repo_door_test.rs`
- Modify: `crates/impresspress-core/src/blocks/products/tests/mod.rs` (declare the new module)

**Interfaces:**
- Consumes: the migration from Tasks 2 and 3.
- Produces: nothing new.

**Why a source scan rather than the compiler:** the convention this block already follows is a `pub(crate) TABLE` const per repo module, because `mod.rs`'s `collections(...)` list needs the name. That makes the const reachable, so the compiler cannot enforce the door and a test has to.

- [ ] **Step 1: Write the failing test**

```rust
//! The products table has exactly one door. A read that goes around
//! `repo::products` skips the `deleted_at` filter, and a soft-deleted
//! product becomes visible again — which for the catalog means purchasable.
//! The gate is a source scan because the table name is necessarily reachable
//! (`mod.rs` registers it in `collections(...)`), so nothing but a test can
//! catch a call site that names it directly.

const TABLE_LITERAL: &str = "impresspress__products__products";

/// Files allowed to name the products table.
const ALLOWED: &[&str] = &[
    "repo/products.rs",  // the door itself
    "tests/repo_door_test.rs", // this file
];

fn block_sources() -> Vec<(String, String)> {
    let root = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/blocks/products"
    ));
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read block dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path
                    .strip_prefix(root)
                    .expect("under root")
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, std::fs::read_to_string(&path).expect("read source")));
            }
        }
    }
    out
}

#[test]
fn only_the_repo_module_names_the_products_table() {
    let offenders: Vec<String> = block_sources()
        .into_iter()
        .filter(|(path, _)| !ALLOWED.iter().any(|a| path.ends_with(a)))
        .filter(|(_, src)| src.contains(TABLE_LITERAL))
        .map(|(path, _)| path)
        .collect();
    assert!(
        offenders.is_empty(),
        "these files name the products table directly and so bypass the \
         soft-delete filter; route them through repo::products: {offenders:?}"
    );
}

/// The old const is gone. A file that still imports it would compile only by
/// redefining it, which is the same bypass wearing the old name.
#[test]
fn the_old_products_table_const_is_gone() {
    let offenders: Vec<String> = block_sources()
        .into_iter()
        .filter(|(_, src)| src.contains("PRODUCTS_TABLE"))
        .map(|(path, _)| path)
        .collect();
    assert!(offenders.is_empty(), "PRODUCTS_TABLE still referenced in {offenders:?}");
}
```

Implement `block_sources` by walking `concat!(env!("CARGO_MANIFEST_DIR"), "/src/blocks/products")` recursively, returning paths relative to that root.

- [ ] **Step 2: Run the tests**

Run: `cargo test -p impresspress-core repo_door`
Expected: PASS if Tasks 2 and 3 are complete. If it fails, it is naming a real bypass — fix the call site, not the allowlist.

- [ ] **Step 3: Verify the test is load-bearing**

Add `let _ = "impresspress__products__products";` to `handlers/catalog.rs`. Confirm `only_the_repo_module_names_the_products_table` fails and names that file. Remove it.

- [ ] **Step 4: Commit**

```bash
git add crates/impresspress-core/src/blocks/products/tests/repo_door_test.rs \
        crates/impresspress-core/src/blocks/products/tests/mod.rs
git commit -m "test(products): gate the products table behind its repo module"
```

---

### Task 5: Make delete soft

**Files:**
- Modify: `crates/impresspress-core/src/blocks/products/repo/products.rs` (add `soft_delete`, `restore`)
- Modify: `crates/impresspress-core/src/blocks/products/handlers/product.rs:185-187`
- Test: `crates/impresspress-core/src/blocks/products/tests/handler_tests.rs`

**Interfaces:**
- Consumes: `repo::products` from Task 1; the migrated reads from Tasks 2-3; the gate from Task 4.
- Produces: `repo::products::soft_delete(ctx, id) -> Result<(), OutputStream>`, `repo::products::restore(ctx, id) -> Result<Record, OutputStream>`.

- [ ] **Step 1: Write the failing tests**

```rust
/// The bug this plan exists for: `line_items.product_id` is NOT NULL, so a
/// hard delete orphaned every order that referenced the product.
#[tokio::test]
async fn deleting_a_product_keeps_its_order_history_resolvable() {
    let ctx = TestContext::new().await;
    seed_active_product(&ctx, "sold").await;
    let order = seed_purchase(&ctx, "sold").await;

    let out = handle_delete_product(ctx.ctx(), &admin_msg("/b/products/api/products/sold")).await;
    assert!(!output_is_error(&out));

    let row = db::get(ctx.ctx(), repo::products::TABLE, "sold")
        .await
        .expect("the row must still exist");
    assert!(!row.str_field("deleted_at").is_empty(), "deleted_at must be stamped");

    let purchase = db::get(ctx.ctx(), PURCHASES_TABLE, &order.id).await.expect("order");
    assert_eq!(purchase.str_field("product_id"), "sold");
}

#[tokio::test]
async fn a_deleted_product_leaves_the_catalog() {
    let ctx = TestContext::new().await;
    seed_active_product(&ctx, "sold").await;
    handle_delete_product(ctx.ctx(), &admin_msg("/b/products/api/products/sold")).await;

    let body = json_body(handle_catalog(ctx.ctx(), &catalog_msg()).await).await;
    assert!(body["items"].as_array().unwrap().is_empty());
}

/// A soft-deleted product frees its slug, because the unique index added in
/// migration 005 is partial on `deleted_at IS NULL`.
#[tokio::test]
async fn a_deleted_product_frees_its_slug() {
    let ctx = TestContext::new().await;
    seed_active_product_with_slug(&ctx, "first", "jacket").await;
    handle_delete_product(ctx.ctx(), &admin_msg("/b/products/api/products/first")).await;
    seed_active_product_with_slug(&ctx, "second", "jacket").await; // must not conflict
}

#[tokio::test]
async fn restore_brings_a_deleted_product_back() {
    let ctx = TestContext::new().await;
    seed_active_product(&ctx, "oops").await;
    repo::products::soft_delete(ctx.ctx(), "oops").await.expect("delete");
    repo::products::restore(ctx.ctx(), "oops").await.expect("restore");
    assert!(repo::products::get(ctx.ctx(), "oops").await.is_ok());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p impresspress-core deleting_a_product`
Expected: FAIL — the row is gone entirely, so `db::get` errors and `deleted_at` is never stamped.

- [ ] **Step 3: Implement**

In `repo/products.rs`:

```rust
/// Soft-delete a product: stamp `deleted_at` and leave the row in place.
///
/// The row stays because `line_items.product_id` is NOT NULL — removing it
/// orphans every order that referenced the product. Stamping also frees the
/// product's slug, since the unique index from migration 005 is partial on
/// `deleted_at IS NULL`.
pub(crate) async fn soft_delete(ctx: &dyn Context, id: &str) -> Result<(), OutputStream> {
    get(ctx, id).await.map_err(|e| {
        if e.code == wafer_run::ErrorCode::NotFound {
            crate::error::err_not_found("Product not found")
        } else {
            crate::error::err_internal("Database error", e)
        }
    })?;
    let data = HashMap::from([("deleted_at".to_string(), Value::String(crate::util::now_iso()))]);
    update(ctx, id, data).await.map(|_| ())
}

/// Clear `deleted_at`, bringing a soft-deleted product back.
pub(crate) async fn restore(ctx: &dyn Context, id: &str) -> Result<Record, OutputStream> {
    let data = HashMap::from([("deleted_at".to_string(), Value::Null)]);
    update(ctx, id, data).await
}
```

In `handlers/product.rs`:

```rust
pub(super) async fn handle_delete_product(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let id = match crud::path_id(msg, ADMIN_PRODUCT_PREFIX, "Product") {
        Ok(id) => id,
        Err(response) => return response,
    };
    match repo::products::soft_delete(ctx, id).await {
        Ok(()) => ok_json(&crud::Deleted::done()),
        Err(response) => response,
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p impresspress-core -- products`
Expected: PASS.

- [ ] **Step 5: Verify the tests are load-bearing**

Point `handle_delete_product` back at `crud::crud_delete`; confirm `deleting_a_product_keeps_its_order_history_resolvable` fails. Restore.

- [ ] **Step 6: Check the snapshot**

Run: `cargo test -p impresspress openapi_snapshot`
The delete response shape is unchanged (`Deleted`), so expect no diff. **If a snapshot changed, read every line before accepting it — never regenerate to get green.**

- [ ] **Step 7: Commit**

```bash
git add crates/impresspress-core/src/blocks/products/
git commit -m "fix(products): soft-delete a product instead of orphaning its order history"
```

---

### Task 6: Give soft delete a door out

**Files:**
- Modify: `crates/impresspress-core/src/blocks/products/pages.rs` (`manage_products` at :422)
- Modify: `crates/impresspress-core/src/blocks/products/mod.rs` (declare the restore endpoint)
- Modify: `crates/impresspress-core/src/blocks/products/handlers/product.rs` (restore handler), `handlers/dispatch.rs` (route it)
- Test: `crates/impresspress-core/src/blocks/products/tests/handler_tests.rs`

**Interfaces:**
- Consumes: `repo::products::{restore, list_page, live_filter}`.
- Produces: `POST /b/products/api/products/{id}/restore` (Admin), returning `ProductView`.

**Why this is in scope:** without it, soft delete is a one-way door and a deleted product is unreachable by any UI. That is a worse state than the hard delete it replaces.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn the_deleted_view_lists_only_deleted_products() {
    let ctx = TestContext::new().await;
    seed_active_product(&ctx, "live").await;
    seed_active_product(&ctx, "gone").await;
    soft_delete_product(&ctx, "gone").await;

    let html = html_body(manage_products(ctx.ctx(), &admin_msg("/b/products/admin/products?view=deleted")).await).await;
    assert!(html.contains("gone"));
    assert!(!html.contains(">live<"));
}

#[tokio::test]
async fn restore_endpoint_returns_the_product_to_the_catalog() {
    let ctx = TestContext::new().await;
    seed_active_product(&ctx, "oops").await;
    soft_delete_product(&ctx, "oops").await;

    let out = handle_restore_product(ctx.ctx(), &admin_msg("/b/products/api/products/oops/restore")).await;
    assert!(!output_is_error(&out));

    let body = json_body(handle_catalog(ctx.ctx(), &catalog_msg()).await).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p impresspress-core deleted_view restore_endpoint`
Expected: FAIL — no `view=deleted` handling and no restore handler.

- [ ] **Step 3: Implement**

In `manage_products`, read `msg.query("view")` and pick the filter:

```rust
    // The default list is live products. `?view=deleted` is the only way to
    // reach a soft-deleted row from the UI, so without it soft delete would
    // be a one-way door.
    let deleted_view = msg.query("view") == "deleted";
    let mut filters = vec![if deleted_view {
        Filter {
            field: "deleted_at".into(),
            operator: FilterOp::IsNotNull,
            value: serde_json::Value::Null,
        }
    } else {
        repo::products::live_filter()
    }];
```

Because this list deliberately reads deleted rows, it goes through `crud::list_page(ctx, repo::products::TABLE, …)` directly with an explicit comment saying so, and `repo_door_test.rs` gains `pages.rs` to `ALLOWED` **only if** the scan flags it — prefer adding `repo::products::list_including_deleted(..)` to the door instead, so the allowlist stays at one entry.

Add the restore handler in `handlers/product.rs`:

```rust
pub(super) async fn handle_restore_product(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let id = match crud::path_id(msg, ADMIN_PRODUCT_PREFIX, "Product") {
        Ok(id) => id,
        Err(response) => return response,
    };
    match repo::products::restore(ctx, id).await {
        Ok(record) => ok_json(&ProductView::from_record(&record)),
        Err(response) => response,
    }
}
```

Declare the endpoint in `mod.rs` beside the other admin product routes:

```rust
                BlockEndpoint::post("/b/products/api/products/{id}/restore")
                    .summary("Restore a soft-deleted product")
                    .auth(AuthLevel::Admin)
                    .path_params_schema(id_path_schema)
                    .output::<contracts::ProductView>()
                    .tags(&["products"]),
```

and route it in `handlers/dispatch.rs`. Add a "Deleted" tab and a per-row Restore button to the products admin template.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p impresspress-core -- products`
Expected: PASS.

- [ ] **Step 5: Check the snapshot**

Run: `cargo test -p impresspress openapi_snapshot`
Expected: exactly one added path, `/b/products/api/products/{id}/restore`. Read the diff line by line before accepting.

- [ ] **Step 6: Verify the tests are load-bearing**

Make `manage_products` ignore `view`; confirm `the_deleted_view_lists_only_deleted_products` fails. Restore.

- [ ] **Step 7: Commit and open the PR**

```bash
git add -A
git commit -m "feat(products): deleted view and restore, so soft delete is not one-way"
git push -u origin fix/products-soft-delete
gh pr create --title "fix(products): soft-delete products instead of orphaning order history" --body "$(cat <<'BODY'
`line_items.product_id` is NOT NULL and `handle_delete_product` hard-deleted,
so deleting a product orphaned every order that referenced it.

The schema always assumed otherwise: `deleted_at` has existed since migration
001, the partial unique slug index in 005 is defined on `deleted_at IS NULL`,
and four read paths already filtered it. Nothing ever wrote the column.

Adds `repo::products` as the single door to the table with the filter built
in, migrates all 53 call sites, gates the door with a source-scan test, then
switches delete to stamping `deleted_at`. The public catalog filtered on
`status` alone, so migrating the reads had to land before the first write of
`deleted_at` or a deleted product would have stayed purchasable.

Also adds a deleted view and a restore route, so soft delete is not one-way.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

https://claude.ai/code/session_01WMJ8nQz9HTrc6CsSesAXUk
BODY
)"
```
