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
    http::{err_bad_request, err_forbidden, err_internal, err_not_found, err_unauthorized},
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
    if error.code == ErrorCode::NotFound {
        return err_not_found(not_found);
    }
    db_error_internal(error, context)
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
    match error.code {
        ErrorCode::PermissionDenied => {
            tracing::warn!(
                context = %context,
                error = %error,
                "database access denied — a WRAP grant or a row guard refused this call",
            );
            err_forbidden("Access denied")
        }
        ErrorCode::ResourceExhausted => OutputStream::error(wafer_run::WaferError::new(
            ErrorCode::ResourceExhausted,
            error.message,
        )),
        _ => err_internal(context, error),
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
        TestContext::new()
            .await
            .with_wrap("test/ungranted", Vec::new(), "impresspress/admin")
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
