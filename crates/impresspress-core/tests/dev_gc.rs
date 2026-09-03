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

use base64ct::{Base64, Encoding as _};
use impresspress_core::{
    blocks::dev::{
        activation::{self, ActivationIntent},
        artifacts, blobs,
        contracts::SiteManifest,
        control::{DynamicBlockSpec, DynamicRoute, RouteAccessKind},
        gc::{self, GcInterleave},
        generation::{self, GenerationManifest},
        repo::{
            self,
            builds::{BuildStatus, NewBuild},
            generations::{self, GenerationCause, GenerationStatus, NewGeneration},
        },
        retention,
        test_support::FakeControl,
        workspace::{self, FileEntry},
        WAFER_GUEST_VERSION,
    },
    test_support::{admin_msg, output_http_status, output_json, TestContext},
};
use serde_json::json;
use wafer_core::clients::storage;
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

/// One block named `name`, with its own artifact stored and the accepted
/// build row a real compile would have left behind.
async fn block_spec(ctx: &TestContext, name: &str) -> DynamicBlockSpec {
    let bytes = format!("\0asm\x01{name}").into_bytes();
    let artifact_sha256 = artifacts::put(ctx, &bytes)
        .await
        .expect("store the artifact a manifest names");
    let spec = DynamicBlockSpec {
        name: format!("site/{name}"),
        artifact_sha256: artifact_sha256.clone(),
        routes: vec![DynamicRoute {
            prefix: format!("/b/{name}/"),
            access: RouteAccessKind::Public,
        }],
        capabilities: wafer_block::BlockCapabilities::default(),
        wafer_guest_version: WAFER_GUEST_VERSION,
    };
    accept_build(ctx, &spec, bytes.len() as u64).await;
    spec
}

/// The `valid` build row staging leaves behind once its activation has landed.
async fn accept_build(ctx: &TestContext, spec: &DynamicBlockSpec, artifact_bytes: u64) -> String {
    let row = repo::builds::insert(
        ctx,
        &NewBuild {
            block_name: spec.name.clone(),
            source_manifest_sha256: "src".to_string(),
            artifact_sha256: spec.artifact_sha256.clone(),
            block_info_json: "null".to_string(),
            diagnostics_json: "[]".to_string(),
            compiler_version: "rubrc@pinned".to_string(),
            artifact_bytes,
        },
    )
    .await
    .expect("insert build");
    repo::builds::set_status(ctx, &row.id, BuildStatus::Valid, None, None)
        .await
        .expect("accept build");
    row.id
}

/// The `staged` build row a compile holds from before its bytes are stored
/// until its activation has minted a generation.
async fn stage_build(ctx: &TestContext, artifact_sha256: &str, artifact_bytes: u64) -> String {
    repo::builds::insert(
        ctx,
        &NewBuild {
            block_name: "site/pending".to_string(),
            source_manifest_sha256: "src".to_string(),
            artifact_sha256: artifact_sha256.to_string(),
            block_info_json: "null".to_string(),
            diagnostics_json: "[]".to_string(),
            compiler_version: "rubrc@pinned".to_string(),
            artifact_bytes,
        },
    )
    .await
    .expect("insert build")
    .id
}

/// A generation carrying `site`, staged and never activated — what a crash
/// between the ledger insert and the runtime swap leaves behind.
async fn stage_generation(ctx: &TestContext, site: SiteManifest) -> String {
    let id = repo::new_id();
    let mut manifest = GenerationManifest::staged(site, Vec::new());
    manifest.identify(id.clone(), None);
    generations::insert(
        ctx,
        &NewGeneration {
            id: id.clone(),
            parent_id: None,
            cause: GenerationCause::SiteWrite,
            site_manifest_json: generation::canonical_text(&manifest.site).expect("canonical"),
            block_manifest_json: generation::canonical_text(&manifest.blocks).expect("canonical"),
            manifest_sha256: generation::manifest_sha256(&manifest).expect("hash"),
        },
    )
    .await
    .expect("stage");
    id
}

