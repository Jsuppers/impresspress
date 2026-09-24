//! Moves each index registered under a name with uppercase letters to its
//! lowercase name, once, when the vector block starts.
//!
//! Index names used to admit `[A-Za-z0-9_]`. The database layer now accepts
//! only plain lowercase identifiers as table names, and every vector backend
//! addresses an index's tables by that rule, so an index created as `Docs`
//! (`impresspress__vector__Docs` in the registry) can no longer be opened,
//! queried or deleted under either spelling. `vector.rename_index` is the one
//! operation that accepts such a legacy name: it moves the index's tables,
//! entries and keyword search to the lowercase name atomically. This step
//! calls it for every registry row whose name is not a plain identifier and
//! then renames that registry row.
//!
//! Idempotent, so every start runs it:
//! - a row already lowercase is skipped;
//! - `NotFound` from the rename means no index is stored under the legacy
//!   spelling. When the lowercase index exists, an earlier start moved the
//!   index and stopped before renaming the row, so the row is renamed now.
//! - When a registry row already holds the lowercase name, two indexes differ
//!   only by case. They are never merged: the legacy one is left as it is and
//!   named in an error log, for the operator to delete one of the two. The
//!   backend refuses the same case with `AlreadyExists`, which is reported the
//!   same way.
//!
//! A transient failure (`Unavailable`) fails the block's `Init`, so the
//! runtime retries it. Any other refusal names the index in an error log and
//! leaves it, so one unmovable index does not take the vector block down.

use wafer_block::db::{Filter, FilterOp, SortField};
use wafer_core::clients::{database as db, vector as vclient};
use wafer_run::{context::Context, ErrorCode, WaferError};

use super::service::{vector_backend_available, REGISTRY_TABLE};
use crate::{
    db_read::{self, Bound},
    util::RecordExt,
};

/// What [`move_legacy_index`] did with one registry row.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Moved {
    /// The index and its registry row now have the lowercase name.
    Renamed,
    /// A registry row or an index already has the lowercase name; nothing
    /// changed.
    Twin,
    /// The backend refused the move for another reason; nothing changed.
    Refused(String),
}

/// Run the move for every registry row whose name is not a plain identifier.
pub(crate) async fn rename_legacy_indexes(ctx: &dyn Context) -> Result<(), WaferError> {
    if !vector_backend_available(ctx) {
        return Ok(());
    }
    let rows = db_read::list_bounded_sorted(
        ctx,
        REGISTRY_TABLE,
        Vec::new(),
        vec![SortField {
            field: "prefixed_name".to_string(),
            desc: false,
        }],
        Bound::Curated("vector indexes are registered from the vector admin surface"),
    )
    .await?;
    let names: Vec<String> = rows
        .iter()
        .map(|row| row.str_field("prefixed_name").to_string())
        .collect();
    for legacy in names
        .iter()
        .filter(|name| !wafer_block::db::is_plain_ident(name))
    {
        let lowercase = legacy.to_ascii_lowercase();
        let registered = names.contains(&lowercase);
        match move_legacy_index(ctx, legacy, &lowercase, registered).await? {
            Moved::Renamed => {
                tracing::info!(from = %legacy, to = %lowercase, "vector index moved to its lowercase name");
            }
            Moved::Twin => {
                tracing::error!(
                    index = %legacy,
                    twin = %lowercase,
                    "two vector indexes differ only by case; the uppercase one cannot be \
                     reached and is left as it is — delete one of the two"
                );
            }
            Moved::Refused(reason) => {
                tracing::error!(index = %legacy, to = %lowercase, reason = %reason, "vector index could not be moved to its lowercase name");
            }
        }
    }
    Ok(())
}

