//! Seed-on-boot: importing generation 0 from a bundle the origin serves.
//!
//! Gated on `block-dev` for the same reason `dev_activation.rs` is: the block
//! does not exist in a default-feature build, so these tests must not compile
//! there.
//!
//! The fetch seam is what makes this host-testable at all — the real source is
//! the service worker's own `fetch`, and [`MapFetch`] below is the same
//! contract backed by a `BTreeMap`.
#![cfg(feature = "block-dev")]

use std::collections::BTreeMap;

use impresspress_core::{
    blocks::dev::{
        activation::{self, ActivationIntent},
        artifacts, blobs,
        control::{DynamicBlockSpec, DynamicRoute, RouteAccessKind},
        paths,
        repo::{self, generations::GenerationCause, runtime_state},
        seed::{self, SeedBlock, SeedFetch, SeedFile, SeedManifest},
        test_support::FakeControl,
        validation, workspace,
    },
    test_support::TestContext,
};

// ---------------------------------------------------------------------------
// The fetch seam
// ---------------------------------------------------------------------------

/// A [`SeedFetch`] over an in-memory `url -> bytes` map.
#[derive(Default)]
struct MapFetch {
    files: BTreeMap<String, Vec<u8>>,
}

impl MapFetch {
    fn with(mut self, url: &str, bytes: &[u8]) -> Self {
        self.files.insert(url.to_string(), bytes.to_vec());
        self
    }
}

