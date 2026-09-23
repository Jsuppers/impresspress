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
    http::{err_bad_request, err_conflict, err_internal, err_not_found, err_unauthorized},
    util::{field_as_string, stamp_created, stamp_updated},
};

/// The response a failed database call turns into — the one place that
/// decides it.
///
/// Every arm exists because collapsing it into the 500 loses something the
/// caller needs:
///
/// - [`ErrorCode::NotFound`] is the row the caller asked for, so it is a 404
///   labelled `not_found` (the full message, not a noun: a route knows what
///   it was looking for and this helper does not).
/// - [`ErrorCode::PermissionDenied`] is a WRAP refusal — either a row guard
///   the caller is not the owner of, or a [`wafer_run::ResourceGrant`] the
///   block never declared. It is a **403**. Before this helper existed, all
///   62 hand-written mappings in the tree fell through to `err_internal`, so
///   a block deployed without a grant answered `500 Internal server error
///   (ref: …)` and an operator had nothing to distinguish it from a corrupt
///   row. The refusal's own message names the missing grant and the target
///   table, which is deployment topology, so it is logged here and the
///   client is told only that access was denied.
/// - [`ErrorCode::ResourceExhausted`] is a quota, which
///   `wafer_block::http_codec` already renders as 429. Its message is a
///   classified, client-actionable refusal from the service — the same class
///   this repo already echoes for `InvalidArgument` — so it is passed
///   through rather than sanitized.
/// - Everything else is an internal failure: `context` is the fixed log
///   label, the cause is logged, and the client gets the sanitized
///   `"Internal server error (ref: <id>)"`.
///
/// Domain classifications a *repo* raises (`InvalidArgument`,
/// `FailedPrecondition`, `Aborted`) are deliberately NOT here: they mean
/// different things per block, and the three block-private helpers that map
/// them (`products/handlers/{sellers,offers,product}.rs`) keep their own arms
/// and delegate only this tail.
pub fn db_error(error: wafer_run::WaferError, not_found: &str, context: &str) -> OutputStream {
    seal(classify_db_error(error, Some(not_found), context), context)
}

/// [`db_error`] for a call whose `NotFound` is NOT the client's row.
///
/// `db::paginated_list` and `db::create` are told the table by the block, not
/// by the request, so a `NotFound` from them means the table is missing —
/// a deployment fault, and a 500. Turning it into a 404 would tell a caller
/// their query found nothing when in fact nothing could be queried.
/// Everything else is classified exactly as [`db_error`] classifies it,
/// `PermissionDenied` included.
pub fn db_error_internal(error: wafer_run::WaferError, context: &str) -> OutputStream {
    seal(classify_db_error(error, None, context), context)
}

/// [`db_error_internal`] for a read a full page renders from.
///
/// A page whose read failed is never drawn from defaults (see
/// [`crate::ui::server_error_response`]), and what it answers instead is
/// classified here like every other failed database call: a WRAP denial is
/// the 403 page and a quota the 429 page ([`crate::ui::refused_response`]),
/// anything else is logged under `context` and answered with the styled 500.
/// An API caller (an `Accept` without `text/html`) gets the same statuses as
/// JSON.
pub fn db_error_page(msg: &Message, error: wafer_run::WaferError, context: &str) -> OutputStream {
    match classify_db_error(error, None, context) {
        DbFailure::Refused(error) => crate::ui::refused_response(msg, error),
        DbFailure::Internal(error) => {
            tracing::error!(context = %context, error = %error, "page read failed");
            crate::ui::server_error_response(msg)
        }
    }
}

