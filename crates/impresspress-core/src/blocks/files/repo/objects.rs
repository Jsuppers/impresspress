//! Row-level access over `impresspress__files__objects`.
//!
//! Object metadata rows — one row per uploaded file (sibling of the raw
//! storage blob in `wafer-run/storage`). Tracks size, content type,
//! status, uploader and timestamps. A row is claimed `pending` *before* the
//! storage upload ([`reserve_upload`], so a later quota check counts it —
//! `quota::check_quota` says what that does and does not bound) and flipped to `complete` afterward; quota accounting sums/counts by
//! `uploaded_by` (including in-flight `pending` reservations), while
//! user-facing search and admin stats only see `complete` rows.
//!
//! `(bucket, key)` is UNIQUE, so a re-upload reuses the existing row rather
//! than inserting a second one — see [`reserve_upload`].

use std::collections::HashMap;

use wafer_block::{
    db::{Filter, FilterOp, ListOptions, SortField},
    wire::database::{self as wire, OnConflict},
};
use wafer_core::clients::database::{self as db, Record};
use wafer_run::{context::Context, ErrorCode, WaferError};

use super::{super::contracts::ObjectStatus, Page};
use crate::{
    db_read::{self, Bound},
    util::{enum_column_or, RecordExt},
};

/// Object metadata table — one row per uploaded file (sibling of the raw
/// storage blob in `wafer-run/storage`). Tracks size, content type, status,
/// uploader and timestamps.
pub const TABLE: &str = "impresspress__files__objects";

/// One object-metadata row, decoded.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
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

/// The row for `(bucket, key)`, or `None` when the key holds no object.
///
/// `(bucket, key)` is UNIQUE (`idx_objects_bucket_key`, migration 001), so
/// there is at most one.
pub async fn find_by_bucket_key(
    ctx: &dyn Context,
    bucket: &str,
    key: &str,
) -> Result<Option<ObjectRow>, WaferError> {
    let records = db_read::list_bounded(
        ctx,
        TABLE,
        bucket_key_filters(bucket, key),
        Bound::UniqueKey("idx_objects_bucket_key on (bucket, key)"),
    )
    .await?;
    records.first().map(ObjectRow::from_record).transpose()
}

fn bucket_key_filters(bucket: &str, key: &str) -> Vec<Filter> {
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
    ]
}

/// How long a [`ObjectStatus::Pending`] row is taken to belong to an upload
/// still in flight, in seconds. Past it the row is an orphan: an upload whose
/// request died between claiming the key and settling it. The largest
/// realistic upload finishes well inside an hour.
///
/// Two readers, one policy: [`reserve_upload`] refuses a key whose `Pending`
/// row is younger than this and takes over one that is older, and
/// `quota::sweep_stale_pending` deletes the uploader's rows past it.
pub const PENDING_RESERVATION_TTL_SECONDS: i64 = 3600;

/// The RFC 3339 instant before which a `Pending` row's `uploaded_at` makes it
/// stale — compared as a string, the way the column is written and the way
/// [`delete_stale_pending`] compares it.
pub fn pending_reservation_cutoff() -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(PENDING_RESERVATION_TTL_SECONDS)).to_rfc3339()
}

/// The stored object a [`Reservation`] took the place of, as its row read
/// before the reservation overwrote it. [`release_reservation`] writes it back
/// when the storage upload fails. Always a `Complete` row: a reservation never
/// takes over an upload still in flight, and a stale `Pending` row is not an
/// object to restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacedObject {
    pub size: i64,
    pub content_type: String,
    pub uploaded_by: String,
    pub uploaded_at: String,
}

/// A claim on `(bucket, key)` held while a storage upload is in flight, taken
/// by [`reserve_upload`] and settled by [`mark_complete`] or
/// [`release_reservation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    /// Row id of the reservation — a new row, or the row it took over.
    pub id: String,
    /// This reservation's token, written to the row's `claim_id` column when
    /// the claim was taken. Random per reservation, so the row still carries
    /// it exactly as long as no other reservation has taken the row since —
    /// which is what [`mark_complete`] and [`release_reservation`] require
    /// before they touch it.
    pub claim_id: String,
    /// `Some` when this upload overwrites an object already stored under the
    /// key, carrying what that object's row said. `None` when there was no
    /// stored object to put back: a new key, or a stale `Pending` row.
    pub replaced: Option<ReplacedObject>,
}

