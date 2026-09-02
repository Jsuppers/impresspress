//! Embedded static assets — CSS and JS.
//!
//! Asset URLs include a content hash for cache busting:
//! `/b/static/app-{hash}.css` and `/b/static/htmx-{hash}.min.js`

use std::sync::OnceLock;

use crate::routing::STATIC_PREFIX;

include!(concat!(env!("OUT_DIR"), "/asset_manifest.rs"));

/// Look up a manifest entry by logical key. Panics on an unknown key: every
/// call site passes a literal that `build.rs` also knows, so a miss is a build
/// mismatch, not a runtime condition.
pub fn entry(logical: &str) -> &'static AssetEntry {
    ASSETS
        .iter()
        .find(|e| e.logical == logical)
        .unwrap_or_else(|| panic!("asset not in manifest: {logical}"))
}

/// Where assets are fetched from when no explicit base is configured.
/// Version-pinned so publishing a new release never breaks a deployment
/// still running an older one.
pub const DEFAULT_CDN_BASE_TEMPLATE: &str = concat!(
    "https://cdn.impresspress.org/ui/v",
    env!("CARGO_PKG_VERSION"),
    "/"
);

/// Worker var / process env var name resolved by [`base_url`]. Shared
/// single source of truth between the writer (the CLI's `wrangler.toml`
/// generator — `impresspress::cli::helpers::cloudflare::wrangler`) and every
/// reader, so the name can never drift out from under either side.
pub const ASSET_BASE_URL_VAR: &str = "IMPRESSPRESS_ASSET_BASE_URL";

/// Platform-pushed override for [`base_url`], for adapters that cannot rely
/// on `std::env` to see [`ASSET_BASE_URL_VAR`]. Cloudflare Workers stub
/// `std::env` to always-empty on `wasm32-unknown-unknown` — the only channel
/// that carries a Worker `[vars]` entry into Rust is `worker::Env::var`, which
/// requires a live per-request `Env` handle `base_url()` doesn't have. The
/// Cloudflare adapter (`impresspress-cloudflare::run_with_config`) reads the
/// var itself and calls this once per isolate, before dispatching any
/// request, so by the time a page render calls `base_url()` the value is
/// already resolved. Native targets never call this: `std::env::var` already
/// reads the real process environment there.
static BASE_URL_OVERRIDE: OnceLock<Option<String>> = OnceLock::new();

/// Register the platform override described on [`BASE_URL_OVERRIDE`]. Must
/// be called, if at all, before the first call to [`base_url`] — `base_url`
/// caches its resolved value forever after its first call. Idempotent: later
/// calls are silently ignored rather than panicking, because a Cloudflare
/// isolate can build the runtime more than once per isolate lifetime (e.g.
/// the `/_deploy/init` funnel always builds a fresh runtime); every call in
/// a given isolate reads the same fixed Worker var, so only the first one
/// needs to land.
pub fn set_base_url_override(value: Option<String>) {
    let _ = BASE_URL_OVERRIDE.set(value);
}

/// Resolve the asset base URL once.
///
/// 1. `IMPRESSPRESS_ASSET_BASE_URL` (infrastructure config — `IMPRESSPRESS_*`,
///    no `__`, never stored in the DB) wins outright. The deployer sets this
///    when assets live somewhere other than this origin. Read via
///    [`BASE_URL_OVERRIDE`] when a platform adapter pushed one in, otherwise
///    via `std::env::var` directly (the native target's real channel).
/// 2. Otherwise `/b/static/`, served by the system block — from memory when
///    `embed-assets` is on, streamed from R2 when it is off.
///
/// Whether R2 is configured is deploy-time knowledge, so the CLI writes that
/// decision into the env var rather than the runtime sniffing for a backend.
pub fn base_url() -> &'static str {
    static BASE: OnceLock<String> = OnceLock::new();
    BASE.get_or_init(|| {
        let from_override = BASE_URL_OVERRIDE.get().cloned().flatten();
        let resolved = from_override.or_else(|| std::env::var(ASSET_BASE_URL_VAR).ok());
        match resolved {
            Some(v) if !v.trim().is_empty() => {
                let v = v.trim().to_string();
                if v.ends_with('/') { v } else { format!("{v}/") }
            }
            _ => STATIC_PREFIX.to_string(),
        }
    })
}

/// Full URL for a logical asset key, e.g. `url("app.css")`.
pub fn url(logical: &str) -> String {
    format!("{}{}", base_url(), entry(logical).filename)
}

