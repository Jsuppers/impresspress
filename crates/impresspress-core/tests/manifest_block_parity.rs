//! The three manifests' `block-*` sets agree, or the difference is written
//! down with a reason.
//!
//! `impresspress-core` declares one cargo feature per optional block.
//! `impresspress-cloudflare` forwards a subset as its own passthrough
//! features; `impresspress-web` enables a subset directly on the
//! `impresspress-core` dependency. Both of those lists used to carry a comment
//! asking the other to "keep in sync" — and both drifted anyway: Cloudflare's
//! comment claimed `block-llm` / `block-vector` were excluded because the LLM
//! block pulled `tokio`/`reqwest` on wasm32, which had already stopped being
//! true, while `impresspress-web` had been running both on wasm32 for
//! releases. A comment asking two files to stay in sync is not a mechanism;
//! this file is.
//!
//! ## What "agree" means here
//!
//! Nothing forces a target to carry every block — a Worker has a hard wasm
//! size budget and the dev sandbox is deliberately browser-only. What the
//! gate refuses is an *unexplained* difference. Every block feature that is
//! not offered by all three crates is listed in [`DIVERGENT`] with the reason
//! it is not, and a stale entry there fails the same test: the table is
//! checked to be exactly the set of features that actually diverge.
//!
//! ## Available is not default
//!
//! `impresspress-cloudflare` offering `block-llm` means a consumer *can* ask
//! for it. It is deliberately in neither `default` nor `full`, and
//! [`cloudflare_keeps_the_heavy_blocks_out_of_its_presets`] pins that: this
//! repo targets Cloudflare Workers, where wasm size is a measured constraint
//! and `default = []` is a deliberate fail-safe-lean choice.

use std::collections::{BTreeMap, BTreeSet};

/// Block features that are NOT offered by all three crates, with the reason.
///
/// Keep the reason specific enough that a reader can tell whether it still
/// holds. "wasm32-incompatible" is a claim with an expiry date; name what is
/// incompatible.
const DIVERGENT: &[(&str, &str)] = &[
    (
        "block-dev",
        "The browser development sandbox's own control plane (compile, stage, \
         activate, export). Its security model is that the block is ABSENT \
         from every normal bundle, not merely switched off in one, so \
         impresspress-web reaches it only through its opt-in \
         `browser-devtools` feature and impresspress-cloudflare does not \
         offer it at all — there is no Worker-side sandbox.",
    ),
    (
        "block-fastembed",
        "The native ONNX-backed embedding block. It pulls \
         `wafer-block-fastembed` (ONNX Runtime) and `rusqlite`, neither of \
         which builds for wasm32. Both wasm targets get their embeddings from \
         an injected `EmbeddingService` through `impresspress/transformers-embed` \
         instead.",
    ),
];

