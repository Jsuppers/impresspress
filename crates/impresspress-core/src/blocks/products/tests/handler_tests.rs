use std::collections::HashMap;

use base64ct::{Base64, Encoding};
use wafer_run::ErrorCode;

use super::harness::*;

// ============================================================
// Admin Product CRUD
// ============================================================

#[tokio::test]
async fn admin_create_product() {
    let ctx = ctx().await;
    let (msg, input) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({
            "name": "Cloud Hosting",
            "description": "Managed hosting",
            "currency": "USD"
        }),
    );

    let out = dispatch_admin(&ctx, msg, input).await;
    let body = output_to_json(out).await;
    assert!(body["id"].as_str().is_some());
    assert_eq!(body["data"]["name"], "Cloud Hosting");
    assert_eq!(body["data"]["status"], "draft");
    assert_eq!(body["data"]["created_by"], "admin_1");
}

#[tokio::test]
async fn admin_list_products() {
    let ctx = ctx().await;

    // Create two products
    let (msg1, input1) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({
            "name": "Product A"
        }),
    );
    dispatch_admin(&ctx, msg1, input1).await;
    let (msg2, input2) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({
            "name": "Product B"
        }),
    );
    dispatch_admin(&ctx, msg2, input2).await;

    let (list_msg, list_input) = admin_get_msg("/admin/b/products/products");
    let out = dispatch_admin(&ctx, list_msg, list_input).await;
    let body = output_to_json(out).await;
    assert!(body["records"].as_array().unwrap().len() >= 2);
}

#[tokio::test]
async fn admin_get_product() {
    let ctx = ctx().await;

    let (create_msg_data, create_input) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({
            "name": "Widget"
        }),
    );
    let create_out = dispatch_admin(&ctx, create_msg_data, create_input).await;
    let id = output_to_json(create_out).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (get_msg_data, get_input) = admin_get_msg(&format!("/admin/b/products/products/{id}"));
    let out = dispatch_admin(&ctx, get_msg_data, get_input).await;
    let body = output_to_json(out).await;
    assert_eq!(body["data"]["name"], "Widget");
}

#[tokio::test]
async fn admin_update_product() {
    let ctx = ctx().await;

    let (create, create_input) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({
            "name": "Old Name"
        }),
    );
    let create_out = dispatch_admin(&ctx, create, create_input).await;
    let id = output_to_json(create_out).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (mut update, update_input) = request_msg(
        "update",
        &format!("/admin/b/products/products/{id}"),
        "admin_1",
        serde_json::json!({
            "name": "New Name"
        }),
    );
    update.set_meta("auth.user_roles", "admin");
    let out = dispatch_admin(&ctx, update, update_input).await;
    let body = output_to_json(out).await;
    assert_eq!(body["data"]["name"], "New Name");
}

#[tokio::test]
async fn admin_delete_product() {
    let ctx = ctx().await;

    let (create, create_input) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({
            "name": "To Delete"
        }),
    );
    let create_out = dispatch_admin(&ctx, create, create_input).await;
    let id = output_to_json(create_out).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (mut del, del_input) = delete_msg(&format!("/admin/b/products/products/{id}"), "admin_1");
    del.set_meta("auth.user_roles", "admin");
    let out = dispatch_admin(&ctx, del, del_input).await;
    let body = output_to_json(out).await;
    assert_eq!(body["deleted"], true);

    // Verify it's gone
    let (get, get_input) = admin_get_msg(&format!("/admin/b/products/products/{id}"));
    let out = dispatch_admin(&ctx, get, get_input).await;
    assert!(output_is_error(out, ErrorCode::NotFound).await);
}

/// A soft-deleted product must 404 from the admin detail endpoint the same
/// as one that never existed — `handle_get_product` used to call
/// `db::get` with the table's old hardcoded constant directly, bypassing
/// the soft-delete filter entirely.
#[tokio::test]
async fn admin_product_detail_404s_for_a_soft_deleted_product() {
    let ctx = ctx().await;

    let mut gone = HashMap::new();
    gone.insert("name".to_string(), serde_json::json!("Gone"));
    gone.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "gone", gone).await;
    soft_delete_product(&ctx, "gone").await;

    let (msg, input) = admin_get_msg("/admin/b/products/products/gone");
    let out = dispatch_admin(&ctx, msg, input).await;
    assert!(output_is_error(out, ErrorCode::NotFound).await);
}

/// `handle_update_product` (the generic admin PATCH) must not be a second
/// door onto `deleted_at`: an admin sending `{"deleted_at": null}` for a
/// soft-deleted product must not silently resurrect it — restore is the
/// only door back in. The refusal now comes from the body check, before the
/// write's own liveness filter is reached, so it reads as `InvalidArgument`
/// rather than `NotFound`; either way the row stays deleted.
#[tokio::test]
async fn admin_update_product_does_not_resurrect_via_deleted_at_null() {
    let ctx = ctx().await;

    let (create, create_input) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({ "name": "Oops" }),
    );
    let create_out = dispatch_admin(&ctx, create, create_input).await;
    let id = output_to_json(create_out).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    soft_delete_product(&ctx, &id).await;

    let (mut update, update_input) = request_msg(
        "update",
        &format!("/admin/b/products/products/{id}"),
        "admin_1",
        serde_json::json!({ "deleted_at": null }),
    );
    update.set_meta("auth.user_roles", "admin");
    let out = dispatch_admin(&ctx, update, update_input).await;
    assert!(
        output_is_error(out, ErrorCode::InvalidArgument).await,
        "an admin PATCH must not resurrect a soft-deleted product"
    );

    let err = super::super::repo::products::get(&ctx, &id)
        .await
        .expect_err("the product must still read as deleted");
    assert_eq!(err.code, ErrorCode::NotFound);
}

/// A soft-deleted product must be restored before it is editable again — the
/// generic admin PATCH must refuse it outright rather than silently applying
/// unrelated field changes to a dead row.
#[tokio::test]
async fn admin_update_product_refuses_a_soft_deleted_product() {
    let ctx = ctx().await;

    let (create, create_input) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({ "name": "Oops" }),
    );
    let create_out = dispatch_admin(&ctx, create, create_input).await;
    let id = output_to_json(create_out).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    soft_delete_product(&ctx, &id).await;

    let (mut update, update_input) = request_msg(
        "update",
        &format!("/admin/b/products/products/{id}"),
        "admin_1",
        serde_json::json!({ "name": "New Name" }),
    );
    update.set_meta("auth.user_roles", "admin");
    let out = dispatch_admin(&ctx, update, update_input).await;
    assert!(
        output_is_error(out, ErrorCode::NotFound).await,
        "a soft-deleted product must not be editable through the normal admin PATCH"
    );
}

/// `deleted_at` is `soft_delete`'s door, not the generic PATCH's: even for a
/// still-live product (so the liveness guard above doesn't reject the
/// request outright), an admin PATCH carrying `deleted_at` must not be able
/// to soft-delete it as a side effect of an otherwise ordinary field update.
///
/// The whole request is refused, rather than the field dropped and the rest
/// applied: a 200 whose body plainly shows `deleted_at` unchanged tells the
/// caller their write succeeded when part of it was discarded.
#[tokio::test]
async fn admin_update_product_refuses_deleted_at_in_the_request_body() {
    let ctx = ctx().await;

    let (create, create_input) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({ "name": "Still Live" }),
    );
    let create_out = dispatch_admin(&ctx, create, create_input).await;
    let id = output_to_json(create_out).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (mut update, update_input) = request_msg(
        "update",
        &format!("/admin/b/products/products/{id}"),
        "admin_1",
        serde_json::json!({ "name": "New Name", "deleted_at": "2026-09-01T00:00:00Z" }),
    );
    update.set_meta("auth.user_roles", "admin");
    let out = dispatch_admin(&ctx, update, update_input).await;
    assert!(
        output_is_error(out, ErrorCode::InvalidArgument).await,
        "a PATCH naming deleted_at must be refused, not partly applied"
    );

    let record = super::super::repo::products::get(&ctx, &id)
        .await
        .expect("the generic PATCH must not have soft-deleted the product");
    assert!(crate::util::RecordExt::str_field(&record, "deleted_at").is_empty());
    assert_eq!(
        crate::util::RecordExt::str_field(&record, "name"),
        "Still Live",
        "a refused request must not apply its other fields either"
    );
}

/// The bug this whole plan exists for: `line_items.product_id` is `TEXT NOT
/// NULL`, so a hard delete of a product that was ever ordered orphaned that
/// order's line item. Soft delete must leave both rows resolvable.
#[tokio::test]
async fn admin_delete_keeps_the_row_and_its_order_history_resolvable() {
    let ctx = ctx().await;

    let mut sold = HashMap::new();
    sold.insert("name".to_string(), serde_json::json!("Sold"));
    sold.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "sold", sold).await;

    let mut order = HashMap::new();
    order.insert("user_id".to_string(), serde_json::json!("user_1"));
    order.insert("status".to_string(), serde_json::json!("completed"));
    seed(&ctx, "impresspress__products__purchases", "order_1", order).await;
    seed(
        &ctx,
        "impresspress__products__line_items",
        "line_1",
        HashMap::from([
            ("purchase_id".to_string(), serde_json::json!("order_1")),
            ("product_id".to_string(), serde_json::json!("sold")),
            ("product_name".to_string(), serde_json::json!("Sold")),
        ]),
    )
    .await;

    let (mut del, del_input) = delete_msg("/admin/b/products/products/sold", "admin_1");
    del.set_meta("auth.user_roles", "admin");
    let body = output_to_json(dispatch_admin(&ctx, del, del_input).await).await;
    assert_eq!(body["deleted"], true);

    let row = wafer_core::clients::database::get(&ctx, "impresspress__products__products", "sold")
        .await
        .expect("the row must still exist");
    assert!(
        !crate::util::RecordExt::str_field(&row, "deleted_at").is_empty(),
        "deleted_at must be stamped"
    );

    let line_item =
        wafer_core::clients::database::get(&ctx, "impresspress__products__line_items", "line_1")
            .await
            .expect("the line item must still resolve");
    assert_eq!(
        crate::util::RecordExt::str_field(&line_item, "product_id"),
        "sold"
    );
}

/// A deleted product must disappear from the public catalog end-to-end
/// through the real delete handler, not just when `deleted_at` is stamped
/// by hand.
#[tokio::test]
async fn admin_delete_removes_the_product_from_the_catalog() {
    let ctx = ctx().await;

    let mut sold = HashMap::new();
    sold.insert("name".to_string(), serde_json::json!("Sold"));
    sold.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "sold", sold).await;

    let (mut del, del_input) = delete_msg("/admin/b/products/products/sold", "admin_1");
    del.set_meta("auth.user_roles", "admin");
    dispatch_admin(&ctx, del, del_input).await;

    let (msg, input) = get_msg("/b/products/catalog", "");
    let body = output_to_json(dispatch_user(&ctx, msg, input).await).await;
    assert!(body["records"].as_array().unwrap().is_empty());
}

/// A soft-deleted product frees its slug, because the unique index added in
/// migration 005 is partial on `deleted_at IS NULL`.
#[tokio::test]
async fn admin_delete_frees_the_products_slug() {
    let ctx = ctx().await;

    let mut first = HashMap::new();
    first.insert("name".to_string(), serde_json::json!("First"));
    first.insert("slug".to_string(), serde_json::json!("jacket"));
    seed(&ctx, "impresspress__products__products", "first", first).await;

    let (mut del, del_input) = delete_msg("/admin/b/products/products/first", "admin_1");
    del.set_meta("auth.user_roles", "admin");
    dispatch_admin(&ctx, del, del_input).await;

    let (create, create_input) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({
            "name": "Second",
            "slug": "jacket"
        }),
    );
    let body = output_to_json(dispatch_admin(&ctx, create, create_input).await).await;
    assert_eq!(
        body["data"]["slug"], "jacket",
        "the reused slug must not conflict"
    );
}

// ============================================================
// Admin Group CRUD
// ============================================================

#[tokio::test]
async fn admin_create_and_list_groups() {
    let ctx = ctx().await;

    let (create, create_input) = admin_create_msg(
        "/admin/b/products/groups",
        serde_json::json!({
            "name": "Electronics"
        }),
    );
    let out = dispatch_admin(&ctx, create, create_input).await;
    let body = output_to_json(out).await;
    assert_eq!(body["data"]["name"], "Electronics");
    assert_eq!(body["data"]["user_id"], "admin_1");

    let (list, list_input) = admin_get_msg("/admin/b/products/groups");
    let list_out = dispatch_admin(&ctx, list, list_input).await;
    let list_body = output_to_json(list_out).await;
    assert_eq!(list_body["records"].as_array().unwrap().len(), 1);
}

// ============================================================
// Admin Types CRUD
// ============================================================

#[tokio::test]
async fn admin_create_and_list_types() {
    let ctx = ctx().await;

    let (create, create_input) = admin_create_msg(
        "/admin/b/products/types",
        serde_json::json!({
            "name": "subscription", "display_name": "Subscription"
        }),
    );
    dispatch_admin(&ctx, create, create_input).await;

    let (list, list_input) = admin_get_msg("/admin/b/products/types");
    let out = dispatch_admin(&ctx, list, list_input).await;
    let body = output_to_json(out).await;
    assert_eq!(body["records"].as_array().unwrap().len(), 1);
}

// ============================================================
// Admin Stats
// ============================================================

#[tokio::test]
async fn admin_stats() {
    let ctx = ctx().await;

    // Seed some products
    let mut data = HashMap::new();
    data.insert("name".to_string(), serde_json::json!("Active Product"));
    data.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "p1", data).await;

    let mut data2 = HashMap::new();
    data2.insert("name".to_string(), serde_json::json!("Draft Product"));
    data2.insert("status".to_string(), serde_json::json!("draft"));
    seed(&ctx, "impresspress__products__products", "p2", data2).await;

    // Seed a completed purchase (user_id is NOT NULL in the real schema)
    let mut purchase_data = HashMap::new();
    purchase_data.insert("user_id".to_string(), serde_json::json!("user_1"));
    purchase_data.insert("status".to_string(), serde_json::json!("completed"));
    purchase_data.insert("total_cents".to_string(), serde_json::json!(2999));
    seed(
        &ctx,
        "impresspress__products__purchases",
        "pur1",
        purchase_data,
    )
    .await;

    let (msg, input) = admin_get_msg("/admin/b/products/stats");
    let out = dispatch_admin(&ctx, msg, input).await;
    let body = output_to_json(out).await;
    assert_eq!(body["total_products"].as_i64().unwrap(), 2);
    assert_eq!(body["active_products"].as_i64().unwrap(), 1);
    assert_eq!(body["total_purchases"].as_i64().unwrap(), 1);
    assert_eq!(body["currency_analytics"][0]["gross_volume_minor"], 2999);
}

