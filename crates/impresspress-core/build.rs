//! Build-time asset manifest.
//!
//! Hashes every static asset and assembles the CSS bundle so the crate has a
//! single source of truth for asset identity. This must happen at build time:
//! with `embed-assets` off the bytes are not in the binary, so nothing is left
//! to hash at runtime, yet the content-hashed URLs still have to resolve.

use std::{env, fs, path::PathBuf};

use sha2::{Digest, Sha256};

/// CSS bundle order. Explicit, never a glob — see CLAUDE.md, "no magic code".
///
/// Tokens first so custom properties are defined before use; layouts last so
/// they can override component defaults. Every entry must exist on disk —
/// this build.rs panics on a missing layer (see `main`'s `fs::read_to_string`
/// below), even for a layer that has no rules yet.
const CSS_ORDER: &[&str] = &[
    "styles/tokens.css",
    "styles/base.css",
    "styles/components/button.css",
    "styles/components/card.css",
    "styles/components/table.css",
    "styles/components/form.css",
    "styles/components/badge.css",
    "styles/components/modal.css",
    "styles/components/nav.css",
    "styles/components/toast.css",
    "styles/components/palette.css",
    "styles/components/stat.css",
    "styles/components/chart.css",
    "styles/components/auth.css",
    "styles/layouts/shell.css",
    "styles/layouts/page.css",
    "styles/layouts/auth-split.css",
];

/// Single-file assets: (relative path under src/ui, logical key, content type).
const FILE_ASSETS: &[(&str, &str, &str)] = &[
    ("assets/htmx.min.js", "htmx.min.js", "application/javascript; charset=utf-8"),
    ("assets/webmcp.js", "webmcp.js", "application/javascript; charset=utf-8"),
    ("assets/marked.min.js", "marked.min.js", "application/javascript; charset=utf-8"),
    ("assets/purify.min.js", "purify.min.js", "application/javascript; charset=utf-8"),
    ("assets/llm-chat.js", "llm-chat.js", "application/javascript; charset=utf-8"),
    ("assets/files-browser.js", "files-browser.js", "application/javascript; charset=utf-8"),
    ("assets/fonts/itim-latin.woff2", "itim-latin.woff2", "font/woff2"),
    ("assets/fonts/itim-latin-ext.woff2", "itim-latin-ext.woff2", "font/woff2"),
    ("assets/impresspress-logo.png", "impresspress-logo.png", "image/png"),
    ("assets/impresspress-logo-long.png", "impresspress-logo-long.png", "image/png"),
    ("assets/favicon.ico", "favicon.ico", "image/x-icon"),
];

fn short_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// `foo.css` + `a1b2c3d4` -> `foo-a1b2c3d4.css`
fn hashed_name(logical: &str, hash: &str) -> String {
    match logical.split_once('.') {
        Some((stem, ext)) => format!("{stem}-{hash}.{ext}"),
        None => format!("{logical}-{hash}"),
    }
}

fn main() {
    let ui = PathBuf::from("src/ui");
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let mut entries: Vec<(String, String, String, usize)> = Vec::new();

    // --- single files -----------------------------------------------------
    let mut font_names = Vec::new();
    for (rel, logical, ct) in FILE_ASSETS {
        let path = ui.join(rel);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = fs::read(&path)
            .unwrap_or_else(|e| panic!("missing asset {}: {e}", path.display()));
        let name = hashed_name(logical, &short_hash(&bytes));
        if logical.ends_with(".woff2") {
            font_names.push((logical.to_string(), name.clone()));
        }
        entries.push((logical.to_string(), name, ct.to_string(), bytes.len()));
    }

    // --- CSS bundle -------------------------------------------------------
    // Font URLs are rewritten to bare hashed filenames, which resolve relative
    // to the stylesheet's own URL. That is what makes the same bytes correct
    // both at /b/static/ and at a CDN path, with no placeholder substitution.
    let mut css = String::new();
    for rel in CSS_ORDER {
        let path = ui.join(rel);
        println!("cargo:rerun-if-changed={}", path.display());
        let part = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing CSS layer {}: {e}", path.display()));
        css.push_str(&part);
        css.push('\n');
    }
    for (logical, name) in &font_names {
        css = css.replace(&format!("url('{logical}')"), &format!("url('{name}')"));
    }
    assert!(!css.contains("__ITIM"), "stale placeholder left in bundle");

    let css_name = hashed_name("app.css", &short_hash(css.as_bytes()));
    entries.push(("app.css".into(), css_name, "text/css; charset=utf-8".into(), css.len()));
    fs::write(out.join("app.css"), &css).expect("write app.css");

    // --- manifest ---------------------------------------------------------
    let mut src = String::from(
        "pub struct AssetEntry {\n\
         \x20   pub logical: &'static str,\n\
         \x20   pub filename: &'static str,\n\
         \x20   pub content_type: &'static str,\n\
         \x20   pub len: usize,\n\
         }\n\
         pub const ASSETS: &[AssetEntry] = &[\n",
    );
    for (logical, name, ct, len) in &entries {
        src.push_str(&format!(
            "    AssetEntry {{ logical: {logical:?}, filename: {name:?}, content_type: {ct:?}, len: {len} }},\n"
        ));
    }
    src.push_str("];\n");
    fs::write(out.join("asset_manifest.rs"), src).expect("write manifest");
}
