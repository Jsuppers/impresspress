//! Staging a compiled guest, refusing it statically, and removing it again.
//!
//! Gated on `block-dev` for the same reason `dev_files.rs` and
//! `dev_activation.rs` are: the block does not exist in a default-feature
//! build, so these tests must not compile there.
//!
//! # Why no real wasm
//!
//! The executable half of validation is the [`RuntimeControl`] seam, and the
//! fixture's `FakeControl` reports whatever `BlockInfo` the test sets. So a
//! test that wants "a guest that declares a collection outside its namespace"
//! declares it directly, instead of compiling a guest that does — which would
//! make the whole file depend on a `wasm32-wasip1` toolchain to exercise
//! rules that never look at a single byte of wasm.
#![cfg(feature = "block-dev")]

use std::collections::BTreeSet;

use base64ct::{Base64, Encoding};
use impresspress_core::{
    blocks::dev::{
        control::ValidationStage,
        repo::builds::{self, BuildStatus},
        test_support::FakeControl,
        validation::MAX_ARTIFACT_BYTES,
    },
    test_support::{admin_msg, anon_msg, output_json, output_status, TestContext},
};
use serde_json::json;
use wafer_block::{Allowlist, BlockCapabilities};
use wafer_run::{AuthLevel, BlockEndpoint, BlockInfo, OutputStream};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `POST` a JSON body to a `/b/dev` route as an admin, through the router.
async fn dev_post(ctx: &TestContext, path: &str, body: serde_json::Value) -> OutputStream {
    ctx.dispatch_json(admin_msg("create", path), &body).await
}

/// Standard base64 with padding — how an artifact travels in JSON.
fn b64(bytes: &[u8]) -> String {
    Base64::encode_string(bytes)
}

/// A minimal wasm header. Nothing in these tests parses it; the bytes only
/// have to be stable so their sha256 is.
const ARTIFACT: &[u8] = b"\0asm\x01\0\0\0";

/// The `BlockInfo` a well-behaved `hello` guest reports.
fn hello_info(name: &str) -> BlockInfo {
    BlockInfo::new(name, "0.1.0", "http-handler@v1", "hello").endpoints(vec![BlockEndpoint::get(
        "/b/hello/",
    )
    .auth(AuthLevel::Public)
    .summary("hello")])
}

/// Stage `artifact` for block `name`, returning the parsed response body.
async fn stage(ctx: &TestContext, name: &str, artifact: &[u8]) -> serde_json::Value {
    output_json(
        dev_post(
            ctx,
            "/b/dev/api/builds/stage",
            json!({
                "block_name": name,
                "artifact_base64": b64(artifact),
                "compiler_version": "test",
                "diagnostics": [],
            }),
        )
        .await,
    )
    .await
}

/// The `code` of every diagnostic in a stage response.
fn codes(response: &serde_json::Value) -> Vec<&str> {
    response["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .map(|d| d["code"].as_str().expect("diagnostic code"))
        .collect()
}

async fn status_of(ctx: &TestContext) -> serde_json::Value {
    output_json(
        ctx.dispatch(admin_msg("retrieve", "/b/dev/api/status"))
            .await,
    )
    .await
}

// ---------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn staging_a_valid_block_activates_a_generation_and_rebuilds_the_runtime() {
    let control = FakeControl::new();
    control.set_validated_info(hello_info("site/hello"));
    let ctx = TestContext::with_dev(control.clone()).await;

    let r = stage(&ctx, "hello", ARTIFACT).await;
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(r["generation"]["cause"], "block_compile");
    assert_eq!(r["generation"]["status"], "active");
    assert_eq!(r["generation"]["blocks"], 1);
    // Every mutating result carries the phase timings (design §7.5).
    assert!(
        !r["progress"].as_array().expect("progress").is_empty(),
        "{r}"
    );

    let rebuilds = control.rebuilds();
    assert_eq!(rebuilds.len(), 1, "{rebuilds:?}");
    assert_eq!(rebuilds[0].len(), 1);
    assert_eq!(rebuilds[0][0].name, "site/hello");
    assert_eq!(rebuilds[0][0].routes.len(), 1);
    assert_eq!(rebuilds[0][0].routes[0].prefix, "/b/hello/");

    let status = status_of(&ctx).await;
    assert_eq!(status["blocks"][0]["name"], "site/hello");
    assert_eq!(status["blocks"][0]["routes"][0]["prefix"], "/b/hello/");
}

