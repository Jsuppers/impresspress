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

use super::repo::Page;

/// One record in the [`RecordListView`] envelope: the row's `id` beside the
/// row's columns, exactly as `wafer_core::clients::database::Record`
/// serializes. The backends put `id` in BOTH places (`row_to_record` inserts
/// every column into `data` and copies `id` out to the envelope), and
/// `packages/impresspress-js` reads the envelope one
/// (`flattenRecordList`: `{ id: r.id, ...r.data }`), so both are published.
#[derive(Debug, Clone, Serialize)]
pub struct RecordView {
    pub id: String,
    pub data: serde_json::Map<String, serde_json::Value>,
}

impl RecordView {
    /// Build the record envelope for one typed row.
    ///
    /// The row serializes to its columns — every row type mirrors its table
    /// column-for-column precisely so this cannot drop one — and `id` is
    /// lifted out to the envelope while staying in `data`.
    pub fn from_row<T: Serialize>(row: &T) -> Self {
        let data = match serde_json::to_value(row) {
            Ok(serde_json::Value::Object(map)) => map,
            // Unreachable: every row type is a plain struct of scalars. A
            // non-object would mean a row grew a serde attribute that
            // changes its shape, which the round-trip test below catches.
            _ => serde_json::Map::new(),
        };
        let id = data
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        Self { id, data }
    }
}

/// The `RecordList` envelope the block's JSON list endpoints publish:
/// `{ records, total_count, page, page_size }`.
///
/// This is a *published contract*, not an implementation detail. The repo
/// returns [`Page`], which is not `Serialize`; this view is the single place
/// a page becomes a response body, so the envelope cannot drift one endpoint
/// at a time. `packages/impresspress-js/src/services/storage.service.ts`
/// declares the matching `RecordListWire<T>` and names
/// `/b/storage/api/search` and `/b/storage/api/recent` in its doc comment;
/// that SDK has its own CI job and is the reason this shape is preserved
/// rather than modernised here. Changing it is a deliberate, separate
/// change that moves the SDK in lockstep — see the follow-up in the PR that
/// introduced this type.
#[derive(Debug, Clone, Serialize)]
pub struct RecordListView {
    pub records: Vec<RecordView>,
    pub total_count: i64,
    pub page: i64,
    pub page_size: i64,
}

