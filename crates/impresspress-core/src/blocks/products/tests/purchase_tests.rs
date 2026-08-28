use std::collections::HashMap;

use wafer_run::ErrorCode;

use super::harness::*;
use crate::blocks::products::purchase;

// ============================================================
// Order history and refunds
// ============================================================

#[tokio::test]
async fn list_user_purchases_only_own() {
    let ctx = ctx().await;

    // Seed purchases for two different users
    let mut p1 = HashMap::new();
    p1.insert("user_id".to_string(), serde_json::json!("user_1"));
    p1.insert("status".to_string(), serde_json::json!("pending"));
    p1.insert("total_cents".to_string(), serde_json::json!(1000));
    seed(&ctx, "impresspress__products__purchases", "pur_1", p1).await;

    let mut p2 = HashMap::new();
    p2.insert("user_id".to_string(), serde_json::json!("user_2"));
    p2.insert("status".to_string(), serde_json::json!("completed"));
    p2.insert("total_cents".to_string(), serde_json::json!(2000));
    seed(&ctx, "impresspress__products__purchases", "pur_2", p2).await;

    let (msg, _input) = get_msg("/b/products/purchases", "user_1");
    let out = purchase::handle_list_user(&ctx, &msg).await;
    let body = output_to_json(out).await;
    let records = body["records"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["id"], "pur_1");
}

// ============================================================
// Purchase detail retrieval
// ============================================================

#[tokio::test]
async fn get_purchase_own() {
    let ctx = ctx().await;

    let mut pd = HashMap::new();
    pd.insert("user_id".to_string(), serde_json::json!("user_1"));
    pd.insert("status".to_string(), serde_json::json!("pending"));
    pd.insert("total_cents".to_string(), serde_json::json!(5000));
    seed(&ctx, "impresspress__products__purchases", "pur_own", pd).await;
    seed(
        &ctx,
        super::super::repo::disputes::TABLE,
        "dp_own",
        HashMap::from([
            ("purchase_id".to_string(), serde_json::json!("pur_own")),
            (
                "provider_dispute_id".to_string(),
                serde_json::json!("dp_provider_own"),
            ),
            ("payment_intent_id".to_string(), serde_json::json!("pi_own")),
            ("status".to_string(), serde_json::json!("under_review")),
            ("amount_minor".to_string(), serde_json::json!(1000)),
            ("currency".to_string(), serde_json::json!("USD")),
        ]),
    )
    .await;

    let (msg, _input) = get_msg("/b/products/purchases/pur_own", "user_1");
    let out = purchase::handle_get(&ctx, &msg).await;
    let body = output_to_json(out).await;
    assert_eq!(body["purchase"]["id"], "pur_own");
    assert_eq!(
        body["disputes"][0]["provider_dispute_id"],
        "dp_provider_own"
    );
}

