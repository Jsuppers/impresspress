//! The workspace manifest — which paths exist and which blob each one names.
//!
//! One JSON object at `workspace.json` in the block's own storage namespace.
//! It is the *editable* state: the sandbox's files API reads and writes it,
//! and a generation is a frozen projection of it (design §11.3), never the
//! other way round.
//!
//! The manifest holds no content. Every entry names a blob by sha, so
//! replacing a file rewrites one small JSON document rather than moving bytes,
//! and two paths holding the same content cost one copy.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use wafer_core::clients::storage;
use wafer_run::{context::Context, ErrorCode, WaferError};

use super::paths::content_type_for;

/// Storage key the manifest lives under, in the block's own namespace
/// (folder `""`, which `wafer-run/storage` resolves to `impresspress/dev`).
pub const KEY: &str = "workspace.json";

/// Storage folder the manifest lives in: the block's namespace root.
pub const FOLDER: &str = "";

/// Content type the manifest is stored under.
const MANIFEST_CONTENT_TYPE: &str = "application/json";

/// The `site/` area's path prefix, including its separator.
pub const SITE_PREFIX: &str = "site/";

/// The `blocks/` area's path prefix, including its separator.
pub const BLOCKS_PREFIX: &str = "blocks/";

/// One file in the workspace: where it is, which blob holds it, how big it is
/// and what it is served as.
///
/// The same type is what a generation's site manifest is made of — a
/// generation *is* the workspace's `site/` entries, frozen — so there is one
/// definition rather than a wire type and a stored type that have to be kept
/// in step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    /// Where the file lives. Workspace-relative (`site/index.html`) in the
    /// files API; relative to its area's root (`index.html`) in a
    /// generation's site manifest and in a block's source listing.
    pub path: String,
    /// SHA-256 of the file's content-addressed blob, hex-encoded.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
    /// Content type the file is served with.
    pub content_type: String,
}

/// Every file the workspace holds, plus what its blob store has cost.
///
/// `files` is a `BTreeMap` for two reasons that are both load-bearing:
/// iteration is in path order, so every projection below is sorted without
/// sorting; and serialization is key-ordered, so the stored manifest is
/// byte-for-byte deterministic for a given workspace.
///
/// # Why the blob totals live here
///
/// `files` describes what is *reachable*; it says nothing about what is
/// *stored*. Content is never edited in place, so overwriting one 512 KiB
/// page two hundred times leaves two hundred blobs behind — 100 MB of
/// storage that `files` still reports as one 512 KiB entry. The quota that
/// matters (design §6.6, "≤ 64 MiB of blobs per workspace") is on the store,
/// so the store's size is tracked here rather than recomputed from `files`,
/// which cannot see the difference.
///
/// Both counters are maintained by [`Workspace::record_blob_stored`] and
/// [`Workspace::record_blob_freed`] only. Nothing else writes them, so
/// "blobs written minus blobs reclaimed" is the whole of their definition.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// Entries by path. The key and [`FileEntry::path`] are always equal —
    /// [`Workspace::insert`] is the only thing that writes either.
    #[serde(default)]
    pub files: BTreeMap<String, FileEntry>,
    /// Total bytes of blobs this workspace has stored and not yet had
    /// reclaimed, including blobs no `files` entry names any more. This is
    /// what [`super::paths::MAX_WORKSPACE_BYTES`] bounds.
    #[serde(default)]
    pub blob_bytes: u64,
    /// How many blobs those bytes are spread over.
    #[serde(default)]
    pub blob_count: u32,
}

impl Workspace {
    /// Record `sha256`/`size` at `path`, deriving the content type from the
    /// path, and return the entry as stored.
    ///
    /// The single writer of `files`, which is what keeps the map key and
    /// [`FileEntry::path`] from drifting apart.
    pub fn insert(&mut self, path: &str, sha256: String, size: u64) -> FileEntry {
        let entry = FileEntry {
            path: path.to_string(),
            sha256,
            size,
            content_type: content_type_for(path).to_string(),
        };
        self.files.insert(path.to_string(), entry.clone());
        entry
    }

    /// The entry at `path`, if any.
    pub fn get(&self, path: &str) -> Option<&FileEntry> {
        self.files.get(path)
    }

