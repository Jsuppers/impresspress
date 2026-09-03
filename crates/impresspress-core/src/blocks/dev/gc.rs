//! Reclaiming the blobs and artifacts nothing can reach any more.
//!
//! Both stores are content-addressed and nothing in them is ever edited in
//! place, so left alone they only grow: overwriting one page two hundred times
//! leaves two hundred blobs behind, and every compile of a block leaves
//! another artifact. Design §6.6 bounds a workspace at 64 MiB of *stored*
//! blobs, which is a bound only because this runs.
//!
//! # What is reachable
//!
//! A **blob** is reachable from a [retained][super::retention] generation's
//! site manifest, or from a workspace entry. The second half is not
//! redundant: a block's sources live in the workspace and in no generation at
//! all — a generation carries the compiled artifact, not the crate it came
//! from — so a collector that only read the ledger would delete a block's
//! source tree the moment it was written.
//!
//! An **artifact** is reachable from a retained generation's block manifest,
//! or from a build row young enough that its compile may still be on its way
//! to one. Staging stores the row before the bytes and only asks for an
//! activation once the guest has been accepted; a site write's collection can
//! run in that window, and without the build rows it would delete the artifact
//! the compile is about to activate.
//!
//! Everything else in the two folders goes. Unreachable is not "probably
//! unused": a blob no retained generation and no workspace path names cannot
//! be read back by any request the sandbox can serve, because every read
//! addresses content through one of those two.
//!
//! # When it runs
//!
//! At the end of every successful activation, after retention has pruned
//! (`super::activation`). That is the only moment a generation stops being
//! reachable, and it is already the moment the workspace may have stopped
//! naming a blob — so the two things that can create garbage are both
//! immediately behind it.

use std::collections::BTreeSet;

use wafer_core::clients::storage::{self, ListOptions, ObjectInfo};
use wafer_run::{context::Context, ErrorCode, WaferError};

use super::{
    artifacts, blobs, contracts::StorageUsage, generation, repo, retention, workspace, DevShared,
};

/// How many objects one storage listing asks for.
///
/// Large enough that a sandbox-sized store is one round trip, small enough
/// that a page is not an unbounded allocation.
const PAGE: i64 = 500;

/// What one collection reclaimed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// How many blobs were deleted.
    pub blobs_deleted: u32,
    /// How many artifacts were deleted.
    pub artifacts_deleted: u32,
    /// Total size of everything deleted, blobs and artifacts together.
    ///
    /// Not the same number the workspace is credited with: only the blob half
    /// counts against its quota ([`workspace::Workspace::blob_bytes`]), which
    /// is what design §6.6 bounds. This is the storage figure.
    pub bytes_freed: u64,
}

/// Delete every blob and artifact nothing retained can reach.
pub async fn collect(ctx: &dyn Context, shared: &DevShared) -> Result<GcReport, WaferError> {
    let retained = retention::retained(ctx).await?;
    let mut live_blobs = BTreeSet::new();
    let mut live_artifacts = BTreeSet::new();
    for row in &retained {
        // Through the manifest rather than the stored column text: the row's
        // two halves ARE the manifest (`generation::from_row` is exact), and
        // reading the shas off a parsed manifest is what keeps this and the
        // activation's own content check reading the same fields.
        let manifest = generation::from_row(row)?;
        live_blobs.extend(manifest.site.files.iter().map(|entry| entry.sha256.clone()));
        live_artifacts.extend(
            manifest
                .blocks
                .iter()
                .map(|spec| spec.artifact_sha256.clone()),
        );
    }

    // A compile that has not reached a generation yet. The boundary is the
    // oldest row retention kept: a build older than that belongs to an era
    // whose generations have already gone, so nothing it staged can still be
    // in flight.
    let oldest_retained = retained.iter().map(|row| row.created_at.as_str()).min();
    for build in repo::builds::list_since(ctx, oldest_retained).await? {
        live_artifacts.insert(build.artifact_sha256);
    }

    let mut report = GcReport::default();
    collect_blobs(ctx, shared, live_blobs, &mut report).await?;
    collect_artifacts(ctx, &live_artifacts, &mut report).await?;
    Ok(report)
}

/// What the two stores and the workspace currently hold.
///
/// Computed from the listings on every call rather than kept as a counter:
/// these are the figures a reader checks the collector *against*, and a
/// counter that drifted would be indistinguishable from a collector that had
/// stopped running.
pub async fn storage_usage(ctx: &dyn Context) -> Result<StorageUsage, WaferError> {
    let blobs = list_all(ctx, blobs::FOLDER).await?;
    let artifacts = list_all(ctx, artifacts::FOLDER).await?;
    let ws = workspace::load(ctx).await?;
    Ok(StorageUsage {
        blobs: blobs.len() as u32,
        blobs_bytes: total_bytes(&blobs),
        artifacts: artifacts.len() as u32,
        artifacts_bytes: total_bytes(&artifacts),
        workspace_files: ws.files.len() as u32,
        retained_generations: retention::retained(ctx).await?.len() as u32,
    })
}

