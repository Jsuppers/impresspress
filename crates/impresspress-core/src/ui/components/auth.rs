//! Shared auth-split brand panel plus the alert/oauth-button components used
//! by `auth_ui::pages::*`. Moved from `blocks/auth`, renamed from
//! `brand_panel` to `auth_panel`.

use maud::{html, Markup};

use crate::ui::{templates::BrandPanel, SiteConfig};

/// Shared brand panel used by `auth_ui::pages::*` (login / signup / reset /
/// OAuth / change-password / bootstrap).
///
/// `tagline` is page-specific — every caller passes copy that matches what
/// the page actually does (e.g. "Create your account." on signup); it used
/// to be hardcoded to the login copy and rendered unchanged on signup,
/// bootstrap, password-reset, and verify pages too. Pass `None` when the
/// caller's own form already owns an equivalent line (login.rs's
/// `.auth-form__subtitle`) — the panel then renders the app-name headline
/// alone, rather than duplicating that sentence in both places.
///
/// The logo prefers `logo_icon_url` (mark only, matches the sidebar's
/// collapsed-state logo) and falls back to `logo_url` (wordmark); both
/// default to a bundled asset in `SiteConfig::load`, so this is `None` only
/// when a deployment explicitly overrides both to empty.
pub fn auth_panel<'a>(config: &'a SiteConfig, tagline: Option<&'a str>) -> BrandPanel<'a> {
    let logo_html = if !config.logo_icon_url.is_empty() {
        Some(html! { img .auth-split__logo-img src=(config.logo_icon_url) alt=""; })
    } else if !config.logo_url.is_empty() {
        Some(html! { img .auth-split__logo-img src=(config.logo_url) alt=(config.app_name); })
    } else {
        None
    };

    BrandPanel {
        logo_html,
        headline: &config.app_name,
        tagline,
    }
}

/// Visual variant for [`alert`].
pub enum AlertVariant {
    Error,
    Info,
    Success,
}

impl AlertVariant {
    fn class(&self) -> &'static str {
        match self {
            AlertVariant::Error => "alert--error",
            AlertVariant::Info => "alert--info",
            AlertVariant::Success => "alert--success",
        }
    }
}

/// Inline page-level message. Rendered with the `hidden` attribute; the
/// page's script fills the text and clears `hidden` to reveal it (never
/// `style.display` — `base.css`'s `[hidden] { display: none !important; }`
/// always wins over a plain inline `style.display`). Replaces the
/// hand-inlined `#error` / `#info` divs.
pub fn alert(variant: AlertVariant, id: &str, message: &str) -> Markup {
    html! {
        div id=(id) class={ "alert " (variant.class()) } hidden { (message) }
    }
}

/// Third-party sign-in button. Only rendered for providers whose full
/// credential triple is configured, so it never 4xxs on click.
pub fn oauth_button(provider: &str, label: &str, icon: Markup) -> Markup {
    html! {
        button type="button" class="btn btn-oauth"
            data-provider=(provider)
            onclick={ "oauthStart('" (provider) "')" } {
            (icon)
            "Continue with " (label)
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn alert_carries_id_and_variant_class_without_inline_styles() {
        let m = super::alert(super::AlertVariant::Error, "error", "boom").into_string();
        assert!(m.contains(r#"id="error""#));
        assert!(m.contains("alert--error"));
        assert!(m.contains("boom"));
        assert!(!m.contains("style="), "alert must not carry inline styles");
    }

    #[test]
    fn oauth_button_has_no_inline_styles() {
        let m = super::oauth_button("github", "GitHub", maud::html! {}).into_string();
        assert!(m.contains(r#"data-provider="github""#));
        assert!(m.contains("Continue with GitHub"));
        assert!(!m.contains("style="), "oauth button must not carry inline styles");
    }
}
