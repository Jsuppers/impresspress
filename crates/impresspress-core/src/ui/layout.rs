//! Page layout components — the full HTML page wrapper.
//!
//! `block_shell()` was removed in Phase 2 of the UI cleanup; pages now build
//! chrome via `ui::Page::response()` which delegates to `ui::shell::shell()`
//! + `ui::sidebar::sidebar_grouped()`.

use maud::{html, Markup, PreEscaped, DOCTYPE};

use super::{assets, SiteConfig};

/// Render a full HTML page with head (CSS + htmx) and body.
pub fn page(title: &str, config: &SiteConfig, body: Markup) -> Markup {
    // Brand accent override. Sanitized to a safe CSS-color charset so a
    // stored value can't break out of the <style> tag. `--primary-hover`
    // derives from it so a single config var re-themes the whole chrome.
    let primary_override = if config.primary_color.trim().is_empty() {
        String::new()
    } else {
        let c: String = config
            .primary_color
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric() || "#(),%. -".contains(*ch))
            .collect();
        format!(":root{{--primary-color:{c};--primary-hover:color-mix(in srgb,{c} 82%,#000)}}")
    };
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width,initial-scale=1";
                title { (title) " — " (config.app_name) }
                link rel="stylesheet" href=(assets::css_url());
                @if !primary_override.is_empty() {
                    style { (PreEscaped(&primary_override)) }
                }
                @if !config.favicon_url.is_empty() {
                    link rel="icon" href=(config.favicon_url);
                }
                script src=(assets::htmx_js_url()) defer {}
                // The chrome's own behaviour — palette, drawer, toasts,
                // modals — as one hashed asset instead of four raw strings
                // inlined at the bottom of every page. `defer` is what keeps
                // that a pure move: a deferred script runs after parsing and
                // in document order, so every element these sections bind to
                // exists by the time they run (their end-of-body placement
                // gave them the same guarantee) and htmx is still installed
                // first.
                script src=(assets::chrome_js_url()) defer {}
            }
            body {
                (body)
                div #toast-container .toast-container {}
                script src=(assets::webmcp_js_url()) defer {}
                @for src in &config.embedded_scripts {
                    script type="module" src=(src) {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_page_includes_the_webmcp_registration_script() {
        let config = SiteConfig {
            app_name: "Test".into(),
            logo_url: String::new(),
            logo_icon_url: String::new(),
            favicon_url: String::new(),
            primary_color: String::new(),
            embedded_scripts: Vec::new(),
            auth_headline: String::new(),
            auth_tagline: String::new(),
        };
        let rendered = page("Title", &config, maud::html! { p { "body" } }).into_string();
        assert!(
            rendered.contains(&assets::webmcp_js_url()),
            "the WebMCP script must be on every page: {rendered}"
        );
    }

    /// The chrome's behaviour ships as one hashed `<script src>`, not as
    /// inlined raw strings. Markers from all four former inline scripts must
    /// be absent from the document, and the asset URL present.
    #[test]
    fn chrome_behaviour_is_one_hashed_script_not_inline_source() {
        let config = SiteConfig {
            app_name: "Test".into(),
            logo_url: String::new(),
            logo_icon_url: String::new(),
            favicon_url: String::new(),
            primary_color: String::new(),
            embedded_scripts: Vec::new(),
            auth_headline: String::new(),
            auth_tagline: String::new(),
        };
        let rendered = page("Title", &config, maud::html! { p { "body" } }).into_string();
        assert!(
            rendered.contains(&format!(
                r#"<script src="{}" defer></script>"#,
                assets::chrome_js_url()
            )),
            "the chrome script must be linked, hashed and deferred: {rendered}"
        );
        for marker in [
            "__cmdkInit",
            "__drawerInit",
            "showToast",
            "function openModal",
        ] {
            assert!(
                !rendered.contains(marker),
                "{marker} is still inlined into the page: {rendered}"
            );
        }
    }
}
