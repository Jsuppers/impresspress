//! Row-level access over `impresspress__files__objects`.
//!
//! Object metadata rows — one row per uploaded file (sibling of the raw
//! storage blob in `wafer-run/storage`). Tracks size, content type,
//! status, uploader and timestamps. Rows are inserted `pending` *before*
//! the storage upload (to close the quota TOCTOU window) and flipped to
//! `complete` afterward; quota accounting sums/counts by `uploaded_by`
//! (including in-flight `pending` reservations), while user-facing search
//! and admin stats only see `complete` rows.

use std::collections::HashMap;

use wafer_block::{
    db::{Filter, FilterOp, ListOptions, SortField},
    wire::database as wire,
};
use wafer_core::clients::database::{self as db, Record};
use wafer_run::{context::Context, WaferError};

use super::{super::contracts::ObjectStatus, Page};
use crate::util::{enum_column_or, RecordExt};

/// Object metadata table — one row per uploaded file (sibling of the raw
/// storage blob in `wafer-run/storage`). Tracks size, content type, status,
/// uploader and timestamps.
pub const TABLE: &str = "impresspress__files__objects";

/// One object-metadata row, decoded.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ObjectRow {
    pub id: String,
    /// Bucket name; `(bucket, key)` is unique.
    pub bucket: String,
    /// Object key within the bucket.
    pub key: String,
    /// Size in bytes. `i64_field` so a TEXT-stored number still counts
    /// toward the quota rather than reading as zero.
    pub size: i64,
    pub content_type: String,
    /// `Pending` while the storage upload is in flight, `Complete` after.
    /// Quota accounting counts both; user-facing search and admin stats see
    /// only `Complete`.
    pub status: ObjectStatus,
    pub uploaded_by: String,
    /// When the upload was reserved — the timestamp the object browser
    /// renders as "modified", and the one `delete_stale_pending` compares.
    pub uploaded_at: String,
    pub created_at: String,
    pub updated_at: String,
}

impl ObjectRow {
    /// The one decode of an object row.
    ///
    /// Fallible since `status` became a type: a row in neither `Pending` nor
    /// `Complete` is counted by neither the quota sum nor the listings, so
    /// it is reported naming the row rather than carried silently. An
    /// *empty* column reads as `Complete`, which is exactly what the
    /// column's own `DEFAULT 'complete'`
    /// (`migrations/001_initial_schema.sqlite.sql`) gives a row inserted
    /// without it; every production insert names the value.
    pub fn from_record(rec: &Record) -> Result<Self, WaferError> {
        Ok(Self {
            id: rec.id.clone(),
            bucket: rec.str_field("bucket").to_string(),
            key: rec.str_field("key").to_string(),
            size: rec.i64_field("size"),
            content_type: rec.str_field("content_type").to_string(),
            status: enum_column_or(rec, "status", ObjectStatus::Complete)?,
            uploaded_by: rec.str_field("uploaded_by").to_string(),
            uploaded_at: rec.str_field("uploaded_at").to_string(),
            created_at: rec.str_field("created_at").to_string(),
            updated_at: rec.str_field("updated_at").to_string(),
        })
    }
}

/// Filter matching only fully uploaded rows, excluding in-flight
/// [`ObjectStatus::Pending`] reservations.
fn complete_filter() -> [Filter; 1] {
    [status_is(ObjectStatus::Complete)]
}

/// An equality filter on the `status` column.
fn status_is(status: ObjectStatus) -> Filter {
    Filter {
        field: "status".to_string(),
        operator: FilterOp::Equal,
        value: serde_json::json!(status),
    }
}

/// Filter matching all objects uploaded by `user_id` (the rows that count
/// toward that user's quota, including in-flight `pending` reservations).
fn owned_objects_filter(user_id: &str) -> Vec<Filter> {
    vec![Filter {
        field: "uploaded_by".to_string(),
        operator: FilterOp::Equal,
        value: serde_json::Value::String(user_id.to_string()),
    }]
}

