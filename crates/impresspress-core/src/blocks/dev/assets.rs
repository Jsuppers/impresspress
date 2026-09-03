//! The `/b/dev` page's own static assets — its script and its stylesheet.
//!
//! They live with the block rather than in [`crate::ui::assets`] because they
//! are the sandbox's, not the admin chrome's: a build without `block-dev`
//! must not carry them, and a page other than `/b/dev` has no use for either.
//! That is also why they are served from `/b/dev/static/*` at the block's own
//! `Admin` tier instead of the public, content-hashed `/b/static/*` bundle.

use std::sync::{LazyLock, OnceLock};

/// The workspace page's script, authored as a TAIL: no IIFE, no
/// `'use strict'`, exactly like `ui/assets/webmcp.js`. It is only ever served
/// through [`dev_js`], which composes it with the shared `webmcp-core.js`
/// fragment — reading this file on its own, a top-level `return` or a call to
/// `toolOptions` looks unbound; it is not, because the composition below is
/// the only form in which the bytes exist on the wire.
const DEV_JS_TAIL: &str = include_str!("assets/dev.js");

/// The workspace page's stylesheet.
const DEV_CSS: &str = include_str!("assets/dev.css");

/// The composed `/b/dev/static/dev.js`: `webmcp-core.js` plus [`DEV_JS_TAIL`]
/// inside one IIFE.
///
/// Composed through [`crate::ui::assets::compose_webmcp_script`] — the same
/// function `webmcp_js()` uses — so both WebMCP scripts this crate serves are
/// wrapped identically and `buildRequest`/`toolOptions` behave the same in
/// each. Cached in a `LazyLock` so repeat requests don't re-format the string.
pub fn dev_js() -> &'static str {
    static SCRIPT: LazyLock<String> =
        LazyLock::new(|| crate::ui::assets::compose_webmcp_script(DEV_JS_TAIL));
    &SCRIPT
}

/// The `/b/dev/static/dev.css` stylesheet.
pub fn dev_css() -> &'static str {
    DEV_CSS
}

/// Short content hash of [`dev_js`], for `page.rs::asset`'s `ETag` — the
/// same projection `ui::assets::webmcp_js_hash()` gives `webmcp.js`, so a
/// rebuilt binary with different script bytes invalidates a repeat
/// visitor's cached copy instead of serving a `304` for content that
/// changed.
pub fn dev_js_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| crate::ui::assets::short_hash(dev_js().as_bytes()))
}

/// Short content hash of [`dev_css`], for `page.rs::asset`'s `ETag`.
pub fn dev_css_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| crate::ui::assets::short_hash(dev_css().as_bytes()))
}