/// A site manifest naming one file whose blob is stored but which no
/// workspace path holds.
async fn site_only_blob(ctx: &TestContext, content: &str) -> (SiteManifest, String) {
    let (sha256, _stored) = blobs::put(ctx, content.as_bytes())
        .await
        .expect("store the blob a manifest names");
    (
        SiteManifest {
            files: vec![FileEntry {
                path: "index.html".to_string(),
                sha256: sha256.clone(),
                size: content.len() as u64,
                content_type: "text/html; charset=utf-8".to_string(),
            }],
        },
        sha256,
    )
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

    // `dev_status` reports those counters rather than walking the store, so
    // something has to check the two against each other: a counter that had
    // drifted would look exactly like a collector that was keeping up.
    let stored = storage::list(&ctx, blobs::FOLDER, &storage::ListOptions::default())
        .await
        .expect("list the blob store");
    assert_eq!(stored.objects.len(), 20, "the counters describe the store");
    assert_eq!(
        stored.objects.iter().map(|o| o.size).sum::<i64>(),
        (5 * 2 + 15 * 3) as i64,
    );
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

    // And it goes the moment nothing names it — on the delete itself, not on
    // some later unrelated site write. A `blocks/` delete publishes nothing
    // (design §7.2), so without the collector running here the blob would stay
    // charged against the workspace's quota until the agent happened to edit
    // the site.
    let deleted = dev_post(
        &ctx,
        "/b/dev/api/files/delete",
        json!({"path": "blocks/hello/src/lib.rs", "expected_sha256": src_sha}),
    )
    .await;
    output_json(deleted).await;
    assert!(
        !blobs::exists(&ctx, &src_sha).await.expect("exists"),
        "the delete that orphaned it is what reclaims it",
    );
    let ws = workspace::load(&ctx).await.expect("load workspace");
    assert!(
        !ws.files.contains_key("blocks/hello/src/lib.rs"),
        "and the entry is gone with it",
    );
    assert_eq!(
        storage_of(&ctx).await["blobs"],
        20,
        "the quota accounting was credited, not just the store",
    );
    let _ = sha;
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

/// A compile stores its artifact before any generation names it, and its build
/// row is what protects the bytes for as long as that lasts.
///
/// The window is a *status*, not a stretch of time. A browser compile takes
/// tens of seconds and an agent's site writes arrive in bursts, so the 21
/// writes here are exactly what an age-based rule ("younger than the oldest
/// retained generation") would let past — each one activates, each activation
/// collects, and by the last the staged build is older than every generation
/// the window keeps.
#[tokio::test]
async fn a_staged_build_protects_its_artifact_through_a_burst_of_site_writes() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;

    // The state `POST /b/dev/api/builds/stage` is in between its row and its
    // activation: row staged, bytes stored, no generation.
    let bytes = b"\0asm\x01staged";
    let artifact = blobs::sha256_hex(bytes);
    stage_build(&ctx, &artifact, bytes.len() as u64).await;
    artifacts::put(&ctx, bytes).await.expect("store");

    // The compile is still running while the agent edits the site.
    let mut sha = None;
    for i in 0..21 {
        sha = Some(write_file(&ctx, "site/index.html", &format!("v{i}"), sha.as_deref()).await);
    }
    assert!(
        artifacts::exists(&ctx, &artifact).await.expect("exists"),
        "a slow compile is still a compile: its row says the bytes are on their way",
    );

    // And the protection ends with the status, not with a clock: a refused
    // compile's artifact goes on the next collection.
    let row = repo::builds::list_in_flight(&ctx)
        .await
        .expect("list")
        .pop()
        .expect("the staged row");
    repo::builds::set_status(&ctx, &row.id, BuildStatus::Invalid, None, None)
        .await
        .expect("refuse");
    write_file(&ctx, "site/index.html", "v21", sha.as_deref()).await;
    assert!(
        !artifacts::exists(&ctx, &artifact).await.expect("exists"),
        "nothing is on its way to a generation any more",
    );
}

