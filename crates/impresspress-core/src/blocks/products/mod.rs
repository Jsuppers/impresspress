pub mod contracts;
mod handlers;
pub(crate) mod migrations;
pub mod money;
pub mod offer_pricing;
mod pages;
mod purchase;
mod repo;
mod routes;
mod stripe;
mod stripe_client;
mod stripe_provider;

#[cfg(test)]
mod tests;

// The crate-level door test (`tests/repo_door.rs`) refuses a call site
// outside `repo::products` that names the table directly — the data
// snapshot's export allowlist is one, deliberately (it needs the name for
// its `TABLE_ALLOWLIST`/`TABLE_EXCLUDED` bookkeeping and as the
// `DataSnapshot` JSON key), and is listed in `IDENT_ALLOWED` there with its
// own justification rather than routed around the scanner through an extra
// same-value constant under a different name. The two functions alongside it
// are how it reads and writes the live set without a query built directly on
// the name.
//
// `block-dev`-gated because `blocks::dev::data_snapshot` is the ONLY consumer
// of all three, and the dev block is off in every default build: an
// ungated re-export is three `unused_imports` warnings (and a dead
// `upsert_from_snapshot`) in every build that does not compile the sandbox.
// The gate says what the re-export is for as well as keeping the default
// build warning-free.
#[cfg(feature = "block-dev")]
pub(crate) use repo::products::{
    list_all as list_live_products, upsert_from_snapshot as upsert_product_from_snapshot, TABLE,
};
// `repo` is private to this module (unlike `auth`'s `pub mod repo`, whose
// table constants are meant to be named from anywhere in the crate) — these
// re-exports are the curated exception list, extended here so
// `blocks::dev::data_snapshot` can name every collection this block declares
// without retyping a table's literal string a second time. Crate-scoped
// (`pub(crate)`), matching the two re-export blocks above: still nobody
// outside `impresspress-core` gets to depend on a products table name
// directly. `block-dev`-gated for the same reason the block above is: the
// data snapshot's closed-list bookkeeping is their only reader.
#[cfg(feature = "block-dev")]
pub(crate) use repo::{
    checkout_presets::TABLE as CHECKOUT_PRESETS_TABLE, disputes::TABLE as DISPUTES_TABLE,
    entitlements::TABLE as ENTITLEMENTS_TABLE, group_templates::TABLE as GROUP_TEMPLATES_TABLE,
    groups::TABLE as GROUPS_TABLE, offer_components::TABLE as OFFER_COMPONENTS_TABLE,
    offers::TABLE as OFFERS_TABLE, payment_links::TABLE as PAYMENT_LINKS_TABLE,
    product_templates::TABLE as PRODUCT_TEMPLATES_TABLE,
    product_versions::TABLE as PRODUCT_VERSIONS_TABLE,
    provider_operations::TABLE as PROVIDER_OPERATIONS_TABLE, refunds::TABLE as REFUNDS_TABLE,
    seller_accounts::TABLE as SELLER_ACCOUNTS_TABLE, stripe_events::TABLE as STRIPE_EVENTS_TABLE,
    subscription_items::TABLE as SUBSCRIPTION_ITEMS_TABLE, subscriptions::SUBSCRIPTIONS_TABLE,
    types::TABLE as TYPES_TABLE,
};
pub(crate) use repo::{
    purchases::{LINE_ITEMS_TABLE, PURCHASES_TABLE},
    variables::TABLE as VARIABLES_TABLE,
};
use wafer_core::clients::config;
use wafer_run::{BlockInfo, ConfigVar, InputType, InstanceMode};

use super::rate_limit::{apply_route_limit, UserRateLimiter};
use crate::{
    endpoint_match,
    http::{err_forbidden, err_internal, err_not_found},
};

/// Adapter-injected runtime identity. The browser service-worker adapter sets
/// this directly on its in-memory ConfigService after loading persisted
/// variables, so an admin database value cannot accidentally turn a public
/// browser runtime into a trusted secret holder. Native and Cloudflare leave
/// it unset and retain the server default. Double-underscore brackets mark
/// the key as internal (same convention as `BLOCK_SETTINGS_CONFIG_KEY`) — it
/// is never set via env var or the variables table, so it must not claim the
/// admin-writable `WAFER_RUN_SHARED__` prefix.
pub const RUNTIME_KIND_CONFIG_KEY: &str = "__IMPRESSPRESS_RUNTIME_KIND__";

