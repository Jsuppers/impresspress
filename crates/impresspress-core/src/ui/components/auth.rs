//! Shared auth-split brand panel. Moved from `blocks/auth`, renamed from
//! `brand_panel` to `auth_panel`.

use crate::ui::{templates::BrandPanel, SiteConfig};

/// Shared brand panel used by `auth_ui::pages::*` (login / signup / reset /
/// OAuth / change-password / bootstrap).
/// Shared auth-split brand panel. `tagline` is page-specific — every caller
/// passes copy that matches what the page actually does (e.g. "Sign in to
/// continue." on the login page, "Create your account." on signup); it used
/// to be hardcoded to the login copy and rendered unchanged on signup,
/// bootstrap, password-reset, and verify pages too.
pub fn auth_panel<'a>(config: &'a SiteConfig, tagline: &'a str) -> BrandPanel<'a> {
    BrandPanel {
        logo_html: None,
        headline: &config.app_name,
        tagline: Some(tagline),
    }
}