#[tokio::test]
async fn admin_stats_never_combine_currencies() {
    let ctx = ctx().await;
    for (id, currency, total) in [("order_nzd", "NZD", 2500), ("order_usd", "USD", 1900)] {
        seed(
            &ctx,
            "impresspress__products__purchases",
            id,
            HashMap::from([
                ("user_id".to_string(), serde_json::json!("buyer_stats")),
                ("status".to_string(), serde_json::json!("completed")),
                ("currency".to_string(), serde_json::json!(currency)),
                ("total_cents".to_string(), serde_json::json!(total)),
            ]),
        )
        .await;
    }
    for (purchase_id, dispute_id, status, currency, amount) in [
        ("order_nzd", "dp_admin_nzd", "needs_response", "NZD", 700),
        ("order_usd", "dp_admin_usd", "lost", "USD", 900),
    ] {
        crate::blocks::products::repo::disputes::reconcile(
            &ctx,
            &crate::blocks::products::repo::disputes::DisputeSnapshot {
                purchase_id: purchase_id.to_string(),
                seller_account_id: String::new(),
                stripe_account_id: String::new(),
                provider_dispute_id: dispute_id.to_string(),
                provider_charge_id: format!("ch_{dispute_id}"),
                payment_intent_id: format!("pi_{dispute_id}"),
                status: status.to_string(),
                amount_minor: amount,
                currency: currency.to_string(),
                reason: "fraudulent".to_string(),
                evidence_due_by: None,
                livemode: false,
                event_created: 1_750_000_000,
            },
        )
        .await
        .unwrap();
    }

    let (msg, input) = admin_get_msg("/admin/b/products/stats");
    let body = output_to_json(dispatch_admin(&ctx, msg, input).await).await;
    let analytics = body["currency_analytics"].as_array().unwrap();
    assert_eq!(analytics.len(), 2);
    assert_eq!(analytics[0]["currency"], "NZD");
    assert_eq!(analytics[0]["gross_volume_minor"], 2500);
    assert_eq!(analytics[0]["open_dispute_count"], 1);
    assert_eq!(analytics[0]["open_disputed_volume_minor"], 700);
    assert_eq!(analytics[0]["lost_dispute_count"], 0);
    assert_eq!(analytics[1]["currency"], "USD");
    assert_eq!(analytics[1]["gross_volume_minor"], 1900);
    assert_eq!(analytics[1]["open_dispute_count"], 0);
    assert_eq!(analytics[1]["lost_dispute_count"], 1);
    assert_eq!(analytics[1]["lost_disputed_volume_minor"], 900);
}

#[tokio::test]
async fn seller_stats_orders_and_refunds_are_tenant_isolated() {
    let ctx = ctx_with(&[("WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS", "true")]).await;
    for (id, user_id) in [
        ("seller_stats_1", "seller_user_1"),
        ("seller_stats_2", "seller_user_2"),
    ] {
        seed(
            &ctx,
            crate::blocks::products::repo::seller_accounts::TABLE,
            id,
            HashMap::from([
                ("user_id".to_string(), serde_json::json!(user_id)),
                ("status".to_string(), serde_json::json!("active")),
            ]),
        )
        .await;
    }
    for (id, seller_account_id, buyer, total) in [
        ("seller_order_1", "seller_stats_1", "buyer_1", 4200),
        ("seller_order_2", "seller_stats_2", "buyer_2", 9900),
    ] {
        seed(
            &ctx,
            "impresspress__products__purchases",
            id,
            HashMap::from([
                ("user_id".to_string(), serde_json::json!(buyer)),
                ("buyer_user_id".to_string(), serde_json::json!(buyer)),
                (
                    "seller_account_id".to_string(),
                    serde_json::json!(seller_account_id),
                ),
                ("status".to_string(), serde_json::json!("completed")),
                ("currency".to_string(), serde_json::json!("NZD")),
                ("total_cents".to_string(), serde_json::json!(total)),
                ("provider".to_string(), serde_json::json!("manual")),
            ]),
        )
        .await;
    }
    for (purchase_id, seller_account_id, dispute_id, status, amount) in [
        (
            "seller_order_1",
            "seller_stats_1",
            "dp_seller_stats_1",
            "under_review",
            700,
        ),
        (
            "seller_order_2",
            "seller_stats_2",
            "dp_seller_stats_2",
            "lost",
            900,
        ),
    ] {
        crate::blocks::products::repo::disputes::reconcile(
            &ctx,
            &crate::blocks::products::repo::disputes::DisputeSnapshot {
                purchase_id: purchase_id.to_string(),
                seller_account_id: seller_account_id.to_string(),
                stripe_account_id: format!("acct_{seller_account_id}"),
                provider_dispute_id: dispute_id.to_string(),
                provider_charge_id: format!("ch_{dispute_id}"),
                payment_intent_id: format!("pi_{dispute_id}"),
                status: status.to_string(),
                amount_minor: amount,
                currency: "NZD".to_string(),
                reason: "fraudulent".to_string(),
                evidence_due_by: None,
                livemode: false,
                event_created: 1_750_000_000,
            },
        )
        .await
        .unwrap();
    }
    seed(
        &ctx,
        "impresspress__products__line_items",
        "seller_line_1",
        HashMap::from([
            (
                "purchase_id".to_string(),
                serde_json::json!("seller_order_1"),
            ),
            (
                "product_id".to_string(),
                serde_json::json!("seller_product_1"),
            ),
            (
                "product_name".to_string(),
                serde_json::json!("Seller One Product"),
            ),
            ("quantity".to_string(), serde_json::json!(2)),
            ("total_minor".to_string(), serde_json::json!(4200)),
        ]),
    )
    .await;
    for (id, seller_account_id, error) in [
        (
            "seller_failure_own",
            "seller_stats_1",
            "Own card payment failed",
        ),
        (
            "seller_failure_other",
            "seller_stats_2",
            "Other seller failure",
        ),
    ] {
        seed(
            &ctx,
            "impresspress__products__purchases",
            id,
            HashMap::from([
                ("user_id".to_string(), serde_json::json!("buyer")),
                (
                    "seller_account_id".to_string(),
                    serde_json::json!(seller_account_id),
                ),
                ("status".to_string(), serde_json::json!("failed")),
                ("currency".to_string(), serde_json::json!("NZD")),
                ("total_cents".to_string(), serde_json::json!(1800)),
                ("reconciliation_error".to_string(), serde_json::json!(error)),
            ]),
        )
        .await;
    }
    for (id, seller_account_id, error) in [
        (
            "seller_pi_failure_own",
            "seller_stats_1",
            "Own PaymentIntent needs another payment method",
        ),
        (
            "seller_pi_failure_other",
            "seller_stats_2",
            "Other seller PaymentIntent failed",
        ),
    ] {
        seed(
            &ctx,
            "impresspress__products__purchases",
            id,
            HashMap::from([
                ("user_id".to_string(), serde_json::json!("buyer")),
                (
                    "seller_account_id".to_string(),
                    serde_json::json!(seller_account_id),
                ),
                ("status".to_string(), serde_json::json!("checkout_started")),
                ("currency".to_string(), serde_json::json!("NZD")),
                ("total_cents".to_string(), serde_json::json!(1600)),
                (
                    "provider_payment_status".to_string(),
                    serde_json::json!("payment_failed"),
                ),
                (
                    "provider_payment_error_message".to_string(),
                    serde_json::json!(error),
                ),
            ]),
        )
        .await;
    }

    let (msg, input) = get_msg("/b/products/seller/stats", "seller_user_1");
    let stats = output_to_json(dispatch_user(&ctx, msg, input).await).await;
    assert_eq!(stats["seller_account_id"], "seller_stats_1");
    assert_eq!(stats["currency_analytics"][0]["gross_volume_minor"], 4200);
    assert_eq!(stats["currency_analytics"][0]["open_dispute_count"], 1);
    assert_eq!(
        stats["currency_analytics"][0]["open_disputed_volume_minor"],
        700
    );
    assert_eq!(stats["currency_analytics"][0]["lost_dispute_count"], 0);
    assert_eq!(
        stats["currency_analytics"][0]["lost_disputed_volume_minor"],
        0
    );
    assert_eq!(
        stats["currency_analytics"][0]["top_products"][0]["product_id"],
        "seller_product_1"
    );
    let recent_failures = stats["recent_failures"].as_array().unwrap();
    assert_eq!(recent_failures.len(), 2);
    let terminal_failure = recent_failures
        .iter()
        .find(|failure| failure["order_id"] == "seller_failure_own")
        .unwrap();
    assert_eq!(terminal_failure["error"], "Own card payment failed");
    let payment_failure = recent_failures
        .iter()
        .find(|failure| failure["order_id"] == "seller_pi_failure_own")
        .unwrap();
    assert_eq!(
        payment_failure["error"],
        "Own PaymentIntent needs another payment method"
    );
    assert!(recent_failures
        .iter()
        .all(|failure| failure.get("buyer_email").is_none()));
    assert!(!recent_failures
        .iter()
        .any(|failure| failure["order_id"] == "seller_pi_failure_other"));

    let (msg, input) = get_msg("/b/products/seller/orders", "seller_user_1");
    let orders = output_to_json(dispatch_user(&ctx, msg, input).await).await;
    assert_eq!(orders["total_count"], 3);
    let order_ids = orders["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|order| order["id"].as_str())
        .collect::<Vec<_>>();
    assert!(order_ids.contains(&"seller_order_1"));
    assert!(order_ids.contains(&"seller_failure_own"));
    assert!(order_ids.contains(&"seller_pi_failure_own"));
    assert!(!order_ids.contains(&"seller_order_2"));
    assert!(!order_ids.contains(&"seller_failure_other"));
    assert!(!order_ids.contains(&"seller_pi_failure_other"));

    let (msg, input) = get_msg("/b/products/seller/orders/seller_order_2", "seller_user_1");
    assert!(
        output_is_error(
            dispatch_user(&ctx, msg, input).await,
            ErrorCode::PermissionDenied
        )
        .await
    );

    let (msg, input) = create_msg(
        "/b/products/seller/orders/seller_order_2/refund",
        "seller_user_1",
        serde_json::json!({"amount_minor": 1000}),
    );
    assert!(
        output_is_error(
            dispatch_user(&ctx, msg, input).await,
            ErrorCode::PermissionDenied
        )
        .await
    );

    let (msg, input) = create_msg(
        "/b/products/seller/orders/seller_order_1/refund",
        "seller_user_1",
        serde_json::json!({"amount_minor": 1200, "note": "Customer request"}),
    );
    let refund = output_to_json(dispatch_user(&ctx, msg, input).await).await;
    assert_eq!(refund["amount_minor"], 1200);
    assert_eq!(refund["refunded_total_minor"], 1200);
}

/// CODE_REVIEW_2026-07-16 "Error semantics fabricate successful defaults":
/// a genuine repository failure on any of the 5 independent stat
/// counts/sums must surface as an error, not be reported as "0 products /
/// $0 revenue" — an admin reading zeroed stats during a real outage would
/// mistake a broken dashboard for real (empty) business data.
/// `unwrap_or(0)` / `unwrap_or(0.0)` used to do exactly that.
#[tokio::test]
async fn admin_stats_repository_failure_surfaces_as_internal_error() {
    let ctx = ctx().await.break_reads();

    let (msg, input) = admin_get_msg("/admin/b/products/stats");
    let out = dispatch_admin(&ctx, msg, input).await;
    assert!(
        output_is_error(out, ErrorCode::Internal).await,
        "a genuine repository failure must surface as Internal, not a fabricated all-zero stats body"
    );
}

/// Two counters read the products table with no filter at all (`total_products`
/// and `active_products`), so a soft-deleted product would still be counted —
/// both the admin dashboard and this stats endpoint would overstate the
/// catalog.
#[tokio::test]
async fn stats_do_not_count_soft_deleted_products() {
    let ctx = ctx().await;

    let mut live = HashMap::new();
    live.insert("name".to_string(), serde_json::json!("Live"));
    live.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "live", live).await;

    let mut gone = HashMap::new();
    gone.insert("name".to_string(), serde_json::json!("Gone"));
    gone.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "gone", gone).await;
    soft_delete_product(&ctx, "gone").await;

    let (msg, input) = admin_get_msg("/admin/b/products/stats");
    let body = output_to_json(dispatch_admin(&ctx, msg, input).await).await;
    assert_eq!(body["total_products"].as_i64().unwrap(), 1);
    assert_eq!(body["active_products"].as_i64().unwrap(), 1);
}

// ============================================================
// User Product CRUD — ownership isolation
// ============================================================

async fn user_products_ctx() -> crate::test_support::TestContext {
    ctx_with(&[("WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS", "true")]).await
}

#[tokio::test]
async fn user_create_product_in_own_group() {
    let ctx = user_products_ctx().await;

    // Create a group for user_1
    let (create_group, cg_input) = create_msg(
        "/b/products/groups",
        "user_1",
        serde_json::json!({
            "name": "My Store"
        }),
    );
    let group_out = dispatch_user(&ctx, create_group, cg_input).await;
    let group_id = output_to_json(group_out).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Create a product in that group
    let (create_prod, cp_input) = create_msg(
        "/b/products/products",
        "user_1",
        serde_json::json!({
            "name": "Widget",
            "group_id": group_id
        }),
    );
    let out = dispatch_user(&ctx, create_prod, cp_input).await;
    let body = output_to_json(out).await;
    assert_eq!(body["data"]["name"], "Widget");
    assert_eq!(body["data"]["created_by"], "user_1");
}

#[tokio::test]
async fn user_cannot_create_product_in_other_users_group() {
    let ctx = user_products_ctx().await;

    // Create a group for user_1
    let (create_group, cg_input) = create_msg(
        "/b/products/groups",
        "user_1",
        serde_json::json!({
            "name": "User1 Store"
        }),
    );
    let group_out = dispatch_user(&ctx, create_group, cg_input).await;
    let group_id = output_to_json(group_out).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    // user_2 tries to create a product in user_1's group
    let (create_prod, cp_input) = create_msg(
        "/b/products/products",
        "user_2",
        serde_json::json!({
            "name": "Sneaky Product",
            "group_id": group_id
        }),
    );
    let out = dispatch_user(&ctx, create_prod, cp_input).await;
    assert!(output_is_error(out, ErrorCode::InvalidArgument).await);
}

#[tokio::test]
async fn user_cannot_see_other_users_products() {
    let ctx = user_products_ctx().await;

    // user_1 creates a product
    let (create, create_input) = create_msg(
        "/b/products/products",
        "user_1",
        serde_json::json!({
            "name": "Private Product"
        }),
    );
    let create_out = dispatch_user(&ctx, create, create_input).await;
    let prod_id = output_to_json(create_out).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    // user_2 tries to get it
    let (get, get_input) = get_msg(&format!("/b/products/products/{prod_id}"), "user_2");
    let out = dispatch_user(&ctx, get, get_input).await;
    assert!(output_is_error(out, ErrorCode::NotFound).await);
}