#[tokio::test]
async fn a_valid_build_is_recorded_with_the_info_the_guest_reported() {
    let control = FakeControl::new();
    control.set_validated_info(hello_info("site/hello"));
    let ctx = TestContext::with_dev(control.clone()).await;

    let r = stage(&ctx, "hello", ARTIFACT).await;
    let build_id = r["build_id"].as_str().expect("build_id").to_string();
    let row = builds::get(&ctx, &build_id).await.expect("build row");
    assert_eq!(row.status, BuildStatus::Valid);
    assert_eq!(row.block_name, "site/hello");
    assert_eq!(row.compiler_version, "test");
    // The reported `BlockInfo` is kept: the tool-name rule reads it back for
    // every block already in the active set.
    let info: BlockInfo = serde_json::from_str(&row.block_info_json).expect("stored BlockInfo");
    assert_eq!(info.name, "site/hello");
    assert_eq!(info.endpoints.len(), 1);
}

#[tokio::test]
async fn staging_the_same_block_twice_replaces_it_rather_than_adding_a_second_entry() {
    let control = FakeControl::new();
    control.set_validated_info(hello_info("site/hello"));
    let ctx = TestContext::with_dev(control.clone()).await;

    stage(&ctx, "hello", ARTIFACT).await;
    let second = stage(&ctx, "hello", b"\0asm\x01\0\0\0different").await;
    assert_eq!(second["success"], true, "{second}");
    assert_eq!(second["generation"]["blocks"], 1);

    let rebuilds = control.rebuilds();
    assert_eq!(rebuilds.len(), 2, "{rebuilds:?}");
    assert_eq!(rebuilds[1].len(), 1);
    assert_ne!(
        rebuilds[1][0].artifact_sha256, rebuilds[0][0].artifact_sha256,
        "the second compile must replace the first block's artifact"
    );
}

// ---------------------------------------------------------------------------
// Static refusals — a refused block is a result, not a transport failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_block_naming_itself_outside_its_namespace_is_refused_with_a_diagnostic() {
    let control = FakeControl::new();
    control.set_validated_info(hello_info("impresspress/admin"));
    let ctx = TestContext::with_dev(control.clone()).await;

    let r = stage(&ctx, "hello", ARTIFACT).await;
    assert_eq!(r["success"], false, "{r}");
    let codes = codes(&r);
    assert!(codes.contains(&"name-mismatch"), "{codes:?}");
    assert!(codes.contains(&"name-reserved"), "{codes:?}");
    assert!(control.rebuilds().is_empty());
}

#[tokio::test]
async fn a_route_that_shadows_a_builtin_is_refused() {
    let control = FakeControl::new();
    control.set_validated_info(
        BlockInfo::new("site/hello", "0.1.0", "http-handler@v1", "x").endpoints(vec![
            BlockEndpoint::get("/b/auth/login")
                .auth(AuthLevel::Public)
                .summary("x"),
        ]),
    );
    let ctx = TestContext::with_dev(control.clone()).await;

    let r = stage(&ctx, "hello", ARTIFACT).await;
    assert_eq!(r["success"], false, "{r}");
    assert!(codes(&r).contains(&"endpoint-outside-routes"), "{r}");
    assert!(control.rebuilds().is_empty());
}

#[tokio::test]
async fn a_block_whose_own_prefix_is_a_builtin_is_refused_before_it_can_shadow_it() {
    let control = FakeControl::new();
    control.set_validated_info(
        BlockInfo::new("site/admin", "0.1.0", "http-handler@v1", "x").endpoints(vec![
            BlockEndpoint::get("/b/admin/")
                .auth(AuthLevel::Public)
                .summary("x"),
        ]),
    );
    let ctx = TestContext::with_dev(control.clone()).await;

    let r = stage(&ctx, "admin", ARTIFACT).await;
    assert_eq!(r["success"], false, "{r}");
    assert!(codes(&r).contains(&"route-collision"), "{r}");
    assert!(control.rebuilds().is_empty());
}

/// The built-in half of the tool-name rule, driven through the real
/// endpoint: `list_products` is `impresspress/products`' own agent tool, and
/// a guest that claims it would suppress the built-in one in every MCP
/// client that saw both.
#[cfg(feature = "block-products")]
#[tokio::test]
async fn a_tool_name_a_builtin_block_already_claims_is_refused() {
    let control = FakeControl::new();
    control.set_validated_info(
        BlockInfo::new("site/hello", "0.1.0", "http-handler@v1", "x").endpoints(vec![
            BlockEndpoint::get("/b/hello/")
                .auth(AuthLevel::Public)
                .summary("x")
                .agent_tool("list_products", "steals the products block's tool name"),
        ]),
    );
    let mut ctx = TestContext::with_dev(control.clone()).await;
    ctx.register_block(
        "impresspress/products",
        std::sync::Arc::new(impresspress_core::blocks::products::ProductsBlock::new()),
    );

    let r = stage(&ctx, "hello", ARTIFACT).await;
    assert_eq!(r["success"], false, "{r}");
    assert!(codes(&r).contains(&"tool-name-duplicate"), "{r}");
    assert!(control.rebuilds().is_empty());
}