/// The order list and detail endpoints publish `contracts::PurchaseView`
/// rows (and `LineItemView` / `RefundView` / `DisputeView` under the detail),
/// flat, with exactly the types' field sets. Two columns the raw echo used
/// to hand out are withheld everywhere: `receipt_token_hash`, the sha256 of
/// the guest receipt capability, together with its expiry, and on refund
/// rows `idempotency_key` and `response_json`, which the block's own
/// provider-operation projection already keeps private.
#[tokio::test]
async fn order_endpoints_publish_typed_views_and_withhold_the_receipt_digest() {
    use crate::blocks::products::contracts::{PurchaseDetailResponse, PurchaseListResponse};

    let ctx = ctx().await;
    seed(
        &ctx,
        "impresspress__products__purchases",
        "pur_typed",
        HashMap::from([
            ("user_id".to_string(), serde_json::json!("user_1")),
            ("buyer_user_id".to_string(), serde_json::json!("user_1")),
            (
                "buyer_email".to_string(),
                serde_json::json!("buyer@example.com"),
            ),
            ("status".to_string(), serde_json::json!("completed")),
            ("total_cents".to_string(), serde_json::json!(5000)),
            ("livemode".to_string(), serde_json::json!(1)),
            (
                "subscription_cancel_at_period_end".to_string(),
                serde_json::json!(0),
            ),
            (
                "metadata".to_string(),
                serde_json::json!({"offer_id": "offer_1", "offer_version": 2}),
            ),
            (
                "receipt_token_hash".to_string(),
                serde_json::json!("deadbeef-digest"),
            ),
            (
                "receipt_token_expires_at".to_string(),
                serde_json::json!("2026-08-01T00:00:00Z"),
            ),
            (
                "payment_at".to_string(),
                serde_json::json!("2026-07-19T01:02:03Z"),
            ),
        ]),
    )
    .await;
    seed(
        &ctx,
        "impresspress__products__line_items",
        "li_typed",
        HashMap::from([
            ("purchase_id".to_string(), serde_json::json!("pur_typed")),
            ("product_id".to_string(), serde_json::json!("prod_1")),
            ("product_name".to_string(), serde_json::json!("Widget")),
            ("quantity".to_string(), serde_json::json!(2)),
            ("total_minor".to_string(), serde_json::json!(5000)),
            (
                "input_snapshot".to_string(),
                serde_json::json!({"size": "large"}),
            ),
        ]),
    )
    .await;
    seed(
        &ctx,
        super::super::repo::refunds::TABLE,
        "rf_typed",
        HashMap::from([
            ("purchase_id".to_string(), serde_json::json!("pur_typed")),
            (
                "payment_intent_id".to_string(),
                serde_json::json!("pi_typed"),
            ),
            (
                "idempotency_key".to_string(),
                serde_json::json!("impresspress_refund_pur_typed_full"),
            ),
            ("amount_minor".to_string(), serde_json::json!(1000)),
            (
                "target_refunded_total_minor".to_string(),
                serde_json::json!(1000),
            ),
            ("currency".to_string(), serde_json::json!("USD")),
            ("status".to_string(), serde_json::json!("succeeded")),
            ("note".to_string(), serde_json::json!("goodwill")),
            (
                "response_json".to_string(),
                serde_json::json!("{\"id\":\"re_secret\"}"),
            ),
        ]),
    )
    .await;
    seed(
        &ctx,
        super::super::repo::disputes::TABLE,
        "dp_typed",
        HashMap::from([
            ("purchase_id".to_string(), serde_json::json!("pur_typed")),
            (
                "provider_dispute_id".to_string(),
                serde_json::json!("dp_provider_typed"),
            ),
            (
                "payment_intent_id".to_string(),
                serde_json::json!("pi_typed"),
            ),
            ("status".to_string(), serde_json::json!("needs_response")),
            ("amount_minor".to_string(), serde_json::json!(1000)),
            ("currency".to_string(), serde_json::json!("USD")),
            ("livemode".to_string(), serde_json::json!(1)),
        ]),
    )
    .await;

    let (msg, _input) = get_msg("/b/products/purchases/pur_typed", "user_1");
    let body = output_to_json(purchase::handle_get(&ctx, &msg).await).await;
    let detail: PurchaseDetailResponse =
        serde_json::from_value(body.clone()).expect("PurchaseDetailResponse");
    assert_eq!(serde_json::to_value(&detail).unwrap(), body);
    assert_eq!(detail.purchase.id, "pur_typed");
    assert_eq!(detail.purchase.total_cents, 5000);
    assert!(
        detail.purchase.livemode,
        "INTEGER column reads as a boolean"
    );
    assert!(!detail.purchase.subscription_cancel_at_period_end);
    assert_eq!(
        detail.purchase.metadata.get("offer_id"),
        Some(&serde_json::json!("offer_1"))
    );
    assert_eq!(
        detail.purchase.payment_at.as_deref(),
        Some("2026-07-19T01:02:03Z")
    );
    assert_eq!(detail.line_items[0].product_name, "Widget");
    assert_eq!(detail.line_items[0].quantity, 2);
    assert_eq!(
        serde_json::Value::Object(detail.line_items[0].input_snapshot.clone()),
        serde_json::json!({"size": "large"})
    );
    assert_eq!(detail.refunds[0].note, "goodwill");
    assert_eq!(detail.refunds[0].amount_minor, 1000);
    assert_eq!(detail.disputes[0].provider_dispute_id, "dp_provider_typed");
    assert!(detail.disputes[0].livemode);

    let encoded = body.to_string();
    for withheld in [
        "receipt_token_hash",
        "receipt_token_expires_at",
        "deadbeef-digest",
        "idempotency_key",
        "impresspress_refund_pur_typed_full",
        "response_json",
        "re_secret",
    ] {
        assert!(
            !encoded.contains(withheld),
            "detail leaked {withheld}: {body}"
        );
    }

    let (msg, _input) = get_msg("/b/products/purchases", "user_1");
    let list = output_to_json(purchase::handle_list_user(&ctx, &msg).await).await;
    let typed: PurchaseListResponse =
        serde_json::from_value(list.clone()).expect("PurchaseListResponse");
    assert_eq!(serde_json::to_value(&typed).unwrap(), list);
    assert_eq!(typed.records[0].id, "pur_typed");
    assert_eq!(typed.page_size, 20);
    let encoded = list.to_string();
    assert!(
        !encoded.contains("receipt_token"),
        "list leaked the digest: {list}"
    );

    let (mut msg, _input) = get_msg("/admin/b/products/purchases", "admin_1");
    msg.set_meta("auth.user_roles", "admin");
    let admin_list = output_to_json(purchase::handle_list_admin(&ctx, &msg).await).await;
    assert_eq!(admin_list["records"][0], list["records"][0]);
}

