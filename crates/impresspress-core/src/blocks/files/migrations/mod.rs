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