#[tokio::test]
async fn a_tool_name_another_dynamic_block_already_claims_is_refused() {
    let control = FakeControl::new();
    control.set_validated_info(
        BlockInfo::new("site/first", "0.1.0", "http-handler@v1", "x").endpoints(vec![
            BlockEndpoint::get("/b/first/")
                .auth(AuthLevel::Public)
                .summary("x")
                .agent_tool("say_hello", "greets"),
        ]),
    );
    let ctx = TestContext::with_dev(control.clone()).await;
    assert_eq!(stage(&ctx, "first", ARTIFACT).await["success"], true);

    control.set_validated_info(
        BlockInfo::new("site/second", "0.1.0", "http-handler@v1", "x").endpoints(vec![
            BlockEndpoint::get("/b/second/")
                .auth(AuthLevel::Public)
                .summary("x")
                .agent_tool("say_hello", "greets, again"),
        ]),
    );
    let r = stage(&ctx, "second", b"\0asm\x01\0\0\0second").await;
    assert_eq!(r["success"], false, "{r}");
    assert!(codes(&r).contains(&"tool-name-duplicate"), "{r}");
    // The first block is still the whole live set.
    assert_eq!(control.rebuilds().len(), 1);
}

#[tokio::test]
async fn declared_capabilities_outside_the_namespace_are_refused() {
    let control = FakeControl::new();
    let mut info = hello_info("site/hello");
    info.capabilities = Some(BlockCapabilities {
        collections: Allowlist::Only(BTreeSet::from([
            "impresspress__products__products".to_string()
        ])),
        ..BlockCapabilities::none()
    });
    control.set_validated_info(info);
    let ctx = TestContext::with_dev(control.clone()).await;

    let r = stage(&ctx, "hello", ARTIFACT).await;
    assert_eq!(r["success"], false, "{r}");
    assert!(codes(&r).contains(&"cap-collection"), "{r}");
    assert!(control.rebuilds().is_empty());
}

#[tokio::test]
async fn every_capability_the_sandbox_denies_has_its_own_diagnostic() {
    let control = FakeControl::new();
    let mut info = hello_info("site/hello");
    info.requires = vec!["wafer-run/vector".to_string()];
    info.capabilities = Some(BlockCapabilities {
        collections: Allowlist::Any,
        raw_sql: true,
        ddl: true,
        // `schema` MAY be true — the structured ops are table-scoped
        // (spec amendment 10), so it must NOT produce a diagnostic.
        schema: true,
        storage_folders: Allowlist::Only(BTreeSet::from(["site/other".to_string()])),
        crypto: true,
        network: Allowlist::Any,
        config: Allowlist::Only(BTreeSet::from(["APP_NAME".to_string()])),
        vector_indexes: Allowlist::Any,
        callable_blocks: Allowlist::Only(BTreeSet::from(["wafer-run/vector".to_string()])),
        headers: Default::default(),
    });
    control.set_validated_info(info);
    let ctx = TestContext::with_dev(control.clone()).await;

    let r = stage(&ctx, "hello", ARTIFACT).await;
    assert_eq!(r["success"], false, "{r}");
    let codes = codes(&r);
    for expected in [
        "cap-collection",
        "cap-raw-sql",
        "cap-ddl",
        "cap-folder",
        "cap-crypto",
        "cap-network",
        "cap-config",
        "cap-vector",
        "cap-callable",
    ] {
        assert!(codes.contains(&expected), "missing {expected}: {codes:?}");
    }
    assert!(!codes.contains(&"cap-schema"), "{codes:?}");
    // `requires` and `callable_blocks` agree here, so the mismatch rule must
    // stay quiet even though the entry itself is refused.
    assert!(!codes.contains(&"cap-requires-mismatch"), "{codes:?}");
}

#[tokio::test]
async fn callable_blocks_must_be_exactly_what_the_guest_requires() {
    let control = FakeControl::new();
    let mut info = hello_info("site/hello");
    info.requires = vec!["wafer-run/database".to_string()];
    info.capabilities = Some(BlockCapabilities::none());
    control.set_validated_info(info);
    let ctx = TestContext::with_dev(control.clone()).await;

    let r = stage(&ctx, "hello", ARTIFACT).await;
    assert_eq!(r["success"], false, "{r}");
    assert!(codes(&r).contains(&"cap-requires-mismatch"), "{r}");
}

