//! Seed-on-boot — generation 0, imported from a bundle the origin serves.
//!
//! A sandbox instance is empty the first time it boots: no workspace, no
//! generations, nothing published. Design §10.2 fills that gap from a **seed
//! bundle** served as static files next to the app shell, so
//! `dev.impresspress.org` can ship a welcome site and an exported bundle can
//! ship the user's own shop through one mechanism.
//!
//! The bundle's layout is design §10.1's, and it is stated once — [`ROOT`],
//! [`MANIFEST_URL`], [`site_url`], [`artifact_url`], [`source_url`]. The
//! exporter (Plan 4) writes those paths and this reads them; a layout spelled
//! at two sites is a layout that can be spelled two different ways.
//!
//! # Why a fetch seam rather than a URL
//!
//! `impresspress-core` has no HTTP client, and the host that does — the
//! service worker's own `fetch` — cannot be reached from here at all. So the
//! importer takes a [`SeedFetch`]: the browser passes a wrapper around
//! `self.fetch`, and the host tests pass an in-memory map. That is also what
//! makes every rule below testable without a browser.
//!
//! # What is verified
//!
//! Every file's declared `sha256`, `size` and `content_type` are checked
//! against the bytes that actually arrived, and every workspace path the
//! bundle would create is run through [`paths::validate_path`]. A seed is
//! same-origin content, but it is still content this instance did not
//! produce: a manifest naming `site/../../elsewhere`, or claiming a hash it
//! does not have, is refused with a message that names the path.
//!
//! Every seeded block's **spec** is re-checked against the rules that do not
//! need a runtime — [`validation::validate_spec`]: its name, its route prefix
//! against the built-ins and against the rest of the bundle, and every §6.5
//! capability rule. A seed is loaded with exactly the capabilities its
//! manifest declares, so without this a bundle asking for `raw_sql` or a
//! collection outside its own namespace would be granted it verbatim, and
//! §10.1 makes exports deliberately re-importable by someone who did not
//! write them.
//!
//! # What is *not* verified
//!
//! The guests are not re-inspected or re-probed. A seeded block was validated
//! when it was staged in the instance that exported it, and the importer has
//! no [`super::control::RuntimeControl`] handle of its own; the activation the
//! caller then requests still refuses a manifest whose artifact is not stored.
//! So the rules that read the guest's own `BlockInfo` — that it calls itself
//! `site/<name>`, that its endpoints fall inside its routes, that its agent
//! tool names are unclaimed, and that `callable_blocks` equals `requires` —
//! are not applied: nothing here can ask the guest what it reports. Running
//! them means executing the artifact, which is [`super::control`]'s job and
//! Plan 4's question.

use serde::{Deserialize, Serialize};
use wafer_run::context::Context;

use super::{
    artifacts, blobs,
    contracts::SiteManifest,
    control::DynamicBlockSpec,
    data_snapshot,
    generation::GenerationManifest,
    paths,
    repo::{self, runtime_state},
    validation,
    workspace::{self, Workspace},
};

/// Seed-manifest schema version this build reads.
///
/// Separate from [`super::generation::SCHEMA_VERSION`] on purpose: a seed
/// bundle is an interchange format between two *builds* of the sandbox, while
/// a generation manifest is this instance's own stored shape. They are `1`
/// together today and are free to move apart.
pub const SCHEMA_VERSION: u32 = 1;

/// URL prefix the seed bundle is served under.
///
/// Also the service worker's bypass prefix (`impresspress-bundle`'s
/// `build_template_vars` adds it when `[dev] enabled`), because these files
/// are served by the static host and must not be answered from the published
/// site the runtime owns.
pub const ROOT: &str = "/seed/";

/// The one file whose presence decides whether there is a seed at all.
pub const MANIFEST_URL: &str = "/seed/manifest.json";

/// One file in a seed bundle.
///
/// The workspace's own [`workspace::FileEntry`], not a parallel shape: a seed
/// entry *is* a workspace entry (path, blob hash, size, served type), and the
/// importer's whole job is to turn one into the other. A second definition
/// would be a second thing to keep in step with the manifest a generation
/// stores.
pub use super::workspace::FileEntry as SeedFile;

/// One block a seed bundle carries: what to register, and the source that
/// produced it.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SeedBlock {
    /// The spec the runtime registers the guest under — the accepted spec the
    /// exporting instance's ledger held, carried verbatim.
    pub spec: DynamicBlockSpec,
    /// The block's crate sources, rooted at the crate (`src/lib.rs`), so the
    /// import is editable and re-compilable rather than a binary drop.
    pub sources: Vec<SeedFile>,
}

