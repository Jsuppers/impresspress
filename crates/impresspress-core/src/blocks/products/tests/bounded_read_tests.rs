//! What the products block does once a table outgrows a single unpaged read.
//!
//! Every test here seeds past `db_read::UNPAGED_LIMIT`, because that is the
//! only size at which the bug these fixes close is visible: below the
//! ceiling, a row scan and a `GROUP BY` agree. The rows go in through
//! `exec_raw` — fixture setup, the documented exception to the no-raw-SQL
//! rule — because ten thousand `db::create` round-trips would turn a
//! sub-second test into a minute of service dispatch.

use wafer_core::clients::database as db;

use super::harness::*;
use crate::{blocks::products::repo, db_read, test_support::TestContext};

/// One more row than a single unpaged read can return.
const PAST_THE_CEILING: i64 = db_read::UNPAGED_LIMIT + 1;

/// Insert `PAST_THE_CEILING` completed USD orders of `total_cents` each,
/// numbered `ord_000001` upward so `id` ordering is the insertion order.
async fn seed_orders_past_the_ceiling(ctx: &TestContext, total_cents: i64) {
    db::exec_raw(
        ctx,
        "WITH RECURSIVE seq(n) AS ( \
             SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < ? \
         ) \
         INSERT INTO impresspress__products__purchases \
             (id, user_id, status, currency, total_cents, refunded_total_cents, \
              platform_fee_cents, created_at, updated_at) \
         SELECT 'ord_' || printf('%06d', n), 'buyer', 'completed', 'USD', ?, 0, 0, \
                '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z' \
         FROM seq",
        &[
            serde_json::json!(PAST_THE_CEILING),
            serde_json::json!(total_cents),
        ],
    )
    .await
    .expect("seed orders");
}

/// Gross volume is the sum over EVERY paid order, not over the first page of
/// them.
///
/// The scan this replaced added `total_cents` up in Rust over an unpaged
/// read, so on a table this size it reported the ceiling's worth of revenue
/// and nothing said the figure was short.
#[tokio::test]
async fn gross_volume_counts_every_order_past_the_unpaged_ceiling() {
    let ctx = ctx().await;
    seed_orders_past_the_ceiling(&ctx, 100).await;

    let analytics = repo::purchases::commerce_analytics(&ctx, None)
        .await
        .expect("analytics");

    assert_eq!(analytics.len(), 1, "one currency");
    assert_eq!(
        analytics[0].gross_volume_minor,
        PAST_THE_CEILING * 100,
        "gross volume must cover all {PAST_THE_CEILING} orders"
    );
    assert_eq!(analytics[0].paid_order_count, PAST_THE_CEILING as u64);
    assert_eq!(analytics[0].order_count, PAST_THE_CEILING as u64);

    // The read the totals used to be summed over, run here so the assertion
    // above is not taking the ceiling on trust: it stops one row short, and
    // adding its rows up in Rust — which is exactly what the old code did —
    // reaches a different, smaller figure.
    let unpaged = db::list_all(&ctx, repo::purchases::PURCHASES_TABLE, vec![])
        .await
        .expect("unpaged read");
    assert_eq!(unpaged.len() as i64, db_read::UNPAGED_LIMIT);
    let scanned: i64 = unpaged
        .iter()
        .map(|record| crate::util::RecordExt::i64_field(record, "total_cents"))
        .sum();
    assert_eq!(scanned, db_read::UNPAGED_LIMIT * 100);
    assert_ne!(
        scanned, analytics[0].gross_volume_minor,
        "the row scan this replaced is short by the truncated tail"
    );
}

/// The order count the analytics publishes and the one `count_all` publishes
/// are the same number.
///
/// They sat side by side on the admin dashboard and disagreed by exactly the
/// truncated tail, because one was a `COUNT(*)` and the other was
/// `rows.len()` of a capped read.
#[tokio::test]
async fn the_analytics_order_count_agrees_with_count_all() {
    let ctx = ctx().await;
    seed_orders_past_the_ceiling(&ctx, 250).await;

    let analytics = repo::purchases::commerce_analytics(&ctx, None)
        .await
        .expect("analytics");
    let counted = repo::purchases::count_all(&ctx).await.expect("count");

    assert_eq!(analytics[0].order_count as i64, counted);
}

