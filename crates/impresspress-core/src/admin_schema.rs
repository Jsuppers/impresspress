//! Shared table-name constants for the `impresspress/admin` block.
//!
//! These live outside `blocks/admin/` (mirroring [`crate::messages_schema`])
//! so that consumers which read admin-owned rows by table name without
//! depending on the admin block module — today the config-snapshot cache
//! (`cache_key.rs`), the request pipeline (`pipeline.rs`), and the shared
//! migration runner (`migration_helper.rs`) — can reference them as a single
//! source of truth.
//!
//! `blocks/admin` re-exports from here (`logs.rs`), so existing
//! `blocks::admin::REQUEST_LOGS_TABLE` references continue to resolve. The
//! variables and block_settings tables already moved to
//! `crate::platform_state`; the request log follows.
//!
//! Why a sibling of `blocks/`?
//! - The constants describe the on-disk schema contract, not block logic, and
//!   `migration_helper.rs` previously open-coded a *duplicate* literal here
//!   with a bogus "avoid a circular dep on `crate::blocks::admin`" comment
//!   (Rust modules in one crate cannot have circular import problems). A leaf
//!   module removes the temptation to re-hardcode the literal.
//! - WRAP grants are still declared by `AdminBlock::info()` (the schema-owning
//!   block); other modules read rows via runtime grants, not by re-declaring
//!   ownership.

/// HTTP request log entries (one row per inbound request). Owned by the admin
/// block.
pub const REQUEST_LOGS_TABLE: &str = "impresspress__admin__request_logs";
