//! Shared auth-split brand panel plus the alert/oauth-button components used
//! by `auth_ui::pages::*`. Moved from `blocks/auth`, renamed from
//! `brand_panel` to `auth_panel`.

use maud::{html, Markup};

use crate::ui::{templates::BrandPanel, SiteConfig};

/// Shared brand panel used by `auth_ui::pages::*` (login / signup / reset /
/// OAuth / change-password / bootstrap).
///
/// `headline` always comes from `config.auth_headline` (config var
/// `WAFER_RUN_SHARED__AUTH_HEADLINE`, defaulting to
/// `config_vars::DEFAULT_AUTH_HEADLINE`) — it used to be `config.app_name`,
/// but a plain app name isn't the marketing headline this panel needs, and
/// a deployment that wants its app name back as the headline can still set
/// it that way via the config var.
///
/// `tagline` is page-specific: `Some(...)` renders that exact copy (e.g.
/// "Create your account." on signup — matches what the page actually
/// does). `None` falls back to `config.auth_tagline` (config var
/// `WAFER_RUN_SHARED__AUTH_TAGLINE`, defaulting to
/// `config_vars::DEFAULT_AUTH_TAGLINE`); an explicitly blanked
/// `auth_tagline` renders no tagline at all. Login is the only caller that
/// passes `None` today, so it shows the brand tagline while every other
/// auth page keeps its own contextual line — see `login.rs`'s call site for
/// why that doesn't duplicate its right-column "Sign in to continue.".
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

    let default_tagline = if config.auth_tagline.is_empty() {
        None
    } else {
        Some(config.auth_tagline.as_str())
    };

    BrandPanel {
        logo_html,
        headline: &config.auth_headline,
        tagline: tagline.or(default_tagline),
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
    use super::auth_panel;
    use crate::ui::SiteConfig;

    fn config_with(auth_headline: &str, auth_tagline: &str) -> SiteConfig {
        SiteConfig {
            app_name: "Impresspress".to_string(),
            logo_url: String::new(),
            logo_icon_url: String::new(),
            favicon_url: String::new(),
            primary_color: String::new(),
            embedded_scripts: Vec::new(),
            auth_headline: auth_headline.to_string(),
            auth_tagline: auth_tagline.to_string(),
        }
    }

    /// Headline always comes from config, never `config.app_name` — matches
    /// the requested navy-panel copy by default (`config_vars::
    /// DEFAULT_AUTH_HEADLINE`), and a white-label deployment can replace it
    /// without touching `login.rs` or the template.
    #[test]
    fn headline_comes_from_config_not_app_name() {
        let config = config_with("Custom Headline", "Custom Tagline");
        let panel = auth_panel(&config, None);
        assert_eq!(panel.headline, "Custom Headline");
    }

    /// Login passes `None`: it must fall back to `config.auth_tagline` (the
    /// brand tagline) rather than rendering no tagline at all.
    #[test]
    fn none_tagline_falls_back_to_config_default() {
        let config = config_with(
            "The backend that lifts its own weight.",
            "One binary. Batteries included. No lock-in.",
        );
        let panel = auth_panel(&config, None);
        assert_eq!(
            panel.tagline,
            Some("One binary. Batteries included. No lock-in.")
        );
    }

    /// The five sibling auth pages pass their own page-specific tagline via
    /// `Some(...)`; that must win over the config default, so their copy
    /// (e.g. "Create your account.") is unaffected by this change.
    #[test]
    fn explicit_tagline_overrides_config_default() {
        let config = config_with("Headline", "Brand tagline");
        let panel = auth_panel(&config, Some("Create your account."));
        assert_eq!(panel.tagline, Some("Create your account."));
    }

    /// A deployment that blanks `WAFER_RUN_SHARED__AUTH_TAGLINE` gets no
    /// tagline on login (rather than an empty `<p>`), mirroring how an
    /// emptied logo URL renders no `<img>`.
    #[test]
    fn blank_config_tagline_renders_no_tagline_when_none_passed() {
        let config = config_with("Headline", "");
        let panel = auth_panel(&config, None);
        assert_eq!(panel.tagline, None);
    }

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
        assert!(
            !m.contains("style="),
            "oauth button must not carry inline styles"
        );
    }
}
