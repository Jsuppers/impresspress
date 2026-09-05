//! Platform state: the five `impresspress__admin__*` tables the runtime
//! itself reads and writes — configuration variables, per-block settings,
//! admin-created WRAP grants, the request log and role grants.
//!
//! Each submodule owns one table: its `TABLE` name, its row struct, one
//! `from_record` and one `to_data`, so a column name is spelled in exactly
//! one Rust file. Every other module — the boot orchestrators, the request
//! pipeline, the admin block's pages, the framework auth block, the dev
//! sandbox, the platform adapters and the CLI — reaches the table through
//! the functions here (spec `2026-09-06-ownership-and-repo-boundaries`,
//! section 2.1).
//!
//! Two callers, one codec: boot runs before WRAP over
//! `Arc<dyn DatabaseService>` (native seeds pre-wafer, Cloudflare and the
//! browser seed after admin's `Init`), blocks and the pipeline run under
//! WRAP over `&dyn Context`. Where the spec names both flavours a module
//! offers both, and both go through the same row codec.
//!
//! The tables are still created by the admin block's migrations —
//! `blocks/admin/migrations/001_admin_schema.{sqlite,postgres}.sql` (all
//! five), `002_variables_block_column` (`variables.block`) and
//! `003_block_settings_seed_hash` (`block_settings.seed_defaults_hash`) —
//! applied as one hash-gated unit by admin's `Init`. Moving the DDL would
//! change the concatenated migration bytes every deployment has blessed, so
//! the schema keeps living there (spec decision 5.4); `blocks/admin` also
//! keeps `ADMIN_BLOCK_ID` and the `collections(..)`/`grants(..)`
//! declarations, because the `impresspress__admin__` prefix makes it the
//! WRAP schema owner.

pub mod block_settings;
pub mod variables;
pub mod wrap_grants;
