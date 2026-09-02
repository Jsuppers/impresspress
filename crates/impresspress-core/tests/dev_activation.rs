//! Generations, the activation queue, the site publisher and boot recovery.
//!
//! Gated on `block-dev` for the same reason `dev_files.rs` is: the block does
//! not exist in a default-feature build, so these tests must not compile
//! there.
#![cfg(feature = "block-dev")]

use impresspress_core::{
    blocks::dev::{
        activation::{self, ActivationError},
        artifacts, blobs,
        contracts::SiteManifest,
        control::{DynamicBlockSpec, DynamicRoute, RouteAccessKind},
        generation::{self, GenerationManifest},
        repo::{
            self,
            generations::{self, GenerationCause, NewGeneration},
            runtime_state::{self, ActivationPhase, RuntimeState},
        },
        test_support::FakeControl,
        workspace::FileEntry,
        DevShared, WAFER_GUEST_VERSION,
    },
    test_support::{
        admin_msg, anon_msg, auth_msg, output_http_header, output_http_status, output_json,
        TestContext,
    },
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

/// `GET` a `/b/dev` route as an admin, through the router.
async fn dev_get(ctx: &TestContext, path: &str) -> OutputStream {
    ctx.dispatch(admin_msg("retrieve", path)).await
}

/// Write `content` at `path`, expecting the file to hold `expected`.
async fn write_file(
    ctx: &TestContext,
    path: &str,
    content: &str,
    expected: Option<&str>,
) -> serde_json::Value {
    output_json(
        dev_post(
            ctx,
            "/b/dev/api/files/write",
            json!({"path": path, "content": content, "expected_sha256": expected}),
        )
        .await,
    )
    .await
}

/// The published site object at `key`, or `None` when nothing is there.
async fn served(ctx: &TestContext, key: &str) -> Option<Vec<u8>> {
    ctx.storage_get("wafer-run/web", "site", key).await.ok()
}

async fn status_of(ctx: &TestContext) -> serde_json::Value {
    output_json(dev_get(ctx, "/b/dev/api/status").await).await
}

// ---------------------------------------------------------------------------
// A site write publishes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_site_write_creates_and_activates_a_generation_without_rebuilding_the_runtime() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;

    let w = write_file(&ctx, "site/index.html", "<h1>v1</h1>", None).await;
    assert_eq!(w["generation"]["cause"], "site_write");
    assert_eq!(w["generation"]["status"], "active");
    assert_eq!(w["generation"]["site_files"], 1);
    assert_eq!(w["generation"]["blocks"], 0);
    assert!(
        control.rebuilds().is_empty(),
        "site-only changes never rebuild the runtime: {:?}",
        control.rebuilds()
    );

    // The published site folder holds exactly the manifest.
    assert_eq!(
        served(&ctx, "index.html").await.as_deref(),
        Some(b"<h1>v1</h1>".as_slice())
    );

    let status = status_of(&ctx).await;
    assert_eq!(status["active_generation"]["id"], w["generation"]["id"]);
    // Nothing is in flight once the request has answered.
    assert_eq!(status["activation"], serde_json::Value::Null);
}

#[tokio::test]
async fn deleting_a_site_file_removes_it_from_the_served_folder() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;

    let w = write_file(&ctx, "site/a.css", "a{}", None).await;
    let sha = w["sha256"].as_str().expect("sha256").to_string();
    assert_eq!(
        served(&ctx, "a.css").await.as_deref(),
        Some(b"a{}".as_slice())
    );

    let d = output_json(
        dev_post(
            &ctx,
            "/b/dev/api/files/delete",
            json!({"path": "site/a.css", "expected_sha256": sha}),
        )
        .await,
    )
    .await;
    assert_eq!(d["generation"]["cause"], "site_delete");
    assert_eq!(d["generation"]["site_files"], 0);
    assert!(
        served(&ctx, "a.css").await.is_none(),
        "the published site must not keep a file the generation dropped"
    );
}