#[tokio::test]
async fn user_cannot_update_other_users_products() {
    let ctx = user_products_ctx().await;

    let (create, create_input) = create_msg(
        "/b/products/products",
        "user_1",
        serde_json::json!({
            "name": "My Product"
        }),
    );
    let create_out = dispatch_user(&ctx, create, create_input).await;
    let prod_id = output_to_json(create_out).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (update, update_input) = update_msg(
        &format!("/b/products/products/{prod_id}"),
        "user_2",
        serde_json::json!({
            "name": "Hijacked!"
        }),
    );
    let out = dispatch_user(&ctx, update, update_input).await;
    assert!(output_is_error(out, ErrorCode::NotFound).await);
}

#[tokio::test]
async fn user_cannot_delete_other_users_products() {
    let ctx = user_products_ctx().await;

    let (create, create_input) = create_msg(
        "/b/products/products",
        "user_1",
        serde_json::json!({
            "name": "My Product"
        }),
    );
    let create_out = dispatch_user(&ctx, create, create_input).await;
    let prod_id = output_to_json(create_out).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (del, del_input) = delete_msg(&format!("/b/products/products/{prod_id}"), "user_2");
    let out = dispatch_user(&ctx, del, del_input).await;
    assert!(output_is_error(out, ErrorCode::NotFound).await);
}

/// The seller's own-product delete is the path a non-admin actually uses:
/// leaving it hard-deleting would orphan `line_items.product_id` (`TEXT NOT
/// NULL`) on exactly the path this task exists to fix.
#[tokio::test]
async fn user_delete_own_product_soft_deletes_instead_of_hard_deleting() {
    let ctx = user_products_ctx().await;

    let (create, create_input) = create_msg(
        "/b/products/products",
        "user_1",
        serde_json::json!({
            "name": "My Product"
        }),
    );
    let create_out = dispatch_user(&ctx, create, create_input).await;
    let prod_id = output_to_json(create_out).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (del, del_input) = delete_msg(&format!("/b/products/products/{prod_id}"), "user_1");
    let body = output_to_json(dispatch_user(&ctx, del, del_input).await).await;
    assert_eq!(body["deleted"], true);

    let row =
        wafer_core::clients::database::get(&ctx, "impresspress__products__products", &prod_id)
            .await
            .expect("the row must still exist");
    assert!(
        !crate::util::RecordExt::str_field(&row, "deleted_at").is_empty(),
        "deleted_at must be stamped"
    );
}

#[tokio::test]
async fn user_list_only_own_products() {
    let ctx = user_products_ctx().await;

    // user_1 creates a product
    let (c1, c1_input) = create_msg(
        "/b/products/products",
        "user_1",
        serde_json::json!({"name": "U1 Product"}),
    );
    dispatch_user(&ctx, c1, c1_input).await;

    // user_2 creates a product
    let (c2, c2_input) = create_msg(
        "/b/products/products",
        "user_2",
        serde_json::json!({"name": "U2 Product"}),
    );
    dispatch_user(&ctx, c2, c2_input).await;

    // user_1 lists — should only see their own
    let (list, list_input) = get_msg("/b/products/products", "user_1");
    let out = dispatch_user(&ctx, list, list_input).await;
    let body = output_to_json(out).await;
    let records = body["records"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["data"]["name"], "U1 Product");
}

#[tokio::test]
async fn user_update_prevents_ownership_change() {
    let ctx = user_products_ctx().await;

    let (create, create_input) = create_msg(
        "/b/products/products",
        "user_1",
        serde_json::json!({"name": "Mine"}),
    );
    let create_out = dispatch_user(&ctx, create, create_input).await;
    let prod_id = output_to_json(create_out).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Try to change created_by — the whole request must be refused, not
    // silently reduced to the fields the caller does own.
    let (update, update_input) = update_msg(
        &format!("/b/products/products/{prod_id}"),
        "user_1",
        serde_json::json!({
            "name": "Updated",
            "created_by": "attacker"
        }),
    );
    let out = dispatch_user(&ctx, update, update_input).await;
    assert!(output_is_error(out, ErrorCode::InvalidArgument).await);

    let record = super::super::repo::products::get(&ctx, &prod_id)
        .await
        .expect("the product is still there");
    assert_eq!(
        crate::util::RecordExt::str_field(&record, "created_by"),
        "user_1"
    );
    assert_eq!(crate::util::RecordExt::str_field(&record, "name"), "Mine");
}

// ============================================================
// User Group CRUD — ownership isolation
// ============================================================

#[tokio::test]
async fn user_list_only_own_groups() {
    let ctx = user_products_ctx().await;

    let (g1, g1_input) = create_msg(
        "/b/products/groups",
        "user_1",
        serde_json::json!({"name": "U1 Group"}),
    );
    dispatch_user(&ctx, g1, g1_input).await;

    let (g2, g2_input) = create_msg(
        "/b/products/groups",
        "user_2",
        serde_json::json!({"name": "U2 Group"}),
    );
    dispatch_user(&ctx, g2, g2_input).await;

    let (list, list_input) = get_msg("/b/products/groups", "user_1");
    let out = dispatch_user(&ctx, list, list_input).await;
    let body = output_to_json(out).await;
    let records = body["records"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["data"]["name"], "U1 Group");
}

#[tokio::test]
async fn user_cannot_update_other_users_group() {
    let ctx = user_products_ctx().await;

    let (create, create_input) = create_msg(
        "/b/products/groups",
        "user_1",
        serde_json::json!({"name": "My Group"}),
    );
    let create_out = dispatch_user(&ctx, create, create_input).await;
    let group_id = output_to_json(create_out).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (update, update_input) = update_msg(
        &format!("/b/products/groups/{group_id}"),
        "user_2",
        serde_json::json!({
            "name": "Stolen"
        }),
    );
    let out = dispatch_user(&ctx, update, update_input).await;
    assert!(output_is_error(out, ErrorCode::NotFound).await);
}

#[tokio::test]
async fn user_group_update_prevents_ownership_change() {
    let ctx = user_products_ctx().await;

    let (create, create_input) = create_msg(
        "/b/products/groups",
        "user_1",
        serde_json::json!({"name": "My Group"}),
    );
    let create_out = dispatch_user(&ctx, create, create_input).await;
    let group_id = output_to_json(create_out).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (update, update_input) = update_msg(
        &format!("/b/products/groups/{group_id}"),
        "user_1",
        serde_json::json!({
            "name": "Renamed",
            "user_id": "attacker"
        }),
    );
    let out = dispatch_user(&ctx, update, update_input).await;
    let body = output_to_json(out).await;
    assert_eq!(body["data"]["user_id"], "user_1");
}

// ============================================================
// Public Catalog
// ============================================================

#[tokio::test]
async fn catalog_only_shows_active_products() {
    let ctx = ctx().await;

    let mut d1 = HashMap::new();
    d1.insert("name".to_string(), serde_json::json!("Active"));
    d1.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "p_active", d1).await;

    let mut d2 = HashMap::new();
    d2.insert("name".to_string(), serde_json::json!("Draft"));
    d2.insert("status".to_string(), serde_json::json!("draft"));
    seed(&ctx, "impresspress__products__products", "p_draft", d2).await;

    let (msg, input) = get_msg("/b/products/catalog", "");
    let out = dispatch_user(&ctx, msg, input).await;
    let body = output_to_json(out).await;
    let records = body["records"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["data"]["name"], "Active");
}

#[tokio::test]
async fn catalog_get_hides_non_active() {
    let ctx = ctx().await;

    let mut d = HashMap::new();
    d.insert("name".to_string(), serde_json::json!("Hidden"));
    d.insert("status".to_string(), serde_json::json!("draft"));
    seed(&ctx, "impresspress__products__products", "p_hidden", d).await;

    let (msg, input) = get_msg("/b/products/catalog/p_hidden", "");
    let out = dispatch_user(&ctx, msg, input).await;
    assert!(output_is_error(out, ErrorCode::NotFound).await);
}

// ============================================================
// Soft-deleted products stay off every customer-facing surface
// ============================================================
//
// The catalog historically filtered on `status` alone, so a soft-deleted
// product that was still `active` stayed listed and purchasable. This is
// the hole soft delete would otherwise open; these tests pin that a
// soft-deleted row is invisible on every customer-facing read.

/// Mark a product soft-deleted the way the (future) soft-delete path will:
/// writing `deleted_at` directly, bypassing any handler.
async fn soft_delete_product(ctx: &crate::test_support::TestContext, id: &str) {
    wafer_core::clients::database::update(
        ctx,
        super::super::repo::products::TABLE,
        id,
        HashMap::from([(
            "deleted_at".to_string(),
            serde_json::json!("2026-09-01T00:00:00Z"),
        )]),
    )
    .await
    .expect("soft delete");
}

/// Whether `id`'s row still carries a `deleted_at` stamp, read straight from
/// the table.
///
/// Not `repo::products::get`, which cannot see a soft-deleted row at all and
/// so cannot tell "still deleted" from "never existed" — the distinction
/// every restore-authorization assertion below turns on.
async fn is_soft_deleted(ctx: &crate::test_support::TestContext, id: &str) -> bool {
    use crate::util::RecordExt;

    let record = wafer_core::clients::database::get(ctx, super::super::repo::products::TABLE, id)
        .await
        .expect("the product row must still exist");
    !record.str_field("deleted_at").is_empty()
}

/// Build an active, approved product with one published offer through the
/// same repo functions `stripe::handle_checkout` calls, so checkout has a
/// real purchasable offer to refuse once the product is soft-deleted.
async fn seed_published_offer(ctx: &crate::test_support::TestContext, product_id: &str) -> String {
    let mut data = HashMap::new();
    data.insert("name".to_string(), serde_json::json!("Checkout product"));
    data.insert("status".to_string(), serde_json::json!("active"));
    seed(ctx, "impresspress__products__products", product_id, data).await;

    let definition: super::super::contracts::OfferDefinitionRequest =
        serde_json::from_value(serde_json::json!({
            "name": "Plan",
            "mode": "payment",
            "currency": "usd",
            "pricing_model": "fixed",
            "usage_type": "licensed",
            "billing_scheme": "per_unit",
            "tax_behavior": "exclusive",
            "components": [{
                "key": "price",
                "label": "Plan",
                "required": true,
                "amount": {"type": "fixed", "unit_amount_minor": 1000}
            }]
        }))
        .expect("offer definition");
    let offer = super::super::repo::offers::create(ctx, product_id, "admin_1", &definition)
        .await
        .expect("create offer");
    super::super::repo::offers::publish(ctx, product_id, &offer.offer.id)
        .await
        .expect("publish offer");
    offer.offer.id
}

#[tokio::test]
async fn catalog_list_omits_a_soft_deleted_active_product() {
    let ctx = ctx().await;

    let mut keep = HashMap::new();
    keep.insert("name".to_string(), serde_json::json!("Keep"));
    keep.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "keep", keep).await;

    let mut gone = HashMap::new();
    gone.insert("name".to_string(), serde_json::json!("Gone"));
    gone.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "gone", gone).await;
    soft_delete_product(&ctx, "gone").await;

    let (msg, input) = get_msg("/b/products/catalog", "");
    let out = dispatch_user(&ctx, msg, input).await;
    let body = output_to_json(out).await;
    let ids: Vec<&str> = body["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["keep"]);
}

#[tokio::test]
async fn catalog_detail_404s_for_a_soft_deleted_active_product() {
    let ctx = ctx().await;

    let mut gone = HashMap::new();
    gone.insert("name".to_string(), serde_json::json!("Gone"));
    gone.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "gone", gone).await;
    soft_delete_product(&ctx, "gone").await;

    let (msg, input) = get_msg("/b/products/catalog/gone", "");
    let out = dispatch_user(&ctx, msg, input).await;
    assert!(output_is_error(out, ErrorCode::NotFound).await);
}

/// Characterisation test, not a regression check: `repo::offers::get_public`
/// already refuses a soft-deleted product's offer today (before this task's
/// migration), so this must PASS before and after. It pins the behaviour the
/// migration must not lose, since checkout stops going through the table's
/// old hardcoded constant directly once this task lands.
#[tokio::test]
async fn checkout_refuses_a_soft_deleted_product() {
    let ctx = ctx_with(&[("IMPRESSPRESS__PRODUCTS__STRIPE_SECRET_KEY", "sk_test_x")]).await;
    let offer_id = seed_published_offer(&ctx, "gone").await;
    soft_delete_product(&ctx, "gone").await;

    let (msg, input) = create_msg(
        "/b/products/checkout",
        "",
        serde_json::json!({ "offer_id": offer_id }),
    );
    let out = super::super::stripe::handle_checkout(&ctx, &msg, input).await;
    assert!(
        output_is_error(out, ErrorCode::NotFound).await,
        "checkout must refuse a soft-deleted product's offer"
    );
}

// ============================================================
// Restoring a soft-deleted product — the door back out
// ============================================================
//
// Soft delete without a way back is worse than the hard delete it replaced:
// a deleted row would be permanently unreachable by any UI. These tests pin
// the restore endpoint's two obligations: it must clear `deleted_at` on the
// right row, and the restored row must be visible again everywhere a live
// product is visible (the public catalog, here — `repo::products::get`'s
// own restore test already covers the repo layer directly).

