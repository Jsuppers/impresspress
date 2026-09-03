//! Which generations the ledger keeps, and deleting the rest.
//!
//! The ledger is append-only while a generation is reachable; it is not
//! infinite. Design §7.3 keeps the last 20 generations, and this module is
//! what makes that sentence true — the rows outside the retained set are
//! *deleted*, not relabelled, and [`super::gc`] then reclaims the content
//! only those rows named.
//!
//! # What "retained" means
//!
//! Three things, and each is load-bearing:
//!
//! * the [`RETAINED_GENERATIONS`] most recent rows — the set the ledger view
//!   shows and a rollback can target;
//! * the generation that is **serving**, however far down the ledger it has
//!   fallen. Twenty refused activations after a good one do not make the good
//!   one collectable: it is what the site currently *is*;
//! * every **in-flight** row (staged, validating, activating). The journal may
//!   name one and boot convergence re-runs it (design §7.3), so deleting one
//!   turns an interrupted activation into an unrecoverable one.
//!
//! The second and third are [`GenerationStatus::survives_retention`], which
//! owns that half of the definition so it cannot be restated differently here
//! and in the query that deletes.
//!
//! # Superseded is not an age
//!
//! `Superseded` is the status of a generation a later one replaced, written by
//! the activation that replaced it. Nothing rewrites a row's status because it
//! got old, so a `Failed` generation still reads as `Failed` for as long as it
//! is kept, and "how the generation ended" and "how old it is" stay two
//! separate questions.
//!
//! # Why deleting, rather than labelling
//!
//! A label leaves the row — and everything it names — reachable forever, which
//! is exactly what the collector cannot work with: the blob store's quota
//! (design §6.6) bounds what is *stored*, and nothing stored can be reclaimed
//! while some row still names it. Deleting is also what makes the retention
//! window a real bound: the ledger holds 20 rows plus whatever is live, rather
//! than every generation the sandbox has ever published with 20 of them
//! unmarked.

use wafer_run::{context::Context, WaferError};

use super::repo::generations::{self, GenerationRow};

/// How many generations the ledger keeps (design §7.3).
///
/// Also the default page size of `GET /b/dev/api/generations`, so the default
/// listing is exactly the set that can still be rolled back to.
pub const RETAINED_GENERATIONS: usize = 20;

/// The rows retention keeps.
///
/// The newest [`RETAINED_GENERATIONS`], followed by any row kept for its
/// status alone that the window did not already hold. Deduplicated by id, so
/// a caller can treat the result as a set — which is what the collector needs
/// it to be.
pub async fn retained(ctx: &dyn Context) -> Result<Vec<GenerationRow>, WaferError> {
    let mut rows = generations::list_recent(ctx, RETAINED_GENERATIONS as i64).await?;
    if rows.len() < RETAINED_GENERATIONS {
        // The whole ledger is inside the window: a second query could only
        // return rows this one already holds.
        return Ok(rows);
    }
    for row in generations::list_retained_by_status(ctx).await? {
        if !rows.iter().any(|kept| kept.id == row.id) {
            rows.push(row);
        }
    }
    Ok(rows)
}

