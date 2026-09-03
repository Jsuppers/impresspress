//! `GET /b/dev/api/export` and `GET /b/dev/api/export/manifest` — the sandbox
//! as a folder anyone can serve.
//!
//! Gated on `block-dev` like every other dev-sandbox integration test: the
//! block does not exist in a default-feature build.
//!
//! # What is under test
//!
//! Three claims, and the third is the one the design rests on:
//!
//!  1. the archive carries the runtime shell **with development mode off**,
//!     the site, every compiled block with its source, and a data snapshot;
//!  2. the manifest endpoint describes exactly that archive without producing
//!     it;
//!  3. the `seed/` half of the archive is not an export format at all — it is
//!     the SAME format `seed::import` reads on a cold boot, so an export from
//!     one instance boots as generation 0 in another, shop and all.
//!
//! The archive is read back with the real `zip` crate (a dev-dependency),
//! never with the writer's own parser: `blocks::dev::zip` writing something
//! only it can read would satisfy a round trip through itself and nothing
//! else.
#![cfg(feature = "block-dev")]

use std::{collections::HashMap, io::Read as _};

use impresspress_core::{
    admin_schema,
    blocks::dev::{
        activation::{self, ActivationIntent},
        blobs,
        contracts::ExportManifest,
        data_snapshot::DataSnapshot,
        repo::generations::GenerationCause,
        seed::{self, SeedManifest},
        test_support::{FakeControl, FakeShell},
    },
    test_support::{
        admin_msg, anon_msg, output_body, output_http_header, output_http_status, output_json,
        TestContext,
    },
};
use serde_json::json;
use wafer_core::clients::database as db;
use wafer_run::{AuthLevel, BlockEndpoint, BlockInfo, OutputStream};

// ---------------------------------------------------------------------------
// Table names this crate keeps private, restated here
// ---------------------------------------------------------------------------
//
// Same reason `tests/dev_data_snapshot.rs` restates them: `impresspress-core`
// keeps the products table names `pub(crate)` (and that block's own door
// tests refuse a re-export reachable from outside `repo::products`), and this
// is a separate compilation unit.

const PRODUCTS_TABLE: &str = "impresspress__products__products";

/// A minimal wasm header. Nothing here parses it; the bytes only have to be
/// stable so their sha256 is.
const ARTIFACT: &[u8] = b"\0asm\x01\0\0\0";

/// The page the agent wrote.
const SHOP_HTML: &[u8] = b"<h1>shop</h1>";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn dev_post(ctx: &TestContext, path: &str, body: serde_json::Value) -> OutputStream {
    ctx.dispatch_json(admin_msg("create", path), &body).await
}

/// The `BlockInfo` the `hello` fixture guest reports.
fn hello_info() -> BlockInfo {
    BlockInfo::new("site/hello", "0.1.0", "http-handler@v1", "hello").endpoints(vec![
        BlockEndpoint::get("/b/hello/")
            .auth(AuthLevel::Public)
            .summary("hello"),
    ])
}

/// Standard base64 with padding — how an artifact travels in JSON.
fn b64(bytes: &[u8]) -> String {
    use base64ct::{Base64, Encoding as _};
    Base64::encode_string(bytes)
}

