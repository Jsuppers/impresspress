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
        // which 404s offline). Real pixel art -- 32x32 art-pixels, generated
        // by the `site` repo's `npm run images` (`brand/logo-32.png`).
        // Templates only ever scale it by whole factors (`.pixel-art`), so
        // its native size is part of the contract (see the tests below).
        "impresspress-logo.png" => include_bytes!("assets/impresspress-logo.png"),
        // The same mark at 64x64 art-pixels (`brand/logo-64.png`) -- served
        // as the `2x` `srcset` candidate so high-DPI screens get one device
        // pixel per art-pixel instead of a nearest-neighbour blow-up of the
        // 32-cell mark. There is no raster wordmark: brand text is rendered
        // as text next to the mark (see `templates::brand_lockup`) -- the
        // old `impresspress-logo-long.png` wordmark was dark-ink artwork
        // illegible on the navy chrome and has been removed outright (see
        // `config_vars::REMOVED_BUILTIN_WORDMARK_URL_PREFIX`).
        "impresspress-logo-2x.png" => include_bytes!("assets/impresspress-logo-2x.png"),
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
///
/// Kept at this branch's own value (not origin/main's `#f0480f`) — this is
/// the redesign's chosen accent, already wired through `tokens.css` and the
/// value-based contrast guard (`text_or_background_in_primary_danger_family_
/// meets_wcag_aa`); main's value was mid-fix on a documented WCAG-AA
/// shortfall this branch already resolved differently.
pub const BRAND_ACCENT_HEX: &str = "#fd3534";

/// Square logo URL with content hash, e.g. `/b/static/impresspress-logo-a1b2c3d4.png`.
pub fn logo_icon_url() -> String {
    url("impresspress-logo.png")
}

/// 2x square logo URL with content hash, e.g. `/b/static/impresspress-logo-2x-a1b2c3d4.png`.
/// Retina candidate for [`templates::brand_icon`]'s `<picture>` `srcset` —
/// one device pixel per art-pixel instead of a nearest-neighbour blow-up of
/// the 32-cell mark.
pub fn logo_icon_2x_url() -> String {
    url("impresspress-logo-2x.png")
}

/// Favicon URL with content hash, e.g. `/b/static/favicon-a1b2c3d4.ico`.
pub fn favicon_url() -> String {
    url("favicon.ico")
}