#[tokio::test]
async fn get_purchase_denied_for_other_user() {
    let ctx = ctx().await;

    let mut pd = HashMap::new();
    pd.insert("user_id".to_string(), serde_json::json!("user_1"));
    pd.insert("status".to_string(), serde_json::json!("pending"));
    seed(&ctx, "impresspress__products__purchases", "pur_priv", pd).await;

    // user_2 tries to access user_1's purchase
    let (msg, _input) = get_msg("/b/products/purchases/pur_priv", "user_2");
    let out = purchase::handle_get(&ctx, &msg).await;
    assert!(output_is_error(out, ErrorCode::PermissionDenied).await);
}

#[tokio::test]
async fn get_purchase_not_found() {
    let ctx = ctx().await;

    let (msg, _input) = get_msg("/b/products/purchases/nonexistent", "user_1");
    let out = purchase::handle_get(&ctx, &msg).await;
    assert!(output_is_error(out, ErrorCode::NotFound).await);
}

#[tokio::test]
async fn get_purchase_admin_can_view_any() {
    let ctx = ctx().await;

    let mut pd = HashMap::new();
    pd.insert("user_id".to_string(), serde_json::json!("user_1"));
    pd.insert("status".to_string(), serde_json::json!("completed"));
    seed(&ctx, "impresspress__products__purchases", "pur_any", pd).await;

    let (mut msg, _input) = get_msg("/b/products/purchases/pur_any", "admin_1");
    msg.set_meta("auth.user_roles", "admin");
    let out = purchase::handle_get(&ctx, &msg).await;
    let body = output_to_json(out).await;
    assert!(body["purchase"]["id"].as_str().is_some());
}

// ============================================================
// Refund
// ============================================================

#[tokio::test]
async fn refund_completed_purchase() {
    let ctx = ctx().await;

    let mut pd = HashMap::new();
    pd.insert("user_id".to_string(), serde_json::json!("user_1"));
    pd.insert("status".to_string(), serde_json::json!("completed"));
    pd.insert("total_cents".to_string(), serde_json::json!(5000));
    seed(&ctx, "impresspress__products__purchases", "pur_refund", pd).await;

    let (mut msg, input) = create_msg(
        "/admin/b/products/purchases/pur_refund/refund",
        "admin_1",
        serde_json::json!({"reason": "Customer requested"}),
    );
    msg.set_meta("auth.user_roles", "admin");

    let out = purchase::handle_refund(&ctx, &msg, input).await;
    let body = output_to_json(out).await;
    assert_eq!(body["status"], "succeeded");
    assert_eq!(body["amount_minor"], 5000);
    assert_eq!(body["refunded_total_minor"], 5000);
    let purchase = super::super::repo::purchases::get(&ctx, "pur_refund")
        .await
        .unwrap();
    assert_eq!(purchase.data["status"], "refunded");
    assert_eq!(purchase.data["refund_reason"], "Customer requested");
    assert_eq!(purchase.data["refunded_by"], "admin_1");
}