/// The single embed point for every asset's bytes. Each arm is either the
/// asset's own `include_str!`/`include_bytes!` literal or (for `app.css`)
/// delegates to `css()`, itself an `include_str!` of the `build.rs`-assembled
/// bundle — either way, exactly one `include_*!` per source file in the
/// whole crate, so asset content has one source-level truth.
#[cfg(feature = "embed-assets")]
pub fn bytes(logical: &str) -> Option<&'static [u8]> {
    Some(match logical {
        "app.css" => css().as_bytes(),
        // htmx 2.x minified JS.
        "htmx.min.js" => include_str!("assets/htmx.min.js").as_bytes(),
        // WebMCP tool-registration script, served on every page. Fetches the
        // auth-filtered manifest at `/b/webmcp/manifest.json` and registers
        // each tool via `document.modelContext.registerTool` (no-ops on
        // browsers without WebMCP support).
        "webmcp.js" => include_str!("assets/webmcp.js").as_bytes(),
        // Itim font binaries, sourced from `impresspress/site-kit`'s
        // `/fonts/` mirror and committed here so every impresspress
        // deployment ships its own glyphs (no cross-origin runtime
        // dependency, no `https://impresspress.org/fonts/` 404).
        "itim-latin.woff2" => include_bytes!("assets/fonts/itim-latin.woff2"),
        "itim-latin-ext.woff2" => include_bytes!("assets/fonts/itim-latin-ext.woff2"),
        // Square Impresspress mark used as the sidebar/login icon. Bundled
        // locally so the admin renders correctly without internet (the
        // previous default pointed at `https://impresspress.org/images/logo.png`
        // which 404s offline).
        "impresspress-logo.png" => include_bytes!("assets/impresspress-logo.png"),
        // Impresspress wordmark/long logo — used in the sidebar brand and
        // login splash.
        "impresspress-logo-long.png" => include_bytes!("assets/impresspress-logo-long.png"),
        // Impresspress favicon — bundled so every deployment ships its own
        // `<link rel="icon">` target without depending on a per-deployment
        // external URL or the implicit browser fallback to `/favicon.ico`
        // (which 404s by default).
        "favicon.ico" => include_bytes!("assets/favicon.ico"),
        // marked.js (markdown parser), vendored from marked@14 — self-hosted
        // instead of a jsdelivr CDN `<script>` so there's no external
        // runtime fetch (CSP-friendly, no third-party availability/supply-
        // chain dependency at page load). Only consumed by the LLM chat
        // page (`blocks::llm::pages`), which is itself gated behind
        // `block-llm` — feature-gated here too so a build without the LLM
        // block (e.g. Cloudflare, which can't enable `block-llm`: the
        // provider service isn't wasm32-compatible) doesn't embed this JS.
        #[cfg(feature = "block-llm")]
        "marked.min.js" => include_str!("assets/marked.min.js").as_bytes(),
        // DOMPurify (HTML sanitizer), vendored from DOMPurify 3.2.4 —
        // self-hosted instead of a CDN `<script>` so there's no external
        // runtime fetch (CSP-friendly, no third-party availability/supply-
        // chain dependency at page load). Loaded before `marked.js`/
        // `llm-chat.js` so `renderMarkdown` can sanitize the parsed
        // markdown before it reaches `innerHTML` (P0 stored-XSS fix). Like
        // marked.js, only the LLM chat page loads this — gated behind
        // `block-llm` for the same reason.
        #[cfg(feature = "block-llm")]
        "purify.min.js" => include_str!("assets/purify.min.js").as_bytes(),
        // Embedded vanilla-JS bundle for the LLM chat surface — markdown,
        // message rendering, model management, chat submission, thread
        // creation/selection. Consumed by the unified LLM page handler and
        // (for the conversation lens) by the Messages context_detail
        // handler. Gated behind `block-llm`: Cloudflare currently cannot
        // enable `block-llm` (the provider service isn't wasm32-compatible),
        // so this JS has no consumer there — embedding it unconditionally
        // was pure bloat.
        #[cfg(feature = "block-llm")]
        "llm-chat.js" => include_str!("assets/llm-chat.js").as_bytes(),
        // Embedded vanilla-JS bundle for the file-browser surfaces —
        // drag-drop upload, bulk select, kebab menus, share modal, upload
        // modal, confirm-delete. Consumed by `pages_user::object_list_page`
        // and `cloudstorage_page`, both in the `block-files`-gated
        // `blocks::files` module — gated here to match, so a build without
        // the Files block drops this JS too.
        #[cfg(feature = "block-files")]
        "files-browser.js" => include_str!("assets/files-browser.js").as_bytes(),
        _ => return None,
    })
}

/// The built-in brand accent, as a hex literal for surfaces that can't use
/// CSS variables (email inline styles). Must match `--primary-color` in
/// `styles/tokens.css` — the `brand_accent_matches_tokens_css` test pins the
/// two together so they can't drift.
pub const BRAND_ACCENT_HEX: &str = "#fd3534";

/// Square logo URL with content hash, e.g. `/b/static/impresspress-logo-a1b2c3d4.png`.
pub fn logo_icon_url() -> String {
    url("impresspress-logo.png")
}

/// Long/wordmark logo URL with content hash, e.g. `/b/static/impresspress-logo-long-a1b2c3d4.png`.
pub fn logo_long_url() -> String {
    url("impresspress-logo-long.png")
}

/// Favicon URL with content hash, e.g. `/b/static/favicon-a1b2c3d4.ico`.
pub fn favicon_url() -> String {
    url("favicon.ico")
}

/// The assembled CSS bundle, built by `build.rs` from `CSS_ORDER`.
pub fn css() -> &'static str {
    include_str!(concat!(env!("OUT_DIR"), "/app.css"))
}

/// CSS URL with content hash, e.g. `/b/static/app-a1b2c3d4.css`
pub fn css_url() -> String {
    url("app.css")
}

/// htmx JS URL with content hash, e.g. `/b/static/htmx-a1b2c3d4.min.js`
pub fn htmx_js_url() -> String {
    url("htmx.min.js")
}

/// WebMCP script URL with content hash, e.g. `/b/static/webmcp-a1b2c3d4.js`
pub fn webmcp_js_url() -> String {
    url("webmcp.js")
}

/// marked.js URL with content hash, e.g. `/b/static/marked-a1b2c3d4.min.js`
#[cfg(feature = "block-llm")]
pub fn marked_js_url() -> String {
    url("marked.min.js")
}

/// DOMPurify JS URL with content hash, e.g. `/b/static/purify-a1b2c3d4.js`
#[cfg(feature = "block-llm")]
pub fn purify_js_url() -> String {
    url("purify.min.js")
}

/// LLM chat JS URL with content hash, e.g. `/b/static/llm-chat-a1b2c3d4.js`.
/// Not minified — readability matters for a script that's debugged in
/// Chrome devtools.
#[cfg(feature = "block-llm")]
pub fn llm_chat_js_url() -> String {
    url("llm-chat.js")
}

/// Files-browser JS URL with content hash, e.g. `/b/static/files-browser-a1b2c3d4.js`.
#[cfg(feature = "block-files")]
pub fn files_browser_js_url() -> String {
    url("files-browser.js")
}

/// Small inline JS for toast notifications (triggered by htmx HX-Trigger).
pub fn toast_js() -> &'static str {
    r#"
document.body.addEventListener("showToast", function(e) {
    var d = e.detail || {};
    var c = document.getElementById("toast-container");
    if (!c) return;
    var t = document.createElement("div");
    var kind = ["success", "error", "warning", "info"].indexOf(d.type) >= 0 ? d.type : "info";
    t.className = "toast toast-" + kind;
    var message = document.createElement("span");
    message.textContent = String(d.message || "");
    var dismiss = document.createElement("button");
    dismiss.className = "toast-dismiss";
    dismiss.type = "button";
    dismiss.setAttribute("aria-label", "Dismiss");
    dismiss.textContent = "×";
    dismiss.addEventListener("click", function() { t.remove(); });
    t.appendChild(message);
    t.appendChild(dismiss);
    c.appendChild(t);
    setTimeout(function() { t.remove(); }, 4000);
});
"#
}

/// Vanilla JS for the command palette — open/close, fuzzy filter,
/// keyboard navigation. Embedded as a string the same way `toast_js()`
/// and `modal_js()` are.
pub fn palette_js() -> &'static str {
    r#"