#[tokio::test]
async fn restore_endpoint_returns_the_product_to_the_catalog() {
    let ctx = ctx().await;

    let mut oops = HashMap::new();
    oops.insert("name".to_string(), serde_json::json!("oops"));
    oops.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "oops", oops).await;
    soft_delete_product(&ctx, "oops").await;

    let (msg, input) = admin_create_msg(
        "/admin/b/products/products/oops/restore",
        serde_json::json!({}),
    );
    let body = output_to_json(dispatch_admin(&ctx, msg, input).await).await;
    assert_eq!(body["id"], "oops");
    assert!(
        body["data"]["deleted_at"].is_null(),
        "restore must clear deleted_at: {body}"
    );

    let (catalog_msg, catalog_input) = get_msg("/b/products/catalog", "");
    let catalog_body = output_to_json(dispatch_user(&ctx, catalog_msg, catalog_input).await).await;
    assert_eq!(catalog_body["records"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn restore_endpoint_404s_for_an_unknown_product_id() {
    let ctx = ctx().await;

    let (msg, input) = admin_create_msg(
        "/admin/b/products/products/missing/restore",
        serde_json::json!({}),
    );
    let out = dispatch_admin(&ctx, msg, input).await;
    assert!(output_is_error(out, ErrorCode::NotFound).await);
}

/// The live-product case the old `restore_endpoint_404s_for_a_product_that_
/// was_never_deleted` claimed to cover and did not: it created a product,
/// ignored its id, and posted restore for the literal id `"missing"`, so it
/// only ever exercised the unknown-id path above.
///
/// Restoring a product that was never deleted is a no-op that answers 200
/// with the record — clearing an already-null `deleted_at` changes nothing.
/// Pinned rather than "fixed" because nothing reaches this endpoint except
/// the Deleted view's Restore button, which only renders for rows that ARE
/// deleted; see the report accompanying this branch for why a 409 here would
/// be defensible but is not this wave's change.
#[tokio::test]
async fn restore_endpoint_is_a_no_op_for_a_product_that_was_never_deleted() {
    let ctx = ctx().await;

    let (create, create_input) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({ "name": "Live" }),
    );
    let created = output_to_json(dispatch_admin(&ctx, create, create_input).await).await;
    let id = created["id"]
        .as_str()
        .expect("created product id")
        .to_string();

    let (msg, input) = admin_create_msg(
        &format!("/admin/b/products/products/{id}/restore"),
        serde_json::json!({}),
    );
    let body = output_to_json(dispatch_admin(&ctx, msg, input).await).await;
    assert_eq!(
        body["id"], id,
        "restoring a live product answers 200 with it: {body}"
    );
    assert!(
        !is_soft_deleted(&ctx, &id).await,
        "the product must stay live"
    );
}

/// Restore is the only endpoint outside `/b/products/api/admin/` that is
/// declared `Admin`, and it must be unreachable for a non-admin through
/// EVERY wire path that reaches its handler — not merely through the one
/// path its declaration happens to match.
///
/// `ProductsBlock::handle` enters `handle_user` from two prefixes: the
/// `/b/products/api`-stripped one and the raw `/b/products/...` one. A
/// `USER_ROUTES` entry therefore answers at two spellings while
/// `declared_access` only ever matches the declared one, so restore declared
/// `Admin` at `/b/products/api/products/{id}/restore` left
/// `POST /b/products/products/{id}/restore` resolving to the undeclared
/// fallback tier (`Authenticated`): any logged-in user could resurrect any
/// soft-deleted product — straight back into the public catalog, and
/// purchasable, when it was active/approved. Product ids are not secret;
/// `/b/products/catalog` hands them out.
///
/// Driven through `dispatch_routed` (the real `route_to_block`) because that
/// is where the tier is enforced — a `dispatch_user` test cannot see this
/// boundary at all, which is exactly how the escalation shipped.
#[tokio::test]
async fn restore_is_unreachable_for_a_non_admin_on_every_path_that_reaches_it() {
    let ctx = ctx().await;

    let mut gone = HashMap::new();
    gone.insert("name".to_string(), serde_json::json!("gone"));
    gone.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "gone", gone).await;
    soft_delete_product(&ctx, "gone").await;

    for path in [
        "/b/products/products/gone/restore",
        "/b/products/api/products/gone/restore",
        "/b/products/api/admin/products/gone/restore",
    ] {
        let (msg, input) = create_msg(path, "user_1", serde_json::json!({}));
        let out = dispatch_routed(&ctx, msg, input).await;
        assert!(
            out.collect_buffered().await.is_err(),
            "a non-admin POST to {path} must not succeed"
        );
        assert!(
            is_soft_deleted(&ctx, "gone").await,
            "a non-admin POST to {path} restored a soft-deleted product"
        );
    }

    // Positive control: the declared admin path DOES restore through the
    // same router, so the assertions above cannot be passing merely because
    // nothing routes anywhere.
    let (msg, input) = admin_create_msg(
        "/b/products/api/admin/products/gone/restore",
        serde_json::json!({}),
    );
    let body = output_to_json(dispatch_routed(&ctx, msg, input).await).await;
    assert_eq!(
        body["id"], "gone",
        "an admin must be able to restore: {body}"
    );
    assert!(!is_soft_deleted(&ctx, "gone").await);
}

/// Soft delete frees the product's slug (migration 005's unique index is
/// partial on `deleted_at IS NULL`), and nothing stops a product created
/// afterwards from claiming it. Restoring the original then violates
/// `impresspress__products__products_owner_slug_uniq`.
///
/// That must read as a conflict naming the slug, not an opaque 500: the
/// Deleted view's Restore button only reloads on success, so a 500 renders
/// as nothing happening at all on the only door out of soft delete.
///
/// The conflict is read off the failed write, not from a pre-check ahead of
/// it. A pre-check answers about the moment before the write, so a slug
/// claimed in between produced the very 500 it existed to prevent; the
/// write's own failure cannot be raced, because the row is still deleted and
/// still probeable exactly when the write did not land.
#[tokio::test]
async fn restore_reports_a_slug_conflict_instead_of_an_opaque_error() {
    let ctx = ctx().await;

    let mut original = HashMap::new();
    original.insert("name".to_string(), serde_json::json!("Original"));
    original.insert("slug".to_string(), serde_json::json!("widget"));
    seed(
        &ctx,
        "impresspress__products__products",
        "original",
        original,
    )
    .await;
    soft_delete_product(&ctx, "original").await;

    // Legal only because the delete freed the slug.
    let mut claimant = HashMap::new();
    claimant.insert("name".to_string(), serde_json::json!("Claimant"));
    claimant.insert("slug".to_string(), serde_json::json!("widget"));
    seed(
        &ctx,
        "impresspress__products__products",
        "claimant",
        claimant,
    )
    .await;

    let (msg, input) = admin_create_msg(
        "/admin/b/products/products/original/restore",
        serde_json::json!({}),
    );
    let out = dispatch_admin(&ctx, msg, input).await;
    let error = match out.collect_buffered().await {
        Err(wafer_run::streams::output::TerminalNotResponse::Error(e)) => e,
        other => panic!("restore over a claimed slug must fail: {other:?}"),
    };
    assert_eq!(
        error.code,
        ErrorCode::AlreadyExists,
        "a slug collision is a conflict, not an internal error: {error:?}"
    );
    assert!(
        error.message.contains("widget"),
        "the conflict must name the colliding slug so an admin can act on it: {}",
        error.message
    );
    assert!(
        is_soft_deleted(&ctx, "original").await,
        "a refused restore must leave the product deleted"
    );
}

/// The admin PATCH refuses a soft-deleted product — a deleted row has to go
/// back through `restore` before it is editable again. It enforced that with
/// a separate `get` followed by a separate `update`, which is a guard with a
/// window in it: a delete landing between the two lets the PATCH write to an
/// already-deleted row and answer 200.
///
/// The window is reproduced exactly rather than raced for. The handler awaits
/// the request body between its liveness check and its write, so a body that
/// arrives only after the delete has committed puts the delete precisely
/// where a concurrent one would land.
#[tokio::test]
async fn admin_patch_refuses_a_product_soft_deleted_inside_the_request() {
    use std::sync::Arc;

    let ctx = Arc::new(ctx().await);

    let mut racer = HashMap::new();
    racer.insert("name".to_string(), serde_json::json!("before"));
    racer.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "racer", racer).await;

    let deleting = ctx.clone();
    let input = wafer_run::InputStream::from_stream(futures::stream::once(async move {
        super::super::repo::products::soft_delete(deleting.as_ref(), "racer")
            .await
            .expect("the concurrent delete lands");
        serde_json::to_vec(&serde_json::json!({"name": "after"})).unwrap()
    }));
    let (msg, _) = update_msg(
        "/admin/b/products/products/racer",
        "admin_1",
        serde_json::json!({}),
    );
    let mut msg = msg;
    msg.set_meta("auth.user_roles", "admin");

    let out = dispatch_admin(&ctx, msg, input).await;
    assert!(
        output_is_error(out, ErrorCode::NotFound).await,
        "a PATCH whose row was deleted before the write must 404, not report success"
    );

    let stored = wafer_core::clients::database::get(
        ctx.as_ref(),
        super::super::repo::products::TABLE,
        "racer",
    )
    .await
    .expect("the row still exists");
    assert_eq!(
        stored.data.get("name"),
        Some(&serde_json::json!("before")),
        "the PATCH must not have written to a deleted row"
    );
}

/// The other direction of the same rule: "could not tell" is not "conflict".
/// When the restore write fails and the collision probe that would name the
/// slug cannot itself run, the response must carry the write's real failure —
/// an `Internal` error against a correlation id an admin can quote — rather
/// than a confident 409 blaming a slug nothing has confirmed is taken.
#[tokio::test]
async fn restore_fails_loudly_when_the_slug_collision_probe_cannot_run() {
    let ctx = ctx().await;

    let mut original = HashMap::new();
    original.insert("name".to_string(), serde_json::json!("Original"));
    original.insert("slug".to_string(), serde_json::json!("widget"));
    seed(
        &ctx,
        "impresspress__products__products",
        "original",
        original,
    )
    .await;
    soft_delete_product(&ctx, "original").await;

    // Listings fail while by-id reads still resolve, so the restore write
    // fails and the collision probe's `list_all` fails with it — the exact
    // interleaving a transient database wobble produces, and the only one
    // that reaches the "probe could not tell" branch.
    let ctx = ctx.break_list_reads();

    let (msg, input) = admin_create_msg(
        "/admin/b/products/products/original/restore",
        serde_json::json!({}),
    );
    let out = dispatch_admin(&ctx, msg, input).await;
    assert!(
        output_is_error(out, ErrorCode::Internal).await,
        "a probe that could not run must fail the restore, not be read as a clear slug"
    );
    assert!(
        is_soft_deleted(&ctx, "original").await,
        "a restore that could not check its slug must leave the product deleted"
    );
}

// ============================================================
// Group products endpoint
// ============================================================