/// Escape SQL LIKE wildcards (`%`, `_`) and the escape char itself (`\`) in
/// user-supplied search terms so a user searching for `100% off` doesn't
/// also match arbitrary characters.
///
/// SQLite's `LIKE` has *no* default escape character — a bare backslash is
/// just a literal byte, so escaping here would be silently inert on its own.
/// What makes it effective is the `wafer-sql-utils` `FilterOp::Like` builder
/// (used by [`search_completed`]'s query below), which renders an explicit
/// `ESCAPE '\'` clause on every backend (SQLite/D1 and Postgres) — see
/// `wafer-sql-utils::query::leaf_expr`. Without that clause, a query
/// containing `_` or `%` would match as a wildcard instead of a literal
/// character.
fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out
}

/// Insert the `pending` reservation row written BEFORE the storage upload,
/// so concurrent quota checks see the in-flight size (closes the
/// check-quota → upload TOCTOU race). `uploaded_at` is stamped with
/// [`crate::util::now_rfc3339`].
pub async fn insert_pending(
    ctx: &dyn Context,
    bucket: &str,
    key: &str,
    size: usize,
    content_type: &str,
    uploaded_by: &str,
) -> Result<ObjectRow, WaferError> {
    let data = crate::util::json_map(serde_json::json!({
        "bucket": bucket,
        "key": key,
        "size": size,
        "content_type": content_type,
        "status": ObjectStatus::Pending,
        "uploaded_by": uploaded_by,
        "uploaded_at": crate::util::now_rfc3339(),
    }));
    ObjectRow::from_record(&db::create(ctx, TABLE, data).await?)
}

/// Flip a [`ObjectStatus::Pending`] row to [`ObjectStatus::Complete`] after
/// its storage upload succeeded.
pub async fn mark_complete(ctx: &dyn Context, id: &str) -> Result<(), WaferError> {
    let data = crate::util::json_map(serde_json::json!({ "status": ObjectStatus::Complete }));
    db::update(ctx, TABLE, id, data).await.map(|_| ())
}

/// Hard-delete one object row by id (the compensating delete when a
/// storage upload fails after its `pending` row was inserted).
pub async fn delete(ctx: &dyn Context, id: &str) -> Result<(), WaferError> {
    db::delete(ctx, TABLE, id).await
}

/// Delete every object row in `bucket` (bucket-deletion metadata cleanup).
pub async fn delete_for_bucket(ctx: &dyn Context, bucket: &str) -> Result<(), WaferError> {
    db::delete_by_field(
        ctx,
        TABLE,
        "bucket",
        serde_json::Value::String(bucket.to_string()),
    )
    .await
}

/// Delete the object row for `(bucket, key)` (object-deletion metadata
/// cleanup). Returns how many rows were removed, so the caller can tell a
/// cleanup from a delete of something that never existed.
pub async fn delete_by_bucket_key(
    ctx: &dyn Context,
    bucket: &str,
    key: &str,
) -> Result<i64, WaferError> {
    db::delete_by_filters_count(
        ctx,
        TABLE,
        vec![
            Filter {
                field: "bucket".to_string(),
                operator: FilterOp::Equal,
                value: serde_json::Value::String(bucket.to_string()),
            },
            Filter {
                field: "key".to_string(),
                operator: FilterOp::Equal,
                value: serde_json::Value::String(key.to_string()),
            },
        ],
    )
    .await
}

/// Delete `user_id`'s `pending`-status rows with `uploaded_at` strictly
/// before `cutoff` (an RFC 3339 timestamp, string-compared the same way the
/// column is written). See `quota::sweep_stale_pending` for the policy and
/// why this is safe to run best-effort on every upload.
pub async fn delete_stale_pending(
    ctx: &dyn Context,
    user_id: &str,
    cutoff: &str,
) -> Result<(), WaferError> {
    let filters = vec![
        Filter {
            field: "uploaded_by".to_string(),
            operator: FilterOp::Equal,
            value: serde_json::Value::String(user_id.to_string()),
        },
        status_is(ObjectStatus::Pending),
        Filter {
            field: "uploaded_at".to_string(),
            operator: FilterOp::LessThan,
            value: serde_json::Value::String(cutoff.to_string()),
        },
    ];
    db::delete_by_filters(ctx, TABLE, filters).await
}

