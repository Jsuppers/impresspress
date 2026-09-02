//! Response types for the `/b/storage/api/...` JSON surface that the
//! `.output::<T>()` derive migration can actually reach.
//!
//! [`ObjectInfoResponse`] / [`ObjectListResponse`] are a field-for-field
//! mirror of `wafer_core::clients::storage::{ObjectInfo, ObjectList}` — the
//! real wire types [`super::storage::objects::handle_list_objects`]
//! populates from `store::list`. Those types live in wafer-run
//! (`wafer_block::wire::storage`) and don't derive `schemars::JsonSchema`,
//! and this migration is scoped to impresspress-core only, so there is no
//! `T` in reach that both IS the handler's real output type and derives the
//! schema trait.
//!
//! This is not a second, independent description of the contract: the
//! handler builds one of these from the real `ObjectList` and serializes
//! *that*, so the type this schema is derived from is the type that goes out
//! on the wire. `last_modified` keeps `chrono::DateTime<Utc>` rather than a
//! pre-formatted `String` for the same reason — the wire type carries no
//! `#[serde(with = ...)]` override, so chrono's own `Serialize` impl runs
//! either way and the bytes are identical. Under schemars' `chrono04`
//! feature that field renders as `{"type": "string", "format": "date-time"}`,
//! matching the previous hand-written schema.

use serde::{Deserialize, Serialize};

/// Mirrors `wafer_core::clients::storage::ObjectInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ObjectInfoResponse {
    /// Object key.
    pub key: String,
    /// Size in bytes.
    pub size: i64,
    pub content_type: String,
    pub last_modified: chrono::DateTime<chrono::Utc>,
}

/// `GET /b/storage/api/buckets/{name}/objects` response body. Mirrors
/// `wafer_core::clients::storage::ObjectList` minus `next_cursor`:
/// [`super::storage::objects::handle_list_objects`] always calls
/// `store::list` with `cursor: None` (offset-only paging), and every
/// backend (S3, local-storage, Cloudflare R2) returns `next_cursor: None`
/// in offset mode unconditionally — so on this endpoint the field can never
/// be anything but absent. Carrying it here would describe a cursor-paging
/// capability this endpoint does not actually expose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ObjectListResponse {
    /// Objects in this page.
    pub objects: Vec<ObjectInfoResponse>,
    /// Total number of objects matching the filter (across all pages). See
    /// `ObjectList::total_count` for the lower-bound caveat on some backends.
    pub total_count: i64,
}
