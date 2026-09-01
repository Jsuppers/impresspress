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

pub(crate) const TABLE: &str = "impresspress__products__products";

// Column invariant — `deleted_at` holds exactly two kinds of value:
//
//   * SQL NULL         — the product is live;
//   * an RFC3339 stamp — the instant it was soft-deleted.
//
// It is never the empty string, and never any other non-timestamp text. The
// only writers are `soft_delete` (a fresh `now_rfc3339()`) and `restore`
// (`Value::Null`); every handler that forwards a caller-supplied body strips
// the field first (`handlers::product::INTERNAL_FIELDS`), which
// `no_handler_path_can_write_an_empty_deleted_at` in
// `tests/seller_governance_tests.rs` pins end-to-end across all four
// create/update handlers. The invariant matters because `''` would otherwise be a
// third state: SQL (and so `live_filter`, `list_deleted`, and migration
// 005's partial unique slug index) reads it as deleted, while any
// string-emptiness check reads it as live.
/// `deleted_at IS NULL` — the predicate that distinguishes a live product
/// from a soft-deleted one.
pub(crate) fn live_filter() -> Filter {
    Filter {
        field: "deleted_at".to_string(),
        operator: FilterOp::IsNull,
        value: Value::Null,
    }
}

/// `deleted_at IS NOT NULL` — [`live_filter`]'s exact complement.
fn deleted_filter() -> Filter {
    Filter {
        field: "deleted_at".to_string(),
        operator: FilterOp::IsNotNull,
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

/// Fetch one soft-deleted product. A live (or missing) row answers
/// `NotFound` — the mirror image of [`get`].
///
/// Shares [`list_deleted`]'s named exception to this module's live-only
/// rule, and exists for the same reason: `restore` re-claims the row's slug
/// against the partial unique index from migration 005, so its caller has to
/// be able to read the slug of a row that `get` cannot see, in order to say
/// which slug collided instead of surfacing the index violation as a 500.
pub(crate) async fn get_deleted(ctx: &dyn Context, id: &str) -> Result<Record, WaferError> {
    let record = db::get(ctx, TABLE, id).await?;
    if !is_deleted(&record) {
        return Err(WaferError::new(ErrorCode::NotFound, "Product not deleted"));
    }
    Ok(record)
}

/// List one page of soft-deleted products. `filters` narrows the deleted
/// set; it cannot widen it — same append-only contract as `list_page`.
///
/// This is the one deliberate, named exception to this module's live-only
/// rule: every other read here is unreachable for a soft-deleted row by
/// design, which is exactly why an admin needs one door that finds them —
/// otherwise a deleted product could never be located in order to restore
/// it, making soft delete permanent in practice.
pub(crate) async fn list_deleted(
    ctx: &dyn Context,
    page: i64,
    page_size: i64,
    mut filters: Vec<Filter>,
    sort: Option<Vec<SortField>>,
) -> Result<RecordList, WaferError> {
    filters.push(deleted_filter());
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

/// List every product matching `filters` *regardless of soft-delete state*,
/// unpaged.
///
/// The second deliberate, named exception to this module's live-only rule,
/// alongside [`list_deleted`]/[`get_deleted`] — and the only one that spans
/// both sets at once.
///
/// It exists for seller suspension. Suspending a seller is a lifecycle and
/// fraud operation, so it must cover every row that seller owns: soft delete
/// touches nothing in Stripe, so a deleted product's Prices and Payment Links
/// are still live in the connected account and still take money. Reading that
/// set through [`list_all`] silently exempted exactly the deleted rows from
/// the archival guarantee, which is the opposite of what a fraud control is
/// for.
///
/// Not a general-purpose escape hatch: a read that merely *displays* products
/// wants [`list_all`]. Use this only where "every row the owner has" is the
/// actual requirement.
pub(crate) async fn list_all_including_deleted(
    ctx: &dyn Context,
    filters: Vec<Filter>,
) -> Result<Vec<Record>, WaferError> {
    db::list_all(ctx, TABLE, filters).await
}

/// Fetch one product regardless of soft-delete state.
///
/// Third and last named exception, for the same reason as
/// [`list_all_including_deleted`] and reached from it: archiving a product's
/// Stripe catalog needs the row's `owner_kind`/`owner_id` (to address the
/// connected account) and its `stripe_product_id`, and has to keep working
/// once the product is soft-deleted — that is precisely when its catalog most
/// needs taking down. The read cannot leak a deleted product to anyone,
/// because archival only ever *removes* things from the live Stripe catalog.
///
/// Not for handlers. A read that decides whether a caller may see or edit a
/// product wants [`get`], which answers `NotFound` for a deleted row.
pub(crate) async fn get_including_deleted(
    ctx: &dyn Context,
    id: &str,
) -> Result<Record, WaferError> {
    db::get(ctx, TABLE, id).await
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

/// Selects exactly one row by id. Paired with [`live_filter`] to make a
/// by-id write conditional on the row still being live.
fn id_filter(id: &str) -> Filter {
    Filter {
        field: "id".to_string(),
        operator: FilterOp::Equal,
        value: Value::String(id.to_string()),
    }
}

/// Update one product *if it is still live*, in a single round trip.
///
/// [`update`] is unconditional, so a caller wanting "only while it is live"
/// would have to [`get`] first — and the gap between that read and the write
/// is a window a concurrent delete fits through, after which the write lands
/// on an already-deleted row and reports success. Making `deleted_at IS NULL`
/// part of the write's own `WHERE` closes it: the row's state at write time
/// is what decides, and zero rows affected is `NotFound`.
pub(crate) async fn update_live(
    ctx: &dyn Context,
    id: &str,
    data: HashMap<String, Value>,
) -> Result<Record, WaferError> {
    let updated =
        db::update_by_filters_count(ctx, TABLE, vec![id_filter(id), live_filter()], data).await?;
    if updated == 0 {
        return Err(WaferError::new(ErrorCode::NotFound, "Product not found"));
    }
    // Re-read for the response body: a filtered update reports how many rows
    // it touched, not what they now hold. Raw rather than `get`, because the
    // write has already established that the row was live when it happened —
    // answering `NotFound` here for a delete that landed afterwards would
    // deny a change that did take effect.
    db::get(ctx, TABLE, id).await
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
/// A filtered update rather than `get` + `update`: `get` refuses to find a
/// soft-deleted row by design, but restoring one is the one operation in this
/// module that must act on exactly that row.
///
/// The filter is `deleted_at IS NOT NULL`, so restoring a product that was
/// never deleted matches zero rows and issues no write at all.
/// `DbExec::update` stamps a fresh `updated_at` on every write, and the admin
/// product list sorts on the timestamps — an operation that changed nothing
/// must not reorder it.
pub(crate) async fn restore(ctx: &dyn Context, id: &str) -> Result<Record, WaferError> {
    let data = HashMap::from([("deleted_at".to_string(), Value::Null)]);
    // Zero rows affected means either "already live" (a no-op) or "no such
    // product". `db::get` below tells those apart, answering `NotFound` only
    // for the second — the same two responses this function has always given.
    db::update_by_filters_count(ctx, TABLE, vec![id_filter(id), deleted_filter()], data).await?;
    db::get(ctx, TABLE, id).await
}

// The single definition of "deleted" for one already-loaded row, and the
// exact per-record twin of [`live_filter`]'s `deleted_at IS NULL`: a row is
// deleted iff its `deleted_at` is not SQL NULL.
//
// Deliberately reads `record.data` rather than `RecordExt::str_field`.
// `str_field` collapses a missing key, a JSON `Null` and the empty string all
// to `""`, so it cannot tell NULL from `''` — and SQL can: `'' IS NOT NULL`.
// A string-emptiness check therefore calls a `deleted_at = ''` row live while
// every list/count read (and the partial unique slug index from migration
// 005, which is also keyed on `deleted_at IS NULL`) calls it deleted. Reading
// the raw value keeps both sides on SQL's answer. A missing key and
// `Value::Null` are the two shapes an unset column decodes to on either
// backend, and both mean NULL.
fn is_deleted(record: &Record) -> bool {
    !matches!(record.data.get("deleted_at"), None | Some(Value::Null))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::{test_support::TestContext, util::RecordExt};

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
    async fn list_deleted_returns_only_soft_deleted_rows() {
        let ctx = TestContext::with_products().await;
        seed(&ctx, "live", None).await;
        seed(&ctx, "gone", Some("2026-09-01T00:00:00Z")).await;
        let list = list_deleted(&ctx, 1, 50, vec![], None)
            .await
            .expect("list_deleted");
        let ids: Vec<&str> = list.records.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["gone"]);
    }

    // Mirrors `caller_filters_are_added_to_the_soft_delete_filter` for the
    // inverse predicate: a caller filter that matches a live row must not
    // let that live row leak into the deleted view.
    #[tokio::test]
    async fn list_deleted_caller_filters_do_not_admit_live_rows() {
        let ctx = TestContext::with_products().await;
        seed(&ctx, "live", None).await;
        let status_active = Filter {
            field: "status".to_string(),
            operator: FilterOp::Equal,
            value: json!("active"),
        };
        let list = list_deleted(&ctx, 1, 50, vec![status_active], None)
            .await
            .expect("list_deleted");
        assert!(
            list.records.is_empty(),
            "a live active row must not appear in the deleted view"
        );
    }

    // `live_filter()` (`deleted_at IS NULL`) is the predicate every list and
    // count read hands to SQL, so the per-record check `get`/`get_deleted`
    // apply has to be the same predicate — otherwise "live" means one thing
    // to a single-row read and another to a listing. An empty-string
    // `deleted_at` is precisely where the two can disagree: SQL says
    // `'' IS NOT NULL`, while a string-emptiness check says "live".
    #[tokio::test]
    async fn an_empty_deleted_at_reads_the_same_way_through_every_door() {
        let ctx = TestContext::with_products().await;
        seed(&ctx, "blank", Some("")).await;

        let get_says_live = get(&ctx, "blank").await.is_ok();
        let list_says_live = !list_page(&ctx, 1, 50, vec![], None)
            .await
            .expect("list")
            .records
            .is_empty();
        assert_eq!(
            get_says_live, list_says_live,
            "`get` and `list_page` must classify the same row the same way"
        );

        let get_deleted_resolves = get_deleted(&ctx, "blank").await.is_ok();
        let list_deleted_shows_it = !list_deleted(&ctx, 1, 50, vec![], None)
            .await
            .expect("list_deleted")
            .records
            .is_empty();
        assert_eq!(
            get_deleted_resolves, list_deleted_shows_it,
            "`get_deleted` and `list_deleted` must classify the same row the same way"
        );

        // And the shared answer is SQL's: `'' IS NOT NULL`, so the row is
        // deleted. Pinning the direction as well as the agreement stops a
        // future "fix" from collapsing both onto the wrong side, where a
        // `''`-stamped row would list as live while the partial unique slug
        // index (`WHERE deleted_at IS NULL`) still claims its slug.
        assert!(!get_says_live, "an empty `deleted_at` is not NULL, so the row is deleted");
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
        let err = get(&ctx, "live")
            .await
            .expect_err("must not resolve as live");
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

    /// Restoring a product that was never deleted changes nothing, so it
    /// must not stamp a fresh `updated_at`: `DbExec::update` moves the
    /// column on every write, and the admin product list sorts on the
    /// timestamps. A no-op has no business reordering it.
    #[tokio::test]
    async fn restoring_a_live_product_does_not_stamp_a_new_updated_at() {
        let ctx = TestContext::with_products().await;
        seed(&ctx, "live", None).await;
        let before = db::get(&ctx, TABLE, "live")
            .await
            .expect("seeded")
            .str_field("updated_at")
            .to_string();

        // Same reason as `update_stamps_a_new_updated_at`: make the
        // "unchanged" assertion robust against coarse clock resolution
        // instead of relying on two `Utc::now()` calls differing.
        tokio::time::sleep(Duration::from_millis(5)).await;

        restore(&ctx, "live").await.expect("restoring a live row is a no-op");

        assert_eq!(
            db::get(&ctx, TABLE, "live")
                .await
                .expect("still there")
                .str_field("updated_at"),
            before,
            "a restore that restored nothing must not move updated_at"
        );
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
