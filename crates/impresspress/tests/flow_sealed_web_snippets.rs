//! Sealed × web must ship all THREE parts of a `wasm-pack --target web`
//! output.
//!
//! The JS glue's first statement is
//! `import { … } from './snippets/<crate>-<hash>/js/bridge.js'`, so a `dist/`
//! without that tree cannot load its own module: `sw.js` self-destructs and
//! the app serves its boot shell forever, with no error anywhere. Nothing
//! about the flow's output *looks* wrong when this regresses, which is why it
//! is a test rather than a comment.
//!
//! Its own integration-test binary because it sets `IMPRESSPRESS_WEB_PKG_DIR`,
//! which is process-global: a test in `flow_sealed_web.rs` running
//! concurrently would see it too.

use std::fs;

use impresspress::cli::{flows::sealed_web, helpers::wasm::PKG_DIR_ENV};
use tempfile::tempdir;

/// A minimal `wasm-pack --target web --out-dir <dir>` output: the manifest
/// that names the glue, the pair itself, and one inline-JS snippet under the
/// `<crate>-<hash>` directory wasm-bindgen assigns.
fn fake_pkg_dir(dir: &std::path::Path) {
    fs::write(
        dir.join("package.json"),
        r#"{"name":"fake-web","type":"module","main":"fake_web.js","types":"fake_web.d.ts"}"#,
    )
    .unwrap();
    fs::write(dir.join("fake_web_bg.wasm"), b"\x00asm\x01\x00\x00\x00").unwrap();
    // The glue must carry the literal the bundler rewrites when it
    // content-hashes the wasm — `sealed_web` writes the pair under
    // impresspress's own names, so that is the literal to spell here.
    fs::write(
        dir.join("fake_web.js"),
        "import { ping } from './snippets/fake-browser-0123456789abcdef/js/bridge.js';\n\
         const url = new URL('impresspress_web_bg.wasm', import.meta.url);\n",
    )
    .unwrap();
    let snippet_dir = dir.join("snippets/fake-browser-0123456789abcdef/js");
    fs::create_dir_all(&snippet_dir).unwrap();
    fs::write(snippet_dir.join("bridge.js"), "export function ping() {}\n").unwrap();
}

#[tokio::test]
async fn a_pkg_dir_override_ships_its_snippets_beside_the_wasm_and_glue() {
    let pkg = tempdir().unwrap();
    fake_pkg_dir(pkg.path());
    let app = tempdir().unwrap();

    std::env::set_var(PKG_DIR_ENV, pkg.path());
    let built = sealed_web::build(app.path(), false).await;
    std::env::remove_var(PKG_DIR_ENV);
    built.unwrap();

    let dist = app.path().join("dist");

    // The snippet lands at the exact path the glue's import names — the whole
    // point: the relative path inside `snippets/` is what wasm-bindgen wrote
    // into the import, so it has to survive the copy verbatim.
    let snippet = dist.join("snippets/fake-browser-0123456789abcdef/js/bridge.js");
    assert!(
        snippet.is_file(),
        "expected {} to exist; dist held {:?}",
        snippet.display(),
        fs::read_dir(&dist)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        fs::read_to_string(&snippet).unwrap(),
        "export function ping() {}\n",
        "the snippet's bytes must be the override's, not the baked build's"
    );

    // And the pair came from that same directory, so the three parts cannot be
    // from three different builds. The bundler content-hashes the pair and
    // deletes the unhashed copies, so they are found by extension rather than
    // by name.
    let one = |ext: &str| -> std::path::PathBuf {
        let mut found: Vec<_> = fs::read_dir(&dist)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some(ext))
            .filter(|p| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|n| n.starts_with("impresspress_web"))
            })
            .collect();
        assert_eq!(
            found.len(),
            1,
            "expected one impresspress_web*.{ext} in dist"
        );
        found.remove(0)
    };
    assert_eq!(fs::read(one("wasm")).unwrap(), b"\x00asm\x01\x00\x00\x00");
    assert!(
        fs::read_to_string(one("js"))
            .unwrap()
            .contains("./snippets/fake-browser-0123456789abcdef/js/bridge.js"),
        "the bundler must not rewrite the snippets import out of the glue"
    );
}

/// Without the override, the flow writes the tree `build.rs` baked — which is
/// what makes the *default* sealed bundle work, not just an overridden one.
#[tokio::test]
async fn the_baked_default_also_ships_snippets() {
    let app = tempdir().unwrap();
    std::env::remove_var(PKG_DIR_ENV);
    sealed_web::build(app.path(), false).await.unwrap();

    let snippets = app.path().join("dist/snippets");
    assert!(
        snippets.is_dir(),
        "the baked impresspress-web build has inline JS modules, so `dist/snippets/` must exist"
    );
    let bridges: Vec<_> = fs::read_dir(&snippets)
        .unwrap()
        .flatten()
        .map(|e| e.path().join("js/bridge.js"))
        .filter(|p| p.is_file())
        .collect();
    assert!(
        !bridges.is_empty(),
        "expected at least one <crate>-<hash>/js/bridge.js under {}",
        snippets.display()
    );
}