/// What a seed bundle describes.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SeedManifest {
    /// Bundle schema version; must be [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The generation id this bundle was exported from, for provenance. Not
    /// reused as the imported generation's id — the importing instance mints
    /// its own, and two instances seeded from one bundle must not claim the
    /// same generation.
    pub source_generation: Option<String>,
    /// Site files, paths relative to the site root (`index.html`), exactly as
    /// a generation's site manifest holds them.
    pub site: Vec<SeedFile>,
    /// Blocks the seeded generation runs.
    pub blocks: Vec<SeedBlock>,
    /// The data snapshot's path within the bundle (design §10.1, amendment
    /// 9: `seed/data.json`), or `None` when the bundle carries no rows.
    ///
    /// A bundle-relative path, not a full [`SeedFetch`] URL — [`import`]
    /// fetches it the same way every other bundle path becomes a URL,
    /// prefixed with `/` (matching [`ROOT`], which already carries the
    /// leading slash and the `seed/` this path itself starts with).
    pub data: Option<String>,
}

/// A future produced by [`SeedFetch::get`].
///
/// `LocalBoxFuture`, not `BoxFuture`: the browser's `fetch` resolves through a
/// `JsFuture`, which is not `Send`, and the sandbox is single-threaded on
/// every target it runs on. A `Send` bound here would be a bound the only real
/// implementation cannot satisfy.
pub type FetchFuture<'a> = futures::future::LocalBoxFuture<'a, Result<Vec<u8>, String>>;

/// How the importer reads one file of the seed bundle.
///
/// A trait rather than the `&dyn Fn(&str) -> …` this started as: a closure
/// whose returned future borrows its argument needs a higher-ranked bound
/// (`for<'a> Fn(&'a str) -> FetchFuture<'a>`) that closure inference does not
/// produce, so every call site would have to spell the bound out. The trait
/// says the same thing and is implementable in three lines — which is what
/// both the service worker's `fetch` wrapper and the host tests' in-memory map
/// are.
pub trait SeedFetch {
    /// Fetch `url` (an absolute path under [`ROOT`]).
    ///
    /// A file the bundle's manifest names but does not carry is an `Err`: the
    /// manifest is the contract, and a partially-shipped bundle must not
    /// import as a partial site. Only the *manifest itself* is allowed to be
    /// absent, and that is the caller's probe, not this call.
    fn get<'a>(&'a self, url: &'a str) -> FetchFuture<'a>;
}

/// URL of one site file.
pub fn site_url(path: &str) -> String {
    format!("{ROOT}site/{path}")
}

/// URL of one block's compiled artifact.
///
/// `name` is the short workspace name (`hello`), not the registered
/// `site/hello` — see [`short_name`].
pub fn artifact_url(name: &str) -> String {
    format!("{ROOT}blocks/{name}.wasm")
}

/// URL of one file of a block's crate sources.
pub fn source_url(name: &str, path: &str) -> String {
    format!("{ROOT}blocks/{name}/{path}")
}

/// The short workspace name of a registered block (`site/hello` → `hello`).
///
/// The workspace directory, the artifact URL and the route prefix are all
/// built from the short name, while the manifest carries the registered one.
/// A spec whose name is not `site/…` keeps its name verbatim, and then fails
/// [`paths::block_name_is_valid`] below — refusing it there, with the name in
/// the message, beats silently inventing a directory for it.
pub fn short_name(registered: &str) -> &str {
    registered.strip_prefix("site/").unwrap_or(registered)
}

/// Whether this instance has never published anything.
///
/// Both halves are required. `active_generation_id` alone would call an
/// instance whose only generation *failed* fresh, and re-seeding over a
/// workspace someone has already edited is the one outcome a seed import must
/// never produce; an empty ledger alone would miss nothing today but says
/// what is actually meant — "nothing has ever been staged here".
pub async fn is_fresh(ctx: &dyn Context) -> Result<bool, String> {
    let state = runtime_state::read(ctx).await.map_err(|e| e.message)?;
    if state.active_generation_id.is_some() {
        return Ok(false);
    }
    let recent = repo::generations::list_recent(ctx, 1)
        .await
        .map_err(|e| e.message)?;
    Ok(recent.is_empty())
}