/// Search `user_id`'s `complete` objects whose key contains `query`
/// (case rules per backend `LIKE`), newest upload first. `query` is
/// LIKE-escaped here ([`escape_like`]) so `%`/`_` match literally.
pub async fn search_completed(
    ctx: &dyn Context,
    user_id: &str,
    query: &str,
    limit: i64,
    offset: i64,
) -> Result<Page<ObjectRow>, WaferError> {
    let opts = ListOptions {
        filters: vec![
            Filter {
                field: "key".to_string(),
                operator: FilterOp::Like,
                value: serde_json::Value::String(format!("%{}%", escape_like(query))),
            },
            // Only show the current user's files
            Filter {
                field: "uploaded_by".to_string(),
                operator: FilterOp::Equal,
                value: serde_json::Value::String(user_id.to_string()),
            },
            // Exclude pending uploads
            Filter {
                field: "status".to_string(),
                operator: FilterOp::Equal,
                value: serde_json::Value::String("complete".to_string()),
            },
        ],
        sort: vec![SortField {
            field: "uploaded_at".to_string(),
            desc: true,
        }],
        limit,
        offset,
        skip_count: false,
        ..Default::default()
    };
    Page::try_decode(db::list(ctx, TABLE, &opts).await?, ObjectRow::from_record)
}

/// List up to `limit` object rows in `bucket`, sorted by `key` ascending
/// (the SSR object-browser order).
pub async fn list_for_bucket(
    ctx: &dyn Context,
    bucket: &str,
    limit: i64,
) -> Result<Page<ObjectRow>, WaferError> {
    let opts = ListOptions {
        filters: vec![Filter {
            field: "bucket".to_string(),
            operator: FilterOp::Equal,
            value: serde_json::Value::String(bucket.to_string()),
        }],
        sort: vec![SortField {
            field: "key".to_string(),
            desc: false,
        }],
        limit,
        ..Default::default()
    };
    Page::try_decode(db::list(ctx, TABLE, &opts).await?, ObjectRow::from_record)
}

/// Object counts per bucket for the given bucket names, via a single
/// GROUP BY aggregate (one row per bucket) — avoids an N+1 `db::count` per
/// bucket. Counts ALL rows in each bucket regardless of `uploaded_by` or
/// status, matching the previous per-bucket `db::count` semantics. Buckets
/// with zero objects are simply absent from the returned map.
pub async fn count_by_bucket(
    ctx: &dyn Context,
    bucket_names: &[String],
) -> Result<HashMap<String, i64>, WaferError> {
    let names: Vec<serde_json::Value> = bucket_names
        .iter()
        .map(|s| serde_json::Value::String(s.clone()))
        .collect();
    let req = wire::AggregateRequest {
        collection: TABLE.to_string(),
        select_columns: vec!["bucket".into()],
        aggregates: vec![wire::AggregateColumnDef::Count {
            alias: "cnt".into(),
        }],
        filters: vec![wire::FilterNode::Leaf(wire::FilterDef {
            field: "bucket".into(),
            operator: "in".into(),
            value: serde_json::Value::Array(names),
        })],
        group_by: vec![wire::GroupByDef::Column("bucket".into())],
        sort: vec![],
        limit: 0,
    };
    let rows = db::aggregate(ctx, req).await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let bucket = r.data.get("bucket").and_then(|v| v.as_str())?.to_string();
            let cnt = r.i64_field("cnt");
            Some((bucket, cnt))
        })
        .collect())
}

/// Number of `complete` object rows (admin stats).
pub async fn count_completed(ctx: &dyn Context) -> Result<i64, WaferError> {
    db::count(ctx, TABLE, &complete_filter()).await
}

/// `SUM(size)` over `complete` object rows (admin stats).
pub async fn sum_size_completed(ctx: &dyn Context) -> Result<f64, WaferError> {
    db::sum(ctx, TABLE, "size", &complete_filter()).await
}

/// Number of object rows uploaded by `user_id` (quota accounting —
/// includes `pending` reservations).
pub async fn count_for_uploader(ctx: &dyn Context, user_id: &str) -> Result<i64, WaferError> {
    db::count(ctx, TABLE, &owned_objects_filter(user_id)).await
}

