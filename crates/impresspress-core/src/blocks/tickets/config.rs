//! Configuration and fail-closed public-submission readiness.

use serde::Serialize;
use wafer_core::clients::config;
use wafer_run::{context::Context, ConfigVar, InputType};

use super::repo;

pub const PUBLIC_ENABLED: &str = "IMPRESSPRESS__TICKETS__PUBLIC_SUBMISSIONS_ENABLED";
pub const BACK_URL: &str = "IMPRESSPRESS__TICKETS__PUBLIC_BACK_URL";
pub const SUPPORT_EMAIL: &str = "IMPRESSPRESS__TICKETS__SUPPORT_EMAIL";
pub const TURNSTILE_SITE_KEY: &str = "IMPRESSPRESS__TICKETS__TURNSTILE_SITE_KEY";
pub const TURNSTILE_SECRET_KEY: &str = "IMPRESSPRESS__TICKETS__TURNSTILE_SECRET_KEY";
pub const IDENTITY_SECRET: &str = "IMPRESSPRESS__TICKETS__IDENTITY_SECRET";
pub const IDENTITY_MAX: &str = "IMPRESSPRESS__TICKETS__IDENTITY_MAX";
pub const IDENTITY_WINDOW: &str = "IMPRESSPRESS__TICKETS__IDENTITY_WINDOW_SECS";
pub const GLOBAL_MAX: &str = "IMPRESSPRESS__TICKETS__GLOBAL_MAX";
pub const GLOBAL_WINDOW: &str = "IMPRESSPRESS__TICKETS__GLOBAL_WINDOW_SECS";
pub const FORM_TTL: &str = "IMPRESSPRESS__TICKETS__FORM_TTL_SECS";
pub const RETENTION_SPAM: &str = "IMPRESSPRESS__TICKETS__RETENTION_SPAM_DAYS";
pub const RETENTION_REJECTED: &str = "IMPRESSPRESS__TICKETS__RETENTION_REJECTED_DAYS";
pub const RETENTION_RESOLVED: &str = "IMPRESSPRESS__TICKETS__RETENTION_RESOLVED_DAYS";

pub fn config_vars() -> Vec<ConfigVar> {
    vec![
        ConfigVar::new(
            PUBLIC_ENABLED,
            "Allow protected public ticket submissions after readiness checks pass",
            "false",
        )
        .name("Public submissions enabled"),
        ConfigVar::new(
            BACK_URL,
            "Safe same-site back link for the public form",
            "/",
        )
        .name("Public back URL")
        .input_type(InputType::Url),
        ConfigVar::new(
            SUPPORT_EMAIL,
            "Contact address shown on the public form as an alternative to submitting a report; leave empty to show none",
            "",
        )
        .name("Support email")
        .optional(),
        ConfigVar::new(
            TURNSTILE_SITE_KEY,
            "Cloudflare Turnstile public widget site key",
            "",
        )
        .name("Turnstile site key")
        .optional(),
        ConfigVar::new(
            TURNSTILE_SECRET_KEY,
            "Cloudflare Turnstile server-side Siteverify secret",
            "",
        )
        .name("Turnstile secret")
        .input_type(InputType::Password)
        .optional(),
        ConfigVar::new(
            IDENTITY_SECRET,
            "Independent high-entropy secret for rotating abuse and form-token digests",
            "",
        )
        .name("Identity secret")
        .input_type(InputType::Password)
        .optional(),
        number(IDENTITY_MAX, "Reports per identity", "3"),
        number(IDENTITY_WINDOW, "Identity limit window (seconds)", "3600"),
        number(GLOBAL_MAX, "Global reports per window", "100"),
        number(GLOBAL_WINDOW, "Global limit window (seconds)", "3600"),
        number(FORM_TTL, "Public form lifetime (seconds)", "7200"),
        number(RETENTION_SPAM, "Spam retention (days)", "30"),
        number(
            RETENTION_REJECTED,
            "Rejected/duplicate retention (days)",
            "180",
        ),
        number(RETENTION_RESOLVED, "Resolved retention (days)", "365"),
    ]
}

