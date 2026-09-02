# Dev Sandbox Plan 4 — Export, data seed, retention and the full scenario

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One button (or `dev_export`) produces a zip that boots the visitor's shop — pages, backend blocks and products — from any static file server; storage stays bounded; the whole spec scenario runs green in CI; the sandbox is documented and measured.

**Architecture:** The dev block assembles the export inside the service worker: the runtime shell (fetched from the bundle's own asset manifest through a `ShellSource` the browser crate provides, with the dev flag rendered false), a `seed/` tree (manifest, site files, artifacts, block sources) that Plan 1's seed-on-boot already installs, and `seed/data.json` — rows from an explicit table allowlist that the importer applies through the typed database client. A stored-entry zip writer keeps it dependency-light. Retention and blob GC close the storage loop.

**Tech Stack:** Rust (stored zip writer + `crc32fast`), typed `wafer_core::clients::database`, Playwright, Python `zipfile`/`http.server` in the e2e harness.

**Spec:** `docs/superpowers/specs/2026-09-02-dev-sandbox-design.md` — §6.6 quotas, §7.3 retention, §10, §16 (scenario steps 6–7, size/time), §18, §21.

**Depends on:** Plans 0–3 merged.

## Global Constraints

- **The export is a closed list.** `export::TABLE_ALLOWLIST` and `export::TABLE_EXCLUDED` together must cover every collection the products, admin and auth blocks declare; a test fails when a new table appears in neither.
- **No secrets leave.** `impresspress__admin__variables` rows with `sensitive = true` or a key starting with `IMPRESSPRESS_` are never exported; sessions, tokens, audit rows and every payment/provider table are excluded by name.
- **Typed writes on import.** `data.json` is applied with `db::upsert`/`db::create`/`db::delete_where` — no SQL text is generated or executed by the sandbox.
- **Stored entries only.** The zip writer emits method 0; the static host compresses on the wire. Every entry has a correct CRC-32 and the central directory is complete; a test reads the archive back with the `zip` crate.
- **The exported bundle has dev mode off** (`initialize({ dev: false })`) and keeps `/seed/` on the bypass list; a test asserts both strings in the exported `sw.js`.
- Retention keeps the last 20 generations; GC deletes only blobs and artifacts referenced by no retained generation and not by the workspace.

---

### Task 1: A stored-entry zip writer

**Files:**
- Create: `crates/impresspress-core/src/blocks/dev/zip.rs`
- Modify: `crates/impresspress-core/Cargo.toml` (`crc32fast = "1"`; dev-dependency `zip = { version = "2", default-features = false }` for read-back)

**Interfaces:**
- Produces:

```rust
pub struct ZipWriter { buf: Vec<u8>, entries: Vec<CentralEntry> }
impl ZipWriter {
    pub fn new() -> Self;
    pub fn add(&mut self, path: &str, bytes: &[u8]) -> Result<(), ZipError>;   // path: forward slashes, no leading '/', ≤ 65535 bytes, unique
    pub fn finish(self) -> Vec<u8>;
}
```

- [ ] **Step 1: Write the failing tests** (inside `zip.rs`)

```rust
    #[test]
    fn archive_reads_back_with_the_zip_crate() {
        let mut w = ZipWriter::new();
        w.add("README.md", b"hello").unwrap();
        w.add("seed/site/index.html", b"<h1>x</h1>").unwrap();
        w.add("seed/blocks/hello.wasm", &[0, 97, 115, 109, 1, 0, 0, 0]).unwrap();
        let bytes = w.finish();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(archive.len(), 3);
        let mut f = archive.by_name("seed/site/index.html").unwrap();
        let mut s = String::new(); std::io::Read::read_to_string(&mut f, &mut s).unwrap();
        assert_eq!(s, "<h1>x</h1>");
        assert_eq!(f.compression(), zip::CompressionMethod::Stored);
        assert_eq!(f.crc32(), crc32fast::hash(b"<h1>x</h1>"));
    }

    #[test]
    fn duplicate_and_absolute_paths_are_rejected() {
        let mut w = ZipWriter::new();
        w.add("a", b"1").unwrap();
        assert!(matches!(w.add("a", b"2"), Err(ZipError::Duplicate(_))));
        assert!(matches!(w.add("/a", b"2"), Err(ZipError::BadPath(_))));
        assert!(matches!(w.add("a\\b", b"2"), Err(ZipError::BadPath(_))));
    }
```

- [ ] **Step 2: Run to verify they fail** — module missing.

- [ ] **Step 3: Implement** — local file header (`PK\x03\x04`, version 20, flags 0x0800 for UTF-8 names, method 0, DOS time/date fixed to 2026-01-01 00:00 for reproducibility, crc32, sizes, name), data, then central directory entries (`PK\x01\x02`) and the end record (`PK\x05\x06`). Refuse when total size would exceed 4 GiB (no ZIP64). ~120 lines.

- [ ] **Step 4: Run** `cargo test -p impresspress-core --features block-dev zip::`. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/impresspress-core Cargo.lock
git commit -m "feat(dev): stored-entry zip writer"
```

---

### Task 2: The data snapshot — allowlist, export and typed import

**Files:**
- Create: `crates/impresspress-core/src/blocks/dev/data_snapshot.rs`
- Modify: `crates/impresspress-core/src/blocks/dev/seed.rs` (`SeedManifest.data: Option<String>`; apply on import)
- Create: `crates/impresspress-core/tests/dev_data_snapshot.rs`

**Interfaces:**
- Produces:

```rust
pub const SCHEMA_VERSION: u32 = 1;
#[derive(Serialize, Deserialize)]
pub struct DataSnapshot { pub schema_version: u32, pub tables: BTreeMap<String, Vec<serde_json::Map<String, Value>>> }
pub enum Mode { Upsert, Replace }                                   // Replace: delete every row first (users, user_roles)
pub const TABLE_ALLOWLIST: &[(&str, Mode)];                          // products, offers, groups, types, presets (Upsert);
                                                                     // impresspress__admin__variables (Upsert, filtered); wafer_run__auth__users + user_roles (Replace)
pub const TABLE_EXCLUDED: &[&str];                                   // purchases, line items, refunds, payment links, provider ops, stripe events,
                                                                     // webhook leases, subscriptions, sessions, refresh/verification tokens, audit log, block_settings
pub async fn export(ctx: &dyn Context) -> Result<DataSnapshot, WaferError>;
pub async fn import(ctx: &dyn Context, snapshot: &DataSnapshot) -> Result<ImportReport, WaferError>;   // ImportReport { tables: BTreeMap<String, usize> }
pub fn variable_is_exportable(row: &serde_json::Map<String, Value>) -> bool;   // !sensitive && !key.starts_with("IMPRESSPRESS_")
```
  The exact table names come from the blocks' repo constants (`products::repo::*::TABLE`, `admin::repo::*`, `auth::repo::*`) — reference them, do not retype strings.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn every_declared_table_of_the_three_blocks_has_an_export_decision() {
    let mut declared: Vec<String> = Vec::new();
    for info in [ProductsBlock::new().info(), AdminBlock::new().info(), AuthBlock::default_for_tests().info()] {
        declared.extend(info.collections.iter().map(|c| c.name.clone()));
    }
    let decided: std::collections::BTreeSet<&str> = TABLE_ALLOWLIST.iter().map(|(t, _)| *t).chain(TABLE_EXCLUDED.iter().copied()).collect();
    let undecided: Vec<&String> = declared.iter().filter(|t| !decided.contains(t.as_str())).collect();
    assert!(undecided.is_empty(), "tables with no export decision: {undecided:?} — add each to TABLE_ALLOWLIST or TABLE_EXCLUDED deliberately");
}

#[tokio::test]
async fn export_carries_products_but_never_secrets_or_orders() {
    let ctx = TestContext::with_products().await.with_dev_added(FakeControl::new()).await;
    seed_product_and_order(&ctx).await;                      // one active product with an offer, one purchase, one sensitive variable
    let snap = data_snapshot::export(&ctx.context()).await.unwrap();
    assert_eq!(snap.tables[products::repo::products::TABLE].len(), 1);
    assert!(!snap.tables.contains_key(products::repo::purchases::TABLE));
    let vars = &snap.tables[admin::repo::variables::TABLE];
    assert!(vars.iter().all(|v| v["sensitive"] != true && !v["key"].as_str().unwrap().starts_with("IMPRESSPRESS_")));
    assert!(!vars.iter().any(|v| v["key"] == "WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_PASSWORD"));
}

#[tokio::test]
async fn import_replaces_users_and_upserts_products_so_ownership_survives() {
    let src = TestContext::with_products().await.with_dev_added(FakeControl::new()).await;
    seed_product_and_order(&src).await;
    let snap = data_snapshot::export(&src.context()).await.unwrap();
    let admin_id = snap.tables[auth::repo::users::TABLE][0]["id"].as_str().unwrap().to_string();

    let dst = TestContext::with_products().await.with_dev_added(FakeControl::new()).await;   // fresh: has its own bootstrap admin with another id
    let report = data_snapshot::import(&dst.context(), &snap).await.unwrap();
    assert_eq!(report.tables[auth::repo::users::TABLE], 1);
    let users = db::list_all(&dst.context(), auth::repo::users::TABLE).await.unwrap();
    assert_eq!(users.len(), 1, "replace semantics: the fresh bootstrap admin is gone");
    assert_eq!(users[0].id, admin_id);
    let products = db::list_all(&dst.context(), products::repo::products::TABLE).await.unwrap();
    assert_eq!(products[0].data["owner_id"], admin_id);
    // Importing again is idempotent.
    data_snapshot::import(&dst.context(), &snap).await.unwrap();
    assert_eq!(db::list_all(&dst.context(), products::repo::products::TABLE).await.unwrap().len(), 1);
}

#[tokio::test]
async fn seed_import_applies_data_json_when_present() {
    let ctx = TestContext::with_products().await.with_dev_added(FakeControl::new()).await;
    let manifest = SeedManifest { schema_version: 1, source_generation: None, site: vec![index_file()], blocks: vec![], data: Some("seed/data.json".into()) };
    let fetch = fake_fetch(&[("seed/site/index.html", b"<h1>shop</h1>"), ("seed/data.json", &serde_json::to_vec(&one_product_snapshot()).unwrap())]);
    seed::import(&ctx.context(), &manifest, &fetch).await.unwrap();
    assert_eq!(db::list_all(&ctx.context(), products::repo::products::TABLE).await.unwrap().len(), 1);
}
```

- [ ] **Step 2: Run to verify they fail** — module missing.

- [ ] **Step 3: Implement** — `export`: for each allowlisted table `db::list_all` → rows as JSON maps (`Record.data` plus `id`); variables filtered by `variable_is_exportable`. `import`: schema version check; for `Replace` tables `db::delete_where(ctx, table, &[])` (all rows) then `db::create` each; for `Upsert` tables `db::upsert(ctx, table, row, conflict_columns = ["id"], OnConflict::SetColumns(all non-id keys))`; tables outside the allowlist in the file are refused (`InvalidArgument`). Wire it into `seed::import` after the files and before returning the generation manifest.

- [ ] **Step 4: Run** `cargo test -p impresspress-core --features block-dev --test dev_data_snapshot --test dev_seed`. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/impresspress-core
git commit -m "feat(dev): data snapshot — allowlisted tables exported and imported through typed writes"
```

---

### Task 3: `GET /b/dev/api/export` and the `dev_export` tool

**Files:**
- Create: `crates/impresspress-core/src/blocks/dev/export.rs`
- Modify: `crates/impresspress-core/src/blocks/dev/{mod.rs, contracts.rs, control.rs, tools.rs, assets/dev.js, page.rs}`
- Modify: `crates/impresspress-web/src/dev_runtime.rs` (`BrowserShellSource`)
- Modify: `crates/impresspress-bundle/src/bundle/manifest.rs` (`files: Vec<String>` — every file under the dist dir, relative, sorted)
- Create: `crates/impresspress-core/tests/dev_export.rs`

**Interfaces:**
- Produces (`control.rs`):

```rust
#[wafer_block::wafer_async_trait]
pub trait ShellSource: Send + Sync {
    /// Every file of the running static shell, as listed by /asset-manifest.json `files`.
    async fn list(&self) -> Result<Vec<String>, String>;
    async fn fetch(&self, path: &str) -> Result<Vec<u8>, String>;
}
pub struct DevShared { pub control: Arc<dyn RuntimeControl>, pub shell: Arc<dyn ShellSource>, ... }
```
- Produces: `GET /b/dev/api/export/manifest` → `ExportManifest { generation_id, files: Vec<{path, bytes}>, total_bytes, shell_files: u32, site_files: u32, blocks: u32, tables: BTreeMap<String, usize> }`; `GET /b/dev/api/export` → `application/zip`, `Content-Disposition: attachment; filename="impresspress-site-<gen8>.zip"`, `X-Export-Bytes`. Zip layout exactly as spec §10.1 with `seed/data.json` in place of `data.sql` (record as spec amendment #9). `README.md` inside the zip is rendered from `templates/export-readme.md` with the generation id, date, file counts and credentials.
- Produces: `dev_export` page-local tool — fetches the manifest, then the zip, triggers a browser download named as above, returns the manifest as `structuredContent`.
- Consumes: `zip::ZipWriter`, `data_snapshot::export`, `workspace`, `blobs`, the active `GenerationManifest`.

- [ ] **Step 1: Write the failing tests** (`tests/dev_export.rs`, with a `FakeShell` listing `index.html`, `sw.js`, `loader.js`, `impresspress_web-abc123.js`, `impresspress_web_bg-abc123.wasm`, `vendor/sql-wasm.wasm`, `asset-manifest.json`; its `sw.js` contains `initialize({ dev: true })` and `url.pathname.startsWith('/seed/')`)

```rust
#[tokio::test]
async fn export_zip_contains_shell_seed_sources_and_data_with_dev_off() {
    let control = FakeControl::new();
    control.set_validated_info(hello_info("site/hello"));
    let ctx = TestContext::with_products().await.with_dev_added_and_shell(control.clone(), FakeShell::new()).await;
    dev_post(&ctx, "/b/dev/api/files/write", json!({"path": "site/index.html", "content": "<h1>shop</h1>", "expected_sha256": null})).await;
    dev_post(&ctx, "/b/dev/api/blocks", json!({"name": "hello", "template": "hello"})).await;
    dev_post(&ctx, "/b/dev/api/builds/stage", json!({"block_name": "hello", "artifact_base64": b64(b"\0asm\x01\0\0\0"), "compiler_version": "t", "diagnostics": [], "wafer_guest_version": 1})).await;
    seed_product(&ctx).await;

    let out = ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export")).await;
    assert_eq!(output_header(out, "content-type").await.as_deref(), Some("application/zip"));
    let bytes = output_body(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export")).await).await;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let names: Vec<String> = (0..archive.len()).map(|i| archive.by_index(i).unwrap().name().to_string()).collect();
    for expected in ["README.md", "index.html", "sw.js", "loader.js", "impresspress_web-abc123.js", "impresspress_web_bg-abc123.wasm", "vendor/sql-wasm.wasm",
                     "seed/manifest.json", "seed/site/index.html", "seed/blocks/hello.wasm", "seed/blocks/hello/src/lib.rs", "seed/blocks/hello/src/wafer_guest.rs", "seed/data.json"] {
        assert!(names.contains(&expected.to_string()), "missing {expected} in {names:?}");
    }
    let sw = read_entry(&mut archive, "sw.js");
    assert!(sw.contains("initialize({ dev: false })"));
    assert!(sw.contains("url.pathname.startsWith('/seed/')"));
    let manifest: SeedManifest = serde_json::from_str(&read_entry(&mut archive, "seed/manifest.json")).unwrap();
    assert_eq!(manifest.blocks[0].spec.name, "site/hello");
    assert_eq!(manifest.data.as_deref(), Some("seed/data.json"));
    let data: DataSnapshot = serde_json::from_str(&read_entry(&mut archive, "seed/data.json")).unwrap();
    assert_eq!(data.tables[products::repo::products::TABLE].len(), 1);
}

#[tokio::test]
async fn export_manifest_previews_the_archive_without_building_it() {
    let ctx = TestContext::with_dev_added_and_shell(FakeControl::new(), FakeShell::new()).await;
    dev_post(&ctx, "/b/dev/api/files/write", json!({"path": "site/index.html", "content": "x", "expected_sha256": null})).await;
    let m = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/export/manifest")).await).await;
    assert_eq!(m["site_files"], 1);
    assert!(m["total_bytes"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn an_exported_seed_imports_into_a_fresh_instance() {
    // Round trip: export from A, feed the seed entries of the zip to B's seed importer, B serves the shop.
    let a = TestContext::with_products().await.with_dev_added_and_shell(FakeControl::new(), FakeShell::new()).await;
    dev_post(&a, "/b/dev/api/files/write", json!({"path": "site/index.html", "content": "<h1>shop</h1>", "expected_sha256": null})).await;
    seed_product(&a).await;
    let bytes = output_body(a.dispatch(admin_msg("retrieve", "/b/dev/api/export")).await).await;

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut entries: std::collections::HashMap<String, Vec<u8>> = Default::default();
    for i in 0..archive.len() {
        let mut f = archive.by_index(i).unwrap();
        let mut buf = Vec::new(); std::io::Read::read_to_end(&mut f, &mut buf).unwrap();
        entries.insert(f.name().to_string(), buf);
    }
    let manifest: SeedManifest = serde_json::from_slice(&entries["seed/manifest.json"]).unwrap();
    let fetch = move |path: &str| { let e = entries.clone(); let p = path.to_string(); Box::pin(async move { e.get(&p).cloned().ok_or_else(|| format!("no entry {p}")) }) as BoxFuture<'static, Result<Vec<u8>, String>> };

    let b = TestContext::with_products().await.with_dev_added(FakeControl::new()).await;
    let gen0 = seed::import(&b.context(), &manifest, &fetch).await.unwrap().expect("fresh instance imports");
    impresspress_core::blocks::dev::activation::request(&b.context(), &b.dev_shared(), GenerationCause::Seed, gen0).await.unwrap();
    assert_eq!(b.storage_get("wafer-run/web", "site", "index.html").await.unwrap(), b"<h1>shop</h1>");
    assert_eq!(db::list_all(&b.context(), products::repo::products::TABLE).await.unwrap().len(), 1);
}
```

- [ ] **Step 2: Run to verify they fail** — 404s.

- [ ] **Step 3: Implement `export.rs`** — `build(ctx, shared) -> Result<Vec<u8>, WaferError>`: active generation manifest (400 when none); shell files via `shared.shell` (rewrite `sw.js` with `.replace("initialize({ dev: true })", "initialize({ dev: false })")` and assert the marker was present — a missing marker is an `Internal` error, never a silent pass-through); `seed/manifest.json` (`SeedManifest` with `data`); `seed/site/<path>` from blobs; `seed/blocks/<name>.wasm` from artifacts; `seed/blocks/<name>/src/**` from the workspace; `seed/data.json`; `README.md`. `manifest_preview` computes the same list with sizes and no bytes. Handlers with the headers above.

- [ ] **Step 4: `BrowserShellSource`** in `dev_runtime.rs`: `list` = `self.fetch('/asset-manifest.json')` → `files`; `fetch(path)` = `self.fetch('/' + path)` via the SW-global `fetch` (uninterrupted by the SW's own handler), `cache: 'no-store'`. Extend the bundler's `AssetManifest` with `files` (walk the dist dir after rendering, excluding `.tmpl`). `DevShared::new(control, shell)`.

- [ ] **Step 5: `dev_export` in `dev.js`** and the `#dev-export` button:

```javascript
  async function exportSite() {
    var manifest = await (await api.get('/b/dev/api/export/manifest')).json();
    var r = await api.get('/b/dev/api/export');
    if (!r.ok) throw new Error('export failed: ' + r.status);
    var blob = await r.blob();
    var a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = 'impresspress-site-' + manifest.generation_id.slice(0, 8) + '.zip';
    document.body.appendChild(a); a.click(); a.remove();
    setTimeout(function () { URL.revokeObjectURL(a.href); }, 60_000);
    return manifest;
  }
```

Tool description: "Export the current site as a runnable static bundle (zip) and hand it to the browser as a download: the runtime, the site files, compiled blocks and their source, and a data snapshot of products, offers and settings. Serve the unzipped folder over http to view it."

- [ ] **Step 6: Run** `cargo test -p impresspress-core --features block-dev --test dev_export` and `cargo test -p impresspress-bundle`. Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/impresspress-core crates/impresspress-web crates/impresspress-bundle
git commit -m "feat(dev): export the site as a runnable bundle with a seed and data snapshot"
```

---

### Task 4: Retention, blob garbage collection, storage figures in `dev_status`

**Files:**
- Create: `crates/impresspress-core/src/blocks/dev/gc.rs`
- Modify: `crates/impresspress-core/src/blocks/dev/{activation.rs, status.rs, contracts.rs}`
- Create: `crates/impresspress-core/tests/dev_gc.rs`

**Interfaces:**
- Produces: `gc::collect(ctx) -> Result<GcReport, WaferError>` (`GcReport { blobs_deleted, artifacts_deleted, bytes_freed }`), run at the end of every successful activation after `mark_superseded_before(20)`; `StatusResponse.storage: StorageUsage { blobs: u32, blobs_bytes: u64, artifacts: u32, artifacts_bytes: u64, workspace_files: u32, retained_generations: u32 }`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn gc_deletes_blobs_no_retained_generation_or_workspace_references() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let mut sha = None;
    let mut first_blob = None;
    for i in 0..25 {
        let w = output_json(dev_post(&ctx, "/b/dev/api/files/write",
            json!({"path": "site/index.html", "content": format!("v{i}"), "expected_sha256": sha})).await).await;
        sha = Some(w["sha256"].as_str().unwrap().to_string());
        first_blob.get_or_insert(w["sha256"].as_str().unwrap().to_string());
    }
    // v0's blob is referenced only by a superseded generation → gone. v24 is the workspace + active → kept.
    assert!(!blobs::exists(&ctx.context(), first_blob.as_ref().unwrap()).await.unwrap());
    assert!(blobs::exists(&ctx.context(), sha.as_ref().unwrap()).await.unwrap());
    let s = output_json(ctx.dispatch(admin_msg("retrieve", "/b/dev/api/status")).await).await;
    assert_eq!(s["storage"]["retained_generations"], 20);
    assert!(s["storage"]["blobs"].as_u64().unwrap() <= 20);
}

#[tokio::test]
async fn gc_never_deletes_a_blob_the_workspace_still_names_even_if_no_generation_does() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    // A block source file lives in the workspace only (no generation references block sources).
    let w = output_json(dev_post(&ctx, "/b/dev/api/files/write",
        json!({"path": "blocks/x/src/lib.rs", "content": "// src", "expected_sha256": null})).await).await;
    let src_sha = w["sha256"].as_str().unwrap().to_string();
    for i in 0..3 {
        let _ = dev_post(&ctx, "/b/dev/api/files/write", json!({"path": format!("site/{i}.txt"), "content": "x", "expected_sha256": null})).await;
    }
    assert!(blobs::exists(&ctx.context(), &src_sha).await.unwrap());
}
```

- [ ] **Step 2: Run to verify they fail** — the first blob still exists.

- [ ] **Step 3: Implement** — `collect`: referenced = ⋃ over retained generations (`status ∈ {Active, Staged, Validating, Activating}` plus the 20 most recent `Superseded`? No — retained means not `Superseded`; `mark_superseded_before(20)` keeps 20 rows unsuperseded) of `site.files[].sha256` ∪ `blocks[].artifact_sha256`, ∪ workspace entries; list the `blobs` and `artifacts` folders (`storage::list` with the cursor until exhausted) and delete what is not referenced. Storage figures are computed from the same listings (sizes from `FileEntry`s where known; artifacts from the builds table).

- [ ] **Step 4: Run** `cargo test -p impresspress-core --features block-dev --test dev_gc --test dev_activation`. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/impresspress-core
git commit -m "feat(dev): retention-driven blob GC and storage figures"
```

---

### Task 5: Documentation and spec amendments

**Files:**
- Create: `docs/dev-sandbox.md`
- Modify: `examples/dev-sandbox/README.md`, `README.md` (one link), `docs/superpowers/specs/2026-09-02-dev-sandbox-design.md` (§20 amendments 7–9)

- [ ] **Step 1: `docs/dev-sandbox.md`** — What it is (browser-local, per-visitor, nothing behind it) · Opening it with an agent (Chromium-based browsers with WebMCP; credentials) · The workspace (`site/`, `blocks/`, generations, rollback, the progress panel) · Backend blocks (link to the reference; std-only; limits) · Stocking the shop (`shop_*`, `status: active`) · Export (what is inside, what is not, how to serve, that dev mode is off in the export) · Browser requirements (`crossOriginIsolated` on `/b/dev`, SharedArrayBuffer, OPFS; Firefox/Safari caveats as verified) · Resetting (clear site data) · Known limits (no dependencies, no network from guests, no cart, no payments).

- [ ] **Step 2: Spec amendments** — append to §20: **7.** the preview iframe is same-origin (Plan 2 Task 3); **8.** `wafer_guest_version` is read by the page (Plan 3 Task 4); **9.** the data snapshot is `seed/data.json` applied through typed upserts, not `data.sql` (this plan, Task 2).

- [ ] **Step 3: Commit**

```bash
git add docs README.md examples/dev-sandbox/README.md
git commit -m "docs: the dev sandbox user guide; spec amendments 7-9"
```

---

### Task 6: Measurements with thresholds

**Files:**
- Create: `examples/dev-sandbox/measure.sh`
- Modify: `.github/workflows/ci.yml` (`e2e-dev-sandbox` job: run `measure.sh` and fail on thresholds; write the numbers to `$GITHUB_STEP_SUMMARY`)

- [ ] **Step 1: `measure.sh`** — builds `crates/impresspress-web` twice (`pkg` default, `pkg-dev` with `browser-devtools`), reports raw / `wasm-opt -Oz` (already applied by wasm-pack release) / `gzip -9` sizes of `impresspress_web_bg.wasm` for both, the delta, and the compiler tree's total bytes and largest file (from `compiler/dist/manifest.json`). Prints a Markdown table. Exit non-zero when: dev gz − default gz > 700 KiB (the spike measured +504 KB); compiler total > 70 MiB; any compiler file > 24 MiB.

- [ ] **Step 2: Record durations** — the `dev-compile.spec.ts` and `dev-foundations.spec.ts` already log `elapsed_ms` and activation `progress`; have the CI step grep them into the summary (`cold init`, `compile`, `activation with rebuild`). No threshold on these yet; the table is the baseline.

- [ ] **Step 3: Commit**

```bash
git add examples/dev-sandbox/measure.sh .github/workflows
git commit -m "ci(dev-sandbox): size and duration measurements with thresholds"
```

---

### Task 7: The full scenario, end to end

**Files:**
- Create: `crates/impresspress-web/tests/e2e/dev-scenario.spec.ts`
- Modify: `.github/workflows/ci.yml` (`e2e-dev-compile` runs it; it needs the real compiler)

- [ ] **Step 1: Write the spec** — the seven steps of spec §16 in one test, each with its assertion, reusing the helpers and fixtures from Plans 2–3:

```ts
test('the spec scenario: welcome → login → block → site → shop → shopper → export → rollback → restart', async ({ browser, page }) => {
  test.setTimeout(15 * 60 * 1000);
  // 1. welcome + login + tool set
  await page.addInitScript(MODEL_CONTEXT_POLYFILL);
  await page.goto('/');
  await expect(page.locator('body')).toContainText('admin@example.com');
  await loginToWorkspace(page);
  const names = (await registeredTools(page, DEV_TOOLS.length + SHOP_TOOLS.length)).map((t) => t.name);
  expect(names).toEqual(expect.arrayContaining([...DEV_TOOLS, ...SHOP_TOOLS]));

  // 2. a Rust block compiled in the browser, discoverable anonymously
  await execute(page, 'dev_create_block', { name: 'newsletter', template: 'table' });
  const compiled = await execute(page, 'dev_compile_block', { name: 'newsletter' });
  expect(compiled.structuredContent.success, JSON.stringify(compiled.structuredContent.diagnostics)).toBe(true);
  expect((await page.evaluate(async () => (await fetch('/b/newsletter/subscribe', { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{"email":"a@b.c"}' })).json())).ok).toBe(true);
  const anonManifest = await page.evaluate(async () => (await fetch('/b/webmcp/manifest.json', { credentials: 'omit' })).json());
  expect(anonManifest.tools.map((t) => t.name)).toContain('subscribe_newsletter');

  // 3. the site
  const index = await execute(page, 'dev_read_file', { path: 'site/index.html' });
  const wrote = await execute(page, 'dev_write_file', { path: 'site/index.html', content: SHOP_PAGE, expected_sha256: index.structuredContent.sha256 });
  const N = wrote.structuredContent.generation.id;
  await expect(page.frameLocator('#dev-preview-frame').locator('h1')).toHaveText('Ceramics');

  // 4. the shop
  const ids = [];
  for (const p of [SHOP_PRODUCT, { ...SHOP_PRODUCT, name: 'Blue Bowl', slug: 'blue-bowl' }, { ...SHOP_PRODUCT, name: 'Tall Vase', slug: 'tall-vase' }]) {
    const created = await execute(page, 'shop_create_product', p);
    const id = created.structuredContent.id; ids.push(id);
    const offer = await execute(page, 'shop_create_offer', { product_id: id, ...SHOP_OFFER });
    await execute(page, 'shop_publish_offer', { product_id: id, offer_id: offer.structuredContent.id });
    await execute(page, 'shop_update_product', { id, status: 'active' });
  }
  await execute(page, 'shop_create_group', { name: 'Ceramics' });

  // 5. the shopper
  const shopperCtx = await browser.newContext();
  const shop = await shopperCtx.newPage();
  await shop.addInitScript(MODEL_CONTEXT_POLYFILL);
  await shop.goto('/');
  await expect(shop.locator('h2')).toHaveCount(3);
  await expect(shop.locator('impresspress-product').first()).toBeVisible();
  const listed = await execute(shop, 'list_products', {});
  expect(listed.structuredContent.items.length).toBe(3);
  const priced = await execute(shop, 'preview_price', { product_id: ids[0], ...PRICE_INPUT });
  expect(priced.isError).toBeUndefined();

  // 6. export → serve on another port → the same shop
  const exported = await execute(page, 'dev_export', {});
  expect(exported.structuredContent.blocks).toBe(1);
  const zipBytes = await page.evaluate(async () => Array.from(new Uint8Array(await (await fetch('/b/dev/api/export')).arrayBuffer())));
  const dir = mkdtempSync(join(tmpdir(), 'impresspress-export-'));
  writeFileSync(join(dir, 'site.zip'), Buffer.from(zipBytes));
  execSync(`python3 -m zipfile -e site.zip out`, { cwd: dir });
  const server = spawn('python3', ['-m', 'http.server', '8099', '-d', join(dir, 'out')]);
  try {
    await waitForPort(8099);
    const fresh = await browser.newContext({ baseURL: 'http://127.0.0.1:8099' });
    const fp = await fresh.newPage();
    await fp.addInitScript(MODEL_CONTEXT_POLYFILL);
    await fp.goto('http://127.0.0.1:8099/');
    await fp.waitForFunction(() => navigator.serviceWorker.controller !== null);
    await expect(fp.locator('h2')).toHaveCount(3);                                   // data.json imported
    expect(await fp.evaluate(async () => (await fetch('/b/dev/api/status')).status)).toBe(404);   // dev off
    expect((await fp.evaluate(async () => (await fetch('/b/newsletter/subscribe', { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{"email":"x@y.z"}' })).json())).ok).toBe(true);
    await fresh.close();
  } finally { server.kill(); }

  // 7. rollback and restart
  await execute(page, 'dev_rollback', { id: N });
  await expect(page.frameLocator('#dev-preview-frame').locator('h1')).toHaveText('Ceramics');   // N still has the shop page…
  const gens = await execute(page, 'dev_list_generations', {});
  const beforeShop = gens.structuredContent.generations.find((g) => g.cause === 'seed');
  await execute(page, 'dev_rollback', { id: beforeShop.id });
  await expect(page.frameLocator('#dev-preview-frame').locator('body')).toContainText('admin@example.com');  // …the seed does not
  await page.evaluate(async () => { for (const r of await navigator.serviceWorker.getRegistrations()) await r.unregister(); });
  await page.goto('/');
  await page.waitForFunction(() => navigator.serviceWorker.controller !== null);
  await expect(page.locator('body')).toContainText('admin@example.com');
  await shopperCtx.close();
});
```

`PRICE_INPUT` comes from the same fixture as `SHOP_OFFER` (the inputs the offer's configurator needs; `global-setup.ts`/`webmcp.spec.ts` price the seeded product with a known page count — reuse exactly that). `waitForPort` polls `http://127.0.0.1:8099/` until it answers.

- [ ] **Step 2: CI** — add the spec to `e2e-dev-compile`; the job now proves the definition of done.

- [ ] **Step 3: Run**, commit, PR:

```bash
git add crates/impresspress-web/tests .github/workflows
git commit -m "test(e2e): the full dev sandbox scenario"
git push && gh pr create --title "Dev sandbox: export, data seed, retention, docs and the full scenario" --body-file - <<'EOF'
Plan 4 of the dev.impresspress.org sandbox (spec §7.3, §10, §16, §18, §21).

- `GET /b/dev/api/export` + `dev_export`: a stored-entry zip of the runtime shell (dev off), the seed (site, artifacts, block sources) and `seed/data.json` from an allowlist of tables; a fresh instance boots the same shop from it.
- Data snapshot: every products/admin/auth table has an explicit export decision (test-enforced); secrets, sessions, orders and provider tables never leave; import is typed (upsert/replace).
- Retention-driven blob GC; storage figures in `dev_status`.
- Docs: `docs/dev-sandbox.md`; spec amendments 7–9.
- Measurements with thresholds in CI; the seven-step scenario e2e is green.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
```

## Self-review notes

- Spec coverage: §10.1 (Tasks 1–3), §10.2 data (Task 2), §7.3 retention + §6.6 quotas (Task 4; file/workspace quotas landed in Plan 1), §16 scenario 6–7 and size/time (Tasks 6–7), §18 non-goals unchanged, §21 definition of done (Task 7 is the proof).
- Names: `blocks::dev::{zip::ZipWriter, data_snapshot::{DataSnapshot, export, import, TABLE_ALLOWLIST, TABLE_EXCLUDED}, export::{build, manifest_preview}, gc::collect, control::ShellSource}`, `impresspress_web::dev_runtime::BrowserShellSource`, bundle `AssetManifest.files`, tool `dev_export`, endpoints `/b/dev/api/export`, `/b/dev/api/export/manifest`, seed `data` field, `docs/dev-sandbox.md`, `examples/dev-sandbox/measure.sh`.
