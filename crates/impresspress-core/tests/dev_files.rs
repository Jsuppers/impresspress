//! The dev sandbox's workspace store — `/b/dev/api/files*`.
//!
//! Gated on `block-dev` for the same reason `dev_status.rs` is: the block does
//! not exist in a default-feature build, so these tests must not compile
//! there.
#![cfg(feature = "block-dev")]

use base64ct::{Base64, Encoding};
use impresspress_core::{
    blocks::dev::{blobs, paths, test_support::FakeControl, DevBlock, DevShared},
    test_support::{
        admin_msg, anon_msg, auth_msg, output_http_header, output_http_status, output_json,
        output_status, TestContext,
    },
};
use serde_json::json;
use wafer_run::{Block as _, Message, OutputStream};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `POST` a JSON body to a `/b/dev` route as an admin, through the router.
async fn dev_post(ctx: &TestContext, path: &str, body: serde_json::Value) -> OutputStream {
    ctx.dispatch_json(admin_msg("create", path), &body).await
}

/// `GET /b/dev/api/files`, optionally with a `?prefix=` filter.
///
/// The filter is set as `req.query.prefix` meta rather than appended to the
/// resource path: that is where the HTTP boundary puts a query parameter
/// (`wafer_block::http_codec` splits the URI once, into `req.resource` +
/// `req.query.*`), and it is where `Message::query` reads it back. A `?…`
/// glued onto the resource would not match any route template.
fn list_msg(prefix: Option<&str>) -> Message {
    let mut msg = admin_msg("retrieve", "/b/dev/api/files");
    if let Some(prefix) = prefix {
        msg.set_meta("req.query.prefix", prefix);
    }
    msg
}

/// Write `content` (utf8) at `path` as a new file and return its sha256.
async fn write_new(ctx: &TestContext, path: &str, content: &str) -> String {
    let out = dev_post(
        ctx,
        "/b/dev/api/files/write",
        json!({"path": path, "content": content, "expected_sha256": null}),
    )
    .await;
    let body = output_json(out).await;
    body["sha256"]
        .as_str()
        .unwrap_or_else(|| panic!("write {path} did not return a sha256: {body}"))
        .to_string()
}

// ---------------------------------------------------------------------------
// The round trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_then_list_then_read_round_trips_with_hashes() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let w = output_json(
        dev_post(
            &ctx,
            "/b/dev/api/files/write",
            json!({"path": "site/index.html", "content": "<h1>hi</h1>", "expected_sha256": null}),
        )
        .await,
    )
    .await;
    assert_eq!(w["path"], "site/index.html");
    assert_eq!(w["size"], 11);
    // Task 6 stages nothing: activation (and the generation it summarizes)
    // arrives in Task 7.
    assert_eq!(w["generation"], serde_json::Value::Null);
    let sha = w["sha256"].as_str().expect("sha256").to_string();
    assert_eq!(sha.len(), 64);

    let l = output_json(ctx.dispatch(list_msg(Some("site/"))).await).await;
    assert_eq!(l["files"][0]["path"], "site/index.html");
    assert_eq!(l["files"][0]["sha256"], serde_json::json!(sha));
    assert_eq!(l["files"][0]["size"], 11);
    assert_eq!(l["files"][0]["content_type"], "text/html; charset=utf-8");

    let r = output_json(
        dev_post(
            &ctx,
            "/b/dev/api/files/read",
            json!({"path": "site/index.html"}),
        )
        .await,
    )
    .await;
    assert_eq!(r["encoding"], "utf8");
    assert_eq!(r["content"], "<h1>hi</h1>");
    assert_eq!(r["sha256"], serde_json::json!(sha));
    assert_eq!(r["size"], 11);
}

#[tokio::test]
async fn the_hash_a_write_returns_is_the_sha256_of_the_bytes() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let sha = write_new(&ctx, "site/index.html", "<h1>hi</h1>").await;
    assert_eq!(sha, blobs::sha256_hex(b"<h1>hi</h1>"));
}

// ---------------------------------------------------------------------------
// Optimistic concurrency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_requires_null_expected_hash_for_a_new_file() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let out = dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/a.css", "content": "a{}", "expected_sha256": "00"}),
    )
    .await;
    assert_eq!(output_status(out).await, 409);
}

#[tokio::test]
async fn write_over_an_existing_file_requires_its_hash() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    write_new(&ctx, "site/a.css", "a{}").await;

    // `null` now means "I believe this file does not exist" — and it does.
    let out = dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/a.css", "content": "b{}", "expected_sha256": null}),
    )
    .await;
    assert_eq!(output_status(out).await, 409);
}