/// Top products are summed over the line items of every paid order.
#[tokio::test]
async fn top_products_cover_every_paid_order_past_the_ceiling() {
    let ctx = ctx().await;
    seed_orders_past_the_ceiling(&ctx, 100).await;
    db::exec_raw(
        &ctx,
        "WITH RECURSIVE seq(n) AS ( \
             SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < ? \
         ) \
         INSERT INTO impresspress__products__line_items \
             (id, purchase_id, product_id, product_name, quantity, total_minor, \
              created_at, updated_at) \
         SELECT 'li_' || printf('%06d', n), 'ord_' || printf('%06d', n), \
                'prod_1', 'Widget', 1, 100, \
                '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z' \
         FROM seq",
        &[serde_json::json!(PAST_THE_CEILING)],
    )
    .await
    .expect("seed line items");

    let analytics = repo::purchases::commerce_analytics(&ctx, None)
        .await
        .expect("analytics");

    assert_eq!(
        db::list_all(&ctx, repo::purchases::LINE_ITEMS_TABLE, vec![])
            .await
            .expect("unpaged read")
            .len() as i64,
        db_read::UNPAGED_LIMIT,
        "the line-item read this replaced stops one row short"
    );
    let top = &analytics[0].top_products;
    assert_eq!(top.len(), 1, "one product");
    assert_eq!(top[0].product_id, "prod_1");
    assert_eq!(top[0].quantity, PAST_THE_CEILING as u64);
    assert_eq!(top[0].revenue_minor, PAST_THE_CEILING * 100);
}

/// Suspension acts on every product the seller owns, however many that is.
///
/// A row this read stopped short of is a product whose Stripe Prices and
/// Payment Links the suspension never archives — they keep taking money in
/// the connected account.
#[tokio::test]
async fn suspension_reads_every_product_the_seller_owns() {
    let ctx = ctx().await;
    db::exec_raw(
        &ctx,
        "WITH RECURSIVE seq(n) AS ( \
             SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < ? \
         ) \
         INSERT INTO impresspress__products__products \
             (id, name, status, owner_kind, owner_id, created_at, updated_at) \
         SELECT 'prod_' || printf('%06d', n), 'Listing', 'active', 'user', 'seller_1', \
                '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z' \
         FROM seq",
        &[serde_json::json!(PAST_THE_CEILING)],
    )
    .await
    .expect("seed products");

    let owned = repo::products::list_owned_by_including_deleted(&ctx, "seller_1")
        .await
        .expect("owned products");

    assert_eq!(owned.len(), PAST_THE_CEILING as usize);
    assert_eq!(
        db::list_all(&ctx, repo::products::TABLE, vec![])
            .await
            .expect("unpaged read")
            .len() as i64,
        db_read::UNPAGED_LIMIT,
        "the read this replaced stops one product short"
    );
}

/// A seller listing that is showing a prefix says so, and publishes the real
/// population beside it.
#[tokio::test]
async fn the_seller_listing_reports_that_it_is_a_prefix() {
    let ctx = ctx().await;
    db::exec_raw(
        &ctx,
        "WITH RECURSIVE seq(n) AS ( \
             SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < ? \
         ) \
         INSERT INTO impresspress__products__seller_accounts \
             (id, user_id, status, created_at, updated_at) \
         SELECT 'acct_' || printf('%06d', n), 'user_' || printf('%06d', n), 'active', \
                '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z' \
         FROM seq",
        &[serde_json::json!(PAST_THE_CEILING)],
    )
    .await
    .expect("seed sellers");

    let listed = repo::seller_accounts::list_contracts(&ctx)
        .await
        .expect("sellers");

    assert!(listed.truncated, "the listing is a prefix and must say so");
    assert_eq!(listed.rows.len(), db_read::UNPAGED_LIMIT as usize);
    assert_eq!(
        repo::seller_accounts::count_all(&ctx).await.expect("count"),
        PAST_THE_CEILING
    );
}
