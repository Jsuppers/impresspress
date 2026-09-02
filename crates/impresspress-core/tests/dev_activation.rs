//! Generations, the activation queue, the site publisher and boot recovery.
//!
//! Gated on `block-dev` for the same reason `dev_files.rs` is: the block does
//! not exist in a default-feature build, so these tests must not compile
//! there.
#![cfg(feature = "block-dev")]

use impresspress_core::{
    blocks::dev::{
        activation::{self, ActivationError, ActivationIntent},
        artifacts, blobs,
        contracts::SiteManifest,
        control::{DynamicBlockSpec, DynamicRoute, RouteAccessKind},
        generation::{self, GenerationManifest},
        repo::{
            self,
            generations::{self, GenerationCause, GenerationStatus, NewGeneration},
            runtime_state::{self, ActivationPhase, RuntimeState},
        },
        test_support::FakeControl,
        workspace::FileEntry,
        WAFER_GUEST_VERSION,
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

/// The sha256 of `content`, as the files API reports it — the value a write
/// passes back as `expected_sha256`.
fn sha_of(content: &str) -> String {
    blobs::sha256_hex(content.as_bytes())
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

/// One block named `name`, with its own artifact stored so validation passes.
async fn block_spec(ctx: &TestContext, name: &str) -> DynamicBlockSpec {
    // Distinct bytes per block, so two specs are distinguishable by artifact
    // as well as by name.
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

/// The intent Task 8 will produce from a successful compile: the workspace's
/// own site, plus this block set.
async fn compile_of(ctx: &TestContext, names: &[&str]) -> ActivationIntent {
    let mut blocks = Vec::new();
    for name in names {
        blocks.push(block_spec(ctx, name).await);
    }
    ActivationIntent::BlockSet { site: None, blocks }
}

/// The block names the active generation declares, sorted.
async fn active_block_names(ctx: &TestContext) -> Vec<String> {
    let status = status_of(ctx).await;
    let mut names: Vec<String> = status["blocks"]
        .as_array()
        .expect("blocks")
        .iter()
        .map(|b| b["name"].as_str().expect("name").to_string())
        .collect();
    names.sort();
    names
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
        compile_of(&ctx, &["hello"]).await,
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
    // An explicit site (rather than the workspace's own) is the only way to
    // name content the store does not hold: a `SiteOnly` intent reads the
    // workspace, whose every entry is stored by construction.
    let mut site = site_of(&ctx, "v1").await;
    site.files[0].sha256 = blobs::sha256_hex(b"never stored");

    let err = activation::request(
        &ctx,
        &ctx.dev_shared(),
        GenerationCause::SiteWrite,
        ActivationIntent::BlockSet {
            site: Some(site),
            blocks: Vec::new(),
        },
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
    write_file(&ctx, "site/index.html", "v1", None).await;
    activation::request(
        &ctx,
        &ctx.dev_shared(),
        GenerationCause::BlockCompile,
        compile_of(&ctx, &["hello"]).await,
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
///
/// Two writes to *different* paths, because the point is that both survive:
/// the queue keeps one pending slot, so a coalesced site publish has to be
/// composed from the persisted workspace rather than from whatever either
/// caller happened to be holding.
#[tokio::test]
async fn site_writes_that_arrive_during_an_activation_coalesce_and_both_publish() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;
    let shared = ctx.dev_shared();
    let compile = compile_of(&ctx, &["hello"]).await;

    // The driver parks inside `rebuild` — the one place an activation can be
    // held open — so the two writes are admitted while it is in flight.
    let release = control.gate_next_rebuild();
    let (driver, first, second, ()) = tokio::join!(
        activation::request(&ctx, &shared, GenerationCause::BlockCompile, compile),
        write_file(&ctx, "site/a.css", "a{}", None),
        write_file(&ctx, "site/b.css", "b{}", None),
        async {
            let _ = release.send(());
        },
    );
    let driver = driver.expect("the driver activates its own intent");

    // Both writes resolved with the same generation — the one composed from
    // the workspace after both had saved — and it is not the driver's.
    assert_eq!(first["generation"]["id"], second["generation"]["id"]);
    assert_ne!(first["generation"]["id"], json!(driver.generation.id));
    assert_eq!(first["generation"]["site_files"], 2);
    assert_eq!(
        served(&ctx, "a.css").await.as_deref(),
        Some(&b"a{}"[..]),
        "the displaced writer's file must still be published"
    );
    assert_eq!(served(&ctx, "b.css").await.as_deref(), Some(&b"b{}"[..]));

    // Two publishes for three requests, and one rebuild: the site writes
    // changed no block set.
    let l = list_generations(&ctx, Some(100)).await;
    assert_eq!(l["generations"].as_array().expect("generations").len(), 2);
    assert_eq!(control.rebuilds().len(), 1);
}

/// The regression the intent design exists for: a site write admitted while a
/// block activation is in flight must not be composed against the *previous*
/// block set. Composed at request time it would carry the pre-compile blocks
/// and, at dequeue, rebuild the runtime back to them — tearing out the block
/// that had just been published.
#[tokio::test]
async fn a_site_write_during_a_block_activation_keeps_the_block_set() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;
    let shared = ctx.dev_shared();
    write_file(&ctx, "site/index.html", "v1", None).await;

    // One block is already live, so the in-flight activation is a change from
    // a non-empty set to a different non-empty set — a site write composed
    // from either would be wrong in a visible way.
    activation::request(
        &ctx,
        &shared,
        GenerationCause::BlockCompile,
        compile_of(&ctx, &["hello"]).await,
    )
    .await
    .expect("activate the first block");
    assert_eq!(active_block_names(&ctx).await, vec!["site/hello"]);

    let second = compile_of(&ctx, &["hello", "goodbye"]).await;
    let v1 = sha_of("v1");
    let release = control.gate_next_rebuild();
    let (compile, write, ()) = tokio::join!(
        activation::request(&ctx, &shared, GenerationCause::BlockCompile, second),
        write_file(&ctx, "site/index.html", "v2", Some(&v1)),
        async {
            let _ = release.send(());
        },
    );
    compile.expect("the second compile activates");

    // The generation the write published still carries both blocks...
    assert_eq!(write["generation"]["blocks"], 2);
    assert_eq!(
        active_block_names(&ctx).await,
        vec!["site/goodbye", "site/hello"]
    );
    // ...and the write itself rebuilt nothing: two compiles, two rebuilds.
    assert_eq!(control.rebuilds().len(), 2);
    assert_eq!(control.rebuilds()[1].len(), 2);
    assert_eq!(
        served(&ctx, "index.html").await.as_deref(),
        Some(&b"v2"[..])
    );
}

/// Liveness: a driver that is cancelled mid-activation must release the queue
/// and fail the requests folded into it, not wedge every later request on a
/// oneshot nobody will send.
#[tokio::test]
async fn a_cancelled_driver_releases_the_queue_and_fails_its_waiters() {
    use std::{
        future::Future,
        pin::Pin,
        task::{Context as TaskContext, Poll, Waker},
    };

    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;
    let shared = ctx.dev_shared();
    let compile = compile_of(&ctx, &["hello"]).await;

    // The gate's sender is held for the whole test, so the driver can never
    // finish: cancelling it is the only way it ends.
    let _never_released = control.gate_next_rebuild();
    let mut cx = TaskContext::from_waker(Waker::noop());

    let mut driver = Box::pin(activation::request(
        &ctx,
        &shared,
        GenerationCause::BlockCompile,
        compile,
    ));
    assert!(
        driver.as_mut().poll(&mut cx).is_pending(),
        "the driver must park inside rebuild"
    );

    let mut waiter = Box::pin(activation::request(
        &ctx,
        &shared,
        GenerationCause::SiteWrite,
        ActivationIntent::SiteOnly,
    ));
    assert!(
        waiter.as_mut().poll(&mut cx).is_pending(),
        "the second request must queue behind the driver"
    );

    // Cancel the driver.
    drop(driver);

    match Pin::new(&mut waiter).poll(&mut cx) {
        Poll::Ready(Err(ActivationError::Runtime(message))) => {
            assert!(message.contains("abandoned"), "{message}");
        }
        other => panic!("the orphaned waiter must be failed: {other:?}"),
    }

    // And the queue is usable again.
    let after = write_file(&ctx, "site/index.html", "v1", None).await;
    assert_eq!(after["generation"]["status"], "active");
    assert_eq!(
        served(&ctx, "index.html").await.as_deref(),
        Some(&b"v1"[..])
    );
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

    // The intent names its own site, different from what is published: a
    // publish only writes what changed, so an activation whose site matched
    // the live one would reach the commit without ever calling `put`.
    let site = site_of(&ctx, "v2").await;
    let intent = ActivationIntent::BlockSet {
        site: Some(site),
        blocks: vec![block_spec(&ctx, "hello").await],
    };
    ctx.fail_next_storage_put("disk is on fire");
    let err = activation::request(
        &ctx,
        &ctx.dev_shared(),
        GenerationCause::BlockCompile,
        intent,
    )
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

// ---------------------------------------------------------------------------
// A journal that points nowhere useful
// ---------------------------------------------------------------------------

/// Point the journal at `desired`, mid-publish, leaving `active` alone.
async fn journal_desired(ctx: &TestContext, desired: &str) {
    let state = runtime_state::read(ctx).await.expect("read journal");
    runtime_state::write(
        ctx,
        &RuntimeState {
            desired_generation_id: Some(desired.to_string()),
            activation_phase: ActivationPhase::Publishing,
            ..state
        },
    )
    .await
    .expect("journal");
}

/// The journal is persistent, so a `desired` that cannot be loaded would fail
/// every boot identically and the instance would never serve again. Boot has
/// to get past it.
#[tokio::test]
async fn boot_clears_a_journal_that_names_a_generation_that_is_gone() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let g1 = write_file(&ctx, "site/index.html", "v1", None).await;
    let active = g1["generation"]["id"].as_str().expect("id").to_string();

    journal_desired(&ctx, "a-generation-that-never-existed").await;

    let blocks = activation::converge_on_boot(&ctx, &ctx.dev_shared())
        .await
        .expect("a dangling journal must not fail the boot");
    assert!(blocks.is_empty());

    let state = runtime_state::read(&ctx).await.expect("read journal");
    assert_eq!(state.desired_generation_id, None);
    assert_eq!(state.activation_phase, ActivationPhase::Idle);
    assert_eq!(state.active_generation_id.as_deref(), Some(active.as_str()));
    assert_eq!(
        served(&ctx, "index.html").await.as_deref(),
        Some(&b"v1"[..])
    );

    // And it stays cleared: a second boot is an ordinary one.
    activation::converge_on_boot(&ctx, &ctx.dev_shared())
        .await
        .expect("the second boot has nothing owed");
    assert_eq!(status_of_generation(&ctx, &active).await, "active");
}

/// The same, for a row that exists but cannot be read: its manifest columns do
/// not parse. Here there *is* a row, so the refusal is recorded on it.
#[tokio::test]
async fn boot_clears_a_journal_that_names_an_unreadable_generation() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let g1 = write_file(&ctx, "site/index.html", "v1", None).await;
    let active = g1["generation"]["id"].as_str().expect("id").to_string();

    let corrupt = repo::new_id();
    generations::insert(
        &ctx,
        &NewGeneration {
            id: corrupt.clone(),
            parent_id: Some(active.clone()),
            cause: GenerationCause::BlockCompile,
            site_manifest_json: r#"{"files":[]}"#.to_string(),
            // Written by a build this one cannot read.
            block_manifest_json: "not-json-at-all".to_string(),
            manifest_sha256: "cc".to_string(),
        },
    )
    .await
    .expect("stage a corrupt generation");
    journal_desired(&ctx, &corrupt).await;

    activation::converge_on_boot(&ctx, &ctx.dev_shared())
        .await
        .expect("an unreadable generation must not fail the boot");

    let state = runtime_state::read(&ctx).await.expect("read journal");
    assert_eq!(state.desired_generation_id, None);
    assert_eq!(state.activation_phase, ActivationPhase::Idle);
    assert_eq!(state.active_generation_id.as_deref(), Some(active.as_str()));
    assert_eq!(
        served(&ctx, "index.html").await.as_deref(),
        Some(&b"v1"[..])
    );

    // The row that could not be read says so, rather than sitting `staged`
    // forever with nothing explaining why it never activated.
    let row = generations::get(&ctx, &corrupt).await.expect("row");
    assert_eq!(row.status, GenerationStatus::Failed);
    assert!(
        row.failure_message
            .as_deref()
            .is_some_and(|m| m.contains("block_manifest_json")),
        "{:?}",
        row.failure_message
    );
}

/// The symmetric hole. `desired` is not the only pointer the persistent
/// journal holds: an `active` that cannot be loaded would make *every* boot
/// return `Err` from here on, and the `/b/dev` page that could fix it is
/// served by the runtime that boot never builds.
#[tokio::test]
async fn boot_clears_an_active_pointer_to_a_generation_that_is_gone() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    write_file(&ctx, "site/index.html", "v1", None).await;

    let state = runtime_state::read(&ctx).await.expect("read journal");
    runtime_state::write(
        &ctx,
        &RuntimeState {
            active_generation_id: Some("a-generation-that-never-existed".to_string()),
            ..state
        },
    )
    .await
    .expect("journal");

    let blocks = activation::converge_on_boot(&ctx, &ctx.dev_shared())
        .await
        .expect("an unloadable active generation must not fail the boot");
    assert!(blocks.is_empty(), "nothing dynamic is served");

    let state = runtime_state::read(&ctx).await.expect("read journal");
    assert_eq!(state.active_generation_id, None);
    assert_eq!(state.desired_generation_id, None);
    assert_eq!(state.activation_phase, ActivationPhase::Idle);

    // The instance is usable again: the next write publishes on top of the
    // cleared journal instead of failing on the pointer.
    let next = write_file(&ctx, "site/index.html", "v2", Some(&sha_of("v1"))).await;
    assert_eq!(next["generation"]["status"], "active");
    assert_eq!(
        served(&ctx, "index.html").await.as_deref(),
        Some(&b"v2"[..])
    );
}

/// The same, for an `active` row that exists but whose manifest columns do not
/// parse. Here there is a row, so the refusal is recorded on it.
#[tokio::test]
async fn boot_clears_an_active_pointer_to_an_unreadable_generation() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;

    let corrupt = repo::new_id();
    generations::insert(
        &ctx,
        &NewGeneration {
            id: corrupt.clone(),
            parent_id: None,
            cause: GenerationCause::Seed,
            // Written by a build this one cannot read.
            site_manifest_json: "not-json-at-all".to_string(),
            block_manifest_json: "[]".to_string(),
            manifest_sha256: "cc".to_string(),
        },
    )
    .await
    .expect("stage a corrupt generation");
    let state = runtime_state::read(&ctx).await.expect("read journal");
    runtime_state::write(
        &ctx,
        &RuntimeState {
            active_generation_id: Some(corrupt.clone()),
            ..state
        },
    )
    .await
    .expect("journal");

    activation::converge_on_boot(&ctx, &ctx.dev_shared())
        .await
        .expect("an unreadable active generation must not fail the boot");

    let state = runtime_state::read(&ctx).await.expect("read journal");
    assert_eq!(state.active_generation_id, None);

    let row = generations::get(&ctx, &corrupt).await.expect("row");
    assert_eq!(row.status, GenerationStatus::Failed);
    assert!(
        row.failure_message
            .as_deref()
            .is_some_and(|m| m.contains("site_manifest_json")),
        "{:?}",
        row.failure_message
    );
}
