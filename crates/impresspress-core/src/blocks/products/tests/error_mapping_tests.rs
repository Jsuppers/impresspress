//! What a failed database call answers, per file that classifies one.
//!
//! Every products handler used to pair an `ErrorCode::NotFound` arm with an
//! `err_internal` tail, so a WRAP `PermissionDenied` — a
//! [`wafer_run::ResourceGrant`] the block was deployed without, or a row
//! guard that refused — reached the client as
//! `500 Internal server error (ref: …)`. An operator could not tell it from
//! a corrupt row and a caller could not tell it from an outage.
//!
//! Each test below drives a REAL `wrap::check_access` denial: the fixture is
//! a products deployment with the migrations applied (so the tables exist and
//! a refusal is a refusal, not a missing table) whose caller identity holds
//! no grants at all. The paired positive control reads the same route with
//! the grants in place and a row that is genuinely absent, so the 404 the
//! endpoint has always given is pinned alongside the 403 that is new.

use wafer_run::{ErrorCode, ResourceGrant, ResourceType, WaferError};

use super::harness::{
    admin_create_msg, admin_get_msg, create_msg, ctx, ctx_with, dispatch, get_msg, output_to_json,
};
use crate::{
    blocks::products::{handlers, stripe},
    test_support::{output_http_status, TestContext},
};

/// A products fixture whose caller holds no WRAP grants, so every typed
/// database call the block makes is refused by the same `wrap::check_access`
/// the runtime applies.
///
/// The migrations run first (`with_products`), so the refusal is a denial
/// and not a missing table — which is the whole point: the two used to be
/// indistinguishable from outside.
async fn denied() -> TestContext {
    TestContext::with_products().await.with_wrap(
        "test/ungranted",
        Vec::new(),
        Vec::new(),
        "impresspress/admin",
    )
}

/// [`denied`] for the checkout path, which reads its Stripe settings through
/// the config service before it reaches a database call.
///
/// The one grant is `Config`-typed, so the settings resolve and the DATABASE
/// is still ungranted — otherwise the handler refuses at "Stripe is not
/// configured" and never reaches the read under test.
async fn denied_checkout(config: &[(&str, &str)]) -> TestContext {
    let mut ctx = TestContext::with_products().await;
    for (key, value) in config {
        ctx.set_config(key, value);
    }
    ctx.with_wrap(
        "test/ungranted",
        Vec::new(),
        vec![ResourceGrant::read("*", "*").typed(ResourceType::Config)],
        "impresspress/admin",
    )
}

// --- handlers/catalog.rs -------------------------------------------------

#[tokio::test]
async fn catalog_read_denial_is_403_not_500() {
    let ctx = denied().await;
    let (msg, input) = get_msg("/b/products/catalog/prod_absent", "");
    assert_eq!(
        output_http_status(dispatch(&ctx, msg, input).await).await,
        403
    );
}

#[tokio::test]
async fn catalog_read_of_a_missing_product_is_still_404() {
    let ctx = ctx().await;
    let (msg, input) = get_msg("/b/products/catalog/prod_absent", "");
    assert_eq!(
        output_http_status(dispatch(&ctx, msg, input).await).await,
        404
    );
}

// --- handlers/product.rs -------------------------------------------------

#[tokio::test]
async fn admin_product_read_denial_is_403_not_500() {
    let ctx = denied().await;
    let (msg, input) = admin_get_msg("/b/products/api/admin/products/prod_absent");
    assert_eq!(
        output_http_status(dispatch(&ctx, msg, input).await).await,
        403
    );
}

#[tokio::test]
async fn admin_product_read_of_a_missing_product_is_still_404() {
    let ctx = ctx().await;
    let (msg, input) = admin_get_msg("/b/products/api/admin/products/prod_absent");
    assert_eq!(
        output_http_status(dispatch(&ctx, msg, input).await).await,
        404
    );
}

// --- handlers/offers.rs --------------------------------------------------

#[tokio::test]
async fn offer_list_denial_is_403_not_500() {
    let ctx = denied().await;
    let (msg, input) = admin_get_msg("/b/products/api/admin/products/prod_absent/offers");
    assert_eq!(
        output_http_status(dispatch(&ctx, msg, input).await).await,
        403
    );
}

#[tokio::test]
async fn offer_list_for_a_missing_product_is_still_404() {
    let ctx = ctx().await;
    let (msg, input) = admin_get_msg("/b/products/api/admin/products/prod_absent/offers");
    assert_eq!(
        output_http_status(dispatch(&ctx, msg, input).await).await,
        404
    );
}

// --- handlers/commerce.rs ------------------------------------------------

#[tokio::test]
async fn storefront_product_denial_is_403_not_500() {
    let ctx = denied().await;
    let (msg, input) = get_msg("/b/products/storefront/prod_absent", "");
    assert_eq!(
        output_http_status(dispatch(&ctx, msg, input).await).await,
        403
    );
}

#[tokio::test]
async fn storefront_product_that_is_missing_is_still_404() {
    let ctx = ctx().await;
    let (msg, input) = get_msg("/b/products/storefront/prod_absent", "");
    assert_eq!(
        output_http_status(dispatch(&ctx, msg, input).await).await,
        404
    );
}

// --- purchase.rs ---------------------------------------------------------