#[tokio::test]
async fn stale_hash_is_a_conflict_that_reports_the_current_hash() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let current = write_new(&ctx, "site/a.css", "a{}").await;

    let out = dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/a.css", "content": "b{}", "expected_sha256": "deadbeef"}),
    )
    .await;
    assert_eq!(output_status(out).await, 409);

    // Re-dispatch to read the body: draining the stream above consumed it.
    let body = output_json(
        dev_post(
            &ctx,
            "/b/dev/api/files/write",
            json!({"path": "site/a.css", "content": "b{}", "expected_sha256": "deadbeef"}),
        )
        .await,
    )
    .await;
    assert_eq!(body["path"], "site/a.css");
    assert_eq!(body["current_sha256"], serde_json::json!(current));
    assert_eq!(body["current_size"], 3);

    // The refused write must not have landed.
    let r =
        output_json(dev_post(&ctx, "/b/dev/api/files/read", json!({"path": "site/a.css"})).await)
            .await;
    assert_eq!(r["content"], "a{}");
}

#[tokio::test]
async fn a_matching_hash_replaces_the_content() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let first = write_new(&ctx, "site/a.css", "a{}").await;

    let w = output_json(
        dev_post(
            &ctx,
            "/b/dev/api/files/write",
            json!({"path": "site/a.css", "content": "b{}", "expected_sha256": first}),
        )
        .await,
    )
    .await;
    assert_eq!(w["sha256"], serde_json::json!(blobs::sha256_hex(b"b{}")));

    let r =
        output_json(dev_post(&ctx, "/b/dev/api/files/read", json!({"path": "site/a.css"})).await)
            .await;
    assert_eq!(r["content"], "b{}");

    // Content is never edited in place: the superseded blob is still there,
    // which is what makes a generation that references it replayable.
    assert_eq!(
        blobs::get(&ctx, &first).await.expect("first blob survives"),
        b"a{}".to_vec()
    );
}

// ---------------------------------------------------------------------------
// Encodings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn binary_files_round_trip_as_base64() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let raw = [0x89u8, b'P', b'N', b'G', 0, 1, 2];
    let png = Base64::encode_string(&raw);
    let out = dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/assets/dot.png", "content": png, "encoding": "base64", "expected_sha256": null}),
    )
    .await;
    let w = output_json(out).await;
    // The recorded size and hash are of the DECODED bytes, not the envelope.
    assert_eq!(w["size"], raw.len());
    assert_eq!(w["sha256"], serde_json::json!(blobs::sha256_hex(&raw)));

    let r = output_json(
        dev_post(
            &ctx,
            "/b/dev/api/files/read",
            json!({"path": "site/assets/dot.png"}),
        )
        .await,
    )
    .await;
    assert_eq!(r["encoding"], "base64");
    assert_eq!(r["content"], serde_json::json!(png));
}

#[tokio::test]
async fn malformed_base64_is_rejected() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let out = dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/assets/dot.png", "content": "not base64!!", "encoding": "base64", "expected_sha256": null}),
    )
    .await;
    assert_eq!(output_http_status(out).await, 400);
}

#[tokio::test]
async fn block_sources_read_back_as_utf8_text() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    write_new(&ctx, "blocks/hello/src/lib.rs", "fn main() {}").await;
    let r = output_json(
        dev_post(
            &ctx,
            "/b/dev/api/files/read",
            json!({"path": "blocks/hello/src/lib.rs"}),
        )
        .await,
    )
    .await;
    assert_eq!(r["encoding"], "utf8");
    assert_eq!(r["content"], "fn main() {}");
}

// ---------------------------------------------------------------------------
// Path validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn paths_outside_site_and_blocks_are_rejected() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    for bad in [
        "../x",
        "site/../../etc",
        "sw.js",
        "site//a",
        "blocks/Bad Name/src/lib.rs",
        "site/a\\b",
        "",
        "site",
        "blocks/hello",
        "/site/a.css",
    ] {
        let out = dev_post(
            &ctx,
            "/b/dev/api/files/write",
            json!({"path": bad, "content": "x", "expected_sha256": null}),
        )
        .await;
        assert_eq!(output_http_status(out).await, 400, "{bad}");
    }
}

