//! `/b/dev/api/generations*` — reading the publication ledger and rolling
//! back to an entry in it.
//!
//! # Rolling back is publishing forwards
//!
//! History is append-only (design §7.2): a rollback does not move the ledger's
//! head backwards, it stages a *new* generation carrying an old one's
//! manifests. That is what makes "roll back the rollback" an ordinary
//! operation, and what keeps every id in the ledger meaning exactly one set of
//! bytes.
//!
//! The workspace follows, because it has to. The workspace — not the ledger —
//! is what the next site write builds its manifest from, so a rollback that
//! republished the old site and left the workspace holding the new one would
//! be silently undone by the very next keystroke.

use wafer_run::{context::Context, ErrorCode, Message, OutputStream, WaferError};

use super::{
    activation::{self, ActivationIntent},
    contracts::{
        ActivationResponse, GenerationDetail, GenerationListQuery, GenerationListResponse,
        SiteManifest,
    },
    generation, no_store, no_store_error,
    repo::{self, generations::GenerationCause},
    workspace::{self, SITE_PREFIX},
    DevShared,
};
use crate::http::err_internal;

/// Default page size: the retention window, so the default listing is exactly
/// the set of generations that can still be rolled back to.
const DEFAULT_LIMIT: u32 = activation::RETAINED_GENERATIONS as u32;

/// `GET /b/dev/api/generations` — the ledger, newest first.
pub async fn handle_list(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let limit = GenerationListQuery::from_message(msg)
        .limit
        .unwrap_or(DEFAULT_LIMIT);
    match list(ctx, limit).await {
        Ok(response) => no_store().json(&response),
        Err(e) => err_internal("dev generation list", e),
    }
}

async fn list(ctx: &dyn Context, limit: u32) -> Result<GenerationListResponse, WaferError> {
    let rows = repo::generations::list_recent(ctx, i64::from(limit)).await?;
    let mut generations = Vec::with_capacity(rows.len());
    for row in &rows {
        let manifest = generation::from_row(row)?;
        generations.push(generation::summarize(row, &manifest));
    }
    Ok(GenerationListResponse { generations })
}

/// `GET /b/dev/api/generations/{id}` — one generation, its manifest and what
/// it changed.
pub async fn handle_detail(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let Some(id) = generation_id(msg) else {
        return no_store_error(ErrorCode::InvalidArgument, "the path names no generation");
    };
    match detail(ctx, &id).await {
        Ok(response) => no_store().json(&response),
        // A generation id that is not in the ledger is a 404 the caller can
        // act on, not an internal failure.
        Err(e) if e.code == ErrorCode::NotFound => {
            no_store_error(ErrorCode::NotFound, &format!("no generation {id:?}"))
        }
        Err(e) => err_internal("dev generation detail", e),
    }
}

async fn detail(ctx: &dyn Context, id: &str) -> Result<GenerationDetail, WaferError> {
    let (row, manifest) = generation::load(ctx, id).await?;
    let parent = match row.parent_id.as_deref() {
        Some(parent_id) => Some(generation::load(ctx, parent_id).await?.1),
        None => None,
    };
    Ok(GenerationDetail {
        summary: generation::summarize(&row, &manifest),
        diff_from_parent: generation::diff(parent.as_ref(), &manifest),
        manifest,
    })
}

/// `POST /b/dev/api/generations/{id}/rollback` — republish an earlier
/// generation as a new one.
pub async fn handle_rollback(ctx: &dyn Context, shared: &DevShared, msg: &Message) -> OutputStream {
    let Some(id) = generation_id(msg) else {
        return no_store_error(ErrorCode::InvalidArgument, "the path names no generation");
    };

    let target = match generation::load(ctx, &id).await {
        Ok((_row, manifest)) => manifest,
        Err(e) if e.code == ErrorCode::NotFound => {
            return no_store_error(ErrorCode::NotFound, &format!("no generation {id:?}"));
        }
        Err(e) => return err_internal("dev generation rollback", e),
    };

    // A new generation carrying the target's contents — not the target row
    // re-activated. The intent carries the target manifest rather than its id
    // because a ledger row is immutable: nothing can have changed it between
    // the lookup above (which is what answers the `404`) and the dequeue. The
    // new id and parent are assigned inside the queue, which is what keeps the
    // history a straight line.
    let intent = ActivationIntent::Rollback {
        target: target.clone(),
    };
    // A rollback replaces the whole block set, exactly as a compile or a
    // removal does — so it takes the same lock, for the same reason. Without
    // it, a compile that read the active block set just before this rollback
    // ran would compose its own set from a snapshot the rollback has already
    // replaced, and would put the rolled-back block straight back.
    //
    // The lock spans the workspace adoption too: a rollback that published an
    // old site but was overtaken before it could write the workspace would
    // leave the two disagreeing, and the next site write would silently undo
    // it.
    let _compiling = shared.compile.lock().await;
    let outcome = match activation::request(ctx, shared, GenerationCause::Rollback, intent).await {
        Ok(outcome) => outcome,
        Err(e) => return e.into_response(),
    };

    // Only now: a rollback that never went live must not leave the workspace
    // pointing at content the published site does not have. The reverse order
    // would rewrite the workspace on every refused rollback — including the
    // ordinary case of a target whose blobs have been collected.
    if let Err(e) = adopt_site(ctx, shared, &target.site).await {
        return err_internal("dev workspace rollback", e);
    }

    no_store().json(&ActivationResponse {
        generation: outcome.generation,
        progress: outcome.progress,
    })
}