(function () {
  if (window.__cmdkInit) return;
  window.__cmdkInit = true;
  const el = document.getElementById('cmdk');
  if (!el) return;
  const input = document.getElementById('cmdk-input');
  const list = document.getElementById('cmdk-list');

  const items = () => Array.from(list.querySelectorAll('.palette__item'));
  let selected = 0;

  function open() {
    el.dataset.open = 'true';
    el.setAttribute('aria-hidden', 'false');
    input.value = '';
    apply('');
    requestAnimationFrame(() => input.focus());
  }
  function close() {
    el.dataset.open = 'false';
    el.setAttribute('aria-hidden', 'true');
  }
  function visibleItems() { return items().filter(i => !i.classList.contains('is-hidden')); }

  function apply(query) {
    const q = query.trim().toLowerCase();
    items().forEach(i => {
      const k = (i.dataset.keywords || '').toLowerCase();
      const match = !q || k.includes(q);
      i.classList.toggle('is-hidden', !match);
      i.setAttribute('aria-selected', 'false');
    });
    const vis = visibleItems();
    selected = 0;
    if (vis[0]) vis[0].setAttribute('aria-selected', 'true');
  }

  function move(delta) {
    const vis = visibleItems();
    if (!vis.length) return;
    vis[selected]?.setAttribute('aria-selected', 'false');
    selected = (selected + delta + vis.length) % vis.length;
    vis[selected].setAttribute('aria-selected', 'true');
    vis[selected].scrollIntoView({ block: 'nearest' });
  }

  function activate() {
    const vis = visibleItems();
    const sel = vis[selected];
    if (!sel?.dataset.href) return;
    if (sel.dataset.external === 'true') {
      window.open(sel.dataset.href, '_blank', 'noopener,noreferrer');
    } else {
      window.location.assign(sel.dataset.href);
    }
  }

  // Hotkeys
  document.addEventListener('keydown', (e) => {
    const isMod = e.metaKey || e.ctrlKey;
    if (isMod && e.key.toLowerCase() === 'k') { e.preventDefault(); open(); return; }
    if (el.dataset.open !== 'true') return;
    if (e.key === 'Escape') { e.preventDefault(); close(); }
    else if (e.key === 'ArrowDown') { e.preventDefault(); move(1); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); move(-1); }
    else if (e.key === 'Enter') { e.preventDefault(); activate(); }
  });

  // Click triggers
  document.addEventListener('click', (e) => {
    const t = e.target.closest('[data-action]');
    if (!t) return;
    if (t.dataset.action === 'palette-open') { e.preventDefault(); open(); }
    if (t.dataset.action === 'palette-close') { e.preventDefault(); close(); }
  });

  // The shortcut hint defaults to the Mac glyph; swap to Ctrl elsewhere so
  // the advertised key matches what the keydown handler above accepts.
  if (!/Mac|iPhone|iPad|iPod/.test(navigator.platform || '')) {
    document.querySelectorAll('.topbar__palette-cmd').forEach((n) => { n.textContent = 'Ctrl'; });
    document.querySelectorAll('.shell__palette-icon').forEach((n) => { n.textContent = 'Ctrl K'; });
  }

  // Linked table rows (`.data-table__row--linked`) style as clickable; make
  // the whole row actually navigate via its row-href anchor, unless the
  // click landed on an interactive element of its own.
  document.addEventListener('click', (e) => {
    const row = e.target.closest('.data-table__row--linked');
    if (!row || e.target.closest('a, button, input, select, label, textarea')) return;
    const anchor = row.querySelector('.data-table__row-href a');
    if (anchor) anchor.click();
  });

  // Item click → navigate
  list.addEventListener('click', (e) => {
    const item = e.target.closest('.palette__item');
    if (!item?.dataset.href) return;
    if (item.dataset.external === 'true') {
      window.open(item.dataset.href, '_blank', 'noopener,noreferrer');
    } else {
      window.location.assign(item.dataset.href);
    }
  });

  input.addEventListener('input', (e) => apply(e.target.value));

  // Keyboard scrolling for the app shell. The document never scrolls (the
  // .shell grid is 100vh; .shell__body is the real scroller), so with no
  // focused element PageDown/PageUp/Space/Home/End/arrows would silently do
  // nothing. Registered after the palette handler above, so an open palette
  // (which preventDefaults its own keys) wins. Only fires when the event
  // target is the page itself — typing in fields and focused widgets keep
  // their native behavior.
  document.addEventListener('keydown', (e) => {
    if (e.defaultPrevented || e.metaKey || e.ctrlKey || e.altKey) return;
    if (e.target !== document.body && e.target !== document.documentElement) return;
    const scroller = document.querySelector('.shell__body');
    if (!scroller) return;
    const pageStep = scroller.clientHeight * 0.9;
    const lineStep = 40;
    let dy;
    switch (e.key) {
      case 'PageDown': dy = pageStep; break;
      case 'PageUp': dy = -pageStep; break;
      case ' ': dy = e.shiftKey ? -pageStep : pageStep; break;
      case 'ArrowDown': dy = lineStep; break;
      case 'ArrowUp': dy = -lineStep; break;
      case 'Home': scroller.scrollTo({ top: 0 }); e.preventDefault(); return;
      case 'End': scroller.scrollTo({ top: scroller.scrollHeight }); e.preventDefault(); return;
      default: return;
    }
    scroller.scrollBy({ top: dy });
    e.preventDefault();
  });
})();
"#
}

/// Small inline JS for modal close (Escape key + overlay click).
pub fn modal_js() -> &'static str {
    r#"
document.addEventListener("keydown", function(e) {
    if (e.key === "Escape") {
        var m = document.querySelector('.modal-overlay:not([hidden])');
        if (m) m.setAttribute("hidden", "");
    }
});
function openModal(id) {
    var m = document.getElementById(id);
    if (m) m.removeAttribute("hidden");
}
function closeModal(id) {
    var m = document.getElementById(id);
    if (m) m.setAttribute("hidden", "");
}
document.body.addEventListener("closeModal", function(e) {
    var d = e.detail || {};
    if (d.id) closeModal(d.id);
});
"#
}

/// Vanilla JS for the mobile sidebar drawer. Toggles `body[data-drawer-open]`
/// from clicks on `[data-action="drawer-open"]` (the hamburger), the overlay
/// (`[data-action="drawer-close"]`), Escape, or any sidebar nav-link click
/// (so navigation auto-collapses the drawer).
pub fn drawer_js() -> &'static str {
    r#"