/// The collector lists before it reads its roots, and that ordering is the
/// whole of its soundness: an object stored after the listing is not a
/// candidate, and a root written after the listing is still read.
///
/// Unobservable without a seam — nothing in the fixture yields, so no compile
/// can interleave itself into the gap — so `GcInterleave` puts one there.
/// Under the reverse order (roots, then listing) both halves below are
/// deleted.
#[tokio::test]
async fn a_stage_that_lands_between_the_listing_and_the_roots_keeps_its_artifact() {
    /// Stages a build the way `blocks_api` does: the row for bytes that are
    /// already stored, then a second artifact stored from scratch.
    struct StageMidCollect<'a> {
        ctx: &'a TestContext,
        listed: String,
        unlisted: Vec<u8>,
    }

    #[wafer_block::wafer_async_trait]
    impl GcInterleave for StageMidCollect<'_> {
        async fn after_listing(&self) {
            // (a) A root for an artifact the listing already saw. The roots
            //     are read after this, so it must be seen.
            stage_build(self.ctx, &self.listed, 16).await;
            // (b) An artifact stored after the listing, with no root at all.
            //     It is not a candidate, so it needs none.
            artifacts::put(self.ctx, &self.unlisted)
                .await
                .expect("store");
        }
    }

    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let shared = ctx.dev_shared();

    // An artifact in the store that nothing names — a candidate, and without
    // the interleaved row it would go.
    let listed = artifacts::put(&ctx, b"\0asm\x01listed")
        .await
        .expect("store");
    // A second one that no generation and no row will ever name, so the
    // collection below is doing real work rather than nothing at all.
    let doomed = artifacts::put(&ctx, b"\0asm\x01doomed")
        .await
        .expect("store");

    let unlisted = b"\0asm\x01unlisted".to_vec();
    let unlisted_sha = blobs::sha256_hex(&unlisted);
    let interleave = StageMidCollect {
        ctx: &ctx,
        listed: listed.clone(),
        unlisted,
    };
    let report = gc::collect_interleaved(&ctx, &shared, &interleave)
        .await
        .expect("collect");

    assert_eq!(report.artifacts_deleted, 1, "the unreferenced one went");
    assert!(!artifacts::exists(&ctx, &doomed).await.expect("exists"));
    assert!(
        artifacts::exists(&ctx, &listed).await.expect("exists"),
        "its build row was written before the roots were read",
    );
    assert!(
        artifacts::exists(&ctx, &unlisted_sha)
            .await
            .expect("exists"),
        "it was stored after the listing, so it was never a candidate",
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
    // survives a ledger dominated by failures — and the assertion is
    // discriminating because that write REPLACES the workspace entry naming
    // it, leaving the serving generation as its only root.
    write_file(&ctx, "site/index.html", "replaced", Some(&live_blob)).await;
    let ws = workspace::load(&ctx).await.expect("load workspace");
    assert!(
        !ws.references(&live_blob),
        "the fixture must leave the workspace naming something else",
    );
    assert!(
        blobs::exists(&ctx, &live_blob).await.expect("exists"),
        "a generation inside the window still names it",
    );
}

/// The serving generation's manifest is a root in its own right. A collector
/// that only trusted the workspace would delete the bytes the site is being
/// served from the moment a generation stopped matching the editable state —
/// which is what a rollback, and every `BlockSet` intent carrying an explicit
/// site, produces.
#[tokio::test]
async fn a_blob_only_the_active_generation_names_survives_collection() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let shared = ctx.dev_shared();

    // Published straight from a manifest, so the workspace never names it.
    let (site, served) = site_only_blob(&ctx, "<h1>live</h1>").await;
    activation::request(
        &ctx,
        &shared,
        GenerationCause::SiteWrite,
        ActivationIntent::BlockSet {
            site: Some(site),
            blocks: Vec::new(),
        },
    )
    .await
    .expect("the manifest activates");

    // Something genuinely unreachable, so the collection below is doing work.
    let (orphan, _stored) = blobs::put(&ctx, b"nobody names me").await.expect("put");

    let ws = workspace::load(&ctx).await.expect("load workspace");
    assert!(
        ws.files.is_empty() && !ws.references(&served),
        "the fixture is only meaningful if the workspace does not name it",
    );

    let report = gc::collect(&ctx, &shared).await.expect("collect");
    assert_eq!(report.blobs_deleted, 1);
    assert!(!blobs::exists(&ctx, &orphan).await.expect("exists"));
    assert!(
        blobs::exists(&ctx, &served).await.expect("exists"),
        "the generation that is serving names it, and that is a root",
    );
}