/// [`db_error_page`] for a read behind an htmx swap: what the notice in place
/// of the fragment says went wrong.
///
/// A swap cannot answer the 403, 429 or 500 a page does — htmx 2 swaps only a
/// 2xx body, so the stale fragment would stay on screen — and so the caller
/// answers a 2xx notice ([`crate::ui::swap_error_response`] and its row
/// variant, or an alert in the swapped body) and puts this reason in it. It is
/// classified like every other failed database call: a WRAP denial says access
/// was denied and a quota says the usage limit, with the denial's own text
/// (grant and table names) logged, never shown. Anything else is logged under
/// `context` and said as a fault.
pub fn db_error_notice(error: wafer_run::WaferError, context: &str) -> &'static str {
    match classify_db_error(error, None, context) {
        DbFailure::Refused(error) if error.code == ErrorCode::ResourceExhausted => {
            "it is over its usage limit right now"
        }
        DbFailure::Refused(_) => "access to it was denied",
        DbFailure::Internal(error) => {
            tracing::error!(context = %context, error = %error, "fragment read failed");
            "something went wrong"
        }
    }
}

/// What [`db_error`] decided, before it is sealed into a response.
///
/// [`db_error`] seals this itself and is what almost every call site wants.
/// A block whose every response carries an extra header cannot use it —
/// an `OutputStream`'s meta is fixed when it is built, so the header has to
/// go on the error before it is sealed — and `blocks::dev` is that block:
/// design §12 makes every `/b/dev` response `Cache-Control: no-store`,
/// including its refusals. It seals this itself through
/// `dev::no_store_db_error`. **Nothing else may**: a third classification of
/// a database failure is exactly what `tests/error_door.rs` exists to stop.
pub enum DbFailure {
    /// A refusal the client is told about as it stands: the caller's 404,
    /// the 403 a WRAP denial becomes, the 429 a quota keeps. The cause, when
    /// it was one that must not be published, has already been logged and
    /// replaced.
    Refused(wafer_run::WaferError),
    /// An internal fault, carried back untouched — sanitizing it and minting
    /// its correlation id is [`crate::http::err_internal`]'s job, and doing
    /// it here would mean two places that log a 500.
    Internal(wafer_run::WaferError),
}

/// Classify a failed database call. `not_found` is `Some` when a `NotFound`
/// from this call means the row the *caller* named (so it is their 404), and
/// `None` when the block chose the address itself — a `db::paginated_list`
/// or `db::create` against a table the request never named, where a
/// `NotFound` is a missing table and therefore a 500.
pub fn classify_db_error(
    error: wafer_run::WaferError,
    not_found: Option<&str>,
    context: &str,
) -> DbFailure {
    match (error.code, not_found) {
        (ErrorCode::NotFound, Some(label)) => {
            DbFailure::Refused(wafer_run::WaferError::new(ErrorCode::NotFound, label))
        }
        (ErrorCode::PermissionDenied, _) => {
            tracing::warn!(
                context = %context,
                error = %error,
                "database access denied — a WRAP grant or a row guard refused this call",
            );
            DbFailure::Refused(wafer_run::WaferError::new(
                ErrorCode::PermissionDenied,
                "Access denied",
            ))
        }
        (ErrorCode::ResourceExhausted, _) => DbFailure::Refused(wafer_run::WaferError::new(
            ErrorCode::ResourceExhausted,
            error.message,
        )),
        _ => DbFailure::Internal(error),
    }
}

/// [`DbFailure`] as the response every caller but `blocks::dev` wants.
fn seal(failure: DbFailure, context: &str) -> OutputStream {
    match failure {
        DbFailure::Refused(error) => OutputStream::error(error),
        DbFailure::Internal(error) => err_internal(context, error),
    }
}

// ---------------------------------------------------------------------------
// Duplicate natural keys
// ---------------------------------------------------------------------------

