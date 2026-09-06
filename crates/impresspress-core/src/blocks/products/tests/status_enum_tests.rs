//! Every products status column reads and writes through the enum that
//! defines its value set.
//!
//! Two kinds of test live here. The first is the closing argument on review
//! bug **B11** (`the_product_status_and_the_approval_status_are_two_columns`
//! below): the review read `pending_review` and `pending` as one vocabulary
//! spelled two ways, and they are not — they are values of two different
//! columns, written two lines apart, and each is consistent with its own
//! published enum. That test pins the invariant so the two can never be
//! collapsed into one by a later "cleanup".
//!
//! The second is the round-trip table: for every enum this PR introduces,
//! the value serde writes IS the literal the migrations default to and the
//! live rows hold. A variant renamed in `contracts.rs` without a migration
//! changes what the column stores, and only a test that names the literals
//! can see that happen.

use std::collections::HashMap;

use serde_json::json;
use wafer_core::clients::database::Record;
use wafer_run::ErrorCode;

use super::{
    super::{
        contracts::{
            ApprovalStatus, DisputeStatus, EventStatus, OfferStatus, OfferSyncStatus,
            OperationStatus, ProductStatus, ProviderPaymentStatus, RefundStatus, SellerApproval,
            SellerStatus, SubscriptionStatus,
        },
        repo,
    },
    harness::{
        admin_create_msg, create_msg, ctx, ctx_with, dispatch, output_to_json, seed, update_msg,
    },
};

/// The wire spelling of one variant, through the same serde path every
/// write and every published view uses.
fn wire<T: serde::Serialize>(value: T) -> String {
    crate::util::wire_str(&value)
}

/// A record holding one column, for the decode-door tests.
fn record_with(column: &str, value: &str) -> Record {
    Record {
        id: "row_1".to_string(),
        data: HashMap::from([(column.to_string(), json!(value))]),
    }
}

// ---------------------------------------------------------------------------
// B11 — two columns, not two spellings of one
// ---------------------------------------------------------------------------

/// **B11, closed as not reproducible.**
///
/// `handlers::product` writes `status = "pending_review"` and
/// `approval_status = "pending"` two lines apart, and the review read that
/// as one value set spelled inconsistently. It is two columns:
/// `products.status` is the *publication* state a buyer sees (is this
/// listing in the catalog?) and `products.approval_status` is the
/// *moderation* state an administrator acts on (has a human looked at it?).
/// A seller submission sets both, to different values, on purpose.
///
/// This test pins three facts, all of which a "unify the two" change would
/// break: the two columns hold different literals after a submission; each
/// literal decodes through its own enum; and neither enum accepts the
/// other's literal, so the two vocabularies cannot be swapped by accident.
#[tokio::test]
async fn the_product_status_and_the_approval_status_are_two_columns() {
    let test_ctx = ctx_with(&[("WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS", "true")]).await;
    seed(
        &test_ctx,
        repo::seller_accounts::TABLE,
        "seller_b11",
        HashMap::from([
            ("user_id".to_string(), json!("maker_b11")),
            ("status".to_string(), json!(wire(SellerStatus::Active))),
            ("stripe_account_id".to_string(), json!("acct_b11")),
            ("details_submitted".to_string(), json!(true)),
            ("charges_enabled".to_string(), json!(true)),
            ("payouts_enabled".to_string(), json!(true)),
            ("requirements_json".to_string(), json!("{}")),
            ("fee_basis_points".to_string(), json!(250)),
        ]),
    )
    .await;

    let (msg, input) = create_msg(
        "/b/products/api/products",
        "maker_b11",
        json!({"name": "Two columns", "slug": "two-columns"}),
    );
    let created = output_to_json(dispatch(&test_ctx, msg, input).await).await;
    let product_id = created["id"].as_str().expect("product id").to_string();

    let (msg, input) = update_msg(
        &format!("/b/products/api/products/{product_id}"),
        "maker_b11",
        json!({"status": "active"}),
    );
    let submitted = output_to_json(dispatch(&test_ctx, msg, input).await).await;

    // The two columns hold different literals, and the published view
    // republishes each unchanged.
    assert_eq!(submitted["status"], json!("pending_review"));
    assert_eq!(submitted["approval_status"], json!("pending"));

    // Each literal is a variant of its own enum, and of nothing else. This
    // is the sentence B11 claimed was false.
    // Through the repo door, not the table constant: `tests/repo_door.rs`
    // pins that only `repo::products` names that table.
    let row = repo::products::get(&test_ctx, &product_id)
        .await
        .expect("the product row reads back");
    assert_eq!(
        crate::util::enum_column::<ProductStatus>(&row, "status").expect("status decodes"),
        ProductStatus::PendingReview,
    );
    assert_eq!(
        crate::util::enum_column::<ApprovalStatus>(&row, "approval_status")
            .expect("approval_status decodes"),
        ApprovalStatus::Pending,
    );
    assert!(
        crate::util::enum_column::<ProductStatus>(&record_with("status", "pending"), "status")
            .is_err(),
        "`pending` is not a publication state",
    );
    assert!(
        crate::util::enum_column::<ApprovalStatus>(
            &record_with("approval_status", "pending_review"),
            "approval_status",
        )
        .is_err(),
        "`pending_review` is not a moderation state",
    );

    // Approval moves both columns, still to different values.
    let (msg, input) = admin_create_msg(
        &format!("/b/products/api/admin/products/{product_id}/approve"),
        json!({}),
    );
    let approved = output_to_json(dispatch(&test_ctx, msg, input).await).await;
    assert_eq!(approved["status"], json!("active"));
    assert_eq!(approved["approval_status"], json!("approved"));
}