#[tokio::test]
async fn manual_partial_refund_retry_is_idempotent() {
    let ctx = ctx().await;

    let mut pd = HashMap::new();
    pd.insert("user_id".to_string(), serde_json::json!("user_1"));
    pd.insert("status".to_string(), serde_json::json!("completed"));
    pd.insert("total_cents".to_string(), serde_json::json!(5000));
    seed(
        &ctx,
        "impresspress__products__purchases",
        "pur_manual_retry",
        pd,
    )
    .await;

    let request = serde_json::json!({
        "amount_minor": 2000,
        "idempotency_key": "manual_retry_1",
    });
    let (mut msg, input) = create_msg(
        "/admin/b/products/purchases/pur_manual_retry/refund",
        "admin_1",
        request.clone(),
    );
    msg.set_meta("auth.user_roles", "admin");
    let body = output_to_json(purchase::handle_refund(&ctx, &msg, input).await).await;
    assert_eq!(body["status"], "succeeded");
    assert_eq!(body["amount_minor"], 2000);
    assert_eq!(body["refunded_total_minor"], 2000);

    // A retried delivery of the same request (same idempotency key, e.g.
    // after a timeout) must return the recorded outcome, not deduct again.
    let (mut msg, input) = create_msg(
        "/admin/b/products/purchases/pur_manual_retry/refund",
        "admin_1",
        request,
    );
    msg.set_meta("auth.user_roles", "admin");
    let body = output_to_json(purchase::handle_refund(&ctx, &msg, input).await).await;
    assert_eq!(body["status"], "succeeded");
    assert_eq!(body["amount_minor"], 2000);
    assert_eq!(body["refunded_total_minor"], 2000);

    let purchase = super::super::repo::purchases::get(&ctx, "pur_manual_retry")
        .await
        .unwrap();
    assert_eq!(
        purchase.data["refunded_total_cents"],
        serde_json::json!(2000)
    );
    assert_eq!(
        purchase.data["status"],
        serde_json::json!("partially_refunded")
    );
}

#[tokio::test]
async fn manual_refund_key_reuse_with_different_amount_fails() {
    let ctx = ctx().await;

    let mut pd = HashMap::new();
    pd.insert("user_id".to_string(), serde_json::json!("user_1"));
    pd.insert("status".to_string(), serde_json::json!("completed"));
    pd.insert("total_cents".to_string(), serde_json::json!(5000));
    seed(
        &ctx,
        "impresspress__products__purchases",
        "pur_manual_reuse",
        pd,
    )
    .await;

    let (mut msg, input) = create_msg(
        "/admin/b/products/purchases/pur_manual_reuse/refund",
        "admin_1",
        serde_json::json!({"amount_minor": 2000, "idempotency_key": "manual_reuse_1"}),
    );
    msg.set_meta("auth.user_roles", "admin");
    let body = output_to_json(purchase::handle_refund(&ctx, &msg, input).await).await;
    assert_eq!(body["status"], "succeeded");

    // The same key with a different amount is a client bug, not a retry.
    let (mut msg, input) = create_msg(
        "/admin/b/products/purchases/pur_manual_reuse/refund",
        "admin_1",
        serde_json::json!({"amount_minor": 1000, "idempotency_key": "manual_reuse_1"}),
    );
    msg.set_meta("auth.user_roles", "admin");
    let out = purchase::handle_refund(&ctx, &msg, input).await;
    assert!(output_is_error(out, ErrorCode::InvalidArgument).await);
}