/// What a failed `create` against a table with a UNIQUE natural key —
/// `variables.key`, `roles.name`, `permissions.name`, `buckets.name` —
/// actually means.
///
/// It lives here, beside [`db_error_internal`] it delegates to, because it is
/// the same decision in every block that has such a key: the admin block
/// (roles, permissions, variables) and the files block (bucket names) call
/// this one function rather than each keeping the reasoning below.
///
/// Those inserts are refused by the database when the key is already
/// taken, and that refusal used to ship as `err_internal("Database error", e)`:
/// a `500 Internal server error (ref: …)` for a request that is not a fault at
/// all. An admin who re-types a key that exists was told the server broke, and
/// an operator reading the log could not tell that request from a corrupt row
/// or an outage. The honest answer is **409** — the key is taken, edit it or
/// pick another — which is what [`ErrorCode::AlreadyExists`] resolves to in
/// `wafer_block::http_codec::error_code_to_http_status`.
///
/// It has to be decided by re-reading rather than by the write's own error,
/// because no `DatabaseService` backend classifies a constraint violation:
/// `wafer_core`'s `db_error_to_wafer` has three arms (`NotFound`, `Internal`,
/// `Other`) and the last two both become [`ErrorCode::Internal`] with the
/// driver's text *sanitized* away unless it is one of a few preserved
/// substrings. So there is nothing in the error to match on, and matching on
/// driver message text would be both magic and backend-specific. The same
/// reasoning, and the same probe-after-the-write shape, is already written out
/// at `products::handlers::product::restore_slug_conflict`.
///
/// The forward path is [`ErrorCode::AlreadyExists`]: a backend that DOES
/// classify the violation has already answered the question the probe exists to
/// ask, so that code short-circuits straight to the 409 and no re-read happens
/// at all. It is wired up now rather than when such a backend lands, because
/// sending it to [`db_error_internal`] instead — which classifies only
/// `NotFound`, `PermissionDenied` and `ResourceExhausted`, and folds everything
/// else into a 500 — would re-introduce this exact bug on the day the backend
/// improved, with every test still green because the in-memory SQLite these run
/// against answers `Internal`.
///
/// [`ErrorCode::Aborted`] is a probe candidate alongside `Internal` for the same
/// reason in reverse: `error_code_to_http_status` already renders it 409, and
/// the "concurrency conflict" it names is precisely what a unique-index
/// collision is. If the key turns out to be taken, that is this conflict; if it
/// does not, the write's own failure is kept.
///
/// Probing **after** the failed write rather than before it is what closes the
/// race: a pre-check that found the key free leaves a gap in which a competing
/// create can claim it, and the loser of that race is exactly the request that
/// would still have answered 500. Re-reading afterwards has no such gap — the
/// insert has already been refused, and the row that refused it is there to be
/// found. It also costs the successful create nothing, since the probe only
/// runs on the error path.
///
/// `probe` is the "is this key taken now?" read, passed as its own future so it
/// is only awaited here. Three answers, not two: taken is the conflict, free is
/// a genuine fault, and a probe that could not run is **not** "free" — "could
/// not tell" keeps the write's own failure, so a transient read outage cannot
/// turn a 500 into a wrong 409 or vice versa.
///
/// `context` is the log label every non-collision answer carries into
/// [`db_error_internal`] — the caller's own ("Failed to create bucket"), not a
/// generic one, so an operator reading the log still knows which write failed.
pub async fn taken_key_or_db_error(
    error: wafer_run::WaferError,
    probe: impl std::future::Future<Output = Result<bool, wafer_run::WaferError>>,
    conflict: &str,
    context: &str,
) -> OutputStream {
    match error.code {
        // For a backend that classifies the violation itself. No backend at
        // the current wafer pin does: they answer `Internal`, which the arm
        // below settles by re-reading the key.
        ErrorCode::AlreadyExists => return err_conflict(conflict),
        // The two codes a constraint violation can arrive as unclassified.
        ErrorCode::Internal | ErrorCode::Aborted => {}
        // A WRAP refusal (403) or a quota (429) is not a name collision and
        // keeps the status `crud` gives it.
        _ => return db_error_internal(error, context),
    }
    match probe.await {
        Ok(true) => err_conflict(conflict),
        Ok(false) => db_error_internal(error, context),
        Err(probe_error) => {
            tracing::warn!(
                error = %probe_error,
                "could not re-read the key a refused insert may have collided with",
            );
            db_error_internal(error, context)
        }
    }
}

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