impl SeedFetch for MapFetch {
    fn get<'a>(&'a self, url: &'a str) -> seed::FetchFuture<'a> {
        Box::pin(async move {
            self.files
                .get(url)
                .cloned()
                .ok_or_else(|| format!("{url}: not in the bundle"))
        })
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const INDEX: &[u8] = b"<h1>welcome</h1>";
const APP_JS: &[u8] = b"console.log('hi');\n";
const LIB_RS: &[u8] = b"// a guest\n";
const ARTIFACT: &[u8] = b"\0asm\x01\0\0\0";

fn file(path: &str, bytes: &[u8]) -> SeedFile {
    SeedFile {
        path: path.to_string(),
        sha256: blobs::sha256_hex(bytes),
        size: bytes.len() as u64,
        // The importer checks this against what the *path* is served as, so
        // the fixture derives it the same way an exporter would.
        content_type: paths::content_type_for(path).to_string(),
    }
}

fn hello_spec() -> DynamicBlockSpec {
    DynamicBlockSpec {
        name: "site/hello".to_string(),
        artifact_sha256: blobs::sha256_hex(ARTIFACT),
        routes: vec![DynamicRoute {
            prefix: "/b/hello/".to_string(),
            access: RouteAccessKind::Public,
        }],
        capabilities: wafer_block::BlockCapabilities::none(),
        wafer_guest_version: 0,
    }
}

/// A two-file site plus one block with one source file.
fn manifest() -> SeedManifest {
    SeedManifest {
        schema_version: seed::SCHEMA_VERSION,
        source_generation: Some("gen-from-elsewhere".to_string()),
        site: vec![file("index.html", INDEX), file("assets/app.js", APP_JS)],
        blocks: vec![SeedBlock {
            spec: hello_spec(),
            sources: vec![file("src/lib.rs", LIB_RS)],
        }],
        data: None,
    }
}

fn bundle() -> MapFetch {
    MapFetch::default()
        .with(&seed::site_url("index.html"), INDEX)
        .with(&seed::site_url("assets/app.js"), APP_JS)
        .with(&seed::artifact_url("hello"), ARTIFACT)
        .with(&seed::source_url("hello", "src/lib.rs"), LIB_RS)
}

// ---------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_seed_bundle_becomes_the_workspace_and_generation_zero() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;

    let generation = seed::import(&ctx, &manifest(), &bundle())
        .await
        .expect("import")
        .expect("a fresh instance imports");

    // The workspace holds every file, at its workspace-relative path.
    let ws = workspace::load(&ctx).await.expect("workspace");
    let paths: Vec<&str> = ws.files.keys().map(String::as_str).collect();
    assert_eq!(
        paths,
        vec![
            "blocks/hello/src/lib.rs",
            "site/assets/app.js",
            "site/index.html",
        ]
    );
    // Content types are derived by the workspace, not copied from the bundle.
    assert_eq!(
        ws.get("site/index.html").expect("entry").content_type,
        "text/html; charset=utf-8"
    );
    assert_eq!(ws.blob_count, 3);
    assert_eq!(
        ws.blob_bytes,
        (INDEX.len() + APP_JS.len() + LIB_RS.len()) as u64
    );

    // Every blob and the artifact are stored under the hash the manifest named.
    assert_eq!(
        blobs::get(&ctx, &blobs::sha256_hex(INDEX))
            .await
            .expect("blob"),
        INDEX.to_vec()
    );
    assert_eq!(
        artifacts::get(&ctx, &blobs::sha256_hex(ARTIFACT))
            .await
            .expect("artifact"),
        ARTIFACT.to_vec()
    );

    // The returned manifest is staged — the site half is the workspace's
    // `site/` entries with the prefix stripped, the block half is the spec.
    assert_eq!(generation.generation_id, "");
    assert_eq!(generation.parent_id, None);
    let site: Vec<&str> = generation
        .site
        .files
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(site, vec!["assets/app.js", "index.html"]);
    assert_eq!(generation.blocks.len(), 1);
    assert_eq!(generation.blocks[0].name, "site/hello");
}

/// The whole point of the import: the manifest it returns activates through
/// the ordinary queue as generation 0, publishing the site.
#[tokio::test]
async fn the_imported_generation_activates_as_generation_zero() {
    let control = FakeControl::new();
    let ctx = TestContext::with_dev(control.clone()).await;
    let shared = ctx.dev_shared();

    let generation = seed::import(&ctx, &manifest(), &bundle())
        .await
        .expect("import")
        .expect("fresh");
    let outcome = activation::request(
        &ctx,
        &shared,
        GenerationCause::Seed,
        ActivationIntent::Seed {
            manifest: generation,
        },
    )
    .await
    .expect("activate the seed");

    assert_eq!(
        outcome.generation.parent_id, None,
        "generation 0 has no parent"
    );
    assert_eq!(outcome.generation.site_files, 2);
    assert_eq!(outcome.generation.blocks, 1);
    assert_eq!(
        ctx.storage_get("wafer-run/web", "site", "index.html")
            .await
            .expect("published index"),
        INDEX.to_vec()
    );
    // The block set reached the host exactly once, as the runtime rebuild.
    let rebuilds = control.rebuilds();
    assert_eq!(rebuilds.len(), 1);
    assert_eq!(rebuilds[0].len(), 1);
    assert_eq!(rebuilds[0][0].name, "site/hello");

    // And the instance is no longer fresh, so a re-import is a no-op.
    assert!(!seed::is_fresh(&ctx).await.expect("is_fresh"));
    assert!(seed::import(&ctx, &manifest(), &bundle())
        .await
        .expect("second import")
        .is_none());
}

// ---------------------------------------------------------------------------
// Freshness
// ---------------------------------------------------------------------------

/// A seed must never overwrite a workspace someone has edited. An instance
/// that has published anything is not fresh, and the import is a no-op — not
/// an error, because this runs on every boot.
#[tokio::test]
async fn a_second_import_on_a_non_fresh_instance_is_a_no_op() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    seed::import(&ctx, &manifest(), &bundle())
        .await
        .expect("first import")
        .expect("fresh");
    // The first import wrote the workspace but staged nothing, so freshness
    // is still decided by the ledger — publish, then re-check.
    activation::request(
        &ctx,
        &ctx.dev_shared(),
        GenerationCause::SiteWrite,
        ActivationIntent::SiteOnly,
    )
    .await
    .expect("publish");

    assert!(seed::import(&ctx, &manifest(), &bundle())
        .await
        .expect("second import")
        .is_none());
}

