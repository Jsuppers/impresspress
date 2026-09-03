//! `GET /b/dev/api/export` and `GET /b/dev/api/export/manifest` — the sandbox
//! as a folder anyone can serve.
//!
//! Design §10.1. The archive is the running deployment's own static shell,
//! with development mode switched off, plus a `seed/` tree in exactly the
//! format [`super::seed`] reads on a cold boot. That symmetry is the whole
//! design: there is one mechanism for "a fresh instance gets a site", and an
//! export is a bundle that feeds it.
//!
//! ```text
//!     README.md                     how to serve it, what is inside, and the
//!                                   disclosure about `data.json`
//!     index.html sw.js loader.js …  the runtime shell, listed by
//!                                   `/asset-manifest.json`'s `files`
//!     seed/manifest.json            a `SeedManifest`: every file below with
//!                                   its hash, size and served type
//!     seed/site/**                  the site's files
//!     seed/blocks/<name>.wasm       each compiled block
//!     seed/blocks/<name>/**         and its full source, so the export is
//!                                   editable and re-compilable
//!     seed/data.json                the data snapshot
//! ```
//!
//! # One list, two answers
//!
//! [`assemble`] produces the whole entry list with its bytes; [`build`] zips
//! that and [`manifest_preview`] summarizes it. They are not two derivations
//! of "what an export contains" — a preview computed from a second reading of
//! the same stores is a preview that can drift from the archive it claims to
//! describe, and the whole point of the preview is that an agent can trust it
//! without downloading 15 MB. The cost is that the preview reads the shell
//! too; it is a browser-local read of a handful of files the service worker
//! already has, on an explicit call.
//!
//! # Why the shell is rewritten rather than re-rendered
//!
//! The exported site is a plain ImpressPress deployment: no `/b/dev`, no
//! in-browser compiler, no cross-origin-isolation headers on static files.
//! All three follow from one build-time constant in `sw.js`
//! (`const DEV_ENABLED = true;`, `impresspress-bundle`'s `sw.js.tmpl`), which
//! `initialize({ dev: DEV_ENABLED })` and the isolation-header passthrough
//! both read — so turning the sandbox off is one line rewritten, and its
//! absence is an [`ErrorCode::Internal`], never a silent pass-through of a
//! shell that would come up as a second sandbox.

use std::collections::BTreeMap;

use wafer_run::{context::Context, ErrorCode, OutputStream, WaferError};

use super::{
    artifacts, blobs,
    contracts::{ExportFile, ExportManifest},
    data_snapshot,
    generation::{self, GenerationManifest},
    no_store, no_store_error_status,
    seed::{self, SeedBlock, SeedManifest},
    workspace,
    zip::ZipWriter,
    DevShared,
};
use crate::http::err_internal;

/// The line `sw.js` carries when it was built for a dev deployment, and the
/// one it must carry after an export.
///
/// Stated as a pair rather than as a `replace` call inline so the assertion
/// and the rewrite cannot disagree about the text. `impresspress-bundle`'s
/// `sw_passes_the_dev_flag_to_initialize` pins the producing half.
const SW_DEV_ON: &str = "const DEV_ENABLED = true;";
const SW_DEV_OFF: &str = "const DEV_ENABLED = false;";

/// The one shell file whose content this export edits.
const SW_PATH: &str = "sw.js";

/// Where the data snapshot lands, relative to [`seed::ROOT`].
///
/// `seed::data_url(DATA_PATH)` is the URL the importer fetches and
/// `seed/{DATA_PATH}` is the archive entry — the two are the same string
/// because [`seed`] owns the layout and this writes what it reads.
const DATA_PATH: &str = "data.json";

/// The content type the importer checks `seed/data.json` against.
const DATA_CONTENT_TYPE: &str = "application/json";

/// Shell paths the export never copies, by prefix.
///
/// Both are things a DEPLOYMENT overlays on top of the bundler's output
/// (`impresspress`'s `apply_overlays`, run after `bundle::run` returns), so
/// neither is in `/asset-manifest.json`'s `files` today. They are excluded
/// explicitly all the same, because "the manifest happens not to list them"
/// is a property of the order two CLI steps run in, and this is a property of
/// what an export MEANS:
///
/// * `seed/` — the exporting deployment's own starter bundle. The archive
///   writes its own `seed/`, and a copied one would either be overwritten or
///   (worse, if it sorted later) overwrite the export's.
/// * `__impresspress_dev/` — the in-browser Rust toolchain: 72 MiB of
///   compiler that only `/b/dev` loads, and the exported site has no `/b/dev`.
const SHELL_EXCLUDED_PREFIXES: &[&str] = &["seed/", "__impresspress_dev/"];