#[tokio::test]
async fn block_source_writes_do_not_create_generations() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;

    let w = write_file(&ctx, "blocks/hello/src/lib.rs", "// rust", None).await;
    assert_eq!(w["generation"], serde_json::Value::Null);

    let l = output_json(dev_get(&ctx, "/b/dev/api/generations").await).await;
    assert!(
        l["generations"].as_array().expect("generations").is_empty(),
        "only a compile publishes a block: {l}"
    );
    assert_eq!(
        status_of(&ctx).await["active_generation"],
        serde_json::Value::Null
    );

    // Deleting block source publishes nothing either.
    let sha = w["sha256"].as_str().expect("sha256").to_string();
    let d = output_json(
        dev_post(
            &ctx,
            "/b/dev/api/files/delete",
            json!({"path": "blocks/hello/src/lib.rs", "expected_sha256": sha}),
        )
        .await,
    )
    .await;
    assert_eq!(d["generation"], serde_json::Value::Null);
}

/// The queue lives on the shared state, not on the block, so a re-instantiated
/// block cannot lose an in-flight activation.
#[test]
fn the_activation_queue_is_shared_state() {
    let shared: std::sync::Arc<DevShared> = DevShared::new(FakeControl::new());
    let also = shared.clone();
    assert!(std::sync::Arc::ptr_eq(&shared, &also));
}

// ---------------------------------------------------------------------------
// Driving the queue directly
// ---------------------------------------------------------------------------

/// A site manifest holding one `index.html` with `content`, its blob stored.
async fn site_of(ctx: &TestContext, content: &str) -> SiteManifest {
    let (sha256, _stored) = blobs::put(ctx, content.as_bytes())
        .await
        .expect("store the blob a manifest names");
    SiteManifest {
        files: vec![FileEntry {
            path: "index.html".to_string(),
            sha256,
            size: content.len() as u64,
            content_type: "text/html; charset=utf-8".to_string(),
        }],
    }
}

/// One block, with its artifact stored so validation passes.
async fn block_spec(ctx: &TestContext) -> DynamicBlockSpec {
    let artifact_sha256 = artifacts::put(ctx, b"\0asm\x01\0\0\0")
        .await
        .expect("store the artifact a manifest names");
    DynamicBlockSpec {
        name: "site/hello".to_string(),
        artifact_sha256,
        routes: vec![DynamicRoute {
            prefix: "/b/hello/".to_string(),
            access: RouteAccessKind::Public,
        }],
        capabilities: wafer_block::BlockCapabilities::default(),
        wafer_guest_version: WAFER_GUEST_VERSION,
    }
}

/// The manifest Task 8 will produce from a successful compile: the site as it
/// stands, plus one block.
async fn manifest_with_block(ctx: &TestContext, content: &str) -> GenerationManifest {
    GenerationManifest::staged(site_of(ctx, content).await, vec![block_spec(ctx).await])
}

#[tokio::test]
async fn rollback_republishes_an_earlier_generation_as_a_new_one() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;

    let g1 = write_file(&ctx, "site/index.html", "v1", None).await;
    let sha1 = g1["sha256"].as_str().expect("sha256").to_string();
    write_file(&ctx, "site/index.html", "v2", Some(&sha1)).await;
    assert_eq!(
        served(&ctx, "index.html").await.as_deref(),
        Some(&b"v2"[..])
    );

    let id1 = g1["generation"]["id"].as_str().expect("id").to_string();
    let r = output_json(
        dev_post(
            &ctx,
            &format!("/b/dev/api/generations/{id1}/rollback"),
            json!({}),
        )
        .await,
    )
    .await;
    assert_eq!(r["generation"]["cause"], "rollback");
    assert_ne!(r["generation"]["id"], json!(id1), "history is append-only");
    assert!(
        r["generation"]["parent_id"].is_string(),
        "a rollback is derived from whatever was live, not from its target"
    );
    // The progress list ends at `active`, as every activation's does.
    let phases: Vec<&str> = r["progress"]
        .as_array()
        .expect("progress")
        .iter()
        .map(|s| s["phase"].as_str().expect("phase"))
        .collect();
    assert_eq!(phases.last(), Some(&"active"), "{phases:?}");
    assert!(
        !phases.contains(&"building_runtime"),
        "a site-only rollback rebuilds nothing"
    );

    assert_eq!(
        served(&ctx, "index.html").await.as_deref(),
        Some(&b"v1"[..])
    );
    // The workspace follows the rollback so the next edit starts from v1.
    let read = output_json(
        dev_post(
            &ctx,
            "/b/dev/api/files/read",
            json!({"path": "site/index.html"}),
        )
        .await,
    )
    .await;
    assert_eq!(read["content"], "v1");
    assert_eq!(read["sha256"], json!(sha1));
}