/// A generation that only ever *failed* still means "something has been staged
/// here", so the instance is not fresh — re-seeding over a workspace whose
/// first publish failed would delete whatever the user wrote next.
#[tokio::test]
async fn a_failed_generation_still_makes_an_instance_non_fresh() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    repo::generations::insert(
        &ctx,
        &repo::generations::NewGeneration {
            id: repo::new_id(),
            parent_id: None,
            cause: GenerationCause::SiteWrite,
            site_manifest_json: r#"{"files":[]}"#.to_string(),
            block_manifest_json: "[]".to_string(),
            manifest_sha256: "aa".to_string(),
        },
    )
    .await
    .expect("stage");

    assert_eq!(
        runtime_state::read(&ctx)
            .await
            .expect("journal")
            .active_generation_id,
        None,
        "nothing is active — freshness must not rest on that alone"
    );
    assert!(!seed::is_fresh(&ctx).await.expect("is_fresh"));
    assert!(seed::import(&ctx, &manifest(), &bundle())
        .await
        .expect("import")
        .is_none());
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn content_that_does_not_match_its_declared_hash_is_refused() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let tampered = bundle().with(&seed::site_url("assets/app.js"), b"alert('gotcha')");

    let error = seed::import(&ctx, &manifest(), &tampered)
        .await
        .expect_err("a hash mismatch must refuse the import");
    assert!(
        error.contains("assets/app.js") && error.contains("hashes to"),
        "{error}"
    );
    assert!(
        workspace::load(&ctx)
            .await
            .expect("workspace")
            .files
            .is_empty(),
        "a refused import leaves no workspace behind"
    );
}

#[tokio::test]
async fn an_artifact_that_does_not_match_its_declared_hash_is_refused() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let tampered = bundle().with(&seed::artifact_url("hello"), b"\0asm\x01\0\0\0different");

    let error = seed::import(&ctx, &manifest(), &tampered)
        .await
        .expect_err("a hash mismatch must refuse the import");
    assert!(
        error.contains("/seed/blocks/hello.wasm") && error.contains("site/hello"),
        "{error}"
    );
}

/// The artifact size limit exists to bound what the artifact store holds, and
/// a bundle that arrived over the network is no more entitled to exceed it
/// than one that arrived over `POST /b/dev/api/builds/stage`.
#[tokio::test]
async fn an_oversized_artifact_is_refused_and_never_stored() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let huge = vec![0u8; validation::MAX_ARTIFACT_BYTES + 1];
    let mut manifest = manifest();
    manifest.blocks[0].spec.artifact_sha256 = blobs::sha256_hex(&huge);
    let bundle = bundle().with(&seed::artifact_url("hello"), &huge);

    let error = seed::import(&ctx, &manifest, &bundle)
        .await
        .expect_err("an oversized artifact must refuse the import");
    assert!(
        error.contains("artifact is at least") && error.contains("/seed/blocks/hello.wasm"),
        "{error}"
    );
    // Refused beside the hash check and before the `put`: the store never saw
    // it, and neither did the workspace.
    assert!(!artifacts::exists(&ctx, &blobs::sha256_hex(&huge))
        .await
        .expect("exists"));
    assert!(workspace::load(&ctx)
        .await
        .expect("workspace")
        .files
        .is_empty());
}

#[tokio::test]
async fn a_size_that_does_not_match_the_content_is_refused() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let mut manifest = manifest();
    manifest.site[0].size += 1;

    let error = seed::import(&ctx, &manifest, &bundle())
        .await
        .expect_err("a size mismatch must refuse the import");
    assert!(
        error.contains("index.html") && error.contains("bytes"),
        "{error}"
    );
}

/// The served type is derived from the path, so a bundle claiming a different
/// one was produced by an exporter that does not agree with this build about
/// how the file is served.
#[tokio::test]
async fn a_content_type_that_is_not_what_the_path_is_served_as_is_refused() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let mut manifest = manifest();
    manifest.site[0].content_type = "text/plain".to_string();

    let error = seed::import(&ctx, &manifest, &bundle())
        .await
        .expect_err("a content-type mismatch must refuse the import");
    assert!(error.contains("content type"), "{error}");
}

