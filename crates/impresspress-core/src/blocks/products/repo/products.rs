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
use wafer_run::{context::Context, ErrorCode, OutputStream, WaferError};

use crate::{
    http::{err_internal, err_not_found},
    util::RecordExt,
};

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
) -> Result<RecordList, OutputStream> {
    filters.push(live_filter());
    let sort = sort.unwrap_or_else(|| {
        vec![SortField {
            field: "created_at".to_string(),
            desc: true,
        }]
    });
    db::paginated_list(ctx, TABLE, page, page_size, filters, sort)
        .await
        .map_err(|e| err_internal("Database error", e))
}

/// Count live products matching `filters`.
pub(crate) async fn count(ctx: &dyn Context, filters: &[Filter]) -> Result<i64, WaferError> {
    let mut all = filters.to_vec();
    all.push(live_filter());
    db::count(ctx, TABLE, &all).await
}

pub(crate) async fn create(
    ctx: &dyn Context,
    data: HashMap<String, Value>,
) -> Result<Record, OutputStream> {
    db::create(ctx, TABLE, data)
        .await
        .map_err(|e| err_internal("Database error", e))
}

pub(crate) async fn update(
    ctx: &dyn Context,
    id: &str,
    data: HashMap<String, Value>,
) -> Result<Record, OutputStream> {
    match db::update(ctx, TABLE, id, data).await {
        Ok(record) => Ok(record),
        Err(e) if e.code == ErrorCode::NotFound => Err(err_not_found("Product not found")),
        Err(e) => Err(err_internal("Database error", e)),
    }
}

/// Hard-delete a row. Reserved for rolling back a product that failed
/// mid-creation and was never visible to anyone — see the cleanup path in
/// `handlers/product.rs`. Not the delete a user's action reaches.
pub(crate) async fn purge(ctx: &dyn Context, id: &str) -> Result<(), WaferError> {
    db::delete(ctx, TABLE, id).await
}

// The DB layer maps a NULL text column to the empty string on read, so a
// live row is either absent or empty here — checking both keeps this correct
// on SQLite/D1 and Postgres alike.
fn is_deleted(record: &Record) -> bool {
    !record.str_field("deleted_at").is_empty()
}

#[cfg(test)]
mod tests {
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
        // `list_page` returns `Result<_, OutputStream>` and `OutputStream`
        // does not implement `Debug`, so `Result::expect` won't compile here;
        // route through `Option::expect` (no `Debug` bound) instead. Same
        // assertion, different plumbing.
        let list = list_page(&ctx, 1, 50, vec![], None)
            .await
            .ok()
            .expect("list");
        let ids: Vec<&str> = list.records.iter().map(|r| r.id.as_str()).collect();
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
            .ok()
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
}