/// The README template, rendered with this export's own numbers.
const README_TEMPLATE: &str = include_str!("templates/export-readme.md");

/// Where the README lands in the archive.
const README_PATH: &str = "README.md";

/// One entry of the archive, with its bytes.
struct Entry {
    path: String,
    bytes: Vec<u8>,
}

/// What [`assemble`] produced: the archive's entries, plus the counts the
/// preview reports.
struct Assembled {
    generation_id: String,
    entries: Vec<Entry>,
    shell_files: u32,
    site_files: u32,
    blocks: u32,
    tables: BTreeMap<String, usize>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /b/dev/api/export` — the zip.
pub async fn handle_export(ctx: &dyn Context, shared: &DevShared) -> OutputStream {
    let assembled = match assemble(ctx, shared).await {
        Ok(assembled) => assembled,
        Err(refusal) => return refusal.into_response(),
    };
    let short = short_id(&assembled.generation_id);
    let bytes = match archive(assembled) {
        Ok(bytes) => bytes,
        Err(e) => return err_internal("dev export archive", e),
    };
    no_store()
        .set_header(
            "Content-Disposition",
            &format!("attachment; filename=\"impresspress-site-{short}.zip\""),
        )
        // The uncompressed content total is already in the manifest; this is
        // the size of the archive the client is about to read, which is what
        // a progress indicator (and the e2e's download assertion) needs and
        // what `Content-Length` would otherwise be the only source of.
        .set_header("X-Export-Bytes", &bytes.len().to_string())
        .body(bytes, "application/zip")
}

/// `GET /b/dev/api/export/manifest` — what the zip would contain.
pub async fn handle_manifest(ctx: &dyn Context, shared: &DevShared) -> OutputStream {
    match assemble(ctx, shared).await {
        Ok(assembled) => no_store().json(&preview(&assembled)),
        Err(refusal) => refusal.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Building the bundle
// ---------------------------------------------------------------------------

/// The complete archive, as bytes.
///
/// Public because the round-trip test (`tests/dev_export.rs`) exports from
/// one instance and seeds another from the result without going through HTTP,
/// which is the property the format's whole design rests on.
pub async fn build(ctx: &dyn Context, shared: &DevShared) -> Result<Vec<u8>, WaferError> {
    let assembled = assemble(ctx, shared).await.map_err(Refusal::into_error)?;
    archive(assembled)
}

/// What the archive would contain, without producing it.
pub async fn manifest_preview(
    ctx: &dyn Context,
    shared: &DevShared,
) -> Result<ExportManifest, WaferError> {
    let assembled = assemble(ctx, shared).await.map_err(Refusal::into_error)?;
    Ok(preview(&assembled))
}

/// Every entry of the archive, in the order it is written, with its bytes.
async fn assemble(ctx: &dyn Context, shared: &DevShared) -> Result<Assembled, Refusal> {
    // The export is a snapshot of what is LIVE, not of the workspace: a block
    // whose source has been edited since it was compiled exports the compiled
    // one, because that is what the exported folder will run. The workspace
    // supplies only the sources that go alongside it.
    let Some((_row, manifest)) = generation::active(ctx).await.map_err(Refusal::Internal)? else {
        return Err(Refusal::NothingPublished);
    };
    let ws = workspace::load(ctx).await.map_err(Refusal::Internal)?;

    // --- the shell -------------------------------------------------------
    let listed = shared.shell.list().await.map_err(Refusal::Shell)?;
    let mut shell: Vec<Entry> = Vec::new();
    for path in listed {
        if SHELL_EXCLUDED_PREFIXES
            .iter()
            .any(|prefix| path.starts_with(prefix))
        {
            continue;
        }
        let bytes = shared
            .shell
            .fetch(&path)
            .await
            .map_err(|e| Refusal::Shell(format!("{path}: {e}")))?;
        let bytes = if path == SW_PATH {
            sw_with_dev_off(&bytes)?
        } else {
            bytes
        };
        shell.push(Entry { path, bytes });
    }
    if shell.is_empty() {
        return Err(Refusal::Shell(
            "/asset-manifest.json listed no files, so there is no runtime to export".to_string(),
        ));
    }

    // --- the seed --------------------------------------------------------
    let snapshot = data_snapshot::export(ctx)
        .await
        .map_err(Refusal::Internal)?;
    let data_bytes = serde_json::to_vec_pretty(&snapshot)
        .map_err(|e| Refusal::Internal(encoding_error("the data snapshot", e)))?;
    let tables: BTreeMap<String, usize> = snapshot
        .tables
        .iter()
        .map(|(table, rows)| (table.clone(), rows.len()))
        .collect();

    let mut seed_entries: Vec<Entry> = Vec::new();
    let mut site: Vec<seed::SeedFile> = Vec::new();
    for entry in &manifest.site.files {
        let bytes = blobs::get(ctx, &entry.sha256)
            .await
            .map_err(Refusal::Internal)?;
        seed_entries.push(Entry {
            path: format!("seed/site/{}", entry.path),
            bytes,
        });
        site.push(entry.clone());
    }

    let mut blocks: Vec<SeedBlock> = Vec::new();
    for spec in &manifest.blocks {
        let name = seed::short_name(&spec.name);
        let artifact = artifacts::get(ctx, &spec.artifact_sha256)
            .await
            .map_err(Refusal::Internal)?;
        seed_entries.push(Entry {
            path: format!("seed/blocks/{name}.wasm"),
            bytes: artifact,
        });
        // The source tree lives in the workspace and in no generation at all,
        // so this reads what is there NOW. A block whose sources were deleted
        // after it was compiled exports as a `.wasm` with no `src/` — the
        // honest answer, and the manifest says so by carrying no source
        // entries for it rather than by failing the export.
        let sources = workspace::block_sources(&ws, name);
        for source in &sources {
            let bytes = blobs::get(ctx, &source.sha256)
                .await
                .map_err(Refusal::Internal)?;
            seed_entries.push(Entry {
                path: format!("seed/blocks/{name}/{}", source.path),
                bytes,
            });
        }
        blocks.push(SeedBlock {
            spec: spec.clone(),
            sources,
        });
    }

    let seed_manifest = SeedManifest {
        schema_version: seed::SCHEMA_VERSION,
        source_generation: Some(manifest.generation_id.clone()),
        site,
        blocks,
        data: Some(seed::SeedFile {
            path: DATA_PATH.to_string(),
            sha256: blobs::sha256_hex(&data_bytes),
            size: data_bytes.len() as u64,
            content_type: DATA_CONTENT_TYPE.to_string(),
        }),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&seed_manifest)
        .map_err(|e| Refusal::Internal(encoding_error("the seed manifest", e)))?;

    // --- the whole archive, in order -------------------------------------
    //
    // README first (it is what a human opening the folder sees), then the
    // shell, then `seed/manifest.json` ahead of the files it describes, then
    // those files, then the data snapshot. Fixed order plus `ZipWriter`'s
    // fixed timestamps means two exports of the same generation are
    // byte-identical archives.
    let shell_files = shell.len() as u32;
    let site_files = manifest.site.files.len() as u32;
    let block_count = manifest.blocks.len() as u32;
    let readme = render_readme(
        ctx,
        &manifest,
        shell_files,
        site_files,
        block_count,
        &tables,
    );

    let mut entries = Vec::with_capacity(shell.len() + seed_entries.len() + 3);
    entries.push(Entry {
        path: README_PATH.to_string(),
        bytes: readme.into_bytes(),
    });
    entries.extend(shell);
    entries.push(Entry {
        path: format!("{}manifest.json", archive_seed_prefix()),
        bytes: manifest_bytes,
    });
    entries.extend(seed_entries);
    entries.push(Entry {
        path: format!("{}{DATA_PATH}", archive_seed_prefix()),
        bytes: data_bytes,
    });

    Ok(Assembled {
        generation_id: manifest.generation_id,
        entries,
        shell_files,
        site_files,
        blocks: block_count,
        tables,
    })
}

/// The archive's `seed/` prefix, derived from the URL prefix the importer
/// fetches ([`seed::ROOT`] is `/seed/`) rather than restated — the archive
/// entry and the URL are the same path with and without its leading `/`, and
/// spelling that twice is how they come apart.
fn archive_seed_prefix() -> &'static str {
    seed::ROOT.trim_start_matches('/')
}

/// Zip what [`assemble`] produced.
fn archive(assembled: Assembled) -> Result<Vec<u8>, WaferError> {
    let mut zip = ZipWriter::new();
    for entry in &assembled.entries {
        zip.add(&entry.path, &entry.bytes).map_err(|e| {
            WaferError::new(
                ErrorCode::Internal,
                format!("the export bundle could not be written: {e}"),
            )
        })?;
    }
    Ok(zip.finish())
}

/// Summarize what [`assemble`] produced.
fn preview(assembled: &Assembled) -> ExportManifest {
    ExportManifest {
        generation_id: assembled.generation_id.clone(),
        files: assembled
            .entries
            .iter()
            .map(|entry| ExportFile {
                path: entry.path.clone(),
                bytes: entry.bytes.len() as u64,
            })
            .collect(),
        total_bytes: assembled
            .entries
            .iter()
            .map(|entry| entry.bytes.len() as u64)
            .sum(),
        shell_files: assembled.shell_files,
        site_files: assembled.site_files,
        blocks: assembled.blocks,
        tables: assembled.tables.clone(),
    }
}

/// `sw.js` with the sandbox turned off.
///
/// A missing marker is [`ErrorCode::Internal`], never a pass-through: this
/// runs on a shell the deployment built for itself, so its absence means the
/// bundler and this function disagree about the constant — and the failure
/// mode of guessing is an exported folder that comes up as a second
/// development sandbox, with `/b/dev` and an in-browser compiler on a site
/// its owner meant to hand to someone else.
fn sw_with_dev_off(bytes: &[u8]) -> Result<Vec<u8>, Refusal> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        Refusal::Internal(WaferError::new(
            ErrorCode::Internal,
            "the deployment's sw.js is not valid UTF-8, so its dev flag cannot be turned off",
        ))
    })?;
    // EXACTLY one occurrence, not "at least one". Two would mean the marker
    // is ambiguous — a comment quoting the declaration, say — and a blanket
    // `replace` would edit both, leaving a file whose prose contradicts its
    // code and, worse, no longer telling this function which line it is
    // meant to own. `sw.js.tmpl` is written so the declaration is the only
    // occurrence; this is what keeps that true.
    let occurrences = text.matches(SW_DEV_ON).count();
    if occurrences != 1 {
        return Err(Refusal::Internal(WaferError::new(
            ErrorCode::Internal,
            format!(
                "the deployment's sw.js contains {occurrences} occurrences of {SW_DEV_ON:?} \
                 and the export needs exactly one, so it cannot turn development mode off; \
                 the bundle was built by a different impresspress-bundle than this runtime \
                 expects"
            ),
        )));
    }
    Ok(text.replace(SW_DEV_ON, SW_DEV_OFF).into_bytes())
}

/// The first eight characters of a generation id — what the downloaded file
/// is named after.
fn short_id(generation_id: &str) -> String {
    generation_id.chars().take(8).collect()
}

/// The README, with this export's own numbers substituted in.
fn render_readme(
    ctx: &dyn Context,
    manifest: &GenerationManifest,
    shell_files: u32,
    site_files: u32,
    blocks: u32,
    tables: &BTreeMap<String, usize>,
) -> String {
    // The literal, as every other reader of this shared variable spells it
    // (`blocks::auth_ui::pages`, `pipeline`): `config_vars` declares it in
    // `shared_config_vars()` without exporting a constant for the key.
    let title = ctx
        .config_get("WAFER_RUN_SHARED__APP_NAME")
        .filter(|name| !name.is_empty())
        .unwrap_or("Your ImpressPress site");
    let admin_email = ctx
        .config_get(crate::blocks::auth::config::BOOTSTRAP_ADMIN_EMAIL_KEY)
        .filter(|email| !email.is_empty())
        .unwrap_or("the account you signed in with");
    let rows: usize = tables.values().sum();
    // A plain textual substitution, not a template engine: the README has six
    // holes and every value is a number or a short string this function
    // produced.
    README_TEMPLATE
        .replace("{{TITLE}}", title)
        .replace("{{DATE}}", &crate::util::now_rfc3339())
        .replace("{{GENERATION_ID}}", &manifest.generation_id)
        .replace("{{SHELL_FILES}}", &shell_files.to_string())
        .replace("{{SITE_FILES}}", &site_files.to_string())
        .replace("{{BLOCKS}}", &blocks.to_string())
        .replace("{{TABLE_ROWS}}", &rows.to_string())
        .replace("{{ADMIN_EMAIL}}", admin_email)
}

/// A `serde_json` failure encoding one of the bundle's own JSON files.
fn encoding_error(what: &str, error: serde_json::Error) -> WaferError {
    WaferError::new(
        ErrorCode::Internal,
        format!("{what} could not be encoded: {error}"),
    )
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// Why an export could not be produced.
///
/// Three shapes, because they reach the caller three different ways: a
/// precondition the agent can act on (publish something first), a host-side
/// failure the agent cannot (the shell would not read), and everything the
/// storage or ledger refused, which is already a [`WaferError`].
enum Refusal {
    /// Nothing has been published, so there is no site to export.
    NothingPublished,
    /// The static shell could not be listed or read.
    Shell(String),
    /// A storage, ledger or encoding failure.
    Internal(WaferError),
}

impl Refusal {
    /// The refusal as an already-sealed response.
    fn into_response(self) -> OutputStream {
        match self {
            // `FailedPrecondition` names the condition exactly — nothing has
            // been published here yet — while the status is 400 rather than
            // the code's default 412, which HTTP reserves for the conditional
            // request headers no caller of this endpoint sends.
            Self::NothingPublished => no_store_error_status(
                ErrorCode::FailedPrecondition,
                400,
                "there is nothing to export yet: no generation is active. Write a site file or \
                 compile a block first.",
            ),
            Self::Shell(message) => err_internal("dev export shell", message),
            Self::Internal(error) => err_internal("dev export", error.message),
        }
    }

    /// The refusal as a [`WaferError`], for the non-HTTP callers
    /// ([`build`], [`manifest_preview`]).
    fn into_error(self) -> WaferError {
        match self {
            Self::NothingPublished => WaferError::new(
                ErrorCode::FailedPrecondition,
                "there is nothing to export yet: no generation is active",
            ),
            Self::Shell(message) => WaferError::new(
                ErrorCode::Internal,
                format!("the static shell could not be read: {message}"),
            ),
            Self::Internal(error) => error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seed_prefix_is_the_url_prefix_without_its_slash() {
        assert_eq!(archive_seed_prefix(), "seed/");
        assert_eq!(format!("/{}", archive_seed_prefix()), seed::ROOT);
    }

    #[test]
    fn the_download_is_named_after_the_first_eight_characters() {
        assert_eq!(short_id("0123456789abcdef"), "01234567");
        assert_eq!(short_id("short"), "short");
        assert_eq!(short_id(""), "");
    }

    #[test]
    fn turning_the_dev_flag_off_rewrites_exactly_one_line() {
        let sw = b"const DEV_ENABLED = true;\nif (DEV_ENABLED && x) {}\n";
        let Ok(out) = sw_with_dev_off(sw) else {
            panic!("a dev shell must rewrite");
        };
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("const DEV_ENABLED = false;"), "{text}");
        // The passthrough branch is untouched: it reads the constant, so
        // flipping the declaration flips it too. That is the whole reason
        // the bundler renders one constant instead of two literals.
        assert!(text.contains("if (DEV_ENABLED && x) {}"), "{text}");
        assert!(!text.contains("= true;"), "{text}");
    }

    /// A second occurrence — a comment quoting the declaration, most likely
    /// — makes the marker ambiguous, and a blanket replace would edit both.
    #[test]
    fn a_shell_with_two_markers_is_refused() {
        let sw = b"// flip const DEV_ENABLED = true; to false\nconst DEV_ENABLED = true;\n";
        let Err(refusal) = sw_with_dev_off(sw) else {
            panic!("an ambiguous marker must be refused");
        };
        assert!(
            refusal.into_error().message.contains("2 occurrences"),
            "the refusal must say how many it found"
        );
    }

    /// The one thing this must never do is pass a shell through unchanged.
    #[test]
    fn a_shell_without_the_marker_is_refused() {
        let Err(refusal) = sw_with_dev_off(b"await initialize({ dev: true });") else {
            panic!("a shell with no marker must be refused");
        };
        let error = refusal.into_error();
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(error.message.contains("DEV_ENABLED"), "{}", error.message);
    }
}