    /// Drop the entry at `path`, returning it when it was there.
    ///
    /// The blob it named is deliberately left alone — an older generation may
    /// still reference it (see [`super::blobs`]).
    pub fn remove(&mut self, path: &str) -> Option<FileEntry> {
        self.files.remove(path)
    }

    /// Total size of the files the workspace can currently reach, in bytes.
    ///
    /// NOT the quota: superseded and deleted content is still stored and still
    /// costs, and only [`Self::blob_bytes`] counts it. This is the "how big is
    /// my site" number, not the "how much have I used" one.
    pub fn total_bytes(&self) -> u64 {
        self.files.values().map(|entry| entry.size).sum()
    }

    /// Whether any entry names the blob `sha`.
    ///
    /// A referenced blob is necessarily still stored — the collector only
    /// reclaims unreachable ones — so this answers "is this content already
    /// paid for?" without a storage round trip.
    pub fn references(&self, sha: &str) -> bool {
        self.files.values().any(|entry| entry.sha256 == sha)
    }

    /// Charge the workspace for a blob of `bytes` that was just written.
    ///
    /// Call this only when the store actually grew — a deduplicated write
    /// ([`super::blobs::Stored::Deduplicated`]) stored nothing and must not be
    /// charged, or two paths holding one asset would count it twice.
    pub fn record_blob_stored(&mut self, bytes: u64) {
        self.blob_bytes = self.blob_bytes.saturating_add(bytes);
        self.blob_count = self.blob_count.saturating_add(1);
    }

    /// Credit the workspace for a blob of `bytes` that was reclaimed.
    ///
    /// For Plan 4's garbage collector, which is the only thing that removes a
    /// blob. Saturating, so a manifest whose counters were somehow lost cannot
    /// underflow into a workspace that appears to have 16 exabytes of headroom.
    pub fn record_blob_freed(&mut self, bytes: u64) {
        self.blob_bytes = self.blob_bytes.saturating_sub(bytes);
        self.blob_count = self.blob_count.saturating_sub(1);
    }

    /// Every distinct block name the workspace defines sources for, sorted.
    pub fn block_names(&self) -> Vec<String> {
        self.files
            .keys()
            .filter_map(|path| block_name_of(path))
            .map(str::to_string)
            .collect::<std::collections::BTreeSet<String>>()
            .into_iter()
            .collect()
    }
}

/// The `site/` entries, with the prefix stripped, in path order.
///
/// This is the shape a generation's site manifest and the site publisher both
/// want: `site/index.html` in the workspace is `index.html` under the
/// published site root.
pub fn site_manifest(ws: &Workspace) -> Vec<FileEntry> {
    entries_under(ws, SITE_PREFIX)
}

/// The `blocks/<name>/` entries, with the prefix stripped, in path order.
///
/// Stripped for the same reason [`site_manifest`] is: the consumer is a
/// compiler that wants a crate rooted at `Cargo.toml` / `src/lib.rs`, not a
/// tree nested two directories deep. The workspace path is recoverable as
/// `blocks/{name}/{entry.path}`.
pub fn block_sources(ws: &Workspace, name: &str) -> Vec<FileEntry> {
    entries_under(ws, &format!("{BLOCKS_PREFIX}{name}/"))
}

/// The entries whose path starts with `prefix`, with `prefix` removed.
fn entries_under(ws: &Workspace, prefix: &str) -> Vec<FileEntry> {
    ws.files
        .iter()
        .filter_map(|(path, entry)| {
            let relative = path.strip_prefix(prefix)?;
            Some(FileEntry {
                path: relative.to_string(),
                ..entry.clone()
            })
        })
        .collect()
}

/// The block name a `blocks/<name>/…` path belongs to.
fn block_name_of(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(BLOCKS_PREFIX)?;
    let (name, remainder) = rest.split_once('/')?;
    // A bare `blocks/<name>/` never reaches the workspace (validate_path
    // refuses an area root), but the projection must not invent a block from
    // one if it somehow did.
    (!name.is_empty() && !remainder.is_empty()).then_some(name)
}