/// Why [`reserve_upload`] did not claim the key.
#[derive(Debug)]
pub enum ReserveError {
    /// Another upload holds the key, or took it between this reservation's
    /// read and its write. A conflict the caller retries once that upload
    /// settles.
    Held,
    /// The key is held by a reservation of this same uploader that is younger
    /// than [`PENDING_RESERVATION_TTL_SECONDS`]: an upload of theirs still in
    /// flight, or one whose storage write finished but whose row could not be
    /// marked complete. The row cannot say which, so it is not taken over
    /// early — two uploads of one key in flight at once would leave the row
    /// describing one of them and the blob holding whichever stored last.
    /// `since` is that reservation's `uploaded_at`; the key is free
    /// [`PENDING_RESERVATION_TTL_SECONDS`] after it.
    HeldByOwnEarlierUpload { since: String },
    /// The database call itself failed.
    Db(WaferError),
}

impl From<WaferError> for ReserveError {
    fn from(error: WaferError) -> Self {
        Self::Db(error)
    }
}

/// Claim `(bucket, key)` for an upload of `size` bytes, BEFORE the storage
/// upload runs, so a quota check that runs after this insert counts the
/// in-flight size. It narrows the check-quota → upload race without closing
/// it: the check and this claim are separate calls, and a claim is exclusive
/// per key, not per bucket (see `quota::check_quota`). `uploaded_at` is stamped
/// with [`crate::util::now_rfc3339`], and `claim_id` with a fresh random token
/// the returned [`Reservation`] carries.
///
/// `(bucket, key)` is UNIQUE, so the key has at most one row, and what that
/// row says decides the claim:
///
/// - **No row**: an insert that yields to the unique index
///   (`ON CONFLICT (bucket, key) DO NOTHING`) rather than being refused by
///   it. If another upload's insert got there first, this one affects
///   nothing and decides again on the row it reads back.
/// - **`Complete`**: a re-upload. The reservation TAKES OVER the row,
///   flipping it to [`ObjectStatus::Pending`] with the new size, content type
///   and uploader. Until the upload settles the row charges the new (possibly
///   larger) size against the new uploader — the conservative direction — and
///   [`release_reservation`] puts the old values back if the upload fails.
/// - **`Pending`, younger than [`PENDING_RESERVATION_TTL_SECONDS`]**: an
///   upload of the key is in flight, or was and could not be recorded.
///   Refused: taking it over would leave two uploads writing one blob and one
///   row. [`ReserveError::HeldByOwnEarlierUpload`] when that reservation is
///   this uploader's own, [`ReserveError::Held`] otherwise.
/// - **`Pending`, older**: an orphan. Taken over as a fresh claim, with
///   nothing to put back.
///
/// Also [`ReserveError::Held`] when another upload claimed the row between
/// this one's read and its write (every take-over is conditional on the row
/// still carrying the `claim_id` it was read with), or when the insert lost
/// to another upload whose row is gone again by the read-back — that upload
/// failed and released the key, which is free to retry.
pub async fn reserve_upload(
    ctx: &dyn Context,
    bucket: &str,
    key: &str,
    size: usize,
    content_type: &str,
    uploaded_by: &str,
) -> Result<Reservation, ReserveError> {
    let uploaded_at = crate::util::now_rfc3339();
    let claim_id = uuid::Uuid::new_v4().to_string();
    let claim = PendingClaim {
        bucket,
        key,
        size,
        content_type,
        uploaded_by,
        uploaded_at: &uploaded_at,
        claim_id: &claim_id,
    };

    if let Some(existing) = find_claimable(ctx, bucket, key).await? {
        return claim_existing(ctx, existing, &claim).await;
    }
    if let Some(id) = insert_unless_taken(ctx, &claim).await? {
        return Ok(Reservation {
            id,
            claim_id,
            replaced: None,
        });
    }
    match find_claimable(ctx, bucket, key).await? {
        Some(existing) => claim_existing(ctx, existing, &claim).await,
        None => Err(ReserveError::Held),
    }
}