/// Stock the shop the way an agent does — through the products admin API,
/// not by writing rows.
///
/// The difference is not cosmetic. A hand-seeded row carries the handful of
/// columns the test bothered to name; a real create/price/publish leaves rows
/// across `products`, `offers` and `offer_components` with every column the
/// schema declares, populated the way production populates them. The data
/// snapshot exports THOSE, and a round trip that only ever moved a bare
/// product row would pass while a real export failed to import — which is
/// exactly what happened: the browser's export carried a real offer and its
/// component, and importing them was where it broke.
async fn seed_shop(ctx: &TestContext) {
    let product = output_json(
        ctx.dispatch_json(
            admin_msg("create", "/b/products/api/admin/products"),
            &json!({
                "name": "Custom print",
                "slug": "custom-print",
                "description": "Made to order, priced by the page.",
                "currency": "nzd",
                "fulfillment_kind": "manual",
            }),
        )
        .await,
    )
    .await;
    let product_id = product["id"]
        .as_str()
        .unwrap_or_else(|| panic!("create product: {product}"))
        .to_string();

    // A components offer with one typed input, the same shape
    // `tests/e2e/fixtures/shop-fixture.ts` uses — a flat price would exercise
    // neither `offer_components` nor the typed-variable columns.
    let offer = output_json(
        ctx.dispatch_json(
            admin_msg(
                "create",
                &format!("/b/products/api/admin/products/{product_id}/offers"),
            ),
            &json!({
                "name": "Custom print",
                "mode": "payment",
                "currency": "nzd",
                "pricing_model": "components",
                "usage_type": "licensed",
                "billing_scheme": "per_unit",
                "tax_behavior": "exclusive",
                "variables": [{
                    "key": "pages", "kind": "integer", "label": "Pages",
                    "required": true, "minimum": "1", "maximum": "20",
                    "step": "1", "sort_order": 0,
                }],
                "components": [{
                    "key": "pages", "label": "Printed pages", "sort_order": 0,
                    "required": true,
                    "amount": { "type": "per_unit", "input": "pages", "unit_amount_minor": 1500 },
                }],
                "checkout": {},
            }),
        )
        .await,
    )
    .await;
    let offer_id = offer["offer"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("create offer: {offer}"))
        .to_string();

    let published = output_json(
        ctx.dispatch_json(
            admin_msg(
                "create",
                &format!("/b/products/api/admin/products/{product_id}/offers/{offer_id}/publish"),
            ),
            &json!({}),
        )
        .await,
    )
    .await;
    assert_eq!(published["status"], "active", "{published}");

    let live = output_json(
        ctx.dispatch_json(
            admin_msg(
                "update",
                &format!("/b/products/api/admin/products/{product_id}"),
            ),
            &json!({ "status": "active" }),
        )
        .await,
    )
    .await;
    assert_eq!(live["status"], "active", "{live}");
}

/// Every entry of an archive, by path.
fn entries(bytes: Vec<u8>) -> HashMap<String, Vec<u8>> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("the export is a readable zip");
    let mut out = HashMap::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).expect("entry");
        let name = file.name().to_string();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).expect("read entry");
        out.insert(name, buf);
    }
    out
}

/// One entry's content as text.
fn text(entries: &HashMap<String, Vec<u8>>, path: &str) -> String {
    let bytes = entries
        .get(path)
        .unwrap_or_else(|| panic!("{path} is not in the archive: {:?}", sorted(entries)));
    String::from_utf8(bytes.clone()).unwrap_or_else(|_| panic!("{path} is not utf8"))
}

fn sorted(entries: &HashMap<String, Vec<u8>>) -> Vec<String> {
    let mut names: Vec<String> = entries.keys().cloned().collect();
    names.sort();
    names
}

/// A sandbox with the products block, a shop page, one compiled block and one
/// product — the state the scenario in design §16 leaves behind.
async fn shop_instance(control: &std::sync::Arc<FakeControl>) -> TestContext {
    control.set_validated_info(hello_info());
    // `with_auth_added`: the data snapshot's allowlist spans products, admin
    // AND auth (`users`, `local_credentials`, `user_roles` — the visitor's own
    // accounts, `Mode::Replace`d as a set). A fixture without auth's tables
    // exercises the export and import of every table EXCEPT those, which is
    // exactly the half a real browser has and a weaker fixture would not —
    // and `Mode::Replace` is the half that can fail.
    let ctx = TestContext::with_products()
        .await
        .with_auth_added()
        .await
        .with_dev_added_and_shell(control.clone(), std::sync::Arc::new(FakeShell::new()))
        .await;
    dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/index.html", "content": "<h1>shop</h1>", "expected_sha256": null}),
    )
    .await;
    dev_post(
        &ctx,
        "/b/dev/api/blocks",
        json!({"name": "hello", "template": "hello"}),
    )
    .await;
    let staged = output_json(
        dev_post(
            &ctx,
            "/b/dev/api/builds/stage",
            json!({
                "block_name": "hello",
                "artifact_base64": b64(ARTIFACT),
                "compiler_version": "t",
                "diagnostics": [],
                "wafer_guest_version": 1,
            }),
        )
        .await,
    )
    .await;
    assert_eq!(staged["success"], true, "{staged}");
    seed_shop(&ctx).await;
    ctx
}

// ---------------------------------------------------------------------------
// The archive
// ---------------------------------------------------------------------------

