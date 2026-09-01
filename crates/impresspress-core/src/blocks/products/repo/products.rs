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
//!
//! Every function here returns `Result<_, WaferError>` — no `OutputStream`,
//! no `err_internal`/`err_not_found`. HTTP-response construction (and every
//! other call-site policy: authz, logging, Stripe-retry) stays at the call
//! site, per the convention documented just above `mod products;` in
//! `repo/mod.rs`.

use std::collections::HashMap;

use serde_json::Value;
use wafer_block::db::{Filter, FilterOp, SortField};
use wafer_core::clients::database::{self as db, Record, RecordList};
use wafer_run::{context::Context, ErrorCode, WaferError};

use crate::util::RecordExt;

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
pub(crate) async fn get(ctx: &dyn Context, id: &str) -> Result<Record, WaferError> {
    let record = db::get(ctx, TABLE, id).await?;
    if is_deleted(&record) {
        return Err(WaferError::new(ErrorCode::NotFound, "Product not found"));
    }
    Ok(record)
}

/// List one page of live products. `filters` narrows the live set; it cannot
/// widen it. `sort` defaults to newest-first by `created_at` when `None`,
/// matching `blocks::crud::crud_list`.
pub(crate) async fn list_page(
    ctx: &dyn Context,
    page: i64,
    page_size: i64,
    mut filters: Vec<Filter>,
    sort: Option<Vec<SortField>>,
) -> Result<RecordList, WaferError> {
    filters.push(live_filter());
    let sort = sort.unwrap_or_else(|| {
        vec![SortField {
            field: "created_at".to_string(),
            desc: true,
        }]
    });
    db::paginated_list(ctx, TABLE, page, page_size, filters, sort).await
}

/// Count live products matching `filters`.
pub(crate) async fn count(ctx: &dyn Context, filters: &[Filter]) -> Result<i64, WaferError> {
    let mut all = filters.to_vec();
    all.push(live_filter());
    db::count(ctx, TABLE, &all).await
}

/// List every live product matching `filters`, unpaged. `filters` narrows
/// the live set; it cannot widen it — same contract as `list_page`, for
/// call sites (admin seller/product listings) that need the whole matching
/// set rather than one page.
pub(crate) async fn list_all(
    ctx: &dyn Context,
    mut filters: Vec<Filter>,
) -> Result<Vec<Record>, WaferError> {
    filters.push(live_filter());
    db::list_all(ctx, TABLE, filters).await
}

// `created_at`/`updated_at` are not stamped here: the database service's
// `DbExec::create`/`update` default impl (shared by every backend) already
// fills in whichever of the two the caller didn't supply — see
// `create_stamps_created_and_updated_at` / `update_stamps_a_new_updated_at`
// below, which pin that behaviour from this module's side of the call.
pub(crate) async fn create(
    ctx: &dyn Context,
    data: HashMap<String, Value>,
) -> Result<Record, WaferError> {
    db::create(ctx, TABLE, data).await
}

pub(crate) async fn update(
    ctx: &dyn Context,
    id: &str,
    data: HashMap<String, Value>,
) -> Result<Record, WaferError> {
    db::update(ctx, TABLE, id, data).await
}

/// Hard-delete a row. Reserved for rolling back a product that failed
/// mid-creation and was never visible to anyone — see the cleanup path in
/// `handlers/product.rs`. Not the delete a user's action reaches.
pub(crate) async fn purge(ctx: &dyn Context, id: &str) -> Result<(), WaferError> {
    db::delete(ctx, TABLE, id).await
}

/// Soft-delete a product: stamp `deleted_at` and leave the row in place.
///
/// The row stays because several tables carry a `product_id` column that is
/// `TEXT NOT NULL` with no default — `line_items`, `offers`,
/// `product_versions`, and `entitlements` among them — so a hard delete
/// would orphan every one of them, most visibly a completed order's line
/// item. Stamping also frees the product's slug, since the unique index
/// from migration 005 is partial on `deleted_at IS NULL`.
///
/// Routes through `get` first so deleting an already soft-deleted (or
/// missing) row answers `NotFound`, matching every other read in this
/// module: a caller can't distinguish "double delete" from "never existed"
/// any more than they can for `get` itself.
pub(crate) async fn soft_delete(ctx: &dyn Context, id: &str) -> Result<(), WaferError> {
    get(ctx, id).await?;
    let data = HashMap::from([(
        "deleted_at".to_string(),
        Value::String(crate::util::now_rfc3339()),
    )]);
    update(ctx, id, data).await.map(|_| ())
}

