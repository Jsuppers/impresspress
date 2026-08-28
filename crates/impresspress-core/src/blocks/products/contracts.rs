//! Stable, provider-neutral JSON contracts for the commerce APIs.
//!
//! # `#[schemars(required)]` on `Option<T>` is not decoration
//!
//! `.output::<T>()` generates under schemars' **serialize** contract, so a
//! response schema states what the server emits. That contract already gets
//! `required` right for `#[serde(default)]` and for `skip_serializing_if`.
//! What it does **not** fix is nullability: `Option<T>`'s `JsonSchema` impl
//! calls `allow_null` unconditionally, without consulting the contract, so
//! every `Option<String>` renders as `["string", "null"]` even when the
//! server omits the key rather than writing `null`.
//!
//! `#[schemars(required)]` is the one lever that drops that `null` branch,
//! under either contract. On a non-`Option` field it is genuinely a no-op —
//! which is why it is easy to mistake for inert everywhere — but on an
//! `Option<T>` it narrows `["T", "null"]` back to `"T"`. Paired with
//! `skip_serializing_if = "Option::is_none"` under the serialize contract it
//! does *not* also force the property into `required`, which is exactly the
//! shape these fields have: optional, and never `null` when present.
//!
//! Removing one of these widens the published response contract. Do not strip
//! them from an `Option<T>` field without changing what the handler emits.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use wafer_core::clients::database::{Record, RecordList};
use wafer_run::Message;

use crate::util::{json_map, RecordExt};