#[tokio::test]
async fn export_zip_contains_shell_seed_sources_and_data_with_dev_off() {
    let control = FakeControl::new();
    let ctx = shop_instance(&control).await;

    // Read through `http_codec`, so these are the header and body a client
    // actually receives — `Content-Type` travels as `resp.content_type` meta,
    // not as a `resp.header.*` entry.
    assert_eq!(
        output_http_header(
            ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
                .await,
            "content-type"
        )
        .await
        .as_deref(),
        Some("application/zip")
    );
    assert!(output_http_header(
        ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
            .await,
        "content-disposition"
    )
    .await
    .expect("Content-Disposition")
    .starts_with("attachment; filename=\"impresspress-site-"));

    let declared: u64 = output_http_header(
        ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
            .await,
        "X-Export-Bytes",
    )
    .await
    .expect("X-Export-Bytes")
    .parse()
    .expect("a byte count");
    let bytes = output_body(
        ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
            .await,
    )
    .await;
    assert_eq!(
        declared,
        bytes.len() as u64,
        "X-Export-Bytes is the archive"
    );

    let entries = entries(bytes);
    for expected in [
        "README.md",
        // The shell, at the paths `/asset-manifest.json` listed.
        "index.html",
        "sw.js",
        "loader.js",
        "impresspress_web-abc123.js",
        "impresspress_web_bg-abc123.wasm",
        "vendor/sql-wasm.wasm",
        // The seed, in exactly the layout `seed::import` reads.
        "seed/manifest.json",
        "seed/site/index.html",
        "seed/blocks/hello.wasm",
        "seed/blocks/hello/src/lib.rs",
        "seed/blocks/hello/src/wafer_guest.rs",
        "seed/data.json",
    ] {
        assert!(
            entries.contains_key(expected),
            "missing {expected} in {:?}",
            sorted(&entries)
        );
    }

    // Development mode is OFF, and the ONE line that says so is the one the
    // bundler renders (`impresspress-bundle`'s `sw.js.tmpl`): the isolation
    // passthrough reads the same constant, so it is off too.
    let sw = text(&entries, "sw.js");
    assert!(sw.contains("const DEV_ENABLED = false;"), "{sw}");
    assert!(!sw.contains("const DEV_ENABLED = true;"), "{sw}");
    assert!(sw.contains("initialize({ dev: DEV_ENABLED })"), "{sw}");
    assert!(
        sw.contains("if (DEV_ENABLED && url.pathname !== '/sw.js')"),
        "{sw}"
    );
    // …and `/seed/` is still bypassed, or the exported folder could never
    // import the seed the archive ships beside it.
    assert!(sw.contains("url.pathname.startsWith('/seed/')"), "{sw}");

    // The seed manifest describes the seed half of the archive.
    let manifest: SeedManifest =
        serde_json::from_str(&text(&entries, "seed/manifest.json")).expect("a seed manifest");
    assert_eq!(manifest.schema_version, seed::SCHEMA_VERSION);
    assert!(manifest.source_generation.is_some());
    assert_eq!(manifest.blocks.len(), 1);
    assert_eq!(manifest.blocks[0].spec.name, "site/hello");
    assert_eq!(
        manifest.blocks[0].spec.artifact_sha256,
        blobs::sha256_hex(ARTIFACT)
    );
    // Every referenced file's hash is the exporter's own (amendment 17): the
    // data snapshot is a `SeedFile` like the rest, not a bare path.
    let data = manifest.data.as_ref().expect("the bundle carries data");
    assert_eq!(data.path, "data.json");
    assert_eq!(data.content_type, "application/json");
    let data_bytes = entries.get("seed/data.json").expect("seed/data.json");
    assert_eq!(data.sha256, blobs::sha256_hex(data_bytes));
    assert_eq!(data.size, data_bytes.len() as u64);
    let site = manifest
        .site
        .iter()
        .find(|f| f.path == "index.html")
        .expect("index.html");
    assert_eq!(site.sha256, blobs::sha256_hex(SHOP_HTML));

    let snapshot: DataSnapshot = serde_json::from_slice(data_bytes).expect("a data snapshot");
    assert_eq!(snapshot.tables[PRODUCTS_TABLE].len(), 1);

    // The README carries this export's own numbers, not a template's.
    let readme = text(&entries, "README.md");
    assert!(
        readme.contains(&manifest.source_generation.clone().unwrap()),
        "{readme}"
    );
    assert!(readme.contains("password hashes"), "{readme}");
    assert!(!readme.contains("{{"), "unsubstituted hole in {readme}");

    // The compiler tree and the exporting deployment's own `seed/` are never
    // copied: the archive writes its own `seed/`, and the 72 MiB toolchain is
    // for a `/b/dev` the exported site does not have.
    assert!(
        !entries
            .keys()
            .any(|path| path.starts_with("__impresspress_dev/")),
        "{:?}",
        sorted(&entries)
    );
}