/// The value the block's route table bound to `{var}`, or the 400 an empty
/// binding turns into. The matcher never binds an empty segment, so the
/// guard only fires for a handler called with a message that did not go
/// through the table.
///
/// `missing` is the whole 400 message, not a noun to be formatted: the noun
/// is per-route (`"Missing bucket name"`, `"Missing setting key"`,
/// `"Missing offer ID"`) and deriving it from a label would be a mapping
/// layer that has to be read to be understood. [`path_id`] is the one
/// spelling common enough to be worth a shorthand.
pub fn path_var<'m>(msg: &'m Message, var: &str, missing: &str) -> Result<&'m str, OutputStream> {
    let value = msg.var(var);
    if value.is_empty() {
        return Err(err_bad_request(missing));
    }
    Ok(value)
}

/// A filter query parameter whose values are a closed set, as the enum that
/// defines them — or the 400 a value outside the set turns into.
///
/// An absent parameter is `Ok(None)`, which every caller reads as "no
/// filter". A value the enum does not define is refused rather than handed
/// to the database as a literal that matches no row: `?role=bot` and
/// `?type=cookies` used to answer `200` with an empty page, which reads as
/// "there are none of those" and is a different sentence from "there is no
/// such thing". serde's own unknown-variant text names the variants, so the
/// 400 lists them without any call site spelling them a second time.
pub fn enum_query<T: DeserializeOwned>(
    msg: &Message,
    param: &str,
) -> Result<Option<T>, OutputStream> {
    let raw = msg.query(param);
    if raw.is_empty() {
        return Ok(None);
    }
    serde_json::from_value::<T>(serde_json::Value::String(raw.to_string()))
        .map(Some)
        .map_err(|e| err_bad_request(&format!("Invalid `{param}` filter: {e}")))
}

/// The record id for a CRUD route — [`path_var`] on `{id}`, with the message
/// the great majority of routes want (`"Missing product ID"` for a label of
/// `"Product"`).
pub fn path_id<'m>(msg: &'m Message, not_found_label: &str) -> Result<&'m str, OutputStream> {
    path_var(
        msg,
        "id",
        &format!("Missing {} ID", not_found_label.to_lowercase()),
    )
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
        .map_err(|e| db_error_internal(e, "Database error"))
}

