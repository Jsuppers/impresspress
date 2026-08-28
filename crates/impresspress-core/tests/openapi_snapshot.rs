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
    ("llm", &["/b/llm"]),
    ("vector", &["/b/vector"]),
];

/// Blocks that legitimately have no schema-carrying endpoints yet, so an
/// empty snapshot for them is correct rather than a sign the prefix map or
/// the document's block list is wrong.
///
/// `vector` (11 endpoints) declares no schemas at all: its handlers
/// deserialize into private in-function structs and answer with
/// `serde_json::json!` literals, so `has_schema()` filters every one of them
/// out. It is listed here so the committed baseline is the pre-typing state;
/// it leaves the list as its handlers gain `contracts` types, exactly as
/// `admin`, `tickets` and `llm` did.
///
/// Every other block in `SNAPSHOTTED_BLOCKS` has schemas, so an empty
/// snapshot for any of them means the gate is vacuous — wrong prefix, or the
/// block missing from the document's block list — and must fail loudly.
const LEGITIMATELY_EMPTY: &[&str] = &["vector"];

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
/// absent from `/openapi.json`. These four reads and three role writes are the
/// ones that carry a contract; the HTML pages must stay absent.
#[tokio::test]
async fn admin_json_api_appears_in_openapi() {
    let ctx = impresspress_core::test_support::TestContext::new().await;
    let doc = impresspress_core::test_support::openapi_document(&ctx).await;

    for (path, method) in [
        ("/b/admin/api/users", "get"),
        ("/b/admin/api/iam/roles", "get"),
        ("/b/admin/api/iam/roles", "post"),
        ("/b/admin/api/iam/roles/{id}", "patch"),
        ("/b/admin/api/iam/roles/{id}", "delete"),
        ("/b/admin/api/settings", "get"),
        ("/b/admin/api/logs", "get"),
    ] {
        assert!(
            !doc["paths"][path][method].is_null(),
            "{method} {path} must carry a schema and appear in /openapi.json - admin's \
             JSON API was previously invisible because its handlers were untyped"
        );
        assert_eq!(
            doc["paths"][path][method]["security"],
            serde_json::json!([{ "bearerAuth": [] }]),
            "{method} {path} is AuthLevel::Admin and must carry a security requirement"
        );
    }

    // The three role writes publish the list's row projection, so a consumer
    // reading one role from any of them can rely on one shape.
    let list_row = &doc["paths"]["/b/admin/api/iam/roles"]["get"]["responses"]["200"]["content"]
        ["application/json"]["schema"]["properties"]["records"]["items"];
    for (path, method) in [
        ("/b/admin/api/iam/roles", "post"),
        ("/b/admin/api/iam/roles/{id}", "patch"),
    ] {
        let written = &doc["paths"][path][method]["responses"]["200"]["content"]
            ["application/json"]["schema"];
        assert_eq!(
            written["properties"], list_row["properties"],
            "{method} {path} must publish the same row projection as the list"
        );
        assert_eq!(
            written["required"], list_row["required"],
            "{method} {path} must publish the same row projection as the list"
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

/// `llm` declared 18 endpoints and zero schemas: every handler deserialized
/// into a private in-function struct and answered with a `serde_json::json!`
/// literal, so `has_schema()` filtered all of them out. The thirteen JSON
/// endpoints below are the ones that carry a contract; the five HTML pages
/// must stay absent, because a schema is what turns an endpoint into a tool.
#[tokio::test]
async fn llm_json_api_appears_in_openapi_and_its_pages_do_not() {
    let ctx = impresspress_core::test_support::TestContext::new().await;
    let doc = impresspress_core::test_support::openapi_document(&ctx).await;

    for (method, path) in [
        ("post", "/b/llm/api/chat"),
        ("post", "/b/llm/api/chat/stream"),
        ("get", "/b/llm/api/providers"),
        ("post", "/b/llm/api/providers"),
        ("patch", "/b/llm/api/providers/{id}"),
        ("delete", "/b/llm/api/providers/{id}"),
        ("post", "/b/llm/api/providers/{id}/discover-models"),
        ("get", "/b/llm/api/models"),
        ("get", "/b/llm/api/models/{backend_id}/{model_id}/status"),
        ("post", "/b/llm/api/models/{backend_id}/{model_id}/load"),
        ("post", "/b/llm/api/models/{backend_id}/{model_id}/unload"),
        ("get", "/b/llm/api/config"),
        ("post", "/b/llm/api/config"),
    ] {
        assert!(
            !doc["paths"][path][method].is_null(),
            "{method} {path} must carry a schema and appear in /openapi.json"
        );
        assert_eq!(
            doc["paths"][path][method]["security"],
            serde_json::json!([{ "bearerAuth": [] }]),
            "{method} {path} is Authenticated or Admin and must carry a security requirement"
        );
    }

    for path in [
        "/b/llm/",
        "/b/llm/threads/{id}",
        "/b/llm/settings",
        "/b/llm/providers",
        "/b/llm/models",
    ] {
        assert!(
            doc["paths"][path].is_null(),
            "{path} serves HTML and must carry no schema - a schema would make it \
             a callable tool"
        );
    }

    // The two SSE endpoints answer with `text/event-stream`, one frame per
    // `ChatChunk` / `LoadProgress`. An `application/json` response schema
    // there would describe a body that is never sent, so they carry their
    // request/path contract only.
    for (method, path) in [
        ("post", "/b/llm/api/chat/stream"),
        ("post", "/b/llm/api/models/{backend_id}/{model_id}/load"),
    ] {
        assert!(
            doc["paths"][path][method]["responses"]["200"]["content"].is_null(),
            "{method} {path} streams SSE and must not publish a JSON response schema"
        );
    }
    assert!(
        !doc["paths"]["/b/llm/api/chat/stream"]["post"]["requestBody"].is_null(),
        "the streaming chat endpoint takes the same body as the buffered one and must say so"
    );

    // Both chat endpoints deserialize the same struct, so they must publish
    // the same request schema; a divergence would mean one of them stopped
    // going through the shared prelude.
    assert_eq!(
        doc["paths"]["/b/llm/api/chat"]["post"]["requestBody"],
        doc["paths"]["/b/llm/api/chat/stream"]["post"]["requestBody"],
        "chat and chat/stream must publish one request contract"
    );

    // The three provider writes publish the list's row projection, so a
    // consumer reading one provider from any of them can rely on one shape.
    let list_row = &doc["paths"]["/b/llm/api/providers"]["get"]["responses"]["200"]["content"]
        ["application/json"]["schema"]["properties"]["providers"]["items"];
    assert!(
        !list_row["properties"].is_null(),
        "the provider list must publish a row projection: {doc}"
    );
    for (method, path) in [
        ("post", "/b/llm/api/providers"),
        ("patch", "/b/llm/api/providers/{id}"),
    ] {
        let written = &doc["paths"][path][method]["responses"]["200"]["content"]
            ["application/json"]["schema"];
        assert_eq!(
            written["properties"], list_row["properties"],
            "{method} {path} must publish the same row projection as the list"
        );
        assert_eq!(
            written["required"], list_row["required"],
            "{method} {path} must publish the same row projection as the list"
        );
    }
}

/// Provider rows reference an API key by the *name* of the admin variable
/// that holds it (`key_var`); the key itself is resolved into the in-memory
/// router at reload time and lives on the very `ProviderConfig` the handlers
/// hold. Every published field must therefore be an explicit projection, and
/// this pins two things structurally:
///
/// 1. No published field name matches a credential pattern. `api_key` is the
///    one this block could leak; the rest are the shared list.
/// 2. `key_var` is the only `key`-bearing name, and it is a plain string (a
///    variable *name*), never an object that could carry a value.
#[tokio::test]
async fn llm_openapi_publishes_no_credential_field() {
    let ctx = impresspress_core::test_support::TestContext::new().await;
    let doc = impresspress_core::test_support::openapi_document(&ctx).await;
    let fields = published_field_names(&doc, &["/b/llm"]);

    assert!(
        !fields.is_empty(),
        "no llm fields found - the walk is looking in the wrong place and this \
         test would pass forever"
    );

    let mut saw_key_var = false;
    for field in &fields {
        let lower = field.to_lowercase();
        for forbidden in [
            "api_key",
            "apikey",
            "secret",
            "password",
            "hash",
            "access_token",
            "refresh_token",
            "session_token",
            "bearer",
            "credential",
        ] {
            assert!(
                !lower.contains(forbidden),
                "llm publishes a field named `{field}`, which matches the \
                 credential pattern `{forbidden}`"
            );
        }
        if lower.contains("key") {
            assert_eq!(
                field, "key_var",
                "the only key-bearing name llm may publish is `key_var`, the \
                 admin variable *name*; got `{field}`"
            );
            saw_key_var = true;
        }
    }
    assert!(
        saw_key_var,
        "`key_var` must be published (it is how an admin sees which variable a \
         provider reads) - its absence means the walk missed the provider view"
    );

    for props in objects_with_property(&doc, &["/b/llm"], "responses", "key_var") {
        assert_eq!(
            props["key_var"]["type"],
            serde_json::json!(["string", "null"]),
            "`key_var` must stay a nullable string naming a variable, never a \
             value-carrying object: {}",
            props["key_var"]
        );
    }
}

/// `tickets` declared 21 endpoints and zero schemas, so `has_schema()` filtered
/// every one of them out and its JSON API was absent from `/openapi.json`. The
/// thirteen JSON endpoints below are the ones that carry a contract; the eight
/// that serve HTML (or redirect) must stay absent, because a schema is what
/// turns an endpoint into a tool.
#[tokio::test]
async fn tickets_json_api_appears_in_openapi_and_its_pages_do_not() {
    let ctx = impresspress_core::test_support::TestContext::new().await;
    let doc = impresspress_core::test_support::openapi_document(&ctx).await;

    for (method, path) in [
        ("post", "/b/tickets/api/submissions"),
        ("get", "/b/tickets/api/admin/tickets"),
        ("post", "/b/tickets/api/admin/tickets"),
        ("get", "/b/tickets/api/admin/tickets/{id}"),
        ("patch", "/b/tickets/api/admin/tickets/{id}"),
        ("post", "/b/tickets/api/admin/tickets/{id}/notes"),
        ("get", "/b/tickets/api/admin/tickets/{id}/analyses"),
        ("post", "/b/tickets/api/admin/tickets/{id}/analyses"),
        ("get", "/b/tickets/api/admin/types"),
        ("post", "/b/tickets/api/admin/types"),
        ("patch", "/b/tickets/api/admin/types/{id}"),
        ("get", "/b/tickets/api/admin/status"),
        ("post", "/b/tickets/api/admin/retention/prune"),
    ] {
        assert!(
            !doc["paths"][path][method].is_null(),
            "{method} {path} must carry a schema and appear in /openapi.json"
        );
    }

    for path in [
        "/b/tickets/submit",
        "/b/tickets/submitted",
        "/b/tickets/admin",
        "/b/tickets/admin/tickets",
        "/b/tickets/admin/tickets/{id}",
        "/b/tickets/admin/types",
        "/b/tickets/admin/settings",
        "/b/tickets/admin/endpoints",
    ] {
        assert!(
            doc["paths"][path].is_null(),
            "{path} serves HTML and must carry no schema - a schema would make it \
             a callable tool"
        );
    }
}

/// The ticket tables hold reporter-supplied text and one abuse digest, and the
/// untyped handlers echoed whole rows. Two invariants the projection exists to
/// hold, checked structurally rather than by reading the snapshot:
///
/// 1. No published field name matches the digest/credential pattern. That is
///    what keeps `dedupe_hash` — an HMAC over the reporter's IP-derived
///    rotating identity — out of every response.
/// 2. Reporter-controlled text is reachable only inside an untrusted-report
///    group, never flattened beside the workflow fields, so an agent client can
///    always tell data from instruction.
#[tokio::test]
async fn tickets_openapi_withholds_the_abuse_digest_and_groups_reporter_text() {
    let ctx = impresspress_core::test_support::TestContext::new().await;
    let doc = impresspress_core::test_support::openapi_document(&ctx).await;
    let fields = published_field_names(&doc, &["/b/tickets"]);

    assert!(
        !fields.is_empty(),
        "no tickets fields found - the walk is looking in the wrong place and \
         this test would pass forever"
    );

    // `…_secret_configured` on the readiness object is a boolean that reports
    // whether a secret is set, never a secret; it is the only field allowed to
    // match the pattern below.
    const READINESS_FLAGS: &[&str] = &["turnstile_secret_configured", "identity_secret_configured"];

    for field in &fields {
        if READINESS_FLAGS.contains(&field.as_str()) {
            continue;
        }
        let lower = field.to_lowercase();
        for forbidden in ["dedupe", "hash", "secret", "password", "remote_addr"] {
            assert!(
                !lower.contains(forbidden),
                "tickets publishes a field named `{field}`, which matches the \
                 withheld pattern `{forbidden}`"
            );
        }
    }

    for flag in READINESS_FLAGS {
        for props in objects_with_property(&doc, &["/b/tickets"], "responses", flag) {
            assert_eq!(
                props[*flag]["type"], "boolean",
                "`{flag}` must stay a boolean readiness flag, never carry a value"
            );
        }
    }

    // `reporter_email` is the marker: it belongs to exactly one shape, the
    // reporter-controlled group. Any other object carrying it would mean a raw
    // ticket row had been echoed again.
    let mut groups = 0;
    for props in objects_with_property(&doc, &["/b/tickets"], "responses", "reporter_email") {
        groups += 1;
        let mut keys: Vec<&str> = props.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "description",
                "evidence_url",
                "reporter_email",
                "reporter_wants_reply",
                "source_path",
                "subject",
                "subject_id",
                "subject_type",
            ],
            "reporter text must appear only as the untrusted-report group, but it \
             is flattened into an object with these keys: {keys:?}"
        );
    }
    assert!(
        groups > 0,
        "no untrusted-report group found - the walk is looking in the wrong place \
         and this test would pass forever"
    );
}

/// Every `properties` map declaring `property` inside the `section` (e.g.
/// `"responses"`) of an operation under `prefixes`.
///
/// The section matters: a *request* body legitimately carries reporter text
/// flat - that is the reporter filling in the form - while a *response* that
/// does so is an echoed row.
fn objects_with_property<'a>(
    doc: &'a serde_json::Value,
    prefixes: &[&str],
    section: &str,
    property: &str,
) -> Vec<&'a serde_json::Map<String, serde_json::Value>> {
    fn walk<'a>(
        node: &'a serde_json::Value,
        property: &str,
        out: &mut Vec<&'a serde_json::Map<String, serde_json::Value>>,
    ) {
        match node {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::Object(props)) = map.get("properties") {
                    if props.contains_key(property) {
                        out.push(props);
                    }
                }
                for value in map.values() {
                    walk(value, property, out);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(item, property, out);
                }
            }
            _ => {}
        }
    }

    let paths = doc["paths"].as_object().expect("openapi paths object");
    let mut out = Vec::new();
    for (path, node) in paths {
        if !prefixes.iter().any(|p| path.starts_with(p)) {
            continue;
        }
        for operation in node.as_object().into_iter().flat_map(|ops| ops.values()) {
            walk(&operation[section], property, &mut out);
        }
    }
    out
}

/// The public submission form carries a honeypot field. It works because a
/// bot that fills every field it can see fills that one too — which means it
/// must not be a field the schema can see. `deny_unknown_fields` on the JSON
/// contract forced it to be declared ("website — Must be left empty"),
/// publishing the bypass to every schema-reading caller.
#[tokio::test]
async fn tickets_openapi_does_not_publish_the_honeypot() {
    let ctx = impresspress_core::test_support::TestContext::new().await;
    let doc = impresspress_core::test_support::openapi_document(&ctx).await;

    let request = &doc["paths"]["/b/tickets/api/submissions"]["post"]["requestBody"]["content"]
        ["application/json"]["schema"];
    assert!(
        !request["properties"].is_null(),
        "the public submission request must stay documented: {doc}"
    );
    assert!(
        request["properties"]["website"].is_null(),
        "the honeypot must not be a published property: {request}"
    );
    assert_ne!(
        request["additionalProperties"],
        serde_json::json!(false),
        "unknown keys must be tolerated on this input or the honeypot cannot be read: {request}"
    );
}