/// Delete the unreachable blobs and credit the workspace for them.
async fn collect_blobs(
    ctx: &dyn Context,
    shared: &DevShared,
    mut live: BTreeSet<String>,
    report: &mut GcReport,
) -> Result<(), WaferError> {
    // Under the same lock every file mutation takes, and for two reasons.
    // Crediting the freed bytes is a read-modify-write of the whole manifest,
    // so a write that loaded before it and saved after it would put them back.
    // And a write stores its blob *inside* that lock, before it saves the
    // entry naming it — so holding the lock across the listing is what stops
    // this deleting a blob whose entry is a few instructions away.
    //
    // Deadlock-free for the reason `activation::adopt_site` documents:
    // `files.rs` releases the lock before it asks for an activation, so
    // nothing holding it is ever waiting on the queue this runs under.
    let _serialized = shared.workspace.lock().await;
    let mut ws = workspace::load(ctx).await?;
    live.extend(ws.files.values().map(|entry| entry.sha256.clone()));

    let mut credited = false;
    for object in list_all(ctx, blobs::FOLDER).await? {
        if live.contains(&object.key) {
            continue;
        }
        blobs::delete(ctx, &object.key).await?;
        let size = size_of(&object);
        ws.record_blob_freed(size);
        report.blobs_deleted += 1;
        report.bytes_freed += size;
        credited = true;
    }
    // Only when something changed: the collector runs after every activation,
    // and rewriting `workspace.json` each time to store the same bytes would
    // make every keystroke cost an extra object write.
    if credited {
        workspace::save(ctx, &ws).await?;
    }
    Ok(())
}

/// Delete the unreachable artifacts and the build rows that named them.
async fn collect_artifacts(
    ctx: &dyn Context,
    live: &BTreeSet<String>,
    report: &mut GcReport,
) -> Result<(), WaferError> {
    for object in list_all(ctx, artifacts::FOLDER).await? {
        // A key this block did not write is left alone. Nothing else writes
        // the folder, so this arm is unreachable in practice — but deleting an
        // object whose hash cannot be read is deleting something the collector
        // cannot claim to have reasoned about.
        let Some(sha) = artifacts::sha_of_key(&object.key) else {
            continue;
        };
        if live.contains(sha) {
            continue;
        }
        artifacts::delete(ctx, sha).await?;
        // The rows go with the bytes, and in that order: a row claiming an
        // accepted artifact the store no longer holds is what
        // `repo::builds::latest_valid_for_artifact` would hand the
        // duplicate-tool check as a loadable block.
        repo::builds::delete_for_artifact(ctx, sha).await?;
        report.artifacts_deleted += 1;
        report.bytes_freed += size_of(&object);
    }
    Ok(())
}

/// Every object in `folder`, walked until the listing is exhausted.
///
/// Two paging modes, because the backends this runs on differ. A store that
/// answers a `next_cursor` is paged by that token, which is what keeps a deep
/// page from re-walking the prefix; one that does not — the sandbox's own
/// in-memory and filesystem backends — is paged by offset. A cursor-only walk
/// would stop after the first page on those and quietly under-collect, which
/// is the one failure mode a collector must not have.
///
/// A folder nothing has written yet is an empty listing. The backends
/// genuinely disagree about it (see [`blobs::exists`]) and an empty store has
/// nothing to collect either way.
async fn list_all(ctx: &dyn Context, folder: &str) -> Result<Vec<ObjectInfo>, WaferError> {
    let mut objects = Vec::new();
    let mut cursor = Some(String::new());
    let mut offset = 0i64;
    loop {
        let page = match storage::list(
            ctx,
            folder,
            &ListOptions {
                prefix: String::new(),
                limit: PAGE,
                offset,
                cursor: cursor.clone(),
            },
        )
        .await
        {
            Ok(page) => page,
            Err(e) if e.code == ErrorCode::NotFound => return Ok(objects),
            Err(e) => return Err(e),
        };
        let count = page.objects.len() as i64;
        objects.extend(page.objects);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            // No token, and the page was full: an offset-only backend, which
            // pages by hand from here.
            None if count == PAGE => {
                cursor = None;
                offset += count;
            }
            None => return Ok(objects),
        }
    }
}

/// One object's size, as a byte count.
///
/// `ObjectInfo::size` is signed because the wire type is; a negative size is
/// not a thing an object store can hold, and clamping beats a wrapping cast
/// that would credit the workspace with sixteen exabytes.
fn size_of(object: &ObjectInfo) -> u64 {
    object.size.max(0) as u64
}

fn total_bytes(objects: &[ObjectInfo]) -> u64 {
    objects.iter().map(size_of).sum()
}