(function () {
  if (window.__drawerInit) return;
  window.__drawerInit = true;
  var body = document.body;
  function open() { body.setAttribute('data-drawer-open', 'true'); }
  function close() { body.removeAttribute('data-drawer-open'); }
  document.addEventListener('click', function (e) {
    var t = e.target;
    if (!(t instanceof Element)) return;
    var actEl = t.closest('[data-action]');
    var action = actEl ? actEl.getAttribute('data-action') : null;
    if (action === 'drawer-open') { open(); e.preventDefault(); return; }
    if (action === 'drawer-close') { close(); e.preventDefault(); return; }
    if (body.hasAttribute('data-drawer-open') && t.closest('.sidebar a')) {
      close();
    }
  });
  document.addEventListener('keydown', function (e) {
    if (e.key === 'Escape' && body.hasAttribute('data-drawer-open')) {
      close();
    }
  });
})();
"#
}

#[cfg(test)]
mod tests {
    #[test]
    fn toast_messages_are_rendered_as_text_not_html() {
        let js = super::toast_js();
        assert!(
            !js.contains("innerHTML"),
            "toast content must not use an HTML sink"
        );
        assert!(js.contains("message.textContent"));
        assert!(js.contains("createElement(\"button\")"));
        assert!(js.contains("addEventListener(\"click\""));
    }

    #[test]
    fn brand_accent_matches_tokens_css() {
        // BRAND_ACCENT_HEX exists for CSS-var-less surfaces (emails). It must
        // stay byte-identical to the stylesheet's --primary-color default.
        assert!(
            super::css().contains(&format!("--primary-color: {}", super::BRAND_ACCENT_HEX)),
            "BRAND_ACCENT_HEX ({}) does not match --primary-color in tokens.css",
            super::BRAND_ACCENT_HEX
        );
    }

