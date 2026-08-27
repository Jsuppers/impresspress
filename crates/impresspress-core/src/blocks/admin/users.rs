use std::collections::HashMap;

use wafer_block::db::{Filter, FilterOp, SortField};
use wafer_core::clients::database as db;
use wafer_run::{context::Context, ErrorCode, InputStream, Message, OutputStream};

use super::{
    contracts::{AdminUserListQuery, AdminUserListResponse, AdminUserView},
    ops,
};
use crate::{
    blocks::auth::USERS_TABLE as COLLECTION,
    http::{err_bad_request, err_internal, err_not_found, ok_json},
};

/// `path` is the normalized `/admin/users[...]` sub-path passed explicitly by
/// the admin dispatcher (no `req.resource` rewrite). The leaf handlers read the
/// user id from `req.param.id`, which this dispatcher binds from `path`.
pub async fn handle(
    ctx: &dyn Context,
    msg: &Message,
    path: &str,
    input: InputStream,
) -> OutputStream {
    let action = msg.action();

    match (action, path) {
        ("retrieve", "/admin/users") => handle_list(ctx, msg).await,
        ("retrieve", _) if path.starts_with("/admin/users/") => {
            handle_get(ctx, msg, user_id_from(path)).await
        }
        ("update", _) if path.starts_with("/admin/users/") => {
            handle_update(ctx, msg, user_id_from(path), input).await
        }
        ("delete", _) if path.starts_with("/admin/users/") => {
            handle_delete(ctx, msg, user_id_from(path)).await
        }
        _ => err_not_found("not found"),
    }
}

/// Extract the first `/`-bounded user-id segment after `/admin/users/`.
fn user_id_from(path: &str) -> &str {
    let rest = path.strip_prefix("/admin/users/").unwrap_or("");
    match rest.find('/') {
        Some(idx) => &rest[..idx],
        None => rest,
    }
}

async fn handle_list(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let query = AdminUserListQuery::from_message(msg);

    let mut filters = vec![Filter {
        field: "deleted_at".to_string(),
        operator: FilterOp::IsNull,
        value: serde_json::Value::Null,
    }];

    if let Some(search) = &query.search {
        filters.push(Filter {
            field: "email".to_string(),
            operator: FilterOp::Like,
            value: serde_json::Value::String(format!("%{search}%")),
        });
    }

    let sort = vec![SortField {
        field: "created_at".to_string(),
        desc: true,
    }];

    match db::paginated_list(
        ctx,
        COLLECTION,
        i64::from(query.page),
        i64::from(query.page_size),
        filters,
        sort,
    )
    .await
    {
        Ok(result) => {
            // Bulk-enrich with roles via a single `In`-filter query (was N+1:
            // one `list_all` per row), then project each row onto the closed
            // `AdminUserView` field list. The projection is what keeps
            // `verification_token` (and any column a future migration adds) off
            // the wire — the previous code echoed the whole row and removed one
            // field by name.
            let user_ids: Vec<&str> = result.records.iter().map(|r| r.id.as_str()).collect();
            let roles_by_user = ops::fetch_roles(ctx, &user_ids).await;
            ok_json(&AdminUserListResponse::from_record_list(
                &result,
                &roles_by_user,
            ))
        }
        Err(e) => err_internal("Database error", e),
    }
}

async fn handle_get(ctx: &dyn Context, _msg: &Message, id: &str) -> OutputStream {
    if id.is_empty() {
        return err_bad_request("Missing user ID");
    }
    get_user(ctx, id).await
}

async fn get_user(ctx: &dyn Context, id: &str) -> OutputStream {
    match db::get(ctx, COLLECTION, id).await {
        Ok(record) => {
            // Get roles via the shared single-query helper.
            let roles = ops::fetch_roles(ctx, &[id])
                .await
                .remove(id)
                .unwrap_or_default();
            // Same projection as the list endpoint. This path used to emit a
            // third shape — the raw `{id, data: {…}}` record with `roles`
            // grafted on beside `data` rather than inside it — so the two read
            // paths disagreed about where a user's roles lived and both echoed
            // `verification_token`.
            ok_json(&AdminUserView::from_record(&record, roles))
        }
        Err(e) if e.code == ErrorCode::NotFound => err_not_found("User not found"),
        Err(e) => err_internal("Database error", e),
    }
}

async fn handle_update(
    ctx: &dyn Context,
    msg: &Message,
    id: &str,
    input: InputStream,
) -> OutputStream {
    if id.is_empty() {
        return err_bad_request("Missing user ID");
    }

    let raw = input.collect_to_bytes().await;
    let body: HashMap<String, serde_json::Value> = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    // The self-disable guard, safe-field whitelist, and audit-log write all
    // live in the shared ops layer so the SSR surface can't diverge.
    match ops::update_user_fields(ctx, msg, id, &body).await {
        Ok(record) => ok_json(&record),
        Err(out) => out,
    }
}

async fn handle_delete(ctx: &dyn Context, msg: &Message, id: &str) -> OutputStream {
    if id.is_empty() {
        return err_bad_request("Missing user ID");
    }

    // Self-delete guard, soft-delete, and audit-log write live in the shared
    // ops layer (the JSON path previously logged nothing).
    match ops::delete_user(ctx, msg, id).await {
        Ok(()) => ok_json(&serde_json::json!({"deleted": true})),
        Err(out) => out,
    }
}

