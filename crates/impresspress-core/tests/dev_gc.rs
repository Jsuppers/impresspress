//! Retention, garbage collection and the storage figures `dev_status` reports.
//!
//! Gated on `block-dev` for the same reason `dev_activation.rs` is: the block
//! does not exist in a default-feature build, so these tests must not compile
//! there.
//!
//! Everything here is driven through the HTTP surface an agent uses, because
//! the property under test is a whole-loop one: a write publishes a
//! generation, the generation falls out of the retention window, the row goes,
//! and only then can the content it named go. A test that called the collector
//! directly would prove the collector's arithmetic and nothing about the loop.
#![cfg(feature = "block-dev")]

use impresspress_core::{
    blocks::dev::{
        activation::{self, ActivationIntent},
        artifacts, blobs,
        control::{DynamicBlockSpec, DynamicRoute, RouteAccessKind},
        repo::{
            self,
            builds::{BuildStatus, NewBuild},
            generations::{self, GenerationStatus},
        },
        retention,
        test_support::FakeControl,
        workspace, WAFER_GUEST_VERSION,
    },
    test_support::{admin_msg, output_json, TestContext},
};
use serde_json::json;
use wafer_run::OutputStream;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `POST` a JSON body to a `/b/dev` route as an admin, through the router.
async fn dev_post(ctx: &TestContext, path: &str, body: serde_json::Value) -> OutputStream {
    ctx.dispatch_json(admin_msg("create", path), &body).await
}

/// Write `content` at `path`, expecting the file to hold `expected`, and
/// return the sha the write reports.
async fn write_file(
    ctx: &TestContext,
    path: &str,
    content: &str,
    expected: Option<&str>,
) -> String {
    let out = output_json(
        dev_post(
            ctx,
            "/b/dev/api/files/write",
            json!({"path": path, "content": content, "expected_sha256": expected}),
        )
        .await,
    )
    .await;
    out["sha256"]
        .as_str()
        .unwrap_or_else(|| panic!("no sha256 in {out}"))
        .to_string()
}

/// The `storage` half of `GET /b/dev/api/status`.
async fn storage_of(ctx: &TestContext) -> serde_json::Value {
    let status = output_json(
        ctx.dispatch(admin_msg("retrieve", "/b/dev/api/status"))
            .await,
    )
    .await;
    status["storage"].clone()
}

/// One block named `name`, with its own artifact stored.
async fn block_spec(ctx: &TestContext, name: &str) -> DynamicBlockSpec {
    let artifact_sha256 = artifacts::put(ctx, format!("\0asm\x01{name}").as_bytes())
        .await
        .expect("store the artifact a manifest names");
    DynamicBlockSpec {
        name: format!("site/{name}"),
        artifact_sha256,
        routes: vec![DynamicRoute {
            prefix: format!("/b/{name}/"),
            access: RouteAccessKind::Public,
        }],
        capabilities: wafer_block::BlockCapabilities::default(),
        wafer_guest_version: WAFER_GUEST_VERSION,
    }
}

// ---------------------------------------------------------------------------
// Blobs
// ---------------------------------------------------------------------------

/// The whole loop: 25 versions of one page, 25 generations, 20 retained — and
/// the five blobs only the deleted generations named are gone while the live
/// one is not.
#[tokio::test]
async fn gc_deletes_blobs_no_retained_generation_or_workspace_references() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let mut sha = None;
    let mut first_blob = None;
    for i in 0..25 {
        let written = write_file(&ctx, "site/index.html", &format!("v{i}"), sha.as_deref()).await;
        first_blob.get_or_insert(written.clone());
        sha = Some(written);
    }

    let first_blob = first_blob.expect("25 writes stored a first blob");
    let last_blob = sha.expect("25 writes stored a last blob");
    assert!(
        !blobs::exists(&ctx, &first_blob).await.expect("exists"),
        "v0 is named only by generations retention has deleted",
    );
    assert!(
        blobs::exists(&ctx, &last_blob).await.expect("exists"),
        "v24 is what the workspace and the active generation both name",
    );

    let storage = storage_of(&ctx).await;
    assert_eq!(storage["retained_generations"], 20);
    assert_eq!(
        storage["blobs"], 20,
        "one blob per retained generation, and the newest is the workspace's: {storage}",
    );
    assert_eq!(storage["workspace_files"], 1);
    // `v5` … `v9` are two bytes, `v10` … `v24` are three.
    assert_eq!(storage["blobs_bytes"], 5 * 2 + 15 * 3);

    // The workspace's own accounting was credited, not just the store: the
    // 64 MiB quota is read off these counters, so a collector that freed
    // bytes without crediting them would shrink the store and leave the
    // workspace believing it was still full.
    let ws = workspace::load(&ctx).await.expect("load workspace");
    assert_eq!(ws.blob_count, 20, "25 stored, 5 reclaimed");
    assert_eq!(ws.blob_bytes, 5 * 2 + 15 * 3);
}

