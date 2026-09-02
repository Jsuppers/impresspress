//! Locates the precompiled impresspress-web wasm in the workspace target dir
//! and copies it into OUT_DIR for include_bytes! consumption from main.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR set by cargo"));

    // CARGO_MANIFEST_DIR is crates/impresspress. Workspace root is two up.
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root is two levels above crates/impresspress");

    // 1. WASM binary candidates
    let wasm_candidates = [
        workspace_root.join("crates/impresspress-web/pkg/impresspress_web_bg.wasm"),
        workspace_root.join("target/wasm32-unknown-unknown/release/impresspress_web.wasm"),
    ];

    let wasm_src = wasm_candidates
        .iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| {
            eprintln!(
                "\nerror: impresspress-web wasm not found. Tried:\n{}\n\nRun \"just build\" or:\n  \
                 cargo build -p impresspress-web --release --target wasm32-unknown-unknown\nfirst.\n",
                wasm_candidates
                    .iter()
                    .map(|p| format!("  - {}", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            std::process::exit(1);
        });

    let wasm_dst = out_dir.join("impresspress-web.wasm");
    fs::copy(wasm_src, &wasm_dst).unwrap_or_else(|e| {
        panic!(
            "failed to copy {} -> {}: {e}",
            wasm_src.display(),
            wasm_dst.display()
        )
    });

    // 2. JS glue file
    let js_src = workspace_root.join("crates/impresspress-web/pkg/impresspress_web.js");
    if !js_src.exists() {
        eprintln!(
            "\nerror: impresspress-web JS glue not found at {}.\nRun \"just build\" first.\n",
            js_src.display()
        );
        std::process::exit(1);
    }

    let js_dst = out_dir.join("impresspress-web.js");
    fs::copy(&js_src, &js_dst).unwrap_or_else(|e| {
        panic!(
            "failed to copy {} -> {}: {e}",
            js_src.display(),
            js_dst.display()
        )
    });

    // 3. Inline-JS snippets.
    //
    // A `wasm-pack --target web` output is THREE things, not two: the wasm,
    // the JS glue, and the `snippets/<crate>-<hash>/…` tree the glue imports
    // from (`impresspress-browser`'s `#[wasm_bindgen(module = "/js/bridge.js")]`
    // lands there). The glue's very first statement is
    //
    //     import { … } from './snippets/impresspress-browser-<hash>/js/bridge.js';
    //
    // so a bundle shipped without the tree fails to load its module at all,
    // `sw.js` self-destructs, and the app serves its boot shell forever. The
    // sealed × web flow writes what this crate bakes, so the tree has to be
    // baked too — otherwise the *default* sealed bundle is broken and only an
    // `IMPRESSPRESS_WEB_PKG_DIR` override can produce a working one.
    //
    // Emitted as a generated `&[(&str, &[u8])]` of (path relative to
    // `snippets/`, contents) so `include_bytes!` still does the embedding and
    // no file list is written by hand.
    let snippets_root = workspace_root.join("crates/impresspress-web/pkg/snippets");
    let mut snippets: Vec<(String, PathBuf)> = Vec::new();
    collect_files(&snippets_root, &snippets_root, &mut snippets);
    // Sorted so the generated file — and therefore the build — is
    // deterministic regardless of directory iteration order.
    snippets.sort();

    // One raw literal, so the generated file is not indented by this file's
    // own formatting.
    let mut generated = String::from(
        r#"/// Precompiled impresspress-web inline-JS snippets, baked at build time:
/// `(path relative to snippets/, contents)`.
///
/// The CLI's sealed x web flow writes these into `dist/snippets/`. The JS glue
/// imports from there, so a bundle without them cannot load its own module.
///
/// Empty only when `crates/impresspress-web/pkg/snippets` did not exist at
/// build time (a wasm-pack output with no inline JS modules).
pub static IMPRESSPRESS_WEB_SNIPPETS: &[(&str, &[u8])] = &[
"#,
    );
    for (rel, abs) in &snippets {
        println!("cargo:rerun-if-changed={}", abs.display());
        generated.push_str(&format!(
            "    ({:?}, include_bytes!({:?})),\n",
            rel,
            abs.display().to_string()
        ));
    }
    generated.push_str("];\n");
    let snippets_dst = out_dir.join("impresspress-web-snippets.rs");
    fs::write(&snippets_dst, generated)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", snippets_dst.display()));

    // Re-run if the source wasm or JS changes.
    println!("cargo:rerun-if-changed={}", wasm_src.display());
    println!("cargo:rerun-if-changed={}", js_src.display());
    // A file ADDED to or REMOVED from the tree changes the directory itself,
    // which the per-file lines above cannot notice.
    println!("cargo:rerun-if-changed={}", snippets_root.display());
    // Allow override during developer iteration via env.
    println!("cargo:rerun-if-env-changed=IMPRESSPRESS_WEB_WASM_OVERRIDE_FOR_BUILD");
}

/// Collect every file under `dir` as `(path relative to `root`, absolute path)`.
///
/// A missing directory is not an error: a wasm-pack output with no inline JS
/// modules has no `snippets/` at all, and that is a legal (if unusual) build.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            // `/` separators: the value is used as a URL path component by the
            // consumer, and this build runs on the platforms the CLI ships from.
            let rel = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((rel, path));
        }
    }
}