    #[test]
    fn favicon_url_has_content_hash() {
        let url = super::favicon_url();
        assert!(url.starts_with("/b/static/favicon-"));
        assert!(url.ends_with(".ico"));
        let hash = url
            .trim_start_matches("/b/static/favicon-")
            .trim_end_matches(".ico");
        assert_eq!(hash.len(), 8, "expected 8-char short hash, got: {hash}");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn tokens_include_new_scale() {
        let s = super::css();
        for tok in [
            "--text-base",
            "--text-2xl",
            "--space-2xl",
            "--surface-1",
            "--primary-button",
            "--focus-ring",
        ] {
            assert!(s.contains(tok), "missing token: {tok}");
        }
    }

    /// Relative luminance per WCAG 2.1.
    fn luminance(hex: &str) -> f64 {
        let h = hex.trim_start_matches('#');
        let ch = |i| {
            let c = u8::from_str_radix(&h[i..i + 2], 16).unwrap() as f64 / 255.0;
            if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * ch(0) + 0.7152 * ch(2) + 0.0722 * ch(4)
    }

    fn contrast(a: &str, b: &str) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        let (hi, lo) = if x > y { (x, y) } else { (y, x) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn brand_tokens_meet_wcag_aa() {
        // White-on-red button surfaces carry normal-size text: 4.5:1 required.
        assert!(contrast("#d92320", "#ffffff") >= 4.5,
            "primary-button fails AA: {}", contrast("#d92320", "#ffffff"));
        // Sidebar foregrounds on navy.
        assert!(contrast("#ffffff", "#0a1122") >= 4.5);
        assert!(contrast("#94a3b8", "#0a1122") >= 4.5,
            "sidebar muted text fails AA");
        // The regression this guards: --text-muted is 3.95:1 on navy and must
        // never be reused as a sidebar foreground.
        assert!(contrast("#64748b", "#0a1122") < 4.5,
            "if this now passes, the sidebar-specific token may be redundant");
    }

    #[test]
    fn tokens_css_declares_the_new_palette() {
        let s = super::css();
        for (name, value) in [
            ("--primary-color", "#fd3534"),
            ("--primary-button", "#d92320"),
            ("--primary-hover", "#e02523"),
            ("--navy-900", "#02112a"),
            ("--navy-800", "#0a1122"),
            ("--navy-700", "#172136"),
            ("--sidebar-text-muted", "#94a3b8"),
        ] {
            assert!(s.contains(&format!("{name}: {value}")), "missing {name}: {value}");
        }
    }

    #[test]
    fn ui_font_stack_is_system_and_itim_is_wordmark_only() {
        let s = super::css();
        assert!(s.contains("--font-ui: system-ui"), "UI face must be the system stack");
        assert!(s.contains(".brand__wordmark"), "Itim must be scoped to the wordmark");
        // The body must not name Itim any more.
        let body_rule = s.split("body {").nth(1).expect("body rule").split('}').next().unwrap();
        assert!(!body_rule.contains("Itim"), "Itim must not be the body face");
    }

    /// Strips every `/* ... */` block comment from `s`. Shared by
    /// `css_leaf_blocks` and `parse_root_tokens` below: a comment sitting
    /// between two declarations (nothing but the comment separates it from
    /// the declaration after it -- no `;`/`{`/`}` to split on) otherwise
    /// gets swallowed into that following declaration's segment when a
    /// caller splits on `;`, silently hiding the declaration. (Found
    /// auditing `.login-button` for Task 15: its `background:
    /// var(--primary-button)` sat right after such a comment and was
    /// invisible to `body.split(';').find(|d| d.starts_with("background:"))`
    /// as a result -- not a bug in that rule, a latent bug in this shared
    /// parsing step.) Also needed because a comment containing literal
    /// `{`/`}` characters in prose (several exist in this bundle, e.g.
    /// base.css's "`hidden` attribute" comment) would otherwise corrupt
    /// `css_leaf_blocks`'s brace-depth scan.
    fn strip_css_comments(s: &str) -> String {
        let mut without_comments = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(start) = rest.find("/*") {
            without_comments.push_str(&rest[..start]);
            rest = match rest[start + 2..].find("*/") {
                Some(end) => &rest[start + 2 + end + 2..],
                None => "",
            };
        }
        without_comments.push_str(rest);
        without_comments
    }

    /// Finds every leaf declaration block (`selector { decl; decl; }`) in a
    /// CSS bundle, including ones nested inside `@media` (comments stripped
    /// first via `strip_css_comments` above). A block whose body still
    /// contains `{` after being popped off the brace stack is a container
    /// (e.g. the `@media` wrapper itself) and is skipped -- its children
    /// are captured on their own pop.
    fn css_leaf_blocks(s: &str) -> Vec<(String, String)> {
        let without_comments = strip_css_comments(s);
        let s = without_comments.as_str();

        let chars: Vec<char> = s.chars().collect();
        let mut stack: Vec<usize> = Vec::new();
        let mut blocks = Vec::new();
        for (i, c) in chars.iter().enumerate() {
            match c {
                '{' => stack.push(i),
                '}' => {
                    let Some(open) = stack.pop() else { continue };
                    let body: String = chars[open + 1..i].iter().collect();
                    if body.contains('{') {
                        continue; // container, not a leaf -- e.g. @media
                    }
                    let prefix: String = chars[..open].iter().collect();
                    let selector = prefix
                        .rsplit(|c| c == '{' || c == '}')
                        .next()
                        .unwrap_or(&prefix)
                        .trim();
                    blocks.push((selector.to_string(), body));
                }
                _ => {}
            }
        }
        blocks
    }

    // ===== Value-based contrast resolution (Task 15 follow-up) =====
    //
    // Two guards touched this exact bug and each missed it from a
    // different angle: `brand_tokens_meet_wcag_aa` asserted token VALUES in
    // isolation, never checking which rules actually paired them (missed
    // every primary button failing AA, Task 7). Its replacement here
    // originally matched `background: var(--primary-color)` + `color:
    // white` (Task 12a), then grew a second assertion for the reverse
    // `color: var(--primary-color)` direction (Task 15) -- but both worked
    // by matching a token's literal `var(--name)` spelling in the CSS
    // source text. That is structurally a blocklist: `--accent-info:
    // #fd3534` was a byte-identical alias of `--primary-color` under a
    // different name, and evaded it completely, as did every bare hex
    // literal (`#ef4444` in `.form-error`) equal to a tracked token's
    // value. The next alias would have evaded it again.
    //
    // The functions below resolve a CSS color expression -- a token
    // reference (`var(--x)`), a `var(--x, fallback)` with its fallback, a
    // `color-mix(in srgb, c1 p1%, c2 p2%)`, or a literal hex/`white`/
    // `black`/`transparent` -- to actual RGBA by walking `tokens.css`'s
    // live `:root` values, so the single test below computes real WCAG
    // contrast instead of matching names.
    type Rgba = (u8, u8, u8, u8);

    /// Parses the assembled bundle's `:root { ... }` custom-property
    /// declarations into a `name -> raw value` map, e.g. `"--primary-color"
    /// -> "#fd3534"`. `styles/tokens.css` is first in build.rs's
    /// `CSS_FILES` order and the only file with a `:root` block.
    fn parse_root_tokens(s: &str) -> std::collections::HashMap<String, String> {
        let without_comments = strip_css_comments(s);
        let s = without_comments.as_str();
        let start = s.find(":root").expect(":root block missing from bundle");
        let open = s[start..].find('{').expect(":root has no body") + start;
        let mut depth = 0i32;
        let mut end = open;
        for (i, c) in s[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let body = &s[open + 1..end];
        let mut map = std::collections::HashMap::new();
        for decl in body.split(';') {
            let decl = decl.trim();
            if let Some(rest) = decl.strip_prefix("--") {
                if let Some((name, val)) = rest.split_once(':') {
                    map.insert(format!("--{}", name.trim()), val.trim().to_string());
                }
            }
        }
        map
    }

    /// Splits a function-argument string on top-level commas (i.e. not
    /// inside a nested `(...)`) -- needed because both a `var(--x,
    /// var(--y))` fallback and a `color-mix(in srgb, c1 p1%, c2 p2%)`
    /// argument list can contain commas one level deeper than the ones
    /// that actually separate arguments.
    fn split_top_level(s: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        let mut depth = 0i32;
        let mut start = 0usize;
        for (i, c) in s.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 0 => {
                    parts.push(s[start..i].trim());
                    start = i + 1;
                }
                _ => {}
            }
        }
        parts.push(s[start..].trim());
        parts
    }

    fn hex_byte(s: &str) -> Option<u8> {
        u8::from_str_radix(s, 16).ok()
    }

    /// Parses a `#rgb`/`#rgba`/`#rrggbb`/`#rrggbbaa` literal.
    fn parse_hex(h: &str) -> Option<Rgba> {
        let h = h.trim_start_matches('#');
        let double = |c: char| -> Option<u8> { hex_byte(&format!("{c}{c}")) };
        match h.len() {
            3 => {
                let mut cs = h.chars();
                Some((double(cs.next()?)?, double(cs.next()?)?, double(cs.next()?)?, 255))
            }
            4 => {
                let mut cs = h.chars();
                Some((
                    double(cs.next()?)?,
                    double(cs.next()?)?,
                    double(cs.next()?)?,
                    double(cs.next()?)?,
                ))
            }
            6 => Some((hex_byte(&h[0..2])?, hex_byte(&h[2..4])?, hex_byte(&h[4..6])?, 255)),
            8 => Some((
                hex_byte(&h[0..2])?,
                hex_byte(&h[2..4])?,
                hex_byte(&h[4..6])?,
                hex_byte(&h[6..8])?,
            )),
            _ => None,
        }
    }

    /// Resolves a CSS color expression to RGBA, following `var()` chains
    /// (including a `var(--x, fallback)`'s fallback when `--x` isn't
    /// declared) and `color-mix(in srgb, c1 [p1%], c2 [p2%])`. Returns
    /// `None` for anything else this test doesn't need to understand --
    /// `currentColor`, `inherit`, gradients, `rgb()`/`rgba()` (none of
    /// which appear on a `color:`/`background:` declaration anywhere in
    /// this bundle today, checked by grep while writing this) -- callers
    /// treat `None` as "can't verify this rule" and skip it rather than
    /// assuming compliance.
    fn resolve_color(value: &str, tokens: &std::collections::HashMap<String, String>, depth: u8) -> Option<Rgba> {
        if depth > 12 {
            return None;
        }
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        let lower = value.to_ascii_lowercase();
        match lower.as_str() {
            "white" => return Some((255, 255, 255, 255)),
            "black" => return Some((0, 0, 0, 255)),
            "transparent" => return Some((0, 0, 0, 0)),
            _ => {}
        }
        if let Some(hex) = value.strip_prefix('#') {
            return parse_hex(hex);
        }
        if lower.starts_with("var(") {
            let open = value.find('(')?;
            let close = value.rfind(')')?;
            let parts = split_top_level(&value[open + 1..close]);
            let name = parts.first()?.trim();
            if let Some(v) = tokens.get(name) {
                return resolve_color(v, tokens, depth + 1);
            }
            if parts.len() > 1 {
                return resolve_color(parts[1], tokens, depth + 1);
            }
            return None;
        }
        if lower.starts_with("color-mix(") {
            let open = value.find('(')?;
            let close = value.rfind(')')?;
            let parts = split_top_level(&value[open + 1..close]);
            if parts.len() < 3 {
                return None;
            }
            let split_pct = |p: &str| -> (String, Option<f64>) {
                match p.rsplit_once(' ') {
                    Some((color, pct)) if pct.ends_with('%') => {
                        match pct.trim_end_matches('%').parse::<f64>() {
                            Ok(v) => (color.trim().to_string(), Some(v)),
                            Err(_) => (p.to_string(), None),
                        }
                    }
                    _ => (p.to_string(), None),
                }
            };
            let (c1, p1) = split_pct(parts[1]);
            let (c2, p2) = split_pct(parts[2]);
            let (w1, w2) = match (p1, p2) {
                (None, None) => (50.0, 50.0),
                (Some(a), None) => (a, 100.0 - a),
                (None, Some(b)) => (100.0 - b, b),
                (Some(a), Some(b)) => (a, b),
            };
            let rgba1 = resolve_color(&c1, tokens, depth + 1)?;
            let rgba2 = resolve_color(&c2, tokens, depth + 1)?;
            let mix = |a: u8, b: u8| -> u8 { ((a as f64 * w1 / 100.0) + (b as f64 * w2 / 100.0)).round() as u8 };
            return Some((
                mix(rgba1.0, rgba2.0),
                mix(rgba1.1, rgba2.1),
                mix(rgba1.2, rgba2.2),
                mix(rgba1.3, rgba2.3),
            ));
        }
        None
    }

    /// Alpha-composites `fg` over an opaque `base` -- e.g. a translucent
    /// tint like the old `--accent-info-bg`'s `#fd353419` over the page's
    /// white surface.
    fn composite_over(fg: Rgba, base: (u8, u8, u8)) -> (u8, u8, u8) {
        let (r, g, b, a) = fg;
        if a == 255 {
            return (r, g, b);
        }
        if a == 0 {
            return base;
        }
        let af = a as f64 / 255.0;
        let mix = |f: u8, b: u8| -> u8 { (f as f64 * af + b as f64 * (1.0 - af)).round() as u8 };
        (mix(r, base.0), mix(g, base.1), mix(b, base.2))
    }

    fn hex_of(rgb: (u8, u8, u8)) -> String {
        format!("#{:02x}{:02x}{:02x}", rgb.0, rgb.1, rgb.2)
    }

    /// Selectors where a `color:` resolving into the primary/danger family
    /// (see the test below) is legitimate non-text, or text-adjacent but
    /// not itself text -- each with its own reason, not a silent pass.
    const PRIMARY_DANGER_FAMILY_EXEMPT_SELECTORS: &[&str] = &[
        // `.db-table-group__icon` wraps `icons::package()`/`icons::database()`
        // (database.rs) -- an SVG icon, not text; `color` only feeds the
        // SVG's `currentColor`. WCAG's 3:1 non-text floor applies, and
        // #fd3534 (3.66:1 on white) already clears it.
        ".db-table-group__icon",
        // `.form-label.required::after`'s generated content (' *') ornaments
        // the visible label text right next to it (e.g. "Email *") rather
        // than substituting for it -- the label itself already conveys the
        // field name, so this doesn't fall under SC 1.4.3 as text. Left at
        // the literal #ef4444 (== --accent-danger's value) rather than
        // darkened.
        ".form-label.required::after",
    ];

    /// Supersedes the two narrower, name-matching contrast guards this test
    /// module used to carry (a `background: var(--primary-color)` +
    /// `color: white` check from Task 12a, and a `color:
    /// var(--primary-color)`/`var(--accent-danger)` check added alongside
    /// it in Task 15) with one value-based assertion covering both
    /// directions at once. Matching by resolved value rather than by a
    /// token's literal name/spelling is what closes the alias hole that
    /// let `--accent-info: #fd3534` -- and the bare literal `#ef4444` in
    /// `.form-error`/`.form-label.required::after` -- evade both
    /// predecessors: a new token or a re-literalized hex sharing one of
    /// these five tokens' current value cannot rename its way past this.
    ///
    /// Scope: this only evaluates a rule whose resolved TEXT color, or
    /// whose own resolved BACKGROUND color (declared in the SAME rule --
    /// this is still a single-rule scan, see below), equals the CURRENT
    /// resolved value of one of `PRIMARY_DANGER_FAMILY_TOKENS` -- i.e. this
    /// project's brand-red/danger-red design family, recomputed from the
    /// live token values every run rather than a hex list frozen into the
    /// test. A rule outside that family (green success text, amber warning
    /// text, disabled/placeholder gray, the navy sidebar/auth-split brand
    /// panel's own white-on-navy palette, ...) is never evaluated here --
    /// those are real, separate contrast questions this guard does not
    /// answer. It was scoped this way deliberately: the fully general
    /// version (assert on every resolvable `color:` in the bundle,
    /// defaulting to a white background when none is declared) was built
    /// and run first, and surfaced ~30 additional offenders -- almost all
    /// either genuine pre-existing gaps in that unrelated success/warning/
    /// gray palette (a separate, larger, unbudgeted audit), or false
    /// positives from elements that render on a dark ancestor background
    /// (`.sidebar__nav-item`, `.auth-split__logo-name`, ...) this
    /// single-rule scan has no way to see -- which the family-scoping
    /// above happens to filter out for free, since white and
    /// `--sidebar-text-muted` aren't primary/danger-family values either.
    /// Both lists (the unrelated pre-existing gaps, and confirmation of
    /// which "false positives" were checked against their true ancestor
    /// background rather than just assumed) are in the Task 15 report.
    ///
    /// What this still can't see, even inside its declared scope: a
    /// background and text color declared in two DIFFERENT rules that
    /// combine at runtime (parent background + child text color); colors
    /// applied via inline `style`/JS; a family value expressed through a
    /// CSS function this resolver doesn't parse (`rgb()`/`rgba()`,
    /// gradients -- none currently appear on a `color:`/`background:` in
    /// this bundle, checked by grep) -- such a rule is skipped rather than
    /// assumed compliant, not silently passed.
    #[test]
    fn text_or_background_in_primary_danger_family_meets_wcag_aa() {
        const PRIMARY_DANGER_FAMILY_TOKENS: &[&str] = &[
            "--primary-color",
            "--primary-hover",
            "--primary-button",
            "--accent-danger",
            "--accent-danger-text",
        ];

        let s = super::css();
        let tokens = parse_root_tokens(s);
        let family_values: std::collections::HashSet<Rgba> = PRIMARY_DANGER_FAMILY_TOKENS
            .iter()
            .filter_map(|name| tokens.get(*name))
            .filter_map(|v| resolve_color(v, &tokens, 0))
            .collect();
        assert_eq!(
            family_values.len(),
            PRIMARY_DANGER_FAMILY_TOKENS.len(),
            "expected all {} family tokens to resolve to distinct values -- \
             if this fails, either the token map/parser broke, or two family \
             tokens now share a value (not necessarily wrong, but re-check \
             this test's scoping assumption if so)",
            PRIMARY_DANGER_FAMILY_TOKENS.len()
        );

        let mut offenders: Vec<String> = Vec::new();
        for (selector, body) in css_leaf_blocks(s) {
            if PRIMARY_DANGER_FAMILY_EXEMPT_SELECTORS.contains(&selector.as_str()) {
                continue;
            }
            let decls: Vec<&str> = body.split(';').map(str::trim).filter(|d| !d.is_empty()).collect();
            let Some(color_decl) = decls.iter().find(|d| d.starts_with("color:")).copied() else {
                continue;
            };
            let Some(text_rgba) = resolve_color(&color_decl["color:".len()..], &tokens, 0) else {
                continue; // unresolvable -- can't verify, skip (documented above)
            };

            let bg_decl: Option<&str> = decls
                .iter()
                .find(|d| d.starts_with("background:") || d.starts_with("background-color:"))
                .copied();
            let bg_rgba = match bg_decl {
                None => (255u8, 255u8, 255u8, 255u8), // no local background -> page surface (--surface-1)
                Some(d) => {
                    let val = d.split_once(':').map(|x| x.1).unwrap_or_default();
                    match resolve_color(val, &tokens, 0) {
                        Some(c) => c,
                        None => continue, // background present but unresolvable -- can't verify, skip
                    }
                }
            };

            if text_rgba.3 == 0 {
                continue; // fully transparent text, not visible
            }
            if !family_values.contains(&text_rgba) && !family_values.contains(&bg_rgba) {
                continue; // neither side is this guard's declared family -- out of scope
            }

            let bg_rgb = composite_over(bg_rgba, (255, 255, 255));
            let text_rgb = composite_over(text_rgba, bg_rgb);
            let ratio = contrast(&hex_of(text_rgb), &hex_of(bg_rgb));
            if ratio < 4.5 {
                offenders.push(format!(
                    "{selector}: {ratio:.2}:1 ({} text on {} background)",
                    hex_of(text_rgb),
                    hex_of(bg_rgb)
                ));
            }
        }

        assert!(
            offenders.is_empty(),
            "primary/danger-family text/background pairs failing 4.5:1 AA (computed): {offenders:?}"
        );
    }

    #[test]
    fn palette_js_present_and_self_invoking() {
        let js = super::palette_js();
        assert!(js.contains("cmdk"));
        assert!(js.contains("Meta+K") || js.contains("metaKey"));
        assert!(js.starts_with("\n(function") || js.contains("(function "));
    }

    #[test]
    fn drawer_js_handles_open_close_esc_and_navlink() {
        let js = super::drawer_js();
        assert!(js.contains("'drawer-open'"));
        assert!(js.contains("'drawer-close'"));
        assert!(js.contains("'Escape'"));
        assert!(js.contains(".sidebar a"));
        assert!(js.contains("data-drawer-open"));
        // Self-invoking + idempotent guard.
        assert!(js.contains("__drawerInit"));
    }

    #[test]
    #[cfg(all(feature = "block-llm", feature = "embed-assets"))]
    fn llm_chat_js_is_self_invoking_and_exposes_init() {
        let js = std::str::from_utf8(super::bytes("llm-chat.js").expect("llm-chat.js embedded"))
            .unwrap();
        assert!(js.contains("(function ()") || js.contains("(function()"));
        assert!(js.contains("__impresspressLlmChatLoaded"));
        assert!(js.contains("window.impresspressLlmChat = { init: init }"));
        for sym in [
            "handleChatSubmit",
            "createNewThread",
            "selectThread",
            "onModelChange",
            "unloadLocalModel",
        ] {
            assert!(
                js.contains(&format!("window.{sym} = {sym}")),
                "missing global re-export for {sym}"
            );
        }
    }

    #[test]
    #[cfg(feature = "block-llm")]
    fn purify_js_url_has_content_hash() {
        let url = super::purify_js_url();
        assert!(url.starts_with("/b/static/purify-"));
        // Source file is `purify.min.js`; the manifest's `hashed_name` splits
        // on the *first* dot, so the hashed filename keeps the full
        // `.min.js` extension (same shape as `marked-{hash}.min.js`) rather
        // than the pre-manifest ad-hoc `purify-{hash}.js`.
        assert!(url.ends_with(".min.js"));
        let hash = url
            .trim_start_matches("/b/static/purify-")
            .trim_end_matches(".min.js");
        assert_eq!(hash.len(), 8, "expected 8-char short hash, got: {hash}");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    #[cfg(all(feature = "block-llm", feature = "embed-assets"))]
    fn purify_js_is_dompurify_umd_build() {
        let js =
            std::str::from_utf8(super::bytes("purify.min.js").expect("purify.min.js embedded"))
                .unwrap();
        assert!(
            js.contains("DOMPurify"),
            "vendored asset should be DOMPurify"
        );
        // UMD build: `(e=...globalThis...||self).DOMPurify=t()` — assigns
        // onto the global object (`window` in a browser) when there's no
        // CommonJS/AMD module system, which is the load path llm-chat.js
        // relies on for the bare `DOMPurify` global.
        assert!(
            js.contains(").DOMPurify=t()"),
            "expected UMD build to assign a global .DOMPurify"
        );
    }

    #[test]
    #[cfg(all(feature = "block-files", feature = "embed-assets"))]
    fn files_browser_js_exposes_init_and_handles_drag_drop() {
        let js = std::str::from_utf8(
            super::bytes("files-browser.js").expect("files-browser.js embedded"),
        )
        .unwrap();
        assert!(
            js.contains("impresspressFilesBrowser"),
            "module namespace missing"
        );
        assert!(js.contains("dragenter"), "drag handler missing");
        assert!(js.contains("dragover"), "drag handler missing");
        assert!(
            js.contains("'drop'") || js.contains("\"drop\""),
            "drop handler missing"
        );
        assert!(js.contains("data-bulk-toggle"), "bulk-select missing");
        assert!(js.contains("data-action-menu"), "kebab handler missing");
        assert!(js.contains("dialog"), "modal uses <dialog>");
    }

    #[test]
    #[cfg(feature = "block-files")]
    fn files_browser_js_url_has_content_hash() {
        let url = super::files_browser_js_url();
        assert!(url.starts_with("/b/static/files-browser-"));
        assert!(url.ends_with(".js"));
        let hash = url
            .trim_start_matches("/b/static/files-browser-")
            .trim_end_matches(".js");
        assert_eq!(hash.len(), 8);
    }

    #[test]
    #[cfg(feature = "block-llm")]
    fn llm_chat_js_url_has_content_hash() {
        let url = super::llm_chat_js_url();
        assert!(url.starts_with("/b/static/llm-chat-"));
        assert!(url.ends_with(".js"));
        assert!(
            !url.ends_with(".min.js"),
            "we deliberately ship un-minified"
        );
        let mid = url
            .trim_start_matches("/b/static/llm-chat-")
            .trim_end_matches(".js");
        assert_eq!(mid.len(), 8, "expected 8-char short hash, got: {mid}");
        assert!(mid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn manifest_covers_core_assets_with_hashed_filenames() {
        let by_logical = |k: &str| super::ASSETS.iter().find(|e| e.logical == k);
        for key in ["app.css", "htmx.min.js", "favicon.ico", "itim-latin.woff2"] {
            let e = by_logical(key).unwrap_or_else(|| panic!("manifest missing {key}"));
            assert!(e.len > 0, "{key} has zero length");
            // `app.css` -> `app-a1b2c3d4.css`: stem, dash, 8 hex chars, extension.
            let stem = key.split_once('.').unwrap().0;
            let rest = e.filename.strip_prefix(stem).expect("filename keeps its stem");
            let hash = &rest[1..9];
            assert_eq!(hash.len(), 8);
            assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "{key}: {hash} not hex");
        }
    }

    #[test]
    fn css_bundle_includes_all_layers_in_order() {
        let s = super::css();
        // Tokens must precede every consumer so custom properties are defined
        // before use; layouts come last so they can override component defaults.
        // This guard is what keeps Task 5's split honest — it must pass both
        // before and after the files are reorganised.
        let tokens = s.find("--primary-color").expect("tokens layer missing");
        let button = s.find(".btn").expect("button layer missing");
        let shell = s.find(".shell").expect("shell layout missing");
        assert!(tokens < button, "tokens must precede components");
        assert!(button < shell, "components must precede layouts");
        for marker in [".card", ".data-table", ".badge", ".modal", ".toast",
                       ".palette", ".stat-", ".charts-css", ".auth-split"] {
            assert!(s.contains(marker), "missing layer marker: {marker}");
        }
    }

    #[test]
    fn base_url_defaults_to_static_prefix() {
        // No IMPRESSPRESS_ASSET_BASE_URL in the test environment.
        assert_eq!(super::base_url(), "/b/static/");
    }

    #[test]
    fn set_base_url_override_is_idempotent() {
        // Exercises `BASE_URL_OVERRIDE` in isolation — never calls
        // `base_url()` itself, whose own `OnceLock` is claimed by
        // `base_url_defaults_to_static_prefix` and must not be touched by
        // any other test in this process (tests share one process and
        // `base_url()`'s result is cached forever after its first call).
        super::set_base_url_override(Some("https://first.example/".to_string()));
        super::set_base_url_override(Some("https://second.example/".to_string()));
        assert_eq!(
            super::BASE_URL_OVERRIDE.get().cloned().flatten(),
            Some("https://first.example/".to_string()),
            "a later call must not clobber the first-set override"
        );
    }

    #[test]
    fn url_joins_base_and_hashed_filename() {
        let u = super::url("app.css");
        assert!(u.starts_with("/b/static/app-"), "unexpected url: {u}");
        assert!(u.ends_with(".css"), "unexpected url: {u}");
    }

    #[test]
    fn cdn_base_template_is_versioned_and_slash_terminated() {
        let t = super::DEFAULT_CDN_BASE_TEMPLATE;
        assert!(t.starts_with("https://cdn.impresspress.org/ui/v"));
        assert!(t.ends_with('/'), "base must end in / so joins are plain concatenation");
        assert!(t.contains(env!("CARGO_PKG_VERSION")), "base must pin the crate version");
    }

    #[cfg(feature = "embed-assets")]
    #[test]
    fn embedded_bytes_agree_with_the_manifest() {
        // The manifest always lists all 11 assets (build.rs panics on a missing
        // file), but `bytes()` is cfg-gated per asset — under a lean build
        // `marked.min.js` and friends are simply not compiled in. So: every
        // asset that IS compiled in must match its manifest length...
        for e in super::ASSETS {
            if let Some(b) = super::bytes(e.logical) {
                assert_eq!(b.len(), e.len, "{} length disagrees with manifest", e.logical);
            }
        }
        // ...and the assets that are never feature-gated must always be there.
        for logical in ["app.css", "htmx.min.js", "webmcp.js", "favicon.ico",
                        "itim-latin.woff2", "itim-latin-ext.woff2",
                        "impresspress-logo.png", "impresspress-logo-long.png"] {
            assert!(super::bytes(logical).is_some(), "core asset missing: {logical}");
        }
    }

    #[test]
    fn manifest_font_names_appear_in_css_bundle_as_relative_urls() {
        let css = super::css();
        for key in ["itim-latin.woff2", "itim-latin-ext.woff2"] {
            let e = super::ASSETS.iter().find(|e| e.logical == key).unwrap();
            assert!(css.contains(&format!("url('{}')", e.filename)), "{key} url not rewritten");
        }
        assert!(!css.contains("__ITIM_LATIN_URL__"), "placeholder survived");
        assert!(!css.contains("/b/static/"), "font url must be relative, not absolute");
    }
}