/// Clear `deleted_at`, bringing a soft-deleted product back.
///
/// Uses `update` directly rather than `get` + `update`: `get` refuses to
/// find a soft-deleted row by design, but restoring one is the one
/// operation in this module that must act on exactly that row.
pub(crate) async fn restore(ctx: &dyn Context, id: &str) -> Result<Record, WaferError> {
    let data = HashMap::from([("deleted_at".to_string(), Value::Null)]);
    update(ctx, id, data).await
}

// `RecordExt::str_field` (util.rs) collapses both a missing key and a JSON
// `Null` value to `""`. A live row's `deleted_at` decodes to exactly one of
// those on either backend (SQLite and Postgres both store SQL NULL for an
// unset column and both round-trip it to `Value::Null`), so checking
// "empty" here is correct on both without a backend-specific branch.
fn is_deleted(record: &Record) -> bool {
    !record.str_field("deleted_at").is_empty()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

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
        db::create(ctx, TABLE, data).await.expect("seed");
    }

    #[tokio::test]
    async fn get_refuses_a_soft_deleted_row() {
        let ctx = TestContext::with_products().await;
        seed(&ctx, "gone", Some("2026-09-01T00:00:00Z")).await;
        let err = get(&ctx, "gone").await.expect_err("must not resolve");
        assert_eq!(err.code, wafer_run::ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn get_returns_a_live_row() {
        let ctx = TestContext::with_products().await;
        seed(&ctx, "here", None).await;
        let row = get(&ctx, "here").await.expect("must resolve");
        assert_eq!(row.str_field("name"), "here");
    }

    #[tokio::test]
    async fn list_page_excludes_soft_deleted_rows() {
        let ctx = TestContext::with_products().await;
        seed(&ctx, "live", None).await;
        seed(&ctx, "gone", Some("2026-09-01T00:00:00Z")).await;
        let list = list_page(&ctx, 1, 50, vec![], None).await.expect("list");
        let ids: Vec<&str> = list.records.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["live"]);
    }

    #[tokio::test]
    async fn list_all_excludes_soft_deleted_rows() {
        let ctx = TestContext::with_products().await;
        seed(&ctx, "live", None).await;
        seed(&ctx, "gone", Some("2026-09-01T00:00:00Z")).await;
        let records = list_all(&ctx, vec![]).await.expect("list_all");
        let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["live"]);
    }

    // A caller-supplied filter must narrow the live set, never replace the
    // soft-delete filter. Appending rather than replacing is the whole point
    // of routing reads through here.
    #[tokio::test]
    async fn caller_filters_are_added_to_the_soft_delete_filter() {
        let ctx = TestContext::with_products().await;
        seed(&ctx, "gone", Some("2026-09-01T00:00:00Z")).await;
        let status_active = Filter {
            field: "status".to_string(),
            operator: FilterOp::Equal,
            value: json!("active"),
        };
        let list = list_page(&ctx, 1, 50, vec![status_active], None)
            .await
            .expect("list");
        assert!(
            list.records.is_empty(),
            "a soft-deleted active row must not list"
        );
    }

    #[tokio::test]
    async fn count_excludes_soft_deleted_rows() {
        let ctx = TestContext::with_products().await;
        seed(&ctx, "live", None).await;
        seed(&ctx, "gone", Some("2026-09-01T00:00:00Z")).await;
        assert_eq!(count(&ctx, &[]).await.expect("count"), 1);
    }

    // Pins the timestamp behaviour that `crud::crud_create` used to provide
    // via `stamp_created` before call sites are migrated onto this module:
    // dropping that helper must not silently drop the timestamps. The
    // database service's own `DbExec::create` default impl fills in any of
    // `created_at`/`updated_at` the caller omitted, so a bare `db::create`
    // pass-through (no client-side stamping) still produces both.
    #[tokio::test]
    async fn create_stamps_created_and_updated_at() {
        let ctx = TestContext::with_products().await;
        let data = HashMap::from([
            ("id".to_string(), json!("stamped")),
            ("name".to_string(), json!("stamped")),
            ("status".to_string(), json!("active")),
        ]);
        let record = create(&ctx, data).await.expect("create");
        assert!(
            !record.str_field("created_at").is_empty(),
            "created_at must be stamped"
        );
        assert!(
            !record.str_field("updated_at").is_empty(),
            "updated_at must be stamped"
        );
    }

    // Same rationale as `create_stamps_created_and_updated_at`, for the
    // `stamp_updated` half: an update that doesn't set `updated_at` itself
    // must still come back with a fresh one.
    #[tokio::test]
    async fn update_stamps_a_new_updated_at() {
        let ctx = TestContext::with_products().await;
        let data = HashMap::from([
            ("id".to_string(), json!("stamped")),
            ("name".to_string(), json!("stamped")),
            ("status".to_string(), json!("active")),
        ]);
        let created = create(&ctx, data).await.expect("create");
        let original_updated_at = created.str_field("updated_at").to_string();

        // RFC3339-with-nanoseconds timestamps from two back-to-back
        // `Utc::now()` calls almost always differ already, but a short sleep
        // makes the "changed" assertion robust against coarse clock
        // resolution in CI rather than relying on that.
        tokio::time::sleep(Duration::from_millis(5)).await;

        let updated = update(
            &ctx,
            "stamped",
            HashMap::from([("name".to_string(), json!("stamped-v2"))]),
        )
        .await
        .expect("update");
        assert_ne!(
            updated.str_field("updated_at"),
            original_updated_at,
            "update must stamp a fresh updated_at"
        );
    }

    /// The bug this whole plan exists for: deleting a product used to remove
    /// the row outright, orphaning every NOT-NULL `product_id` reference to
    /// it (most visibly a completed order's line item). Soft delete must
    /// stamp `deleted_at` and leave the row resolvable by a raw `db::get`.
    #[tokio::test]
    async fn soft_delete_stamps_deleted_at_and_leaves_the_row_in_place() {
        let ctx = TestContext::with_products().await;
        seed(&ctx, "live", None).await;

        soft_delete(&ctx, "live").await.expect("soft delete");

        let raw = db::get(&ctx, TABLE, "live")
            .await
            .expect("the row must still exist");
        assert!(
            !raw.str_field("deleted_at").is_empty(),
            "deleted_at must be stamped"
        );
        let err = get(&ctx, "live").await.expect_err("must not resolve as live");
        assert_eq!(err.code, wafer_run::ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn soft_delete_of_a_missing_row_is_not_found() {
        let ctx = TestContext::with_products().await;
        let err = soft_delete(&ctx, "missing")
            .await
            .expect_err("nothing to delete");
        assert_eq!(err.code, wafer_run::ErrorCode::NotFound);
    }

    /// The unique index added in migration 005 on `(owner_kind, owner_id,
    /// slug)` is partial on `deleted_at IS NULL`, so soft-deleting a product
    /// must free its slug for reuse rather than leaving it permanently
    /// claimed.
    #[tokio::test]
    async fn soft_delete_frees_the_slug_for_reuse() {
        let ctx = TestContext::with_products().await;
        create(
            &ctx,
            HashMap::from([
                ("id".to_string(), json!("first")),
                ("name".to_string(), json!("first")),
                ("status".to_string(), json!("active")),
                ("slug".to_string(), json!("jacket")),
            ]),
        )
        .await
        .expect("create first");

        soft_delete(&ctx, "first").await.expect("soft delete");

        create(
            &ctx,
            HashMap::from([
                ("id".to_string(), json!("second")),
                ("name".to_string(), json!("second")),
                ("status".to_string(), json!("active")),
                ("slug".to_string(), json!("jacket")),
            ]),
        )
        .await
        .expect("the freed slug must not conflict");
    }

    #[tokio::test]
    async fn restore_brings_a_deleted_product_back() {
        let ctx = TestContext::with_products().await;
        seed(&ctx, "oops", None).await;
        soft_delete(&ctx, "oops").await.expect("delete");

        restore(&ctx, "oops").await.expect("restore");

        assert!(get(&ctx, "oops").await.is_ok());
    }
}