#[tokio::test]
async fn refund_non_completed_fails() {
    let ctx = ctx().await;

    let mut pd = HashMap::new();
    pd.insert("user_id".to_string(), serde_json::json!("user_1"));
    pd.insert("status".to_string(), serde_json::json!("pending"));
    seed(&ctx, "impresspress__products__purchases", "pur_pending", pd).await;

    let (mut msg, input) = create_msg(
        "/admin/b/products/purchases/pur_pending/refund",
        "admin_1",
        serde_json::json!({}),
    );
    msg.set_meta("auth.user_roles", "admin");

    let out = purchase::handle_refund(&ctx, &msg, input).await;
    assert!(output_is_error(out, ErrorCode::InvalidArgument).await);
}

#[tokio::test]
async fn refund_already_refunded_fails() {
    let ctx = ctx().await;

    let mut pd = HashMap::new();
    pd.insert("user_id".to_string(), serde_json::json!("user_1"));
    pd.insert("status".to_string(), serde_json::json!("refunded"));
    seed(&ctx, "impresspress__products__purchases", "pur_already", pd).await;

    let (mut msg, input) = create_msg(
        "/admin/b/products/purchases/pur_already/refund",
        "admin_1",
        serde_json::json!({}),
    );
    msg.set_meta("auth.user_roles", "admin");

    let out = purchase::handle_refund(&ctx, &msg, input).await;
    assert!(output_is_error(out, ErrorCode::InvalidArgument).await);
}

#[tokio::test]
async fn refund_purchase_not_found() {
    let ctx = ctx().await;

    let (mut msg, input) = create_msg(
        "/admin/b/products/purchases/nonexistent/refund",
        "admin_1",
        serde_json::json!({}),
    );
    msg.set_meta("auth.user_roles", "admin");

    let out = purchase::handle_refund(&ctx, &msg, input).await;
    assert!(output_is_error(out, ErrorCode::NotFound).await);
}

#[tokio::test]
async fn refund_without_reason() {
    let ctx = ctx().await;

    let mut pd = HashMap::new();
    pd.insert("user_id".to_string(), serde_json::json!("user_1"));
    pd.insert("status".to_string(), serde_json::json!("completed"));
    pd.insert("total_cents".to_string(), serde_json::json!(1200));
    seed(
        &ctx,
        "impresspress__products__purchases",
        "pur_noreason",
        pd,
    )
    .await;

    let (mut msg, input) = create_msg(
        "/admin/b/products/purchases/pur_noreason/refund",
        "admin_1",
        serde_json::json!({}),
    );
    msg.set_meta("auth.user_roles", "admin");

    let out = purchase::handle_refund(&ctx, &msg, input).await;
    let body = output_to_json(out).await;
    assert_eq!(body["status"], "succeeded");
    assert_eq!(body["refunded_total_minor"], 1200);
}

/// CODE_REVIEW_2026-07-16 "Error semantics fabricate successful defaults":
/// malformed refund JSON must be rejected, not silently treated as "no
/// reason given" (`unwrap_or_default()` used to swallow the parse error).
/// The purchase must be left untouched — no fabricated refund out of a
/// broken request body.
#[tokio::test]
async fn refund_rejects_malformed_json_body() {
    let ctx = ctx().await;

    let mut pd = HashMap::new();
    pd.insert("user_id".to_string(), serde_json::json!("user_1"));
    pd.insert("status".to_string(), serde_json::json!("completed"));
    pd.insert("total_cents".to_string(), serde_json::json!(1200));
    seed(
        &ctx,
        "impresspress__products__purchases",
        "pur_malformed",
        pd,
    )
    .await;

    let mut msg = wafer_run::Message::new("http.request");
    msg.set_meta("req.action", "create");
    msg.set_meta(
        "req.resource",
        "/admin/b/products/purchases/pur_malformed/refund",
    );
    msg.set_meta("auth.user_id", "admin_1");
    msg.set_meta("auth.user_roles", "admin");
    let input = wafer_run::InputStream::from_bytes(b"{not valid json".to_vec());

    let out = purchase::handle_refund(&ctx, &msg, input).await;
    assert!(
        output_is_error(out, ErrorCode::InvalidArgument).await,
        "malformed refund body must be rejected as a bad request"
    );

    let record = super::super::repo::purchases::get(&ctx, "pur_malformed")
        .await
        .expect("purchase still exists");
    assert_eq!(
        record.data.get("status").and_then(|v| v.as_str()),
        Some("completed"),
        "a malformed body must not fabricate a refund"
    );
}

