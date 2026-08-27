//! Per-block `/openapi.json` snapshots.
//!
//! This is the gate for the derive migration. Replacing a hand-written
//! schema with a derived one can change the *public contract* in two ways
//! that are invisible at the call site:
//!
//! 1. **Widening** — derive exposes every field not marked `serde(skip)`,
//!    including any a hand-written schema deliberately omitted.
//! 2. **Description loss** — editorial text vanishes unless it is
//!    reintroduced as a doc comment.
//!
//! So: snapshot a block before migrating it, migrate, then read the diff.
//! Every changed line is a decision. Regenerate with
//! `UPDATE_OPENAPI_SNAPSHOTS=1 cargo test -p impresspress-core --test openapi_snapshot`.

use std::path::PathBuf;

/// Blocks under migration, mapped to the URL prefixes they actually serve.
///
/// **The prefix is NOT derivable from the block name**, and assuming it is
/// makes this whole gate silently vacuous:
///
/// * `auth_ui` serves `/b/auth/*` — nothing is under `/b/auth_ui/`.
/// * `files` serves TWO prefixes, `/b/storage/*` and `/b/cloudstorage/*`.
///
/// A name-derived prefix would produce a permanently empty snapshot for both,
/// which passes forever and reviews nothing.
const SNAPSHOTTED_BLOCKS: &[(&str, &[&str])] = &[
    ("products", &["/b/products"]),
    ("auth_ui", &["/b/auth"]),
    ("files", &["/b/storage", "/b/cloudstorage"]),
    ("messages", &["/b/messages"]),
    ("admin", &["/b/admin"]),
    ("tickets", &["/b/tickets"]),
];

/// Blocks that legitimately have no schema-carrying endpoints yet, so an
/// empty snapshot for them is correct rather than a sign the prefix map or
/// the document's block list is wrong.
///
/// * `admin` gets its schemas in Task 5.
/// * `tickets` is a block that arrived recently and has no schemas yet
///   either — its endpoints exist (see `SNAPSHOTTED_BLOCKS`) but none of
///   them call `.input_schema(...)` / `.output_schema(...)` today.
///
/// Every other block in `SNAPSHOTTED_BLOCKS` already has hand-written
/// schemas, so an empty snapshot for any of *them* means the gate is
/// vacuous — wrong prefix, or the block missing from the document's block
/// list — and must fail loudly.
const LEGITIMATELY_EMPTY: &[&str] = &["admin", "tickets"];

fn snapshot_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots")
}

/// Every path in the generated OpenAPI document belonging to `block`,
/// pretty-printed with sorted keys so the output is diff-stable.
fn block_openapi(doc: &serde_json::Value, prefixes: &[&str]) -> String {
    let paths = doc["paths"].as_object().expect("openapi paths object");

    // BTreeMap gives deterministic key ordering regardless of how the
    // generator happened to insert them.
    let filtered: std::collections::BTreeMap<&String, &serde_json::Value> = paths
        .iter()
        .filter(|(path, _)| prefixes.iter().any(|p| path.starts_with(p)))
        .collect();

    serde_json::to_string_pretty(&filtered).expect("serialize block paths")
}

#[tokio::test]
async fn openapi_matches_committed_snapshots() {
    let ctx = impresspress_core::test_support::TestContext::new().await;
    let doc = impresspress_core::test_support::openapi_document(&ctx).await;

    let updating = std::env::var("UPDATE_OPENAPI_SNAPSHOTS").is_ok();
    std::fs::create_dir_all(snapshot_dir()).expect("create snapshot dir");

    let mut failures = Vec::new();

    for (block, prefixes) in SNAPSHOTTED_BLOCKS {
        let actual = block_openapi(&doc, prefixes);
        let path = snapshot_dir().join(format!("{block}.openapi.json"));

        // An empty snapshot for a block that has schema-carrying endpoints
        // means the prefix map is wrong and this block is being "guarded" by
        // a diff that can never change. Only the blocks in
        // `LEGITIMATELY_EMPTY` are exempt.
        if !LEGITIMATELY_EMPTY.contains(block) && actual.trim() == "{}" {
            failures.push(format!(
                "\n=== {block} ===\nEMPTY snapshot. This block's prefixes {prefixes:?} matched no \
                 OpenAPI paths, so its gate is vacuous. Either the prefix map is wrong or the \
                 block is missing from the document's block list."
            ));
            continue;
        }

        if updating || !path.exists() {
            std::fs::write(&path, &actual).expect("write snapshot");
            continue;
        }

        let expected = std::fs::read_to_string(&path).expect("read snapshot");
        if expected != actual {
            failures.push(format!(
                "\n=== {block} ===\nSnapshot differs. Review EVERY changed line:\n\
                 - a new property = the contract widened; decide serde(skip), a view type, or accept\n\
                 - a removed description = editorial text lost; restore it as a /// doc comment\n\
                 Accept with: UPDATE_OPENAPI_SNAPSHOTS=1 cargo test -p impresspress-core --test openapi_snapshot\n\
                 Snapshot: {}",
                path.display()
            ));
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
