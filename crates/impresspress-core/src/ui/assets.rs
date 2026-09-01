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

/// Resolve the asset base URL once.
///
/// 1. `IMPRESSPRESS_ASSET_BASE_URL` (infrastructure config — `IMPRESSPRESS_*`,
///    no `__`, never stored in the DB) wins outright. The deployer sets this
///    when assets live somewhere other than this origin.
/// 2. Otherwise `/b/static/`, served by the system block — from memory when
///    `embed-assets` is on, streamed from R2 when it is off.
///
/// Whether R2 is configured is deploy-time knowledge, so the CLI writes that
/// decision into the env var rather than the runtime sniffing for a backend.
pub fn base_url() -> &'static str {
    static BASE: OnceLock<String> = OnceLock::new();
    BASE.get_or_init(|| match std::env::var("IMPRESSPRESS_ASSET_BASE_URL") {
        Ok(v) if !v.trim().is_empty() => {
            let v = v.trim().to_string();
            if v.ends_with('/') { v } else { format!("{v}/") }
        }
        _ => STATIC_PREFIX.to_string(),
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
pub const BRAND_ACCENT_HEX: &str = "#f0480f";

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