pub const COMMERCE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Draft,
    Pending,
    Approved,
    Rejected,
    Suspended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FulfillmentKind {
    None,
    Manual,
    Download,
    Entitlement,
    Webhook,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OfferMode {
    Payment,
    Subscription,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OfferStatus {
    Draft,
    Active,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PricingModel {
    Fixed,
    Components,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecurringInterval {
    Day,
    Week,
    Month,
    Year,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UsageType {
    Licensed,
    Metered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BillingScheme {
    PerUnit,
    Tiered,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaxBehavior {
    #[default]
    Unspecified,
    Inclusive,
    Exclusive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShippingEstimateUnit {
    Hour,
    Day,
    BusinessDay,
    Week,
    Month,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutPresentation {
    #[default]
    Hosted,
    Embedded,
    PaymentLink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VariableKind {
    Number,
    Integer,
    Boolean,
    Date,
    DateTime,
    Select,
    MultiSelect,
    Text,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum VariableVisibility {
    #[default]
    Public,
    Hidden,
    AdminOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VariableDefinition {
    pub key: String,
    pub kind: VariableKind,
    pub label: String,
    #[serde(default)]
    pub help_text: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default_value: Option<Value>,
    #[serde(default)]
    pub allowed_values: Vec<String>,
    #[serde(default)]
    pub minimum: Option<String>,
    #[serde(default)]
    pub maximum: Option<String>,
    #[serde(default)]
    pub step: Option<String>,
    #[serde(default)]
    pub maximum_length: Option<usize>,
    #[serde(default)]
    pub visibility: VariableVisibility,
    #[serde(default)]
    pub sort_order: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Condition {
    #[default]
    Always,
    All {
        conditions: Vec<Condition>,
    },
    Any {
        conditions: Vec<Condition>,
    },
    Not {
        condition: Box<Condition>,
    },
    Present {
        input: String,
    },
    Equals {
        input: String,
        value: Value,
    },
    NotEquals {
        input: String,
        value: Value,
    },
    GreaterThan {
        input: String,
        value: Value,
    },
    GreaterThanOrEqual {
        input: String,
        value: Value,
    },
    LessThan {
        input: String,
        value: Value,
    },
    LessThanOrEqual {
        input: String,
        value: Value,
    },
    In {
        input: String,
        values: Vec<Value>,
    },
    Contains {
        input: String,
        value: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PricingTier {
    /// Inclusive upper bound for this tier. Only the final tier may omit it.
    #[serde(default)]
    pub up_to: Option<u64>,
    pub unit_amount_minor: i64,
    #[serde(default)]
    pub flat_amount_minor: i64,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PackageRounding {
    /// Charge one package for any partially used package.
    #[default]
    Up,
    /// Require the input to be an exact multiple of the package size.
    Exact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AmountRule {
    Fixed {
        unit_amount_minor: i64,
    },
    PerUnit {
        input: String,
        unit_amount_minor: i64,
    },
    FlatPlusPerUnit {
        base_amount_minor: i64,
        input: String,
        unit_amount_minor: i64,
    },
    Lookup {
        input: String,
        prices: BTreeMap<String, i64>,
    },
    Graduated {
        input: String,
        tiers: Vec<PricingTier>,
    },
    Volume {
        input: String,
        tiers: Vec<PricingTier>,
    },
    Package {
        input: String,
        units_per_package: u64,
        package_amount_minor: i64,
        #[serde(default)]
        rounding: PackageRounding,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuantityRule {
    Fixed {
        value: u64,
    },
    FromInput {
        input: String,
        #[serde(default = "one_u64")]
        minimum: u64,
        #[serde(default)]
        maximum: Option<u64>,
    },
}

impl Default for QuantityRule {
    fn default() -> Self {
        Self::Fixed { value: 1 }
    }
}

fn one_u64() -> u64 {
    1
}
fn one_u32() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ComponentRecurrence {
    pub interval: RecurringInterval,
    #[serde(default = "one_u32")]
    pub interval_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OfferComponent {
    pub id: String,
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub sort_order: i32,
    #[serde(default)]
    pub required: bool,
    pub amount: AmountRule,
    #[serde(default)]
    pub quantity: QuantityRule,
    #[serde(default)]
    pub condition: Condition,
    #[serde(default)]
    pub recurrence: Option<ComponentRecurrence>,
    #[serde(default)]
    pub stripe_price_id: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OfferComponentDraft {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub sort_order: i32,
    #[serde(default)]
    pub required: bool,
    pub amount: AmountRule,
    #[serde(default)]
    pub quantity: QuantityRule,
    #[serde(default)]
    pub condition: Condition,
    #[serde(default)]
    pub recurrence: Option<ComponentRecurrence>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutPolicy {
    /// Minimum evaluated item total before provider discounts, tax, or shipping.
    #[serde(default)]
    pub minimum_total_minor: Option<i64>,
    /// Maximum evaluated item total before provider discounts, tax, or shipping.
    #[serde(default)]
    pub maximum_total_minor: Option<i64>,
    #[serde(default)]
    pub allow_promotion_codes: bool,
    #[serde(default)]
    pub automatic_tax: bool,
    #[serde(default)]
    pub collect_billing_address: bool,
    #[serde(default)]
    pub collect_shipping_address: bool,
    #[serde(default)]
    pub allowed_shipping_countries: Vec<String>,
    #[serde(default)]
    pub shipping_options: Vec<ShippingOption>,
    #[serde(default)]
    pub create_customer: bool,
    #[serde(default)]
    pub require_terms_consent: bool,
    #[serde(default)]
    pub trial_days: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShippingDeliveryEstimate {
    #[serde(default)]
    pub minimum: Option<u32>,
    #[serde(default)]
    pub maximum: Option<u32>,
    pub unit: ShippingEstimateUnit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShippingOption {
    pub display_name: String,
    pub amount_minor: i64,
    #[serde(default)]
    pub tax_behavior: TaxBehavior,
    #[serde(default)]
    pub delivery_estimate: Option<ShippingDeliveryEstimate>,
    #[serde(default)]
    pub stripe_shipping_rate_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    pub id: String,
    pub product_id: String,
    pub version: u32,
    pub name: String,
    pub mode: OfferMode,
    pub currency: String,
    pub pricing_model: PricingModel,
    #[serde(default)]
    pub recurring_interval: Option<RecurringInterval>,
    #[serde(default = "one_u32")]
    pub interval_count: u32,
    pub usage_type: UsageType,
    pub billing_scheme: BillingScheme,
    pub tax_behavior: TaxBehavior,
    #[serde(default)]
    pub variables: Vec<VariableDefinition>,
    pub components: Vec<OfferComponent>,
    #[serde(default)]
    pub checkout: CheckoutPolicy,
    #[serde(default)]
    pub stripe_product_id: String,
    #[serde(default)]
    pub stripe_price_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OfferDefinitionRequest {
    pub name: String,
    pub mode: OfferMode,
    pub currency: String,
    pub pricing_model: PricingModel,
    #[serde(default)]
    pub recurring_interval: Option<RecurringInterval>,
    #[serde(default = "one_u32")]
    pub interval_count: u32,
    pub usage_type: UsageType,
    pub billing_scheme: BillingScheme,
    pub tax_behavior: TaxBehavior,
    #[serde(default)]
    pub variables: Vec<VariableDefinition>,
    pub components: Vec<OfferComponentDraft>,
    #[serde(default)]
    pub checkout: CheckoutPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ManagedOffer {
    pub status: OfferStatus,
    pub sync_status: String,
    #[serde(default)]
    pub sync_error: String,
    pub offer: Offer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorefrontOffer {
    pub id: String,
    pub version: u32,
    pub name: String,
    pub mode: OfferMode,
    pub currency: String,
    pub pricing_model: PricingModel,
    #[serde(default)]
    pub recurring_interval: Option<RecurringInterval>,
    pub interval_count: u32,
    pub variables: Vec<VariableDefinition>,
    pub checkout: CheckoutPolicy,
    #[serde(default)]
    pub payment_links: Vec<StorefrontPaymentLink>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorefrontProduct {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub slug: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub image_url: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub fulfillment_kind: FulfillmentKind,
    pub offers: Vec<StorefrontOffer>,
}

/// Which Stripe environment a validated publishable key belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StripeMode {
    Test,
    Live,
}

/// Browser-safe deployment configuration. The Stripe secret key, webhook
/// secret, account ids, and provider API URL are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorefrontConfig {
    pub schema_version: u32,
    pub embedded_checkout_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub stripe_publishable_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub stripe_mode: Option<StripeMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PricingPreviewRequest {
    pub offer_id: String,
    #[serde(default = "one_u64")]
    #[schemars(range(min = 1))]
    pub quantity: u64,
    #[serde(default)]
    pub inputs: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolvedComponent {
    pub component_id: String,
    pub key: String,
    pub label: String,
    pub included: bool,
    pub required: bool,
    pub unit_amount_minor: i64,
    pub quantity: u64,
    pub total_amount_minor: i64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoneyBreakdown {
    pub currency: String,
    pub subtotal_minor: i64,
    pub discount_minor: i64,
    pub tax_minor: i64,
    #[serde(default)]
    pub shipping_minor: i64,
    pub platform_fee_minor: i64,
    pub total_minor: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PricingPreview {
    pub schema_version: u32,
    pub offer_id: String,
    pub offer_version: u32,
    pub quantity: u64,
    pub inputs: BTreeMap<String, Value>,
    pub components: Vec<ResolvedComponent>,
    pub amounts: MoneyBreakdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutRequest {
    pub offer_id: String,
    #[serde(default)]
    pub preset_id: Option<String>,
    #[serde(default = "one_u64")]
    #[schemars(range(min = 1))]
    pub quantity: u64,
    #[serde(default)]
    pub inputs: BTreeMap<String, Value>,
    #[serde(default)]
    pub presentation: CheckoutPresentation,
    #[serde(default)]
    #[schemars(url)]
    pub success_url: Option<String>,
    #[serde(default)]
    #[schemars(url)]
    pub cancel_url: Option<String>,
    #[serde(default)]
    #[schemars(email)]
    pub buyer_email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutResponse {
    pub order_id: String,
    /// Returned once and never persisted in plaintext. Static storefronts use
    /// it to poll the minimal guest order-status endpoint after Stripe returns.
    ///
    /// A bearer capability: whoever holds it can read the order's status.
    /// Treat it like a session token — never log it, never put it in a URL
    /// that gets shared. This response is its only delivery, which is why it
    /// is not `writeOnly`: that keyword claims a field is never present in a
    /// response, and this one is always present in this one.
    pub receipt_token: String,
    #[schemars(extend("format" = "date-time"))]
    pub receipt_token_expires_at: String,
    pub presentation: CheckoutPresentation,
    #[serde(default)]
    #[schemars(url)]
    pub checkout_url: Option<String>,
    /// Stripe Embedded Checkout client secret. Present only for the embedded
    /// presentation; it is how the browser opens the session, so it is
    /// returned here and nowhere else. Same handling as `receipt_token`:
    /// never log it.
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    #[schemars(url)]
    pub payment_link_url: Option<String>,
    pub amounts: MoneyBreakdown,
}

/// Minimal order state exposed to a guest who presents the checkout receipt
/// capability. Buyer details and all Stripe resource ids remain private.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GuestOrderStatus {
    pub schema_version: u32,
    pub order_id: String,
    pub status: String,
    pub reconciliation_status: String,
    pub amounts: MoneyBreakdown,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(required)]
    pub subscription_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    #[schemars(required)]
    pub subscription_current_period_end: Option<String>,
    pub subscription_cancel_at_period_end: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    #[schemars(required)]
    pub paid_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    #[schemars(required)]
    pub refunded_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutPresetRequest {
    pub name: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutPreset {
    pub id: String,
    pub offer_id: String,
    pub name: String,
    pub slug: String,
    pub inputs: BTreeMap<String, Value>,
    pub active: bool,
    pub configuration_hash: String,
}

/// Every checkout preset defined for one offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutPresetList {
    pub presets: Vec<CheckoutPreset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PaymentLinkCreateRequest {
    #[serde(default)]
    pub preset_id: Option<String>,
    /// Where Stripe sends the buyer once the Payment Link is paid.
    #[serde(default)]
    #[schemars(url)]
    pub after_completion_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ManagedPaymentLink {
    pub id: String,
    pub offer_id: String,
    #[serde(default)]
    pub preset_id: String,
    /// Stripe-hosted Payment Link the buyer opens.
    #[schemars(url)]
    pub url: String,
    pub active: bool,
    pub configuration_hash: String,
    pub sync_status: String,
    #[serde(default)]
    pub sync_error: String,
}

/// Every reusable Stripe Payment Link synchronized for one offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PaymentLinkList {
    pub payment_links: Vec<ManagedPaymentLink>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StorefrontPaymentLink {
    pub id: String,
    #[serde(default)]
    pub preset_id: String,
    pub url: String,
    /// Immutable server-resolved pricing captured when the reusable link was
    /// synchronized. This lets static pages display the link's actual price
    /// without issuing a runtime checkout or evaluating unrelated inputs.
    pub pricing: PricingPreview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SellerCapabilities {
    pub details_submitted: bool,
    pub charges_enabled: bool,
    pub payouts_enabled: bool,
    #[serde(default)]
    pub requirements_due: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SellerAccount {
    pub id: String,
    pub user_id: String,
    pub status: String,
    pub approval_status: ApprovalStatus,
    #[serde(default)]
    pub stripe_account_id: String,
    pub capabilities: SellerCapabilities,
    /// Platform fee applied to this seller's sales, in basis points.
    #[schemars(range(max = 10000))]
    pub fee_basis_points: u32,
    #[serde(default)]
    pub livemode: bool,
    #[serde(default)]
    pub country: String,
    #[serde(default)]
    pub default_currency: String,
    #[serde(default)]
    pub dashboard_type: String,
    #[serde(default)]
    pub disabled_reason: String,
    #[serde(default)]
    pub sync_error: String,
    #[serde(default)]
    pub last_synced_at: String,
}

/// Every seller account known to the platform, with its capability state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SellerAccountList {
    pub sellers: Vec<SellerAccount>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StripeConnectionState {
    NotConfigured,
    ConnectedTest,
    ConnectedLive,
    Misconfigured,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StripeConnectionStatus {
    pub state: StripeConnectionState,
    pub configured: bool,
    pub livemode: bool,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub country: String,
    #[serde(default)]
    pub default_currency: String,
    #[serde(default)]
    pub business_name: String,
    pub charges_enabled: bool,
    pub payouts_enabled: bool,
    pub details_submitted: bool,
    #[serde(default)]
    pub capabilities: BTreeMap<String, String>,
    pub publishable_key_configured: bool,
    pub webhook_secret_configured: bool,
    pub api_version: String,
    #[serde(default)]
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SellerOnboardingRequest {
    /// Where Stripe returns the seller once onboarding is submitted.
    #[schemars(url)]
    pub return_url: String,
    /// Where Stripe returns the seller if the onboarding link expired.
    #[schemars(url)]
    pub refresh_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SellerOnboardingResponse {
    pub account: SellerAccount,
    /// Single-use Stripe-hosted onboarding link.
    #[schemars(url)]
    pub url: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderRedirect {
    /// Absolute provider-hosted URL the browser must be sent to.
    #[schemars(url)]
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BillingPortalRequest {
    /// Where Stripe returns the customer when they leave the portal.
    #[schemars(url)]
    pub return_url: String,
    #[serde(default)]
    pub order_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RefundReason {
    Duplicate,
    Fraudulent,
    RequestedByCustomer,
}

impl RefundReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::Fraudulent => "fraudulent",
            Self::RequestedByCustomer => "requested_by_customer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RefundRequest {
    /// Exact amount in the order currency's minor unit. Omit to refund the
    /// complete remaining refundable amount.
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub amount_minor: Option<i64>,
    /// Stripe's constrained provider reason. Human context belongs in `note`.
    #[serde(default)]
    pub provider_reason: Option<RefundReason>,
    /// Private operator note retained in ImpressPress, never sent to Stripe.
    #[serde(default, alias = "reason")]
    #[schemars(length(max = 500))]
    pub note: Option<String>,
    /// Stable client operation key. Supplying a fresh key allows a deliberate
    /// second partial refund of the same amount; retries must reuse the key.
    #[serde(default)]
    #[schemars(length(max = 80))]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RefundResultStatus {
    Pending,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RefundResult {
    pub purchase_id: String,
    #[serde(default)]
    pub refund_id: String,
    #[serde(default)]
    pub provider_refund_id: String,
    pub status: RefundResultStatus,
    #[serde(default)]
    pub provider_status: String,
    pub amount_minor: i64,
    pub refunded_total_minor: i64,
    pub order_total_minor: i64,
    pub currency: String,
    pub livemode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommerceAnalytics {
    pub currency: String,
    pub gross_volume_minor: i64,
    pub refunded_volume_minor: i64,
    pub net_volume_minor: i64,
    pub platform_fees_minor: i64,
    pub order_count: u64,
    pub paid_order_count: u64,
    pub refunded_order_count: u64,
    pub failed_order_count: u64,
    pub open_dispute_count: u64,
    pub open_disputed_volume_minor: i64,
    pub lost_dispute_count: u64,
    pub lost_disputed_volume_minor: i64,
    pub active_subscription_count: u64,
    pub trialing_subscription_count: u64,
    pub past_due_subscription_count: u64,
    pub canceled_subscription_count: u64,
    pub top_products: Vec<AnalyticsProduct>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyticsProduct {
    pub product_id: String,
    pub name: String,
    pub quantity: u64,
    pub revenue_minor: i64,
}

/// Ownership-safe failed-order projection for seller operational dashboards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SellerFailureSummary {
    pub order_id: String,
    pub status: String,
    pub currency: String,
    pub total_minor: i64,
    #[serde(default)]
    pub error: String,
    #[schemars(extend("format" = "date-time"))]
    pub created_at: String,
}

/// Platform-wide commerce counters for the admin dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdminStats {
    pub total_products: i64,
    pub active_products: i64,
    pub total_purchases: i64,
    /// One entry per currency the platform has transacted in.
    pub currency_analytics: Vec<CommerceAnalytics>,
    pub total_groups: i64,
}

/// One seller's own commerce counters and recent operational failures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SellerStats {
    /// Empty when the user has not started Stripe onboarding.
    pub seller_account_id: String,
    /// One entry per currency this seller has transacted in.
    pub currency_analytics: Vec<CommerceAnalytics>,
    pub recent_failures: Vec<SellerFailureSummary>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Acknowledgement returned to Stripe for a delivered webhook event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WebhookAck {
    pub received: bool,
    /// Present only when the event id had already been recorded, in which
    /// case no side effect ran.
    #[serde(default, skip_serializing_if = "is_false")]
    pub duplicate: bool,
    /// Present only when the event exhausted its retry budget.
    #[serde(default, skip_serializing_if = "is_false")]
    pub dead_letter: bool,
}

impl WebhookAck {
    /// The event was accepted and processed.
    pub fn received() -> Self {
        Self {
            received: true,
            duplicate: false,
            dead_letter: false,
        }
    }

    /// The event id was already recorded, so no side effect ran.
    pub fn duplicate() -> Self {
        Self {
            received: true,
            duplicate: true,
            dead_letter: false,
        }
    }

    /// The event exhausted its retry budget.
    pub fn dead_letter() -> Self {
        Self {
            received: true,
            duplicate: false,
            dead_letter: true,
        }
    }
}

/// Safe operational projection of a Stripe event. The signed payload and
/// processing owner are never serialized to the admin API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebhookEventSummary {
    pub id: String,
    pub event_type: String,
    /// One of `pending`, `processing`, `failed`, `processed`, `dead_letter`.
    #[schemars(extend("enum" = ["pending", "processing", "failed", "processed", "dead_letter"]))]
    pub status: String,
    #[serde(default)]
    pub stripe_account_id: String,
    pub livemode: bool,
    pub attempts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    #[schemars(required)]
    pub processing_started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    #[schemars(required)]
    pub next_retry_at: Option<String>,
    #[serde(default)]
    pub last_error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    #[schemars(required)]
    pub processed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    #[schemars(required)]
    pub terminal_at: Option<String>,
    #[schemars(extend("format" = "date-time"))]
    pub created_at: String,
    #[schemars(extend("format" = "date-time"))]
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebhookEventList {
    pub records: Vec<WebhookEventSummary>,
    pub total_count: i64,
    pub page: i64,
    pub page_size: i64,
}

/// Safe administrator projection of a durable Stripe provider operation.
/// Request/response payloads, idempotency keys, and lease owners stay private.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderOperationSummary {
    pub id: String,
    /// Currently only `refund.reconcile`.
    #[schemars(extend("enum" = ["refund.reconcile"]))]
    pub operation_type: String,
    /// Currently only `refund`.
    #[schemars(extend("enum" = ["refund"]))]
    pub aggregate_type: String,
    pub aggregate_id: String,
    #[serde(default)]
    pub stripe_account_id: String,
    /// One of `pending`, `processing`, `failed`, `succeeded`, `dead_letter`.
    #[schemars(extend("enum" = ["pending", "processing", "failed", "succeeded", "dead_letter"]))]
    pub status: String,
    pub attempts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    #[schemars(required)]
    pub processing_started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    #[schemars(required)]
    pub next_attempt_at: Option<String>,
    #[serde(default)]
    pub last_error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    #[schemars(required)]
    pub completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("format" = "date-time"))]
    #[schemars(required)]
    pub terminal_at: Option<String>,
    #[schemars(extend("format" = "date-time"))]
    pub created_at: String,
    #[schemars(extend("format" = "date-time"))]
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderOperationList {
    pub records: Vec<ProviderOperationSummary>,
    pub total_count: i64,
    pub page: i64,
    pub page_size: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderReconcileResult {
    pub claimed: u64,
    pub succeeded: u64,
    pub retry_scheduled: u64,
    pub dead_letter: u64,
}

// ---------------------------------------------------------------------------
// Catalog rows — typed views of the block's own tables
// ---------------------------------------------------------------------------
//
// Until these existed the product, group, type and purchase endpoints echoed
// the database layer's `Record` / `RecordList` (`{id, data: {column → value}}`)
// straight to the wire, so a response was whatever the row held and its JSON
// types depended on the backend (`tags` / `metadata` are JSON-encoded `TEXT`
// that only SQLite decodes; `is_system` / `livemode` are `INTEGER`). Each view
// below is a closed field list built column by column, normalized through
// `RecordExt`, so a column added by a migration is never published by
// accident and one schema is true on SQLite, D1 and Postgres.
//
// The `{records, total_count, page, page_size}` list envelope is unchanged;
// only the row moves from `{id, data: {…}}` to the flat view.

/// A nullable timestamp column as `Option<String>`.
///
/// The moderation path writes `published_at = ""` when it returns a product to
/// draft, so the column holds `NULL`, the empty string, or an RFC 3339
/// timestamp. The empty string is not a `date-time`; it reads as `None` so the
/// declared format is true.
fn timestamp_field(record: &Record, key: &str) -> Option<String> {
    record.opt_str_field(key).filter(|value| !value.is_empty())
}

/// An absent query parameter and an empty one mean the same thing to every
/// filter here (`msg.query` returns `""` for both), so collapse them onto
/// `None` rather than letting `Some("")` reach a `LIKE '%%'`.
fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

/// Default page size for every paginated list in this block.
const DEFAULT_PAGE_SIZE: u32 = 20;

/// `?page` default.
fn default_page() -> u32 {
    1
}

fn default_page_size() -> u32 {
    DEFAULT_PAGE_SIZE
}

/// Turn a request struct into the column map the database client writes.
///
/// Every optional field carries `skip_serializing_if = "Option::is_none"`, so
/// a key the client did not send is not written — an update touches only what
/// arrived, and an explicit `null` is treated the same as an absent key rather
/// than becoming a `NULL` write against a `NOT NULL` column.
fn columns<T: Serialize>(request: &T) -> HashMap<String, Value> {
    json_map(serde_json::to_value(request).expect("request structs serialize"))
}

/// Publication state of a product row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProductStatus {
    Draft,
    PendingReview,
    Active,
    Archived,
}

/// A product row as published to its owner and to administrators: every
/// column of `impresspress__products__products`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProductView {
    /// Stable product identifier.
    pub id: String,
    pub name: String,
    pub description: String,
    /// URL slug, unique per owner among non-deleted products. Empty when the
    /// product has none.
    pub slug: String,
    /// ISO 4217 presentment currency.
    pub currency: String,
    /// Publication state: `draft`, `pending_review` (seller product awaiting
    /// moderation), `active` (in the public catalog) or `archived`.
    #[schemars(extend("enum" = ["draft", "pending_review", "active", "archived"]))]
    pub status: String,
    pub category: String,
    pub tags: Vec<String>,
    /// Free-form key/value metadata attached by the product builder.
    pub metadata: serde_json::Map<String, Value>,
    pub image_url: String,
    /// Units in stock; `0` when inventory is not tracked.
    pub stock: i64,
    /// Group the product is listed under, or empty.
    pub group_id: String,
    /// Product type (taxonomy) id, or empty.
    pub type_id: String,
    pub group_template_id: String,
    /// Product builder template this product was created from.
    pub product_template_id: String,
    /// Id of a product the buyer must already own before checkout, or empty.
    pub requires: String,
    /// Id of the user who created the row.
    pub created_by: String,
    /// `platform` for an administrator-owned product, `user` for a seller's.
    #[schemars(extend("enum" = ["platform", "user"]))]
    pub owner_kind: String,
    /// Owning seller's user id; empty for platform products.
    pub owner_id: String,
    /// Seller account the product sells through; empty for platform products.
    pub seller_account_id: String,
    /// Moderation state: `draft`, `pending` (submitted for review), `approved`,
    /// `rejected` or `suspended`.
    #[schemars(extend("enum" = ["draft", "pending", "approved", "rejected", "suspended"]))]
    pub approval_status: String,
    /// How a purchase is fulfilled.
    #[schemars(extend("enum" = ["none", "manual", "download", "entitlement", "webhook"]))]
    pub fulfillment_kind: String,
    /// Stripe Product id once the catalog has been synchronized, or empty.
    pub stripe_product_id: String,
    /// Version counter of the product's immutable offer definitions.
    #[schemars(range(min = 1))]
    pub current_version: i64,
    /// RFC 3339 timestamp the seller last submitted the product for
    /// moderation, or `null`.
    #[schemars(extend("format" = "date-time"))]
    pub submitted_at: Option<String>,
    /// RFC 3339 timestamp the product last became active, or `null`.
    #[schemars(extend("format" = "date-time"))]
    pub published_at: Option<String>,
    /// RFC 3339 soft-delete timestamp, or `null` unless the product has been
    /// soft-deleted.
    #[schemars(extend("format" = "date-time"))]
    pub deleted_at: Option<String>,
    /// RFC 3339 creation timestamp.
    #[schemars(extend("format" = "date-time"))]
    pub created_at: String,
    /// RFC 3339 timestamp of the last modification.
    #[schemars(extend("format" = "date-time"))]
    pub updated_at: String,
}

impl ProductView {
    /// Project an `impresspress__products__products` row.
    pub fn from_record(record: &Record) -> Self {
        Self {
            id: record.id.clone(),
            name: record.str_field("name").to_string(),
            description: record.str_field("description").to_string(),
            slug: record.str_field("slug").to_string(),
            currency: record.str_field("currency").to_string(),
            status: record.str_field("status").to_string(),
            category: record.str_field("category").to_string(),
            tags: record.string_list_field("tags"),
            metadata: record.json_object_field("metadata"),
            image_url: record.str_field("image_url").to_string(),
            stock: record.i64_field("stock"),
            group_id: record.str_field("group_id").to_string(),
            type_id: record.str_field("type_id").to_string(),
            group_template_id: record.str_field("group_template_id").to_string(),
            product_template_id: record.str_field("product_template_id").to_string(),
            requires: record.str_field("requires").to_string(),
            created_by: record.str_field("created_by").to_string(),
            owner_kind: record.str_field("owner_kind").to_string(),
            owner_id: record.str_field("owner_id").to_string(),
            seller_account_id: record.str_field("seller_account_id").to_string(),
            approval_status: record.str_field("approval_status").to_string(),
            fulfillment_kind: record.str_field("fulfillment_kind").to_string(),
            stripe_product_id: record.str_field("stripe_product_id").to_string(),
            current_version: record.i64_field("current_version"),
            submitted_at: timestamp_field(record, "submitted_at"),
            published_at: timestamp_field(record, "published_at"),
            deleted_at: timestamp_field(record, "deleted_at"),
            created_at: record.str_field("created_at").to_string(),
            updated_at: record.str_field("updated_at").to_string(),
        }
    }
}

/// One page of product rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProductListResponse {
    /// Products on this page, newest first.
    pub records: Vec<ProductView>,
    /// Total products matching the filters, across all pages.
    pub total_count: i64,
    /// 1-based index of this page.
    pub page: i64,
    /// Rows per page used to compute `page`.
    pub page_size: i64,
}

impl ProductListResponse {
    /// Project a `RecordList` of product rows.
    pub fn from_record_list(list: &RecordList) -> Self {
        Self {
            records: list.records.iter().map(ProductView::from_record).collect(),
            total_count: list.total_count,
            page: list.page,
            page_size: list.page_size,
        }
    }
}

/// Query parameters accepted by the list endpoints that paginate and do
/// nothing else.
///
/// Built by [`Self::from_message`], which is the handler's only source for
/// these values — the type is the parser, not a parallel description of one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub struct PageQuery {
    /// 1-based page number. Values below 1 clamp to 1.
    #[serde(default = "default_page")]
    pub page: u32,
    /// Rows per page, capped at 100.
    #[serde(default = "default_page_size")]
    pub page_size: u32,
}

impl PageQuery {
    /// Resolve the query string on `msg`, applying the same defaults and
    /// clamps the handler applied inline before this type existed.
    pub fn from_message(msg: &Message) -> Self {
        let (page, page_size, _) = msg.pagination_params(DEFAULT_PAGE_SIZE as usize);
        Self {
            page: page as u32,
            page_size: page_size as u32,
        }
    }
}

/// Query parameters accepted by the product list endpoints.
///
/// Built by [`Self::from_message`], which is the handler's only source for
/// these values — the type is the parser, not a parallel description of one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub struct ProductListQuery {
    /// 1-based page number. Values below 1 clamp to 1.
    #[serde(default = "default_page")]
    pub page: u32,
    /// Rows per page, capped at 100.
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    /// Exact-match filter on the product's group.
    pub group_id: Option<String>,
    /// Exact-match filter on the publication state.
    pub status: Option<String>,
    /// `LIKE '%…%'` filter on the product name.
    pub search: Option<String>,
}

impl ProductListQuery {
    /// Resolve the query string on `msg`, applying the same defaults and
    /// clamps the handler applied inline before this type existed.
    pub fn from_message(msg: &Message) -> Self {
        let (page, page_size, _) = msg.pagination_params(DEFAULT_PAGE_SIZE as usize);
        Self {
            page: page as u32,
            page_size: page_size as u32,
            group_id: non_empty(msg.query("group_id")),
            status: non_empty(msg.query("status")),
            search: non_empty(msg.query("search")),
        }
    }
}

// The write requests are closed field lists too, and that is the point: the
// column-map path they replace wrote every key it was sent. On the seller
// create path that included `seller_account_id`, `stripe_product_id`,
// `current_version`, `published_at`, `submitted_at` and `deleted_at` — the
// update path stripped them, the create path did not. A field that is not
// declared here cannot reach the row from any tier. Unknown keys are ignored
// rather than refused so that a client sending one of those protected columns
// gets the same answer it always did on update: the row, unchanged in that
// column.
/// `POST /b/products/api/admin/products` and `POST /b/products/api/products`
/// request body. Ownership, moderation and provider columns are set by the
/// server and cannot be supplied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateProductRequest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// ISO 4217 currency. A seller product defaults to the platform default
    /// currency when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Initial publication state. Defaults to `draft`; a seller product is
    /// always created as `draft` regardless of this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ProductStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Free-form key/value metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stock: Option<i64>,
    /// Group to list the product under. A seller may only use a group they
    /// own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_template_id: Option<String>,
    /// Product builder template. A seller product defaults to the seeded
    /// `default` template when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_template_id: Option<String>,
    /// Id of a product the buyer must already own before checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fulfillment_kind: Option<FulfillmentKind>,
}

impl CreateProductRequest {
    /// The columns this request writes: only the fields that were sent.
    pub fn into_columns(self) -> HashMap<String, Value> {
        columns(&self)
    }
}

/// `PATCH /b/products/api/admin/products/{id}` and
/// `PATCH /b/products/api/products/{id}` request body. Every field is
/// optional and only the ones present are applied. Ownership, moderation and
/// provider columns are set by the server and cannot be supplied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UpdateProductRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// ISO 4217 currency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Publication state. A seller may set `draft`, `active` or `archived`;
    /// `active` on a moderated product submits it for review instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ProductStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Free-form key/value metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stock: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_template_id: Option<String>,
    /// Product builder template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_template_id: Option<String>,
    /// Id of a product the buyer must already own before checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fulfillment_kind: Option<FulfillmentKind>,
}

impl UpdateProductRequest {
    /// The columns this request writes: only the fields that were sent.
    pub fn into_columns(self) -> HashMap<String, Value> {
        columns(&self)
    }
}

// Not declared through `.output::<T>()`: `ManagedOffer` reaches the recursive
// `Condition`, so the endpoint schema for this response stays hand-written in
// `mod.rs` (`product_duplicate_schema`), with the `product` half derived from
// `ProductView`. The handler still builds this type, so the wire shape has one
// source.
/// Response body of the product duplication endpoints: the new draft product
/// and its copied, editable offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProductDuplicateResponse {
    pub product: ProductView,
    pub offers: Vec<ManagedOffer>,
}

/// A product group row: every column of `impresspress__products__groups`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GroupView {
    /// Stable group identifier.
    pub id: String,
    pub name: String,
    pub description: String,
    /// Group template the group was created from.
    pub group_template_id: String,
    /// Id of the user who owns the group. The owner tier lists and edits
    /// only groups whose `user_id` is the caller.
    pub user_id: String,
    /// `active` unless the group has been retired.
    pub status: String,
    /// Id of the user who created the row; empty when the row was created
    /// through the admin API, which records the owner in `user_id` instead.
    pub created_by: String,
    /// RFC 3339 creation timestamp.
    #[schemars(extend("format" = "date-time"))]
    pub created_at: String,
    /// RFC 3339 timestamp of the last modification.
    #[schemars(extend("format" = "date-time"))]
    pub updated_at: String,
}

impl GroupView {
    /// Project an `impresspress__products__groups` row.
    pub fn from_record(record: &Record) -> Self {
        Self {
            id: record.id.clone(),
            name: record.str_field("name").to_string(),
            description: record.str_field("description").to_string(),
            group_template_id: record.str_field("group_template_id").to_string(),
            user_id: record.str_field("user_id").to_string(),
            status: record.str_field("status").to_string(),
            created_by: record.str_field("created_by").to_string(),
            created_at: record.str_field("created_at").to_string(),
            updated_at: record.str_field("updated_at").to_string(),
        }
    }
}

/// One page of group rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GroupListResponse {
    /// Groups on this page.
    pub records: Vec<GroupView>,
    /// Total groups matching the filters, across all pages.
    pub total_count: i64,
    /// 1-based index of this page.
    pub page: i64,
    /// Rows per page used to compute `page`.
    pub page_size: i64,
}

impl GroupListResponse {
    /// Project a `RecordList` of group rows.
    pub fn from_record_list(list: &RecordList) -> Self {
        Self {
            records: list.records.iter().map(GroupView::from_record).collect(),
            total_count: list.total_count,
            page: list.page,
            page_size: list.page_size,
        }
    }
}

/// `POST /b/products/api/admin/groups` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateGroupRequest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_template_id: Option<String>,
    /// Owner of the group. Defaults to the administrator creating it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

impl CreateGroupRequest {
    /// The columns this request writes: only the fields that were sent.
    pub fn into_columns(self) -> HashMap<String, Value> {
        columns(&self)
    }
}

/// `PATCH /b/products/api/admin/groups/{id}` request body. Every field is
/// optional and only the ones present are applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UpdateGroupRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_template_id: Option<String>,
    /// Owner of the group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

impl UpdateGroupRequest {
    /// The columns this request writes: only the fields that were sent.
    pub fn into_columns(self) -> HashMap<String, Value> {
        columns(&self)
    }
}

/// `POST /b/products/groups` request body. The owner is always the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateOwnGroupRequest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Group template. Defaults to the seeded `default` template when
    /// omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_template_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

impl CreateOwnGroupRequest {
    /// The columns this request writes: only the fields that were sent.
    pub fn into_columns(self) -> HashMap<String, Value> {
        columns(&self)
    }
}

/// `PATCH /b/products/groups/{id}` request body. Every field is optional
/// and only the ones present are applied; the owner cannot be changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UpdateOwnGroupRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_template_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

impl UpdateOwnGroupRequest {
    /// The columns this request writes: only the fields that were sent.
    pub fn into_columns(self) -> HashMap<String, Value> {
        columns(&self)
    }
}

/// A product type (taxonomy) row: every column of
/// `impresspress__products__types`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProductTypeView {
    /// Stable type identifier.
    pub id: String,
    pub name: String,
    pub description: String,
    /// Whether the type is built in. System types are seeded by the block
    /// rather than created through the API.
    pub is_system: bool,
    /// RFC 3339 creation timestamp.
    #[schemars(extend("format" = "date-time"))]
    pub created_at: String,
    /// RFC 3339 timestamp of the last modification.
    #[schemars(extend("format" = "date-time"))]
    pub updated_at: String,
}

impl ProductTypeView {
    /// Project an `impresspress__products__types` row.
    pub fn from_record(record: &Record) -> Self {
        Self {
            id: record.id.clone(),
            name: record.str_field("name").to_string(),
            description: record.str_field("description").to_string(),
            is_system: record.bool_field("is_system"),
            created_at: record.str_field("created_at").to_string(),
            updated_at: record.str_field("updated_at").to_string(),
        }
    }
}

/// One page of product type rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProductTypeListResponse {
    /// Types on this page, newest first.
    pub records: Vec<ProductTypeView>,
    /// Total types, across all pages.
    pub total_count: i64,
    /// 1-based index of this page.
    pub page: i64,
    /// Rows per page used to compute `page`.
    pub page_size: i64,
}

impl ProductTypeListResponse {
    /// Project a `RecordList` of type rows.
    pub fn from_record_list(list: &RecordList) -> Self {
        Self {
            records: list
                .records
                .iter()
                .map(ProductTypeView::from_record)
                .collect(),
            total_count: list.total_count,
            page: list.page,
            page_size: list.page_size,
        }
    }
}

/// `POST /b/products/api/admin/types` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateProductTypeRequest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the type is built in. Defaults to `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_system: Option<bool>,
}

impl CreateProductTypeRequest {
    /// The columns this request writes: only the fields that were sent.
    ///
    /// `is_system` is an `INTEGER` column on every backend (the Postgres
    /// migration mirrors SQLite's `INTEGER NOT NULL DEFAULT 0` deliberately),
    /// so the boolean is written as `0` / `1` rather than bound as a boolean
    /// Postgres would refuse.
    pub fn into_columns(self) -> HashMap<String, Value> {
        let is_system = self.is_system;
        let mut data = columns(&self);
        if let Some(is_system) = is_system {
            data.insert(
                "is_system".to_string(),
                Value::from(if is_system { 1 } else { 0 }),
            );
        }
        data
    }
}

/// A group template row: every column of
/// `impresspress__products__group_templates`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GroupTemplateView {
    /// Stable template identifier.
    pub id: String,
    /// Machine name (`default`, …).
    pub name: String,
    /// Human-readable name shown in the product builder.
    pub display_name: String,
    /// RFC 3339 creation timestamp.
    #[schemars(extend("format" = "date-time"))]
    pub created_at: String,
    /// RFC 3339 timestamp of the last modification.
    #[schemars(extend("format" = "date-time"))]
    pub updated_at: String,
}

impl GroupTemplateView {
    /// Project an `impresspress__products__group_templates` row.
    pub fn from_record(record: &Record) -> Self {
        Self {
            id: record.id.clone(),
            name: record.str_field("name").to_string(),
            display_name: record.str_field("display_name").to_string(),
            created_at: record.str_field("created_at").to_string(),
            updated_at: record.str_field("updated_at").to_string(),
        }
    }
}

/// Every group template, as one page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GroupTemplateListResponse {
    /// Templates, sorted by name.
    pub records: Vec<GroupTemplateView>,
    /// Total templates. Always equal to the number of records: the endpoint
    /// does not paginate.
    pub total_count: i64,
    /// Always `1`.
    pub page: i64,
    /// The fixed ceiling on rows returned.
    pub page_size: i64,
}

impl GroupTemplateListResponse {
    /// Project a `RecordList` of template rows.
    pub fn from_record_list(list: &RecordList) -> Self {
        Self {
            records: list
                .records
                .iter()
                .map(GroupTemplateView::from_record)
                .collect(),
            total_count: list.total_count,
            page: list.page,
            page_size: list.page_size,
        }
    }
}

/// Response body of `GET /b/products/api/admin/sellers/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AdminSellerDetail {
    pub seller: SellerAccount,
    /// Every product owned by the seller's user, in any publication state.
    pub products: Vec<ProductView>,
}

#[cfg(test)]
mod tests {
    use super::{Condition, OfferMode, PricingPreviewRequest};

    #[test]
    fn enums_use_stable_snake_case_wire_names() {
        assert_eq!(
            serde_json::to_string(&OfferMode::Subscription).unwrap(),
            "\"subscription\""
        );
    }

    #[test]
    fn requests_reject_unknown_fields() {
        let error = serde_json::from_value::<PricingPreviewRequest>(serde_json::json!({
            "offer_id": "offer_1",
            "surprise": true
        }))
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn recursive_conditions_have_explicit_operations() {
        let condition: Condition = serde_json::from_value(serde_json::json!({
            "op": "all",
            "conditions": [
                {"op": "present", "input": "size"},
                {"op": "not", "condition": {"op": "equals", "input": "rush", "value": false}}
            ]
        }))
        .unwrap();
        assert!(matches!(condition, Condition::All { .. }));
    }
}