pub(crate) async fn stripe_secret_operations_allowed(
    ctx: &dyn wafer_run::context::Context,
) -> bool {
    config::get_default(ctx, RUNTIME_KIND_CONFIG_KEY, "server").await != "browser"
}

/// The products block's own declared config vars. Single source of truth for
/// both `BlockInfo::config_keys` and the admin settings page (which renders
/// these via `ui::settings_form` rather than a parallel tuple table).
/// Stripe presentment currencies offered for the platform default. Values
/// are ISO 4217; the storefront still accepts any valid currency configured
/// per product — this list only drives the admin default select.
const CURRENCY_OPTIONS: &[(&str, &str)] = &[
    ("USD", "USD — US Dollar"),
    ("EUR", "EUR — Euro"),
    ("GBP", "GBP — British Pound"),
    ("AUD", "AUD — Australian Dollar"),
    ("CAD", "CAD — Canadian Dollar"),
    ("NZD", "NZD — New Zealand Dollar"),
    ("JPY", "JPY — Japanese Yen"),
    ("CHF", "CHF — Swiss Franc"),
    ("SEK", "SEK — Swedish Krona"),
    ("NOK", "NOK — Norwegian Krone"),
    ("DKK", "DKK — Danish Krone"),
    ("SGD", "SGD — Singapore Dollar"),
    ("HKD", "HKD — Hong Kong Dollar"),
    ("INR", "INR — Indian Rupee"),
    ("BRL", "BRL — Brazilian Real"),
    ("MXN", "MXN — Mexican Peso"),
    ("PLN", "PLN — Polish Zloty"),
    ("CZK", "CZK — Czech Koruna"),
    ("AED", "AED — UAE Dirham"),
    ("ZAR", "ZAR — South African Rand"),
];

/// Countries where Stripe supports platform accounts (ISO 3166-1 alpha-2).
/// The leading empty value renders as "Not set" so the optional var can be
/// cleared from the select widget.
const COUNTRY_OPTIONS: &[(&str, &str)] = &[
    ("", "Not set"),
    ("AU", "Australia"),
    ("AT", "Austria"),
    ("BE", "Belgium"),
    ("BG", "Bulgaria"),
    ("BR", "Brazil"),
    ("CA", "Canada"),
    ("HR", "Croatia"),
    ("CY", "Cyprus"),
    ("CZ", "Czechia"),
    ("DK", "Denmark"),
    ("EE", "Estonia"),
    ("FI", "Finland"),
    ("FR", "France"),
    ("DE", "Germany"),
    ("GI", "Gibraltar"),
    ("GR", "Greece"),
    ("HK", "Hong Kong"),
    ("HU", "Hungary"),
    ("IN", "India"),
    ("ID", "Indonesia"),
    ("IE", "Ireland"),
    ("IT", "Italy"),
    ("JP", "Japan"),
    ("LV", "Latvia"),
    ("LI", "Liechtenstein"),
    ("LT", "Lithuania"),
    ("LU", "Luxembourg"),
    ("MY", "Malaysia"),
    ("MT", "Malta"),
    ("MX", "Mexico"),
    ("NL", "Netherlands"),
    ("NZ", "New Zealand"),
    ("NG", "Nigeria"),
    ("NO", "Norway"),
    ("PL", "Poland"),
    ("PT", "Portugal"),
    ("RO", "Romania"),
    ("SG", "Singapore"),
    ("SK", "Slovakia"),
    ("SI", "Slovenia"),
    ("ZA", "South Africa"),
    ("ES", "Spain"),
    ("SE", "Sweden"),
    ("CH", "Switzerland"),
    ("TH", "Thailand"),
    ("AE", "United Arab Emirates"),
    ("GB", "United Kingdom"),
    ("US", "United States"),
];

