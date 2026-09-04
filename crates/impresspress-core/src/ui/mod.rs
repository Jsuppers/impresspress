//! Server-side rendered UI components for impresspress blocks.
//!
//! Uses maud for compile-time HTML generation and htmx for interactivity.
//! CSS, htmx JS, logos, and the favicon are embedded in the binary; their
//! URLs can be overridden via environment variables (`WAFER_RUN_SHARED__LOGO_URL`,
//! `WAFER_RUN_SHARED__LOGO_ICON_URL`, `WAFER_RUN_SHARED__FAVICON_URL`).

pub mod assets;
pub mod components;
pub mod icons;
pub mod layout;
pub mod nav_groups;
pub mod palette;
pub mod settings_form;
pub mod shell;
pub mod sidebar;
pub mod templates;

/// Branding/site config loaded from environment variables.
/// Passed through to layout and sidebar so every page renders consistently.
pub struct SiteConfig {
    pub app_name: String,
    pub logo_url: String,
    pub logo_icon_url: String,
    pub favicon_url: String,
    /// Optional brand accent (`--primary-color`) override. Empty = keep the
    /// bundled default. Lets an app built on impresspress-core (e.g. a site)
    /// theme the chrome to its own brand instead of inheriting ours.
    pub primary_color: String,
    /// Extra module-type script URLs appended to every rendered page.
    /// Browser targets populate this (e.g. `/webllm-engine.js` for the
    /// page-side LLM engine); native targets leave it empty.
    pub embedded_scripts: Vec<String>,
    /// Headline on the auth-split brand panel (login/signup/reset/etc. left
    /// navy column) — see `ui::components::auth_panel`. Defaults to
    /// marketing copy (`config_vars::DEFAULT_AUTH_HEADLINE`); a white-label
    /// deployment overrides via `WAFER_RUN_SHARED__AUTH_HEADLINE` so the
    /// stock copy never ships under someone else's brand.
    pub auth_headline: String,
    /// Sub-line under `auth_headline`, shown when a page doesn't supply its
    /// own (see `auth_panel`'s `tagline` param). Empty hides it entirely.
    pub auth_tagline: String,
}

impl SiteConfig {
    /// Load site config from the WAFER config system (env vars / variables table).
    pub async fn load(ctx: &dyn wafer_run::context::Context) -> Self {
        use wafer_core::clients::config;
        let scripts_raw = config::get_default(ctx, "WAFER_RUN_SHARED__EMBEDDED_SCRIPTS", "").await;
        Self {
            app_name: config::get_default(ctx, "WAFER_RUN_SHARED__APP_NAME", "Impresspress").await,
            // Blank = no wordmark image: templates render the app name as
            // text next to the (pixel-art) icon. Set to white-label with a
            // wordmark of your own.
            logo_url: config::get_default(ctx, crate::config_vars::LOGO_URL_KEY, "").await,
            logo_icon_url: config::get_default(
                ctx,
                "WAFER_RUN_SHARED__LOGO_ICON_URL",
                &assets::logo_icon_url(),
            )
            .await,
            favicon_url: config::get_default(
                ctx,
                "WAFER_RUN_SHARED__FAVICON_URL",
                &assets::favicon_url(),
            )
            .await,
            primary_color: config::get_default(ctx, "WAFER_RUN_SHARED__PRIMARY_COLOR", "").await,
            embedded_scripts: scripts_raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            auth_headline: config::get_default(
                ctx,
                "WAFER_RUN_SHARED__AUTH_HEADLINE",
                crate::config_vars::DEFAULT_AUTH_HEADLINE,
            )
            .await,
            auth_tagline: config::get_default(
                ctx,
                "WAFER_RUN_SHARED__AUTH_TAGLINE",
                crate::config_vars::DEFAULT_AUTH_TAGLINE,
            )
            .await,
        }
    }
}

/// User info available during rendering (extracted from auth metadata).
pub struct UserInfo {
    pub id: String,
    pub email: String,
    pub roles: Vec<String>,
}

impl UserInfo {
    /// Create from message auth metadata.
    pub fn from_message(msg: &wafer_run::Message) -> Option<Self> {
        let id = msg.get_meta("auth.user_id");
        if id.is_empty() {
            return None;
        }
        let email = msg.get_meta("auth.user_email").to_string();
        let roles: Vec<String> = msg
            .get_meta("auth.user_roles")
            .split(',')
            .filter(|r| !r.trim().is_empty())
            .map(|r| r.trim().to_string())
            .collect();
        Some(Self {
            id: id.to_string(),
            email,
            roles,
        })
    }

    pub fn is_admin(&self) -> bool {
        self.roles.iter().any(|r| r == "admin")
    }

    /// First letter of email, uppercased, for avatar.
    pub fn avatar_initial(&self) -> char {
        self.email
            .chars()
            .next()
            .unwrap_or('?')
            .to_ascii_uppercase()
    }
}

/// A navigation item for the sidebar.
pub struct NavItem {
    pub label: String,
    pub href: String,
    /// The icon renderer, referenced directly (e.g. `icons::users`). Typed as
    /// a function pointer rather than a name string so the compiler rejects a
    /// missing or misspelled icon instead of silently falling back to a
    /// default glyph (the bug the old `nav_icon` string match hid).
    pub icon: fn() -> maud::Markup,
    /// When true, render as `target="_blank"` and open in a new tab from
    /// both the sidebar and the ⌘K palette. Used for cross-block links
    /// that have their own chrome (e.g. Inspector).
    pub external: bool,
    /// The `{org}/{block}` that serves this item's page, when that block is
    /// optional (feature-gated per target: e.g. `impresspress/vector` isn't
    /// compiled into the browser demo, `impresspress/llm` isn't on
    /// Cloudflare). [`shell_page`] drops items whose block isn't in
    /// `ctx.registered_blocks()` so the nav never links to a route that
    /// would 404. `None` = always shown (backing block is unconditional).
    pub block: Option<&'static str>,
}

pub use sidebar::NavGroup;

/// Check if the current request is an htmx partial request.
pub fn is_htmx(msg: &wafer_run::Message) -> bool {
    !msg.get_meta("http.header.hx-request").is_empty()
}

/// Respond with full HTML page or htmx fragment depending on request type.
pub fn html_response(markup: maud::Markup) -> wafer_run::OutputStream {
    crate::http::ResponseBuilder::new().body(
        markup.into_string().into_bytes(),
        "text/html; charset=utf-8",
    )
}

/// Declarative description of a shelled SSR page.
///
/// Built with named fields (rather than the former 8-positional-arg
/// `shelled_response`) so the two `&str` fields `title` and `current_path`
/// can't be transposed, and so the compiler enforces every field is supplied.
/// Render it with [`Page::render`] (full `Markup`) or [`Page::response`]
/// (htmx-aware `OutputStream`).
pub struct Page<'a> {
    pub config: &'a SiteConfig,
    pub title: &'a str,
    /// The audience's sidebar groups (admin or portal).
    pub nav: &'a [NavGroup],
    pub user: Option<&'a UserInfo>,
    pub current_path: &'a str,
    pub topbar: shell::Topbar<'a>,
    pub body: maud::Markup,
}

impl<'a> Page<'a> {
    /// Render the full page: `page()` wrapping `shell()` + the ⌘K palette
    /// modal (mounted only when `topbar.show_palette` is true).
    pub fn render(self) -> maud::Markup {
        use maud::{html, PreEscaped};
        let palette_markup = if self.topbar.show_palette {
            palette::palette(nav_groups::palette_entries_from_groups(self.nav))
        } else {
            html! {}
        };
        layout::page(
            self.title,
            self.config,
            html! {
                (shell::shell(
                    self.nav,
                    self.user,
                    self.current_path,
                    &self.config.logo_url,
                    &self.config.logo_icon_url,
                    &self.config.app_name,
                    self.topbar,
                    self.body,
                ))
                (palette_markup)
                script { (PreEscaped(assets::palette_js())) }
                script { (PreEscaped(assets::drawer_js())) }
            },
        )
    }

    /// htmx-aware markup: the raw `body` (no chrome) for an htmx partial,
    /// else the full [`render`](Self::render) document.
    ///
    /// Split out of [`response`](Self::response) because an `OutputStream`'s
    /// response meta is fixed when the stream is built — a page that needs
    /// headers of its own (`/b/dev` and its COOP/COEP/`no-store`) cannot
    /// amend a response after the fact and must build one around this markup
    /// instead. See [`shell_document`].
    pub fn document(self, msg: &wafer_run::Message) -> maud::Markup {
        if is_htmx(msg) {
            return self.body;
        }
        self.render()
    }

    /// htmx-aware response: [`document`](Self::document) as `text/html`.
    pub fn response(self, msg: &wafer_run::Message) -> wafer_run::OutputStream {
        html_response(self.document(msg))
    }
}

