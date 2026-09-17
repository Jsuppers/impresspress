//! Files block migrations. Applied from the block's `Init` lifecycle via
//! [`crate::migration_helper::lifecycle_init`].

const SQL_001_SQLITE: &str = include_str!("001_initial_schema.sqlite.sql");
#[cfg(feature = "postgres")]
const SQL_001_POSTGRES: &str = include_str!("001_initial_schema.postgres.sql");
// 002 makes `buckets.name` unique. The name is the blob-namespace folder name
// in `wafer-run/storage`, so two rows for one name are two owners of one
// folder; the index is what makes the metadata insert the atomic claim
// `storage::buckets::handle_create_bucket` answers 409 on. The file's own
// header carries the rest, including why pre-existing duplicates are resolved
// in favour of the earliest creator.
const SQL_002_SQLITE: &str = include_str!("002_bucket_name_unique.sqlite.sql");
#[cfg(feature = "postgres")]
const SQL_002_POSTGRES: &str = include_str!("002_bucket_name_unique.postgres.sql");

/// Ordered SQLite migration scripts for this block, as `(basename, content)`
/// pairs. Feeds the runtime `lifecycle_init` apply path.
/// Order here is the apply order.
pub(crate) const SQLITE_MIGRATIONS: &[(&str, &str)] = &[
    ("001_initial_schema", SQL_001_SQLITE),
    ("002_bucket_name_unique", SQL_002_SQLITE),
];

/// Ordered PostgreSQL migration scripts, matching [`SQLITE_MIGRATIONS`]. Empty
/// when the `postgres` feature is off — e.g. Cloudflare/D1 never selects the
/// postgres dialect at runtime, so keeping the `.postgres.sql` files out of
/// that build entirely (rather than embedding-then-ignoring them) drops dead
/// SQL bytes from the wasm binary.
#[cfg(feature = "postgres")]
pub(crate) const POSTGRES_MIGRATIONS: &[&str] = &[SQL_001_POSTGRES, SQL_002_POSTGRES];
#[cfg(not(feature = "postgres"))]
pub(crate) const POSTGRES_MIGRATIONS: &[&str] = &[];

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{blocks::files::repo, test_support::TestContext};

    /// Migration 002's repair half, on the only database that can need it:
    /// one that already holds two rows for one bucket name.
    ///
    /// Every other fixture applies 001 and 002 together against an empty
    /// table, so the `DELETE` never has a duplicate to find and a SQL error
    /// in it would go unnoticed until a real deployment tried to upgrade —
    /// where it fails the whole batch, leaves the index uncreated, and
    /// re-fails on every later boot (`apply_if_blessed` tolerates only a
    /// duplicate `ALTER … ADD COLUMN`).
    ///
    /// So this applies 001 alone, plants the takeover the index exists to
    /// stop, and then applies the real migration list the way an operator
    /// upgrading with `--run-migrations` does.
    #[tokio::test]
    async fn migration_002_repairs_a_database_that_already_holds_a_duplicate_name() {
        let mut ctx = TestContext::with_auth().await;
        crate::migration_helper::apply_migrations(
            &ctx,
            "impresspress/files",
            &[SQL_001_SQLITE],
            &[],
        )
        .await
        .expect("001 applies");

        // Alice's bucket, then mallory's row for the same folder — accepted
        // before the index existed, and what an upgrading deployment holds.
        for (owner, created_at) in [
            ("alice", "2026-01-01T00:00:00Z"),
            ("mallory", "2026-06-01T00:00:00Z"),
        ] {
            repo::buckets::seed(
                &ctx,
                crate::util::json_map(json!({
                    "name": "assets",
                    "public": false,
                    "created_by": owner,
                    "created_at": created_at,
                })),
            )
            .await
            .expect("seed the duplicate");
        }

        ctx.set_config(crate::migration_helper::RUN_MIGRATIONS_KEY, "1");
        let sqlite: Vec<&str> = SQLITE_MIGRATIONS.iter().map(|(_, sql)| *sql).collect();
        crate::migration_helper::apply_migrations(&ctx, "impresspress/files", &sqlite, &[])
            .await
            .expect("002 applies to a database holding a duplicate");

        assert!(
            repo::buckets::find_owned(&ctx, "assets", "alice")
                .await
                .expect("bucket lookup")
                .is_some(),
            "the earliest creator keeps the bucket",
        );
        assert!(
            repo::buckets::find_owned(&ctx, "assets", "mallory")
                .await
                .expect("bucket lookup")
                .is_none(),
            "the later claim on someone else's folder is removed",
        );
        assert!(
            repo::buckets::insert(&ctx, "assets", false, "mallory")
                .await
                .is_err(),
            "and the index is in place, so it cannot be made again",
        );
    }
}
