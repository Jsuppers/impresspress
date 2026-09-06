//! Row-level data access for the `wafer-run/auth` block.
//!
//! Each submodule exposes pure-function async helpers that take
//! `&dyn wafer_run::context::Context` and operate on a single table defined
//! by migration 001. Errors are the database client's own
//! [`wafer_run::WaferError`], carried through unchanged apart from a label:
//! [`db_failed`] prefixes the statement that failed, and [`internal_error`]
//! is this layer's own fault (a row whose columns are not the shape its type
//! describes).
//!
//! There used to be a `RepoError` here — `NotFound | Db(String)` — and the
//! `Db(String)` arm threw the [`wafer_run::ErrorCode`] away. That is the one
//! thing a caller needs: a WRAP grant refusal is `PermissionDenied` and must
//! reach the client as a 403, a missing row is `NotFound` and must reach it
//! as a 404, and everything else is a 500. Collapsed into a string, all
//! three arrived at `admin::users::get_user` as
//! `500 Internal server error (ref: …)`, so an operator running a
//! deployment whose admin block was missing its `wafer_run__auth__users`
//! grant could not tell the missing grant from an outage. Keeping the code
//! is what lets those call sites answer through `crud::db_error`.
//!
//! The small row-decoding utilities every submodule needs — the ISO-8601
//! timestamp writer ([`now_iso`]), hex decoding ([`decode_hex`]), and the
//! `&HashMap<String, Value>` map accessors ([`map_str`]/[`map_opt_str`]/
//! [`map_bool`]) — live here so all auth tables share one implementation. In
//! particular [`now_iso`] is **the** timestamp writer for auth-table rows:
//! keeping a single `…Z` formatter stops the documented `Z`/`+00:00`
//! intermixing (see `service::is_expired`) from growing.

use std::collections::HashMap;

use serde_json::Value;
use wafer_run::{ErrorCode, WaferError};

pub mod api_keys;
pub mod bootstrap_tokens;
pub mod jwt_blocklist;
pub mod local_credentials;
pub mod maintenance;
pub mod oauth_pkce;
pub mod orgs;
pub mod pats;
pub mod provider_links;
pub mod rate_limits;
pub mod sessions;
pub mod tokens;
pub mod users;

/// Label a failed database call with the statement that failed, keeping the
/// [`wafer_run::ErrorCode`] the client classified it with.
///
/// `what` is the same short label the deleted `RepoError::Db(format!("{what}:
/// {e}"))` carried, and it stays in the message because it is the only thing
/// naming which statement of a multi-step repo function gave up. The code is
/// what that spelling destroyed: `crud::db_error` needs it to answer 404 for
/// a missing row, **403 for a WRAP refusal** and 500 for anything else, and
/// a `String` makes all three identical.
pub(crate) fn db_failed(what: &str, error: WaferError) -> WaferError {
    WaferError {
        code: error.code,
        message: format!("{what}: {}", error.message),
        meta: error.meta,
    }
}

/// A fault this layer found in a successful read: a row whose columns are
/// not the shape the module's row type describes.
///
/// Always [`ErrorCode::Internal`], never the caller's fault — the row came
/// back, the table is simply not the one migration 001 describes, which is a
/// deployment or migration fault. Separate from [`db_failed`] because there
/// is no client error to carry a code from.
pub(crate) fn internal_error(what: impl Into<String>) -> WaferError {
    WaferError::new(ErrorCode::Internal, what)
}

/// Current UTC time as an ISO-8601 string with a literal `Z` suffix
/// (`%Y-%m-%dT%H:%M:%SZ`).
///
/// This is the single timestamp writer for every auth table. Using one
/// formatter everywhere keeps stored timestamps in one format so the
/// string-comparison cleanup queries (e.g. `sessions::delete_expired`'s
/// `expires_at < cutoff`) stay correct, and stops the historical
/// `Z`-vs-`+00:00` intermixing documented in `service::is_expired`.
pub(crate) fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Decode a lowercase hex string into raw bytes. Returns `None` for an
/// odd-length or non-hex input. Used by the token-hash columns
/// (`sessions`, `pats`) which persist `hex_encode(sha256(raw))`.
pub(crate) fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Map accessor: owned `String` for a TEXT column, or `None` when the key is
/// absent / not a JSON string. Mirrors `RecordExt::str_field`'s "absent → empty"
/// intent but preserves the `Option` so callers can distinguish missing.
pub(crate) fn map_opt_str(m: &HashMap<String, Value>, key: &str) -> Option<String> {
    m.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Map accessor: owned `String` for a TEXT column, defaulting to empty.
pub(crate) fn map_str(m: &HashMap<String, Value>, key: &str) -> String {
    map_opt_str(m, key).unwrap_or_default()
}

/// Map accessor: bool for a column, tolerant of the shapes the different
/// backends return (JSON bool, SQLite TEXT-int `0`/`1`, Postgres BOOLEAN,
/// string `'true'`/`'false'`). Mirrors `RecordExt::bool_field`.
pub(crate) fn map_bool(m: &HashMap<String, Value>, key: &str) -> bool {
    match m.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        Some(Value::String(s)) => s == "1" || s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}