/// Fetch `id` from `collection`, mapping a missing row to a 404 labelled
/// `not_found_label`.
pub async fn get_record(
    ctx: &dyn Context,
    collection: &str,
    id: &str,
    not_found_label: &str,
) -> Result<Record, OutputStream> {
    db::get(ctx, collection, id)
        .await
        .map_err(|e| db_error(e, &format!("{not_found_label} not found"), "Database error"))
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
        .map_err(|e| db_error_internal(e, "Database error"))?;
    db::get(ctx, collection, &created.id)
        .await
        .map_err(|e| db_error_internal(e, "Database error"))
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
    db::update(ctx, collection, id, data)
        .await
        .map_err(|e| db_error(e, &format!("{not_found_label} not found"), "Database error"))
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
        .map_err(|e| db_error(e, &format!("{not_found_label} not found"), "Database error"))
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
/// whatever [`db_error`] makes of the database failure (403 for a WRAP
/// refusal, 500 for the rest).
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
        Err(e) => Err(db_error(
            e,
            &format!("{not_found_label} not found"),
            "Database error",
        )),
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

#[cfg(test)]
mod db_error_tests {
    use wafer_core::clients::database as db;
    use wafer_run::WaferError;

    use super::*;
    use crate::test_support::{output_http_status, TestContext};

    fn wafer_err(code: ErrorCode, message: &str) -> WaferError {
        WaferError::new(code, message)
    }

    #[tokio::test]
    async fn db_error_maps_not_found_to_404() {
        let out = db_error(
            wafer_err(ErrorCode::NotFound, "row 7 is not there"),
            "Product not found",
            "Database error",
        );
        assert_eq!(output_http_status(out).await, 404);
    }

    /// The behaviour fix. A WRAP row-guard denial is a `PermissionDenied`
    /// from the database client; every hand-written mapping in the tree
    /// falls through to `err_internal`, so a missing grant reaches the
    /// client as `500 Internal server error (ref: …)`.
    #[tokio::test]
    async fn db_error_maps_permission_denied_to_403() {
        let out = db_error(
            wafer_err(
                ErrorCode::PermissionDenied,
                "WRAP: block 'impresspress/products' has no grant for the table it read",
            ),
            "Product not found",
            "Database error",
        );
        assert_eq!(output_http_status(out).await, 403);
    }

    /// …and the denial's own message names the missing grant and the table,
    /// which is deployment topology. It is logged, not published.
    #[tokio::test]
    async fn the_403_does_not_republish_the_wrap_error_text() {
        let out = db_error(
            wafer_err(
                ErrorCode::PermissionDenied,
                "WRAP: no grant for secret_table",
            ),
            "Product not found",
            "Database error",
        );
        match out.collect_buffered().await {
            Err(wafer_run::TerminalNotResponse::Error(e)) => {
                assert!(
                    !e.message.contains("secret_table") && !e.message.contains("WRAP"),
                    "403 body must not carry the denial detail, got {:?}",
                    e.message
                );
            }
            other => panic!("expected an error terminal, got {other:?}"),
        }
    }

    /// `db_error_internal` is the same classification MINUS the 404: a
    /// `NotFound` from a call the block addressed (a missing table) is a
    /// deployment fault, not the caller's missing row.
    #[tokio::test]
    async fn db_error_internal_keeps_a_missing_table_a_500_but_still_403s_a_denial() {
        let missing_table = db_error_internal(
            wafer_err(ErrorCode::NotFound, "no such table"),
            "Database error",
        );
        assert_eq!(output_http_status(missing_table).await, 500);

        let denied = db_error_internal(
            wafer_err(ErrorCode::PermissionDenied, "WRAP: no grant"),
            "Database error",
        );
        assert_eq!(output_http_status(denied).await, 403);
    }

    #[tokio::test]
    async fn db_error_keeps_resource_exhausted_at_429() {
        let out = db_error(
            wafer_err(ErrorCode::ResourceExhausted, "storage quota exceeded"),
            "Object not found",
            "Database error",
        );
        assert_eq!(output_http_status(out).await, 429);
    }

    #[tokio::test]
    async fn db_error_sanitizes_everything_else_into_a_500() {
        let out = db_error(
            wafer_err(ErrorCode::Internal, "connection reset by peer"),
            "Product not found",
            "Database error",
        );
        match out.collect_buffered().await {
            Err(wafer_run::TerminalNotResponse::Error(e)) => {
                assert_eq!(wafer_block::http_codec::resolve_error_status(&e), 500);
                assert!(
                    e.message.starts_with("Internal server error (ref: "),
                    "500 body must be the sanitized form, got {:?}",
                    e.message
                );
            }
            other => panic!("expected an error terminal, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------------
    // End to end: the same denial arriving through the CRUD primitives.
    // ---------------------------------------------------------------------

    /// A table this test owns, so `tests/repo_door.rs` does not see the
    /// fixture as `crud.rs` reaching past another block's door.
    const FOREIGN_TABLE: &str = "impresspress__crudtest__rows";

    /// A context acting as a block with NO grants, so every typed database
    /// call it makes is refused by the same `wrap::check_access` the runtime
    /// applies.
    async fn denied_ctx() -> TestContext {
        TestContext::new().await.with_wrap(
            "test/ungranted",
            Vec::new(),
            Vec::new(),
            "impresspress/admin",
        )
    }

    #[tokio::test]
    async fn a_denied_read_through_get_record_is_403_not_500() {
        let ctx = denied_ctx().await;
        let out = get_record(&ctx, FOREIGN_TABLE, "any-id", "User")
            .await
            .expect_err("WRAP denies the read");
        assert_eq!(output_http_status(out).await, 403);
    }

    #[tokio::test]
    async fn a_denied_list_through_list_page_is_403_not_500() {
        let ctx = denied_ctx().await;
        let out = list_page(&ctx, FOREIGN_TABLE, 1, 10, Vec::new(), None)
            .await
            .expect_err("WRAP denies the list");
        assert_eq!(output_http_status(out).await, 403);
    }

    #[tokio::test]
    async fn a_denied_write_through_create_record_is_403_not_500() {
        let ctx = denied_ctx().await;
        let out = create_record(&ctx, FOREIGN_TABLE, HashMap::new())
            .await
            .expect_err("WRAP denies the write");
        assert_eq!(output_http_status(out).await, 403);
    }

    #[tokio::test]
    async fn a_denied_write_through_update_record_is_403_not_500() {
        let ctx = denied_ctx().await;
        let out = update_record(&ctx, FOREIGN_TABLE, "any-id", HashMap::new(), "User")
            .await
            .expect_err("WRAP denies the write");
        assert_eq!(output_http_status(out).await, 403);
    }

    #[tokio::test]
    async fn a_denied_delete_through_delete_record_is_403_not_500() {
        let ctx = denied_ctx().await;
        let out = delete_record(&ctx, FOREIGN_TABLE, "any-id", "User")
            .await
            .expect_err("WRAP denies the delete");
        assert_eq!(output_http_status(out).await, 403);
    }

    #[tokio::test]
    async fn a_denied_read_through_verify_owner_is_403_not_500() {
        let ctx = denied_ctx().await;
        let out = verify_owner(
            &ctx,
            FOREIGN_TABLE,
            "any-id",
            "created_by",
            "user-1",
            "User",
        )
        .await
        .expect_err("WRAP denies the read");
        assert_eq!(output_http_status(out).await, 403);
    }

    /// The grant path still answers as it did: a caller that may read the
    /// table gets the 404 a missing row deserves, so the 403 above is the
    /// denial and not a blanket refusal.
    #[tokio::test]
    async fn a_granted_read_of_a_missing_row_is_still_404() {
        let ctx = TestContext::new().await;
        db::ensure_table(
            &ctx,
            &wafer_block::wire::database::TableDef {
                name: FOREIGN_TABLE.to_string(),
                columns: vec![wafer_block::wire::database::ColumnDef {
                    name: "id".to_string(),
                    kind: "text".to_string(),
                    nullable: false,
                    primary_key: true,
                    auto_increment: false,
                    unique: false,
                    default: None,
                }],
                indexes: vec![],
                primary_key: vec![],
                unique_keys: vec![],
            },
        )
        .await
        .expect("the ungated fixture creates its table");
        let out = get_record(&ctx, FOREIGN_TABLE, "no-such-id", "Row")
            .await
            .expect_err("the row does not exist");
        assert_eq!(output_http_status(out).await, 404);
    }
}

#[cfg(test)]
mod path_var_tests {
    use super::*;
    use crate::test_support::output_http_status;

    fn msg_with(var: &str, value: &str) -> Message {
        let mut m = Message::new("http.request");
        m.set_meta(format!("req.param.{var}"), value);
        m
    }

    #[test]
    fn a_bound_segment_is_its_value() {
        let m = msg_with("offer_id", "off_1");
        assert_eq!(
            path_var(&m, "offer_id", "Missing offer ID").ok(),
            Some("off_1")
        );
        let m = msg_with("id", "prod_1");
        assert_eq!(path_id(&m, "Product").ok(), Some("prod_1"));
    }

    #[tokio::test]
    async fn an_unbound_segment_is_a_400_carrying_the_caller_s_message() {
        let m = Message::new("http.request");
        let out = path_var(&m, "offer_id", "Missing offer ID").expect_err("no binding");
        match out.collect_buffered().await {
            Err(wafer_run::TerminalNotResponse::Error(e)) => {
                assert_eq!(e.message, "Missing offer ID");
            }
            other => panic!("expected an error terminal, got {other:?}"),
        }
        let out = path_var(&Message::new("http.request"), "id", "Missing product ID")
            .expect_err("no binding");
        assert_eq!(output_http_status(out).await, 400);
    }

    /// `path_id` produces exactly the message the hand-rolled guards it
    /// replaces spelled, so converting them changes no wire text.
    #[tokio::test]
    async fn path_id_spells_the_message_the_hand_rolled_guards_spelled() {
        for (label, expected) in [
            ("Product", "Missing product ID"),
            ("Seller", "Missing seller ID"),
            ("User", "Missing user ID"),
            ("Grant", "Missing grant ID"),
        ] {
            let out = path_id(&Message::new("http.request"), label).expect_err("no binding");
            match out.collect_buffered().await {
                Err(wafer_run::TerminalNotResponse::Error(e)) => assert_eq!(e.message, expected),
                other => panic!("expected an error terminal, got {other:?}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use wafer_run::{ErrorCode, WaferError};

    use super::taken_key_or_db_error;

    /// A backend that DOES classify the violation short-circuits: the 409 comes
    /// straight off `AlreadyExists` and the probe is never run.
    ///
    /// This is the forward path the helper documents. Before it was wired up,
    /// `AlreadyExists` fell through to `crud::db_error_internal`, which
    /// classifies only `NotFound` / `PermissionDenied` / `ResourceExhausted`
    /// and folds the rest into a 500 — so the day a backend started reporting
    /// constraint violations properly, this bug would have come back, with
    /// every other test still green because the in-memory SQLite these run
    /// against answers `Internal`.
    #[tokio::test]
    async fn a_backend_classified_already_exists_is_the_conflict_without_a_probe() {
        let probed = std::cell::Cell::new(false);
        let out = taken_key_or_db_error(
            WaferError::new(ErrorCode::AlreadyExists, "duplicate key"),
            async {
                probed.set(true);
                Ok(false)
            },
            "TAKEN already exists",
            "Database error",
        )
        .await;

        assert_eq!(crate::test_support::output_http_status(out).await, 409);
        assert!(
            !probed.get(),
            "the backend already answered; do not re-read"
        );
    }

    /// `Aborted` is a probe candidate alongside `Internal`: it renders as 409
    /// too, and the "concurrency conflict" it names is what a unique-index
    /// collision is. The re-read still decides, so a free key keeps the fault.
    #[tokio::test]
    async fn an_aborted_write_is_classified_by_the_probe_like_an_internal_one() {
        let taken = taken_key_or_db_error(
            WaferError::new(ErrorCode::Aborted, "write conflict"),
            async { Ok(true) },
            "TAKEN already exists",
            "Database error",
        )
        .await;
        assert_eq!(crate::test_support::output_http_status(taken).await, 409);

        let free = taken_key_or_db_error(
            WaferError::new(ErrorCode::Aborted, "write conflict"),
            async { Ok(false) },
            "TAKEN already exists",
            "Database error",
        )
        .await;
        assert_eq!(crate::test_support::output_http_status(free).await, 500);
    }

    /// A code that is neither is not a collision candidate at all — it keeps
    /// the status `crud` gives it, and never reaches the probe.
    #[tokio::test]
    async fn a_wrap_refusal_keeps_its_403_and_is_never_probed() {
        let probed = std::cell::Cell::new(false);
        let out = taken_key_or_db_error(
            WaferError::new(ErrorCode::PermissionDenied, "denied"),
            async {
                probed.set(true);
                Ok(true)
            },
            "TAKEN already exists",
            "Database error",
        )
        .await;

        assert_eq!(crate::test_support::output_http_status(out).await, 403);
        assert!(!probed.get(), "a WRAP refusal is not a name collision");
    }
}
