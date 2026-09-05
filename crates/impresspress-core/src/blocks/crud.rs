//! Generic CRUD helpers for block handlers.
//!
//! `Result`-returning primitives (`read_json_body`, `list_page`,
//! `get_record`, `create_record`, `update_record`, `delete_record` and the
//! `*_owned` variants) do one database step each and hand back either the
//! row or a ready-to-send error response. A handler that publishes a typed
//! view composes them: parse a typed request, turn it into the column map,
//! run the step, project the row through `View::from_record`. The record id
//! is read only as the block's route table bound it (`path_id`, `msg.var`);
//! the untyped `crud_*` one-liners that used to strip it off the path went
//! with their last caller.
//!
// audit-allow-file: pure pass-through helpers — every db::* call here takes
// the table name as a `collection: &str` parameter from the caller. WRAP
// coverage is the caller's responsibility; static analysis at this file
// would flag every line as unresolved without surfacing a real bug.

use std::collections::HashMap;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use wafer_block::db::{Filter, SortField};
use wafer_core::clients::database::{self as db, Record, RecordList};
use wafer_run::{context::Context, ErrorCode, InputStream, Message, OutputStream};

use crate::{
    http::{err_bad_request, err_internal, err_not_found, err_unauthorized},
    util::{field_as_string, stamp_created, stamp_updated},
};

/// Response body of every CRUD delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Deleted {
    /// Always `true`: a delete that did not happen is an error response.
    pub deleted: bool,
}

impl Deleted {
    /// The one value this type ever carries.
    pub const fn done() -> Self {
        Self { deleted: true }
    }
}

/// The record id for a CRUD route — `{id}` as the block's route table bound
/// it — or the 400 a missing id turns into. The matcher never binds an empty
/// segment, so the guard only fires for a handler called with a message that
/// did not go through the table.
pub fn path_id<'m>(msg: &'m Message, not_found_label: &str) -> Result<&'m str, OutputStream> {
    let id = msg.var("id");
    if id.is_empty() {
        return Err(err_bad_request(&format!(
            "Missing {} ID",
            not_found_label.to_lowercase()
        )));
    }
    Ok(id)
}

// ---------------------------------------------------------------------------
// Typed primitives
// ---------------------------------------------------------------------------

/// Deserialize the request body into `T`, or the 400 a malformed body turns
/// into. The error text names the serde failure so a client learns which
/// field was wrong.
pub async fn read_json_body<T: DeserializeOwned>(input: InputStream) -> Result<T, OutputStream> {
    read_json_body_or(input, |detail| {
        err_bad_request(&format!("Invalid body: {detail}"))
    })
    .await
}

/// [`read_json_body`] for a block that must build the 400 itself.
///
/// `on_error` receives the serde failure text and returns the refusal to send.
/// The dev sandbox is the caller that needs this: every `/b/dev` response —
/// the refusals included — has to carry `Cache-Control: no-store`, which
/// [`err_bad_request`]'s plain error terminal does not. Parameterizing the
/// error here keeps one body reader rather than a second copy of it in the
/// block.
pub async fn read_json_body_or<T, F>(input: InputStream, on_error: F) -> Result<T, OutputStream>
where
    T: DeserializeOwned,
    F: FnOnce(String) -> OutputStream,
{
    let raw = input.collect_to_bytes().await;
    serde_json::from_slice(&raw).map_err(|e| on_error(e.to_string()))
}

/// One page of `collection`, with caller-supplied filters and sort (`None` =
/// newest first by `created_at`).
pub async fn list_page(
    ctx: &dyn Context,
    collection: &str,
    page: i64,
    page_size: i64,
    filters: Vec<Filter>,
    sort: Option<Vec<SortField>>,
) -> Result<RecordList, OutputStream> {
    let sort = sort.unwrap_or_else(|| {
        vec![SortField {
            field: "created_at".to_string(),
            desc: true,
        }]
    });
    db::paginated_list(ctx, collection, page, page_size, filters, sort)
        .await
        .map_err(|e| err_internal("Database error", e))
}

/// Fetch `id` from `collection`, mapping a missing row to a 404 labelled
/// `not_found_label`.
pub async fn get_record(
    ctx: &dyn Context,
    collection: &str,
    id: &str,
    not_found_label: &str,
) -> Result<Record, OutputStream> {
    db::get(ctx, collection, id).await.map_err(|e| {
        if e.code == ErrorCode::NotFound {
            err_not_found(&format!("{not_found_label} not found"))
        } else {
            err_internal("Database error", e)
        }
    })
}

