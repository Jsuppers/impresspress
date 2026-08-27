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
/// * `tickets` is a block that arrived recently and has no schemas yet —
///   its endpoints exist (see `SNAPSHOTTED_BLOCKS`) but none of them call
///   `.input_schema(...)` / `.output_schema(...)` today.
///
/// `admin` left this list when its four JSON API reads were typed: its
/// handlers now build `blocks::admin::contracts` types, so the block has a
/// non-empty snapshot to guard.
///
/// Every other block in `SNAPSHOTTED_BLOCKS` already has schemas, so an
/// empty snapshot for any of *them* means the gate is vacuous — wrong
/// prefix, or the block missing from the document's block list — and must
/// fail loudly.
const LEGITIMATELY_EMPTY: &[&str] = &["tickets"];

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

/// `admin` shipped 17 endpoints and zero schemas: its handlers returned the
/// database layer's untyped `RecordList` / `HashMap<String, Value>` directly,
/// so `has_schema()` filtered every one of them out and the whole JSON API was
/// absent from `/openapi.json`. These four reads are the ones that carry a
/// contract; the other thirteen serve HTML and must stay absent.
#[tokio::test]
async fn admin_json_api_appears_in_openapi() {
    let ctx = impresspress_core::test_support::TestContext::new().await;
    let doc = impresspress_core::test_support::openapi_document(&ctx).await;

    for path in [
        "/b/admin/api/users",
        "/b/admin/api/iam/roles",
        "/b/admin/api/settings",
        "/b/admin/api/logs",
    ] {
        assert!(
            !doc["paths"][path]["get"].is_null(),
            "{path} must carry a schema and appear in /openapi.json - admin's \
             JSON API was previously invisible because its handlers were untyped"
        );
        assert_eq!(
            doc["paths"][path]["get"]["security"],
            serde_json::json!([{ "bearerAuth": [] }]),
            "{path} is AuthLevel::Admin and must carry a security requirement"
        );
    }
}

/// Every field name `block` publishes: the keys of every `properties` object
/// anywhere in its schemas, plus the `name` of every declared parameter.
///
/// Names, not prose. A description that explains which column is deliberately
/// withheld is documentation; a `properties` key with the same text is a
/// published field. Only the second is a leak, so only the second is checked.
fn published_field_names(doc: &serde_json::Value, prefixes: &[&str]) -> Vec<String> {
    fn walk(node: &serde_json::Value, out: &mut Vec<String>) {
        match node {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::Object(props)) = map.get("properties") {
                    out.extend(props.keys().cloned());
                }
                if let Some(serde_json::Value::Array(params)) = map.get("parameters") {
                    out.extend(
                        params
                            .iter()
                            .filter_map(|p| p.get("name")?.as_str().map(str::to_string)),
                    );
                }
                for value in map.values() {
                    walk(value, out);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(item, out);
                }
            }
            _ => {}
        }
    }

    let paths = doc["paths"].as_object().expect("openapi paths object");
    let mut out = Vec::new();
    for (path, node) in paths {
        if prefixes.iter().any(|p| path.starts_with(p)) {
            walk(node, &mut out);
        }
    }
    out
}

/// The admin block is the most sensitive surface in the document, and this is
/// the first release in which any of it is publicly described. Its handlers
/// read `wafer_run__auth__users` and `impresspress__admin__variables`, tables
/// that hold credential material, so every published field must be an explicit
/// projection rather than an echoed row.
#[tokio::test]
async fn admin_openapi_publishes_no_credential_field() {
    let ctx = impresspress_core::test_support::TestContext::new().await;
    let doc = impresspress_core::test_support::openapi_document(&ctx).await;
    let fields = published_field_names(&doc, &["/b/admin"]);

    assert!(
        !fields.is_empty(),
        "no admin fields found - the walk is looking in the wrong place and this \
         test would pass forever"
    );

    for field in &fields {
        let lower = field.to_lowercase();
        for forbidden in [
            "password",
            "verification_token",
            "token_hash",
            "access_token",
            "refresh_token",
            "session_token",
            "secret",
            "hash",
        ] {
            assert!(
                !lower.contains(forbidden),
                "admin publishes a field named `{field}`, which matches the \
                 credential pattern `{forbidden}`"
            );
        }
    }
}