/// The builds table is the index of what the artifact store holds, so a row
/// for bytes that never made it in is a lie in both directions: the collector
/// would protect an object that does not exist, and `dev_status` would report
/// it as stored.
#[tokio::test]
async fn a_stage_whose_artifact_cannot_be_stored_leaves_no_build_row() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    ctx.fail_next_storage_put("the disk is on fire");

    let out = dev_post(
        &ctx,
        "/b/dev/api/builds/stage",
        json!({
            "block_name": "hello",
            "artifact_base64": Base64::encode_string(b"\0asm\x01\0\0\0"),
            "compiler_version": "test",
            "diagnostics": [],
        }),
    )
    .await;
    assert_eq!(output_http_status(out).await, 500);

    assert!(
        repo::builds::list_in_flight(&ctx)
            .await
            .expect("list")
            .is_empty(),
        "the row went back out with the bytes that never arrived",
    );
    assert!(repo::builds::artifact_index(&ctx)
        .await
        .expect("index")
        .is_empty());
    assert_eq!(storage_of(&ctx).await["artifacts"], 0);
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

/// An in-flight generation is kept because an activation might still finish
/// it. Nothing is running on a process that has just started, so a staged row
/// at boot is wreckage — and left staged it would keep its blobs against the
/// workspace quota for the life of the instance, however far down the ledger
/// it fell.
#[tokio::test]
async fn an_orphaned_staged_generation_is_retired_at_boot_and_its_blobs_collected() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let shared = ctx.dev_shared();

    let (site, pinned) = site_only_blob(&ctx, "<h1>never activated</h1>").await;
    let orphan = stage_generation(&ctx, site).await;

    // Push it well past the retention window.
    let mut sha = None;
    for i in 0..21 {
        sha = Some(write_file(&ctx, "site/index.html", &format!("v{i}"), sha.as_deref()).await);
    }
    assert_eq!(
        generations::get(&ctx, &orphan).await.expect("get").status,
        GenerationStatus::Staged,
        "an in-flight row is not retention's to delete",
    );
    assert!(
        blobs::exists(&ctx, &pinned).await.expect("exists"),
        "and so its blob is pinned — which is the leak",
    );

    // The journal names nothing, so nothing is owed and the row is wreckage.
    activation::converge_on_boot(&ctx, &shared)
        .await
        .expect("boot");
    let retired = generations::get(&ctx, &orphan).await.expect("get");
    assert_eq!(retired.status, GenerationStatus::Failed);
    assert!(
        retired
            .failure_message
            .as_deref()
            .unwrap_or_default()
            .contains("abandoned at boot"),
        "the row must say why it was closed: {retired:?}",
    );

    // Now it is ordinary history, so the next activation prunes it and the
    // collector reclaims what only it named.
    write_file(&ctx, "site/index.html", "v21", sha.as_deref()).await;
    assert_eq!(
        generations::get(&ctx, &orphan)
            .await
            .expect_err("pruned")
            .code,
        wafer_run::ErrorCode::NotFound,
    );
    assert!(
        !blobs::exists(&ctx, &pinned).await.expect("exists"),
        "nothing names it any more",
    );
}

/// The same for builds, which have no journal to be named by: a staged row is
/// a compile that was running, and none is.
#[tokio::test]
async fn an_orphaned_staged_build_is_retired_at_boot_and_its_artifact_collected() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let shared = ctx.dev_shared();

    let bytes = b"\0asm\x01abandoned";
    let artifact = artifacts::put(&ctx, bytes).await.expect("store");
    let row = stage_build(&ctx, &artifact, bytes.len() as u64).await;

    activation::converge_on_boot(&ctx, &shared)
        .await
        .expect("boot");
    let retired = repo::builds::get(&ctx, &row).await.expect("get");
    assert_eq!(retired.status, BuildStatus::Invalid);
    assert!(
        retired.diagnostics_json.contains("abandoned at boot"),
        "the row must say why it was closed: {}",
        retired.diagnostics_json,
    );

    gc::collect(&ctx, &shared).await.expect("collect");
    assert!(
        !artifacts::exists(&ctx, &artifact).await.expect("exists"),
        "no compile is coming for it",
    );
}