/// Read the workspace manifest. A missing manifest is an empty workspace —
/// that is what a fresh instance looks like.
///
/// A manifest that is present but does not parse is an error, never an empty
/// workspace: answering "no files" for a workspace that has some would let the
/// next write save a manifest that had dropped every existing entry.
pub async fn load(ctx: &dyn Context) -> Result<Workspace, WaferError> {
    let bytes = match storage::get(ctx, FOLDER, KEY).await {
        Ok((bytes, _info)) => bytes,
        Err(e) if e.code == ErrorCode::NotFound => return Ok(Workspace::default()),
        Err(e) => return Err(e),
    };
    serde_json::from_slice(&bytes).map_err(|e| {
        WaferError::new(
            ErrorCode::Internal,
            format!("dev workspace manifest ({KEY}) did not parse: {e}"),
        )
    })
}

/// Write the workspace manifest.
///
/// Serialized compact, with the file map in path order (it is a
/// [`BTreeMap`]), so the bytes are deterministic for a given workspace. Note
/// this is *deterministic*, not *canonical*: serde emits a struct's fields in
/// declaration order, not sorted, so `workspace.json` is not the sorted-key
/// form design §11.3 requires of a **generation** manifest. That is fine
/// because this file is never hashed — but a generation manifest must be
/// built through `serde_json::Value` (whose maps sort) rather than serialized
/// straight from a struct, as `dev_status`'s fixture already does.
///
/// Compact rather than pretty because this is rewritten on every single file
/// write.
pub async fn save(ctx: &dyn Context, ws: &Workspace) -> Result<(), WaferError> {
    let bytes = serde_json::to_vec(ws).map_err(|e| {
        WaferError::new(
            ErrorCode::Internal,
            format!("serializing the dev workspace manifest: {e}"),
        )
    })?;
    storage::put(ctx, FOLDER, KEY, &bytes, MANIFEST_CONTENT_TYPE).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{blocks::dev::test_support::FakeControl, test_support::TestContext};

    fn populated() -> Workspace {
        let mut ws = Workspace::default();
        ws.insert("site/z.css", "z".to_string(), 3);
        ws.insert("site/a.css", "a".to_string(), 5);
        ws.insert("site/nested/b.html", "b".to_string(), 7);
        ws.insert("blocks/hello/src/lib.rs", "l".to_string(), 11);
        ws.insert("blocks/hello/Cargo.toml", "c".to_string(), 13);
        ws.insert("blocks/other/src/lib.rs", "o".to_string(), 17);
        ws
    }

    #[test]
    fn insert_derives_the_content_type_and_keys_by_path() {
        let mut ws = Workspace::default();
        let entry = ws.insert("site/index.html", "abc".to_string(), 11);
        assert_eq!(entry.content_type, "text/html; charset=utf-8");
        assert_eq!(ws.get("site/index.html"), Some(&entry));
        // The map key and the entry's own path can never disagree.
        for (key, entry) in &ws.files {
            assert_eq!(key, &entry.path);
        }
    }

    #[test]
    fn site_manifest_strips_the_prefix_and_comes_out_sorted() {
        let ws = populated();
        let files = site_manifest(&ws);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["a.css", "nested/b.html", "z.css"]);
        // Everything else on the entry survives the projection.
        assert_eq!(files[0].sha256, "a");
        assert_eq!(files[0].size, 5);
        assert_eq!(files[0].content_type, "text/css; charset=utf-8");
    }

    #[test]
    fn block_sources_are_scoped_to_one_block_and_rooted_at_its_crate() {
        let ws = populated();
        let paths: Vec<String> = block_sources(&ws, "hello")
            .into_iter()
            .map(|f| f.path)
            .collect();
        assert_eq!(paths, vec!["Cargo.toml", "src/lib.rs"]);
        assert!(block_sources(&ws, "missing").is_empty());
        // A name that is a prefix of another must not pull in its files.
        assert!(block_sources(&ws, "hell").is_empty());
    }

    #[test]
    fn totals_and_block_names_are_derived_from_the_entries() {
        let ws = populated();
        assert_eq!(ws.total_bytes(), 3 + 5 + 7 + 11 + 13 + 17);
        assert_eq!(ws.block_names(), vec!["hello", "other"]);
        assert!(Workspace::default().block_names().is_empty());
        assert_eq!(Workspace::default().total_bytes(), 0);
    }

    #[test]
    fn remove_drops_the_entry_and_reports_what_it_dropped() {
        let mut ws = populated();
        let removed = ws.remove("site/a.css").expect("entry was there");
        assert_eq!(removed.sha256, "a");
        assert!(ws.get("site/a.css").is_none());
        assert!(ws.remove("site/a.css").is_none());
    }

    /// The stored bytes are compact and path-ordered, so a given workspace
    /// always serializes identically.
    #[test]
    fn the_manifest_serializes_deterministically() {
        let mut ws = Workspace::default();
        ws.insert("site/z.css", "z".to_string(), 1);
        ws.insert("site/a.css", "a".to_string(), 2);
        ws.record_blob_stored(1);
        ws.record_blob_stored(2);
        let json = serde_json::to_string(&ws).expect("serialize");
        assert_eq!(
            json,
            r#"{"files":{"site/a.css":{"path":"site/a.css","sha256":"a","size":2,"content_type":"text/css; charset=utf-8"},"site/z.css":{"path":"site/z.css","sha256":"z","size":1,"content_type":"text/css; charset=utf-8"}},"blob_bytes":3,"blob_count":2}"#
        );
        assert!(!json.contains('\n'));
    }

    /// A manifest written before the blob counters existed must still load —
    /// it describes a real workspace whose files are still reachable.
    #[test]
    fn a_manifest_without_the_blob_counters_loads_with_them_at_zero() {
        let ws: Workspace = serde_json::from_str(
            r#"{"files":{"site/a.css":{"path":"site/a.css","sha256":"a","size":2,"content_type":"text/css; charset=utf-8"}}}"#,
        )
        .expect("deserialize");
        assert_eq!(ws.files.len(), 1);
        assert_eq!(ws.blob_bytes, 0);
        assert_eq!(ws.blob_count, 0);
    }

    /// The two totals answer different questions, and the quota uses the
    /// second: superseded content is unreachable but still stored.
    #[test]
    fn stored_blob_bytes_outlive_the_entries_that_named_them() {
        let mut ws = Workspace::default();
        ws.insert("site/a.css", "one".to_string(), 100);
        ws.record_blob_stored(100);
        // Overwrite: a new blob, the old one still stored.
        ws.insert("site/a.css", "two".to_string(), 100);
        ws.record_blob_stored(100);
        assert_eq!(ws.total_bytes(), 100);
        assert_eq!(ws.blob_bytes, 200);
        assert_eq!(ws.blob_count, 2);

        // Deleting the entry frees nothing; only the collector does.
        ws.remove("site/a.css");
        assert_eq!(ws.total_bytes(), 0);
        assert_eq!(ws.blob_bytes, 200);

        ws.record_blob_freed(100);
        assert_eq!(ws.blob_bytes, 100);
        assert_eq!(ws.blob_count, 1);
    }

    #[test]
    fn freeing_more_than_was_stored_saturates_at_zero() {
        let mut ws = Workspace::default();
        ws.record_blob_freed(10);
        assert_eq!(ws.blob_bytes, 0);
        assert_eq!(ws.blob_count, 0);
    }

    #[test]
    fn references_reports_whether_any_entry_names_a_blob() {
        let mut ws = Workspace::default();
        ws.insert("site/a.html", "shared".to_string(), 1);
        ws.insert("site/b.html", "shared".to_string(), 1);
        assert!(ws.references("shared"));
        assert!(!ws.references("other"));
        ws.remove("site/a.html");
        assert!(ws.references("shared"), "the other path still names it");
        ws.remove("site/b.html");
        assert!(!ws.references("shared"));
    }

    #[tokio::test]
    async fn a_missing_manifest_loads_as_an_empty_workspace() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        assert_eq!(load(&ctx).await.expect("load"), Workspace::default());
    }

    #[tokio::test]
    async fn the_manifest_round_trips_through_storage() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let ws = populated();
        save(&ctx, &ws).await.expect("save");
        assert_eq!(load(&ctx).await.expect("load"), ws);
    }

    /// A corrupt manifest must not read as "no files": the next write would
    /// then save a manifest that had silently dropped every entry.
    #[tokio::test]
    async fn a_manifest_that_does_not_parse_is_an_error_not_an_empty_workspace() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        storage::put(&ctx, FOLDER, KEY, b"{ not json", MANIFEST_CONTENT_TYPE)
            .await
            .expect("put");
        assert_eq!(
            load(&ctx).await.expect_err("load must fail").code,
            ErrorCode::Internal
        );
    }
}