#[cfg(test)]
mod tests {
    use wafer_core::clients::database as db;

    use super::*;
    use crate::test_support::{admin_msg, output_json, TestContext};

    /// Seed one user row carrying every column the table has, including the
    /// two the API must never publish.
    async fn seed_user(ctx: &dyn Context) -> String {
        let mut data = crate::util::json_map(serde_json::json!({
            "email": "admin@example.com",
            "display_name": "Ada",
            "name": "Ada Lovelace",
            "avatar_url": serde_json::Value::Null,
            "role": "user",
            "email_verified": 1,
            "disabled": 0,
            "last_login_at": serde_json::Value::Null,
            "deleted_at": serde_json::Value::Null,
            // The two columns the untyped handler used to echo.
            "verification_token": "3d1f0ac0deadbeef",
            "last_verification_sent": "2026-08-01T00:00:00Z",
            "auth_version": 7,
        }));
        crate::util::stamp_created(&mut data);
        db::create(ctx, COLLECTION, data)
            .await
            .expect("seed user")
            .id
    }

    async fn users_ctx() -> TestContext {
        let ctx = TestContext::new().await;
        // Admin first: the migration runner records its state in
        // `block_settings`, which the admin schema creates.
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");
        crate::blocks::auth::migrations::apply(&ctx)
            .await
            .expect("apply auth migrations");
        ctx
    }

    /// The field set the endpoint publishes is exactly the contract's, no more.
    ///
    /// This is the assertion the `/openapi.json` schema rests on: the schema is
    /// derived from `AdminUserView`, so it is only true while the handler emits
    /// that type's fields and nothing else. Before the projection existed, the
    /// handler echoed the whole row, and this test's `verification_token` would
    /// have been on the wire.
    #[tokio::test]
    async fn list_publishes_exactly_the_contract_fields() {
        let ctx = users_ctx().await;
        seed_user(&ctx).await;

        let body =
            output_json(handle_list(&ctx, &admin_msg("retrieve", "/admin/users")).await).await;

        let row = body["records"][0]
            .as_object()
            .expect("one user record on the wire");
        let mut got: Vec<&str> = row.keys().map(String::as_str).collect();
        got.sort_unstable();
        let mut want = vec![
            "avatar_url",
            "created_at",
            "deleted_at",
            "disabled",
            "display_name",
            "email",
            "email_verified",
            "id",
            "last_login_at",
            "name",
            "role",
            "roles",
            "updated_at",
        ];
        want.sort_unstable();
        assert_eq!(
            got, want,
            "the wire field set must equal AdminUserView's, or the published \
             schema describes something the handler does not emit"
        );

        // The envelope is unchanged from the untyped `RecordList` it replaced.
        assert!(body["total_count"].is_i64());
        assert!(body["page"].is_i64());
        assert!(body["page_size"].is_i64());
    }

    /// No credential material reaches the wire, checked against the serialized
    /// body rather than a field list so a nested or renamed leak still trips it.
    #[tokio::test]
    async fn list_never_emits_credential_columns() {
        let ctx = users_ctx().await;
        seed_user(&ctx).await;

        let body =
            output_json(handle_list(&ctx, &admin_msg("retrieve", "/admin/users")).await).await;
        let raw = body.to_string();

        for leaked in ["verification_token", "3d1f0ac0deadbeef", "password_hash"] {
            assert!(
                !raw.contains(leaked),
                "GET /b/admin/api/users leaked `{leaked}`: {raw}"
            );
        }
    }

    /// `email_verified` / `disabled` are `INTEGER` on SQLite and `BOOLEAN` on
    /// Postgres. The schema says `boolean`, so the handler must normalize —
    /// echoing the column would make the schema false on one backend or the
    /// other.
    #[tokio::test]
    async fn list_normalizes_backend_dependent_column_types() {
        let ctx = users_ctx().await;
        seed_user(&ctx).await;

        let body =
            output_json(handle_list(&ctx, &admin_msg("retrieve", "/admin/users")).await).await;

        assert_eq!(
            body["records"][0]["email_verified"],
            serde_json::json!(true)
        );
        assert_eq!(body["records"][0]["disabled"], serde_json::json!(false));
    }

    /// Both read paths project through the same view. `GET /…/users/{id}` used
    /// to emit a third shape (`{id, data: {…}, roles: […]}`) that echoed the
    /// same columns the list did.
    #[tokio::test]
    async fn get_by_id_uses_the_same_projection_as_list() {
        let ctx = users_ctx().await;
        let id = seed_user(&ctx).await;

        let body = output_json(get_user(&ctx, &id).await).await;

        assert_eq!(body["id"], serde_json::json!(id));
        assert_eq!(body["email"], serde_json::json!("admin@example.com"));
        assert!(
            body.get("data").is_none(),
            "the record envelope must not survive the projection: {body}"
        );
        assert!(
            !body.to_string().contains("verification_token"),
            "GET /b/admin/api/users/{{id}} leaked verification_token: {body}"
        );
    }
}