/// The deployment's own overlays are excluded by rule, not by luck: a shell
/// that DOES list them (a bundler that ran after the overlays, say) still
/// exports without them.
#[tokio::test]
async fn the_compiler_tree_and_the_deployments_own_seed_are_never_copied() {
    let control = FakeControl::new();
    control.set_validated_info(hello_info());
    let shell = FakeShell::new()
        .with("__impresspress_dev/compiler/manifest.json", b"{}")
        .with("seed/manifest.json", b"{\"schema_version\":1}")
        .with("seed/site/index.html", b"<h1>welcome</h1>");
    let ctx = TestContext::with_admin()
        .await
        .with_dev_added_and_shell(control, std::sync::Arc::new(shell))
        .await;
    dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/index.html", "content": "<h1>shop</h1>", "expected_sha256": null}),
    )
    .await;

    let bytes = output_body(
        ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
            .await,
    )
    .await;
    let entries = entries(bytes);
    assert!(
        !entries
            .keys()
            .any(|path| path.starts_with("__impresspress_dev/")),
        "{:?}",
        sorted(&entries)
    );
    // The archive's own `seed/site/index.html` is the exported site, NOT the
    // deployment's welcome page that the shell also listed.
    assert_eq!(
        entries.get("seed/site/index.html").map(Vec::as_slice),
        Some(SHOP_HTML)
    );
}

/// The one edit the export makes to a shell file has to be verifiable, so a
/// shell whose `sw.js` does not carry the marker is a 500 — never a silent
/// pass-through of a service worker that would come up as a second sandbox.
#[tokio::test]
async fn a_shell_whose_sw_js_has_no_dev_marker_is_refused() {
    let control = FakeControl::new();
    let shell = FakeShell::new().with("sw.js", b"await initialize({ dev: true });");
    let ctx = TestContext::with_admin()
        .await
        .with_dev_added_and_shell(control, std::sync::Arc::new(shell))
        .await;
    dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/index.html", "content": "x", "expected_sha256": null}),
    )
    .await;

    let status = output_http_status(
        ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
            .await,
    )
    .await;
    assert_eq!(status, 500);
}

/// And a shell that cannot be listed at all: an export with no runtime in it
/// is a folder that cannot be served, so it must fail rather than produce one.
#[tokio::test]
async fn a_shell_that_cannot_be_listed_is_refused() {
    let control = FakeControl::new();
    let shell = FakeShell::new().failing_to_list("/asset-manifest.json: HTTP 404");
    let ctx = TestContext::with_admin()
        .await
        .with_dev_added_and_shell(control, std::sync::Arc::new(shell))
        .await;
    dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/index.html", "content": "x", "expected_sha256": null}),
    )
    .await;

    let status = output_http_status(
        ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
            .await,
    )
    .await;
    assert_eq!(status, 500);
}

/// Two exports of one generation are byte-identical — the WHOLE archive,
/// README included.
///
/// `ZipWriter` fixes every entry's timestamp, `assemble` fixes the entry
/// order, and the README is dated from the ACTIVE GENERATION's `created_at`
/// rather than the wall clock. That last one is the point: an export is a
/// function of what is live, and a README carrying the download's own
/// timestamp would have made the one entry that differs between two otherwise
/// identical exports the one entry nobody diffing them cares about.
#[tokio::test]
async fn two_exports_of_the_same_generation_are_identical() {
    let control = FakeControl::new();
    let ctx = shop_instance(&control).await;
    let first = output_body(
        ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
            .await,
    )
    .await;
    let second = output_body(
        ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
            .await,
    )
    .await;
    assert_eq!(
        first, second,
        "two exports of one generation must be identical, README included"
    );
}