#[tokio::test]
async fn a_failed_runtime_rebuild_leaves_the_previous_generation_active() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;
    write_file(&ctx, "site/index.html", "v1", None).await;
    let before = status_of(&ctx).await;

    control.fail_next_rebuild("wasmi: boom");
    // Task 8 stages a block; here drive the queue directly with a manifest
    // that carries one.
    let err = activation::request(
        &ctx,
        &ctx.dev_shared(),
        GenerationCause::BlockCompile,
        manifest_with_block(&ctx, "v1").await,
    )
    .await
    .expect_err("a refused rebuild must refuse the activation");
    assert!(
        matches!(&err, ActivationError::Runtime(m) if m.contains("boom")),
        "{err:?}"
    );

    let after = status_of(&ctx).await;
    assert_eq!(
        after["active_generation"]["id"],
        before["active_generation"]["id"]
    );
    // The journal is back at rest: nothing is owed on the next boot.
    assert_eq!(after["activation"], serde_json::Value::Null);
    assert_eq!(
        served(&ctx, "index.html").await.as_deref(),
        Some(&b"v1"[..])
    );

    let l = output_json(dev_get(&ctx, "/b/dev/api/generations").await).await;
    assert_eq!(l["generations"][0]["status"], "failed");
    assert_eq!(l["generations"][0]["cause"], "block_compile");
    assert_eq!(l["generations"][1]["status"], "active");
}

#[tokio::test]
async fn a_manifest_naming_content_that_is_not_stored_is_refused() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let mut next = site_of(&ctx, "v1").await;
    next.files[0].sha256 = blobs::sha256_hex(b"never stored");

    let err = activation::request(
        &ctx,
        &ctx.dev_shared(),
        GenerationCause::SiteWrite,
        GenerationManifest::staged(next, Vec::new()),
    )
    .await
    .expect_err("a manifest naming content that is not stored cannot activate");
    assert!(matches!(err, ActivationError::Validation(_)), "{err:?}");
    assert_eq!(err.status(), 422);

    // The staged generation is on the ledger as `failed`, not silently gone.
    let l = output_json(dev_get(&ctx, "/b/dev/api/generations").await).await;
    assert_eq!(l["generations"][0]["status"], "failed");
    assert_eq!(
        status_of(&ctx).await["active_generation"],
        serde_json::Value::Null
    );
}

/// The 422 as a caller sees it, through a real endpoint: a rollback to a
/// generation whose blobs have been collected.
#[tokio::test]
async fn rolling_back_to_a_generation_whose_content_is_gone_is_a_422() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let g1 = write_file(&ctx, "site/index.html", "v1", None).await;
    let sha1 = g1["sha256"].as_str().expect("sha256").to_string();
    write_file(&ctx, "site/index.html", "v2", Some(&sha1)).await;

    blobs::delete(&ctx, &sha1).await.expect("collect the blob");

    let id1 = g1["generation"]["id"].as_str().expect("id").to_string();
    let out = dev_post(
        &ctx,
        &format!("/b/dev/api/generations/{id1}/rollback"),
        json!({}),
    )
    .await;
    assert_eq!(output_http_status(out).await, 422);

    // Nothing moved: the site and the workspace both still hold v2.
    assert_eq!(
        served(&ctx, "index.html").await.as_deref(),
        Some(&b"v2"[..])
    );
    let read = output_json(
        dev_post(
            &ctx,
            "/b/dev/api/files/read",
            json!({"path": "site/index.html"}),
        )
        .await,
    )
    .await;
    assert_eq!(read["content"], "v2");
}

// ---------------------------------------------------------------------------
// Boot recovery
// ---------------------------------------------------------------------------