/// A block's sources live in the workspace and in no generation at all — a
/// generation carries the compiled artifact, not the crate it came from. A
/// collector that only read the ledger would delete a block's source tree the
/// moment it was written.
#[tokio::test]
async fn gc_never_deletes_a_blob_the_workspace_still_names_even_if_no_generation_does() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let src_sha = write_file(&ctx, "blocks/hello/src/lib.rs", "// src", None).await;

    // Enough site writes to push the window past everything, so the source
    // blob's survival cannot be an accident of nothing having been collected.
    let mut sha = None;
    for i in 0..24 {
        sha = Some(write_file(&ctx, "site/page.html", &format!("v{i}"), sha.as_deref()).await);
    }

    assert!(
        blobs::exists(&ctx, &src_sha).await.expect("exists"),
        "the workspace still names it, so it is still reachable",
    );
    let storage = storage_of(&ctx).await;
    assert_eq!(storage["workspace_files"], 2);
    assert_eq!(
        storage["blobs"], 21,
        "twenty retained site versions plus the block source: {storage}",
    );

    // And it goes the moment nothing names it: the delete leaves the blob
    // alone (an older generation might have named it), the next collection
    // takes it.
    let deleted = dev_post(
        &ctx,
        "/b/dev/api/files/delete",
        json!({"path": "blocks/hello/src/lib.rs", "expected_sha256": src_sha}),
    )
    .await;
    output_json(deleted).await;
    write_file(&ctx, "site/page.html", "v24", sha.as_deref()).await;
    assert!(
        !blobs::exists(&ctx, &src_sha).await.expect("exists"),
        "nothing names it any more",
    );
}

/// The figures move as content is reclaimed — that is what makes them worth
/// polling. Asserted against a fresh instance and against one whose window has
/// started deleting, so a hard-coded zero could not pass either.
#[tokio::test]
async fn dev_status_reports_the_stores_as_the_collector_shrinks_them() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;

    let fresh = storage_of(&ctx).await;
    assert_eq!(fresh["blobs"], 0);
    assert_eq!(fresh["blobs_bytes"], 0);
    assert_eq!(fresh["artifacts"], 0);
    assert_eq!(fresh["workspace_files"], 0);
    assert_eq!(fresh["retained_generations"], 0);

    let mut sha = None;
    for i in 0..5 {
        sha = Some(write_file(&ctx, "site/index.html", &format!("v{i}"), sha.as_deref()).await);
    }
    let inside = storage_of(&ctx).await;
    assert_eq!(inside["blobs"], 5, "nothing has fallen out of the window");
    assert_eq!(inside["blobs_bytes"], 5 * 2);
    assert_eq!(inside["retained_generations"], 5);

    // Twenty more, so fifteen of the first twenty-five generations go.
    for i in 5..25 {
        sha = Some(write_file(&ctx, "site/index.html", &format!("v{i}"), sha.as_deref()).await);
    }
    let collected = storage_of(&ctx).await;
    assert_eq!(collected["blobs"], 20);
    assert_eq!(collected["retained_generations"], 20);
    assert!(
        collected["blobs_bytes"].as_u64().expect("bytes")
            > inside["blobs_bytes"].as_u64().expect("bytes"),
        "twenty three-byte blobs outweigh five two-byte ones: {collected}",
    );
}

// ---------------------------------------------------------------------------
// Artifacts
// ---------------------------------------------------------------------------

/// An artifact is reachable from a retained generation's block manifest. A
/// block that has been replaced keeps its artifact for as long as a generation
/// that can be rolled back to names it, and loses it — with its build row —
/// once none does.
#[tokio::test]
async fn gc_deletes_the_artifact_and_the_build_row_of_a_block_no_generation_names() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let shared = ctx.dev_shared();

    let spec = block_spec(&ctx, "hello").await;
    let superseded = spec.artifact_sha256.clone();
    // The build row a real compile would have left behind, accepted.
    let build = repo::builds::insert(
        &ctx,
        &NewBuild {
            block_name: spec.name.clone(),
            source_manifest_sha256: "src".to_string(),
            artifact_sha256: superseded.clone(),
            block_info_json: "null".to_string(),
            diagnostics_json: "[]".to_string(),
            compiler_version: "rubrc@pinned".to_string(),
        },
    )
    .await
    .expect("insert build");
    repo::builds::set_status(&ctx, &build.id, BuildStatus::Valid, None, None)
        .await
        .expect("accept build");

    activation::request(
        &ctx,
        &shared,
        repo::generations::GenerationCause::BlockCompile,
        ActivationIntent::BlockSet {
            site: None,
            blocks: vec![spec],
        },
    )
    .await
    .expect("the block activates");

    // A second compile of the same block: the first artifact is now named only
    // by generations that are still inside the window.
    let replacement = block_spec(&ctx, "hello2").await;
    let kept = replacement.artifact_sha256.clone();
    activation::request(
        &ctx,
        &shared,
        repo::generations::GenerationCause::BlockCompile,
        ActivationIntent::BlockSet {
            site: None,
            blocks: vec![replacement],
        },
    )
    .await
    .expect("the replacement activates");
    assert!(
        artifacts::exists(&ctx, &superseded).await.expect("exists"),
        "a generation inside the window still names it, so it can be rolled back to",
    );

    // Twenty site writes push both of those generations out of the window.
    let mut sha = None;
    for i in 0..21 {
        sha = Some(write_file(&ctx, "site/index.html", &format!("v{i}"), sha.as_deref()).await);
    }

    assert!(
        !artifacts::exists(&ctx, &superseded).await.expect("exists"),
        "no retained generation names it any more",
    );
    assert!(
        artifacts::exists(&ctx, &kept).await.expect("exists"),
        "the active generation still runs it",
    );
    // The row went with the bytes: a build claiming an accepted artifact the
    // store no longer holds is what the duplicate-tool check would read back.
    assert_eq!(
        repo::builds::latest_valid_for_artifact(&ctx, &superseded)
            .await
            .expect("lookup"),
        None,
    );
    assert_eq!(storage_of(&ctx).await["artifacts"], 1);
}