/// And the date the README carries is the generation's own, so it says when
/// the site came to be rather than when someone pressed the button.
#[tokio::test]
async fn the_readme_is_dated_by_the_generation_not_the_download() {
    let control = FakeControl::new();
    let ctx = shop_instance(&control).await;
    let archive = entries(
        output_body(
            ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
                .await,
        )
        .await,
    );
    let status = output_json(
        ctx.dispatch(admin_msg("retrieve", "/b/dev/api/status"))
            .await,
    )
    .await;
    let created_at = status["active_generation"]["created_at"]
        .as_str()
        .expect("created_at")
        .to_string();
    assert!(
        text(&archive, "README.md").contains(&created_at),
        "the README must carry the generation's own timestamp"
    );
}

/// The exported `sw.js` keeps `/seed/` on the bypass list — without it the
/// folder could never import the seed shipped beside it — and DROPS the
/// compiler's, because the export copies none of those assets. A bypass for a
/// tree that is not there waves every request under the prefix past the
/// runtime to a 404 from the static host.
#[tokio::test]
async fn the_exported_sw_drops_the_compiler_bypass_and_keeps_the_seed_one() {
    let control = FakeControl::new();
    let ctx = shop_instance(&control).await;
    let archive = entries(
        output_body(
            ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
                .await,
        )
        .await,
    );
    let sw = text(&archive, "sw.js");

    assert!(
        !sw.contains("__impresspress_dev"),
        "the compiler bypass must be gone; {sw}"
    );
    assert!(sw.contains("url.pathname.startsWith('/seed/')"), "{sw}");
    // Only that one clause: the app's other bypasses are untouched, and the
    // expression still reads the way the bundler would have rendered it for a
    // bundle that never asked for the compiler.
    assert!(sw.contains("url.pathname.startsWith('/sql-')"), "{sw}");
    assert!(
        sw.contains(
            "if (url.pathname.startsWith('/sql-') || url.pathname.startsWith('/seed/')) \
             { return; }"
        ),
        "the remaining expression must be exactly what a compiler-less bundle renders; {sw}"
    );
}

/// A shell with no compiler bypass to begin with is left alone — CI's
/// foundations bundle ships no compiler, and its absence is an ordinary
/// build rather than a mismatched one.
#[tokio::test]
async fn a_shell_with_no_compiler_bypass_is_exported_unchanged() {
    let control = FakeControl::new();
    control.set_validated_info(hello_info());
    let plain_sw = "const DEV_ENABLED = true;\n\
                    if (url.pathname.startsWith('/seed/')) { return; }\n";
    let shell = FakeShell::new().with("sw.js", plain_sw.as_bytes());
    let ctx = TestContext::with_admin()
        .await
        .with_dev_added_and_shell(control, std::sync::Arc::new(shell))
        .await;
    dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/index.html", "content": "x", "expected_sha256": null}),
    )
    .await;

    let archive = entries(
        output_body(
            ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
                .await,
        )
        .await,
    );
    assert_eq!(
        text(&archive, "sw.js"),
        plain_sw.replace("= true;", "= false;"),
        "only the dev flag may change on a shell with no compiler bypass"
    );
}

// ---------------------------------------------------------------------------
// Source provenance
// ---------------------------------------------------------------------------

/// The artifact comes from the live generation and the sources come from the
/// workspace as it stands, so the two CAN disagree — an agent that edited
/// `blocks/hello/src/lib.rs` and did not recompile leaves an export whose
/// `.wasm` and `src/` describe different programs. The README says which,
/// per block, so it is never silent.
#[tokio::test]
async fn the_readme_says_whether_each_blocks_sources_match_its_artifact() {
    let control = FakeControl::new();
    let ctx = shop_instance(&control).await;

    // `shop_instance` staged without a `source_manifest_sha256`, so the build
    // row records none and the verdict is honestly "unknown" rather than a
    // guess in either direction.
    let archive = entries(
        output_body(
            ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
                .await,
        )
        .await,
    );
    let readme = text(&archive, "README.md");
    assert!(
        readme.contains("site/hello: no source digest recorded"),
        "{readme}"
    );
}