/// Import `manifest` into a fresh instance and return the generation 0 the
/// caller should activate.
///
/// `Ok(None)` when the instance is not fresh — a second boot, or a bundle that
/// was already imported. That is the ordinary case on every boot after the
/// first, not an error.
///
/// The order is: fetch and verify everything, write the blobs and artifacts,
/// then save the workspace. A workspace saved before its blobs would name
/// content that is not stored, and every later read of those paths would be a
/// 500; in this order a failure part-way leaves stored bytes that no manifest
/// names, which costs storage and nothing else.
///
/// The returned manifest is *staged* — it has no id and no parent. Minting
/// those is [`super::activation::request`]'s job, exactly as for every other
/// generation.
pub async fn import(
    ctx: &dyn Context,
    manifest: &SeedManifest,
    fetch: &dyn SeedFetch,
) -> Result<Option<GenerationManifest>, String> {
    if !is_fresh(ctx).await? {
        return Ok(None);
    }
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "the seed bundle declares schema_version {}; this build reads {SCHEMA_VERSION}",
            manifest.schema_version
        ));
    }
    if manifest.blocks.len() > paths::MAX_BLOCKS {
        return Err(format!(
            "the seed bundle carries {} blocks; the limit is {}",
            manifest.blocks.len(),
            paths::MAX_BLOCKS
        ));
    }

    // Every spec, before a single byte is fetched. Two reasons for the
    // ordering: a bundle whose second block is refused must not have left the
    // first one's artifact in the store, and the rules that read the rest of
    // the set need the whole set in hand. `validate_spec` is also what refuses
    // an unregisterable name, which everything below derives a URL and a
    // workspace directory from.
    let specs: Vec<DynamicBlockSpec> = manifest.blocks.iter().map(|b| b.spec.clone()).collect();
    let builtin_routes = validation::builtin_route_prefixes();
    for (index, spec) in specs.iter().enumerate() {
        let others: Vec<DynamicBlockSpec> = specs
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .map(|(_, spec)| spec.clone())
            .collect();
        if let Err(found) = validation::validate_spec(spec, &builtin_routes, &others) {
            return Err(format!(
                "the seed bundle's block {:?} cannot be registered: {}",
                spec.name,
                found
                    .iter()
                    // A diagnostic with no code is a compiler's, and the
                    // seed bundle's blocks are checked by the VALIDATOR,
                    // whose diagnostics all carry one — so the bare-message
                    // arm should never run here. It is written out rather
                    // than unwrapped because a boot refusal that panicked
                    // instead of naming the block would be the worst
                    // possible failure of this function.
                    .map(|d| match &d.code {
                        Some(code) => format!("{} [{}]", d.message, code),
                        None => d.message.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
    }

    let mut ws = Workspace::default();
    for entry in &manifest.site {
        let workspace_path = format!("{}{}", workspace::SITE_PREFIX, entry.path);
        let bytes = fetch_verified(fetch, &site_url(&entry.path), entry, &workspace_path).await?;
        store(ctx, &mut ws, &workspace_path, &bytes).await?;
    }

    for block in &manifest.blocks {
        let name = short_name(&block.spec.name);
        // The artifact is content-addressed by the spec itself, so the spec's
        // own `artifact_sha256` is the check — there is no second declaration
        // of it to disagree with.
        let url = artifact_url(name);
        let bytes = fetch.get(&url).await?;
        // The same bound the staging path enforces, for the same reason: the
        // limit exists to bound what the artifact store holds, and a bundle
        // that arrived over the network is no more entitled to exceed it than
        // one that arrived over `POST /b/dev/api/builds/stage`. Checked beside
        // the hash and before the `put`, so an oversized module is never
        // stored. The wording is `Diagnostic::artifact_too_large`'s, which is
        // where the limit and its remedy are stated.
        if bytes.len() > validation::MAX_ARTIFACT_BYTES {
            return Err(format!(
                "{url}: {}",
                validation::Diagnostic::artifact_too_large(bytes.len()).message
            ));
        }
        let actual = blobs::sha256_hex(&bytes);
        if actual != block.spec.artifact_sha256 {
            return Err(format!(
                "{url}: the artifact hashes to {actual}, but block {:?} declares {}",
                block.spec.name, block.spec.artifact_sha256
            ));
        }
        artifacts::put(ctx, &bytes).await.map_err(|e| e.message)?;

        for entry in &block.sources {
            let workspace_path = format!("{}{name}/{}", workspace::BLOCKS_PREFIX, entry.path);
            let bytes = fetch_verified(
                fetch,
                &source_url(name, &entry.path),
                entry,
                &workspace_path,
            )
            .await?;
            store(ctx, &mut ws, &workspace_path, &bytes).await?;
        }
    }

    workspace::save(ctx, &ws).await.map_err(|e| e.message)?;

    // The data snapshot, if the bundle carries one — after every file and
    // block artifact is stored (so a snapshot referencing this generation's
    // own content lands on a workspace that already has it), before the
    // staged manifest is handed back for activation.
    if let Some(data_path) = &manifest.data {
        let url = format!("/{data_path}");
        let bytes = fetch.get(&url).await?;
        let snapshot: data_snapshot::DataSnapshot = serde_json::from_slice(&bytes)
            .map_err(|e| format!("{url}: not a valid data snapshot: {e}"))?;
        data_snapshot::import(ctx, &snapshot)
            .await
            .map_err(|e| format!("{url}: {}", e.message))?;
    }

    Ok(Some(GenerationManifest::staged(
        SiteManifest {
            files: workspace::site_manifest(&ws),
        },
        manifest.blocks.iter().map(|b| b.spec.clone()).collect(),
    )))
}

/// Fetch one declared file and check it is what the manifest said it was.
///
/// All three declared properties are load-bearing, so all three are checked:
/// `sha256` is what design §10.2 requires ("verify every referenced file's
/// hash"); `size` costs nothing over bytes already in hand and catches a
/// manifest built from a different tree; `content_type` is what the site will
/// actually be served as, and this instance derives it from the path, so a
/// bundle claiming a different one was produced by an exporter that does not
/// agree with this build about how files are served.
async fn fetch_verified(
    fetch: &dyn SeedFetch,
    url: &str,
    declared: &SeedFile,
    workspace_path: &str,
) -> Result<Vec<u8>, String> {
    // Refuse the *path* before anything is fetched: a manifest entry naming
    // `../../elsewhere` must not cause a request for it either.
    paths::validate_path(workspace_path)
        .map_err(|e| format!("the seed bundle names {workspace_path:?}: {e}"))?;
    if declared.size > paths::MAX_FILE_BYTES as u64 {
        return Err(format!(
            "{url}: declares {} bytes; the per-file limit is {}",
            declared.size,
            paths::MAX_FILE_BYTES
        ));
    }

    let bytes = fetch.get(url).await?;
    let actual = blobs::sha256_hex(&bytes);
    if actual != declared.sha256 {
        return Err(format!(
            "{url}: content hashes to {actual}, but the manifest declares {}",
            declared.sha256
        ));
    }
    if bytes.len() as u64 != declared.size {
        return Err(format!(
            "{url}: content is {} bytes, but the manifest declares {}",
            bytes.len(),
            declared.size
        ));
    }
    let served = paths::content_type_for(workspace_path);
    if declared.content_type != served {
        return Err(format!(
            "{url}: the manifest declares content type {:?}, but {workspace_path:?} is served as \
             {served:?}",
            declared.content_type
        ));
    }
    Ok(bytes)
}

/// Store one verified file's bytes and record it in the workspace under
/// construction.
///
/// Charges [`Workspace::record_blob_stored`] only when the store actually
/// grew, exactly as the files API does — two seed paths carrying the same
/// asset cost one blob and must be counted once.
async fn store(
    ctx: &dyn Context,
    ws: &mut Workspace,
    workspace_path: &str,
    bytes: &[u8],
) -> Result<(), String> {
    if ws.files.len() >= paths::MAX_FILES {
        return Err(format!(
            "the seed bundle carries more than the {} files a workspace holds",
            paths::MAX_FILES
        ));
    }
    // The same file/directory rule the files API enforces. A bundle is a set
    // of paths from another instance, and nothing about crossing the wire
    // makes a pair of them agree about whether a name is a directory — while
    // a bundle that got in would wedge this instance's publisher for good.
    if let Some(clash) = ws.path_collision(workspace_path) {
        return Err(format!(
            "the seed bundle carries both {workspace_path:?} and {clash:?}; one name cannot be              a file in one entry and a directory in the other"
        ));
    }
    let sha = blobs::sha256_hex(bytes);
    let grows_by = if ws.references(&sha) {
        0
    } else {
        bytes.len() as u64
    };
    if ws.blob_bytes.saturating_add(grows_by) > paths::MAX_WORKSPACE_BYTES {
        return Err(format!(
            "the seed bundle is larger than the {}-byte workspace quota",
            paths::MAX_WORKSPACE_BYTES
        ));
    }
    match blobs::put_hashed(ctx, &sha, bytes)
        .await
        .map_err(|e| e.message)?
    {
        blobs::Stored::New => ws.record_blob_stored(bytes.len() as u64),
        blobs::Stored::Deduplicated => {}
    }
    ws.insert(workspace_path, sha, bytes.len() as u64);
    Ok(())
}
