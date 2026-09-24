//! Vector block migrations. Delegated to `crate::migration_helper`.
//!
//! Backend selection mirrors `files/migrations/mod.rs`: read
//! `WAFER_RUN_SHARED__DATABASE__BACKEND` from the config snapshot, fall back
//! to `sqlite` when the config block is not registered. The actual apply +
//! gating + statement splitting lives in
//! [`crate::migration_helper::apply_if_blessed`].
//!
//! Scope: only the static `impresspress__vector__registry` catalog and its
//! rows. Per-index
//! storage tables (`{prefixed}_meta`, `{prefixed}_fts`, vec0 virtual) are
//! materialized on demand by the upstream `wafer-run/vector` runtime block
//! via `vclient::create_index` — their names are user-supplied at runtime
//! and so cannot be expressed as a static SQL migration. See the SQL file
//! header for the long-form rationale.

const SQL_001_SQLITE: &str = include_str!("001_vector_schema.sqlite.sql");
#[cfg(feature = "postgres")]
const SQL_001_POSTGRES: &str = include_str!("001_vector_schema.postgres.sql");
const SQL_002_SQLITE: &str = include_str!("002_lowercase_index_names.sqlite.sql");
#[cfg(feature = "postgres")]
const SQL_002_POSTGRES: &str = include_str!("002_lowercase_index_names.postgres.sql");

/// Ordered SQLite migration scripts for this block, as `(basename, content)`
/// pairs. Feeds the runtime `lifecycle_init` apply path.
pub(crate) const SQLITE_MIGRATIONS: &[(&str, &str)] = &[
    ("001_vector_schema", SQL_001_SQLITE),
    ("002_lowercase_index_names", SQL_002_SQLITE),
];

/// Ordered PostgreSQL migration scripts, matching [`SQLITE_MIGRATIONS`]. Empty
/// when the `postgres` feature is off — see `files::migrations`'s doc for the
/// rationale (Cloudflare/D1 never selects postgres; don't embed dead SQL).
#[cfg(feature = "postgres")]
pub(crate) const POSTGRES_MIGRATIONS: &[&str] = &[SQL_001_POSTGRES, SQL_002_POSTGRES];
#[cfg(not(feature = "postgres"))]
pub(crate) const POSTGRES_MIGRATIONS: &[&str] = &[];

#[cfg(test)]
mod tests {
    #[cfg(feature = "postgres")]
    use super::SQL_001_POSTGRES;
    use super::SQL_001_SQLITE;

    /// The migration_helper statement splitter splits on bare `;` outside
    /// `--` line comments. Make sure every embedded statement parses into
    /// at least the table count we expect — protects against a stray
    /// `;` inside a comment / string literal silently dropping DDL.
    fn count_create_table(sql: &str) -> usize {
        sql.match_indices("CREATE TABLE IF NOT EXISTS ").count()
    }
    fn count_create_index(sql: &str) -> usize {
        sql.match_indices("CREATE INDEX IF NOT EXISTS ").count()
    }

    #[test]
    fn sqlite_script_has_expected_tables_and_indexes() {
        // 1 table: registry
        assert_eq!(count_create_table(SQL_001_SQLITE), 1);
        // 1 index: model lookup (PK already covers prefixed_name)
        assert_eq!(count_create_index(SQL_001_SQLITE), 1);
        assert!(SQL_001_SQLITE.contains("impresspress__vector__registry"));
        assert!(SQL_001_SQLITE.contains("idx_vector_registry_model"));
    }

    #[test]
    #[cfg(feature = "postgres")]
    fn postgres_script_has_expected_tables_and_indexes() {
        assert_eq!(count_create_table(SQL_001_POSTGRES), 1);
        assert_eq!(count_create_index(SQL_001_POSTGRES), 1);
        assert!(SQL_001_POSTGRES.contains("impresspress__vector__registry"));
    }

    /// Registry rows spelled with uppercase fold to the lowercase name the
    /// database layer admits, one row per folded name: an existing lowercase
    /// row wins, else the lowest-sorting spelling, and no other row changes.
    #[tokio::test]
    async fn migration_002_folds_registry_names_to_lowercase() {
        use crate::{db_read, test_support::TestContext};

        const REGISTRY: &str = "impresspress__vector__registry";
        let mut ctx = TestContext::with_admin().await;
        crate::migration_helper::apply_migrations(
            &ctx,
            "impresspress/vector",
            &[SQL_001_SQLITE],
            &[],
        )
        .await
        .expect("001 applies");
        for (name, model) in [
            ("impresspress__vector__Docs", "upper"),
            ("impresspress__vector__DOCS", "upper-2"),
            ("impresspress__vector__Notes", "upper"),
            ("impresspress__vector__notes", "lower"),
            ("impresspress__vector__plain", "lower"),
        ] {
            wafer_core::clients::database::upsert(
                &ctx,
                REGISTRY,
                vec![
                    ("prefixed_name".to_string(), serde_json::json!(name)),
                    ("model".to_string(), serde_json::json!(model)),
                ],
                vec!["prefixed_name".to_string()],
                wafer_block::wire::database::OnConflict::SetColumns(vec!["model".to_string()]),
            )
            .await
            .expect("seed a registry row");
        }

        ctx.set_config(crate::migration_helper::RUN_MIGRATIONS_KEY, "1");
        let sqlite: Vec<&str> = super::SQLITE_MIGRATIONS
            .iter()
            .map(|(_, sql)| *sql)
            .collect();
        crate::migration_helper::apply_migrations(&ctx, "impresspress/vector", &sqlite, &[])
            .await
            .expect("002 applies");

        let mut rows: Vec<(String, String)> = db_read::list_bounded_sorted(
            &ctx,
            REGISTRY,
            Vec::new(),
            vec![wafer_block::db::SortField {
                field: "prefixed_name".to_string(),
                desc: false,
            }],
            db_read::Bound::Curated("five rows seeded above"),
        )
        .await
        .expect("read the registry")
        .into_iter()
        .map(|r| {
            let field = |k: &str| {
                r.data
                    .get(k)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            (field("prefixed_name"), field("model"))
        })
        .collect();
        rows.sort();
        assert_eq!(
            rows,
            vec![
                (
                    "impresspress__vector__docs".to_string(),
                    "upper-2".to_string()
                ),
                (
                    "impresspress__vector__notes".to_string(),
                    "lower".to_string()
                ),
                (
                    "impresspress__vector__plain".to_string(),
                    "lower".to_string()
                ),
            ]
        );
    }
}