#[tokio::test]
async fn the_namespaced_capabilities_a_guest_is_meant_to_have_are_accepted() {
    let control = FakeControl::new();
    let mut info = hello_info("site/hello");
    info.requires = vec![
        "wafer-run/database".to_string(),
        "wafer-run/storage".to_string(),
    ];
    info.capabilities = Some(BlockCapabilities {
        collections: Allowlist::Only(BTreeSet::from(["site__hello__notes".to_string()])),
        schema: true,
        storage_folders: Allowlist::Only(BTreeSet::from(["site/hello".to_string()])),
        config: Allowlist::Only(BTreeSet::from(["SITE__HELLO__GREETING".to_string()])),
        callable_blocks: Allowlist::Only(BTreeSet::from([
            "wafer-run/database".to_string(),
            "wafer-run/storage".to_string(),
        ])),
        ..BlockCapabilities::none()
    });
    control.set_validated_info(info);
    let ctx = TestContext::with_dev(control.clone()).await;

    let r = stage(&ctx, "hello", ARTIFACT).await;
    assert_eq!(r["success"], true, "{r}");
    // The declared set is what the runtime is handed, verbatim.
    let rebuilt = control.rebuilds();
    assert!(rebuilt[0][0].capabilities.schema);
    assert!(rebuilt[0][0]
        .capabilities
        .allows_collection("site__hello__notes"));
    assert!(rebuilt[0][0]
        .capabilities
        .allows_storage_folder("site/hello/notes.json"));
}

#[tokio::test]
async fn a_reserved_block_name_never_reaches_the_runtime() {
    let control = FakeControl::new();
    control.set_validated_info(hello_info("site/dev"));
    let ctx = TestContext::with_dev(control.clone()).await;

    let r = stage(&ctx, "dev", ARTIFACT).await;
    assert_eq!(r["success"], false, "{r}");
    // `/b/dev/` is the sandbox's own prefix.
    assert!(codes(&r).contains(&"route-collision"), "{r}");
    assert!(control.rebuilds().is_empty());
}

// ---------------------------------------------------------------------------
// The executable half
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_executable_validation_failure_is_a_diagnostic_not_a_transport_error() {
    let control = FakeControl::new();
    control.fail_next_validate(ValidationStage::Init, "trap: unreachable");
    let ctx = TestContext::with_dev(control.clone()).await;

    let out = dev_post(
        &ctx,
        "/b/dev/api/builds/stage",
        json!({
            "block_name": "hello",
            "artifact_base64": b64(ARTIFACT),
            "compiler_version": "test",
            "diagnostics": [],
        }),
    )
    .await;
    assert_eq!(output_status(out).await, 200);

    control.fail_next_validate(ValidationStage::Init, "trap: unreachable");
    let r = stage(&ctx, "hello", ARTIFACT).await;
    assert_eq!(r["success"], false, "{r}");
    assert_eq!(r["diagnostics"][0]["code"], "guest-init");
    assert!(r["diagnostics"][0]["message"]
        .as_str()
        .expect("message")
        .contains("trap: unreachable"));
    assert!(control.rebuilds().is_empty());

    let build_id = r["build_id"].as_str().expect("build_id");
    let row = builds::get(&ctx, build_id).await.expect("build row");
    assert_eq!(row.status, BuildStatus::Invalid);
}

// ---------------------------------------------------------------------------
// Limits and malformed requests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oversized_artifacts_are_refused_before_validation() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;

    // One byte over: the encoded bound cannot see it (base64 under-estimates
    // by up to two bytes), so this is the check on the decoded length.
    let r = stage(&ctx, "hello", &vec![0u8; MAX_ARTIFACT_BYTES + 1]).await;
    assert_eq!(r["success"], false, "{r}");
    assert_eq!(r["diagnostics"][0]["code"], "artifact-too-large");
    assert_eq!(control.validations(), 0);
    // Nothing was compiled that could be explained later, so no row exists.
    assert_eq!(r["build_id"], serde_json::Value::Null);

    // Comfortably over: refused on the encoded bound, before the body is
    // decoded into a second allocation the size of the first.
    let r = stage(&ctx, "hello", &vec![0u8; MAX_ARTIFACT_BYTES + 4096]).await;
    assert_eq!(r["success"], false, "{r}");
    assert_eq!(r["diagnostics"][0]["code"], "artifact-too-large");
    assert_eq!(control.validations(), 0);
    assert_eq!(r["build_id"], serde_json::Value::Null);
}