impl RecordListView {
    /// Build the envelope from a repo page of typed rows.
    pub fn from_page<T: Serialize>(page: &Page<T>) -> Self {
        Self {
            records: page.rows.iter().map(RecordView::from_row).collect(),
            total_count: page.total,
            page: page.page,
            page_size: page.page_size,
        }
    }
}

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

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wafer_core::clients::database::Record;

    use super::*;
    use crate::blocks::files::repo;

    fn object_page() -> Page<repo::objects::ObjectRow> {
        Page {
            rows: vec![repo::objects::ObjectRow::from_record(&Record {
                id: "o1".to_string(),
                data: [
                    ("id", json!("o1")),
                    ("bucket", json!("photos")),
                    ("key", json!("nested/a.png")),
                    ("size", json!(1024)),
                    ("content_type", json!("image/png")),
                    ("status", json!("complete")),
                    ("uploaded_by", json!("alice")),
                    ("uploaded_at", json!("2026-05-06T10:00:00Z")),
                    ("created_at", json!("2026-05-06T10:00:00Z")),
                    ("updated_at", json!("2026-05-06T10:00:01Z")),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            })],
            total: 7,
            page: 2,
            page_size: 20,
        }
    }

    /// The envelope IS a published contract, and this is the test that says
    /// so. `packages/impresspress-js/src/services/storage.service.ts`
    /// declares `RecordListWire<T>` as `{ records: Array<{ id, data }>,
    /// total_count, page, page_size }` and flattens it with
    /// `{ id: r.id, ...r.data }`, naming `/b/storage/api/search` and
    /// `/b/storage/api/recent`. Every key asserted below is one that SDK
    /// reads; the SDK has its own CI job, so a Rust-only refactor that
    /// reshaped this would break it silently.
    #[test]
    fn the_envelope_matches_the_sdk_s_record_list_wire() {
        let body = serde_json::to_value(RecordListView::from_page(&object_page()))
            .expect("the view serializes");

        assert_eq!(body["total_count"], json!(7));
        assert_eq!(body["page"], json!(2));
        assert_eq!(body["page_size"], json!(20));

        let records = body["records"].as_array().expect("records is an array");
        assert_eq!(records.len(), 1);
        // `flattenRecordList` takes `id` from the envelope, not from `data`.
        assert_eq!(records[0]["id"], json!("o1"));
        // ...and spreads `data` over it, so the columns live one level down.
        assert_eq!(records[0]["data"]["key"], json!("nested/a.png"));
        assert_eq!(records[0]["data"]["size"], json!(1024));
    }

    /// The backends put `id` in `data` as well as in the envelope
    /// (`row_to_record` inserts every column and copies `id` out), so the
    /// view does too — otherwise a consumer reading `data.id` would start
    /// seeing `undefined`.
    #[test]
    fn the_record_view_publishes_id_in_both_places() {
        let body =
            serde_json::to_value(RecordListView::from_page(&object_page())).expect("serializes");
        assert_eq!(body["records"][0]["id"], json!("o1"));
        assert_eq!(body["records"][0]["data"]["id"], json!("o1"));
    }

    /// Every column of the objects table reaches the wire. The row type is
    /// what the view serializes, so a column missing from the row is a
    /// column missing from the response — this is the assertion that makes
    /// "the row mirrors the table" load-bearing rather than aspirational.
    #[test]
    fn the_object_row_publishes_every_column_of_its_table() {
        let body =
            serde_json::to_value(RecordListView::from_page(&object_page())).expect("serializes");
        let data = body["records"][0]["data"]
            .as_object()
            .expect("data is an object")
            .clone();
        let mut keys: Vec<&str> = data.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "bucket",
                "content_type",
                "created_at",
                "id",
                "key",
                "size",
                "status",
                "updated_at",
                "uploaded_at",
                "uploaded_by",
            ],
            "the columns `migrations/001_initial_schema.sqlite.sql` declares \
             for the objects table, plus its `id` primary key"
        );
    }

    /// The second SDK consumer of this envelope:
    /// `packages/impresspress-js/src/services/extensions.service.ts`
    /// `CloudStorageExtension.listShares` reads `GET /b/cloudstorage/shares`
    /// as `{ records: Array<{ id, data }>, total_count, page, page_size }`
    /// and flattens it into `ShareRecord`. Every field that interface names
    /// must be published.
    #[test]
    fn the_share_envelope_carries_every_field_the_sdk_s_share_record_names() {
        let row = repo::shares::ShareRow::from_record(&Record {
            id: "s1".to_string(),
            data: [
                ("id", json!("s1")),
                ("token", json!("tok")),
                ("bucket", json!("photos")),
                ("key", json!("a.png")),
                ("created_by", json!("alice")),
                ("created_at", json!("2026-05-06T10:00:00Z")),
                ("access_count", json!(4)),
                ("expires_at", json!("2026-06-06T10:00:00Z")),
                ("max_access_count", json!(10)),
                ("updated_at", json!("2026-05-06T10:00:01Z")),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        });
        let body = serde_json::to_value(RecordView::from_row(&row)).expect("serializes");

        assert_eq!(body["id"], json!("s1"));
        for field in [
            "id",
            "token",
            "bucket",
            "key",
            "created_by",
            "created_at",
            "access_count",
            "expires_at",
            "max_access_count",
        ] {
            assert!(
                body["data"].get(field).is_some(),
                "`ShareRecord.{field}` missing from the published share row: {body}"
            );
        }
    }

    /// `QuotaRow` groups its four cap columns behind a `QuotaConfig` for the
    /// enforcement path, but they ARE four columns of one table, so the wire
    /// stays flat — `#[serde(flatten)]`. A `config` key here would be a
    /// reshaped response body for `GET /b/cloudstorage/admin/quotas` and
    /// `PATCH /b/cloudstorage/admin/quotas/{id}`.
    #[test]
    fn the_quota_row_publishes_its_caps_flat_not_nested() {
        let row = repo::quota::QuotaRow::from_record(&Record {
            id: "q1".to_string(),
            data: [
                ("id", json!("q1")),
                ("user_id", json!("u-9")),
                ("max_storage_bytes", json!(2048)),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        });
        let body = serde_json::to_value(RecordView::from_row(&row)).expect("serializes");

        assert_eq!(body["id"], json!("q1"));
        assert_eq!(body["data"]["user_id"], json!("u-9"));
        assert_eq!(body["data"]["max_storage_bytes"], json!(2048));
        assert!(
            body["data"].get("config").is_none(),
            "the caps are columns, not a nested object: {body}"
        );
    }
}