/// Move the index registered as `legacy` to `lowercase`, then its registry
/// row. `registered` says whether a registry row already holds `lowercase`.
pub(crate) async fn move_legacy_index(
    ctx: &dyn Context,
    legacy: &str,
    lowercase: &str,
    registered: bool,
) -> Result<Moved, WaferError> {
    if registered {
        return Ok(Moved::Twin);
    }
    match vclient::rename_index(ctx, legacy, lowercase).await {
        Ok(()) => {}
        Err(e) if e.code == ErrorCode::NotFound => match vclient::count(ctx, lowercase).await {
            // Moved by an earlier start that did not get to the row.
            Ok(_) => {}
            Err(e) if e.code == ErrorCode::NotFound => {
                return Ok(Moved::Refused(format!(
                    "no index is stored under {legacy:?} or {lowercase:?}"
                )));
            }
            Err(e) => return Err(e),
        },
        Err(e) if e.code == ErrorCode::AlreadyExists => return Ok(Moved::Twin),
        Err(e) if e.code == ErrorCode::Unavailable => return Err(e),
        Err(e) => return Ok(Moved::Refused(e.message)),
    }
    db::update_by_filters(
        ctx,
        REGISTRY_TABLE,
        vec![Filter {
            field: "prefixed_name".to_string(),
            operator: FilterOp::Equal,
            value: serde_json::json!(legacy),
        }],
        crate::util::json_map(serde_json::json!({ "prefixed_name": lowercase })),
    )
    .await?;
    Ok(Moved::Renamed)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use wafer_block::{
        db::{Filter, FilterOp, SortField},
        wire::database::OnConflict,
    };
    use wafer_block_sqlite::vector::SqliteVecService;
    use wafer_core::{
        clients::{database as db, vector as vclient},
        interfaces::vector::service::{EmbeddingService, SearchMode, VectorEntry},
        service_blocks::vector::VectorBlock,
    };

    use super::{move_legacy_index, rename_legacy_indexes, Moved, REGISTRY_TABLE};
    use crate::{
        db_read::{self, Bound},
        test_support::TestContext,
        util::RecordExt,
    };

    const LEGACY: &str = "impresspress__vector__Docs";
    const LOWERCASE: &str = "impresspress__vector__docs";

    /// The vector block's embedding half, never reached by these tests.
    struct NoEmbedding;

    #[wafer_block::wafer_async_trait]
    impl EmbeddingService for NoEmbedding {
        fn model(&self) -> &str {
            "none"
        }
        fn dimensions(&self) -> u32 {
            3
        }
        async fn embed(
            &self,
            _texts: Vec<String>,
        ) -> wafer_core::interfaces::vector::service::Result<Vec<Vec<f32>>> {
            unreachable!("these tests never embed")
        }
    }

    fn blob(v: [f32; 3]) -> Vec<u8> {
        v.iter().flat_map(|f| f.to_le_bytes()).collect()
    }

    /// A SQLite file holding one index as the code before lowercase names
    /// wrote it: the statements `VectorIndexSchema` rendered then, with the
    /// mixed-case name spliced in unquoted, and two entries written the way
    /// its upsert wrote them.
    fn legacy_index_file(dir: &tempfile::TempDir) -> std::path::PathBuf {
        // Registers sqlite-vec as an auto-extension for every connection
        // opened after it.
        drop(SqliteVecService::open_in_memory().expect("register sqlite-vec"));
        let path = dir.path().join("vectors.sqlite3");
        let conn = rusqlite::Connection::open(&path).expect("open the vector file");
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE {LEGACY}_vec USING vec0(embedding float[3]);\n\
             CREATE TABLE {LEGACY}_meta(\n\
                id TEXT PRIMARY KEY,\n\
                rowid INTEGER NOT NULL,\n\
                metadata TEXT,\n\
                text TEXT\n\
             );"
        ))
        .expect("create the legacy index");
        for (rowid, id, vector) in [(1_i64, "a", [1.0, 0.0, 0.0]), (2, "b", [0.0, 1.0, 0.0])] {
            conn.execute(
                &format!(
                    "INSERT INTO {LEGACY}_meta(id, rowid, metadata, text) VALUES (?1, ?2, ?3, '')"
                ),
                rusqlite::params![id, rowid, format!("{{\"doc\":\"{id}\"}}")],
            )
            .expect("legacy meta row");
            conn.execute(
                &format!("INSERT INTO {LEGACY}_vec(rowid, embedding) VALUES (?1, ?2)"),
                rusqlite::params![rowid, blob(vector)],
            )
            .expect("legacy vector row");
        }
        path
    }

    /// A context with the vector block's schema and a real
    /// `wafer-run/vector` over `SqliteVecService` on `path`.
    async fn ctx_over(path: &std::path::Path) -> TestContext {
        let service =
            SqliteVecService::new(rusqlite::Connection::open(path).expect("open the vector file"))
                .expect("vector service");
        let mut ctx = TestContext::with_vector().await;
        ctx.register_block(
            "wafer-run/vector",
            Arc::new(VectorBlock::new(Arc::new(service), Arc::new(NoEmbedding))),
        );
        ctx
    }

    async fn register(ctx: &TestContext, name: &str, model: &str) {
        db::upsert(
            ctx,
            REGISTRY_TABLE,
            vec![
                ("prefixed_name".to_string(), serde_json::json!(name)),
                ("model".to_string(), serde_json::json!(model)),
                ("dimensions".to_string(), serde_json::json!(3)),
                ("keyword_search".to_string(), serde_json::json!(0)),
            ],
            vec!["prefixed_name".to_string()],
            OnConflict::SetColumns(vec!["model".to_string()]),
        )
        .await
        .expect("registry row");
    }

    async fn registry(ctx: &TestContext) -> Vec<(String, String)> {
        db_read::list_bounded_sorted(
            ctx,
            REGISTRY_TABLE,
            Vec::new(),
            vec![SortField {
                field: "prefixed_name".to_string(),
                desc: false,
            }],
            Bound::Curated("a handful of rows seeded by the test"),
        )
        .await
        .expect("registry")
        .iter()
        .map(|r| {
            (
                r.str_field("prefixed_name").to_string(),
                r.str_field("model").to_string(),
            )
        })
        .collect()
    }

    async fn nearest(ctx: &TestContext, index: &str, v: [f32; 3]) -> Vec<String> {
        vclient::query(ctx, index, v.to_vec(), 2, None, SearchMode::Vector, None)
            .await
            .expect("query")
            .into_iter()
            .map(|m| m.id)
            .collect()
    }

    /// An index stored under a mixed-case name is moved to its lowercase
    /// name with the entries it held, its registry row follows, and the
    /// index is then usable under that name: its old entries answer a
    /// query, a new entry can be written. A second start changes nothing.
    #[tokio::test]
    async fn a_mixed_case_index_answers_under_its_lowercase_name_with_its_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = legacy_index_file(&dir);
        let ctx = ctx_over(&path).await;
        register(&ctx, LEGACY, "bge-m3").await;

        rename_legacy_indexes(&ctx).await.expect("startup step");

        assert_eq!(
            registry(&ctx).await,
            vec![(LOWERCASE.to_string(), "bge-m3".to_string())]
        );
        assert_eq!(nearest(&ctx, LOWERCASE, [0.0, 1.0, 0.0]).await[0], "b");
        vclient::upsert(
            &ctx,
            LOWERCASE,
            vec![VectorEntry {
                id: "c".to_string(),
                vector: vec![0.0, 0.0, 1.0],
                metadata: None,
                text: None,
            }],
        )
        .await
        .expect("a new entry");
        assert_eq!(vclient::count(&ctx, LOWERCASE).await.expect("count"), 3);
        assert_eq!(nearest(&ctx, LOWERCASE, [1.0, 0.0, 0.0]).await[0], "a");

        rename_legacy_indexes(&ctx).await.expect("a second start");
        assert_eq!(
            registry(&ctx).await,
            vec![(LOWERCASE.to_string(), "bge-m3".to_string())]
        );
        assert_eq!(vclient::count(&ctx, LOWERCASE).await.expect("count"), 3);
    }

    /// A start that moved the index and stopped before renaming its registry
    /// row finishes the job: the backend reports no index under the legacy
    /// name, and the lowercase index exists.
    #[tokio::test]
    async fn a_move_interrupted_before_the_registry_row_is_finished() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = legacy_index_file(&dir);
        let ctx = ctx_over(&path).await;
        vclient::rename_index(&ctx, LEGACY, LOWERCASE)
            .await
            .expect("the backend half of the move");
        register(&ctx, LEGACY, "bge-m3").await;

        rename_legacy_indexes(&ctx).await.expect("startup step");

        assert_eq!(
            registry(&ctx).await,
            vec![(LOWERCASE.to_string(), "bge-m3".to_string())]
        );
        assert_eq!(vclient::count(&ctx, LOWERCASE).await.expect("count"), 2);
    }

    /// Two registry rows that differ only by case are never merged: the
    /// legacy row, its index and the lowercase row are all left as they were.
    #[tokio::test]
    async fn two_indexes_differing_only_by_case_are_left_for_the_operator() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = legacy_index_file(&dir);
        let ctx = ctx_over(&path).await;
        register(&ctx, LEGACY, "legacy-model").await;
        register(&ctx, LOWERCASE, "lowercase-model").await;

        assert_eq!(
            move_legacy_index(&ctx, LEGACY, LOWERCASE, true)
                .await
                .expect("no fault"),
            Moved::Twin
        );
        rename_legacy_indexes(&ctx).await.expect("startup step");

        assert_eq!(
            registry(&ctx).await,
            vec![
                (LEGACY.to_string(), "legacy-model".to_string()),
                (LOWERCASE.to_string(), "lowercase-model".to_string()),
            ]
        );
        let still_legacy = db::list_all(
            &ctx,
            REGISTRY_TABLE,
            vec![Filter {
                field: "prefixed_name".to_string(),
                operator: FilterOp::Equal,
                value: serde_json::json!(LEGACY),
            }],
        )
        .await
        .expect("read");
        assert_eq!(still_legacy.len(), 1);
        // The legacy tables were not touched: the backend still finds the
        // index under exactly that spelling.
        vclient::rename_index(&ctx, LEGACY, LOWERCASE)
            .await
            .expect("the legacy index is still there to move");
    }
}
