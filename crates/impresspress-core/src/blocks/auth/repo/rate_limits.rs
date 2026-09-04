//! Table declaration for `wafer_run__auth__rate_limits`.
//!
//! Sliding-window counters keyed by user/IP. Row-level helpers live
//! alongside the caller in `blocks/rate_limit.rs`, which only references
//! this table on `wasm32` (the native code path uses the in-memory
//! `UserRateLimiter`). The const is platform-agnostic so we keep it
//! always-defined and silence the dead-code warning here.
//!
//! `pub`, matching every other table constant in this module (`repo/mod.rs`'s
//! "one module per table" convention) — `tests/dev_data_snapshot.rs` names
//! this table the same way it names every other `auth::repo::*::TABLE`, and
//! an integration test crate can only reach a `pub` item.
#[allow(dead_code)]
pub const TABLE: &str = "wafer_run__auth__rate_limits";