/// The column values a [`Reservation`] writes.
struct PendingClaim<'a> {
    bucket: &'a str,
    key: &'a str,
    size: usize,
    content_type: &'a str,
    uploaded_by: &'a str,
    uploaded_at: &'a str,
    claim_id: &'a str,
}

/// The key's row as a reservation reads it: the decoded row, plus the
/// `claim_id` a take-over is conditional on. Kept off [`ObjectRow`], which is
/// published: the token is the row's lock, not something a reader of the
/// object needs.
struct ClaimableRow {
    row: ObjectRow,
    /// `None` for a row written before migration 004 added the column.
    claim_id: Option<String>,
}

/// [`find_by_bucket_key`], keeping the row's `claim_id`.
async fn find_claimable(
    ctx: &dyn Context,
    bucket: &str,
    key: &str,
) -> Result<Option<ClaimableRow>, WaferError> {
    let records = db_read::list_bounded(
        ctx,
        TABLE,
        bucket_key_filters(bucket, key),
        Bound::UniqueKey("idx_objects_bucket_key on (bucket, key)"),
    )
    .await?;
    records
        .first()
        .map(|rec| {
            Ok(ClaimableRow {
                row: ObjectRow::from_record(rec)?,
                claim_id: rec.opt_str_field("claim_id"),
            })
        })
        .transpose()
}

/// Filters matching row `id` only while it still carries `claim_id` — the
/// claim it was read with, or `None` for a row no reservation has written
/// since migration 004.
fn still_claimed_by(id: &str, claim_id: Option<&str>) -> Vec<Filter> {
    let claim = match claim_id {
        Some(claim_id) => Filter {
            field: "claim_id".to_string(),
            operator: FilterOp::Equal,
            value: serde_json::Value::String(claim_id.to_string()),
        },
        None => Filter {
            field: "claim_id".to_string(),
            operator: FilterOp::IsNull,
            value: serde_json::Value::Null,
        },
    };
    vec![
        Filter {
            field: "id".to_string(),
            operator: FilterOp::Equal,
            value: serde_json::Value::String(id.to_string()),
        },
        claim,
    ]
}

/// Claim the key's existing row per [`reserve_upload`]'s rules: take over a
/// stored object or an orphaned reservation, refuse an upload in flight — and
/// refuse, the same way, a row another upload claimed since it was read.
async fn claim_existing(
    ctx: &dyn Context,
    existing: ClaimableRow,
    claim: &PendingClaim<'_>,
) -> Result<Reservation, ReserveError> {
    let ClaimableRow { row, claim_id } = existing;
    let replaced = match row.status {
        ObjectStatus::Complete => Some(ReplacedObject {
            size: row.size,
            content_type: row.content_type,
            uploaded_by: row.uploaded_by,
            uploaded_at: row.uploaded_at,
        }),
        ObjectStatus::Pending if row.uploaded_at < pending_reservation_cutoff() => None,
        ObjectStatus::Pending if row.uploaded_by == claim.uploaded_by => {
            return Err(ReserveError::HeldByOwnEarlierUpload {
                since: row.uploaded_at,
            })
        }
        ObjectStatus::Pending => return Err(ReserveError::Held),
    };
    let data = crate::util::json_map(serde_json::json!({
        "size": claim.size,
        "content_type": claim.content_type,
        "status": ObjectStatus::Pending,
        "uploaded_by": claim.uploaded_by,
        "uploaded_at": claim.uploaded_at,
        "updated_at": claim.uploaded_at,
        "claim_id": claim.claim_id,
    }));
    // Conditional on the row still carrying the claim it was read with: two
    // uploads that both read the same `Complete` row (or the same orphan) must
    // not both take it over, or the row ends up describing whichever wrote
    // last while the other's failure puts values back over an upload in
    // flight. Every reservation writes a fresh random `claim_id`, so the first
    // take-over changes it and the second matches nothing — whatever the two
    // clocks said.
    let unchanged = still_claimed_by(&row.id, claim_id.as_deref());
    if db::update_by_filters_count(ctx, TABLE, unchanged, data).await? == 0 {
        return Err(ReserveError::Held);
    }
    Ok(Reservation {
        id: row.id,
        claim_id: claim.claim_id.to_string(),
        replaced,
    })
}