/// `SUM(size)` over the rows uploaded by `user_id` (quota accounting —
/// includes `pending` reservations; no row materialization).
pub async fn sum_size_for_uploader(ctx: &dyn Context, user_id: &str) -> Result<f64, WaferError> {
    db::sum(ctx, TABLE, "size", &owned_objects_filter(user_id)).await
}

/// Test-fixture seeding: insert a raw row map exactly as given (no stamped
/// columns), so tests control the precise row shape.
#[cfg(test)]
pub async fn seed(
    ctx: &dyn Context,
    data: HashMap<String, serde_json::Value>,
) -> Result<ObjectRow, WaferError> {
    ObjectRow::from_record(&db::create(ctx, TABLE, data).await?)
}

/// Test helper: every object row, unfiltered.
#[cfg(test)]
pub async fn list_all(ctx: &dyn Context) -> Result<Vec<ObjectRow>, WaferError> {
    db::list_all(ctx, TABLE, vec![])
        .await?
        .iter()
        .map(ObjectRow::from_record)
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn record(data: &[(&str, serde_json::Value)]) -> Record {
        Record {
            id: "o1".to_string(),
            data: data
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        }
    }

    #[test]
    fn from_record_decodes_the_whole_row() {
        let row = ObjectRow::from_record(&record(&[
            ("bucket", json!("photos")),
            ("key", json!("nested/a.png")),
            ("size", json!(1024)),
            ("content_type", json!("image/png")),
            ("status", json!(ObjectStatus::Complete)),
            ("uploaded_by", json!("alice")),
            ("uploaded_at", json!("2026-05-06T10:00:00Z")),
            ("created_at", json!("2026-05-06T10:00:00Z")),
            ("updated_at", json!("2026-05-06T10:00:01Z")),
        ]))
        .expect("the row decodes");
        assert_eq!(
            row,
            ObjectRow {
                id: "o1".to_string(),
                bucket: "photos".to_string(),
                key: "nested/a.png".to_string(),
                size: 1024,
                content_type: "image/png".to_string(),
                status: ObjectStatus::Complete,
                uploaded_by: "alice".to_string(),
                uploaded_at: "2026-05-06T10:00:00Z".to_string(),
                created_at: "2026-05-06T10:00:00Z".to_string(),
                updated_at: "2026-05-06T10:00:01Z".to_string(),
            }
        );
    }

    /// `size` is `INTEGER` in the schema but a TEXT-typed backend hands it
    /// back as a string. `i64_field` takes both, so a TEXT-stored size still
    /// counts toward the user's quota instead of reading as zero — the same
    /// class of bug as B13's `public`, on the column that decides whether an
    /// upload is admitted.
    #[test]
    fn from_record_reads_a_text_stored_size() {
        assert_eq!(
            ObjectRow::from_record(&record(&[("size", json!("2048"))]))
                .expect("the row decodes")
                .size,
            2048
        );
        assert_eq!(
            ObjectRow::from_record(&record(&[("size", json!(2048))]))
                .expect("the row decodes")
                .size,
            2048
        );
        assert_eq!(
            ObjectRow::from_record(&record(&[]))
                .expect("the row decodes")
                .size,
            0
        );
    }

    /// `status` decides whether an object is a completed upload or an
    /// in-flight reservation — quota counts both, search and the admin stats
    /// count only `complete`. A row holding anything else belongs to neither
    /// set, so it is a decode failure naming the row, not a value the block
    /// carries around and compares against two literals.
    #[test]
    fn a_status_outside_the_set_is_refused_and_names_the_row() {
        for stored in ["complete", "pending"] {
            assert!(ObjectRow::from_record(&record(&[("status", json!(stored))])).is_ok());
        }
        // An unset column is what the DDL's `DEFAULT 'complete'` produces.
        assert_eq!(
            ObjectRow::from_record(&record(&[]))
                .expect("an unset status decodes")
                .status,
            ObjectStatus::Complete
        );

        let err = ObjectRow::from_record(&record(&[("status", json!("half"))]))
            .expect_err("a status outside the set must not decode");
        assert_eq!(err.code, wafer_run::ErrorCode::Internal);
        assert!(err.message.contains("o1"), "{}", err.message);
        assert!(err.message.contains("status"), "{}", err.message);
        assert!(err.message.contains("half"), "{}", err.message);
    }
}