// ---------------------------------------------------------------------------
// The seller ladder
// ---------------------------------------------------------------------------

/// The three copies of the seller-status ladder — `set_admin_suspended`,
/// `sync_account` and `sync_account_event` — are one function, and it
/// answers the same thing they did across every input combination.
#[test]
fn the_seller_ladder_is_one_function() {
    use repo::seller_accounts::ladder;

    // suspended wins over every capability state
    for charges in [false, true] {
        for details in [false, true] {
            assert_eq!(ladder(true, charges, details), SellerStatus::Suspended);
        }
    }
    // charges enabled is `active` regardless of the details flag
    assert_eq!(ladder(false, true, false), SellerStatus::Active);
    assert_eq!(ladder(false, true, true), SellerStatus::Active);
    // details submitted without charges is `restricted`
    assert_eq!(ladder(false, false, true), SellerStatus::Restricted);
    // and neither is `onboarding` — never `not_started`, which is only ever
    // the value `ensure_for_user` inserts.
    assert_eq!(ladder(false, false, false), SellerStatus::Onboarding);
}

/// `SellerAccount.approval_status` names the column it is bound to.
///
/// It used to be `ApprovalStatus`, whose five variants describe the
/// *product* moderation column; `repo::seller_accounts::to_contract` can
/// only ever produce two of them, so three of the five were published as
/// reachable states of a seller account and never were. `SellerApproval` is
/// the two that exist.
#[test]
fn seller_approval_is_the_two_states_a_seller_row_can_hold() {
    assert_eq!(wire(SellerApproval::Approved), "approved");
    assert_eq!(wire(SellerApproval::Suspended), "suspended");

    let suspended = record_with("status", &wire(SellerStatus::Suspended));
    assert_eq!(
        repo::seller_accounts::approval_from_status(
            crate::util::enum_column::<SellerStatus>(&suspended, "status").expect("decodes"),
        ),
        SellerApproval::Suspended,
    );
    for live in [
        SellerStatus::NotStarted,
        SellerStatus::Onboarding,
        SellerStatus::Restricted,
        SellerStatus::Active,
    ] {
        assert_eq!(
            repo::seller_accounts::approval_from_status(live),
            SellerApproval::Approved,
        );
    }
}

// ---------------------------------------------------------------------------
// Round trips: the variant's wire spelling IS the stored literal
// ---------------------------------------------------------------------------