/// And when the compile DID record a digest, the verdict is a real
/// comparison: matching sources read "current", and one edited byte reads
/// "SOURCES DIFFER".
#[tokio::test]
async fn a_recorded_source_digest_is_compared_against_the_workspace() {
    let control = FakeControl::new();
    control.set_validated_info(hello_info());
    let ctx = TestContext::with_admin()
        .await
        .with_dev_added_and_shell(control.clone(), std::sync::Arc::new(FakeShell::new()))
        .await;
    dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/index.html", "content": "<h1>shop</h1>", "expected_sha256": null}),
    )
    .await;
    dev_post(
        &ctx,
        "/b/dev/api/blocks",
        json!({"name": "hello", "template": "hello"}),
    )
    .await;

    // The digest the page computes: sorted `"<crate-relative path>\0<sha>\n"`
    // lines over the block's sources, exactly as `dev.js`'s `snapshotBlock`
    // builds it. Restated here rather than reached for, because the whole
    // point of the check is that two independent computations of it agree.
    let listed = output_json(
        ctx.dispatch({
            let mut msg = admin_msg("retrieve", "/b/dev/api/files");
            msg.set_meta("req.query.prefix", "blocks/hello/");
            msg
        })
        .await,
    )
    .await;
    let mut lines: Vec<String> = listed["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| {
            format!(
                "{}\0{}\n",
                f["path"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("blocks/hello/"),
                f["sha256"].as_str().unwrap()
            )
        })
        .collect();
    lines.sort();
    let digest = impresspress_core::blocks::dev::blobs::sha256_hex(lines.concat().as_bytes());

    let staged = output_json(
        dev_post(
            &ctx,
            "/b/dev/api/builds/stage",
            json!({
                "block_name": "hello",
                "artifact_base64": b64(ARTIFACT),
                "source_manifest_sha256": digest,
                "compiler_version": "t",
                "diagnostics": [],
                "wafer_guest_version": 1,
            }),
        )
        .await,
    )
    .await;
    assert_eq!(staged["success"], true, "{staged}");

    let readme = text(
        &entries(
            output_body(
                ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
                    .await,
            )
            .await,
        ),
        "README.md",
    );
    assert!(
        readme.contains("site/hello: sources match the compiled artifact"),
        "{readme}"
    );

    // Now edit a source without recompiling. The artifact in the generation is
    // unchanged; the workspace is not.
    let read = output_json(
        dev_post(
            &ctx,
            "/b/dev/api/files/read",
            json!({"path": "blocks/hello/src/lib.rs"}),
        )
        .await,
    )
    .await;
    dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({
            "path": "blocks/hello/src/lib.rs",
            "content": format!("{}\n// edited\n", read["content"].as_str().unwrap()),
            "expected_sha256": read["sha256"],
        }),
    )
    .await;

    let readme = text(
        &entries(
            output_body(
                ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
                    .await,
            )
            .await,
        ),
        "README.md",
    );
    assert!(readme.contains("site/hello: SOURCES DIFFER"), "{readme}");
}

// ---------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn export_manifest_previews_the_archive_without_building_it() {
    let ctx = TestContext::with_dev_added_and_shell(
        TestContext::with_admin().await,
        FakeControl::new(),
        std::sync::Arc::new(FakeShell::new()),
    )
    .await;
    dev_post(
        &ctx,
        "/b/dev/api/files/write",
        json!({"path": "site/index.html", "content": "x", "expected_sha256": null}),
    )
    .await;

    let m = output_json(
        ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export/manifest"))
            .await,
    )
    .await;
    assert_eq!(m["site_files"], 1);
    assert_eq!(m["blocks"], 0);
    assert_eq!(m["shell_files"], 7, "{m}");
    assert!(m["total_bytes"].as_u64().expect("total_bytes") > 0);
    assert!(!m["generation_id"]
        .as_str()
        .expect("generation_id")
        .is_empty());
}