#[tokio::test]
async fn purchase_read_denial_is_403_not_500() {
    let ctx = denied().await;
    let (msg, input) = get_msg("/b/products/purchases/pur_absent", "buyer_1");
    assert_eq!(
        output_http_status(dispatch(&ctx, msg, input).await).await,
        403
    );
}

#[tokio::test]
async fn purchase_read_of_a_missing_order_is_still_404() {
    let ctx = ctx().await;
    let (msg, input) = get_msg("/b/products/purchases/pur_absent", "buyer_1");
    assert_eq!(
        output_http_status(dispatch(&ctx, msg, input).await).await,
        404
    );
}

// --- pages.rs ------------------------------------------------------------

#[tokio::test]
async fn admin_product_page_denial_is_403_not_500() {
    let ctx = denied().await;
    let (msg, input) = admin_get_msg("/b/products/admin/products/prod_absent");
    assert_eq!(
        output_http_status(dispatch(&ctx, msg, input).await).await,
        403
    );
}

#[tokio::test]
async fn admin_product_page_for_a_missing_product_is_still_404() {
    let ctx = ctx().await;
    let (msg, input) = admin_get_msg("/b/products/admin/products/prod_absent");
    assert_eq!(
        output_http_status(dispatch(&ctx, msg, input).await).await,
        404
    );
}

// --- stripe.rs -----------------------------------------------------------

const STRIPE_CONFIG: &[(&str, &str)] = &[
    ("IMPRESSPRESS__PRODUCTS__STRIPE_SECRET_KEY", "sk_test_x"),
    ("WAFER_RUN_SHARED__FRONTEND_URL", "https://shop.example"),
];

#[tokio::test]
async fn checkout_offer_read_denial_is_403_not_500() {
    let ctx = denied_checkout(STRIPE_CONFIG).await;
    let (msg, input) = create_msg(
        "/b/products/checkout",
        "",
        serde_json::json!({"offer_id": "offer_absent"}),
    );
    assert_eq!(
        output_http_status(stripe::handle_checkout(&ctx, &msg, input).await).await,
        403
    );
}

#[tokio::test]
async fn checkout_for_a_missing_offer_is_still_404() {
    let ctx = ctx_with(STRIPE_CONFIG).await;
    let (msg, input) = create_msg(
        "/b/products/checkout",
        "",
        serde_json::json!({"offer_id": "offer_absent"}),
    );
    assert_eq!(
        output_http_status(stripe::handle_checkout(&ctx, &msg, input).await).await,
        404
    );
}

// --- handlers/provider.rs ------------------------------------------------

#[tokio::test]
async fn provider_error_gives_a_quota_refusal_its_429() {
    assert_eq!(
        output_http_status(handlers::provider::provider_error(
            "Could not list provider operations",
            WaferError::new(ErrorCode::ResourceExhausted, "daily read quota exhausted"),
        ))
        .await,
        429
    );
}

#[tokio::test]
async fn provider_error_keeps_its_other_classifications() {
    for (code, status) in [
        (ErrorCode::NotFound, 404),
        (ErrorCode::PermissionDenied, 403),
        (ErrorCode::InvalidArgument, 400),
        (ErrorCode::FailedPrecondition, 400),
        (ErrorCode::Internal, 500),
    ] {
        assert_eq!(
            output_http_status(handlers::provider::provider_error(
                "Could not list provider operations",
                WaferError::new(code, "cause"),
            ))
            .await,
            status,
            "{code:?}"
        );
    }
}

#[tokio::test]
async fn provider_operation_list_denial_is_403_not_500() {
    let ctx = denied().await;
    let (msg, input) = admin_get_msg("/b/products/api/admin/provider-operations");
    assert_eq!(
        output_http_status(dispatch(&ctx, msg, input).await).await,
        403
    );
}

// --- the sibling reads: a list and a create refuse the same way -----------

/// `repo::products::list_page` is told its table by the block, not by the
/// request, so its `NotFound` stays a 500 (a missing table is a deployment
/// fault, not an empty result). Its `PermissionDenied` is still a 403.
#[tokio::test]
async fn admin_product_list_denial_is_403_not_500() {
    let ctx = denied().await;
    let (msg, input) = admin_get_msg("/b/products/api/admin/products");
    assert_eq!(
        output_http_status(dispatch(&ctx, msg, input).await).await,
        403
    );
}

/// The positive control for the distinction above: an empty table is an
/// empty page, never a 404.
#[tokio::test]
async fn admin_product_list_of_an_empty_table_is_200() {
    let ctx = ctx().await;
    let (msg, input) = admin_get_msg("/b/products/api/admin/products");
    assert_eq!(
        output_http_status(dispatch(&ctx, msg, input).await).await,
        200
    );
}

/// A real row proves the denial tests above fail on the grant and not on the
/// fixture: the same read the denied context refuses answers 200 here, for a
/// product created through the block's own endpoint.
#[tokio::test]
async fn a_granted_read_of_a_present_product_is_200() {
    let ctx = ctx().await;
    let (msg, input) = admin_create_msg(
        "/b/products/api/admin/products",
        serde_json::json!({"name": "Present", "slug": "prod-present"}),
    );
    let created = output_to_json(dispatch(&ctx, msg, input).await).await;
    let id = created["id"].as_str().expect("created product id");

    let (msg, input) = admin_get_msg(&format!("/b/products/api/admin/products/{id}"));
    assert_eq!(
        output_http_status(dispatch(&ctx, msg, input).await).await,
        200
    );
}