/// A genuine repository failure while applying the refund must surface as an
/// internal-server error, not be folded into the same `rows == 0` branch as
/// the legitimate "purchase isn't in `completed` status" business outcome —
/// `unwrap_or(0)` used to conflate the two, reporting a real outage as the
/// same 400 "can only refund completed purchases" message.
#[tokio::test]
async fn refund_repository_failure_surfaces_as_internal_error() {
    let ctx = ctx().await;

    let mut pd = HashMap::new();
    pd.insert("user_id".to_string(), serde_json::json!("user_1"));
    pd.insert("status".to_string(), serde_json::json!("completed"));
    pd.insert("total_cents".to_string(), serde_json::json!(1200));
    seed(&ctx, "impresspress__products__purchases", "pur_outage", pd).await;

    let ctx = ctx.break_writes();

    let (mut msg, input) = create_msg(
        "/admin/b/products/purchases/pur_outage/refund",
        "admin_1",
        serde_json::json!({"reason": "Customer requested"}),
    );
    msg.set_meta("auth.user_roles", "admin");

    let out = purchase::handle_refund(&ctx, &msg, input).await;
    assert!(
        output_is_error(out, ErrorCode::Internal).await,
        "a genuine repository failure must surface as Internal, not the \
         business-rule 400 used for an already-settled purchase"
    );
}

// ============================================================
// Purchase via user handler routing
// ============================================================

#[tokio::test]
async fn purchase_list_via_user_handler() {
    let ctx = ctx().await;

    let mut pd = HashMap::new();
    pd.insert("user_id".to_string(), serde_json::json!("user_1"));
    pd.insert("status".to_string(), serde_json::json!("pending"));
    seed(&ctx, "impresspress__products__purchases", "pur_route", pd).await;

    let (msg, input) = get_msg("/b/products/purchases", "user_1");
    let out = dispatch_user(&ctx, msg, input).await;
    let body = output_to_json(out).await;
    assert_eq!(body["records"].as_array().unwrap().len(), 1);
}

/// A stored order state outside the contract is a data-integrity error. The
/// projection reports it as an internal error naming the row, never as a
/// `200` carrying a value the schema does not define, and never as a default.
#[tokio::test]
async fn order_rows_outside_the_state_contract_are_an_internal_error() {
    let ctx = ctx().await;
    seed(
        &ctx,
        "impresspress__products__purchases",
        "pur_bad_reconciliation",
        HashMap::from([
            ("user_id".to_string(), serde_json::json!("user_1")),
            ("status".to_string(), serde_json::json!("completed")),
            ("total_cents".to_string(), serde_json::json!(1000)),
            (
                "reconciliation_status".to_string(),
                serde_json::json!("half_done"),
            ),
        ]),
    )
    .await;

    let (msg, _input) = get_msg("/b/products/purchases/pur_bad_reconciliation", "user_1");
    assert!(
        output_is_error(purchase::handle_get(&ctx, &msg).await, ErrorCode::Internal).await,
        "a 200 would publish `half_done`, which the contract does not define"
    );
    let (msg, _input) = get_msg("/b/products/purchases", "user_1");
    assert!(
        output_is_error(
            purchase::handle_list_user(&ctx, &msg).await,
            ErrorCode::Internal
        )
        .await,
        "the list must not publish the row either"
    );

    seed(
        &ctx,
        "impresspress__products__purchases",
        "pur_bad_status",
        HashMap::from([
            ("user_id".to_string(), serde_json::json!("user_2")),
            ("status".to_string(), serde_json::json!("shipped")),
            ("total_cents".to_string(), serde_json::json!(1000)),
            (
                "reconciliation_status".to_string(),
                serde_json::json!("reconciled"),
            ),
        ]),
    )
    .await;
    let (msg, _input) = get_msg("/b/products/purchases/pur_bad_status", "user_2");
    assert!(
        output_is_error(purchase::handle_get(&ctx, &msg).await, ErrorCode::Internal).await,
        "a 200 would publish `shipped`, which is not an order state"
    );
}