fn manifest(crate_dir: &str) -> toml::Table {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(crate_dir)
        .join("Cargo.toml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    text.parse::<toml::Table>()
        .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// The `[features]` table, as `name -> [implied…]`.
fn features(manifest: &toml::Table) -> BTreeMap<String, Vec<String>> {
    manifest
        .get("features")
        .and_then(toml::Value::as_table)
        .map(|table| {
            table
                .iter()
                .map(|(name, implied)| {
                    let implied = implied
                        .as_array()
                        .unwrap_or_else(|| panic!("feature `{name}` is not an array"))
                        .iter()
                        .map(|v| {
                            v.as_str()
                                .unwrap_or_else(|| {
                                    panic!("feature `{name}` has a non-string entry")
                                })
                                .to_string()
                        })
                        .collect();
                    (name.clone(), implied)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The features this crate enables on its `impresspress-core` dependency.
fn core_dependency_features(manifest: &toml::Table) -> Vec<String> {
    manifest
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .and_then(|deps| deps.get("impresspress-core"))
        .and_then(toml::Value::as_table)
        .and_then(|dep| dep.get("features"))
        .and_then(toml::Value::as_array)
        .map(|list| {
            list.iter()
                .map(|v| v.as_str().expect("a feature name").to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Every block feature `impresspress-core` declares.
fn core_blocks() -> BTreeSet<String> {
    features(&manifest("impresspress-core"))
        .into_keys()
        .filter(|name| name.starts_with("block-"))
        .collect()
}

/// Every block feature `impresspress-cloudflare` offers, as its own
/// passthrough features.
fn cloudflare_blocks() -> BTreeSet<String> {
    features(&manifest("impresspress-cloudflare"))
        .into_keys()
        .filter(|name| name.starts_with("block-"))
        .collect()
}

/// Every block feature `impresspress-web` makes reachable: the ones it
/// enables directly on the `impresspress-core` dependency, plus any a feature
/// of its own turns on (`browser-devtools` → `impresspress-core/block-dev`).
fn web_blocks() -> BTreeSet<String> {
    let manifest = manifest("impresspress-web");
    let mut blocks: BTreeSet<String> = core_dependency_features(&manifest)
        .into_iter()
        .filter(|name| name.starts_with("block-"))
        .collect();
    for implied in features(&manifest).into_values().flatten() {
        if let Some(block) = implied.strip_prefix("impresspress-core/") {
            if block.starts_with("block-") {
                blocks.insert(block.to_string());
            }
        }
    }
    blocks
}

/// The gate. The three sets are equal except for [`DIVERGENT`], and
/// `DIVERGENT` names exactly the features that differ — a stale entry is as
/// much a failure as a missing one, because a table nobody prunes stops being
/// read.
#[test]
fn the_three_manifests_offer_the_same_blocks_or_say_why_not() {
    let core = core_blocks();
    let cloudflare = cloudflare_blocks();
    let web = web_blocks();

    let everywhere: BTreeSet<String> = core
        .iter()
        .filter(|b| cloudflare.contains(*b) && web.contains(*b))
        .cloned()
        .collect();
    let all: BTreeSet<String> = core
        .union(&cloudflare)
        .cloned()
        .collect::<BTreeSet<_>>()
        .union(&web)
        .cloned()
        .collect();
    let actually_divergent: BTreeSet<String> = all.difference(&everywhere).cloned().collect();

    let declared_divergent: BTreeSet<String> =
        DIVERGENT.iter().map(|(name, _)| name.to_string()).collect();

    assert_eq!(
        actually_divergent, declared_divergent,
        "\ncore:       {core:?}\ncloudflare: {cloudflare:?}\nweb:        {web:?}\n\
         Every block feature not offered by all three crates must be listed in \
         `DIVERGENT` with the reason it is not, and every entry in `DIVERGENT` \
         must still be one — add the entry, or delete the stale one."
    );

    for (name, reason) in DIVERGENT {
        assert!(
            !reason.trim().is_empty(),
            "`{name}` is listed as divergent with no reason"
        );
    }
}

/// Every block feature the two wasm crates name is one `impresspress-core`
/// actually declares. A typo here is otherwise a feature that silently does
/// nothing (Cargo only errors on an unknown feature of a *dependency*, and
/// `impresspress-web` names those, but a Cloudflare passthrough could be
/// declared and never forwarded).
#[test]
fn no_wasm_crate_names_a_block_feature_that_does_not_exist() {
    let core = core_blocks();
    for (crate_name, blocks) in [
        ("impresspress-cloudflare", cloudflare_blocks()),
        ("impresspress-web", web_blocks()),
    ] {
        for block in blocks {
            assert!(
                core.contains(&block),
                "{crate_name} names `{block}`, which impresspress-core does not declare"
            );
        }
    }
}

/// A Cloudflare passthrough forwards its own name and nothing else.
///
/// CLAUDE.md: no translation between representations. `block-llm` here means
/// `impresspress-core/block-llm` there — a passthrough that renamed or
/// bundled would make the feature name in a consumer's `Cargo.toml` stop
/// describing what it turns on.
#[test]
fn every_cloudflare_passthrough_forwards_its_own_name() {
    for (name, implied) in features(&manifest("impresspress-cloudflare")) {
        if !name.starts_with("block-") {
            continue;
        }
        assert_eq!(
            implied,
            vec![format!("impresspress-core/{name}")],
            "`{name}` must forward exactly `impresspress-core/{name}`"
        );
    }
}

/// Available is not default.
///
/// `block-llm` and `block-vector` are offered so a Worker deployment *can*
/// have the chat UI and the vector admin pages. They are in neither preset:
/// this repo targets Cloudflare Workers, `default = []` is a deliberate
/// fail-safe-lean choice (a consumer that forgets `default-features = false`
/// gets the lean bundle, not the bloated one), and `full` is documented as
/// "the set this crate used to enable by default" — adding to it would make
/// every consumer of `full` pay for blocks they never asked for.
///
/// Measured on `examples/webmcp-demo` (a real Worker cdylib whose
/// `#[event(fetch)]` calls `impresspress_cloudflare::run`; building the
/// adapter crate alone measures nothing, because it exports no entry point
/// and the linker discards every block): 7,790,612 bytes as shipped,
/// +445,656 with `block-llm`, +736,120 with `block-vector`. The full figures
/// and the command are in `impresspress-cloudflare/Cargo.toml` beside the
/// features themselves.
#[test]
fn cloudflare_keeps_the_heavy_blocks_out_of_its_presets() {
    let features = features(&manifest("impresspress-cloudflare"));
    let default = features.get("default").expect("a `default` feature");
    let full = features.get("full").expect("a `full` feature");

    assert!(
        default.is_empty(),
        "Cloudflare's `default` is deliberately empty; it is {default:?}"
    );
    for heavy in ["block-llm", "block-vector"] {
        assert!(
            !full.contains(&heavy.to_string()),
            "`{heavy}` is available on the Cloudflare target but must not be in \
             `full` — see this test's documentation for the measurement"
        );
    }
}