/// Stage a generation the way a crashed activation would have left one: the
/// row exists, its blobs are stored, and the journal points at it mid-publish.
async fn insert_staged_generation(ctx: &TestContext, content: &str) -> String {
    let state = runtime_state::read(ctx).await.expect("read journal");
    let id = repo::new_id();
    let mut manifest = GenerationManifest::staged(site_of(ctx, content).await, Vec::new());
    manifest.identify(id.clone(), state.active_generation_id.clone());

    generations::insert(
        ctx,
        &NewGeneration {
            id,
            parent_id: manifest.parent_id.clone(),
            cause: GenerationCause::SiteWrite,
            site_manifest_json: generation::canonical_text(&manifest.site).expect("canonical"),
            block_manifest_json: generation::canonical_text(&manifest.blocks).expect("canonical"),
            manifest_sha256: generation::manifest_sha256(&manifest).expect("hash"),
        },
    )
    .await
    .expect("stage the generation");

    runtime_state::write(
        ctx,
        &RuntimeState {
            desired_generation_id: Some(manifest.generation_id.clone()),
            activation_phase: ActivationPhase::Publishing,
            ..state
        },
    )
    .await
    .expect("journal the interrupted activation");
    manifest.generation_id
}

#[tokio::test]
async fn boot_converges_an_interrupted_activation() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;
    write_file(&ctx, "site/index.html", "v1", None).await;

    // Simulate a crash mid-activation: desired points at a staged generation.
    let staged = insert_staged_generation(&ctx, "v2").await;

    let blocks = activation::converge_on_boot(&ctx, &ctx.dev_shared())
        .await
        .expect("converge");
    assert!(
        blocks.is_empty(),
        "the active generation declares no blocks"
    );

    let state = runtime_state::read(&ctx).await.expect("read journal");
    assert_eq!(state.desired_generation_id, None);
    assert_eq!(state.activation_phase, ActivationPhase::Idle);
    assert_eq!(state.active_generation_id.as_deref(), Some(staged.as_str()));
    assert_eq!(
        served(&ctx, "index.html").await.as_deref(),
        Some(&b"v2"[..])
    );
}

/// Convergence that cannot succeed must still leave a coherent instance: the
/// previous generation live, its site published, and nothing owed.
#[tokio::test]
async fn a_failed_convergence_restores_the_previous_generation() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let g1 = write_file(&ctx, "site/index.html", "v1", None).await;
    let active = g1["generation"]["id"].as_str().expect("id").to_string();

    let staged = insert_staged_generation(&ctx, "v2").await;
    // Collect the blob the staged generation names, so it can never activate.
    let (sha, _) = blobs::put(&ctx, b"v2").await.expect("hash");
    blobs::delete(&ctx, &sha).await.expect("collect");

    let blocks = activation::converge_on_boot(&ctx, &ctx.dev_shared())
        .await
        .expect("a failed convergence is not a failed boot");
    assert!(blocks.is_empty());

    let state = runtime_state::read(&ctx).await.expect("read journal");
    assert_eq!(state.desired_generation_id, None);
    assert_eq!(state.activation_phase, ActivationPhase::Idle);
    assert_eq!(state.active_generation_id.as_deref(), Some(active.as_str()));
    assert_eq!(
        served(&ctx, "index.html").await.as_deref(),
        Some(&b"v1"[..])
    );

    let detail =
        output_json(dev_get(&ctx, &format!("/b/dev/api/generations/{staged}")).await).await;
    assert_eq!(detail["summary"]["status"], "failed");
}

/// Nothing in flight: convergence is a read that reports the block set the
/// host should build with.
#[tokio::test]
async fn boot_reports_the_active_block_set_when_nothing_is_owed() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;
    activation::request(
        &ctx,
        &ctx.dev_shared(),
        GenerationCause::BlockCompile,
        manifest_with_block(&ctx, "v1").await,
    )
    .await
    .expect("activate a generation with a block");

    let blocks = activation::converge_on_boot(&ctx, &ctx.dev_shared())
        .await
        .expect("converge");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].name, "site/hello");
    // Convergence with nothing owed must not rebuild: the host is about to
    // build its runtime from what this returned.
    assert_eq!(control.rebuilds().len(), 1, "only the activation rebuilt");
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