#[tokio::test]
async fn user_group_products_list() {
    let ctx = user_products_ctx().await;

    // Create group
    let (cg, cg_input) = create_msg(
        "/b/products/groups",
        "user_1",
        serde_json::json!({"name": "Store"}),
    );
    let gr = dispatch_user(&ctx, cg, cg_input).await;
    let gid = output_to_json(gr).await["id"].as_str().unwrap().to_string();

    // Create product in group
    let (cp, cp_input) = create_msg(
        "/b/products/products",
        "user_1",
        serde_json::json!({
            "name": "In Group",
            "group_id": gid
        }),
    );
    dispatch_user(&ctx, cp, cp_input).await;

    // List products in group
    let (list, list_input) = get_msg(&format!("/b/products/groups/{gid}/products"), "user_1");
    let out = dispatch_user(&ctx, list, list_input).await;
    let body = output_to_json(out).await;
    assert!(!body["records"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn user_cannot_list_other_users_group_products() {
    let ctx = user_products_ctx().await;

    let (cg, cg_input) = create_msg(
        "/b/products/groups",
        "user_1",
        serde_json::json!({"name": "Private"}),
    );
    let gr = dispatch_user(&ctx, cg, cg_input).await;
    let gid = output_to_json(gr).await["id"].as_str().unwrap().to_string();

    // user_2 tries to list user_1's group products
    let (list, list_input) = get_msg(&format!("/b/products/groups/{gid}/products"), "user_2");
    let out = dispatch_user(&ctx, list, list_input).await;
    assert!(output_is_error(out, ErrorCode::NotFound).await);
}

// ============================================================
// User products disabled by default
// ============================================================

#[tokio::test]
async fn user_products_rejected_when_disabled() {
    let ctx = ctx().await; // no ALLOW_USER_PRODUCTS config → defaults to false

    let (create, create_input) = create_msg(
        "/b/products/products",
        "user_1",
        serde_json::json!({"name": "Test"}),
    );
    let out = dispatch_user(&ctx, create, create_input).await;
    assert!(output_is_error(out, ErrorCode::PermissionDenied).await);

    let (list, list_input) = get_msg("/b/products/products", "user_1");
    let out = dispatch_user(&ctx, list, list_input).await;
    assert!(output_is_error(out, ErrorCode::PermissionDenied).await);

    let (group, group_input) = create_msg(
        "/b/products/groups",
        "user_1",
        serde_json::json!({"name": "Group"}),
    );
    let out = dispatch_user(&ctx, group, group_input).await;
    assert!(output_is_error(out, ErrorCode::PermissionDenied).await);
}

#[tokio::test]
async fn catalog_still_works_when_user_products_disabled() {
    let ctx = ctx().await; // user products disabled

    let mut d = std::collections::HashMap::new();
    d.insert("name".to_string(), serde_json::json!("Plan"));
    d.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "p1", d).await;

    let (msg, input) = get_msg("/b/products/catalog", "");
    let out = dispatch_user(&ctx, msg, input).await;
    let body = output_to_json(out).await;
    assert_eq!(body["records"].as_array().unwrap().len(), 1);
}

// ============================================================
// Not-found routes
// ============================================================

#[tokio::test]
async fn unknown_admin_route() {
    let ctx = ctx().await;
    let (msg, input) = admin_get_msg("/admin/b/products/nonexistent");
    let out = dispatch_admin(&ctx, msg, input).await;
    assert!(output_is_error(out, ErrorCode::NotFound).await);
}

#[tokio::test]
async fn unknown_user_route() {
    let ctx = ctx().await;
    let (msg, input) = get_msg("/b/products/nonexistent", "user_1");
    let out = dispatch_user(&ctx, msg, input).await;
    assert!(output_is_error(out, ErrorCode::NotFound).await);
}

// ============================================================
// Page shell (ui::shell_page) + data_table adoption
// ============================================================

/// Commerce administration belongs in the admin shell. Its registered sidebar
/// item and the page's canonical request path must agree so Products is active.
#[tokio::test]
async fn overview_highlights_products_nav_via_request_path() {
    let ctx = ctx().await;
    let (msg, _input) = admin_get_msg("/b/products/admin/");
    let html = output_to_html(super::super::pages::overview(&ctx, &msg).await).await;

    // Full shell chrome present (shell_page wrapped a non-htmx request in the
    // sidebar+topbar document, not a bare fragment). The `.shell` wrapper only
    // exists on the full page, so it's the distinguishing marker.
    assert!(html.contains(r#"class="shell""#), "expected shell chrome");
    assert!(
        html.contains(r#"class="sidebar"#),
        "expected sidebar in full doc"
    );
    // Products admin nav item is active because current_path == its href.
    assert!(
        html.contains(r#"href="/b/products/admin/""#),
        "Products admin nav item should be present"
    );
    assert!(
        html.contains("is-active"),
        "the active sidebar item (Products) should carry is-active"
    );
}

/// When `WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS` is off and the catalog is
/// empty, the Overview page used to render a bare, actionless stat grid — a
/// live 403 on the gated user route (`/b/products/api/products`) was the
/// only signal that self-serve selling was disabled. It must now name the
/// config var and point at Settings, and must NOT show the "Add product"
/// CTA that belongs to the enabled+empty state (that CTA is safe either
/// way — it targets the *admin* create route, which isn't gated by this
/// flag — but the two states render distinct copy, so assert only the
/// disabled-state text appears).
#[tokio::test]
async fn overview_shows_disabled_notice_when_user_products_off() {
    let ctx = ctx().await; // no ALLOW_USER_PRODUCTS config → defaults to false
    let (msg, _input) = admin_get_msg("/b/products/admin/");
    let html = output_to_html(super::super::pages::overview(&ctx, &msg).await).await;

    assert!(
        html.contains("Customer accounts cannot create their own listings yet"),
        "disabled notice should clearly explain the customer-facing effect: {html}"
    );
    assert!(
        html.contains("Settings"),
        "disabled notice should point at how to enable it: {html}"
    );
    assert!(
        html.contains(r#"href="/b/products/admin/settings""#),
        "disabled notice's action should link to the Settings page: {html}"
    );
    assert!(
        !html.contains("Add your first product"),
        "disabled overview should not show the enabled+empty CTA copy: {html}"
    );
}

/// Enabled + empty catalog: the Overview page must show a working
/// "Add your first product" CTA to the real admin create path (Manage
/// Products, which owns the "+ New Product" modal), and must not show the
/// disabled-state notice.
#[tokio::test]
async fn overview_shows_add_product_cta_when_enabled_and_empty() {
    let ctx = ctx_with(&[("WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS", "true")]).await;
    let (msg, _input) = admin_get_msg("/b/products/admin/");
    let html = output_to_html(super::super::pages::overview(&ctx, &msg).await).await;

    assert!(
        html.contains("Add your first product"),
        "enabled+empty overview should show the add-product CTA: {html}"
    );
    assert!(
        html.contains(r#"href="/b/products/admin/manage""#),
        "CTA should link to the real create path (Manage Products): {html}"
    );
    assert!(
        !html.contains("WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS"),
        "enabled overview should not show the disabled-state notice: {html}"
    );
}

/// Once the catalog has products, the empty-state block (CTA or notice)
/// disappears entirely regardless of the enabled flag.
#[tokio::test]
async fn overview_hides_empty_state_once_products_exist() {
    let ctx = ctx_with(&[("WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS", "true")]).await;
    let (c, c_input) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({ "name": "Cloud Hosting" }),
    );
    dispatch_admin(&ctx, c, c_input).await;

    let (msg, _input) = admin_get_msg("/b/products/admin/");
    let html = output_to_html(super::super::pages::overview(&ctx, &msg).await).await;

    assert!(
        !html.contains("Add your first product"),
        "CTA should be gone once the catalog has products: {html}"
    );
}

/// CODE_REVIEW_2026-07-16 "Error semantics fabricate successful defaults": a
/// genuine repository failure on the Overview page's stat counts must
/// surface as an error, not silently render the page with fabricated "0"
/// stats — which would ALSO wrongly trigger the "Add your first product"
/// empty-state CTA during a real outage on a catalog that isn't actually
/// empty.
#[tokio::test]
async fn overview_repository_failure_surfaces_as_internal_error() {
    let ctx = ctx().await.break_reads();
    let (msg, _input) = admin_get_msg("/b/products/admin/");
    let out = super::super::pages::overview(&ctx, &msg).await;
    assert!(
        output_is_error(out, ErrorCode::Internal).await,
        "a genuine repository failure must surface as Internal, not a fabricated empty overview page"
    );
}

/// The catalog's primary action opens the dedicated product wizard rather
/// than the removed name/price-only modal.
#[tokio::test]
async fn manage_products_page_links_to_product_wizard() {
    let ctx = ctx().await;
    let (msg, _input) = admin_get_msg("/b/products/admin/manage");
    let html = output_to_html(super::super::pages::manage_products(&ctx, &msg).await).await;

    assert!(
        html.contains("+ New Product"),
        "manage page should render the create-product trigger: {html}"
    );
    assert!(
        html.contains(r#"href="/b/products/admin/new""#),
        "manage page should link to the full product wizard: {html}"
    );
}

#[tokio::test]
async fn admin_product_wizard_exposes_simple_and_advanced_templates() {
    let ctx = ctx_with(&[
        ("IMPRESSPRESS__PRODUCTS__DEFAULT_CURRENCY", "NZD"),
        ("IMPRESSPRESS__PRODUCTS__AUTOMATIC_TAX", "true"),
        ("IMPRESSPRESS__PRODUCTS__PLATFORM_COUNTRY", "nz"),
    ])
    .await;
    let (msg, _input) = admin_get_msg("/b/products/admin/new");
    let html = output_to_html(super::super::pages::product_wizard(&ctx, &msg, true).await).await;

    for template in [
        "simple_product",
        "simple_subscription",
        "configurable_product",
        "configurable_subscription",
    ] {
        assert!(html.contains(&format!(r#"value="{template}""#)));
    }
    assert!(html.contains("Customer fields"));
    assert!(html.contains("Itemized price rows"));
    assert!(html.contains("Condition"));
    assert!(html.contains("Checkout options"));
    assert!(html.contains("Create and publish"));
    assert!(html.contains(r#"value="NZD""#));
    assert!(html.contains(r#"id="wizard-automatic-tax" type="checkbox" checked"#));
    assert!(html.contains("/b/products/api/admin/products"));
    assert!(html.contains("BigInt"), "money conversion must be exact");
    assert!(html.contains("wizardCurrencyExponent"));
    assert!(html.contains("unit_amount_minor"));
    assert!(html.contains(r#"value="graduated""#));
    assert!(html.contains(r#"value="volume""#));
    assert!(html.contains(r#"value="package""#));
    assert!(html.contains("wizardParseTiers"));
    assert!(html.contains("wizardParseLookup"));
    assert!(html.contains("wizardParseShippingCountries"));
    assert!(html.contains("wizardParseShippingOptions"));
    assert!(html.contains(r#"id="wizard-shipping-countries""#));
    assert!(html.contains(r#"value="NZ""#));
    assert!(html.contains("Inline rates work in hosted and embedded Checkout"));
    assert!(html.contains("Create a Stripe Customer for one-time payments"));
    assert!(html.contains("upper bound | unit amount | flat amount"));
}

#[tokio::test]
async fn seller_product_wizard_reuses_builder_with_seller_routes_and_moderation_copy() {
    let ctx = ctx_with(&[("WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS", "true")]).await;
    let (msg, _input) = get_msg("/b/products/my-products/new", "seller_1");
    let html = output_to_html(super::super::pages::product_wizard(&ctx, &msg, false).await).await;

    assert!(html.contains("Submit for publication"));
    assert!(html.contains("administrator review"));
    assert!(html.contains("/b/products/api/products"));
    assert!(!html.contains("/b/products/api/admin/products"));
    assert!(html.contains(r#"href="/b/products/my-products""#));
}

#[tokio::test]
async fn admin_product_manager_renders_product_offer_lifecycle_and_payment_link_controls() {
    let test_ctx = ctx().await;
    let (msg, input) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({
            "name": "Managed plan",
            "slug": "managed-plan",
            "description": "Lifecycle test",
            "currency": "NZD",
            "fulfillment_kind": "entitlement"
        }),
    );
    let product = output_to_json(dispatch_admin(&test_ctx, msg, input).await).await;
    let product_id = product["id"].as_str().unwrap();
    let offer_collection = format!("/admin/b/products/products/{product_id}/offers");
    let definition = |name: &str| {
        serde_json::json!({
            "name": name,
            "mode": "payment",
            "currency": "NZD",
            "pricing_model": "fixed",
            "interval_count": 1,
            "usage_type": "licensed",
            "billing_scheme": "per_unit",
            "tax_behavior": "exclusive",
            "variables": [],
            "components": [{
                "key": "price",
                "label": name,
                "sort_order": 0,
                "required": true,
                "amount": {"type": "fixed", "unit_amount_minor": 2599},
                "quantity": {"type": "fixed", "value": 1},
                "condition": {"op": "always"}
            }],
            "checkout": {"automatic_tax": true}
        })
    };
    let (msg, input) = admin_create_msg(&offer_collection, definition("Published price"));
    let active = output_to_json(dispatch_admin(&test_ctx, msg, input).await).await;
    let active_id = active["offer"]["id"].as_str().unwrap();
    let (msg, input) = admin_create_msg(
        &format!("{offer_collection}/{active_id}/publish"),
        serde_json::json!({}),
    );
    output_to_json(dispatch_admin(&test_ctx, msg, input).await).await;
    let (msg, input) = admin_create_msg(&offer_collection, definition("Editable price"));
    let draft = output_to_json(dispatch_admin(&test_ctx, msg, input).await).await;
    let draft_id = draft["offer"]["id"].as_str().unwrap();

    let (msg, _input) = admin_get_msg(&format!("/b/products/admin/products/{product_id}"));
    let html = output_to_html(
        super::super::pages::product_manager(&test_ctx, &msg, product_id, true).await,
    )
    .await;

    assert!(html.contains("Managed plan"));
    assert!(html.contains("Save product details"));
    assert!(html.contains("Duplicate product"));
    assert!(html.contains("Published offers are immutable"));
    assert!(html.contains("Advanced draft definition"));
    assert!(html.contains("Duplicate to draft"));
    assert!(html.contains("Sync to Stripe"));
    assert!(html.contains("Shareable Stripe Payment Links"));
    assert!(html.contains("Create or reuse Payment Link"));
    assert!(html.contains("unit_amount_minor"));
    assert!(html.contains(&format!(
        "/b/products/api/admin/products/{product_id}/offers/{draft_id}"
    )));
    assert!(html.contains(&format!(
        "/b/products/api/admin/products/{product_id}/offers/{active_id}/presets"
    )));
    assert!(html.contains(&format!(
        "/b/products/api/admin/products/{product_id}/offers/{active_id}"
    )));
    assert!(html.contains(&format!(
        "/b/products/api/admin/products/{product_id}/offers/{active_id}/payment-links"
    )));
    assert!(html.contains("navigator.clipboard"));

    wafer_core::clients::database::update(
        &test_ctx,
        super::super::repo::offers::TABLE,
        active_id,
        HashMap::from([
            ("sync_status".to_string(), serde_json::json!("failed")),
            (
                "sync_error".to_string(),
                serde_json::json!("Stripe Price response did not match the immutable offer row"),
            ),
        ]),
    )
    .await
    .unwrap();
    let (msg, _input) = admin_get_msg(&format!("/b/products/admin/products/{product_id}"));
    let retry_html = output_to_html(
        super::super::pages::product_manager(&test_ctx, &msg, product_id, true).await,
    )
    .await;
    assert!(retry_html.contains("Retry Stripe sync"));
    assert!(retry_html.contains(
        "Stripe sync error: Stripe Price response did not match the immutable offer row"
    ));

    wafer_core::clients::database::update(
        &test_ctx,
        super::super::repo::offers::TABLE,
        active_id,
        HashMap::from([
            ("sync_status".to_string(), serde_json::json!("synced")),
            ("sync_error".to_string(), serde_json::json!("")),
        ]),
    )
    .await
    .unwrap();
    let (msg, _input) = admin_get_msg(&format!("/b/products/admin/products/{product_id}"));
    let reconcile_html = output_to_html(
        super::super::pages::product_manager(&test_ctx, &msg, product_id, true).await,
    )
    .await;
    assert!(reconcile_html.contains("Reconcile Stripe"));
}

#[tokio::test]
async fn seller_product_manager_is_owner_isolated_and_uses_seller_endpoints() {
    let test_ctx = ctx_with(&[("WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS", "true")]).await;
    let (msg, input) = create_msg(
        "/b/products/products",
        "seller_owner",
        serde_json::json!({
            "name": "Seller product",
            "slug": "seller-product",
            "currency": "USD"
        }),
    );
    let product = output_to_json(dispatch_user(&test_ctx, msg, input).await).await;
    let product_id = product["id"].as_str().unwrap();

    let (owner_msg, _input) = get_msg(
        &format!("/b/products/my-products/{product_id}"),
        "seller_owner",
    );
    let html = output_to_html(
        super::super::pages::product_manager(&test_ctx, &owner_msg, product_id, false).await,
    )
    .await;
    assert!(html.contains("Seller product"));
    assert!(html.contains(&format!("/b/products/api/products/{product_id}")));
    assert!(!html.contains("/b/products/api/admin/products"));

    let (other_msg, _input) = get_msg(
        &format!("/b/products/my-products/{product_id}"),
        "different_seller",
    );
    let out = super::super::pages::product_manager(&test_ctx, &other_msg, product_id, false).await;
    assert!(output_is_error(out, ErrorCode::NotFound).await);
}

#[tokio::test]
async fn admin_product_duplicate_copies_safe_metadata_and_non_archived_offers_as_drafts() {
    let test_ctx = ctx().await;
    let (msg, input) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({
            "name": "Original product",
            "slug": "original-product",
            "description": "Keep this description",
            "currency": "NZD",
            "fulfillment_kind": "download"
        }),
    );
    let source = output_to_json(dispatch_admin(&test_ctx, msg, input).await).await;
    let source_id = source["id"].as_str().unwrap();
    let collection = format!("/admin/b/products/products/{source_id}/offers");
    let offer_definition = |name: &str, amount: i64| {
        serde_json::json!({
            "name": name,
            "mode": "payment",
            "currency": "NZD",
            "pricing_model": "fixed",
            "interval_count": 1,
            "usage_type": "licensed",
            "billing_scheme": "per_unit",
            "tax_behavior": "exclusive",
            "variables": [],
            "components": [{
                "key": "price",
                "label": name,
                "required": true,
                "amount": {"type": "fixed", "unit_amount_minor": amount},
                "quantity": {"type": "fixed", "value": 1},
                "condition": {"op": "always"}
            }],
            "checkout": {}
        })
    };
    let (msg, input) = admin_create_msg(&collection, offer_definition("Current price", 2599));
    let current = output_to_json(dispatch_admin(&test_ctx, msg, input).await).await;
    let current_id = current["offer"]["id"].as_str().unwrap();
    let (msg, input) = admin_create_msg(
        &format!("{collection}/{current_id}/publish"),
        serde_json::json!({}),
    );
    output_to_json(dispatch_admin(&test_ctx, msg, input).await).await;
    let (msg, input) = admin_create_msg(&collection, offer_definition("Old price", 1999));
    let old = output_to_json(dispatch_admin(&test_ctx, msg, input).await).await;
    let old_id = old["offer"]["id"].as_str().unwrap();
    let (mut msg, input) = delete_msg(&format!("{collection}/{old_id}"), "admin_1");
    msg.set_meta("auth.user_roles", "admin");
    output_to_json(dispatch_admin(&test_ctx, msg, input).await).await;

    let (msg, input) = admin_create_msg(
        &format!("/admin/b/products/products/{source_id}/duplicate"),
        serde_json::json!({}),
    );
    let duplicated = output_to_json(dispatch_admin(&test_ctx, msg, input).await).await;
    let copy = &duplicated["product"];
    assert_ne!(copy["id"], source["id"]);
    assert_eq!(copy["data"]["name"], "Original product copy");
    assert!(copy["data"]["slug"]
        .as_str()
        .unwrap()
        .starts_with("original-product-copy-"));
    assert_eq!(copy["data"]["description"], "Keep this description");
    assert_eq!(copy["data"]["status"], "draft");
    assert_eq!(copy["data"]["owner_kind"], "platform");
    assert_eq!(copy["data"]["approval_status"], "approved");
    let offers = duplicated["offers"].as_array().unwrap();
    assert_eq!(offers.len(), 1, "archived offers must not be copied");
    assert_eq!(offers[0]["status"], "draft");
    assert_eq!(offers[0]["offer"]["name"], "Current price");
    assert_eq!(
        offers[0]["offer"]["components"][0]["amount"]["unit_amount_minor"],
        2599
    );
    assert_ne!(offers[0]["offer"]["id"], current["offer"]["id"]);
    assert_eq!(offers[0]["offer"]["stripe_product_id"], "");
    assert_eq!(offers[0]["offer"]["stripe_price_id"], "");
}

#[tokio::test]
async fn seller_product_duplicate_preserves_owner_moderation_and_rejects_other_sellers() {
    let test_ctx = ctx_with(&[
        ("WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS", "true"),
        ("IMPRESSPRESS__PRODUCTS__SELLER_MODERATION_REQUIRED", "true"),
    ])
    .await;
    let (msg, input) = create_msg(
        "/b/products/products",
        "seller_a",
        serde_json::json!({"name": "Owned product", "slug": "owned-product"}),
    );
    let source = output_to_json(dispatch_user(&test_ctx, msg, input).await).await;
    let source_id = source["id"].as_str().unwrap();
    let path = format!("/b/products/products/{source_id}/duplicate");

    let (msg, input) = create_msg(&path, "seller_b", serde_json::json!({}));
    assert!(
        output_is_error(
            dispatch_user(&test_ctx, msg, input).await,
            ErrorCode::NotFound
        )
        .await
    );

    let (msg, input) = create_msg(&path, "seller_a", serde_json::json!({}));
    let duplicated = output_to_json(dispatch_user(&test_ctx, msg, input).await).await;
    assert_eq!(duplicated["product"]["data"]["owner_kind"], "user");
    assert_eq!(duplicated["product"]["data"]["owner_id"], "seller_a");
    assert_eq!(duplicated["product"]["data"]["created_by"], "seller_a");
    assert_eq!(duplicated["product"]["data"]["status"], "draft");
    assert_eq!(duplicated["product"]["data"]["approval_status"], "draft");
}

#[tokio::test]
async fn admin_wizard_sequence_creates_and_publishes_subscription_offer() {
    let test_ctx = ctx().await;
    let (msg, input) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({
            "name": "Team plan",
            "slug": "team-plan",
            "description": "Monthly access",
            "currency": "NZD",
            "fulfillment_kind": "entitlement",
            "product_template_id": "simple_subscription"
        }),
    );
    let product = output_to_json(dispatch_admin(&test_ctx, msg, input).await).await;
    let product_id = product["id"].as_str().unwrap();
    assert_eq!(product["data"]["status"], "draft");

    let offer_collection = format!("/admin/b/products/products/{product_id}/offers");
    let (msg, input) = admin_create_msg(
        &offer_collection,
        serde_json::json!({
            "name": "Team plan",
            "mode": "subscription",
            "currency": "NZD",
            "pricing_model": "fixed",
            "recurring_interval": "month",
            "interval_count": 1,
            "usage_type": "licensed",
            "billing_scheme": "per_unit",
            "tax_behavior": "exclusive",
            "variables": [],
            "components": [{
                "key": "price",
                "label": "Team plan",
                "sort_order": 0,
                "required": true,
                "amount": {"type": "fixed", "unit_amount_minor": 1999},
                "quantity": {"type": "fixed", "value": 1},
                "condition": {"op": "always"},
                "recurrence": {"interval": "month", "interval_count": 1}
            }],
            "checkout": {
                "allow_promotion_codes": true,
                "automatic_tax": true,
                "collect_billing_address": true,
                "trial_days": 14
            }
        }),
    );
    let managed = output_to_json(dispatch_admin(&test_ctx, msg, input).await).await;
    let offer_id = managed["offer"]["id"].as_str().unwrap();
    assert_eq!(
        managed["offer"]["components"][0]["amount"]["unit_amount_minor"],
        1999
    );
    assert_eq!(managed["offer"]["checkout"]["trial_days"], 14);

    let (msg, input) = admin_create_msg(
        &format!("{offer_collection}/{offer_id}/publish"),
        serde_json::json!({}),
    );
    let published = output_to_json(dispatch_admin(&test_ctx, msg, input).await).await;
    assert_eq!(published["status"], "active");
    let (msg, input) = update_msg(
        &format!("/admin/b/products/products/{product_id}"),
        "admin_1",
        serde_json::json!({"status": "active"}),
    );
    let active_product = output_to_json(dispatch_admin(&test_ctx, msg, input).await).await;
    assert_eq!(active_product["data"]["status"], "active");

    let (msg, input) = get_msg(&format!("/b/products/storefront/{product_id}"), "");
    let storefront = output_to_json(dispatch_user(&test_ctx, msg, input).await).await;
    assert_eq!(storefront["offers"][0]["mode"], "subscription");
    assert_eq!(storefront["offers"][0]["recurring_interval"], "month");
}

#[tokio::test]
async fn seller_wizard_sequence_creates_configurable_offer_then_enters_moderation() {
    let test_ctx = ctx_with(&[
        ("WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS", "true"),
        ("IMPRESSPRESS__PRODUCTS__SELLER_MODERATION_REQUIRED", "true"),
    ])
    .await;
    let (msg, input) = create_msg(
        "/b/products/products",
        "seller_wizard",
        serde_json::json!({
            "name": "Custom engraving",
            "slug": "custom-engraving",
            "currency": "USD",
            "fulfillment_kind": "manual",
            "product_template_id": "configurable_product"
        }),
    );
    let product = output_to_json(dispatch_user(&test_ctx, msg, input).await).await;
    let product_id = product["id"].as_str().unwrap();
    assert_eq!(product["data"]["owner_id"], "seller_wizard");
    assert_eq!(product["data"]["approval_status"], "draft");

    let offer_collection = format!("/b/products/products/{product_id}/offers");
    let (msg, input) = create_msg(
        &offer_collection,
        "seller_wizard",
        serde_json::json!({
            "name": "Custom engraving",
            "mode": "payment",
            "currency": "USD",
            "pricing_model": "components",
            "interval_count": 1,
            "usage_type": "licensed",
            "billing_scheme": "per_unit",
            "tax_behavior": "unspecified",
            "variables": [{
                "key": "characters",
                "kind": "integer",
                "label": "Characters",
                "required": true,
                "minimum": "1",
                "maximum": "100",
                "step": "1",
                "visibility": "public",
                "sort_order": 0
            }],
            "components": [
                {
                    "key": "base",
                    "label": "Engraving setup",
                    "sort_order": 0,
                    "required": true,
                    "amount": {"type": "fixed", "unit_amount_minor": 500},
                    "quantity": {"type": "fixed", "value": 1},
                    "condition": {"op": "always"}
                },
                {
                    "key": "characters",
                    "label": "Characters",
                    "sort_order": 1,
                    "required": true,
                    "amount": {"type": "per_unit", "input": "characters", "unit_amount_minor": 25},
                    "quantity": {"type": "fixed", "value": 1},
                    "condition": {"op": "always"}
                }
            ],
            "checkout": {"collect_billing_address": true}
        }),
    );
    let managed = output_to_json(dispatch_user(&test_ctx, msg, input).await).await;
    let offer_id = managed["offer"]["id"].as_str().unwrap();
    assert_eq!(managed["offer"]["pricing_model"], "components");

    let (msg, input) = create_msg(
        &format!("{offer_collection}/{offer_id}/publish"),
        "seller_wizard",
        serde_json::json!({}),
    );
    let offer = output_to_json(dispatch_user(&test_ctx, msg, input).await).await;
    assert_eq!(offer["status"], "active");
    let (msg, input) = update_msg(
        &format!("/b/products/products/{product_id}"),
        "seller_wizard",
        serde_json::json!({"status": "active"}),
    );
    let pending = output_to_json(dispatch_user(&test_ctx, msg, input).await).await;
    assert_eq!(pending["data"]["status"], "pending_review");
    assert_eq!(pending["data"]["approval_status"], "pending");

    let (msg, input) = get_msg(&format!("/b/products/storefront/{product_id}"), "");
    assert!(
        output_is_error(
            dispatch_user(&test_ctx, msg, input).await,
            ErrorCode::NotFound
        )
        .await,
        "pending seller products must stay out of the public storefront"
    );
}

/// The products list pages adopt `ui::components::data_table`, which carries
/// the PR #75 mobile card-collapse fix via `td[data-label]`. Assert the manage
/// page renders the `.data-table` structure with per-cell data labels (so the
/// mobile baseline collapse works) instead of the old `.table-container`.
#[tokio::test]
async fn manage_products_uses_data_table_with_mobile_labels() {
    let ctx = ctx().await;
    let (c, c_input) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({ "name": "Widget" }),
    );
    dispatch_admin(&ctx, c, c_input).await;

    let (msg, _input) = admin_get_msg("/b/products/admin/manage");
    let html = output_to_html(super::super::pages::manage_products(&ctx, &msg).await).await;

    assert!(
        html.contains(r#"class="data-table""#),
        "manage page should render the shared data_table component"
    );
    assert!(
        html.contains(r#"data-label="Name""#),
        "data_table cells should carry data-label for the mobile card collapse"
    );
    assert!(html.contains("Widget"), "the seeded product should render");
}

/// `?view=deleted` is the only way to reach a soft-deleted row from the
/// admin UI — without it, soft delete is a one-way door. Pins that the
/// deleted view shows exactly the deleted rows (not the live ones), and
/// that the default (no `view`) list keeps excluding them.
#[tokio::test]
async fn manage_products_deleted_view_lists_only_deleted_products() {
    let ctx = ctx().await;

    let mut live = HashMap::new();
    live.insert("name".to_string(), serde_json::json!("live"));
    live.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "live", live).await;

    let mut gone = HashMap::new();
    gone.insert("name".to_string(), serde_json::json!("gone"));
    gone.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "gone", gone).await;
    soft_delete_product(&ctx, "gone").await;

    let (default_msg, _input) = admin_get_msg("/b/products/admin/manage");
    let default_html =
        output_to_html(super::super::pages::manage_products(&ctx, &default_msg).await).await;
    assert!(default_html.contains(">live<"), "{default_html}");
    assert!(!default_html.contains(">gone<"), "{default_html}");

    let (mut deleted_msg, _input) = admin_get_msg("/b/products/admin/manage");
    deleted_msg.set_meta("req.query.view", "deleted");
    let deleted_html =
        output_to_html(super::super::pages::manage_products(&ctx, &deleted_msg).await).await;
    assert!(deleted_html.contains(">gone<"), "{deleted_html}");
    assert!(!deleted_html.contains(">live<"), "{deleted_html}");
}

/// The deleted view's table has a `Deleted` column, so it has to be ordered
/// by it. Sorting the deleted list `created_at desc` (the live list's order)
/// puts the product an admin just deleted wherever its creation date happens
/// to fall — which for an old product is the bottom of the list, on the one
/// page whose entire purpose is undoing a delete that was probably a moment
/// ago.
#[tokio::test]
async fn manage_products_deleted_view_sorts_by_when_the_product_was_deleted() {
    let ctx = ctx().await;

    // Created most recently, deleted longest ago: first by `created_at
    // desc`, last by `deleted_at desc`.
    let mut stale = HashMap::new();
    stale.insert("name".to_string(), serde_json::json!("Deleted long ago"));
    stale.insert("status".to_string(), serde_json::json!("active"));
    stale.insert(
        "created_at".to_string(),
        serde_json::json!("2026-02-01T00:00:00Z"),
    );
    stale.insert(
        "deleted_at".to_string(),
        serde_json::json!("2026-03-01T00:00:00Z"),
    );
    seed(&ctx, "impresspress__products__products", "stale", stale).await;

    let mut fresh = HashMap::new();
    fresh.insert("name".to_string(), serde_json::json!("Deleted just now"));
    fresh.insert("status".to_string(), serde_json::json!("active"));
    fresh.insert(
        "created_at".to_string(),
        serde_json::json!("2026-01-01T00:00:00Z"),
    );
    fresh.insert(
        "deleted_at".to_string(),
        serde_json::json!("2026-03-02T00:00:00Z"),
    );
    seed(&ctx, "impresspress__products__products", "fresh", fresh).await;

    let (mut msg, _input) = admin_get_msg("/b/products/admin/manage");
    msg.set_meta("req.query.view", "deleted");
    let html = output_to_html(super::super::pages::manage_products(&ctx, &msg).await).await;

    let fresh_at = html
        .find("Deleted just now")
        .expect("the recently deleted product must render");
    let stale_at = html
        .find("Deleted long ago")
        .expect("the older deletion must render");
    assert!(
        fresh_at < stale_at,
        "the deleted view must be ordered by deleted_at desc, so the product just deleted \
         is at the top; got 'Deleted long ago' at {stale_at} before 'Deleted just now' at {fresh_at}"
    );
}

/// The deleted view is a dead end unless the way back is obvious: pin that
/// each deleted row's Restore action posts to the actual restore endpoint,
/// not the edit page (which 404s for a soft-deleted product until it is
/// restored).
#[tokio::test]
async fn manage_products_deleted_view_offers_restore_not_an_edit_link() {
    let ctx = ctx().await;

    let mut gone = HashMap::new();
    gone.insert("name".to_string(), serde_json::json!("gone"));
    gone.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "gone", gone).await;
    soft_delete_product(&ctx, "gone").await;

    let (mut msg, _input) = admin_get_msg("/b/products/admin/manage");
    msg.set_meta("req.query.view", "deleted");
    let html = output_to_html(super::super::pages::manage_products(&ctx, &msg).await).await;

    assert!(
        html.contains("/b/products/api/admin/products/gone/restore"),
        "deleted row should offer a Restore action wired to the restore endpoint: {html}"
    );
    assert!(
        !html.contains(r#"href="/b/products/admin/products/gone""#),
        "a deleted row must not link into the edit page, which refuses a soft-deleted product: {html}"
    );
}

/// The Restore button reloads the page on success. Without an explicit
/// failure branch a refused restore — a slug collision is reachable, see
/// `restore_reports_a_slug_conflict_instead_of_an_opaque_error` — renders as
/// nothing happening at all: no reload, no message, on the only door out of
/// soft delete. Pin that the button feeds the failure into the shared toast
/// channel `ui::assets::toast_js` already listens on.
#[tokio::test]
async fn manage_products_deleted_view_reports_a_failed_restore() {
    let ctx = ctx().await;

    let mut gone = HashMap::new();
    gone.insert("name".to_string(), serde_json::json!("gone"));
    gone.insert("status".to_string(), serde_json::json!("active"));
    seed(&ctx, "impresspress__products__products", "gone", gone).await;
    soft_delete_product(&ctx, "gone").await;

    let (mut msg, _input) = admin_get_msg("/b/products/admin/manage");
    msg.set_meta("req.query.view", "deleted");
    let html = output_to_html(super::super::pages::manage_products(&ctx, &msg).await).await;

    // `showToast` alone would match the page shell's own listener script,
    // which every admin page carries — the assertion has to see the BUTTON
    // raising the event.
    assert!(
        html.contains("new CustomEvent('showToast'"),
        "a failed restore must surface, not vanish: {html}"
    );
}

#[tokio::test]
async fn stripe_setup_guides_configuration_without_rendering_credentials() {
    let ctx = ctx_with(&[
        (
            "IMPRESSPRESS__PRODUCTS__STRIPE_PUBLISHABLE_KEY",
            "pk_test_must_never_render",
        ),
        (
            "IMPRESSPRESS__PRODUCTS__STRIPE_WEBHOOK_SECRET",
            "whsec_must_never_render",
        ),
    ])
    .await;
    let (msg, _input) = admin_get_msg("/b/products/admin/stripe");
    let html = output_to_html(super::super::pages::stripe_setup(&ctx, &msg).await).await;

    assert!(html.contains("Stripe setup"));
    assert!(html.contains("Not configured"));
    assert!(html.contains("Go-live checklist"));
    assert!(html.contains("/b/products/webhooks"));
    assert!(html.contains("checkout.session.completed"));
    assert!(html.contains("checkout.session.async_payment_succeeded"));
    assert!(html.contains("checkout.session.async_payment_failed"));
    assert!(html.contains("payment_intent.succeeded"));
    assert!(html.contains("payment_intent.payment_failed"));
    assert!(html.contains("payment_intent.processing"));
    assert!(html.contains("payment_intent.requires_action"));
    assert!(html.contains("payment_intent.canceled"));
    assert!(html.contains("Test connection"));
    assert!(html.contains("Webhook delivery health"));
    assert!(html.contains("Needs manual review"));
    assert!(html.contains("/b/products/api/admin/webhook-events"));
    assert!(html.contains("replayStripeWebhookEvent"));
    assert!(html.contains("Provider reconciliation"));
    assert!(html.contains("Reconcile due operations"));
    assert!(html.contains("/b/products/api/admin/provider-operations"));
    assert!(html.contains("reconcileStripeProviderOperations"));
    assert!(!html.contains("pk_test_must_never_render"));
    assert!(!html.contains("whsec_must_never_render"));
    assert!(html.contains(r#"href="/b/products/admin/stripe""#));
    assert!(html.contains("class=\"tab active\""));
}

#[tokio::test]
async fn commerce_home_keeps_buyer_actions_and_hides_seller_ui_when_disabled() {
    let ctx = ctx().await;
    let (msg, _input) = get_msg("/b/products/", "buyer_1");
    let html = output_to_html(super::super::pages::portal_home(&ctx, &msg).await).await;

    assert!(html.contains("Purchases and subscriptions"));
    assert!(html.contains("View purchases"));
    assert!(html.contains("Manage billing"));
    assert!(html.contains(r#"href="/b/products/my-purchases""#));
    assert!(!html.contains(r#"href="/b/products/my-products""#));
    assert!(!html.contains("Stripe seller account"));
    assert!(!html.contains("Connect Stripe to sell"));
    assert!(!html.contains("Products for sale"));
}

#[tokio::test]
async fn commerce_home_renders_seller_requirements_and_actions_when_enabled() {
    let ctx = ctx_with(&[
        ("WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS", "true"),
        ("IMPRESSPRESS__PRODUCTS__SELLER_APPLICATION_FEE_BPS", "250"),
    ])
    .await;
    seed(
        &ctx,
        super::super::repo::seller_accounts::TABLE,
        "seller_account_1",
        HashMap::from([
            ("user_id".to_string(), serde_json::json!("seller_1")),
            ("status".to_string(), serde_json::json!("onboarding")),
            (
                "stripe_account_id".to_string(),
                serde_json::json!("acct_test_seller_1"),
            ),
            ("details_submitted".to_string(), serde_json::json!(false)),
            ("charges_enabled".to_string(), serde_json::json!(false)),
            ("payouts_enabled".to_string(), serde_json::json!(false)),
            (
                "requirements_json".to_string(),
                serde_json::json!(r#"{"currently_due":["individual.verification.document"]}"#),
            ),
            ("fee_basis_points".to_string(), serde_json::json!(250)),
            ("dashboard_type".to_string(), serde_json::json!("express")),
        ]),
    )
    .await;

    let (msg, _input) = get_msg("/b/products/", "seller_1");
    let html = output_to_html(super::super::pages::portal_home(&ctx, &msg).await).await;
    assert!(html.contains(r#"href="/b/products/my-products""#));
    assert!(html.contains("Products for sale"));
    assert!(html.contains("Stripe seller account"));
    assert!(html.contains("Information Stripe still needs"));
    assert!(html.contains("individual › verification › document"));
    assert!(html.contains("Continue Stripe setup"));
    assert!(html.contains("Open Stripe dashboard"));
    assert!(html.contains("2.50%"));
}

#[tokio::test]
async fn seller_page_is_forbidden_when_user_selling_is_disabled() {
    use wafer_run::Block;

    let ctx = ctx().await;
    for path in [
        "/b/products/my-products",
        "/b/products/my-products/new",
        "/b/products/my-products/product_1",
        "/b/products/selling",
        "/b/products/selling/orders",
        "/b/products/selling/orders/order_1",
    ] {
        let (msg, input) = get_msg(path, "seller_1");
        let out = super::super::ProductsBlock::new()
            .handle(&ctx, msg, input)
            .await;
        assert!(
            output_is_error(out, ErrorCode::PermissionDenied).await,
            "{path} must be rejected while user selling is disabled"
        );
    }
}

#[test]
fn commerce_ssr_routes_declare_their_auth_tiers() {
    use wafer_run::{AuthLevel, Block};

    let info = super::super::ProductsBlock::new().info();
    for path in [
        "/b/products",
        "/b/products/",
        "/b/products/my-products",
        "/b/products/my-products/new",
        "/b/products/my-products/product_1",
        "/b/products/my-purchases",
        "/b/products/my-purchases/order_1",
        "/b/products/selling",
        "/b/products/selling/orders",
        "/b/products/selling/orders/order_1",
    ] {
        assert_eq!(
            crate::endpoint_match::endpoint_auth(&info.endpoints, "retrieve", path),
            Some(AuthLevel::Authenticated),
            "{path} must require an authenticated user"
        );
    }
    assert_eq!(
        crate::endpoint_match::endpoint_auth(
            &info.endpoints,
            "retrieve",
            "/b/products/admin/stripe"
        ),
        Some(AuthLevel::Admin)
    );
    assert_eq!(
        crate::endpoint_match::endpoint_auth(&info.endpoints, "retrieve", "/b/products/admin/new"),
        Some(AuthLevel::Admin)
    );
    assert_eq!(
        crate::endpoint_match::endpoint_auth(
            &info.endpoints,
            "retrieve",
            "/b/products/admin/products/product_1"
        ),
        Some(AuthLevel::Admin)
    );
    for (action, path) in [
        ("retrieve", "/b/products/api/products"),
        ("create", "/b/products/api/products"),
        ("update", "/b/products/api/products/product_1"),
        ("delete", "/b/products/api/products/product_1"),
        ("create", "/b/products/api/products/product_1/duplicate"),
    ] {
        assert_eq!(
            crate::endpoint_match::endpoint_auth(&info.endpoints, action, path),
            Some(AuthLevel::Authenticated),
            "{action} {path} must require seller authentication"
        );
    }
    assert_eq!(
        crate::endpoint_match::endpoint_auth(
            &info.endpoints,
            "create",
            "/b/products/api/admin/products/product_1/duplicate"
        ),
        Some(AuthLevel::Admin)
    );
    for (action, path) in [
        ("retrieve", "/b/products/api/admin/webhook-events"),
        (
            "create",
            "/b/products/api/admin/webhook-events/evt_1/replay",
        ),
    ] {
        assert_eq!(
            crate::endpoint_match::endpoint_auth(&info.endpoints, action, path),
            Some(AuthLevel::Admin),
            "{action} {path} must require an administrator"
        );
    }
}

#[tokio::test]
async fn admin_can_inspect_and_replay_dead_letter_webhooks_without_payload_disclosure() {
    let ctx = ctx_with(&[(
        "IMPRESSPRESS__PRODUCTS__STRIPE_WEBHOOK_SECRET",
        "whsec_route_replay",
    )])
    .await;
    let payload = r#"{"id":"evt_route_replay","type":"charge.refunded","livemode":false,"data":{"object":{"payment_intent":"pi_route_private","livemode":false}}}"#;
    seed(
        &ctx,
        "impresspress__products__stripe_events",
        "evt_route_replay",
        HashMap::from([
            (
                "event_type".to_string(),
                serde_json::json!("charge.refunded"),
            ),
            ("status".to_string(), serde_json::json!("dead_letter")),
            ("attempts".to_string(), serde_json::json!(8)),
            (
                "processing_owner".to_string(),
                serde_json::json!("private-owner-token"),
            ),
            (
                "payload_base64".to_string(),
                serde_json::json!(Base64::encode_string(payload.as_bytes())),
            ),
            (
                "payload_sha256".to_string(),
                serde_json::json!(crate::util::sha256_hex(payload.as_bytes())),
            ),
            (
                "last_error".to_string(),
                serde_json::json!("temporary reconciliation failure"),
            ),
        ]),
    )
    .await;

    let (list, list_input) = admin_get_msg("/admin/b/products/webhook-events");
    let body = output_to_json(dispatch_admin(&ctx, list, list_input).await).await;
    assert_eq!(body["total_count"], 1);
    assert_eq!(body["records"][0]["id"], "evt_route_replay");
    let encoded = serde_json::to_string(&body).unwrap();
    assert!(!encoded.contains("pi_route_private"));
    assert!(!encoded.contains("private-owner-token"));
    assert!(!encoded.contains("payload_base64"));
    assert!(!encoded.contains("payload_sha256"));

    let (replay, replay_input) = admin_create_msg(
        "/admin/b/products/webhook-events/evt_route_replay/replay",
        serde_json::json!({}),
    );
    let replayed = output_to_json(dispatch_admin(&ctx, replay, replay_input).await).await;
    assert_eq!(replayed["received"], true);

    let event = wafer_core::clients::database::get(
        &ctx,
        "impresspress__products__stripe_events",
        "evt_route_replay",
    )
    .await
    .expect("replayed event");
    assert_eq!(
        crate::util::RecordExt::str_field(&event, "status"),
        "processed"
    );
}

#[tokio::test]
async fn order_pages_use_exact_currency_and_enforce_buyer_seller_actions() {
    let ctx = ctx_with(&[("WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS", "true")]).await;
    seed(
        &ctx,
        super::super::repo::seller_accounts::TABLE,
        "seller_page_account",
        HashMap::from([
            ("user_id".to_string(), serde_json::json!("seller_page_user")),
            ("status".to_string(), serde_json::json!("active")),
        ]),
    )
    .await;
    seed(
        &ctx,
        super::super::repo::seller_accounts::TABLE,
        "seller_other_account",
        HashMap::from([
            (
                "user_id".to_string(),
                serde_json::json!("seller_other_user"),
            ),
            ("status".to_string(), serde_json::json!("active")),
        ]),
    )
    .await;
    seed(
        &ctx,
        "impresspress__products__purchases",
        "order_page_jpy",
        HashMap::from([
            ("user_id".to_string(), serde_json::json!("buyer_page_user")),
            (
                "buyer_user_id".to_string(),
                serde_json::json!("buyer_page_user"),
            ),
            (
                "buyer_email".to_string(),
                serde_json::json!("buyer@example.com"),
            ),
            (
                "seller_account_id".to_string(),
                serde_json::json!("seller_page_account"),
            ),
            ("status".to_string(), serde_json::json!("completed")),
            ("currency".to_string(), serde_json::json!("JPY")),
            ("subtotal_cents".to_string(), serde_json::json!(1234)),
            ("total_cents".to_string(), serde_json::json!(1234)),
            ("provider".to_string(), serde_json::json!("stripe")),
            (
                "stripe_customer_id".to_string(),
                serde_json::json!("cus_page_buyer"),
            ),
            (
                "stripe_subscription_id".to_string(),
                serde_json::json!("sub_page_buyer"),
            ),
            (
                "subscription_status".to_string(),
                serde_json::json!("active"),
            ),
            (
                "reconciliation_status".to_string(),
                serde_json::json!("reconciled"),
            ),
            (
                "provider_payment_status".to_string(),
                serde_json::json!("succeeded"),
            ),
            (
                "provider_payment_intent_id".to_string(),
                serde_json::json!("pi_page_order"),
            ),
            (
                "stripe_payment_intent_id".to_string(),
                serde_json::json!("pi_page_order"),
            ),
        ]),
    )
    .await;
    seed(
        &ctx,
        "impresspress__products__line_items",
        "order_page_line",
        HashMap::from([
            (
                "purchase_id".to_string(),
                serde_json::json!("order_page_jpy"),
            ),
            ("product_id".to_string(), serde_json::json!("tokyo_pass")),
            ("product_name".to_string(), serde_json::json!("Tokyo pass")),
            ("quantity".to_string(), serde_json::json!(1)),
            ("unit_amount_minor".to_string(), serde_json::json!(1234)),
            ("total_minor".to_string(), serde_json::json!(1234)),
        ]),
    )
    .await;
    seed(
        &ctx,
        super::super::repo::disputes::TABLE,
        "order_page_dispute",
        HashMap::from([
            (
                "purchase_id".to_string(),
                serde_json::json!("order_page_jpy"),
            ),
            (
                "seller_account_id".to_string(),
                serde_json::json!("seller_page_account"),
            ),
            (
                "stripe_account_id".to_string(),
                serde_json::json!("acct_page_seller"),
            ),
            (
                "provider_dispute_id".to_string(),
                serde_json::json!("dp_page_order"),
            ),
            (
                "payment_intent_id".to_string(),
                serde_json::json!("pi_page_order"),
            ),
            ("status".to_string(), serde_json::json!("needs_response")),
            ("amount_minor".to_string(), serde_json::json!(500)),
            ("currency".to_string(), serde_json::json!("JPY")),
            ("reason".to_string(), serde_json::json!("fraudulent")),
            (
                "evidence_due_by".to_string(),
                serde_json::json!("2033-05-18T03:33:20+00:00"),
            ),
        ]),
    )
    .await;

    let (buyer_msg, _) = get_msg("/b/products/my-purchases/order_page_jpy", "buyer_page_user");
    let buyer_html = output_to_html(
        super::super::pages::my_purchase_detail(&ctx, &buyer_msg, "order_page_jpy").await,
    )
    .await;
    assert!(buyer_html.contains("1234 JPY"));
    assert!(!buyer_html.contains("12.34 JPY"));
    assert!(buyer_html.contains("Tokyo pass"));
    assert!(buyer_html.contains("Manage subscription and billing"));
    assert!(buyer_html.contains("Payment state:"));
    assert!(buyer_html.contains("pi_page_order"));
    assert!(buyer_html.contains("Payment disputes"));
    assert!(buyer_html.contains("500 JPY"));
    assert!(!buyer_html.contains(r#"id="order-refund-amount""#));

    let (admin_msg, _) = admin_get_msg("/b/products/admin/purchases/order_page_jpy");
    let admin_html = output_to_html(
        super::super::pages::admin_purchase_detail(&ctx, &admin_msg, "order_page_jpy").await,
    )
    .await;
    assert!(admin_html.contains("Create refund"));
    assert!(admin_html.contains("dp_page_order"));
    assert!(
        admin_html.contains("Evidence, balance impact, and payout actions are managed in Stripe")
    );
    assert!(admin_html.contains(r#"id="order-refund-amount""#));
    assert!(admin_html.contains("/b/products/api/admin/purchases/order_page_jpy/refund"));

    let (seller_msg, _) = get_msg(
        "/b/products/selling/orders/order_page_jpy",
        "seller_page_user",
    );
    let seller_html = output_to_html(
        super::super::pages::seller_order_detail(&ctx, &seller_msg, "order_page_jpy").await,
    )
    .await;
    assert!(seller_html.contains("Create refund"));
    assert!(seller_html.contains("dp_page_order"));
    assert!(seller_html.contains("/b/products/api/seller/orders/order_page_jpy/refund"));

    let (other_seller, _) = get_msg(
        "/b/products/selling/orders/order_page_jpy",
        "seller_other_user",
    );
    assert!(
        output_is_error(
            super::super::pages::seller_order_detail(&ctx, &other_seller, "order_page_jpy").await,
            ErrorCode::PermissionDenied,
        )
        .await
    );
}

#[tokio::test]
async fn seller_dashboard_renders_only_own_currency_stats() {
    let ctx = ctx_with(&[("WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS", "true")]).await;
    seed(
        &ctx,
        super::super::repo::seller_accounts::TABLE,
        "seller_dashboard_account",
        HashMap::from([
            (
                "user_id".to_string(),
                serde_json::json!("seller_dashboard_user"),
            ),
            ("status".to_string(), serde_json::json!("active")),
        ]),
    )
    .await;
    for (id, seller, total) in [
        ("seller_dashboard_own", "seller_dashboard_account", 4200),
        ("seller_dashboard_other", "another_seller_account", 9900),
    ] {
        seed(
            &ctx,
            "impresspress__products__purchases",
            id,
            HashMap::from([
                ("user_id".to_string(), serde_json::json!("buyer")),
                ("seller_account_id".to_string(), serde_json::json!(seller)),
                ("status".to_string(), serde_json::json!("completed")),
                ("currency".to_string(), serde_json::json!("NZD")),
                ("total_cents".to_string(), serde_json::json!(total)),
            ]),
        )
        .await;
    }
    seed(
        &ctx,
        "impresspress__products__purchases",
        "seller_dashboard_failed_own",
        HashMap::from([
            ("user_id".to_string(), serde_json::json!("buyer")),
            (
                "seller_account_id".to_string(),
                serde_json::json!("seller_dashboard_account"),
            ),
            ("status".to_string(), serde_json::json!("failed")),
            ("currency".to_string(), serde_json::json!("NZD")),
            ("total_cents".to_string(), serde_json::json!(1700)),
            (
                "reconciliation_error".to_string(),
                serde_json::json!("Own checkout needs attention"),
            ),
        ]),
    )
    .await;
    seed(
        &ctx,
        "impresspress__products__purchases",
        "seller_dashboard_failed_other",
        HashMap::from([
            ("user_id".to_string(), serde_json::json!("buyer")),
            (
                "seller_account_id".to_string(),
                serde_json::json!("another_seller_account"),
            ),
            ("status".to_string(), serde_json::json!("failed")),
            ("currency".to_string(), serde_json::json!("NZD")),
            ("total_cents".to_string(), serde_json::json!(9900)),
            (
                "reconciliation_error".to_string(),
                serde_json::json!("Other seller secret failure"),
            ),
        ]),
    )
    .await;
    let (msg, _) = get_msg("/b/products/selling", "seller_dashboard_user");
    let html = output_to_html(super::super::pages::seller_dashboard(&ctx, &msg).await).await;
    assert!(html.contains("42.00 NZD"));
    assert!(!html.contains("99.00 NZD"));
    assert!(html.contains("Before Stripe fees"));
    assert!(html.contains("before Stripe fees, disputes, reserves, and payout adjustments"));
    assert!(html.contains("Recent payment failures"));
    assert!(html.contains("Own checkout needs attention"));
    assert!(!html.contains("Other seller secret failure"));
    assert!(html.contains(r#"href="/b/products/selling/orders""#));
    assert!(html.contains(r#"href="/b/products/my-products""#));
}

#[test]
fn owned_group_and_taxonomy_routes_are_explicitly_authenticated() {
    use wafer_run::{AuthLevel, Block};

    let info = super::super::ProductsBlock::new().info();
    for (action, path) in [
        ("retrieve", "/b/products/groups"),
        ("create", "/b/products/groups"),
        ("retrieve", "/b/products/groups/group_1"),
        ("update", "/b/products/groups/group_1"),
        ("delete", "/b/products/groups/group_1"),
        ("retrieve", "/b/products/groups/group_1/products"),
        ("retrieve", "/b/products/types"),
        ("retrieve", "/b/products/group-templates"),
    ] {
        assert_eq!(
            crate::endpoint_match::endpoint_auth(&info.endpoints, action, path),
            Some(AuthLevel::Authenticated),
            "{action} {path} must be explicitly declared as authenticated"
        );
    }
}

#[test]
fn every_products_json_endpoint_has_discovery_schema() {
    use wafer_run::Block;

    let info = super::super::ProductsBlock::new().info();
    let missing = info
        .endpoints
        .iter()
        .filter(|endpoint| {
            endpoint.path.starts_with("/b/products/api/")
                || matches!(
                    endpoint.path.as_str(),
                    "/b/products/groups"
                        | "/b/products/groups/{id}"
                        | "/b/products/groups/{id}/products"
                        | "/b/products/types"
                        | "/b/products/group-templates"
                        | "/b/products/catalog"
                        | "/b/products/catalog/{id}"
                        | "/b/products/storefront/config"
                        | "/b/products/storefront/{product_id}"
                        | "/b/products/webhooks"
                        | "/b/products/pricing/preview"
                        | "/b/products/checkout"
                        | "/b/products/orders/{id}/status"
                        | "/b/products/purchases"
                        | "/b/products/purchases/{id}"
                        | "/b/products/subscription"
                        | "/b/products/billing-portal"
                )
        })
        .filter(|endpoint| !endpoint.has_schema())
        .map(|endpoint| format!("{:?} {}", endpoint.method, endpoint.path))
        .collect::<Vec<_>>();

    assert!(
        missing.is_empty(),
        "products JSON endpoints omitted from discovery:\n{}",
        missing.join("\n")
    );
}

#[test]
fn dispatch_tables_are_backed_by_declared_endpoints() {
    // Central auth enforcement matches the on-the-wire path against the
    // endpoints DECLARED in `BlockInfo`; an undeclared path falls back to
    // `Authenticated`, not `Admin`. The dispatch tables are matched AFTER
    // that gate, so a dispatch entry without a matching declaration is a
    // reachable route with the wrong tier (PR #59 shipped exactly this: a
    // stale PATCH refund alias only in the dispatch table, refundable by any
    // logged-in user). Drive both tables against the real `BlockInfo` so the
    // two surfaces cannot drift again.
    use wafer_run::{AuthLevel, Block};

    use crate::endpoint_match;

    let info = super::super::ProductsBlock::new().info();

    for route in super::super::handlers::ADMIN_ROUTES {
        let declared_path =
            route
                .template
                .replacen("/admin/b/products", "/b/products/api/admin", 1);
        assert!(
            info.endpoints.iter().any(|endpoint| {
                endpoint.method == route.method
                    && endpoint.path == declared_path
                    && endpoint.auth == AuthLevel::Admin
            }),
            "admin dispatch route {:?} {} has no declared Admin endpoint {}",
            route.method,
            route.template,
            declared_path,
        );
    }

    // A user dispatch route answers at BOTH wire spellings, because
    // `ProductsBlock::handle` enters `handle_user` from `/b/products/api/...`
    // (normalized) AND from the raw `/b/products/...` path. Declaring only
    // one of them is legal — the other then resolves to `declared_access`'s
    // `Authenticated` fallback — but only while that fallback is no weaker
    // than the declaration. Restore was declared `Admin` at the `/api/`
    // spelling alone and was therefore reachable at `Authenticated` through
    // the raw one: any logged-in user could resurrect any soft-deleted
    // product. So the rule is not "some spelling is declared" (which that
    // route satisfied) but "EVERY spelling that reaches the handler is
    // enforced at least as strictly as the strictest declaration" — which
    // in practice keeps `Admin` routes off this table entirely, where they
    // belong on `ADMIN_ROUTES` behind the single `/b/products/api/admin`
    // prefix.
    for route in super::super::handlers::USER_ROUTES {
        let api_path = route.template.replacen("/b/products", "/b/products/api", 1);
        let action = endpoint_match::action_for_method(route.method);
        let spellings = [
            (
                api_path.as_str(),
                endpoint_match::endpoint_auth(&info.endpoints, action, &api_path),
            ),
            (
                route.template,
                endpoint_match::endpoint_auth(&info.endpoints, action, route.template),
            ),
        ];
        assert!(
            spellings.iter().any(|(_, declared)| declared.is_some()),
            "user dispatch route {:?} {} declared neither as {} nor as {}",
            route.method,
            route.template,
            api_path,
            route.template,
        );
        let strictest = spellings
            .iter()
            .filter_map(|(_, declared)| *declared)
            .max_by_key(|auth| auth_rank(*auth))
            .expect("at least one spelling is declared");
        for (spelling, declared) in spellings {
            // Undeclared spellings get `declared_access`'s fail-closed
            // fallback, mirroring routing.rs:567's `route.access.max(..)`
            // (the `/b/products` prefix tier is `Public`, so the fallback is
            // the whole decision).
            let enforced = declared.unwrap_or(AuthLevel::Authenticated);
            assert!(
                auth_rank(enforced) >= auth_rank(strictest),
                "user dispatch route {:?} {} is enforced at {:?} on the {} spelling but \
                 declared {:?} elsewhere — the weaker spelling reaches the same handler, \
                 so the declaration is not the tier a caller actually faces. Move it to \
                 ADMIN_ROUTES (one prefix, one spelling) or declare every spelling.",
                route.method,
                route.template,
                enforced,
                spelling,
                strictest,
            );
        }
    }
}

// The strictness ordering below is `endpoint_match::auth_rank`, not a copy of
// it. A copy would go on asserting against its own idea of strictness after
// the router's changed, leaving this gate green while the thing it guards
// weakened.
use crate::endpoint_match::auth_rank;

// ============================================================
// The request body cannot rewrite a product's identity
// ============================================================

/// A PATCH body carrying `id` used to reach `update_live` verbatim, so the
/// write became `SET id = 'new' WHERE id = 'old' AND deleted_at IS NULL`:
/// one row updated, the guard satisfied, and every `product_id` reference in
/// `line_items`, `offers`, `product_versions` and `entitlements` orphaned —
/// the exact failure soft delete exists to prevent. The re-read then looked
/// up the ORIGINAL id, found nothing, and answered "Product not found", so
/// the caller was told the write had failed while it had in fact rewritten
/// the primary key.
#[tokio::test]
async fn admin_patch_cannot_rewrite_a_products_id() {
    let ctx = ctx().await;

    let (create, create_input) = admin_create_msg(
        "/admin/b/products/products",
        serde_json::json!({ "name": "Original" }),
    );
    let id = output_to_json(dispatch_admin(&ctx, create, create_input).await).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (mut update, update_input) = request_msg(
        "update",
        &format!("/admin/b/products/products/{id}"),
        "admin_1",
        serde_json::json!({ "id": "p_hijacked", "name": "Renamed" }),
    );
    update.set_meta("auth.user_roles", "admin");
    let out = dispatch_admin(&ctx, update, update_input).await;
    assert!(
        output_is_error(out, ErrorCode::InvalidArgument).await,
        "a body that names an unsettable field must be refused outright"
    );

    // The row still answers to its own id, and no row answers to the one the
    // body tried to claim.
    let kept = super::super::repo::products::get(&ctx, &id)
        .await
        .expect("the product must still answer to its original id");
    assert_eq!(kept.id, id);
    assert_eq!(
        crate::util::RecordExt::str_field(&kept, "name"),
        "Original",
        "a refused write must not apply its other fields either"
    );
    assert!(
        wafer_core::clients::database::get(&ctx, "impresspress__products__products", "p_hijacked")
            .await
            .is_err(),
        "no row may answer to the id the body tried to claim"
    );
}

/// The seller-owned PATCH reaches the same `update_live` with the same
/// caller-supplied body, so it carries the identical primary-key rewrite.
#[tokio::test]
async fn seller_patch_cannot_rewrite_a_products_id() {
    let ctx = user_products_ctx().await;

    let (create, create_input) = create_msg(
        "/b/products/products",
        "user_1",
        serde_json::json!({ "name": "Original" }),
    );
    let id = output_to_json(dispatch_user(&ctx, create, create_input).await).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (update, update_input) = update_msg(
        &format!("/b/products/products/{id}"),
        "user_1",
        serde_json::json!({ "id": "p_hijacked", "name": "Renamed" }),
    );
    let out = dispatch_user(&ctx, update, update_input).await;
    assert!(
        output_is_error(out, ErrorCode::InvalidArgument).await,
        "a body that names an unsettable field must be refused outright"
    );

    let kept = super::super::repo::products::get(&ctx, &id)
        .await
        .expect("the product must still answer to its original id");
    assert_eq!(kept.id, id);
    assert_eq!(
        crate::util::RecordExt::str_field(&kept, "name"),
        "Original",
        "a refused write must not apply its other fields either"
    );
    assert!(
        wafer_core::clients::database::get(&ctx, "impresspress__products__products", "p_hijacked")
            .await
            .is_err(),
        "no row may answer to the id the body tried to claim"
    );
}

/// The handler-level refusal is a 400 for a clear message; the invariant
/// itself belongs to the repo, which is the layer every future caller goes
/// through. `update_live` must refuse an `id` in `data` on its own, so a new
/// call site cannot reintroduce the rewrite by forwarding a map the handler
/// never saw.
#[tokio::test]
async fn update_live_refuses_to_rewrite_the_primary_key() {
    let ctx = ctx().await;
    let mut data = HashMap::new();
    data.insert("name".to_string(), serde_json::json!("Original"));
    seed(&ctx, "impresspress__products__products", "p1", data).await;

    let error = super::super::repo::products::update_live(
        &ctx,
        "p1",
        HashMap::from([("id".to_string(), serde_json::json!("p_hijacked"))]),
    )
    .await
    .expect_err("a product's id is immutable");
    assert_eq!(error.code, ErrorCode::InvalidArgument);

    assert!(
        super::super::repo::products::get(&ctx, "p1").await.is_ok(),
        "the row must still answer to its original id"
    );
}