/// Insert the `Pending` row `claim` describes unless `(bucket, key)` already
/// has one. `Some(id)` when this insert created the row; `None` when the
/// unique index already held a row for the key and nothing was written.
///
/// The id is minted here, not by the database, because `db::upsert` answers
/// only how many rows it affected: a row this call created has the id it
/// was given.
async fn insert_unless_taken(
    ctx: &dyn Context,
    claim: &PendingClaim<'_>,
) -> Result<Option<String>, WaferError> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = crate::util::now_rfc3339();
    let inserted = db::upsert(
        ctx,
        TABLE,
        vec![
            ("id".to_string(), serde_json::json!(id)),
            ("bucket".to_string(), serde_json::json!(claim.bucket)),
            ("key".to_string(), serde_json::json!(claim.key)),
            ("size".to_string(), serde_json::json!(claim.size)),
            (
                "content_type".to_string(),
                serde_json::json!(claim.content_type),
            ),
            (
                "status".to_string(),
                serde_json::json!(ObjectStatus::Pending),
            ),
            (
                "uploaded_by".to_string(),
                serde_json::json!(claim.uploaded_by),
            ),
            (
                "uploaded_at".to_string(),
                serde_json::json!(claim.uploaded_at),
            ),
            ("claim_id".to_string(), serde_json::json!(claim.claim_id)),
            ("created_at".to_string(), serde_json::json!(now)),
            ("updated_at".to_string(), serde_json::json!(now)),
        ],
        vec!["bucket".to_string(), "key".to_string()],
        // No columns to set: a conflict is `DO NOTHING`, and the row that
        // caused it is the one the caller reads back and takes over.
        OnConflict::SetColumns(vec![]),
    )
    .await?;
    Ok((inserted > 0).then_some(id))
}

/// The refusal [`mark_complete`] and [`release_reservation`] answer when the
/// row no longer carries the reservation's `claim_id`: it passed
/// [`PENDING_RESERVATION_TTL_SECONDS`] and another upload took the key over,
/// or the uploader's sweep deleted it. Either way the row is not this
/// reservation's to settle.
fn claim_lost() -> WaferError {
    WaferError::new(
        ErrorCode::Aborted,
        "the upload's reservation of this key was taken over",
    )
}

/// Give up a [`Reservation`] whose storage upload failed: delete the row it
/// claimed, or — when it took over the row of an object that is still stored
/// (`put` failed, so the old blob is in place) — put that object's values
/// back, so the blob keeps being described and charged as it was.
///
/// Only while the row still carries this reservation's `claim_id`; otherwise
/// [`ErrorCode::Aborted`] and nothing is written, because the row now belongs
/// to whichever upload took it over.
pub async fn release_reservation(
    ctx: &dyn Context,
    reservation: &Reservation,
) -> Result<(), WaferError> {
    let mine = still_claimed_by(&reservation.id, Some(&reservation.claim_id));
    let touched = match &reservation.replaced {
        None => db::delete_by_filters_count(ctx, TABLE, mine).await?,
        Some(previous) => {
            let data = crate::util::json_map(serde_json::json!({
                "size": previous.size,
                "content_type": previous.content_type,
                "status": ObjectStatus::Complete,
                "uploaded_by": previous.uploaded_by,
                "uploaded_at": previous.uploaded_at,
            }));
            db::update_by_filters_count(ctx, TABLE, mine, data).await?
        }
    };
    if touched == 0 {
        return Err(claim_lost());
    }
    Ok(())
}

