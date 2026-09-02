//! Persistence for the dev sandbox control plane.
//!
//! One module per table; each owns its own `pub const TABLE` (the repo-wide
//! convention — see `auth/repo/users.rs`). Every query goes through the typed
//! `wafer_core::clients::database` client, so the schema stays swappable
//! between SQLite/D1 and Postgres.

pub mod builds;
pub mod generations;
pub mod runtime_state;

/// A fresh row id.
///
/// `new_v4` rather than `now_v7`: generation and build ids are quoted back to
/// the agent and embedded in export bundles, and a v7 id leaks the wall-clock
/// time of every workspace edit into those artifacts. Ordering comes from
/// `created_at`, which the rows carry explicitly.
pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The current time, RFC 3339, as every other block stamps it.
pub(crate) fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// A JSON-encoded `TEXT` column read back as text on every backend.
///
/// The SQLite service sniffs JSON-shaped text in `row_to_record` and hands
/// back an already-decoded value, while Postgres and D1 return the literal
/// string. `RecordExt::str_field` would collapse the SQLite case to `""` —
/// silently losing a whole manifest, `BlockInfo` or diagnostics list — so
/// re-encode whatever came back instead of assuming one backend's shape.
pub(crate) fn json_text(record: &wafer_core::clients::database::Record, key: &str) -> String {
    match record.data.get(key) {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}
