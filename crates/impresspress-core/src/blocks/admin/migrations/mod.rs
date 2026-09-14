//! Admin block migrations. Applied from the block's `Init` lifecycle via
//! [`crate::migration_helper::lifecycle_init`].
//!
//! SQL files are embedded with `include_str!`. Backend dispatch + concat +
//! the `current_hash` / `blessed_hash` / `IMPRESSPRESS_RUN_MIGRATIONS` gate
//! all live in [`crate::migration_helper::apply_migrations`]. Earlier
//! versions of this module called `db::ddl` directly in a loop, bypassing
//! the gate and re-running every DDL on every cold isolate (~2,800 D1
//! queries/day on wafer.run — see the 2026-05-14 config-snapshot spec).

const SQL_001_SQLITE: &str = include_str!("001_admin_schema.sqlite.sql");
#[cfg(feature = "postgres")]
const SQL_001_POSTGRES: &str = include_str!("001_admin_schema.postgres.sql");
// 002's header comment — in BOTH dialect files — says the `block` column is
// "for indexed per-block lookup by D1ConfigSource". That was true when the
// migration shipped and is not any more: `D1ConfigSource` reads the whole
// variables table once and groups by `block` in memory, issuing no
// `WHERE block = ?` at all (see that module's doc). The column and the index
// this migration creates are still live — `block` is exactly what the
// in-memory grouping keys on — so only the query strategy moved on.
//
// The .sql files are deliberately NOT edited to say so. A shipped migration
// is hash-addressed over its whole text, comments included, so retouching a
// `--` line logs `schema drift` on every boot of every deployment that
// already applied it and needs a `--run-migrations` redeploy to clear. See
// `crate::migration_helper`'s "A shipped .sql file is immutable, comments
// included", which prescribes exactly this note.
const SQL_002_SQLITE: &str = include_str!("002_variables_block_column.sqlite.sql");
#[cfg(feature = "postgres")]
const SQL_002_POSTGRES: &str = include_str!("002_variables_block_column.postgres.sql");
const SQL_003_SQLITE: &str = include_str!("003_block_settings_seed_hash.sqlite.sql");
#[cfg(feature = "postgres")]
const SQL_003_POSTGRES: &str = include_str!("003_block_settings_seed_hash.postgres.sql");

/// Ordered SQLite migration scripts for this block, as `(basename, content)`
/// pairs. Feeds the runtime `lifecycle_init` apply path.
/// Order here is the apply order.
pub(crate) const SQLITE_MIGRATIONS: &[(&str, &str)] = &[
    ("001_admin_schema", SQL_001_SQLITE),
    ("002_variables_block_column", SQL_002_SQLITE),
    ("003_block_settings_seed_hash", SQL_003_SQLITE),
];

/// Ordered PostgreSQL migration scripts, matching [`SQLITE_MIGRATIONS`] one
/// for one. Selected at runtime by `apply_migrations` and reused by
/// [`ddl_files`] for the pre-wafer native CLI path. Empty when the `postgres`
/// feature is off — see `files::migrations`'s doc for the rationale
/// (Cloudflare/D1 never selects postgres; don't embed dead SQL).
#[cfg(feature = "postgres")]
pub(crate) const POSTGRES_MIGRATIONS: &[&str] =
    &[SQL_001_POSTGRES, SQL_002_POSTGRES, SQL_003_POSTGRES];
#[cfg(not(feature = "postgres"))]
pub(crate) const POSTGRES_MIGRATIONS: &[&str] = &[];

/// Apply the admin schema through the shared migration-state gate.
///
/// Production no longer calls this: `AdminBlock::lifecycle(Init)` applies the
/// admin schema via [`crate::migration_helper::lifecycle_init`]. This thin
/// forwarder exists for the `tests/admin/*` + `tests/auth/*` integration
/// suites, which bootstrap `impresspress__admin__block_settings` (the tracking
/// table every other block's gate upserts into) in their fixtures —
/// test-fixture setup is an explicit exception to the no-raw-migration-runner
/// rule (CLAUDE.md).
pub async fn apply(ctx: &dyn wafer_run::context::Context) -> Result<(), String> {
    let sqlite: Vec<&str> = SQLITE_MIGRATIONS.iter().map(|(_, sql)| *sql).collect();
    crate::migration_helper::apply_migrations(
        ctx,
        "impresspress/admin",
        &sqlite,
        POSTGRES_MIGRATIONS,
    )
    .await
}

/// The admin migration SQL files for the given `db_type`, in apply order —
/// the same constants the gated `lifecycle_init` runner feeds. `"postgres"`
/// (case-insensitive) selects the postgres dialect; everything else selects
/// SQLite, matching [`crate::migration_helper::db_backend`].
///
/// Exposed so the native CLI can create the admin tables *before* the wafer
/// exists (it seeds the JWT secret + block_settings pre-build), via
/// [`crate::migration_helper::apply_ddl_via_service`]. Cloudflare and browser
/// don't need this — their seeders run after the gated apply has already
/// created the tables at `init_block(admin)`.
pub fn ddl_files(db_type: &str) -> &'static [&'static str] {
    if db_type.eq_ignore_ascii_case("postgres") {
        POSTGRES_MIGRATIONS
    } else {
        &[SQL_001_SQLITE, SQL_002_SQLITE, SQL_003_SQLITE]
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "postgres")]
    use super::{SQL_001_POSTGRES, SQL_002_POSTGRES, SQL_003_POSTGRES};
    use super::{SQL_001_SQLITE, SQL_002_SQLITE, SQL_003_SQLITE};

    #[test]
    fn sqlite_migrations_contain_expected_ddl() {
        // 001 schema (variables UNIQUE INDEX)
        assert!(SQL_001_SQLITE.contains("impresspress__admin__variables_key_uniq"));
        // 002 follow-up (ALTER TABLE ADD COLUMN block + index)
        assert!(SQL_002_SQLITE.contains("ADD COLUMN block"));
        assert!(SQL_002_SQLITE.contains("impresspress__admin__variables_block_idx"));
        // 003 follow-up (ADD COLUMN seed_defaults_hash)
        assert!(SQL_003_SQLITE.contains("ADD COLUMN seed_defaults_hash"));
    }

    #[test]
    #[cfg(feature = "postgres")]
    fn postgres_migrations_contain_expected_ddl() {
        assert!(SQL_001_POSTGRES.contains("impresspress__admin__variables_key_uniq"));
        assert!(SQL_002_POSTGRES.contains("ADD COLUMN"));
        assert!(SQL_003_POSTGRES.contains("seed_defaults_hash"));
    }
}