/// A generation that was staged and never activated — a refused compile, or a
/// crash before the swap. Retention is the only thing that ever labels one.
async fn stage_only(ctx: &TestContext) -> String {
    let id = repo::new_id();
    let manifest = {
        let mut manifest = GenerationManifest::staged(SiteManifest::default(), Vec::new());
        manifest.identify(id.clone(), None);
        manifest
    };
    generations::insert(
        ctx,
        &NewGeneration {
            id: id.clone(),
            parent_id: None,
            cause: GenerationCause::BlockCompile,
            site_manifest_json: generation::canonical_text(&manifest.site).expect("canonical"),
            block_manifest_json: generation::canonical_text(&manifest.blocks).expect("canonical"),
            manifest_sha256: generation::manifest_sha256(&manifest).expect("hash"),
        },
    )
    .await
    .expect("stage");
    id
}

/// The status of `id`, read back through the API.
async fn status_of_generation(ctx: &TestContext, id: &str) -> String {
    let detail = output_json(dev_get(ctx, &format!("/b/dev/api/generations/{id}")).await).await;
    detail["summary"]["status"]
        .as_str()
        .unwrap_or_else(|| panic!("no status for {id}: {detail}"))
        .to_string()
}

/// `GET /b/dev/api/generations?limit=n`, with the query where the HTTP
/// boundary puts it.
async fn list_generations(ctx: &TestContext, limit: Option<u32>) -> serde_json::Value {
    let mut msg = admin_msg("retrieve", "/b/dev/api/generations");
    if let Some(limit) = limit {
        msg.set_meta("req.query.limit", limit.to_string());
    }
    output_json(ctx.dispatch(msg).await).await
}

/// Write `site/index.html` `count` times from version `from`, chaining the
/// hashes, and return the last one.
async fn write_repeatedly(
    ctx: &TestContext,
    count: usize,
    from: usize,
    mut sha: Option<String>,
) -> Option<String> {
    for i in from..(from + count) {
        let w = write_file(ctx, "site/index.html", &format!("v{i}"), sha.as_deref()).await;
        sha = Some(w["sha256"].as_str().expect("sha256").to_string());
    }
    sha
}

#[tokio::test]
async fn only_the_last_twenty_generations_are_retained() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;

    // Three stagings that never activated. They are the rows retention has
    // work to do on: an activation supersedes the generation it replaces by
    // itself, so without these the retention pass would be unobservable.
    let mut staged = Vec::new();
    for _ in 0..3 {
        staged.push(stage_only(&ctx).await);
    }

    // 3 + 10 = 13 rows: everything is still inside the window.
    let sha = write_repeatedly(&ctx, 10, 0, None).await;
    for id in &staged {
        assert_eq!(
            status_of_generation(&ctx, id).await,
            "staged",
            "a generation inside the retention window keeps its own status"
        );
    }

    // 3 + 21 = 24 rows: the four oldest fall out.
    write_repeatedly(&ctx, 11, 10, sha).await;
    for id in &staged {
        assert_eq!(status_of_generation(&ctx, id).await, "superseded");
    }

    let l = list_generations(&ctx, Some(100)).await;
    let statuses: Vec<&str> = l["generations"]
        .as_array()
        .expect("generations")
        .iter()
        .map(|g| g["status"].as_str().expect("status"))
        .collect();
    assert_eq!(
        statuses.len(),
        24,
        "the ledger is append-only: {statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == "active").count(),
        1,
        "exactly one generation is serving: {statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == "staged").count(),
        0,
        "24 made, 20 retained: {statuses:?}"
    );
    // The default page size is the retention window, so the default listing
    // is exactly the set that can still be rolled back to.
    assert_eq!(
        list_generations(&ctx, None).await["generations"]
            .as_array()
            .expect("generations")
            .len(),
        20
    );
}

// ---------------------------------------------------------------------------
// Coalescing
// ---------------------------------------------------------------------------