/// Which audience's sidebar a shelled page should render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavKind {
    /// End-user portal sidebar (Account / Apps).
    Portal,
    /// Admin sidebar (Workspace / Data / System).
    Admin,
}

impl NavKind {
    fn groups(self) -> Vec<NavGroup> {
        match self {
            NavKind::Portal => nav_groups::portal(),
            NavKind::Admin => nav_groups::admin(),
        }
    }
}

/// Declarative inputs for [`shell_page`] — everything a block page needs to
/// render the standard chrome, minus the body and the data ([`SiteConfig`] /
/// [`UserInfo`] are loaded internally).
pub struct Shell<'a> {
    /// `<title>` text.
    pub title: &'a str,
    /// Which sidebar to render.
    pub nav: NavKind,
    /// Breadcrumb trail. A single `Crumb { label, href: None }` is the common case.
    pub crumbs: Vec<shell::Crumb<'a>>,
    /// Optional subtitle shown after the crumbs.
    pub subtitle: Option<&'a str>,
    /// Optional primary action button in the topbar.
    pub primary_action: Option<maud::Markup>,
}

impl<'a> Shell<'a> {
    /// The single-crumb, no-subtitle, no-action shell that almost every page uses.
    pub fn simple(title: &'a str, nav: NavKind, crumb_label: &'a str) -> Self {
        Self {
            title,
            nav,
            crumbs: vec![shell::Crumb {
                label: crumb_label,
                href: None,
            }],
            subtitle: None,
            primary_action: None,
        }
    }
}

/// Render `body` inside the standard page chrome (sidebar + topbar + ⌘K
/// palette), loading [`SiteConfig`] and [`UserInfo`] internally and returning
/// an htmx-aware [`OutputStream`]. This is the single shell constructor that
/// replaced the six per-block `*_page` wrapper functions (`render_page`,
/// `legalpages_page`, `messages_page`, `products_page`, `llm_page`,
/// `files_page*`) and the inline `Page { .. }.response(msg)` reconstructions.
///
/// `current_path` is taken from the request path so the active sidebar item
/// highlights correctly.
pub async fn shell_page(
    ctx: &dyn wafer_run::context::Context,
    msg: &wafer_run::Message,
    shell: Shell<'_>,
    body: maud::Markup,
) -> wafer_run::OutputStream {
    html_response(shell_document(ctx, msg, shell, body).await)
}

/// [`shell_page`]'s markup, before it becomes a response.
///
/// Every page that only needs `text/html` should call [`shell_page`]. This
/// exists for the one that needs more: `/b/dev` must carry
/// `Cross-Origin-Opener-Policy` / `Cross-Origin-Embedder-Policy` (cross-origin
/// isolation, without which the in-browser compiler has no `SharedArrayBuffer`)
/// and `Cache-Control: no-store`, and an `OutputStream`'s meta is fixed when
/// the stream is built — so the headers have to be on the response as it is
/// constructed, not bolted onto one that already exists.
pub async fn shell_document(
    ctx: &dyn wafer_run::context::Context,
    msg: &wafer_run::Message,
    shell: Shell<'_>,
    body: maud::Markup,
) -> maud::Markup {
    let config = SiteConfig::load(ctx).await;
    let user = UserInfo::from_message(msg);
    let mut groups = shell.nav.groups();
    // Hide nav items whose backing block isn't registered on this target
    // (feature-gated blocks vary per deployment — see NavItem::block).
    let registered: std::collections::HashSet<&str> = ctx
        .registered_blocks()
        .iter()
        .map(|b| b.name.as_str())
        .collect();
    nav_groups::retain_registered(&mut groups, &registered);
    let path = msg.path().to_string();
    Page {
        config: &config,
        title: shell.title,
        nav: &groups,
        user: user.as_ref(),
        current_path: &path,
        topbar: shell::Topbar {
            crumbs: shell.crumbs,
            subtitle: shell.subtitle,
            primary_action: shell.primary_action,
            show_palette: true,
        },
        body,
    }
    .document(msg)
}

/// Minimal `SiteConfig` used by the status-page helpers. They render
/// before context is available, so they can't load real config; fixed
/// branding + no embedded scripts is the right shape.
fn minimal_config() -> SiteConfig {
    SiteConfig {
        app_name: "Impresspress".to_string(),
        logo_url: String::new(),
        logo_icon_url: String::new(),
        favicon_url: assets::favicon_url(),
        primary_color: String::new(),
        embedded_scripts: Vec::new(),
        auth_headline: crate::config_vars::DEFAULT_AUTH_HEADLINE.to_string(),
        auth_tagline: crate::config_vars::DEFAULT_AUTH_TAGLINE.to_string(),
    }
}

/// Render the styled `status_page` body wrapped in `layout::page` and
/// return it with the requested HTTP status. Used by 403/404/500 helpers.
fn status_response(
    status: u16,
    page_title: &str,
    code: &str,
    title: &str,
    body_text: &str,
    primary_action: (&str, &str),
) -> wafer_run::OutputStream {
    let config = minimal_config();
    let body = templates::status_page(
        code,
        title,
        body_text,
        Some((primary_action.0.to_string(), primary_action.1.to_string())),
    );
    let markup = layout::page(page_title, &config, body);
    crate::http::ResponseBuilder::new().status(status).body(
        markup.into_string().into_bytes(),
        "text/html; charset=utf-8",
    )
}

/// Return styled 403 for browser requests, JSON for API requests.
pub fn forbidden_response(msg: &wafer_run::Message) -> wafer_run::OutputStream {
    let accept = msg.get_meta("http.header.accept");
    if accept.contains("text/html") && !accept.contains("application/json") {
        status_response(
            403,
            "Forbidden",
            "403",
            "Forbidden",
            "You don't have access to this page.",
            ("Sign in", "/b/auth/login"),
        )
    } else {
        crate::http::err_forbidden("admin access required")
    }
}

/// A credentialed, state-changing request that failed the CSRF origin policy
/// (see [`crate::csrf::enforce_origin_policy`]). Kept distinct from
/// [`forbidden_response`] so the message names the actual cause — a request
/// that couldn't be verified as same-origin — instead of the misleading
/// "admin access required" (which belongs to genuine admin-role denials).
pub fn csrf_blocked_response(msg: &wafer_run::Message) -> wafer_run::OutputStream {
    let accept = msg.get_meta("http.header.accept");
    if accept.contains("text/html") && !accept.contains("application/json") {
        status_response(
            403,
            "Request Blocked",
            "403",
            "Request blocked",
            "This request couldn't be verified as coming from this site. Reload the page and try again.",
            ("Go to homepage", "/"),
        )
    } else {
        crate::http::err_forbidden("cross-origin request blocked")
    }
}

/// Anonymous (or stale-session — identical by the time enforcement runs)
/// browser request on a protected route: send the user to login with a return
/// path so they land back where they started after signing in. API callers
/// (non-HTML `Accept`) keep the JSON 403 contract instead of a redirect.
///
/// The return path is form-encoded via [`crate::util::urlencode`] into a
/// `?redirect=` query param — the exact param name and encoding the login page
/// consumes (`is_safe_local_redirect` on the consumer side); the producer only
/// needs to encode.
pub fn unauthenticated_response(msg: &wafer_run::Message) -> wafer_run::OutputStream {
    let accept = msg.get_meta("http.header.accept");
    if accept.contains("text/html") && !accept.contains("application/json") {
        let target = format!(
            "/b/auth/login?redirect={}",
            crate::util::urlencode(msg.path())
        );
        crate::http::redirect(302, &target)
    } else {
        crate::http::err_forbidden("authentication required")
    }
}

/// Return styled 404 for browser requests, JSON for API requests.
pub fn not_found_response(msg: &wafer_run::Message) -> wafer_run::OutputStream {
    let accept = msg.get_meta("http.header.accept");
    if accept.contains("text/html") && !accept.contains("application/json") {
        status_response(
            404,
            "Not found",
            "404",
            "Not found",
            "We couldn't find that page.",
            ("Go home", "/"),
        )
    } else {
        crate::http::err_not_found("endpoint not found")
    }
}

/// Return styled 500 for browser requests, JSON for API requests.
pub fn server_error_response(msg: &wafer_run::Message) -> wafer_run::OutputStream {
    let accept = msg.get_meta("http.header.accept");
    if accept.contains("text/html") && !accept.contains("application/json") {
        status_response(
            500,
            "Server error",
            "500",
            "Something went wrong",
            "An unexpected error occurred. Please try again.",
            ("Go home", "/"),
        )
    } else {
        crate::http::err_internal_no_cause("internal server error")
    }
}