pub(crate) fn config_vars() -> Vec<ConfigVar> {
    vec![
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__STRIPE_SECRET_KEY",
            "Stripe API secret key",
            "",
        )
        .name("Stripe Secret Key")
        .input_type(InputType::Password)
        .optional(),
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__STRIPE_PUBLISHABLE_KEY",
            "Stripe publishable key used by embedded Checkout and static storefronts. This key is safe to send to browsers, but is masked in admin storage and pages to prevent accidental configuration disclosure.",
            "",
        )
        .name("Stripe Publishable Key")
        .input_type(InputType::Password)
        .optional(),
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__STRIPE_WEBHOOK_SECRET",
            "Stripe webhook signing secret",
            "",
        )
        .name("Stripe Webhook Secret")
        .input_type(InputType::Password)
        .optional(),
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__STRIPE_API_URL",
            "Stripe API base URL",
            "https://api.stripe.com",
        )
        .name("Stripe API URL")
        .input_type(InputType::Url),
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__STRIPE_API_VERSION",
            "Stripe API version sent with every provider request and expected by the webhook destination",
            "2026-02-25.clover",
        )
        .name("Stripe API Version")
        .input_type(InputType::Text),
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__DEFAULT_CURRENCY",
            "Currency preselected for new products and offers",
            "USD",
        )
        .name("Default Currency")
        .input_type(InputType::Select)
        .options(CURRENCY_OPTIONS),
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__PLATFORM_COUNTRY",
            "Country of the platform Stripe account; also the seller onboarding default",
            "",
        )
        .name("Platform Country")
        .input_type(InputType::Select)
        .options(COUNTRY_OPTIONS)
        .optional(),
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__AUTOMATIC_TAX",
            "Enable Stripe automatic tax by default for new offers",
            "false",
        )
        .name("Automatic Tax")
        .input_type(InputType::Toggle),
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__CHECKOUT_ALLOWED_ORIGINS",
            "Comma-separated HTTPS origins allowed for Checkout return and cancel URLs; localhost HTTP origins are accepted in development",
            "",
        )
        .name("Checkout Allowed Origins")
        .input_type(InputType::Text)
        .optional(),
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__SELLER_APPLICATION_FEE_BPS",
            "Default platform application fee for connected-account sales, in basis points (0-10000)",
            "0",
        )
        .name("Seller Application Fee (bps)")
        .input_type(InputType::Number),
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__SELLER_MODERATION_REQUIRED",
            "Require admin approval before a user-owned product can be published",
            "true",
        )
        .name("Moderate Seller Products")
        .input_type(InputType::Toggle),
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__SELLER_ALLOWED_TEMPLATES",
            "Optional comma-separated product template IDs sellers may use; blank allows every template",
            "",
        )
        .name("Seller Allowed Templates")
        .input_type(InputType::Text)
        .optional(),
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__SELLER_ALLOWED_CURRENCIES",
            "Optional comma-separated ISO currency codes sellers may use; blank allows every valid currency",
            "",
        )
        .name("Seller Allowed Currencies")
        .input_type(InputType::Text)
        .optional(),
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__SELLER_ALLOWED_CATEGORIES",
            "Optional comma-separated product categories sellers may use; blank allows every category",
            "",
        )
        .name("Seller Allowed Categories")
        .input_type(InputType::Text)
        .optional(),
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__SELLER_MAX_PRODUCTS",
            "Maximum non-deleted products per seller; 0 means unlimited",
            "0",
        )
        .name("Seller Product Limit")
        .input_type(InputType::Number),
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__WEBHOOK_URL",
            "Webhook URL for billing events",
            "",
        )
        .name("Billing Webhook URL")
        .input_type(InputType::Url)
        .optional(),
        ConfigVar::new(
            "IMPRESSPRESS__PRODUCTS__WEBHOOK_SECRET",
            "Webhook signing secret",
            "",
        )
        .name("Billing Webhook Secret")
        .input_type(InputType::Password)
        .auto_generate(),
    ]
}