/// Requests that arrive while an activation is running collapse into one, and
/// every waiter resolves with the generation that carries its change.
#[tokio::test]
async fn requests_that_arrive_during_an_activation_coalesce() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;
    let shared = ctx.dev_shared();

    // Prepare every manifest up front: building one is itself asynchronous,
    // and the point of the test is what the queue does, not what storage does.
    let first = manifest_with_block(&ctx, "v1").await;
    let second = manifest_with_block(&ctx, "v2").await;
    let third = manifest_with_block(&ctx, "v3").await;

    // The driver parks inside `rebuild` — the one place an activation can be
    // held open — so the other two are admitted while it is in flight.
    let release = control.gate_next_rebuild();
    let (driver, waiter_a, waiter_b, ()) = tokio::join!(
        activation::request(&ctx, &shared, GenerationCause::BlockCompile, first),
        activation::request(&ctx, &shared, GenerationCause::SiteWrite, second),
        activation::request(&ctx, &shared, GenerationCause::SiteWrite, third),
        async {
            let _ = release.send(());
        },
    );

    let driver = driver.expect("the driver activates its own manifest");
    let waiter_a = waiter_a.expect("a coalesced waiter resolves");
    let waiter_b = waiter_b.expect("a coalesced waiter resolves");

    // The two waiters resolved with the same generation — the one carrying the
    // newest desired state — and it is not the driver's.
    assert_eq!(waiter_a.generation.id, waiter_b.generation.id);
    assert_ne!(driver.generation.id, waiter_a.generation.id);
    assert_eq!(
        served(&ctx, "index.html").await.as_deref(),
        Some(&b"v3"[..])
    );

    // Two publishes for three requests: the displaced manifest never became a
    // generation at all.
    let l = list_generations(&ctx, Some(100)).await;
    assert_eq!(l["generations"].as_array().expect("generations").len(), 2);
    // And one rebuild for three requests: the block set only changed once.
    assert_eq!(control.rebuilds().len(), 1);
}