/// Respond with HTML + an HX-Trigger header for toast notifications.
///
/// The trigger payload lands in an HTTP response header and is parsed by
/// htmx as JSON. Building it with `format!` would let a toast message
/// containing `"` or `\` produce malformed JSON (and a possible header-
/// injection vector via embedded `\r\n`). Route through `serde_json` so
/// the message text is properly escaped.
pub fn html_response_with_toast(
    markup: maud::Markup,
    toast_message: &str,
    toast_type: &str,
) -> wafer_run::OutputStream {
    let trigger = serde_json::json!({
        "showToast": {
            "message": toast_message,
            "type": toast_type,
        }
    })
    .to_string();
    crate::http::ResponseBuilder::new()
        .set_header("HX-Trigger", &trigger)
        .body(
            markup.into_string().into_bytes(),
            "text/html; charset=utf-8",
        )
}

#[cfg(test)]
mod tests {
    use maud::{html, Markup};
    use wafer_run::Message;

    use super::*;
    use crate::ui::shell::{Crumb, Topbar};

    fn site_config() -> SiteConfig {
        SiteConfig {
            app_name: "TestApp".to_string(),
            logo_url: String::new(),
            logo_icon_url: String::new(),
            favicon_url: String::new(),
            primary_color: String::new(),
            embedded_scripts: Vec::new(),
            auth_headline: String::new(),
            auth_tagline: String::new(),
        }
    }

    /// `SiteConfig::load` defaults the auth-panel headline/tagline to the
    /// requested marketing copy when no config var is set.
    #[tokio::test]
    async fn site_config_load_defaults_auth_headline_and_tagline() {
        let ctx = crate::test_support::TestContext::new().await;
        let config = SiteConfig::load(&ctx).await;
        assert_eq!(
            config.auth_headline,
            crate::config_vars::DEFAULT_AUTH_HEADLINE
        );
        assert_eq!(
            config.auth_tagline,
            crate::config_vars::DEFAULT_AUTH_TAGLINE
        );
    }

    /// A white-label deployment overrides both via
    /// `WAFER_RUN_SHARED__AUTH_HEADLINE` / `WAFER_RUN_SHARED__AUTH_TAGLINE`,
    /// same as `WAFER_RUN_SHARED__APP_NAME` overrides `app_name` — the
    /// stock marketing copy must never be baked in unconditionally.
    #[tokio::test]
    async fn site_config_load_honors_auth_headline_and_tagline_overrides() {
        let mut ctx = crate::test_support::TestContext::new().await;
        ctx.set_config("WAFER_RUN_SHARED__AUTH_HEADLINE", "Acme Cloud");
        ctx.set_config("WAFER_RUN_SHARED__AUTH_TAGLINE", "Built for Acme.");
        let config = SiteConfig::load(&ctx).await;
        assert_eq!(config.auth_headline, "Acme Cloud");
        assert_eq!(config.auth_tagline, "Built for Acme.");
    }