/// The path check runs before the fetch, so a traversing entry is refused for
/// *being* a traversal rather than merely for being unfetchable — the bundle
/// here carries the file, so the only thing that can refuse it is the rule.
#[tokio::test]
async fn a_path_that_escapes_the_workspace_is_refused() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let mut manifest = manifest();
    manifest.site[0].path = "../../elsewhere.html".to_string();
    let bundle = bundle().with(&seed::site_url("../../elsewhere.html"), INDEX);

    let error = seed::import(&ctx, &manifest, &bundle)
        .await
        .expect_err("a traversing path must refuse the import");
    assert!(
        error.contains("site/../../elsewhere.html") && error.contains(r#"segment "..""#),
        "{error}"
    );
}

#[tokio::test]
async fn a_file_the_bundle_does_not_carry_is_refused() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let incomplete = MapFetch::default().with(&seed::site_url("index.html"), INDEX);

    let error = seed::import(&ctx, &manifest(), &incomplete)
        .await
        .expect_err("a missing file must refuse the import");
    assert!(error.contains("assets/app.js"), "{error}");
}

#[tokio::test]
async fn a_bundle_from_another_schema_version_is_refused() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let mut manifest = manifest();
    manifest.schema_version = seed::SCHEMA_VERSION + 1;

    let error = seed::import(&ctx, &manifest, &bundle())
        .await
        .expect_err("an unknown schema version must refuse the import");
    assert!(error.contains("schema_version"), "{error}");
}

/// wafer-run refuses `_` in a block-name segment, so a bundle carrying one
/// describes a block this runtime could never register. The bundle here is
/// complete under the offending name, so the *only* thing that can refuse it
/// is the name rule.
#[tokio::test]
async fn a_block_name_that_cannot_be_registered_is_refused() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let mut manifest = manifest();
    manifest.blocks[0].spec.name = "site/my_shop".to_string();
    let bundle = bundle()
        .with(&seed::artifact_url("my_shop"), ARTIFACT)
        .with(&seed::source_url("my_shop", "src/lib.rs"), LIB_RS);

    let error = seed::import(&ctx, &manifest, &bundle)
        .await
        .expect_err("an unregisterable block name must refuse the import");
    assert!(
        error.contains("site/my_shop") && error.contains("cannot be registered"),
        "{error}"
    );
}

/// Two paths carrying the same asset cost one blob and are charged once — the
/// same rule the files API follows, and the one the workspace quota rests on.
#[tokio::test]
async fn identical_content_at_two_paths_is_stored_once() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let mut manifest = manifest();
    manifest.site.push(file("copy.html", INDEX));
    let bundle = bundle().with(&seed::site_url("copy.html"), INDEX);

    seed::import(&ctx, &manifest, &bundle)
        .await
        .expect("import")
        .expect("fresh");

    let ws = workspace::load(&ctx).await.expect("workspace");
    assert_eq!(ws.files.len(), 4);
    assert_eq!(ws.blob_count, 3, "the duplicate is not a second blob");
    assert_eq!(
        ws.blob_bytes,
        (INDEX.len() + APP_JS.len() + LIB_RS.len()) as u64
    );
}

/// The URL layout is design §10.1's, and both halves of the bundle read it
/// from here — a layout spelled at two sites is one that can be spelled two
/// different ways.
#[test]
fn the_bundle_layout_is_stated_once() {
    assert_eq!(seed::MANIFEST_URL, "/seed/manifest.json");
    assert_eq!(seed::site_url("assets/app.js"), "/seed/site/assets/app.js");
    assert_eq!(seed::artifact_url("hello"), "/seed/blocks/hello.wasm");
    assert_eq!(
        seed::source_url("hello", "src/lib.rs"),
        "/seed/blocks/hello/src/lib.rs"
    );
    assert_eq!(seed::short_name("site/hello"), "hello");
    assert_eq!(seed::short_name("hello"), "hello");
}