// ---------------------------------------------------------------------------
// The generations API
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_ledger_publishes_each_generation_with_its_manifest_and_diff() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let g1 = write_file(&ctx, "site/index.html", "v1", None).await;
    let sha1 = g1["sha256"].as_str().expect("sha256").to_string();
    let g2 = write_file(&ctx, "site/index.html", "v2", Some(&sha1)).await;

    let id1 = g1["generation"]["id"].as_str().expect("id").to_string();
    let id2 = g2["generation"]["id"].as_str().expect("id").to_string();

    let l = list_generations(&ctx, Some(10)).await;
    let ids: Vec<&str> = l["generations"]
        .as_array()
        .expect("generations")
        .iter()
        .map(|g| g["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, vec![id2.as_str(), id1.as_str()], "newest first");
    assert_eq!(l["generations"][0]["status"], "active");
    assert_eq!(l["generations"][1]["status"], "superseded");
    assert_eq!(
        list_generations(&ctx, Some(1)).await["generations"]
            .as_array()
            .expect("generations")
            .len(),
        1
    );

    let first = output_json(dev_get(&ctx, &format!("/b/dev/api/generations/{id1}")).await).await;
    assert_eq!(first["summary"]["id"], json!(id1));
    assert_eq!(first["manifest"]["schema_version"], 1);
    assert_eq!(first["manifest"]["generation_id"], json!(id1));
    assert_eq!(first["manifest"]["site"]["files"][0]["path"], "index.html");
    assert_eq!(first["manifest"]["blocks"], json!([]));
    // No parent: the first generation adds everything it holds.
    assert_eq!(
        first["diff_from_parent"]["added_paths"],
        json!(["index.html"])
    );

    let second = output_json(dev_get(&ctx, &format!("/b/dev/api/generations/{id2}")).await).await;
    assert_eq!(second["summary"]["parent_id"], json!(id1));
    assert_eq!(
        second["diff_from_parent"]["changed_paths"],
        json!(["index.html"])
    );
    assert_eq!(second["diff_from_parent"]["added_paths"], json!([]));

    // The stored hash covers the manifest the API publishes.
    let manifest: GenerationManifest =
        serde_json::from_value(second["manifest"].clone()).expect("manifest round-trips");
    assert_eq!(
        generation::manifest_sha256(&manifest).expect("hash"),
        generations::get(&ctx, &id2)
            .await
            .expect("row")
            .manifest_sha256
    );

    assert_eq!(
        output_http_status(dev_get(&ctx, "/b/dev/api/generations/nope").await).await,
        404
    );
    assert_eq!(
        output_http_status(dev_post(&ctx, "/b/dev/api/generations/nope/rollback", json!({})).await)
            .await,
        404
    );
}

#[tokio::test]
async fn the_generations_api_is_admin_only() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    for msg in [
        anon_msg("retrieve", "/b/dev/api/generations"),
        auth_msg("retrieve", "/b/dev/api/generations", "u1"),
        anon_msg("retrieve", "/b/dev/api/generations/g1"),
        auth_msg("retrieve", "/b/dev/api/generations/g1", "u1"),
        anon_msg("create", "/b/dev/api/generations/g1/rollback"),
        auth_msg("create", "/b/dev/api/generations/g1/rollback", "u1"),
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
async fn every_generations_response_is_never_cached() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let g1 = write_file(&ctx, "site/index.html", "v1", None).await;
    let sha1 = g1["sha256"].as_str().expect("sha256").to_string();
    write_file(&ctx, "site/index.html", "v2", Some(&sha1)).await;
    blobs::delete(&ctx, &sha1).await.expect("collect the blob");
    let id1 = g1["generation"]["id"].as_str().expect("id").to_string();

    // One request per assertion: reading an `OutputStream` consumes it.
    for (label, out) in [
        (
            "a 200 listing",
            dev_get(&ctx, "/b/dev/api/generations").await,
        ),
        (
            "a 200 detail",
            dev_get(&ctx, &format!("/b/dev/api/generations/{id1}")).await,
        ),
        ("a 404", dev_get(&ctx, "/b/dev/api/generations/nope").await),
        (
            "a 422 refusal",
            dev_post(
                &ctx,
                &format!("/b/dev/api/generations/{id1}/rollback"),
                json!({}),
            )
            .await,
        ),
    ] {
        assert_eq!(
            output_http_header(out, "Cache-Control").await.as_deref(),
            Some("no-store"),
            "{label}"
        );
    }
}

/// The one failure that has to be unwound rather than merely refused: the
/// runtime has already been swapped when the publish fails (design §7.3).
#[tokio::test]
async fn a_publish_that_fails_after_the_swap_restores_the_previous_runtime_and_site() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;
    let g1 = write_file(&ctx, "site/index.html", "v1", None).await;
    let active = g1["generation"]["id"].as_str().expect("id").to_string();

    let next = manifest_with_block(&ctx, "v2").await;
    ctx.fail_next_storage_put("disk is on fire");
    let err = activation::request(&ctx, &ctx.dev_shared(), GenerationCause::BlockCompile, next)
        .await
        .expect_err("a publish that fails must fail the activation");
    assert!(
        matches!(&err, ActivationError::Storage(m) if m.contains("disk is on fire")),
        "{err:?}"
    );
    assert_eq!(err.status(), 500);

    // The runtime was rebuilt with the new block set, then rebuilt again with
    // the previous one — which here is empty.
    let rebuilds = control.rebuilds();
    assert_eq!(rebuilds.len(), 2, "the swap must be unwound");
    assert_eq!(rebuilds[0].len(), 1);
    assert!(rebuilds[1].is_empty(), "restored to the previous block set");

    // The previous generation is still live, still serving its own content.
    let status = status_of(&ctx).await;
    assert_eq!(status["active_generation"]["id"], json!(active));
    assert_eq!(status["activation"], serde_json::Value::Null);
    assert_eq!(
        served(&ctx, "index.html").await.as_deref(),
        Some(&b"v1"[..])
    );

    let l = list_generations(&ctx, Some(10)).await;
    assert_eq!(l["generations"][0]["status"], "failed");
}

/// A journal whose `desired` already names the live generation must converge
/// to a no-op, not supersede the very generation it activates.
#[tokio::test]
async fn converging_on_the_generation_that_is_already_live_leaves_it_live() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let g1 = write_file(&ctx, "site/index.html", "v1", None).await;
    let active = g1["generation"]["id"].as_str().expect("id").to_string();

    let state = runtime_state::read(&ctx).await.expect("read journal");
    runtime_state::write(
        &ctx,
        &RuntimeState {
            desired_generation_id: Some(active.clone()),
            activation_phase: ActivationPhase::Publishing,
            ..state
        },
    )
    .await
    .expect("journal");

    activation::converge_on_boot(&ctx, &ctx.dev_shared())
        .await
        .expect("converge");

    let after = runtime_state::read(&ctx).await.expect("read journal");
    assert_eq!(after.active_generation_id.as_deref(), Some(active.as_str()));
    assert_eq!(after.desired_generation_id, None);
    assert_eq!(status_of_generation(&ctx, &active).await, "active");
    assert_eq!(
        served(&ctx, "index.html").await.as_deref(),
        Some(&b"v1"[..])
    );
}