/// The assembled CSS bundle, built by `build.rs` from `CSS_ORDER`.
///
/// Gated on `embed-assets` like [`bytes`]: without it, this `include_str!`
/// would still compile the ~75 KB bundle into every binary that links this
/// crate (including a lean Cloudflare Worker), relying on the linker to
/// notice nothing calls it and garbage-collect it -- "very likely" GC'd is
/// exactly the gap a `#[cfg]` closes outright. See `bytes`'s doc for why
/// `embed-assets` is the feature that governs asset bytes at all.
#[cfg(feature = "embed-assets")]
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

    #[cfg(feature = "embed-assets")]
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

    /// PNG width/height from the IHDR chunk (big-endian u32 at bytes 16/20).
    #[cfg(feature = "embed-assets")]
    fn png_size(png: &[u8]) -> (u32, u32) {
        assert_eq!(&png[1..4], b"PNG", "not a PNG");
        let be = |i: usize| u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]);
        (be(16), be(20))
    }

    // The brand art is real pixel art (generated in the `site` repo's
    // `brand/` kit) and the templates scale it only by whole factors, so
    // the native sizes are part of the contract. Ported from origin/main
    // during the main merge -- 2026-09-02 -- adapted to call `bytes()`
    // (the manifest's single embed point) rather than main's dedicated
    // per-asset accessors, which this branch's detachable-assets refactor
    // superseded.
    #[cfg(feature = "embed-assets")]
    #[test]
    fn logo_icon_png_is_the_32_cell_mark() {
        assert_eq!(
            png_size(super::bytes("impresspress-logo.png").unwrap()),
            (32, 32)
        );
    }

    #[cfg(feature = "embed-assets")]
    #[test]
    fn logo_icon_2x_png_is_the_64_cell_mark() {
        assert_eq!(
            png_size(super::bytes("impresspress-logo-2x.png").unwrap()),
            (64, 64)
        );
    }

    #[test]
    fn logo_icon_2x_url_has_content_hash() {
        let url = super::logo_icon_2x_url();
        assert!(url.starts_with("/b/static/impresspress-logo-2x-"), "{url}");
        assert!(url.ends_with(".png"));
        let hash = url
            .trim_start_matches("/b/static/impresspress-logo-2x-")
            .trim_end_matches(".png");
        assert_eq!(hash.len(), 8, "expected 8-char short hash, got: {hash}");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[cfg(feature = "embed-assets")]
    #[test]
    fn favicon_ico_frames_are_16_32_48_at_native_size() {
        let ico = super::bytes("favicon.ico").unwrap();
        assert_eq!(u16::from_le_bytes([ico[2], ico[3]]), 1, "ICO type");
        let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
        let sizes: Vec<u8> = (0..count).map(|i| ico[6 + i * 16]).collect();
        assert_eq!(sizes, vec![16, 32, 48]);
        for (i, &size) in sizes.iter().enumerate() {
            let e = 6 + i * 16;
            let len =
                u32::from_le_bytes([ico[e + 8], ico[e + 9], ico[e + 10], ico[e + 11]]) as usize;
            let off =
                u32::from_le_bytes([ico[e + 12], ico[e + 13], ico[e + 14], ico[e + 15]]) as usize;
            let frame = &ico[off..off + len];
            assert_eq!(
                png_size(frame),
                (u32::from(size), u32::from(size)),
                "frame {i} is a 1:1 PNG"
            );
        }
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

    #[cfg(feature = "embed-assets")]
    #[test]
    fn tokens_include_new_scale() {
        let s = super::css();
        for tok in [
            "--text-base",
            "--text-2xl",
            "--space-12",
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

    /// Every value here is read from the LIVE token map rather than written
    /// as a frozen hex literal. It used to spell all four out -- and drifted:
    /// it asserted on `#64748b` while calling it "--text-muted" long after
    /// that token had moved to `#5d6b7f`, so the assertion still passed while
    /// describing a colour the stylesheet no longer used. A test naming a
    /// token must resolve that token, or it is a snapshot of a past build
    /// wearing the token's name.
    #[cfg(feature = "embed-assets")]
    #[test]
    fn brand_tokens_meet_wcag_aa() {
        let tokens = parse_root_tokens(super::css());
        let hex = |name: &str| -> String {
            let raw = tokens
                .get(name)
                .unwrap_or_else(|| panic!("token {name} missing from :root"));
            let rgba = resolve_color(raw, &tokens, 0)
                .unwrap_or_else(|| panic!("token {name} ({raw}) did not resolve to a colour"));
            hex_of(composite_over(rgba, (255, 255, 255)))
        };

        let primary_button = hex("--primary-button");
        let sidebar_bg = hex("--bg-sidebar");
        let sidebar_muted = hex("--sidebar-text-muted");
        let text_muted = hex("--text-muted");

        // White-on-red button surfaces carry normal-size text: 4.5:1 required.
        assert!(
            contrast(&primary_button, "#ffffff") >= 4.5,
            "--primary-button ({primary_button}) fails AA under white text: {:.2}:1",
            contrast(&primary_button, "#ffffff")
        );
        // Sidebar foregrounds on the navy slab.
        assert!(
            contrast("#ffffff", &sidebar_bg) >= 4.5,
            "white on --bg-sidebar ({sidebar_bg}) fails AA"
        );
        assert!(
            contrast(&sidebar_muted, &sidebar_bg) >= 4.5,
            "--sidebar-text-muted ({sidebar_muted}) on --bg-sidebar ({sidebar_bg}) fails AA: {:.2}:1",
            contrast(&sidebar_muted, &sidebar_bg)
        );
        // The regression this guards: the page-level muted token is too dark
        // for the navy slab (3.47:1 as of --text-muted #5d6b7f), which is why
        // --sidebar-text-muted exists as a separate, lighter token. If this
        // ever passes, the two have converged and one of them is redundant.
        assert!(
            contrast(&text_muted, &sidebar_bg) < 4.5,
            "--text-muted ({text_muted}) now clears AA on --bg-sidebar ({sidebar_bg}) -- \
             the separate --sidebar-text-muted token may be redundant"
        );
    }

    #[cfg(feature = "embed-assets")]
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

    #[cfg(feature = "embed-assets")]
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

    /// Selectors this contrast guard does not hold to the 4.5:1 text floor,
    /// each with its own reason -- not a silent pass.
    const CONTRAST_EXEMPT_SELECTORS: &[&str] = &[
        // `.db-table-group__icon` wraps `icons::package()`/`icons::database()`
        // (database.rs) -- an SVG icon, not text; `color` only feeds the
        // SVG's `currentColor`. WCAG's 3:1 non-text floor applies, and
        // #fd3534 (3.66:1 on white) already clears it.
        ".db-table-group__icon",
        // Disabled form controls. WCAG 2.x SC 1.4.3 explicitly exempts text
        // in "inactive user interface components" from the contrast minimum,
        // and the muted look is what communicates the disabled state. 4.39:1
        // (computed) -- deliberately just under, not an oversight.
        ".form-input:disabled",
        ".form-select:disabled",
        ".form-textarea:disabled",
    ];

    /// Text whose background comes from an ANCESTOR rule rather than its own.
    /// A single-rule scan cannot see the cascade, so without this table these
    /// selectors would either be skipped (unverified) or measured against a
    /// wrong assumed-white background and reported as false failures. Mapping
    /// each to the token its true ancestor actually sets means they get
    /// genuinely checked instead of waved through -- the navy panels are the
    /// only place in the bundle where text sits on a non-white surface set by
    /// a parent.
    const ANCESTOR_BACKGROUNDS: &[(&str, &str)] = &[
        // `.sidebar`'s navy slab is painted by `.sidebar__nav` in
        // components/nav.css (`background: var(--bg-sidebar)`); every
        // `.sidebar__*` label, link and avatar caption renders on it.
        (".sidebar__", "--bg-sidebar"),
        // `.auth-split__brand` sets `background: var(--navy-900)` for the
        // login page's left-hand brand panel (layouts/auth-split.css).
        (".auth-split__", "--navy-900"),
    ];

    /// Asserts every text/background pair the CSS bundle declares meets the
    /// 4.5:1 WCAG AA floor for normal text, matching on RESOLVED COLOR VALUES
    /// rather than token names.
    ///
    /// Value-matching is what closes the alias hole that let
    /// `--accent-info: #fd3534` -- and bare literals like `#ef4444` --
    /// evade the two earlier name-matching guards (Tasks 12a and 15): a new
    /// token, or a re-literalized hex, cannot rename its way past this.
    ///
    /// This guard used to be scoped to the primary/danger (brand-red) token
    /// family only. That scoping is why it never saw the success and warning
    /// families: `--accent-success` (#10b981) as text was 2.54:1 on white and
    /// 2.31:1 on its own tint, and `.badge-warning` was 1.99:1 -- the worst
    /// pair in the bundle -- both structurally invisible to a family-filtered
    /// check, and both shipped. The narrow scope was a deliberate, documented
    /// deferral of "a separate, larger, unbudgeted audit"; that audit has now
    /// been done (every failing pair fixed, `--accent-success-text` added to
    /// match the `-text` siblings danger and warning already had, and
    /// `--text-secondary`/`--text-muted` given tint headroom), so the filter
    /// is gone and this now evaluates EVERY rule with a resolvable text color.
    /// Scoping a guard to the family whose bug prompted it is exactly how the
    /// next family's copy of that bug survives.
    ///
    /// Background resolution, in order: the rule's own `background`/
    /// `background-color`; else an `ANCESTOR_BACKGROUNDS` entry; else the
    /// page surface (white). Translucent tints are alpha-composited over the
    /// resolved background before measuring.
    ///
    /// What this still cannot see: a parent background paired with child text
    /// outside the `ANCESTOR_BACKGROUNDS` table; colors applied via inline
    /// `style` or JS; and values expressed through CSS functions the resolver
    /// does not parse. Such a rule is SKIPPED rather than assumed compliant --
    /// so the count below is asserted too, to catch a future refactor that
    /// silently drops rules out of coverage by making them unresolvable.
    // Depends on `css()`, so it needs the same `embed-assets` gate as every
    // other CSS-content test in this module -- a no-embed build has no CSS
    // bytes to check contrast on.
    #[cfg(feature = "embed-assets")]
    #[test]
    fn text_and_background_pairs_meet_wcag_aa() {
        let s = super::css();
        let tokens = parse_root_tokens(s);

        let exempt = |selector: &str| -> bool {
            selector
                .split(',')
                .map(str::trim)
                .any(|part| CONTRAST_EXEMPT_SELECTORS.contains(&part))
        };
        let ancestor_bg = |selector: &str| -> Option<Rgba> {
            ANCESTOR_BACKGROUNDS.iter().find_map(|(prefix, token)| {
                if selector
                    .split(',')
                    .map(str::trim)
                    .any(|part| part.starts_with(prefix))
                {
                    resolve_color(tokens.get(*token)?, &tokens, 0)
                } else {
                    None
                }
            })
        };

        let mut offenders: Vec<String> = Vec::new();
        let mut checked = 0usize;
        for (selector, body) in css_leaf_blocks(s) {
            if exempt(&selector) {
                continue;
            }
            let decls: Vec<&str> = body
                .split(';')
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .collect();
            let Some(color_decl) = decls.iter().find(|d| d.starts_with("color:")).copied() else {
                continue;
            };
            let Some(text_rgba) = resolve_color(&color_decl["color:".len()..], &tokens, 0) else {
                continue; // unresolvable -- can't verify, skip (documented above)
            };
            if text_rgba.3 == 0 {
                continue; // fully transparent text, not visible
            }

            // The surface this rule's own background (if any) paints onto:
            // an ancestor's background where we know it, else the page.
            let base_rgb = composite_over(
                ancestor_bg(&selector).unwrap_or((255, 255, 255, 255)),
                (255, 255, 255),
            );
            let bg_decl: Option<&str> = decls
                .iter()
                .find(|d| d.starts_with("background:") || d.starts_with("background-color:"))
                .copied();
            // A declared background composites OVER that base rather than
            // replacing it, so `background: transparent` (and any translucent
            // tint) correctly shows the ancestor through it. Compositing
            // against white unconditionally instead would have reported
            // `.sidebar__collapse-toggle` -- transparent over the navy slab --
            // as 2.56:1 white-background text, a false failure.
            let bg_rgb = match bg_decl {
                Some(d) => {
                    let val = d.split_once(':').map(|x| x.1).unwrap_or_default();
                    match resolve_color(val, &tokens, 0) {
                        Some(c) => composite_over(c, base_rgb),
                        None => continue, // declared but unresolvable -- can't verify
                    }
                }
                None => base_rgb,
            };

            let text_rgb = composite_over(text_rgba, bg_rgb);
            let ratio = contrast(&hex_of(text_rgb), &hex_of(bg_rgb));
            checked += 1;
            if ratio < 4.5 {
                offenders.push(format!(
                    "{}: {ratio:.2}:1 ({} text on {} background)",
                    selector.split(',').next().unwrap_or(&selector).trim(),
                    hex_of(text_rgb),
                    hex_of(bg_rgb)
                ));
            }
        }

        assert!(
            offenders.is_empty(),
            "text/background pairs failing 4.5:1 AA (computed): {offenders:#?}"
        );
        // Coverage floor: the bundle currently resolves ~200 such pairs. If a
        // refactor makes colors unresolvable to this parser (moving them into
        // an unparsed CSS function, say), rules would drop out of coverage
        // silently and the assert above would pass vacuously. Deliberately
        // loose -- this catches a collapse, not normal drift.
        assert!(
            checked > 120,
            "only {checked} text/background pairs were resolvable -- expected >120; \
             the resolver has probably stopped understanding a common value form, \
             which would make the contrast assertion above vacuous"
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

    #[cfg(feature = "embed-assets")]
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
                        "impresspress-logo.png", "impresspress-logo-2x.png"] {
            assert!(super::bytes(logical).is_some(), "core asset missing: {logical}");
        }
    }

    /// The "no-embed build" case the branch's spec called for and never got:
    /// assert the asset bytes are genuinely absent when `embed-assets` is
    /// off, not merely unreferenced-and-hopefully-linker-stripped.
    ///
    /// The strongest form of that guarantee is structural, not a runtime
    /// assertion: [`bytes`] and [`css`] are themselves `#[cfg(feature =
    /// "embed-assets")]`-gated (as of this fix), so under this cfg neither
    /// function -- nor the `include_bytes!`/`include_str!` literals inside
    /// them -- exists in the compiled crate at all; the compiler enforces
    /// it, the linker never has to. This test (only compiled under the
    /// opposite cfg from `embedded_bytes_agree_with_the_manifest` above)
    /// documents and exercises the other half of that contract: the
    /// manifest-driven URL surface -- what a lean build actually needs to
    /// keep working, e.g. to point at R2 or the CDN -- has no dependency on
    /// embedded bytes and still resolves every asset to a valid
    /// content-hashed URL.
    #[cfg(not(feature = "embed-assets"))]
    #[test]
    fn no_embed_build_has_no_asset_bytes_only_the_manifest() {
        assert!(!cfg!(feature = "embed-assets"));
        assert!(!super::ASSETS.is_empty(), "build.rs still populates the manifest without embed-assets");
        for e in super::ASSETS {
            // The filename carries its own content hash (baked in by
            // build.rs from the source file's bytes, at *build* time -- this
            // does not require `embed-assets`, which only controls whether
            // those bytes are additionally compiled into the *runtime*
            // binary). `url()` must still resolve it correctly.
            let u = super::url(e.logical);
            assert!(u.ends_with(e.filename), "{}: url {u} does not end in its own filename", e.logical);
        }
    }

    #[cfg(feature = "embed-assets")]
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