crate::impresspress_feature_block! {
    /// Products, groups, pricing, purchases, subscriptions (`impresspress/products`).
    pub struct ProductsBlock;
    fields: { limiter: UserRateLimiter },
    name: "impresspress/products",
    info: |_this| {
        use wafer_run::CollectionSchema;

        BlockInfo::new("impresspress/products", "0.0.1", "http-handler@v1", "Products, pricing, purchases, and payment integration")
            .instance_mode(InstanceMode::Singleton)
            .requires(vec!["wafer-run/database".into(), "wafer-run/config".into(), "wafer-run/network".into()])
            // Advisory table list — admin "Database tables" discovery + the
            // WRAP grant-UI read only `CollectionSchema::name`. The schema
            // itself (columns, indexes, FKs) lives solely in the block's
            // hand-authored `migrations/*.sqlite.sql` files (the single
            // source for both runtime `migrations::apply()` and the
            // Cloudflare D1 build).
            .collections(vec![
                CollectionSchema::new(repo::products::TABLE),
                CollectionSchema::new(repo::groups::TABLE),
                CollectionSchema::new(repo::types::TABLE),
                CollectionSchema::new(PURCHASES_TABLE),
                CollectionSchema::new(LINE_ITEMS_TABLE),
                CollectionSchema::new(repo::group_templates::TABLE),
                CollectionSchema::new(repo::product_templates::TABLE),
                CollectionSchema::new(VARIABLES_TABLE),
                CollectionSchema::new(repo::subscriptions::SUBSCRIPTIONS_TABLE),
                CollectionSchema::new(repo::product_versions::TABLE),
                CollectionSchema::new(repo::offers::TABLE),
                CollectionSchema::new(repo::offer_components::TABLE),
                CollectionSchema::new(repo::checkout_presets::TABLE),
                CollectionSchema::new(repo::payment_links::TABLE),
                CollectionSchema::new(repo::seller_accounts::TABLE),
                CollectionSchema::new(repo::subscription_items::TABLE),
                CollectionSchema::new(repo::entitlements::TABLE),
                CollectionSchema::new(repo::provider_operations::TABLE),
                CollectionSchema::new(repo::refunds::TABLE),
                CollectionSchema::new(repo::disputes::TABLE),
            ])
            .category(wafer_run::BlockCategory::Feature)
            .description("Product catalog and offer-based commerce. Manages typed customer inputs, itemized pricing, orders, sellers, and Stripe checkout for one-time and recurring products.")
            // Declared from the route table, so the central router enforces
            // each tier from the level every row names — the block has no
            // in-handler `is_admin` check (`routes::ROUTES`).
            .endpoints(endpoint_match::declare(routes::ROUTES))
            .config_keys(config_vars())
            .admin_url("/b/products/admin/")
            .can_disable(true)
    },
    handle: |this, ctx, msg, input| {
        // Auth is enforced centrally by `route_to_block` from each row's
        // declared `AuthLevel`. The matcher binds `{id}`, `{product_id}`,
        // `{offer_id}`, `{preset_id}` and `{link_id}` into `req.param.*` for
        // the handlers' `msg.var` readers; nothing else in this block reads
        // a path.
        let Some(route) = endpoint_match::dispatch(&mut msg, routes::ROUTES) else {
            return err_not_found("not found");
        };
        // Guest pricing, checkout and receipt polling spend route-specific
        // IP buckets; every other JSON route spends the per-user read/write
        // bucket; pages and the webhook spend none. `Allowed` headers are
        // discarded (see `apply_route_limit`).
        if let Some((key, category, limit)) = routes::rate_limit_for(route) {
            if let Some(limited) =
                apply_route_limit(&this.limiter, ctx, &msg, key, category, limit).await
            {
                return limited;
            }
        }
        // Own products, groups and the seller surface exist only while
        // `WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS` is on.
        if let Some(refusal) = routes::user_products_refusal(route) {
            if !handlers::user_products_enabled(ctx).await {
                return err_forbidden(refusal);
            }
        }
        // A platform suspension stops the seller's mutations while leaving
        // their read-only catalog and order history available.
        if routes::requires_unsuspended_seller(route) {
            match repo::seller_accounts::is_suspended(ctx, msg.user_id()).await {
                Ok(true) => return err_forbidden("Seller account is suspended"),
                Ok(false) => {}
                Err(error) => return err_internal("Could not verify seller status", error),
            }
        }
        handlers::run(ctx, &msg, route, input).await
    },
    lifecycle: |_this, ctx, event| {
        // Apply block-owned schema migrations. Migration 002 seeds the default
        // group/product templates (the static FK-parent rows the
        // groups/products tables require) via idempotent INSERTs, so there is
        // no per-request runtime existence-check + seed — the hash-gate
        // short-circuits in memory once applied.
        crate::migration_helper::lifecycle_init(
            ctx,
            &event,
            "impresspress/products",
            migrations::SQLITE_MIGRATIONS,
            migrations::POSTGRES_MIGRATIONS,
        )
        .await
    },
}