/// The manifest is not a second derivation of what an export contains — it is
/// a summary of the same assembled entry list, so every path and size it
/// publishes is in the archive with exactly that size.
#[tokio::test]
async fn the_manifest_describes_the_archive_entry_for_entry() {
    let control = FakeControl::new();
    let ctx = shop_instance(&control).await;

    let manifest: ExportManifest = serde_json::from_value(
        output_json(
            ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export/manifest"))
                .await,
        )
        .await,
    )
    .expect("an ExportManifest");
    let entries = entries(
        output_body(
            ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
                .await,
        )
        .await,
    );

    let mut listed: Vec<String> = manifest.files.iter().map(|f| f.path.clone()).collect();
    listed.sort();
    assert_eq!(listed, sorted(&entries));
    for file in &manifest.files {
        // The README is the one entry whose size can move between two calls
        // (it carries the wall-clock date), so it is compared for presence
        // rather than for length.
        if file.path == "README.md" {
            continue;
        }
        assert_eq!(
            entries[&file.path].len() as u64,
            file.bytes,
            "{} is a different size in the archive",
            file.path
        );
    }
    assert_eq!(manifest.blocks, 1);
    assert_eq!(manifest.site_files, 1);
    assert_eq!(manifest.tables[PRODUCTS_TABLE], 1);
    // Every allowlisted table is reported, empty ones included — "no
    // products" and "no products table in this build" must not read the same.
    assert!(manifest.tables.contains_key(admin_schema::VARIABLES_TABLE));
}

/// Nothing published, nothing to export — and the refusal says what to do
/// about it rather than 500ing on an absent generation.
#[tokio::test]
async fn exporting_a_fresh_instance_is_refused() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    assert_eq!(
        output_http_status(
            ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export"))
                .await
        )
        .await,
        400
    );
    assert_eq!(
        output_http_status(
            ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export/manifest"))
                .await
        )
        .await,
        400
    );
}

/// Both routes are `/b/dev`'s, so both are admin-only at the router.
#[tokio::test]
async fn the_export_routes_are_admin_only() {
    let control = FakeControl::new();
    let ctx = shop_instance(&control).await;
    for path in ["/b/dev/api/export", "/b/dev/api/export/manifest"] {
        let status = output_http_status(ctx.dispatch(anon_msg("retrieve", path)).await).await;
        assert!(
            status == 401 || status == 403,
            "{path} answered an anonymous caller with {status}"
        );
    }
}

// ---------------------------------------------------------------------------
// The round trip
// ---------------------------------------------------------------------------

/// The claim the whole format rests on: an export is a seed bundle. Export
/// from A, feed the archive's `seed/` entries to B's importer, and B serves
/// the same shop with the same product.
#[tokio::test]
async fn an_exported_seed_imports_into_a_fresh_instance() {
    let a_control = FakeControl::new();
    let a = shop_instance(&a_control).await;
    let archive =
        entries(output_body(a.dispatch(admin_msg("retrieve", "/b/dev/api/export")).await).await);

    let manifest: SeedManifest =
        serde_json::from_slice(&archive["seed/manifest.json"]).expect("a seed manifest");
    // The importer fetches by URL under `/seed/`; the archive holds the same
    // paths without the leading slash. That correspondence IS the format.
    let fetch = ArchiveFetch { archive };

    let b_control = FakeControl::new();
    b_control.set_validated_info(hello_info());
    let b = TestContext::with_products()
        .await
        .with_auth_added()
        .await
        .with_dev_added_and_shell(b_control.clone(), std::sync::Arc::new(FakeShell::new()))
        .await;
    let generation = seed::import(&b, b_control.as_ref(), &manifest, &fetch)
        .await
        .expect("import")
        .expect("a fresh instance imports");
    activation::request(
        &b,
        &b.dev_shared(),
        GenerationCause::Seed,
        ActivationIntent::Seed {
            manifest: generation,
        },
    )
    .await
    .expect("activate the imported generation");

    // The shop is being served.
    assert_eq!(
        b.storage_get("wafer-run/web", "site", "index.html")
            .await
            .expect("published index"),
        SHOP_HTML.to_vec()
    );
    // The block is live, with its source alongside it.
    let live = b_control.live_blocks().expect("a live set");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].name, "site/hello");
    // And the data came with it.
    let products = db::list_all(&b, PRODUCTS_TABLE, Vec::new())
        .await
        .expect("products");
    assert_eq!(products.len(), 1);
    assert_eq!(products[0].data["slug"], json!("custom-print"));
}

/// A [`seed::SeedFetch`] over the archive's own entries, keyed the way the
/// importer asks for them.
struct ArchiveFetch {
    archive: HashMap<String, Vec<u8>>,
}

impl seed::SeedFetch for ArchiveFetch {
    fn get<'a>(&'a self, url: &'a str) -> seed::FetchFuture<'a> {
        Box::pin(async move {
            let path = url.trim_start_matches('/');
            self.archive
                .get(path)
                .cloned()
                .ok_or_else(|| format!("{url}: not in the archive"))
        })
    }
}