/// Every literal below is one a migration defaults to or a live row holds.
/// A variant renamed without a migration changes what the column stores.
#[test]
fn every_status_enum_round_trips_the_literal_its_column_holds() {
    // `005_commerce_v2.sqlite.sql:173` defaults seller_accounts.status to
    // `not_started`.
    assert_eq!(wire(SellerStatus::NotStarted), "not_started");
    assert_eq!(wire(SellerStatus::Onboarding), "onboarding");
    assert_eq!(wire(SellerStatus::Restricted), "restricted");
    assert_eq!(wire(SellerStatus::Active), "active");
    assert_eq!(wire(SellerStatus::Suspended), "suspended");

    // `005_commerce_v2.sqlite.sql:80` defaults offers.status to `draft`,
    // `:94` defaults offers.sync_status to `not_synced`.
    assert_eq!(wire(OfferStatus::Draft), "draft");
    assert_eq!(wire(OfferStatus::Active), "active");
    assert_eq!(wire(OfferStatus::Archived), "archived");
    assert_eq!(wire(OfferSyncStatus::NotSynced), "not_synced");
    assert_eq!(wire(OfferSyncStatus::Syncing), "syncing");
    assert_eq!(wire(OfferSyncStatus::Synced), "synced");
    assert_eq!(wire(OfferSyncStatus::Failed), "failed");

    // `016_payment_intent_state.sqlite.sql:1` defaults
    // purchases.provider_payment_status to the empty string, which is why
    // the enum has a variant for it — the published contract has always
    // listed `""` beside the five Stripe states.
    assert_eq!(wire(ProviderPaymentStatus::Unset), "");
    assert_eq!(wire(ProviderPaymentStatus::Succeeded), "succeeded");
    assert_eq!(wire(ProviderPaymentStatus::PaymentFailed), "payment_failed");
    assert_eq!(wire(ProviderPaymentStatus::Processing), "processing");
    assert_eq!(
        wire(ProviderPaymentStatus::RequiresAction),
        "requires_action"
    );
    assert_eq!(wire(ProviderPaymentStatus::Canceled), "canceled");

    // `009_commerce_subscription_state.sqlite.sql:3` defaults
    // purchases.subscription_status to the empty string.
    assert_eq!(wire(SubscriptionStatus::Unset), "");
    assert_eq!(wire(SubscriptionStatus::Incomplete), "incomplete");
    assert_eq!(
        wire(SubscriptionStatus::IncompleteExpired),
        "incomplete_expired"
    );
    assert_eq!(wire(SubscriptionStatus::Trialing), "trialing");
    assert_eq!(wire(SubscriptionStatus::Active), "active");
    assert_eq!(wire(SubscriptionStatus::PastDue), "past_due");
    assert_eq!(wire(SubscriptionStatus::Unpaid), "unpaid");
    assert_eq!(wire(SubscriptionStatus::Paused), "paused");
    assert_eq!(wire(SubscriptionStatus::Canceled), "canceled");

    // `015_dispute_ledger.sqlite.sql` — Stripe's eight dispute states.
    assert_eq!(
        wire(DisputeStatus::WarningNeedsResponse),
        "warning_needs_response"
    );
    assert_eq!(
        wire(DisputeStatus::WarningUnderReview),
        "warning_under_review"
    );
    assert_eq!(wire(DisputeStatus::WarningClosed), "warning_closed");
    assert_eq!(wire(DisputeStatus::NeedsResponse), "needs_response");
    assert_eq!(wire(DisputeStatus::UnderReview), "under_review");
    assert_eq!(wire(DisputeStatus::Won), "won");
    assert_eq!(wire(DisputeStatus::Lost), "lost");
    assert_eq!(wire(DisputeStatus::Prevented), "prevented");

    // `008_refund_ledger.sqlite.sql:11` defaults refunds.status to
    // `pending`; `provider_succeeded` is the local-only state between the
    // provider answering and the ledger settling.
    assert_eq!(wire(RefundStatus::Pending), "pending");
    assert_eq!(wire(RefundStatus::ProviderSucceeded), "provider_succeeded");
    assert_eq!(wire(RefundStatus::Succeeded), "succeeded");
    assert_eq!(wire(RefundStatus::Failed), "failed");

    // `003_stripe_events.sqlite.sql:29` defaults stripe_events.status to
    // `pending`.
    assert_eq!(wire(EventStatus::Pending), "pending");
    assert_eq!(wire(EventStatus::Processing), "processing");
    assert_eq!(wire(EventStatus::Failed), "failed");
    assert_eq!(wire(EventStatus::Processed), "processed");
    assert_eq!(wire(EventStatus::DeadLetter), "dead_letter");

    // `005_commerce_v2.sqlite.sql:276` defaults
    // provider_operations.status to `pending`. It differs from the event
    // vocabulary in exactly one variant — `succeeded` where an event says
    // `processed` — which is why the two are separate types.
    assert_eq!(wire(OperationStatus::Pending), "pending");
    assert_eq!(wire(OperationStatus::Processing), "processing");
    assert_eq!(wire(OperationStatus::Failed), "failed");
    assert_eq!(wire(OperationStatus::Succeeded), "succeeded");
    assert_eq!(wire(OperationStatus::DeadLetter), "dead_letter");

    // `001_products_schema.sqlite.sql:23` defaults products.status to
    // `draft`; `005_commerce_v2.sqlite.sql:12` defaults
    // products.approval_status to `approved`.
    assert_eq!(wire(ProductStatus::Draft), "draft");
    assert_eq!(wire(ProductStatus::PendingReview), "pending_review");
    assert_eq!(wire(ProductStatus::Active), "active");
    assert_eq!(wire(ProductStatus::Archived), "archived");
    assert_eq!(wire(ApprovalStatus::Draft), "draft");
    assert_eq!(wire(ApprovalStatus::Pending), "pending");
    assert_eq!(wire(ApprovalStatus::Approved), "approved");
    assert_eq!(wire(ApprovalStatus::Rejected), "rejected");
    assert_eq!(wire(ApprovalStatus::Suspended), "suspended");
}