/// Flip a reservation's [`ObjectStatus::Pending`] row to
/// [`ObjectStatus::Complete`] after its storage upload succeeded — the only
/// thing that settles a [`Reservation`] as a stored object.
///
/// Only while the row still carries this reservation's `claim_id`; otherwise
/// [`ErrorCode::Aborted`] and nothing is written — the row belongs to another
/// upload now, whose own completion is the one that settles it.
///
/// A row left `pending` is swept within the hour
/// (`quota::sweep_stale_pending`), so this failing means the upload is not
/// recorded: the caller reports it rather than answering `uploaded: true`.
pub async fn mark_complete(ctx: &dyn Context, reservation: &Reservation) -> Result<(), WaferError> {
    let data = crate::util::json_map(serde_json::json!({ "status": ObjectStatus::Complete }));
    let mine = still_claimed_by(&reservation.id, Some(&reservation.claim_id));
    if db::update_by_filters_count(ctx, TABLE, mine, data).await? == 0 {
        return Err(claim_lost());
    }
    Ok(())
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
    db::delete_by_filters_count(ctx, TABLE, bucket_key_filters(bucket, key)).await
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
            column: None,
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

/// Number of object rows `user_id` uploaded into `bucket` — what the
/// per-bucket file-count cap (`QuotaConfig::max_files_per_bucket`) is
/// checked against. Includes `pending` reservations, on the same basis as
/// [`count_for_uploader`] and [`sum_size_for_uploader`].
pub async fn count_for_uploader_in_bucket(
    ctx: &dyn Context,
    user_id: &str,
    bucket: &str,
) -> Result<i64, WaferError> {
    let mut filters = owned_objects_filter(user_id);
    filters.push(Filter {
        field: "bucket".to_string(),
        operator: FilterOp::Equal,
        value: serde_json::Value::String(bucket.to_string()),
    });
    db::count(ctx, TABLE, &filters).await
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
    db_read::list_every(ctx, TABLE, vec![])
        .await?
        .iter()
        .map(ObjectRow::from_record)
        .collect()
}

/// Test helper: every row of `TABLE`, undecoded — each column exactly as
/// stored, for asserting that something left the table alone.
#[cfg(test)]
pub async fn raw_rows(
    ctx: &dyn Context,
) -> Result<Vec<wafer_core::clients::database::Record>, WaferError> {
    crate::db_read::list_every(ctx, TABLE, vec![]).await
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

    /// A take-over is conditional on the claim the row was read with, not on
    /// a timestamp.
    ///
    /// The condition used to be the row's `updated_at`, and a claim stamps
    /// `updated_at` with its own `uploaded_at` — taken at the START of
    /// `reserve_upload`, on whatever clock the isolate has. A second upload
    /// that read the row before the first one's claim landed could therefore
    /// find the claim's stamp EQUAL to the one it read (same millisecond, or
    /// two clocks that disagree) and take the row too: two uploads in flight
    /// on one row. Pinning the first claim's stamp to the value both read is
    /// that collision, forced.
    #[tokio::test]
    async fn a_second_take_over_from_the_same_read_is_refused_whatever_the_clocks_say() {
        let ctx = crate::test_support::TestContext::with_files().await;
        let read_at = "2026-05-06T10:00:01Z";
        seed(
            &ctx,
            crate::util::json_map(json!({
                "bucket": "assets",
                "key": "same.txt",
                "size": 3,
                "status": ObjectStatus::Complete,
                "uploaded_by": "alice",
                "uploaded_at": "2026-05-06T10:00:00Z",
                "created_at": "2026-05-06T10:00:00Z",
                "updated_at": read_at,
            })),
        )
        .await
        .expect("seed a stored object");
        let first_read = find_claimable(&ctx, "assets", "same.txt")
            .await
            .expect("read")
            .expect("the row");
        let second_read = find_claimable(&ctx, "assets", "same.txt")
            .await
            .expect("read")
            .expect("the row");

        let a = PendingClaim {
            bucket: "assets",
            key: "same.txt",
            size: 5,
            content_type: "text/plain",
            uploaded_by: "alice",
            uploaded_at: read_at,
            claim_id: "claim-a",
        };
        claim_existing(&ctx, first_read, &a)
            .await
            .expect("the first take-over claims the row");

        let b = PendingClaim {
            uploaded_at: read_at,
            claim_id: "claim-b",
            size: 9,
            ..a
        };
        let second = claim_existing(&ctx, second_read, &b).await;

        assert!(
            matches!(second, Err(ReserveError::Held)),
            "the row was claimed since it was read: {second:?}"
        );
        let row = find_claimable(&ctx, "assets", "same.txt")
            .await
            .expect("read")
            .expect("the row");
        assert_eq!(
            (row.row.size, row.claim_id.as_deref()),
            (5, Some("claim-a")),
            "the row still describes the first claim"
        );
    }

    /// A reservation settles only the row it still holds.
    ///
    /// A reservation past [`PENDING_RESERVATION_TTL_SECONDS`] is an orphan to
    /// every other upload, which takes the row over. If the first upload then
    /// finishes, its `mark_complete` must not flip the NEW upload's row to
    /// `Complete` while that upload's bytes are still in flight, and its
    /// `release_reservation` must not delete the row out from under it — both
    /// used to act on the row id alone.
    #[tokio::test]
    async fn a_superseded_reservation_neither_completes_nor_releases_the_row() {
        let ctx = crate::test_support::TestContext::with_files().await;
        let alice = reserve_upload(&ctx, "assets", "same.txt", 5, "text/plain", "alice")
            .await
            .expect("alice reserves");
        // Alice's upload stalls past the TTL.
        let stale = (chrono::Utc::now()
            - chrono::Duration::seconds(2 * PENDING_RESERVATION_TTL_SECONDS))
        .to_rfc3339();
        db::update(
            &ctx,
            TABLE,
            &alice.id,
            crate::util::json_map(json!({ "uploaded_at": stale })),
        )
        .await
        .expect("age alice's reservation");
        let bob = reserve_upload(&ctx, "assets", "same.txt", 9, "text/csv", "bob")
            .await
            .expect("bob takes over the orphan");
        assert_eq!(bob.id, alice.id, "one key, one row");

        let completed = mark_complete(&ctx, &alice).await;
        let released = release_reservation(&ctx, &alice).await;

        assert_eq!(
            completed.map_err(|e| e.code),
            Err(ErrorCode::Aborted),
            "alice no longer holds the row"
        );
        assert_eq!(
            released.map_err(|e| e.code),
            Err(ErrorCode::Aborted),
            "alice no longer holds the row"
        );
        let rows = list_all(&ctx).await.expect("rows");
        assert_eq!(rows.len(), 1, "bob's reservation must not be deleted");
        assert_eq!(
            (rows[0].status, rows[0].uploaded_by.as_str(), rows[0].size),
            (ObjectStatus::Pending, "bob", 9),
            "bob's upload is still in flight and must stay so"
        );

        mark_complete(&ctx, &bob)
            .await
            .expect("bob settles his own reservation");
    }

    /// A key held by the uploader's OWN fresh reservation is refused with
    /// what holds it, not as someone else's upload.
    #[tokio::test]
    async fn a_key_held_by_the_uploaders_own_reservation_says_so() {
        let ctx = crate::test_support::TestContext::with_files().await;
        let first = reserve_upload(&ctx, "assets", "same.txt", 5, "text/plain", "alice")
            .await
            .expect("alice reserves");
        let began = find_by_bucket_key(&ctx, "assets", "same.txt")
            .await
            .expect("read")
            .expect("the row")
            .uploaded_at;

        let own = reserve_upload(&ctx, "assets", "same.txt", 5, "text/plain", "alice").await;
        let other = reserve_upload(&ctx, "assets", "same.txt", 5, "text/plain", "bob").await;

        match own {
            Err(ReserveError::HeldByOwnEarlierUpload { since }) => assert_eq!(since, began),
            other => panic!("alice's own reservation holds the key: {other:?}"),
        }
        assert!(
            matches!(other, Err(ReserveError::Held)),
            "bob is told another upload holds it: {other:?}"
        );
        mark_complete(&ctx, &first)
            .await
            .expect("neither refusal touched alice's reservation");
    }
}