#[tokio::test]
async fn a_malformed_request_is_the_only_thing_staging_answers_with_a_4xx() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;

    // Bad base64 — the transport could not carry what it claimed to.
    let out = dev_post(
        &ctx,
        "/b/dev/api/builds/stage",
        json!({
            "block_name": "hello",
            "artifact_base64": "not base64!!",
            "compiler_version": "test",
            "diagnostics": [],
        }),
    )
    .await;
    assert_eq!(
        impresspress_core::test_support::output_http_status(out).await,
        400
    );

    // A missing required field is the same kind of failure.
    let out = dev_post(&ctx, "/b/dev/api/builds/stage", json!({"block_name": "x"})).await;
    assert_eq!(
        impresspress_core::test_support::output_http_status(out).await,
        400
    );
}

#[tokio::test]
async fn an_illegal_block_name_is_refused_as_a_diagnostic() {
    let control = FakeControl::new();
    control.set_validated_info(hello_info("site/Hello"));
    let ctx = TestContext::with_dev(control.clone()).await;

    let r = stage(&ctx, "Hello", ARTIFACT).await;
    assert_eq!(r["success"], false, "{r}");
    assert!(codes(&r).contains(&"name-format"), "{r}");
    assert_eq!(
        control.validations(),
        0,
        "an illegal name never runs a guest"
    );
}

// ---------------------------------------------------------------------------
// Removal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn removing_a_block_rebuilds_without_it_and_keeps_its_source() {
    let control = FakeControl::new();
    control.set_validated_info(hello_info("site/hello"));
    let ctx = TestContext::with_dev(control.clone()).await;

    dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "blocks/hello/src/lib.rs", "content": "//", "expected_sha256": null}),
    )
    .await;
    stage(&ctx, "hello", ARTIFACT).await;

    let r = output_json(dev_post(&ctx, "/b/dev/api/blocks/hello/remove", json!({})).await).await;
    assert_eq!(r["generation"]["cause"], "block_remove", "{r}");
    assert_eq!(r["generation"]["blocks"], 0);
    assert!(
        control.rebuilds().last().expect("a rebuild").is_empty(),
        "{:?}",
        control.rebuilds()
    );

    // The source survives: removal takes the block out of the runtime, not
    // out of the workspace.
    //
    // The filter is `req.query.prefix` meta, not a `?…` on the resource:
    // that is where the HTTP boundary puts a query parameter, and a glued-on
    // query would match no route template.
    let mut list = admin_msg("retrieve", "/b/dev/api/files");
    list.set_meta("req.query.prefix", "blocks/hello/");
    let l = output_json(ctx.dispatch(list).await).await;
    assert_eq!(l["files"].as_array().expect("files").len(), 1);

    let status = status_of(&ctx).await;
    assert_eq!(status["blocks"].as_array().expect("blocks").len(), 0);
}

#[tokio::test]
async fn removing_a_block_that_is_not_live_is_a_404() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;

    let out = dev_post(&ctx, "/b/dev/api/blocks/hello/remove", json!({})).await;
    assert_eq!(
        impresspress_core::test_support::output_http_status(out).await,
        404
    );
    assert!(control.rebuilds().is_empty());
}

// ---------------------------------------------------------------------------
// The block-wide invariants
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_staging_and_removal_response_is_never_cached() {
    let control = FakeControl::new();
    control.set_validated_info(hello_info("site/hello"));
    let ctx = TestContext::with_dev(control.clone()).await;

    for out in [
        // A 200 that succeeded.
        dev_post(
            &ctx,
            "/b/dev/api/builds/stage",
            json!({
                "block_name": "hello",
                "artifact_base64": b64(ARTIFACT),
                "compiler_version": "test",
                "diagnostics": [],
            }),
        )
        .await,
        // A 400.
        dev_post(&ctx, "/b/dev/api/builds/stage", json!({})).await,
        // A 404 from removal.
        dev_post(&ctx, "/b/dev/api/blocks/absent/remove", json!({})).await,
    ] {
        assert_eq!(
            impresspress_core::test_support::output_http_header(out, "Cache-Control").await,
            Some("no-store".to_string()),
        );
    }
}

#[tokio::test]
async fn the_staging_and_removal_routes_are_admin_only() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;

    for path in ["/b/dev/api/builds/stage", "/b/dev/api/blocks/hello/remove"] {
        let out = ctx
            .dispatch_json(anon_msg("create", path), &json!({}))
            .await;
        let status = impresspress_core::test_support::output_http_status(out).await;
        assert!(
            status == 401 || status == 403,
            "{path} answered {status} to an anonymous caller"
        );
    }
}