fn number(key: &str, name: &str, default: &str) -> ConfigVar {
    ConfigVar::new(key, name, default)
        .name(name)
        .input_type(InputType::Number)
}

#[derive(Debug, Clone, Serialize)]
pub struct SecurityReadiness {
    pub ready: bool,
    pub block_enabled: bool,
    pub public_enabled: bool,
    pub site_key_configured: bool,
    pub turnstile_secret_configured: bool,
    pub identity_secret_configured: bool,
    pub positive_limits: bool,
    pub has_public_type: bool,
    pub reasons: Vec<String>,
}

impl SecurityReadiness {
    pub async fn load(ctx: &dyn Context) -> Self {
        let block_enabled = ctx
            .config_get(crate::features::BLOCK_SETTINGS_CONFIG_KEY)
            .map(|value| {
                crate::features::BlockSettings::state_for(value, "impresspress/tickets").enabled
            })
            .unwrap_or(true);
        let public_enabled = bool_value(&config::get_default(ctx, PUBLIC_ENABLED, "false").await);
        let site_key_configured = !config::get_default(ctx, TURNSTILE_SITE_KEY, "")
            .await
            .trim()
            .is_empty();
        let turnstile_secret_configured = !config::get_default(ctx, TURNSTILE_SECRET_KEY, "")
            .await
            .trim()
            .is_empty();
        let identity_secret_configured = !config::get_default(ctx, IDENTITY_SECRET, "")
            .await
            .trim()
            .is_empty();
        let identity_max = positive(ctx, IDENTITY_MAX, 3).await;
        let identity_window = positive(ctx, IDENTITY_WINDOW, 3_600).await;
        let global_max = positive(ctx, GLOBAL_MAX, 100).await;
        let global_window = positive(ctx, GLOBAL_WINDOW, 3_600).await;
        let positive_limits = identity_max && identity_window && global_max && global_window;
        let has_public_type = repo::count_public_types(ctx)
            .await
            .is_ok_and(|count| count > 0);

        let checks = [
            (block_enabled, "tickets block is disabled"),
            (public_enabled, "public submissions are disabled"),
            (site_key_configured, "Turnstile site key is missing"),
            (
                turnstile_secret_configured,
                "Turnstile server secret is missing",
            ),
            (identity_secret_configured, "identity secret is missing"),
            (positive_limits, "rate limits must be positive"),
            (has_public_type, "no active public ticket type exists"),
        ];
        let reasons = checks
            .into_iter()
            .filter_map(|(ok, reason)| (!ok).then(|| reason.to_string()))
            .collect::<Vec<_>>();
        Self {
            ready: reasons.is_empty(),
            block_enabled,
            public_enabled,
            site_key_configured,
            turnstile_secret_configured,
            identity_secret_configured,
            positive_limits,
            has_public_type,
            reasons,
        }
    }
}

pub async fn u32_value(ctx: &dyn Context, key: &str, default: u32) -> u32 {
    config::get_default(ctx, key, &default.to_string())
        .await
        .parse()
        .unwrap_or(default)
}

pub async fn u64_value(ctx: &dyn Context, key: &str, default: u64) -> u64 {
    config::get_default(ctx, key, &default.to_string())
        .await
        .parse()
        .unwrap_or(default)
}

async fn positive(ctx: &dyn Context, key: &str, default: u64) -> bool {
    u64_value(ctx, key, default).await > 0
}

fn bool_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_declared_as_passwords() {
        let vars = config_vars();
        for key in [TURNSTILE_SECRET_KEY, IDENTITY_SECRET] {
            let var = vars.iter().find(|var| var.key == key).expect("declared");
            assert_eq!(var.input_type, InputType::Password);
        }
    }
}