/// The platform-billing projection has stored the British spelling since
/// `repo::subscriptions::cancel_and_reset_addons` was written, while every
/// Stripe-sourced column stores `canceled`. `repo::subscription_status_rank`
/// used to be the only thing that knew, as a two-arm string match; the alias is
/// now the one place the two spellings meet, so a row holding either reads
/// as the same variant.
#[test]
fn the_legacy_cancelled_spelling_reads_as_canceled() {
    assert_eq!(
        crate::util::enum_column::<SubscriptionStatus>(
            &record_with("subscription_status", "cancelled"),
            "subscription_status",
        )
        .expect("the legacy spelling decodes"),
        SubscriptionStatus::Canceled,
    );
    assert_eq!(
        crate::util::enum_column::<SubscriptionStatus>(
            &record_with("subscription_status", "canceled"),
            "subscription_status",
        )
        .expect("the Stripe spelling decodes"),
        SubscriptionStatus::Canceled,
    );
    // Both are terminal, which is the fact the ranking exists for.
    assert!(SubscriptionStatus::Canceled.is_terminal());
    assert!(!SubscriptionStatus::PastDue.is_terminal());
    // And a same-second delivery may only move toward the more terminal of
    // the two, whichever spelling the stored row holds.
    assert!(!repo::subscription_transition_allowed(
        SubscriptionStatus::Canceled,
        100,
        SubscriptionStatus::Active,
        100,
    ));
    assert!(repo::subscription_transition_allowed(
        SubscriptionStatus::Active,
        100,
        SubscriptionStatus::Canceled,
        100,
    ));
}

/// **The one column this PR deliberately did not type, and why.**
///
/// `impresspress__products__subscriptions.status` is the same vocabulary as
/// the order column, but `SubscriptionView` republishes it verbatim to
/// `GET /b/products/subscription`, and this table has stored the British
/// `cancelled` since `cancel_and_reset_addons` was written. Giving the field
/// the enum would serialize the canonical `canceled` instead, so every
/// subscription cancelled from that release on would report a different
/// string from the rows already in the table.
///
/// This test is the pin on that decision: the write is still the stored
/// literal, the published field still carries it unchanged, and the *reads*
/// are nonetheless reconciled — `SubscriptionStatus` parses the stored
/// spelling to the same variant the Stripe-sourced columns produce. A later
/// change that types the field has to move the stored rows too, and this test
/// is what makes it notice.
#[tokio::test]
async fn the_platform_subscription_view_still_publishes_the_stored_spelling() {
    let ctx = ctx().await;
    seed(
        &ctx,
        repo::subscriptions::SUBSCRIPTIONS_TABLE,
        "sub_row_spelling",
        HashMap::from([
            ("user_id".to_string(), json!("user_spelling")),
            (
                "stripe_subscription_id".to_string(),
                json!("sub_stripe_spelling"),
            ),
            ("plan".to_string(), json!("pro")),
            (
                "status".to_string(),
                json!(wire(SubscriptionStatus::Active)),
            ),
            ("stripe_event_created".to_string(), json!(0)),
        ]),
    )
    .await;

    let rows = repo::subscriptions::cancel_and_reset_addons(&ctx, "sub_stripe_spelling", 1)
        .await
        .expect("cancel ok");
    assert_eq!(rows, 1);

    let view = repo::subscriptions::subscription_for_user(&ctx, "user_spelling")
        .await
        .expect("the subscription reads back")
        .expect("the row exists");
    assert_eq!(
        view.status, "cancelled",
        "the published field is the stored spelling, not the canonical one",
    );
    assert_ne!(
        view.status,
        wire(SubscriptionStatus::Canceled),
        "and the two spellings are still different strings, which is the whole point",
    );

    // The read side is reconciled even though the write side is not: both
    // spellings parse to the one variant.
    assert_eq!(
        crate::util::enum_column::<SubscriptionStatus>(
            &record_with("status", &view.status),
            "status",
        )
        .expect("the stored spelling decodes"),
        SubscriptionStatus::Canceled,
    );
}

/// A stored value outside a column's set is a data-integrity fault, not a
/// default. `util::enum_column` reports it as `Internal` naming the row —
/// the products block used to carry its own copy of that door beside
/// `repo::offers::wire_enum`, and the two disagreed on the empty column.
#[test]
fn a_value_outside_the_set_is_an_internal_fault_naming_the_row() {
    let error = crate::util::enum_column::<SellerStatus>(
        &record_with("status", "half_onboarded"),
        "status",
    )
    .expect_err("an undefined value is refused");
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(
        error.message.contains("row_1") && error.message.contains("half_onboarded"),
        "the message names the row and the value: {}",
        error.message
    );
}