    fn dashboard_page<'a>(
        config: &'a SiteConfig,
        groups: &'a [NavGroup],
        body: Markup,
    ) -> Page<'a> {
        Page {
            config,
            title: "Dashboard",
            nav: groups,
            user: None,
            current_path: "/b/admin/",
            topbar: Topbar {
                crumbs: vec![Crumb {
                    label: "Dashboard",
                    href: None,
                }],
                primary_action: None,
                subtitle: None,
                show_palette: true,
            },
            body,
        }
    }

    #[test]
    fn page_full_render_includes_html_doctype_shell_and_body() {
        let config = site_config();
        let groups = nav_groups::admin();
        let s = dashboard_page(&config, &groups, html! { p { "hello" } })
            .render()
            .into_string();
        assert!(s.contains("<!DOCTYPE html>"));
        assert!(s.contains(r#"class="shell""#));
        assert!(s.contains(r#"id="cmdk""#)); // palette mounted
        assert!(s.contains("hello"));
    }

    #[tokio::test]
    async fn page_response_returns_raw_body_for_htmx_and_full_doc_otherwise() {
        let config = site_config();
        let groups = nav_groups::admin();

        // Non-htmx → full document with chrome.
        let full = dashboard_page(&config, &groups, html! { p { "hello" } })
            .response(&Message::new("http.request"))
            .collect_buffered()
            .await
            .unwrap();
        let full_body = String::from_utf8(full.body).unwrap_or_default();
        assert!(full_body.contains("<!DOCTYPE html>"));
        assert!(full_body.contains(r#"class="shell""#));

        // htmx partial → raw body, no chrome.
        let mut htmx = Message::new("http.request");
        htmx.set_meta("http.header.hx-request", "true");
        let partial = dashboard_page(&config, &groups, html! { p { "hello" } })
            .response(&htmx)
            .collect_buffered()
            .await
            .unwrap();
        let partial_body = String::from_utf8(partial.body).unwrap_or_default();
        assert!(partial_body.contains("hello"));
        assert!(!partial_body.contains("<!DOCTYPE html>"));
        assert!(!partial_body.contains(r#"class="shell""#));
    }

    #[tokio::test]
    async fn not_found_response_uses_status_template() {
        let mut msg = Message::new("http.request");
        msg.set_meta("http.header.accept", "text/html");
        let out = not_found_response(&msg);
        let buf = out.collect_buffered().await.unwrap();
        let body = String::from_utf8(buf.body).unwrap_or_default();
        assert!(
            body.contains("status-page"),
            "body should contain status-page class"
        );
        assert!(body.contains(">404<"), "body should contain 404 code");
        assert!(
            body.contains("Go home"),
            "body should contain Go home action"
        );
    }

    #[tokio::test]
    async fn forbidden_response_uses_status_template() {
        let mut msg = Message::new("http.request");
        msg.set_meta("http.header.accept", "text/html");
        let out = forbidden_response(&msg);
        let buf = out.collect_buffered().await.unwrap();
        let body = String::from_utf8(buf.body).unwrap_or_default();
        assert!(
            body.contains("status-page"),
            "body should contain status-page class"
        );
        assert!(body.contains(">403<"), "body should contain 403 code");
        assert!(
            body.contains("Sign in"),
            "body should contain Sign in action"
        );
    }

    #[tokio::test]
    async fn unauthenticated_response_redirects_html_to_login() {
        // Browser (HTML) request with an empty identity → 302 to the login page
        // carrying a form-encoded `?redirect=` return path the login page
        // consumes via `is_safe_local_redirect`.
        let mut msg = Message::new("http.request");
        msg.set_meta("req.resource", "/b/chat/hello");
        msg.set_meta("http.header.accept", "text/html,application/xhtml+xml");
        let buf = unauthenticated_response(&msg)
            .collect_buffered()
            .await
            .expect("redirect is a Response terminal");
        let status = buf
            .meta
            .iter()
            .find(|e| e.key == "resp.status")
            .map(|e| e.value.as_str());
        let location = buf
            .meta
            .iter()
            .find(|e| e.key == "resp.header.Location")
            .map(|e| e.value.as_str());
        assert_eq!(status, Some("302"));
        assert_eq!(location, Some("/b/auth/login?redirect=%2Fb%2Fchat%2Fhello"));
    }

    #[tokio::test]
    async fn unauthenticated_response_json_accept_stays_403() {
        // API caller (non-HTML Accept) keeps the JSON 403 contract — status
        // stays 403 (not 401) so existing API clients/tests don't break.
        use wafer_block::http_codec;
        use wafer_run::streams::output::TerminalNotResponse;
        let mut msg = Message::new("http.request");
        msg.set_meta("req.resource", "/b/chat/hello");
        msg.set_meta("http.header.accept", "application/json");
        let status = match unauthenticated_response(&msg).collect_buffered().await {
            Ok(buf) => i64::from(http_codec::resolve_status(&buf.meta, 200)),
            Err(TerminalNotResponse::Error(err)) => {
                i64::from(http_codec::resolve_error_status(&err))
            }
            Err(other) => panic!("unexpected terminal: {other:?}"),
        };
        assert_eq!(status, 403);
    }

    #[tokio::test]
    async fn server_error_response_uses_status_template() {
        let mut msg = Message::new("http.request");
        msg.set_meta("http.header.accept", "text/html");
        let out = server_error_response(&msg);
        let buf = out.collect_buffered().await.unwrap();
        let body = String::from_utf8(buf.body).unwrap_or_default();
        assert!(body.contains(">500<"), "body should contain 500 code");
        assert!(
            body.contains("Something went wrong"),
            "body should contain 'Something went wrong' title"
        );
    }

    /// Icons are Lucide SVGs from `ui::icons`, never text characters. A glyph
    /// icon renders in the body font (or, for emoji, the platform's colour
    /// emoji font), so it ignores `currentColor`, ignores the icon set's
    /// stroke weight, and sits on the text baseline instead of the icon grid
    /// -- it cannot be made to match the icons beside it.
    ///
    /// This scans BOTH spellings, because they are equally invisible in a
    /// rendered page but not equally visible in source: a literal `\u{26a0}`
    /// character, and the `\u{...}` Rust escape. Every glyph icon this guard
    /// was written for was in the escaped form -- a grep for emoji characters
    /// over this same tree reported zero hits while six escaped glyph icons
    /// (a speech balloon, a gear, two warning signs, a tick/cross pair and two
    /// back-arrows) were live on real pages. Checking only the literal form
    /// would reproduce exactly that false all-clear.
    ///
    /// General Punctuation (U+2000-U+206F) is deliberately NOT flagged: em
    /// dashes, ellipses and curly quotes are typography, not icons, and this
    /// tree uses them heavily as text.
    ///
    /// Only STRING LITERALS are scanned. Prose comments in this tree use `→`
    /// constantly ("Non-htmx → full document with chrome", "status→color
    /// policy") and none of it reaches a page; scanning whole lines made this
    /// guard fail on 15 comments and 1 real string, which is a guard that gets
    /// deleted rather than obeyed.
    #[test]
    fn pages_use_lucide_icons_not_text_glyphs() {
        /// Pictographs, dingbats, arrows and symbol blocks -- the ranges a
        /// glyph icon is drawn from. Excludes General Punctuation.
        fn is_glyph_icon(c: u32) -> bool {
            matches!(c,
                0x1F300..=0x1FAFF   // emoji / pictographs
                | 0x2190..=0x21FF   // arrows
                | 0x2600..=0x27BF   // misc symbols + dingbats (checkmarks, gear, warning)
                | 0x2B00..=0x2BFF   // misc symbols and arrows
                | 0xFE0F            // variation selector-16 (emoji presentation)
            )
        }

        /// Glyphs that stand for a physical KEY in a keyboard-shortcut hint,
        /// not for an action. The command palette's footer reads
        /// "\u{2191}\u{2193} navigate \u{b7} \u{21b5} open \u{b7} Esc close":
        /// those arrows are what is printed on the arrow keys, the same
        /// convention as \u{2318} for Cmd. Substituting Lucide arrows there
        /// would read as "go up"/"go back" -- an instruction rather than a key
        /// name -- and would not sit inline in a compact hint string.
        const KEYBOARD_HINT_GLYPHS: &[(&str, u32)] = &[
            ("src/ui/palette.rs", 0x2191), // UP ARROW
            ("src/ui/palette.rs", 0x2193), // DOWN ARROW
            ("src/ui/palette.rs", 0x21B5), // RETURN SYMBOL
        ];

        /// The spans of `line` that are inside a double-quoted literal.
        /// Deliberately simple: it tracks `\"` escapes and nothing else, which
        /// is enough because the alternative it must exclude -- comment prose
        /// -- never contains a quote-delimited glyph.
        fn string_spans(line: &str) -> Vec<String> {
            let mut spans = Vec::new();
            let mut cur = String::new();
            let mut in_str = false;
            let mut escaped = false;
            for c in line.chars() {
                if in_str {
                    if escaped {
                        cur.push(c);
                        escaped = false;
                    } else if c == '\\' {
                        cur.push(c);
                        escaped = true;
                    } else if c == '"' {
                        spans.push(std::mem::take(&mut cur));
                        in_str = false;
                    } else {
                        cur.push(c);
                    }
                } else if c == '"' {
                    in_str = true;
                }
            }
            if !cur.is_empty() {
                spans.push(cur);
            }
            spans
        }

        let roots = [
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/blocks"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui"),
        ];
        let mut offenders: Vec<String> = Vec::new();
        for root in roots {
            for entry in walkdir::WalkDir::new(root)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
                // `blocks/email.rs` builds HTML email bodies. Inline SVG is
                // unreliable across mail clients (Gmail strips it outright),
                // so a text glyph is the working mechanism there -- the same
                // carve-out, for the same reason, as the inline-style guard.
                .filter(|e| !e.path().ends_with("blocks/email.rs"))
                // `ui/icons.rs` IS the icon set; its doc comments name the
                // glyphs each icon replaced.
                .filter(|e| !e.path().ends_with("ui/icons.rs"))
            {
                let src = std::fs::read_to_string(entry.path()).unwrap();
                let file = entry
                    .path()
                    .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                    .unwrap_or(entry.path())
                    .display()
                    .to_string();
                let allowed = |cp: u32| {
                    KEYBOARD_HINT_GLYPHS
                        .iter()
                        .any(|(f, c)| *c == cp && file.replace('\\', "/").ends_with(f))
                };
                // Skip `#[cfg(test)]` modules: assertion messages are not
                // rendered markup, and they legitimately use symbols as prose
                // ("no row yet \u{21d2} defaults enabled"). Tracked by brace
                // depth from the `mod ... {` that follows the attribute.
                let mut test_mod_depth: Option<i32> = None;
                let mut pending_cfg_test = false;
                for (n, line) in src.lines().enumerate() {
                    let trimmed = line.trim_start();
                    if let Some(depth) = test_mod_depth.as_mut() {
                        *depth += line.matches('{').count() as i32;
                        *depth -= line.matches('}').count() as i32;
                        if *depth <= 0 {
                            test_mod_depth = None;
                        }
                        continue;
                    }
                    if trimmed.starts_with("#[cfg(test)]") {
                        pending_cfg_test = true;
                        continue;
                    }
                    if pending_cfg_test && trimmed.starts_with("mod ") && line.contains('{') {
                        pending_cfg_test = false;
                        let depth =
                            line.matches('{').count() as i32 - line.matches('}').count() as i32;
                        if depth > 0 {
                            test_mod_depth = Some(depth);
                        }
                        continue;
                    }
                    if trimmed.starts_with("//") {
                        continue; // comment line -- prose, never rendered
                    }
                    for span in string_spans(line) {
                        for c in span.chars() {
                            if is_glyph_icon(c as u32) && !allowed(c as u32) {
                                offenders.push(format!(
                                    "{file}:{}: literal glyph {c:?} (U+{:04X})",
                                    n + 1,
                                    c as u32
                                ));
                            }
                        }
                        // The escaped spelling: `\u{26a0}`.
                        let mut rest = span.as_str();
                        while let Some(i) = rest.find("\\u{") {
                            rest = &rest[i + 3..];
                            let Some(end) = rest.find('}') else { break };
                            if let Ok(cp) = u32::from_str_radix(&rest[..end], 16) {
                                if is_glyph_icon(cp) && !allowed(cp) {
                                    offenders.push(format!(
                                        "{file}:{}: escaped glyph \\u{{{:x}}} (U+{cp:04X})",
                                        n + 1,
                                        cp
                                    ));
                                }
                            }
                            rest = &rest[end..];
                        }
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "text glyphs used as icons -- use a `ui::icons::*` Lucide SVG instead \
             (add one if it is missing): {offenders:#?}"
        );
    }

    /// Inline styles bypass the shared style layer, so pages drift out of the
    /// design system one hardcoded colour at a time. The only legitimate use is
    /// handing a runtime value to CSS as a custom property.
    #[test]
    fn pages_carry_no_static_inline_styles() {
        // Both trees: `src/blocks` holds the page code, but `ui/settings_form.rs`
        // carries 28 of its own and is just as much a drift source.
        let roots = [
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/blocks"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui"),
        ];
        let mut offenders = Vec::new();
        for root in roots {
            for entry in walkdir::WalkDir::new(root)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
                .filter(|e| !e.path().ends_with("blocks/email.rs"))
            {
                let src = std::fs::read_to_string(entry.path()).unwrap();
                for (n, line) in src.lines().enumerate() {
                    // `blocks/email.rs` is exempt entirely: it builds HTML email
                    // bodies, and email clients strip `<style>` blocks and do not
                    // load external stylesheets, so inline styles are the only
                    // mechanism that works there. This is the same reason
                    // `assets::BRAND_ACCENT_HEX` exists. Removing them would break
                    // email rendering, not improve it.

                    // `style=(…)`/`style={…}` -- a Rust expression spliced into
                    // the attribute (maud's dynamic-attribute syntax) -- hands a
                    // runtime value to CSS, most often as a custom property
                    // (--size, --chart-color). That is the correct, deliberately
                    // allowed mechanism; skip it outright rather than trying to
                    // parse Rust expressions as CSS.
                    if line.contains("style=(") || line.contains("style={") {
                        continue;
                    }
                    // A *static* `style="…"` attribute is scanned per
                    // declaration, not as a whole line: a declaration is exempt
                    // only when it assigns a custom property itself
                    // (`--foo: …`), not merely because some *other* declaration
                    // on the same line/attribute happens to reference one via
                    // `var(--foo)`. `var(--foo)` as a *value* is exactly as
                    // static/hardcoded as any other literal -- only *setting* a
                    // custom property is the deferred-to-CSS mechanism this
                    // guard exists to preserve. (A line that merely contains
                    // the substring `style="` by coincidence -- e.g. this very
                    // check's own `"style="` string literal, self-scanned
                    // because this guard also walks `src/ui` -- never yields a
                    // colon-bearing declaration below, so it produces no
                    // offenders, same as a real absence.)
                    if let Some(value) = line
                        .split_once("style=\"")
                        .and_then(|(_, rest)| rest.split_once('"'))
                        .map(|(value, _)| value)
                    {
                        for decl in value.split(';').map(str::trim).filter(|d| !d.is_empty()) {
                            let Some((prop, _)) = decl.split_once(':') else {
                                continue;
                            };
                            if !prop.trim().starts_with("--") {
                                offenders.push(format!(
                                    "{}:{} ({decl})",
                                    entry.path().display(),
                                    n + 1
                                ));
                            }
                        }
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "static inline styles remain:\n{}",
            offenders.join("\n")
        );
    }

    /// `pages_carry_no_static_inline_styles` above only sees a static
    /// `style` attribute. Two other ways of hardcoding presentation are
    /// invisible to it *by construction*, not merely by directory scope
    /// (task 12e):
    ///
    /// 1. A page-local `const FOO_CSS: &str = "…"` rendered into an inline
    ///    `<style>` block -- real CSS, but outside `ui/styles/` where no
    ///    central audit sees it.
    /// 2. A JS `.style.<prop>` assignment carrying a literal value (e.g.
    ///    `row.style.cssText` or `el.style.display` set to a quoted
    ///    constant) -- per-element hardcoded styling that evades the first
    ///    guard because the attribute it scans for never appears; only a
    ///    JS property access does.
    ///
    /// A *template-interpolated* `.style.` assignment (a Rust `format!`
    /// value threaded into the JS, visible on the line as a `{…}`
    /// placeholder) is the legitimate dynamic case and is deliberately
    /// allowed -- e.g. a runtime-computed percentage or pixel offset.
    ///
    /// Deliberately a second, separately-scoped test rather than folded
    /// into `pages_carry_no_static_inline_styles`: broadening that guard's
    /// own pattern would immediately re-fail every file 12a-12d already
    /// cleared, destroying the invariant that a cleared file stays clear.
    #[test]
    fn pages_carry_no_page_local_style_drift() {
        // Scopes the dynamic-value check (below) to the assigned expression
        // only, not the rest of the physical line. This codebase's JS is
        // written as single minified lines, so a `{` from an unrelated
        // later callback/object literal in the same statement chain (e.g.
        // `.forEach(function(x){...})`, `JSON.stringify(y||{})`) must not
        // suppress a real offender -- exactly the false negative that let
        // two `row.style.cssText='...'` sites in `productManagerLoadPresets`/
        // `productManagerLoadLinks` survive an earlier version of this
        // guard undetected. A quoted string literal (the common case for a
        // style value) is scoped to its own contents; an unquoted
        // expression (e.g. a ternary of two literals) is scoped to the next
        // top-level `;` that ends the statement.
        fn assigned_expr(value: &str) -> &str {
            if let Some(quote @ ('\'' | '"')) = value.chars().next() {
                let rest = &value[1..];
                let mut escaped = false;
                for (i, c) in rest.char_indices() {
                    if escaped {
                        escaped = false;
                        continue;
                    }
                    if c == '\\' {
                        escaped = true;
                        continue;
                    }
                    if c == quote {
                        return &rest[..i];
                    }
                }
                return rest;
            }
            match value.find(';') {
                Some(end) => &value[..end],
                None => value,
            }
        }

        let roots = [
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/blocks"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui"),
        ];
        let mut css_const_offenders = Vec::new();
        let mut style_assignment_offenders = Vec::new();
        for root in roots {
            for entry in walkdir::WalkDir::new(root)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
                // This file's own detection code below necessarily contains
                // the patterns it searches for, in code form (not just the
                // doc comments the `//` skip above handles) -- e.g. the
                // literal `"_CSS:"` needle. Same shape as guard 1's
                // `blocks/email.rs` exemption: a legitimate, load-bearing
                // reason, not scope-narrowing.
                .filter(|e| !e.path().ends_with("ui/mod.rs"))
            {
                let src = std::fs::read_to_string(entry.path()).unwrap();
                for (n, line) in src.lines().enumerate() {
                    let loc = || format!("{}:{}", entry.path().display(), n + 1);

                    // Skip comments entirely for both vectors below (doc/
                    // inline notes quoting the pattern aren't drift).
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("//") {
                        continue;
                    }

                    // Vector 1: `const SOMETHING_CSS: &str = …` outside
                    // ui/styles/ (every file this test walks IS outside
                    // ui/styles/, since that directory holds only .css
                    // files, not .rs -- so any hit here is an offender).
                    if line.contains("_CSS:") && line.contains("&str") {
                        // A const bound to `include_str!` of a real `.css`
                        // file is not drift. What this vector exists to catch
                        // is CSS written as a Rust string literal, where no
                        // stylesheet tooling -- and no central audit, and not
                        // the class guard below -- can ever see it. A `.css`
                        // file is visible to all three; it just belongs to a
                        // block instead of the shared bundle.
                        //
                        // A block owning its stylesheet is deliberate, not a
                        // shortcut: `blocks::dev`'s assets are served from its
                        // own `/b/dev/static/` tier at the block's `Admin`
                        // access, and must be absent entirely from a build
                        // without `block-dev`. Folding them into `ui/styles/`
                        // would ship sandbox CSS to every deployment,
                        // including the Cloudflare build that deliberately
                        // carries no block-dev at all.
                        let from_stylesheet_file =
                            line.contains("include_str!") && line.contains(".css");
                        if !from_stylesheet_file {
                            css_const_offenders.push(loc());
                        }
                    }

                    // Vector 2: a literal-value `.style.<prop> = …`
                    // assignment.
                    let mut search_from = 0;
                    while let Some(rel) = line[search_from..].find(".style.") {
                        let prop_start = search_from + rel + ".style.".len();
                        let rest = &line[prop_start..];
                        // Property name: identifier chars only.
                        let prop_len = rest
                            .find(|c: char| !c.is_alphanumeric())
                            .unwrap_or(rest.len());
                        let after_prop = rest[prop_len..].trim_start();
                        // An assignment is a single `=` not immediately
                        // followed (or, since we trimmed whitespace-only
                        // before it, preceded) by another `=` -- so `===`/
                        // `==` (a comparison, e.g. `m.style.display ===
                        // 'none'`) is excluded.
                        if let Some(stripped) = after_prop.strip_prefix('=') {
                            if !stripped.starts_with('=') {
                                let value = stripped.trim_start();
                                // Dynamic case: a Rust `format!` placeholder
                                // threaded into this JS, inside the assigned
                                // expression itself (see `assigned_expr`
                                // above for why this isn't just "the rest of
                                // the line").
                                if !assigned_expr(value).contains('{') {
                                    style_assignment_offenders.push(loc());
                                }
                            }
                        }
                        search_from = prop_start + prop_len;
                    }
                }
            }
        }
        // Both asserted together (rather than the first short-circuiting
        // before the second runs) so a single failing run always reports
        // the full picture of both vectors.
        assert!(
            css_const_offenders.is_empty() && style_assignment_offenders.is_empty(),
            "page-local *_CSS stylesheet constants remain (migrate into ui/styles/components/*.css):\n{}\n\
             literal-value `.style.` JS assignments remain (convert to a class or the `hidden` IDL property):\n{}",
            css_const_offenders.join("\n"),
            style_assignment_offenders.join("\n"),
        );
    }

    /// From `chars[open]` (which must be `{`), scan forward tracking brace
    /// depth, skipping over string/char literals (plain and raw) and
    /// comments so their contents never perturb brace counting or get
    /// misread as markup. Returns the index one past the matching closing
    /// `}`, plus a "masked" copy of `chars[open..end]` where every
    /// string/char-literal character and every comment character has been
    /// replaced with a space (newlines are kept as newlines, so
    /// line-number arithmetic on the masked copy still lines up with the
    /// real source).
    fn scan_delimited_block(chars: &[char], open: usize) -> (usize, Vec<char>) {
        enum St {
            Code,
            Str,
            Char,
            RawStr(usize),
            LineComment,
            BlockComment,
        }
        let n = chars.len();
        let mut depth = 0i32;
        let mut j = open;
        let mut masked = Vec::with_capacity(n - open);
        let mut state = St::Code;
        while j < n {
            let c = chars[j];
            match state {
                St::LineComment => {
                    masked.push(if c == '\n' { '\n' } else { ' ' });
                    if c == '\n' {
                        state = St::Code;
                    }
                    j += 1;
                }
                St::BlockComment => {
                    if c == '*' && chars.get(j + 1) == Some(&'/') {
                        masked.push(' ');
                        masked.push(' ');
                        j += 2;
                        state = St::Code;
                        continue;
                    }
                    masked.push(if c == '\n' { '\n' } else { ' ' });
                    j += 1;
                }
                St::Str => {
                    if c == '\\' && j + 1 < n {
                        masked.push(' ');
                        masked.push(' ');
                        j += 2;
                        continue;
                    }
                    masked.push(if c == '\n' { '\n' } else { ' ' });
                    j += 1;
                    if c == '"' {
                        state = St::Code;
                    }
                }
                St::RawStr(hashes) => {
                    if c == '"' {
                        let mut k = j + 1;
                        let mut cnt = 0;
                        while k < n && chars[k] == '#' && cnt < hashes {
                            k += 1;
                            cnt += 1;
                        }
                        if cnt == hashes {
                            masked.resize(masked.len() + (k - j), ' ');
                            j = k;
                            state = St::Code;
                            continue;
                        }
                    }
                    masked.push(if c == '\n' { '\n' } else { ' ' });
                    j += 1;
                }
                St::Char => {
                    if c == '\\' && j + 1 < n {
                        masked.push(' ');
                        masked.push(' ');
                        j += 2;
                        continue;
                    }
                    masked.push(if c == '\n' { '\n' } else { ' ' });
                    j += 1;
                    if c == '\'' {
                        state = St::Code;
                    }
                }
                St::Code => {
                    if c == '/' && chars.get(j + 1) == Some(&'/') {
                        state = St::LineComment;
                        masked.push(' ');
                        masked.push(' ');
                        j += 2;
                        continue;
                    }
                    if c == '/' && chars.get(j + 1) == Some(&'*') {
                        state = St::BlockComment;
                        masked.push(' ');
                        masked.push(' ');
                        j += 2;
                        continue;
                    }
                    if c == 'r' {
                        let mut k = j + 1;
                        let mut hashes = 0usize;
                        while k < n && chars[k] == '#' {
                            hashes += 1;
                            k += 1;
                        }
                        if k < n && chars[k] == '"' {
                            masked.resize(masked.len() + (k + 1 - j), ' ');
                            j = k + 1;
                            state = St::RawStr(hashes);
                            continue;
                        }
                    }
                    if c == '"' {
                        state = St::Str;
                        masked.push(' ');
                        j += 1;
                        continue;
                    }
                    if c == '\'' {
                        // Distinguish a char literal ('x' or '\x' followed
                        // by a closing quote) from a lifetime ('a, 'static)
                        // -- a lifetime is not a string, so it must not
                        // suppress brace-counting or get masked.
                        let is_char_lit = (chars.get(j + 1) != Some(&'\\')
                            && chars.get(j + 2) == Some(&'\''))
                            || (chars.get(j + 1) == Some(&'\\') && chars.get(j + 3) == Some(&'\''));
                        if is_char_lit {
                            state = St::Char;
                            masked.push(' ');
                            j += 1;
                            continue;
                        }
                        masked.push(c);
                        j += 1;
                        continue;
                    }
                    if c == '{' {
                        depth += 1;
                        masked.push(c);
                        j += 1;
                        continue;
                    }
                    if c == '}' {
                        depth -= 1;
                        masked.push(c);
                        j += 1;
                        if depth == 0 {
                            break;
                        }
                        continue;
                    }
                    masked.push(c);
                    j += 1;
                }
            }
        }
        (j, masked)
    }

    /// Every `#[cfg(test)] mod ident { ... }` span in the file, as
    /// `(attribute_start, block_end)` char indices. `html!` invocations
    /// inside these spans are unit-test fixtures, not real pages -- a
    /// throwaway `html! { span .av {} }` built to exercise a template
    /// function's slot handling was never meant as a real design-system
    /// class, and scanning it produces exactly that false positive (this
    /// guard's own development tripped on `.av`/`.logo`/`.probe-icon`/
    /// `.probe-primary`/`.probe-spark`/`.probe-icon-users`/
    /// `.probe-icon-storage`, all test-only, none ever rendered on a real
    /// page). Only the `#[cfg(test)] mod ...` shape is recognized -- the
    /// only shape this codebase actually uses for gating a whole test
    /// module (154 of 164 `#[cfg(test)]` occurrences in `src/blocks` +
    /// `src/ui`; the other 10 gate individual non-html!-bearing seed-data
    /// helper functions, which this scan simply won't find a `mod` after,
    /// so they're harmlessly skipped).
    fn find_test_mod_spans(chars: &[char]) -> Vec<(usize, usize)> {
        let attr: Vec<char> = "#[cfg(test)]".chars().collect();
        let mut spans = Vec::new();
        let mut i = 0;
        'outer: while i + attr.len() <= chars.len() {
            if chars[i..i + attr.len()] != attr[..] {
                i += 1;
                continue;
            }
            let attr_start = i;
            let search_end = (i + attr.len() + 400).min(chars.len());
            let mut k = i + attr.len();
            while k + 3 < search_end {
                let is_mod_kw = chars[k] == 'm'
                    && chars[k + 1] == 'o'
                    && chars[k + 2] == 'd'
                    && chars.get(k + 3).is_some_and(|c| c.is_whitespace());
                if is_mod_kw {
                    let mut p = k + 3;
                    while p < search_end && chars[p].is_whitespace() {
                        p += 1;
                    }
                    while p < search_end && (chars[p].is_alphanumeric() || chars[p] == '_') {
                        p += 1;
                    }
                    while p < search_end && chars[p].is_whitespace() {
                        p += 1;
                    }
                    if p < search_end && chars[p] == '{' {
                        let (end, _masked) = scan_delimited_block(chars, p);
                        spans.push((attr_start, end));
                        i = end;
                        continue 'outer;
                    }
                }
                k += 1;
            }
            i += 1;
        }
        spans
    }

    /// Maud's dot-shorthand class syntax (`div .card__body { ... }`,
    /// `.alert .alert--success .mb-4`). A `.` only starts a class token
    /// when the previous character is a maud "new selector position"
    /// boundary (whitespace, `{`, `}`, `;`, or the very start of the
    /// block) -- this is what tells a real class shorthand apart from an
    /// ordinary Rust field/method access like `readiness.reasons` or
    /// `c.label` (preceded by an identifier character, never a boundary)
    /// inside the very same `html! { ... }` block, without needing to
    /// parse the surrounding Rust expression at all.
    ///
    /// Runs against the *masked* body (string/char-literal contents and
    /// comments already blanked by `scan_delimited_block`) so a raw CSS
    /// blob embedded via `style { (PreEscaped("...")) }` never gets read
    /// as if it were maud markup.
    ///
    /// A token immediately followed by `(` is a method call
    /// (`.into_iter()`, `.filter_map(...)`), not a class -- skipped
    /// *without* backtracking to a shorter match. (An earlier version of
    /// this scan used a greedy-regex-style match with a `(?!\()`
    /// lookahead; on backtracking failure the engine retried with the
    /// match one character shorter, which passed the lookahead and
    /// silently reported truncated garbage like `.as_st` for
    /// `.as_str()`. Consuming the full identifier once and only then
    /// checking the next character -- never re-shortening the match --
    /// avoids that class of bug entirely.)
    fn find_shorthand_classes(masked: &[char]) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < masked.len() {
            if masked[i] == '.' {
                let boundary_ok =
                    i == 0 || matches!(masked[i - 1], ' ' | '\t' | '\n' | '\r' | '{' | '}' | ';');
                if boundary_ok && masked.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic()) {
                    let start = i + 1;
                    let mut j = start;
                    while j < masked.len()
                        && (masked[j].is_ascii_alphanumeric()
                            || masked[j] == '_'
                            || masked[j] == '-')
                    {
                        j += 1;
                    }
                    if masked.get(j) == Some(&'(') {
                        // Method call, not a class -- record nothing, and
                        // resume scanning right after the identifier (not
                        // a shorter prefix of it).
                        i = j;
                        continue;
                    }
                    out.push((start, masked[start..j].iter().collect()));
                    i = j;
                    continue;
                }
            }
            i += 1;
        }
        out
    }

    /// Maud's static `class="foo bar"` attribute form -- a plain
    /// space-separated literal, never dynamic. No escape handling: every
    /// instance of this attribute in `src/blocks`/`src/ui` (verified by
    /// grep before writing this) is a plain token list with no embedded
    /// quote, so a naive scan-to-next-`"` is exact here, not just a
    /// heuristic approximation.
    fn find_class_attr_literals(body: &[char]) -> Vec<(usize, String)> {
        let needle: Vec<char> = "class=\"".chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i + needle.len() <= body.len() {
            if body[i..i + needle.len()] == needle[..] {
                let val_start = i + needle.len();
                let mut j = val_start;
                while j < body.len() && body[j] != '"' {
                    j += 1;
                }
                out.push((i, body[val_start..j].iter().collect()));
                i = j + 1;
                continue;
            }
            i += 1;
        }
        out
    }

    /// Maud's dynamic `class={ ... }` attribute -- a mix of literal string
    /// fragments and interpolated/conditional pieces, e.g.
    /// `class={ "block-card" @if !is_enabled { " block-card--disabled" } }`.
    /// Only the literal `"..."` fragments are extracted (both come back
    /// here: `"block-card"` and `" block-card--disabled"`); the `@if`
    /// keyword and any `(expr)` splice inside the block are left alone --
    /// per this guard's documented scope, a class name assembled from a
    /// Rust expression rather than written as a literal isn't something a
    /// static scan can verify, so it's silently skipped rather than
    /// guessed at.
    fn find_class_dyn_literals(body: &[char]) -> Vec<(usize, Vec<String>)> {
        let needle: Vec<char> = "class={".chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i + needle.len() <= body.len() {
            if body[i..i + needle.len()] == needle[..] {
                let open = i + needle.len() - 1;
                let (end, _masked_unused) = scan_delimited_block(body, open);
                let mut literals = Vec::new();
                let mut j = open;
                while j < end {
                    if body[j] == '"' {
                        let vs = j + 1;
                        let mut k = vs;
                        while k < end && body[k] != '"' {
                            k += 1;
                        }
                        literals.push(body[vs..k].iter().collect());
                        j = k + 1;
                        continue;
                    }
                    j += 1;
                }
                out.push((i, literals));
                i = end;
                continue;
            }
            i += 1;
        }
        out
    }

    /// Walks one `.rs` file's `html! { ... }` invocations (skipping any
    /// inside a `#[cfg(test)]` module -- see `find_test_mod_spans`) and
    /// records every class the three mechanisms above find, keyed by class
    /// name with the first `(file, line, snippet)` it was seen at.
    fn collect_markup_classes(
        src: &str,
        path: &str,
        used: &mut std::collections::BTreeMap<String, (String, usize, String)>,
    ) {
        let chars: Vec<char> = src.chars().collect();
        let test_spans = find_test_mod_spans(&chars);
        let in_test = |pos: usize| test_spans.iter().any(|&(s, e)| pos >= s && pos < e);

        let needle: Vec<char> = "html!".chars().collect();
        let mut i = 0;
        while i + needle.len() <= chars.len() {
            if chars[i..i + needle.len()] != needle[..] {
                i += 1;
                continue;
            }
            let mut p = i + needle.len();
            while p < chars.len() && chars[p].is_whitespace() {
                p += 1;
            }
            if chars.get(p) != Some(&'{') || in_test(i) {
                i += 1;
                continue;
            }
            let open = p;
            let (end, masked) = scan_delimited_block(&chars, open);
            let base_line = 1 + chars[..open].iter().filter(|&&c| c == '\n').count();
            let real_body = &chars[open..end];

            for (idx, name) in find_shorthand_classes(&masked) {
                let line = base_line + masked[..idx].iter().filter(|&&c| c == '\n').count();
                let snip_start = idx.saturating_sub(20);
                let snip_end = (idx + 30).min(masked.len());
                let snippet: String = masked[snip_start..snip_end]
                    .iter()
                    .collect::<String>()
                    .replace('\n', " ");
                used.entry(name)
                    .or_insert_with(|| (path.to_string(), line, snippet));
            }

            for (attr_idx, value) in find_class_attr_literals(real_body) {
                let line = base_line + real_body[..attr_idx].iter().filter(|&&c| c == '\n').count();
                for tok in value.split_whitespace() {
                    if tok.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
                        used.entry(tok.to_string()).or_insert_with(|| {
                            (path.to_string(), line, format!("class=\"{value}\""))
                        });
                    }
                }
            }

            for (blk_idx, literals) in find_class_dyn_literals(real_body) {
                let line = base_line + real_body[..blk_idx].iter().filter(|&&c| c == '\n').count();
                for value in &literals {
                    for tok in value.split_whitespace() {
                        if tok.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
                            used.entry(tok.to_string()).or_insert_with(|| {
                                (path.to_string(), line, format!("class={{\"{value}\"}}"))
                            });
                        }
                    }
                }
            }

            i = end;
        }
    }

    /// Non-nested `/* ... */` stripper -- CSS comments never nest, so this
    /// is exact, not a heuristic.
    fn strip_css_comments_for_class_scan(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '/' && chars.peek() == Some(&'*') {
                chars.next();
                while let Some(c2) = chars.next() {
                    if c2 == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    /// Every `.classname` token appearing in `text`. Used both for a CSS
    /// selector (everything before a `{`) and it does not need to know the
    /// selector's full grammar -- comma-separated lists, compound
    /// selectors (`.foo.bar`), descendant combinators (`.foo .bar`),
    /// pseudo-classes/elements, attribute selectors -- extracting every
    /// `.ident` substring finds every class in all of them alike.
    fn collect_class_tokens(text: &str, out: &mut std::collections::HashSet<String>) {
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '.'
                && matches!(chars.get(i + 1), Some(c) if c.is_ascii_alphabetic() || *c == '_')
            {
                let start = i + 1;
                let mut j = start;
                while j < chars.len()
                    && (chars[j].is_ascii_alphanumeric() || chars[j] == '_' || chars[j] == '-')
                {
                    j += 1;
                }
                out.insert(chars[start..j].iter().collect());
                i = j;
                continue;
            }
            i += 1;
        }
    }

    /// Every class any rule in a stylesheet defines -- selector text is
    /// everything since the last `{`/`}`/`;` boundary, up to (not
    /// including) the next `{`; this naturally covers rules nested inside
    /// `@media`/`@supports` blocks too, since their inner rules' `{` are
    /// found by the same scan.
    fn collect_css_classes(css_no_comments: &str, out: &mut std::collections::HashSet<String>) {
        let chars: Vec<char> = css_no_comments.chars().collect();
        let mut last_boundary = 0usize;
        for (i, &c) in chars.iter().enumerate() {
            if c == '{' {
                let selector: String = chars[last_boundary..i].iter().collect();
                collect_class_tokens(&selector, out);
                last_boundary = i + 1;
            } else if c == '}' || c == ';' {
                last_boundary = i + 1;
            }
        }
    }

    /// Nothing in this codebase checked that a class used in maud markup
    /// actually has a matching rule in `ui/styles/` -- exactly how
    /// `.card-body` (a typo for `.card__body`), `.alert-success`/
    /// `.alert-warning` (stragglers from a superseded single-dash naming
    /// scheme; the BEM family is `.alert--success`/`.alert--warning`),
    /// `.breadcrumbs`/`.breadcrumbs__sep`, and `.row--folder` all shipped
    /// rendering with silent no-op styling (admin-redesign task,
    /// 2026-09-01/02 -- see `.superpowers/sdd/2026-09-01-admin-redesign/
    /// undefined-classes-report.md`). This guard closes that hole: it
    /// extracts every class maud markup in `src/blocks`/`src/ui` actually
    /// renders, extracts every class `ui/styles/**/*.css` actually
    /// defines, and asserts the former is a subset of the latter.
    ///
    /// Three extraction mechanisms, each a real maud class-authoring
    /// syntax used in this codebase (see the three `find_*` helpers above
    /// for how each works):
    ///   1. Shorthand `.foo` tokens -- the overwhelming majority of usage,
    ///      and the syntax all five original bugs used.
    ///   2. A static `class="foo bar"` attribute.
    ///   3. The *literal string* fragments of a dynamic
    ///      `class={ "foo" @if c { " bar" } }` attribute.
    ///
    /// What this deliberately cannot see -- skipped outright, not silently
    /// assumed fine:
    ///   - `class=(expr)` -- a single fully-dynamic Rust expression (e.g.
    ///     `img class=(class) ...` in `templates.rs`). The class name
    ///     isn't a literal anywhere in the markup to check.
    ///   - `.(expr)` -- maud's dynamic-class shorthand. This never even
    ///     reaches the skip logic: `find_shorthand_classes` requires a
    ///     *letter* immediately after the `.`, so `.(` fails on the very
    ///     first character, by construction.
    ///   - Any interpolated `(expr)` piece inside a `class={ ... }` block
    ///     (e.g. `(variant.class())`, `(size_class)`) -- only the literal
    ///     string fragments around it are read.
    ///   - A class name that only ever exists inside a big JS
    ///     template-literal string spliced into a `<script>` (e.g. the
    ///     wizard-row `innerHTML` templates in `blocks/products/pages.rs`,
    ///     which build raw `<div class="...">` HTML as plain text). That
    ///     text lives inside a Rust string constant, not as maud attribute
    ///     syntax, so this guard's html!-scoped extraction never sees it
    ///     even though the string is spliced into an `html! {}` block
    ///     elsewhere in the same file.
    ///   - Test-fixture markup: every `#[cfg(test)] mod ... { ... }`
    ///     module is excluded outright (see `find_test_mod_spans`), not
    ///     just its assertions -- unit tests routinely stand up throwaway
    ///     `html! { span .av {} }`-style placeholders to exercise a
    ///     template function's slot handling, never meaning `.av` as a
    ///     real design-system class.
    ///
    /// Two named exceptions where a real, non-test usage has no
    /// stylesheet rule *by design*:
    ///   - `cf-turnstile` (`blocks/tickets/public.rs`) -- Cloudflare
    ///     Turnstile's own script finds and fills this element by class
    ///     name per Cloudflare's public embed contract; not this
    ///     codebase's CSS to own.
    ///   - `bulk-select` (`blocks/files/pages_user/objects.rs`) -- a pure
    ///     JS selector hook (`files-browser.js`'s
    ///     `document.querySelectorAll('.bulk-select')`, confirmed by
    ///     grep) on a native `<input type="checkbox">`; the browser's
    ///     native checkbox rendering *is* the entire visual treatment,
    ///     deliberately unstyled.
    ///
    /// Everything else this guard finds -- i.e. that this task's five-class
    /// fix didn't touch -- is real, pre-existing drift outside this task's
    /// scope, listed explicitly in `KNOWN_PRE_EXISTING_GAPS` below (each
    /// with a file:line) rather than silently passed, so this guard still
    /// catches every *new* undefined-class regression from here on, and a
    /// fixed entry has to be deleted from the list (the trailing
    /// self-check below fails loudly if a listed name stops being both
    /// used and undefined -- so the list can't silently rot into covering
    /// for a class nobody uses any more, or one that got a real rule
    /// without anyone remembering to shrink this list). Some of these are
    /// styled only in the page's own inline
    /// `style { (PreEscaped("...")) }` block rather than centrally in
    /// `ui/styles/` -- the exact drift `pages_carry_no_page_local_style_drift`
    /// exists to catch, but that guard fingerprints named `_CSS: &str`
    /// consts, not these anonymous blocks, a gap in *that* guard this one
    /// incidentally exposes. The rest have no rule anywhere at all. See
    /// the admin-redesign task report for the full inventory (38 names)
    /// and a recommended follow-up; this task's mandate was five specific
    /// classes, not an unbounded stylesheet audit.
    #[test]
    fn pages_use_only_classes_defined_in_the_stylesheet() {
        // The shared bundle, plus any stylesheet a block owns and serves
        // itself. The invariant is that a class used in markup has a rule in
        // some stylesheet that reaches the page rendering it -- NOT that every
        // class lives in the shared bundle. `blocks::dev` serves its own
        // `dev.css` from `/b/dev/static/`, so its `.dev-*` rules are as real
        // to that page as `ui/styles/` is to an admin page; requiring them in
        // the shared bundle would ship them to builds that do not have the
        // block. Scanning both is what keeps this guard honest without
        // forcing that.
        let blocks_root = concat!(env!("CARGO_MANIFEST_DIR"), "/src/blocks");

        /// The block a path under `src/blocks/` belongs to (`"dev"` for
        /// `src/blocks/dev/assets/dev.css`), or `None` for anything outside.
        fn owning_block(path: &str, blocks_root: &str) -> Option<String> {
            path.strip_prefix(blocks_root)?
                .trim_start_matches(std::path::MAIN_SEPARATOR)
                .split(std::path::MAIN_SEPARATOR)
                .next()
                .filter(|seg| !seg.is_empty() && !seg.ends_with(".rs"))
                .map(str::to_string)
        }

        // The shared bundle, which reaches every page this crate renders.
        let mut defined: std::collections::HashSet<String> = std::collections::HashSet::new();
        for entry in walkdir::WalkDir::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/ui/styles"
        ))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "css"))
        {
            let src = std::fs::read_to_string(entry.path()).unwrap();
            collect_css_classes(&strip_css_comments_for_class_scan(&src), &mut defined);
        }

        // Stylesheets a block owns and serves itself, kept PER BLOCK rather
        // than merged into `defined`.
        //
        // `blocks/dev/assets/dev.css` is served from `/b/dev/static/` and
        // reaches the sandbox page and nothing else, so `.dev-pane` used from
        // an admin page is still an offender. Merging every block's
        // stylesheet into one vocabulary would hide exactly that mistake,
        // which is the failure this guard exists to catch — a class in markup
        // with no rule on the page that renders it.
        //
        // A block owning a stylesheet is deliberate: folding these into
        // `ui/styles/` would ship sandbox CSS to every deployment, including
        // builds compiled without `block-dev` at all.
        let mut block_local: std::collections::HashMap<
            String,
            std::collections::HashSet<String>,
        > = Default::default();
        for entry in walkdir::WalkDir::new(blocks_root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "css"))
        {
            let path = entry.path().display().to_string();
            let Some(owner) = owning_block(&path, blocks_root) else {
                continue;
            };
            let src = std::fs::read_to_string(entry.path()).unwrap();
            collect_css_classes(
                &strip_css_comments_for_class_scan(&src),
                block_local.entry(owner).or_default(),
            );
        }

        let roots = [
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/blocks"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui"),
        ];
        let mut used: std::collections::BTreeMap<String, (String, usize, String)> =
            Default::default();
        for root in roots {
            for entry in walkdir::WalkDir::new(root)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
            {
                let src = std::fs::read_to_string(entry.path()).unwrap();
                collect_markup_classes(&src, &entry.path().display().to_string(), &mut used);
            }
        }

        const NO_STYLESHEET_RULE_NEEDED: &[&str] = &["cf-turnstile", "bulk-select"];

        const KNOWN_PRE_EXISTING_GAPS: &[&str] = &[
            // Styled only in the page's own inline
            // `style { (PreEscaped("...")) }` block, not centrally --
            // blocks/tickets/pages.rs and blocks/admin/pages/network.rs.
            "ticket-col-type",
            "ticket-col-source",
            "ticket-col-age",
            "ticket-filter-assignee",
            "tickets-table",
            "ticket-analysis-meta",
            "ticket-analysis-actions",
            "detail-rows",
            "expand-row",
            // No rule anywhere.
            "auth-form",
            "bulk-select-all",
            "chat-form",
            "checkbox-inline",
            "custom-tab",
            "custom-tab__hint",
            "dashboard-grid__primary",
            "dashboard-grid__secondary",
            "db-table-list__count",
            "detail-body__main",
            "empty__action",
            "form-section__head",
            "kebab-trigger",
            "kv-list",
            "messages-new__title",
            "messages-new__type",
            "nav-icon",
            "page--dashboard",
            "page--detail",
            "page--form",
            "page--list",
            "pagination__page",
            "palette__item-label",
            "quota-card",
            "quota-warning",
            "section",
            "sidebar__brand--text",
        ];

        let exempt: std::collections::HashSet<&str> = NO_STYLESHEET_RULE_NEEDED
            .iter()
            .chain(KNOWN_PRE_EXISTING_GAPS.iter())
            .copied()
            .collect();

        let mut offenders = Vec::new();
        for (class, (file, line, snippet)) in &used {
            // A rule in the owning block's own stylesheet counts, because that
            // sheet is served with that block's pages. A rule in some OTHER
            // block's stylesheet does not.
            let served_locally = owning_block(file, blocks_root)
                .and_then(|owner| block_local.get(&owner))
                .is_some_and(|classes| classes.contains(class));
            if defined.contains(class) || served_locally || exempt.contains(class.as_str()) {
                continue;
            }
            offenders.push(format!("{file}:{line}: .{class}  ({snippet})"));
        }
        assert!(
            offenders.is_empty(),
            "classes used in markup with no matching rule in ui/styles/**/*.css:\n{}",
            offenders.join("\n")
        );

        // Anti-rot: every named exception must still be both actually used
        // and actually undefined, or it's either dead weight (nothing uses
        // that name any more -- delete it) or stale (something now defines
        // it -- delete it and let the main assertion above re-verify the
        // usage for real).
        let mut stale = Vec::new();
        for name in NO_STYLESHEET_RULE_NEEDED
            .iter()
            .chain(KNOWN_PRE_EXISTING_GAPS.iter())
        {
            let still_used = used.contains_key(*name);
            let still_undefined = !defined.contains(*name);
            if !(still_used && still_undefined) {
                stale.push(*name);
            }
        }
        assert!(
            stale.is_empty(),
            "exception list entries no longer both used and undefined -- remove them: {stale:?}"
        );
    }
}