/// Insert `data` into `collection`, stamping `created_at` / `updated_at`
/// when the caller did not, and return the row as stored.
///
/// `db::create` hands back the map it was given plus the id — not the row.
/// Every column the caller omitted and the table defaulted (`currency`,
/// `current_version`, `metadata`, …) is absent from that map, so a view
/// projected from it would report the zero value where the database holds
/// the default. `db::update` already re-fetches by id; this does the same so
/// a create response and a subsequent read describe the same row.
pub async fn create_record(
    ctx: &dyn Context,
    collection: &str,
    mut data: HashMap<String, serde_json::Value>,
) -> Result<Record, OutputStream> {
    stamp_created(&mut data);
    let created = db::create(ctx, collection, data)
        .await
        .map_err(|e| err_internal("Database error", e))?;
    db::get(ctx, collection, &created.id)
        .await
        .map_err(|e| err_internal("Database error", e))
}

/// Apply `data` to `id` in `collection`, stamping `updated_at`; a missing
/// row is a 404 labelled `not_found_label`.
pub async fn update_record(
    ctx: &dyn Context,
    collection: &str,
    id: &str,
    mut data: HashMap<String, serde_json::Value>,
    not_found_label: &str,
) -> Result<Record, OutputStream> {
    stamp_updated(&mut data);
    db::update(ctx, collection, id, data).await.map_err(|e| {
        if e.code == ErrorCode::NotFound {
            err_not_found(&format!("{not_found_label} not found"))
        } else {
            err_internal("Database error", e)
        }
    })
}

/// Delete `id` from `collection`; a missing row is a 404 labelled
/// `not_found_label`.
pub async fn delete_record(
    ctx: &dyn Context,
    collection: &str,
    id: &str,
    not_found_label: &str,
) -> Result<Deleted, OutputStream> {
    db::delete(ctx, collection, id)
        .await
        .map(|()| Deleted::done())
        .map_err(|e| {
            if e.code == ErrorCode::NotFound {
                err_not_found(&format!("{not_found_label} not found"))
            } else {
                err_internal("Database error", e)
            }
        })
}

// ---------------------------------------------------------------------------
// Owner-scoped CRUD helpers
// ---------------------------------------------------------------------------

/// Identifies an owner-scoped resource for the `*_owned` helpers.
///
/// Owner-scoped resources are user-facing rows where access requires the
/// requesting user to match the row's owner column (e.g. a user's own
/// products or groups). The record is the `{id}` the route table bound.
pub struct OwnedResource<'a> {
    /// Table the records live in.
    pub collection: &'a str,
    /// Column holding the owning user's id (e.g. `"created_by"`).
    pub owner_field: &'a str,
    /// Human-readable label for error messages (e.g. `"Product"`).
    pub label: &'a str,
}

/// Fetch `id` from `collection` and verify `record[owner_field] == user_id`.
///
/// Returns the record on success. On failure returns a ready-to-send error
/// response: 401 for unauthenticated callers, 404 for both "row missing" and
/// "row owned by someone else" (existence must not leak to non-owners), and
/// 500 for database errors.
pub async fn verify_owner(
    ctx: &dyn Context,
    collection: &str,
    id: &str,
    owner_field: &str,
    user_id: &str,
    not_found_label: &str,
) -> Result<Record, OutputStream> {
    if user_id.is_empty() {
        return Err(err_unauthorized("Not authenticated"));
    }
    match db::get(ctx, collection, id).await {
        Ok(record) => {
            if field_as_string(&record, owner_field) != user_id {
                return Err(err_not_found(&format!("{not_found_label} not found")));
            }
            Ok(record)
        }
        Err(e) if e.code == ErrorCode::NotFound => {
            Err(err_not_found(&format!("{not_found_label} not found")))
        }
        Err(e) => Err(err_internal("Database error", e)),
    }
}

/// The owner-scoped record named by the path, after the ownership check.
pub async fn get_owned(
    ctx: &dyn Context,
    msg: &Message,
    res: &OwnedResource<'_>,
) -> Result<Record, OutputStream> {
    let id = path_id(msg, res.label)?;
    verify_owner(
        ctx,
        res.collection,
        id,
        res.owner_field,
        msg.user_id(),
        res.label,
    )
    .await
}

/// Apply `data` to the owner-scoped record named by the path, after the
/// ownership check, stamping `updated_at`.
pub async fn update_owned(
    ctx: &dyn Context,
    msg: &Message,
    res: &OwnedResource<'_>,
    data: HashMap<String, serde_json::Value>,
) -> Result<Record, OutputStream> {
    let id = path_id(msg, res.label)?.to_string();
    verify_owner(
        ctx,
        res.collection,
        &id,
        res.owner_field,
        msg.user_id(),
        res.label,
    )
    .await?;
    update_record(ctx, res.collection, &id, data, res.label).await
}

/// Delete the owner-scoped record named by the path, after the ownership
/// check.
pub async fn delete_owned(
    ctx: &dyn Context,
    msg: &Message,
    res: &OwnedResource<'_>,
) -> Result<Deleted, OutputStream> {
    let id = path_id(msg, res.label)?.to_string();
    verify_owner(
        ctx,
        res.collection,
        &id,
        res.owner_field,
        msg.user_id(),
        res.label,
    )
    .await?;
    delete_record(ctx, res.collection, &id, res.label).await
}
