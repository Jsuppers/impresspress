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
//! # The ordering invariant
//!
//! **List first, then read the roots.** The candidate set is fixed by the
//! folder listing, and every root is read after it, so anything stored *after*
//! the listing is not a candidate at all and needs no root to protect it. The
//! reverse order — roots, then listing — has a hole with no bottom: a compile
//! that inserts its build row after the roots are read and stores its bytes
//! before the listing produces an object that is a candidate and has no root,
//! and the collector deletes the artifact the compile is about to activate.
//!
//! That is why staging inserts its build row *before* it stores the artifact
//! (`super::blocks_api`). Together the two orderings close the interval: bytes
//! in the listing were stored before it, their row was written before them, so
//! the root read that follows the listing cannot miss it.
//!
//! The workspace is read under the same lock a file write takes, after the
//! listing, for the same reason in the other store: a write stores its blob
//! and saves the entry naming it inside one lock hold.
//!
//! Each artifact is asked about once more, immediately before it goes, in case
//! a stage arrived in between ([`repo::builds::is_in_flight_for_artifact`]) —
//! cheap, because only a deletion pays for it.
//!
//! # When it runs
//!
//! At the end of every successful activation, after retention has pruned
//! (`super::activation`), and after a `blocks/` file delete, which changes what
//! the workspace names without publishing anything. Those are the two moments
//! content stops being reachable.

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

/// A seam the collector yields at, once, between its listing and its roots.
///
/// The module's whole soundness argument is an ordering one, and orderings are
/// exactly what a serial test cannot observe: nothing in the fixture's storage
/// or database yields, so no compile can interleave itself into the gap the
/// argument is about. This is the one place a test can put something there.
///
/// Production passes [`Uninterrupted`]. There is no `cfg(test)` on the trait
/// because a seam that only exists under `cfg(test)` is a seam whose shipped
/// build is a different function from the tested one.
#[wafer_block::wafer_async_trait]
pub trait GcInterleave: wafer_run::MaybeSend + wafer_run::MaybeSync {
    /// Called once, after both folders have been listed and before any root
    /// has been read.
    async fn after_listing(&self);
}

/// The [`GcInterleave`] production uses: nothing happens in the gap.
pub struct Uninterrupted;

#[wafer_block::wafer_async_trait]
impl GcInterleave for Uninterrupted {
    async fn after_listing(&self) {}
}

/// Delete every blob and artifact nothing retained can reach.
pub async fn collect(ctx: &dyn Context, shared: &DevShared) -> Result<GcReport, WaferError> {
    collect_interleaved(ctx, shared, &Uninterrupted).await
}

/// [`collect`], with the [`GcInterleave`] seam exposed.
///
/// Public for `tests/dev_gc.rs`; every production caller wants [`collect`].
pub async fn collect_interleaved(
    ctx: &dyn Context,
    shared: &DevShared,
    interleave: &dyn GcInterleave,
) -> Result<GcReport, WaferError> {
    // 1. The listings, first and before anything else is read. They fix the
    //    candidate set: an object stored after this point is not in it.
    let blob_objects = list_all(ctx, blobs::FOLDER).await?;
    let artifact_objects = list_all(ctx, artifacts::FOLDER).await?;

    interleave.after_listing().await;

    // 2. The roots, all read after the listings.
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
    // Plus every compile that has stored an artifact and not yet reached a
    // generation — a *status*, not an age: a browser compile takes tens of
    // seconds, and a rule that expired the protection by time would collect
    // the artifact of a compile that was merely slow.
    for build in repo::builds::list_in_flight(ctx).await? {
        live_artifacts.insert(build.artifact_sha256);
    }

    // 3. The deletes.
    let mut report = GcReport::default();
    collect_blobs(ctx, shared, blob_objects, live_blobs, &mut report).await?;
    collect_artifacts(ctx, artifact_objects, &live_artifacts, &mut report).await?;
    Ok(report)
}

/// What the two stores, the workspace and the ledger hold.
///
/// Read from the counters and the ledger, never by walking the stores. The
/// `/b/dev` page polls status every ~300 ms while a tool call is outstanding,
/// and a storage `list` is `O(folder)` on the OPFS backend the sandbox runs
/// on — one full listing path is enough, and it belongs to [`collect`], which
/// runs once per activation rather than three times a second.
///
/// The two sources are the same bytes counted at the two ends that maintain
/// them: [`workspace::Workspace`]'s blob counters are written by the file
/// writes that store blobs and credited by [`collect`] as it frees them, and
/// the builds table has a row per stored artifact because staging writes the
/// row before the bytes and [`collect`] deletes the row with them.
pub async fn storage_usage(ctx: &dyn Context) -> Result<StorageUsage, WaferError> {
    let ws = workspace::load(ctx).await?;
    let artifacts = repo::builds::artifact_index(ctx).await?;
    Ok(StorageUsage {
        blobs: ws.blob_count,
        blobs_bytes: ws.blob_bytes,
        artifacts: artifacts.len() as u32,
        artifacts_bytes: artifacts.values().sum(),
        workspace_files: ws.files.len() as u32,
        retained_generations: retention::retained(ctx).await?.len() as u32,
    })
}

/// Delete the unreachable blobs and credit the workspace for them.
async fn collect_blobs(
    ctx: &dyn Context,
    shared: &DevShared,
    candidates: Vec<ObjectInfo>,
    mut live: BTreeSet<String>,
    report: &mut GcReport,
) -> Result<(), WaferError> {
    // Under the same lock every file mutation takes, and for two reasons.
    // Crediting the freed bytes is a read-modify-write of the whole manifest,
    // so a write that loaded before it and saved after it would put them back.
    // And a write stores its blob *inside* that lock, before it saves the
    // entry naming it — so reading the workspace here, after the listing and
    // under the lock, cannot miss an entry for a blob that is a candidate:
    // either the write had not stored its blob when the listing ran (not a
    // candidate) or it had already saved the entry (a root).
    //
    // Deadlock-free for the reason `activation::adopt_site` documents:
    // `files.rs` releases the lock before it asks for an activation, so
    // nothing holding it is ever waiting on the queue this runs under.
    let _serialized = shared.workspace.lock().await;
    let mut ws = workspace::load(ctx).await?;
    live.extend(ws.files.values().map(|entry| entry.sha256.clone()));

    let mut credited = false;
    for object in candidates {
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
    candidates: Vec<ObjectInfo>,
    live: &BTreeSet<String>,
    report: &mut GcReport,
) -> Result<(), WaferError> {
    for object in candidates {
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
        // One last look, immediately before the object goes: the root set was
        // read a few awaits ago, and a stage that inserted its row after that
        // read would not be in it. Only a deletion pays for this query.
        if repo::builds::is_in_flight_for_artifact(ctx, sha).await? {
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
/// The only full-listing path in the block, and it is reached only from
/// [`collect_interleaved`] — `dev_status` reports the stores from the counters
/// that track them, so nothing walks a folder on a poll.
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