/// A compile stores its artifact before any generation names it. Its build row
/// is what protects the bytes in that window — without it, a site write's
/// collection landing between the stage and the activation would delete the
/// artifact the compile is about to activate.
#[tokio::test]
async fn a_staged_build_protects_its_artifact_before_any_generation_names_it() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;

    // A ledger already past the window, so the collector has work to do and
    // is not merely retaining everything.
    let mut sha = None;
    for i in 0..21 {
        sha = Some(write_file(&ctx, "site/index.html", &format!("v{i}"), sha.as_deref()).await);
    }

    // The state `POST /b/dev/api/builds/stage` is in between its row and its
    // activation: bytes stored, row staged, no generation.
    let artifact = artifacts::put(&ctx, b"\0asm\x01staged")
        .await
        .expect("store the artifact");
    repo::builds::insert(
        &ctx,
        &NewBuild {
            block_name: "site/pending".to_string(),
            source_manifest_sha256: "src".to_string(),
            artifact_sha256: artifact.clone(),
            block_info_json: "null".to_string(),
            diagnostics_json: "[]".to_string(),
            compiler_version: "rubrc@pinned".to_string(),
        },
    )
    .await
    .expect("insert build");

    // Another activation, which collects.
    write_file(&ctx, "site/index.html", "v21", sha.as_deref()).await;

    assert!(
        artifacts::exists(&ctx, &artifact).await.expect("exists"),
        "the build row is what says this compile is still in flight",
    );
}

// ---------------------------------------------------------------------------
// Retention keeps what is live
// ---------------------------------------------------------------------------

/// Twenty refused activations after a good one do not make the good one
/// collectable: the active generation is what the site *is*, and its blobs are
/// what the site serves.
///
/// Driven through real refusals — a `rebuild` the control refuses leaves a
/// `Failed` row and the previous generation still live — so the fixture is the
/// state a run of bad compiles actually produces.
#[tokio::test]
async fn retention_keeps_the_serving_generation_and_its_blobs_under_a_run_of_failures() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;
    let shared = ctx.dev_shared();

    let live_blob = write_file(&ctx, "site/index.html", "live", None).await;
    let active = output_json(
        ctx.dispatch(admin_msg("retrieve", "/b/dev/api/status"))
            .await,
    )
    .await["active_generation"]["id"]
        .as_str()
        .expect("an active generation")
        .to_string();

    // Twenty-two failed block activations, each leaving a row newer than the
    // one that is serving.
    for i in 0..22 {
        control.fail_next_rebuild("wasmi: boom");
        let spec = block_spec(&ctx, &format!("b{i}")).await;
        activation::request(
            &ctx,
            &shared,
            repo::generations::GenerationCause::BlockCompile,
            ActivationIntent::BlockSet {
                site: None,
                blocks: vec![spec],
            },
        )
        .await
        .expect_err("a refused rebuild refuses the activation");
    }

    // Retention runs on a successful activation, and the serving generation is
    // by then far outside the newest twenty.
    let pruned = retention::prune(&ctx).await.expect("prune");
    assert!(!pruned.is_empty(), "the ledger is past the window");
    assert!(
        pruned.iter().all(|row| row.id != active),
        "the serving generation is not retention's to delete: {pruned:?}",
    );
    assert_eq!(
        generations::get(&ctx, &active).await.expect("get").status,
        GenerationStatus::Active,
    );

    // And a `Failed` row inside the window keeps its status: nothing rewrites
    // a status because a row got old.
    let newest = generations::list_recent(&ctx, 1).await.expect("list");
    assert_eq!(newest[0].status, GenerationStatus::Failed);

    // The collector reads the same retained set, so the live site's blob
    // survives a ledger dominated by failures.
    write_file(&ctx, "site/other.html", "x", None).await;
    assert!(
        blobs::exists(&ctx, &live_blob).await.expect("exists"),
        "the active generation still names it",
    );
}