#[tokio::test]
async fn a_space_inside_a_segment_is_a_legitimate_path() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let sha = write_new(&ctx, "site/my page.html", "<p>ok</p>").await;
    let l = output_json(ctx.dispatch(list_msg(None)).await).await;
    assert_eq!(l["files"][0]["path"], "site/my page.html");
    assert_eq!(l["files"][0]["sha256"], serde_json::json!(sha));
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_listing_is_sorted_by_path_and_filtered_by_prefix() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    write_new(&ctx, "site/z.css", "z{}").await;
    write_new(&ctx, "site/a.css", "a{}").await;
    write_new(&ctx, "blocks/hello/src/lib.rs", "fn main() {}").await;

    let all = output_json(ctx.dispatch(list_msg(None)).await).await;
    let paths: Vec<&str> = all["files"]
        .as_array()
        .expect("files array")
        .iter()
        .map(|f| f["path"].as_str().expect("path"))
        .collect();
    assert_eq!(
        paths,
        vec!["blocks/hello/src/lib.rs", "site/a.css", "site/z.css"]
    );

    let site = output_json(ctx.dispatch(list_msg(Some("site/"))).await).await;
    let paths: Vec<&str> = site["files"]
        .as_array()
        .expect("files array")
        .iter()
        .map(|f| f["path"].as_str().expect("path"))
        .collect();
    assert_eq!(paths, vec!["site/a.css", "site/z.css"]);
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_with_matching_hash_removes_the_entry_and_keeps_the_blob_for_history() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let sha = write_new(&ctx, "site/a.css", "a{}").await;

    let out = dev_post(
        &ctx,
        "/b/dev/api/files/delete",
        json!({"path": "site/a.css", "expected_sha256": sha}),
    )
    .await;
    assert_eq!(output_status(out).await, 200);

    let l = output_json(ctx.dispatch(list_msg(None)).await).await;
    assert!(l["files"].as_array().expect("files array").is_empty());

    // The blob outlives the manifest entry: an earlier generation still
    // names it, and Plan 4's GC — not the delete handler — reclaims it.
    assert_eq!(
        blobs::get(&ctx, &sha).await.expect("blob survives delete"),
        b"a{}".to_vec()
    );
}

#[tokio::test]
async fn delete_with_a_stale_hash_is_a_conflict() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let sha = write_new(&ctx, "site/a.css", "a{}").await;

    let out = dev_post(
        &ctx,
        "/b/dev/api/files/delete",
        json!({"path": "site/a.css", "expected_sha256": "deadbeef"}),
    )
    .await;
    assert_eq!(output_status(out).await, 409);

    let body = output_json(
        dev_post(
            &ctx,
            "/b/dev/api/files/delete",
            json!({"path": "site/a.css", "expected_sha256": "deadbeef"}),
        )
        .await,
    )
    .await;
    assert_eq!(body["current_sha256"], serde_json::json!(sha));

    let l = output_json(ctx.dispatch(list_msg(None)).await).await;
    assert_eq!(l["files"].as_array().expect("files array").len(), 1);
}

#[tokio::test]
async fn deleting_a_file_that_is_not_there_is_a_conflict_with_no_current_hash() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let body = output_json(
        dev_post(
            &ctx,
            "/b/dev/api/files/delete",
            json!({"path": "site/a.css", "expected_sha256": "deadbeef"}),
        )
        .await,
    )
    .await;
    assert_eq!(body["current_sha256"], serde_json::Value::Null);
    assert_eq!(body["current_size"], serde_json::Value::Null);
}

#[tokio::test]
async fn reading_a_file_that_is_not_there_is_a_404() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let out = dev_post(&ctx, "/b/dev/api/files/read", json!({"path": "site/a.css"})).await;
    assert_eq!(output_http_status(out).await, 404);
}

// ---------------------------------------------------------------------------
// Content addressing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn identical_content_at_two_paths_shares_one_blob() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let a = write_new(&ctx, "site/a.html", "<p>same</p>").await;
    let b = write_new(&ctx, "site/b.html", "<p>same</p>").await;
    assert_eq!(a, b);

    // Deleting one path must not take the shared blob with it.
    let out = dev_post(
        &ctx,
        "/b/dev/api/files/delete",
        json!({"path": "site/a.html", "expected_sha256": a}),
    )
    .await;
    assert_eq!(output_status(out).await, 200);
    let r = output_json(
        dev_post(
            &ctx,
            "/b/dev/api/files/read",
            json!({"path": "site/b.html"}),
        )
        .await,
    )
    .await;
    assert_eq!(r["content"], "<p>same</p>");
}

// ---------------------------------------------------------------------------
// Quotas
// ---------------------------------------------------------------------------

#[tokio::test]
async fn file_size_quota_is_enforced() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let big = "x".repeat(paths::MAX_FILE_BYTES + 1);
    let out = dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/big.txt", "content": big, "expected_sha256": null}),
    )
    .await;
    assert_eq!(output_http_status(out).await, 413);

    // The limit itself is not the failing case.
    let at_limit = "x".repeat(paths::MAX_FILE_BYTES);
    let out = dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/big.txt", "content": at_limit, "expected_sha256": null}),
    )
    .await;
    assert_eq!(output_status(out).await, 200);
}