/// Replace the workspace's `site/` half with `site`, leaving `blocks/` alone.
///
/// The blob counters are untouched on purpose: every entry here names content
/// the store already holds (a generation cannot reference a blob that was
/// never written), so nothing has been stored and nothing has been reclaimed.
/// Charging for it would make a rollback look like a fresh upload of the whole
/// site and eat the workspace's quota for content it already paid for.
async fn adopt_site(
    ctx: &dyn Context,
    shared: &DevShared,
    site: &SiteManifest,
) -> Result<(), WaferError> {
    // Under the same lock every file mutation takes: this is a
    // read-modify-write of the whole manifest, and a concurrent write that
    // loaded before it and saved after it would resurrect the site half this
    // is replacing.
    let _serialized = shared.workspace.lock().await;
    let mut ws = workspace::load(ctx).await?;
    let stale: Vec<String> = ws
        .files
        .keys()
        .filter(|path| path.starts_with(SITE_PREFIX))
        .cloned()
        .collect();
    for path in stale {
        ws.remove(&path);
    }
    for entry in &site.files {
        // Through `Workspace::insert` — the only writer of `files` — so the
        // map key and `FileEntry::path` cannot drift apart, and the content
        // type is derived from the path exactly as a write would derive it.
        ws.insert(
            &format!("{SITE_PREFIX}{}", entry.path),
            entry.sha256.clone(),
            entry.size,
        );
    }
    workspace::save(ctx, &ws).await
}

/// The `{id}` the route bound, or `None` when it is empty.
fn generation_id(msg: &Message) -> Option<String> {
    let id = msg.var("id");
    (!id.is_empty()).then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        blocks::dev::{
            test_support::FakeControl,
            workspace::{FileEntry, Workspace},
        },
        test_support::TestContext,
    };

    fn entry(path: &str, sha: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            sha256: sha.to_string(),
            size: 4,
            content_type: "text/html; charset=utf-8".to_string(),
        }
    }

    /// Adopting a site manifest replaces the whole `site/` half and leaves
    /// `blocks/` alone — a rollback is a site+block republish, not a workspace
    /// wipe.
    #[tokio::test]
    async fn adopting_a_site_replaces_only_the_site_half() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let mut ws = Workspace::default();
        ws.insert("site/index.html", "new".to_string(), 4);
        ws.insert("site/added-later.css", "css".to_string(), 4);
        ws.insert("blocks/hello/src/lib.rs", "rs".to_string(), 4);
        ws.record_blob_stored(4);
        ws.record_blob_stored(4);
        ws.record_blob_stored(4);
        workspace::save(&ctx, &ws).await.expect("save");

        adopt_site(
            &ctx,
            &DevShared::new(FakeControl::new()),
            &SiteManifest {
                files: vec![entry("index.html", "old")],
            },
        )
        .await
        .expect("adopt");

        let after = workspace::load(&ctx).await.expect("load");
        assert_eq!(
            after.files.keys().collect::<Vec<_>>(),
            vec!["blocks/hello/src/lib.rs", "site/index.html"],
            "a path the target generation did not have must be dropped"
        );
        assert_eq!(after.get("site/index.html").expect("entry").sha256, "old");
        // Nothing was stored and nothing was reclaimed: the target's blobs
        // were already paid for.
        assert_eq!(after.blob_bytes, ws.blob_bytes);
        assert_eq!(after.blob_count, ws.blob_count);
        // And the projection round-trips: what was adopted is what a site
        // manifest reads back.
        assert_eq!(
            workspace::site_manifest(&after),
            vec![entry("index.html", "old")]
        );
    }
}