/// Delete every generation outside the retained set, returning the rows that
/// went, newest first.
///
/// Runs at the end of every successful activation — the only moment a
/// generation stops being the newest of anything. Idempotent: a second pass
/// over a ledger already inside the window deletes nothing and returns an
/// empty list.
///
/// The boundary is the oldest row the window keeps, and the pass deletes
/// strictly below it. Paged rather than listed whole: every row a page holds
/// is deleted before the next page is asked for, so an empty page means done
/// and a ledger far past the window is collected in full rather than down to
/// one page's worth of rows.
pub async fn prune(ctx: &dyn Context) -> Result<Vec<GenerationRow>, WaferError> {
    let newest = generations::list_recent(ctx, RETAINED_GENERATIONS as i64).await?;
    let Some(boundary) = newest.get(RETAINED_GENERATIONS - 1) else {
        // Fewer rows than the window holds: nothing is outside it.
        return Ok(Vec::new());
    };

    let mut pruned = Vec::new();
    loop {
        let batch = generations::list_prunable(ctx, boundary).await?;
        if batch.is_empty() {
            return Ok(pruned);
        }
        for row in batch {
            generations::delete(ctx, &row.id).await?;
            pruned.push(row);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        blocks::dev::{
            contracts::SiteManifest,
            generation::{self, GenerationManifest},
            repo,
            repo::generations::{GenerationCause, GenerationStatus, NewGeneration},
            test_support::FakeControl,
        },
        test_support::TestContext,
    };

    /// Append one row in `status`, with an empty manifest.
    async fn row(ctx: &TestContext, status: GenerationStatus) -> GenerationRow {
        let id = repo::new_id();
        let mut manifest = GenerationManifest::staged(SiteManifest::default(), Vec::new());
        manifest.identify(id.clone(), None);
        let row = generations::insert(
            ctx,
            &NewGeneration {
                id,
                parent_id: None,
                cause: GenerationCause::SiteWrite,
                site_manifest_json: generation::canonical_text(&manifest.site).expect("canonical"),
                block_manifest_json: generation::canonical_text(&manifest.blocks)
                    .expect("canonical"),
                manifest_sha256: generation::manifest_sha256(&manifest).expect("hash"),
            },
        )
        .await
        .expect("insert");
        if status != GenerationStatus::Staged {
            generations::set_status(ctx, &row.id, status, None, None)
                .await
                .expect("set status");
        }
        GenerationRow { status, ..row }
    }

    /// The ids the ledger still holds, newest first.
    async fn ledger(ctx: &TestContext) -> Vec<String> {
        generations::list_recent(ctx, 200)
            .await
            .expect("list")
            .into_iter()
            .map(|row| row.id)
            .collect()
    }

    #[tokio::test]
    async fn a_ledger_inside_the_window_is_left_alone() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        for _ in 0..RETAINED_GENERATIONS {
            row(&ctx, GenerationStatus::Superseded).await;
        }
        assert!(prune(&ctx).await.expect("prune").is_empty());
        assert_eq!(ledger(&ctx).await.len(), RETAINED_GENERATIONS);
        assert_eq!(
            retained(&ctx).await.expect("retained").len(),
            RETAINED_GENERATIONS
        );
    }

    #[tokio::test]
    async fn rows_past_the_window_are_deleted_newest_first_and_the_pass_is_idempotent() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let mut ids = Vec::new();
        for _ in 0..(RETAINED_GENERATIONS + 3) {
            ids.push(row(&ctx, GenerationStatus::Superseded).await.id);
        }

        let pruned = prune(&ctx).await.expect("prune");
        assert_eq!(
            pruned.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            vec![ids[2].clone(), ids[1].clone(), ids[0].clone()],
            "the three oldest went, newest of them first",
        );
        assert_eq!(ledger(&ctx).await.len(), RETAINED_GENERATIONS);
        assert!(prune(&ctx).await.expect("prune again").is_empty());
    }

    /// The pass is not bounded by one listing page: a ledger far past the
    /// window is collected all the way down.
    ///
    /// 250 rows against a 200-row page cap: 230 are outside the window, so
    /// the second page is what finishes the job.
    #[tokio::test]
    async fn a_ledger_deeper_than_one_page_is_pruned_in_full() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        for _ in 0..250 {
            row(&ctx, GenerationStatus::Superseded).await;
        }
        assert_eq!(prune(&ctx).await.expect("prune").len(), 230);
        assert_eq!(ledger(&ctx).await.len(), RETAINED_GENERATIONS);
    }

    /// The active generation is what the site *is*. Age cannot collect it, and
    /// neither the pass nor the retained set may lose sight of it.
    #[tokio::test]
    async fn the_serving_generation_survives_however_far_down_it_has_fallen() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let active = row(&ctx, GenerationStatus::Active).await;
        let staged = row(&ctx, GenerationStatus::Staged).await;
        for _ in 0..(RETAINED_GENERATIONS + 5) {
            row(&ctx, GenerationStatus::Failed).await;
        }

        let pruned = prune(&ctx).await.expect("prune");
        assert_eq!(pruned.len(), 5, "only the failed rows past the window");
        assert!(pruned.iter().all(|r| r.status == GenerationStatus::Failed));

        let ledger = ledger(&ctx).await;
        assert!(
            ledger.contains(&active.id),
            "the serving generation is kept"
        );
        assert!(ledger.contains(&staged.id), "so is the in-flight one");

        // And the collector's view of "retained" holds them too, even though
        // the window alone would not.
        let retained_ids: Vec<String> = retained(&ctx)
            .await
            .expect("retained")
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert!(retained_ids.contains(&active.id));
        assert!(retained_ids.contains(&staged.id));
        assert_eq!(retained_ids.len(), RETAINED_GENERATIONS + 2);
    }

    /// Retention deletes by age; it never rewrites how a generation ended.
    #[tokio::test]
    async fn a_failed_row_inside_the_window_keeps_its_status() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let failed = row(&ctx, GenerationStatus::Failed).await;
        for _ in 0..(RETAINED_GENERATIONS - 1) {
            row(&ctx, GenerationStatus::Superseded).await;
        }
        prune(&ctx).await.expect("prune");
        assert_eq!(
            generations::get(&ctx, &failed.id)
                .await
                .expect("get")
                .status,
            GenerationStatus::Failed,
        );
    }
}