// ---------------------------------------------------------------------------
// Access and caching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_files_api_is_admin_only() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    for msg in [
        anon_msg("retrieve", "/b/dev/api/files"),
        auth_msg("retrieve", "/b/dev/api/files", "u1"),
        anon_msg("create", "/b/dev/api/files/write"),
        auth_msg("create", "/b/dev/api/files/write", "u1"),
        anon_msg("create", "/b/dev/api/files/read"),
        anon_msg("create", "/b/dev/api/files/delete"),
    ] {
        let path = msg.path().to_string();
        assert_eq!(
            output_http_status(ctx.dispatch(msg).await).await,
            403,
            "{path}"
        );
    }
}

#[tokio::test]
async fn every_files_response_is_never_cached() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    write_new(&ctx, "site/a.css", "a{}").await;

    // One request per assertion: reading an `OutputStream` consumes it.
    assert_eq!(
        output_http_header(ctx.dispatch(list_msg(None)).await, "Cache-Control")
            .await
            .as_deref(),
        Some("no-store"),
    );
    assert_eq!(
        output_http_header(
            dev_post(
                &ctx,
                "/b/dev/api/files/write",
                json!({"path": "site/a.css", "content": "b{}", "expected_sha256": "stale"}),
            )
            .await,
            "Cache-Control"
        )
        .await
        .as_deref(),
        Some("no-store"),
        "a 409 must be no-store too",
    );
    assert_eq!(
        output_http_header(
            dev_post(
                &ctx,
                "/b/dev/api/files/write",
                json!({"path": "../nope", "content": "x", "expected_sha256": null}),
            )
            .await,
            "Cache-Control"
        )
        .await
        .as_deref(),
        Some("no-store"),
        "a 400 must be no-store too",
    );
}

// ---------------------------------------------------------------------------
// The published contract and the deserializer must agree
// ---------------------------------------------------------------------------

/// The schema a client reads and the body the handler accepts must name the
/// same required fields: a schema that is stricter turns a valid call into a
/// client-side error, and one that is laxer turns it into a 400.
#[test]
fn the_write_schema_and_the_handler_agree_on_what_is_required() {
    let info = DevBlock::new(DevShared::new(FakeControl::new())).info();
    let write = info
        .endpoints
        .iter()
        .find(|e| e.path == "/b/dev/api/files/write")
        .expect("write endpoint is declared");
    let required: Vec<&str> = write.input_schema.as_ref().expect("input schema")["required"]
        .as_array()
        .expect("required list")
        .iter()
        .map(|v| v.as_str().expect("field name"))
        .collect();
    assert_eq!(required, vec!["path", "content"]);
}

/// Omitting `expected_sha256` is the same as sending `null`: "I expect no
/// file here". Over a file that exists that is a conflict, so a caller cannot
/// clobber an edit by forgetting the field.
#[tokio::test]
async fn an_omitted_expected_hash_means_the_file_should_not_exist_yet() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let out = dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/a.css", "content": "a{}"}),
    )
    .await;
    assert_eq!(output_status(out).await, 200);

    let out = dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/a.css", "content": "b{}"}),
    )
    .await;
    assert_eq!(output_status(out).await, 409);
}

// ---------------------------------------------------------------------------
// The block-count quota, end to end
// ---------------------------------------------------------------------------

/// A block quota refusal is a 409, not a 413: nothing about the request is too
/// large — the workspace's shape conflicts with a limit on how much the
/// runtime can be asked to rebuild.
#[tokio::test]
async fn the_block_count_quota_is_a_conflict_not_a_payload_error() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    for i in 0..paths::MAX_BLOCKS {
        write_new(&ctx, &format!("blocks/b{i}/src/lib.rs"), "fn main() {}").await;
    }
    // A second file in a block that already exists is not a new block.
    let out = dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "blocks/b0/Cargo.toml", "content": "[package]", "expected_sha256": null}),
    )
    .await;
    assert_eq!(output_status(out).await, 200);

    let out = dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "blocks/extra/src/lib.rs", "content": "fn main() {}", "expected_sha256": null}),
    )
    .await;
    assert_eq!(output_http_status(out).await, 409);
    // A site write is unaffected by the block count.
    let out = dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/index.html", "content": "<p>hi</p>", "expected_sha256": null}),
    )
    .await;
    assert_eq!(output_status(out).await, 200);
}
