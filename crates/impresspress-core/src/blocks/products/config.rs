//! The block's own commerce settings, each read in exactly one place.
//!
//! Two keys had more than one reader and the readers disagreed:
//!
//! - `IMPRESSPRESS__PRODUCTS__SELLER_APPLICATION_FEE_BPS` was parsed five
//!   times. Seller onboarding refused an unparseable value; checkout and
//!   Payment Links took `0`, so a mis-set fee meant the platform silently
//!   earned nothing on every sale (B22).
//! - `IMPRESSPRESS__PRODUCTS__PLATFORM_COUNTRY` was read three times with
//!   two different defaults — `""` in onboarding, `"US"` in checkout and in
//!   the product wizard, which also fell back to `"US"` for a value it could
//!   not read. So a non-US platform that never set the key onboarded with no
//!   country and shipped US-only, with nothing reporting either (B23).
//!
//! Both now answer once, and both refuse a value they cannot read rather
//! than substituting one. `super::config` in a call site is this module;
//! `config` inside one of the block's other files is
//! `wafer_core::clients::config`, the config client this module reads
//! through.

use wafer_core::clients::config;
use wafer_run::{context::Context, ErrorCode, WaferError};

/// Config key: the platform application fee, in basis points, applied to
/// connected-account sales when the seller carries no fee of its own.
/// Declared as a `ConfigVar` in [`super::config_vars`].
pub(crate) const SELLER_APPLICATION_FEE_BPS: &str =
    "IMPRESSPRESS__PRODUCTS__SELLER_APPLICATION_FEE_BPS";

/// Config key: the two-letter country of the platform's own Stripe account.
/// Declared as a `ConfigVar` in [`super::config_vars`]; blank by default,
/// and blank means "not configured", never a country.
pub(crate) const PLATFORM_COUNTRY: &str = "IMPRESSPRESS__PRODUCTS__PLATFORM_COUNTRY";

/// The largest legal application fee: 100%, expressed in basis points.
const MAX_FEE_BASIS_POINTS: u16 = 10_000;

/// A validated ISO 3166-1 alpha-2 country code, upper-cased.
///
/// A newtype rather than a `String` so "this was checked" is a property of
/// the value instead of a promise repeated at each call site — the three
/// hand-copied validations this replaces each drew a different conclusion
/// from the same input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CountryCode(String);

impl CountryCode {
    /// Two ASCII letters, upper-cased. `None` for anything else, blank
    /// included.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let trimmed = value.trim();
        (trimmed.len() == 2 && trimmed.bytes().all(|byte| byte.is_ascii_alphabetic()))
            .then(|| Self(trimmed.to_ascii_uppercase()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The platform application fee in basis points.
///
/// An unset key is the `ConfigVar`'s `"0"` — a real, deliberate value. Any
/// other value that is not 0..=10000 basis points is a `FailedPrecondition`,
/// because the alternative the two money paths used to pick — treat it as
/// zero — is the one outcome that loses money without telling anyone.
pub(crate) async fn seller_fee_bps(ctx: &dyn Context) -> Result<u16, WaferError> {
    let raw = config::get_default(ctx, SELLER_APPLICATION_FEE_BPS, "0").await;
    raw.trim()
        .parse::<u16>()
        .ok()
        .filter(|value| *value <= MAX_FEE_BASIS_POINTS)
        .ok_or_else(|| {
            WaferError::new(
                ErrorCode::FailedPrecondition,
                "seller application fee must be between 0 and 10000 basis points",
            )
        })
}

/// The platform's country, if one is configured.
///
/// `Ok(None)` is "the operator has not set one" — the `ConfigVar`'s declared
/// default — and every caller decides for itself what to do with that:
/// seller onboarding omits `country` from the Connect account (Stripe infers
/// it), while a checkout that must produce a shipping-country list refuses
/// rather than inventing one. A non-empty value that is not a two-letter
/// code is a `FailedPrecondition`; it is a typo in the settings form, not a
/// reason to ship to a country nobody chose.
pub(crate) async fn platform_country(ctx: &dyn Context) -> Result<Option<CountryCode>, WaferError> {
    let raw = config::get_default(ctx, PLATFORM_COUNTRY, "").await;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    CountryCode::parse(&raw).map(Some).ok_or_else(|| {
        WaferError::new(
            ErrorCode::FailedPrecondition,
            "platform country must be a two-letter country code",
        )
    })
}
