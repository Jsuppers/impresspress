use std::collections::HashMap;

use wafer_block::db::{ListOptions, SortField};
use wafer_core::clients::database as db;
use wafer_run::{context::Context, ErrorCode, InputStream, Message, OutputStream};

use super::{
    contracts::{
        AdminRoleDeleteResponse, AdminRoleListResponse, AdminRoleView, CreateRoleRequest,
        UpdateRoleRequest,
    },
    logs::audit_log,
};
use crate::{
    blocks::{auth::bump_auth_version, crud},
    http::{err_bad_request, err_conflict, err_forbidden, err_internal, err_not_found, ok_json},
    platform_state::user_roles::{self, Assigned},
    util::{json_map, RecordExt},
};

/// Role definitions table (one row per named role).
pub(crate) const ROLES_TABLE: &str = "impresspress__admin__roles";

/// Per-role permission rows (resource + actions tuples).
pub(crate) const PERMISSIONS_TABLE: &str = "impresspress__admin__permissions";

/// `GET /b/admin/api/iam/roles`.
pub(super) async fn handle_list_roles(ctx: &dyn Context) -> OutputStream {
    let opts = ListOptions {
        sort: vec![SortField {
            field: "name".to_string(),
            desc: false,
        }],
        limit: 1000,
        ..Default::default()
    };
    match db::list(ctx, ROLES_TABLE, &opts).await {
        // Project onto the closed `AdminRoleView` field list. Besides pinning
        // the published field set, this normalizes `permissions`: the column is
        // JSON-encoded TEXT that the SQLite backend sniffs back into an array
        // while Postgres/D1 return the raw string, so the untyped response had
        // no single shape a schema could describe.
        Ok(result) => ok_json(&AdminRoleListResponse::from_record_list(&result)),
        Err(e) => err_internal("Database error", e),
    }
}

/// `POST /b/admin/api/iam/roles`.
pub(super) async fn handle_create_role(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let raw = input.collect_to_bytes().await;
    let body: CreateRoleRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };
    // Validation, audit-log write, and the create live in the shared ops layer.
    match super::ops::create_role(
        ctx,
        msg,
        &body.name,
        body.description.as_deref(),
        body.permissions,
    )
    .await
    {
        // Same projection as the list: the ops layer returns the raw
        // `db::Record`, whose `{id, data: {…}}` envelope and backend-dependent
        // `permissions` encoding are exactly what `AdminRoleView` exists to
        // normalize away. Echoing it here would publish a second shape for
        // the same row.
        Ok(record) => ok_json(&AdminRoleView::from_record(&record)),
        Err(out) => out,
    }
}

/// `PATCH /b/admin/api/iam/roles/{id}`. `{id}` is read only as the route
/// table bound it.
pub(super) async fn handle_update_role(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let id = match crud::path_id(msg, "Role") {
        Ok(value) => value,
        Err(response) => return response,
    };

    let raw = input.collect_to_bytes().await;
    // Typed rather than a `HashMap` peek plus a per-branch key whitelist: the
    // published schema names exactly these three fields, and a `permissions`
    // that is not an array of strings is refused here instead of being
    // written to the column as whatever JSON arrived.
    let body: UpdateRoleRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    // Protect system roles from name changes (renaming "admin" would break
    // auth). The guard read must fail closed: success / not-found / infra
    // error are matched explicitly, and an infra error rejects the mutation
    // instead of silently falling through to the unprotected update below
    // (the old `if let Ok(existing) =` swallowed any non-success result,
    // including a transient DB error, as "not a system role").
    let existing = match db::get(ctx, ROLES_TABLE, id).await {
        Ok(record) => record,
        Err(e) if e.code == ErrorCode::NotFound => return err_not_found("Role not found"),
        Err(e) => return err_internal("Database error", e),
    };

    let is_system = existing.bool_field("is_system");
    if is_system && body.name.is_some() {
        return err_forbidden("Cannot rename system roles");
    }

    // `user_roles.role` stores the role NAME, not its id (`fetch_roles` reads
    // the `role` column; `handle_assign_role` writes `body.role`). A rename
    // that does not carry the grants with it leaves every assignment naming a
    // role that no longer exists, so the grant silently stops matching.
    let old_name = existing.str_field("name").to_string();
    let rename_to = body
        .name
        .as_deref()
        .filter(|name| *name != old_name)
        .map(str::to_string);

    let mut data = HashMap::new();
    if let Some(name) = body.name {
        data.insert("name".to_string(), serde_json::Value::String(name));
    }
    if let Some(description) = body.description {
        data.insert(
            "description".to_string(),
            serde_json::Value::String(description),
        );
    }
    if let Some(permissions) = body.permissions {
        data.insert("permissions".to_string(), serde_json::json!(permissions));
    }
    crate::util::stamp_updated(&mut data);
    let record = match db::update(ctx, ROLES_TABLE, id, data).await {
        Ok(record) => record,
        Err(e) if e.code == ErrorCode::NotFound => return err_not_found("Role not found"),
        Err(e) => return err_internal("Database error", e),
    };

    if let Some(new_name) = rename_to {
        if let Err(out) = cascade_role_rename(ctx, &old_name, &new_name).await {
            return out;
        }
    }

    audit_log(
        ctx,
        msg.user_id(),
        "role.update",
        &format!("roles/{id}"),
        msg.remote_addr(),
    )
    .await;

    // Same projection as list/create, for the same reason.
    ok_json(&AdminRoleView::from_record(&record))
}

/// Carry a role rename onto every `user_roles` row naming the old value, and
/// invalidate the affected users' access tokens.
///
/// The grants store the role name, so this is what keeps them pointing at the
/// role they were granted. The auth-version bump is the same reasoning as
/// `handle_assign_role`'s: the set of roles a live JWT was minted with has
/// changed, so it must stop authenticating.
async fn cascade_role_rename(
    ctx: &dyn Context,
    old_name: &str,
    new_name: &str,
) -> Result<(), OutputStream> {
    let grants = match user_roles::list_by_role(ctx, old_name).await {
        Ok(rows) => rows,
        Err(e) => return Err(err_internal("Database error", e)),
    };

    for grant in &grants {
        if let Err(e) = user_roles::rename_role(ctx, &grant.id, new_name).await {
            return Err(err_internal(
                "Role renamed but its grants did not follow",
                e,
            ));
        }

        let user_id = grant.user_id.as_str();
        if let Err(e) = bump_auth_version(ctx, user_id).await {
            tracing::error!(
                user_id = %user_id,
                error = %e,
                "role grant renamed but auth_version bump failed"
            );
            return Err(err_internal(
                "Role renamed but session invalidation failed",
                e,
            ));
        }
    }
    Ok(())
}

/// `DELETE /b/admin/api/iam/roles/{id}`. `{id}` is read only as the route
/// table bound it.
pub(super) async fn handle_delete_role(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let id = msg.var("id");
    // System-role guard, delete, and audit-log write live in the shared ops
    // layer (the JSON path previously logged nothing).
    match super::ops::delete_role(ctx, msg, id).await {
        Ok(()) => ok_json(&AdminRoleDeleteResponse { deleted: true }),
        Err(out) => out,
    }
}

/// `GET /b/admin/api/iam/permissions`.
pub(super) async fn handle_list_permissions(ctx: &dyn Context) -> OutputStream {
    match db::list_all(ctx, PERMISSIONS_TABLE, vec![]).await {
        Ok(records) => {
            let total_count = records.len() as i64;
            ok_json(&db::RecordList {
                records,
                total_count,
                page: 1,
                page_size: total_count,
            })
        }
        Err(e) => err_internal("Database error", e),
    }
}

/// `POST /b/admin/api/iam/permissions`.
pub(super) async fn handle_create_permission(
    ctx: &dyn Context,
    input: InputStream,
) -> OutputStream {
    #[derive(serde::Deserialize)]
    struct Req {
        name: String,
        resource: String,
        actions: Vec<String>,
    }
    let raw = input.collect_to_bytes().await;
    let body: Req = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };
    let mut data = json_map(serde_json::json!({
        "name": body.name,
        "resource": body.resource,
        "actions": body.actions
    }));
    crate::util::stamp_created(&mut data);
    match db::create(ctx, PERMISSIONS_TABLE, data).await {
        Ok(record) => ok_json(&record),
        Err(e) => err_internal("Database error", e),
    }
}

/// `DELETE /b/admin/api/iam/permissions/{id}`. `{id}` is read only as the
/// route table bound it.
pub(super) async fn handle_delete_permission(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let id = match crud::path_id(msg, "Permission") {
        Ok(value) => value,
        Err(response) => return response,
    };
    match db::delete(ctx, PERMISSIONS_TABLE, id).await {
        Ok(()) => ok_json(&serde_json::json!({"deleted": true})),
        Err(e) if e.code == ErrorCode::NotFound => err_not_found("Permission not found"),
        Err(e) => err_internal("Database error", e),
    }
}

/// `GET /b/admin/api/iam/user-roles`.
pub(super) async fn handle_list_user_roles(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let user_id = msg.query("user_id").to_string();
    let rows = if user_id.is_empty() {
        user_roles::list_all(ctx).await
    } else {
        user_roles::list_for_user(ctx, &user_id).await
    };
    match rows {
        Ok(rows) => {
            // Echoed in the `{id, data}` record envelope this endpoint has
            // always published; declared without a schema until it is typed.
            let records: Vec<db::Record> = rows
                .iter()
                .map(|row| db::Record {
                    id: row.id.clone(),
                    data: row.to_data(),
                })
                .collect();
            let total_count = records.len() as i64;
            ok_json(&db::RecordList {
                records,
                total_count,
                page: 1,
                page_size: total_count,
            })
        }
        Err(e) => err_internal("Database error", e),
    }
}

/// `POST /b/admin/api/iam/user-roles`.
pub(super) async fn handle_assign_role(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    #[derive(serde::Deserialize)]
    struct Req {
        user_id: String,
        role: String,
    }
    let raw = input.collect_to_bytes().await;
    let body: Req = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    let assigned = format!("users/{}/roles/{}", body.user_id, body.role);
    match user_roles::assign(ctx, &body.user_id, &body.role, msg.user_id()).await {
        Ok(Assigned::AlreadyAssigned) => err_conflict("Role already assigned to user"),
        Ok(Assigned::Created(row)) => {
            // P2c: a role grant is a security-relevant change — bump the
            // affected user's auth_version so any already-issued access JWT
            // (minted with the old role set) is invalidated instead of
            // keeping its stale `roles` claim until natural expiry. The row
            // has already landed, so a failed bump must not read as success.
            if let Err(e) = bump_auth_version(ctx, &body.user_id).await {
                tracing::error!(
                    user_id = %body.user_id,
                    error = %e,
                    "role assigned but auth_version bump failed"
                );
                return err_internal("Role assigned but session invalidation failed", e);
            }
            // Audit-log like every other admin mutation (this JSON path used to
            // write zero audit rows).
            audit_log(
                ctx,
                msg.user_id(),
                "user_role.assign",
                &assigned,
                msg.remote_addr(),
            )
            .await;
            // Echoed in the `{id, data}` record envelope this endpoint has
            // always published; declared without a schema until it is typed.
            ok_json(&db::Record {
                id: row.id.clone(),
                data: row.to_data(),
            })
        }
        Err(e) => err_internal("Database error", e),
    }
}

/// `DELETE /b/admin/api/iam/user-roles/{id}`. `{id}` is read only as the
/// route table bound it.
pub(super) async fn handle_remove_role(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let id = match crud::path_id(msg, "User-role") {
        Ok(value) => value,
        Err(response) => return response,
    };

    // Prevent admins from removing their own admin role (self-lockout).
    // Also captures the affected user id so a successful removal can bump
    // their auth_version (P2c) below.
    let role_user = match user_roles::get(ctx, id).await {
        Ok(Some(grant)) => {
            if grant.user_id == msg.user_id() && grant.role == "admin" {
                return err_bad_request("Cannot remove your own admin role");
            }
            grant.user_id
        }
        Ok(None) => {
            return err_not_found("User-role assignment not found");
        }
        Err(e) => {
            return err_internal("Database error", e);
        }
    };

    match user_roles::remove(ctx, id).await {
        Ok(()) => {
            // P2c: role removal (demotion) is exactly the change this
            // mechanism exists for — bump so a JWT minted with the removed
            // role stops authenticating as that role immediately rather
            // than at its natural expiry.
            if let Err(e) = bump_auth_version(ctx, &role_user).await {
                tracing::error!(
                    user_id = %role_user,
                    error = %e,
                    "role removed but auth_version bump failed"
                );
                return err_internal("Role removed but session invalidation failed", e);
            }
            audit_log(
                ctx,
                msg.user_id(),
                "user_role.remove",
                &format!("user_roles/{id}"),
                msg.remote_addr(),
            )
            .await;
            ok_json(&serde_json::json!({"deleted": true}))
        }
        Err(e) if e.code == ErrorCode::NotFound => err_not_found("User-role assignment not found"),
        Err(e) => err_internal("Database error", e),
    }
}

pub async fn seed_defaults(ctx: &dyn Context) {
    let count = db::count(ctx, ROLES_TABLE, &[]).await.unwrap_or(0);
    if count > 0 {
        return;
    }

    let now = crate::util::now_rfc3339();
    for (name, desc) in &[
        ("admin", "Full access to all resources"),
        ("user", "Standard user access"),
    ] {
        let data = json_map(serde_json::json!({
            "name": name,
            "description": desc,
            "is_system": true,
            "created_at": now,
            "permissions": []
        }));
        if let Err(e) = db::create(ctx, ROLES_TABLE, data).await {
            tracing::warn!("Failed to seed default role '{name}': {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use wafer_block::db::{Filter, FilterOp};
    use wafer_run::{BlockInfo, WaferError};

    use super::*;
    use crate::{
        blocks::admin::test_support::routed,
        test_support::{admin_msg, output_is_error, output_json, TestContext},
    };

    /// `PATCH /b/admin/api/iam/roles/{role_id}`, with `{id}` bound by the
    /// table the way it is on the wire.
    fn update_role_msg(role_id: &str) -> Message {
        routed(admin_msg(
            "update",
            &format!("/b/admin/api/iam/roles/{role_id}"),
        ))
    }

    /// Wraps a `TestContext` and turns every `db::get` call
    /// (`ServiceOp::DATABASE_GET`, wire kind `"database.get"`) into a
    /// simulated infra failure while every other database op (list, update,
    /// count, ...) passes through untouched. Used to reproduce "the DB read
    /// used for the system-role guard fails transiently" without needing a
    /// fake database backend — everything else in the fixture is the real
    /// in-memory SQLite `TestContext`.
    #[derive(Clone)]
    struct FailingGetContext {
        inner: TestContext,
    }

    #[async_trait]
    impl Context for FailingGetContext {
        fn check_resource_access(
            &self,
            resource: &str,
            resource_type: wafer_run::ResourceType,
            is_write: bool,
        ) -> Result<(), WaferError> {
            self.inner
                .check_resource_access(resource, resource_type, is_write)
        }

        async fn call_block(&self, name: &str, msg: Message, input: InputStream) -> OutputStream {
            if name == "wafer-run/database" && msg.action() == "database.get" {
                return OutputStream::error(WaferError::new(
                    ErrorCode::Internal,
                    "simulated database outage",
                ));
            }
            self.inner.call_block(name, msg, input).await
        }

        fn is_cancelled(&self) -> bool {
            self.inner.is_cancelled()
        }

        fn registered_blocks(&self) -> &[BlockInfo] {
            self.inner.registered_blocks()
        }

        fn config_get(&self, key: &str) -> Option<&str> {
            self.inner.config_get(key)
        }

        fn clone_arc(&self) -> Arc<dyn Context> {
            Arc::new(self.clone())
        }
    }

    /// The roles list publishes exactly `AdminRoleView`'s fields, and
    /// `permissions` arrives as an array of strings.
    ///
    /// The array is the part worth pinning: the column is JSON-encoded TEXT,
    /// and only the SQLite backend decodes it on read. Echoing the row would
    /// make the published `array of string` schema false on Postgres and D1,
    /// where the same column comes back as a string.
    #[tokio::test]
    async fn list_roles_publishes_exactly_the_contract_fields() {
        let ctx = TestContext::with_admin().await;
        let msg = crate::test_support::admin_msg("create", "/b/admin/api/iam/roles");
        let created = super::super::ops::create_role(
            &ctx,
            &msg,
            "editor",
            Some("Can edit content"),
            Some(vec!["posts.write".to_string(), "posts.read".to_string()]),
        )
        .await;
        assert!(created.is_ok(), "create role should succeed");

        let body = crate::test_support::output_json(handle_list_roles(&ctx).await).await;

        let editor = body["records"]
            .as_array()
            .expect("records array")
            .iter()
            .find(|r| r["name"] == serde_json::json!("editor"))
            .expect("the created role is listed");

        let mut got: Vec<&str> = editor
            .as_object()
            .expect("role object")
            .keys()
            .map(String::as_str)
            .collect();
        got.sort_unstable();
        assert_eq!(
            got,
            vec![
                "created_at",
                "description",
                "id",
                "is_system",
                "name",
                "permissions",
                "updated_at"
            ],
            "the wire field set must equal AdminRoleView's"
        );

        assert_eq!(
            editor["permissions"],
            serde_json::json!(["posts.write", "posts.read"]),
            "permissions must be an array of strings on every backend"
        );
        assert_eq!(editor["is_system"], serde_json::json!(false));
    }

    /// Seed a real system role (`is_system: true`) via the shared
    /// `seed_defaults` path and return its row id.
    async fn seed_system_role(ctx: &dyn Context) -> String {
        seed_defaults(ctx).await;
        let records = db::list_all(
            ctx,
            ROLES_TABLE,
            vec![Filter {
                field: "name".to_string(),
                operator: FilterOp::Equal,
                value: serde_json::json!("admin"),
            }],
        )
        .await
        .expect("list seeded admin role");
        records
            .into_iter()
            .next()
            .expect("admin role was seeded")
            .id
    }

    fn body_input(json: serde_json::Value) -> InputStream {
        InputStream::from_bytes(serde_json::to_vec(&json).unwrap())
    }

    /// The system-role guard on the delete path must fail closed, exactly as
    /// the update path does. A transient read error previously fell through
    /// the `if let Ok(role)` and deleted the row — and this endpoint is now
    /// declared, schema-bearing and agent-reachable.
    #[tokio::test]
    async fn delete_role_rejects_deletion_when_guard_read_errors() {
        let ctx = TestContext::with_admin().await;
        let role_id = seed_system_role(&ctx).await;
        let failing = FailingGetContext { inner: ctx };

        let out = super::super::ops::delete_role(
            &failing,
            &admin_msg("delete", "/b/admin/api/iam/roles"),
            &role_id,
        )
        .await;

        match out {
            Err(stream) => {
                assert!(
                    output_is_error(stream, "Internal").await,
                    "a failed guard read must be reported, not treated as \
                     'not a system role'"
                );
            }
            Ok(()) => panic!("delete succeeded while the system-role guard read was failing"),
        }

        // The row must still be there: the mutation must not have run.
        let still_there = db::get(&failing.inner, ROLES_TABLE, &role_id).await;
        assert!(
            still_there.is_ok(),
            "the system role was deleted despite the guard read failing"
        );
    }

    /// `user_roles.role` stores the role NAME, so renaming a role definition
    /// without cascading silently orphans every grant: the assignment rows
    /// keep naming a role that no longer exists, and every auth check that
    /// reads them stops matching.
    #[tokio::test]
    async fn update_role_rename_cascades_to_its_assignments() {
        let ctx = TestContext::with_admin().await;

        let created = output_json(
            handle_create_role(
                &ctx,
                &admin_msg("create", "/b/admin/api/iam/roles"),
                body_input(serde_json::json!({ "name": "editor", "permissions": ["posts.write"] })),
            )
            .await,
        )
        .await;
        let role_id = created["id"].as_str().expect("created role id").to_string();

        // Grant it to a user, the way the admin UI does.
        let assigned = handle_assign_role(
            &ctx,
            &admin_msg("create", "/b/admin/api/iam/user-roles"),
            body_input(serde_json::json!({ "user_id": "user_1", "role": "editor" })),
        )
        .await;
        assert!(
            assigned.collect_buffered().await.is_ok(),
            "seeding the role assignment must succeed"
        );

        let out = handle_update_role(
            &ctx,
            &update_role_msg(&role_id),
            body_input(serde_json::json!({ "name": "editor-v2" })),
        )
        .await;
        assert!(out.collect_buffered().await.is_ok(), "rename must succeed");

        let rows = user_roles::list_for_user(&ctx, "user_1")
            .await
            .expect("list assignments");
        let names: Vec<&str> = rows.iter().map(|r| r.role.as_str()).collect();
        assert_eq!(
            names,
            vec!["editor-v2"],
            "the grant must follow the rename, or it names a role that no \
             longer exists"
        );
    }

    /// Every other role mutation writes an audit row; a rename — which
    /// invalidates every grant naming the old value — wrote none.
    #[tokio::test]
    async fn update_role_writes_an_audit_row() {
        let ctx = TestContext::with_admin().await;
        let created = output_json(
            handle_create_role(
                &ctx,
                &admin_msg("create", "/b/admin/api/iam/roles"),
                body_input(serde_json::json!({ "name": "auditor", "permissions": [] })),
            )
            .await,
        )
        .await;
        let role_id = created["id"].as_str().expect("created role id").to_string();

        let out = handle_update_role(
            &ctx,
            &update_role_msg(&role_id),
            body_input(serde_json::json!({ "description": "reads everything" })),
        )
        .await;
        assert!(out.collect_buffered().await.is_ok(), "update must succeed");

        let rows = db::list_all(
            &ctx,
            super::super::logs::AUDIT_LOGS_TABLE,
            vec![Filter {
                field: "action".to_string(),
                operator: FilterOp::Equal,
                value: serde_json::json!("role.update"),
            }],
        )
        .await
        .expect("list audit rows");
        assert_eq!(rows.len(), 1, "a role update must leave an audit trail");
    }

    #[tokio::test]
    async fn update_role_rejects_mutation_when_guard_read_errors() {
        // Real system role exists in the DB (renaming it would break auth).
        let ctx = TestContext::with_admin().await;
        let role_id = seed_system_role(&ctx).await;
        let failing = FailingGetContext { inner: ctx };

        // Attempt to rename the system role while the protective guard read
        // (db::get) is failing. The mutation must be rejected — not silently
        // let through because the guard couldn't be evaluated.
        let out = handle_update_role(
            &failing,
            &update_role_msg(&role_id),
            body_input(serde_json::json!({"name": "renamed-admin"})),
        )
        .await;
        assert!(
            output_is_error(out, "Internal").await,
            "a guard-read infra error must reject the mutation (fail closed)"
        );

        // Verify no rename actually happened — `list` isn't intercepted, so
        // this reads the real row through the same context.
        let records = db::list_all(
            &failing,
            ROLES_TABLE,
            vec![Filter {
                field: "id".to_string(),
                operator: FilterOp::Equal,
                value: serde_json::json!(role_id),
            }],
        )
        .await
        .expect("list role after failed update");
        assert_eq!(
            records[0].str_field("name"),
            "admin",
            "system role name must be unchanged after a fail-closed rejection"
        );
    }

    #[tokio::test]
    async fn update_role_still_forbids_system_role_rename_on_success() {
        // Regression guard: the normal (non-erroring) guard-read path must
        // still block a rename of a real system role.
        let ctx = TestContext::with_admin().await;
        let role_id = seed_system_role(&ctx).await;

        let out = handle_update_role(
            &ctx,
            &update_role_msg(&role_id),
            body_input(serde_json::json!({"name": "renamed-admin"})),
        )
        .await;
        assert!(output_is_error(out, "PermissionDenied").await);
    }

    #[tokio::test]
    async fn update_role_missing_row_returns_not_found() {
        let ctx = TestContext::with_admin().await;
        let out = handle_update_role(
            &ctx,
            &update_role_msg("does-not-exist"),
            body_input(serde_json::json!({"description": "x"})),
        )
        .await;
        assert!(output_is_error(out, "NotFound").await);
    }

    #[tokio::test]
    async fn update_role_non_system_role_updates_normally() {
        let ctx = TestContext::with_admin().await;
        let data = json_map(serde_json::json!({
            "name": "editor",
            "description": "old",
            "is_system": false,
            "permissions": []
        }));
        let created = db::create(&ctx, ROLES_TABLE, data).await.unwrap();

        let out = handle_update_role(
            &ctx,
            &update_role_msg(&created.id),
            body_input(serde_json::json!({"name": "renamed-editor"})),
        )
        .await;
        let json = output_json(out).await;
        assert_eq!(json["name"], "renamed-editor");
        assert!(
            json.get("data").is_none(),
            "the record envelope must not survive the projection: {json}"
        );
    }

    /// Every field name a role write publishes, sorted — must equal what the
    /// list publishes, since all three go through `AdminRoleView`.
    fn role_fields(role: &serde_json::Value) -> Vec<&str> {
        let mut got: Vec<&str> = role
            .as_object()
            .expect("role object")
            .keys()
            .map(String::as_str)
            .collect();
        got.sort_unstable();
        got
    }

    const ROLE_VIEW_FIELDS: [&str; 7] = [
        "created_at",
        "description",
        "id",
        "is_system",
        "name",
        "permissions",
        "updated_at",
    ];

    /// `POST` used to `ok_json` the raw `db::Record` from `ops::create_role`
    /// — the `{id, data: {…}}` envelope with `permissions` in whatever
    /// encoding the backend returned. It must publish the list's projection.
    #[tokio::test]
    async fn create_role_publishes_the_list_projection() {
        let ctx = TestContext::with_admin().await;
        let out = handle_create_role(
            &ctx,
            &admin_msg("create", "/b/admin/api/iam/roles"),
            body_input(serde_json::json!({
                "name": "editor",
                "description": "Can edit content",
                "permissions": ["posts.write"]
            })),
        )
        .await;
        let role = output_json(out).await;

        assert_eq!(role_fields(&role), ROLE_VIEW_FIELDS);
        assert_eq!(role["name"], serde_json::json!("editor"));
        assert_eq!(role["permissions"], serde_json::json!(["posts.write"]));
        assert_eq!(role["is_system"], serde_json::json!(false));
    }

    /// `PATCH` publishes the same projection, and a `permissions` value the
    /// schema does not admit is refused rather than written.
    #[tokio::test]
    async fn update_role_publishes_the_list_projection_and_types_permissions() {
        let ctx = TestContext::with_admin().await;
        let created = output_json(
            handle_create_role(
                &ctx,
                &admin_msg("create", "/b/admin/api/iam/roles"),
                body_input(serde_json::json!({"name": "editor"})),
            )
            .await,
        )
        .await;
        let role_id = created["id"].as_str().unwrap();

        let updated = output_json(
            handle_update_role(
                &ctx,
                &update_role_msg(role_id),
                body_input(serde_json::json!({"permissions": ["posts.read", "posts.write"]})),
            )
            .await,
        )
        .await;
        assert_eq!(role_fields(&updated), ROLE_VIEW_FIELDS);
        assert_eq!(
            updated["permissions"],
            serde_json::json!(["posts.read", "posts.write"])
        );

        let out = handle_update_role(
            &ctx,
            &update_role_msg(role_id),
            body_input(serde_json::json!({"permissions": "posts.*"})),
        )
        .await;
        assert!(
            output_is_error(out, "InvalidArgument").await,
            "a permissions value that is not an array of strings must be refused"
        );
    }

    #[tokio::test]
    async fn delete_role_reports_deleted() {
        let ctx = TestContext::with_admin().await;
        let created = output_json(
            handle_create_role(
                &ctx,
                &admin_msg("create", "/b/admin/api/iam/roles"),
                body_input(serde_json::json!({"name": "editor"})),
            )
            .await,
        )
        .await;
        let msg = routed(admin_msg(
            "delete",
            &format!("/b/admin/api/iam/roles/{}", created["id"].as_str().unwrap()),
        ));

        let body = output_json(handle_delete_role(&ctx, &msg).await).await;
        assert_eq!(body, serde_json::json!({"deleted": true}));
    }

    /// P2c: assigning a role is a security-relevant grant — it must bump the
    /// target user's auth_version so an already-issued access JWT (minted
    /// with the old, smaller role set) is invalidated instead of keeping its
    /// stale `roles` claim until natural expiry.
    #[tokio::test]
    async fn assign_role_bumps_the_targets_auth_version() {
        use crate::blocks::auth::repo::users;

        let ctx = TestContext::with_auth().await;
        let uid = users::insert(
            &ctx,
            users::NewUser {
                email: "grantee@example.com".into(),
                display_name: "Grantee".into(),
                avatar_url: None,
                role: "user".into(),
                email_verified: false,
                verification_token_hash: None,
            },
        )
        .await
        .unwrap()
        .id;
        assert_eq!(users::auth_version(&ctx, &uid).await.unwrap(), 0);

        let msg = admin_msg("create", "/b/admin/api/iam/user-roles");
        let out = handle_assign_role(
            &ctx,
            &msg,
            body_input(serde_json::json!({"user_id": uid, "role": "editor"})),
        )
        .await;
        assert!(
            !output_is_error(out, "Internal").await,
            "assign must succeed"
        );

        assert_eq!(
            users::auth_version(&ctx, &uid).await.unwrap(),
            1,
            "assigning a role must bump the target user's auth_version"
        );
    }

    /// P2c: removing a role (demotion) is exactly the change auth_version
    /// exists to invalidate — an already-issued JWT minted with the removed
    /// role must stop working immediately, not at its natural expiry.
    #[tokio::test]
    async fn remove_role_bumps_the_targets_auth_version() {
        use crate::blocks::auth::repo::users;

        let ctx = TestContext::with_auth().await;
        let uid = users::insert(
            &ctx,
            users::NewUser {
                email: "demotee@example.com".into(),
                display_name: "Demotee".into(),
                avatar_url: None,
                role: "user".into(),
                email_verified: false,
                verification_token_hash: None,
            },
        )
        .await
        .unwrap()
        .id;

        let msg = admin_msg("create", "/b/admin/api/iam/user-roles");
        let assigned = output_json(
            handle_assign_role(
                &ctx,
                &msg,
                body_input(serde_json::json!({"user_id": uid, "role": "editor"})),
            )
            .await,
        )
        .await;
        let role_row_id = assigned["id"]
            .as_str()
            .expect("assign response carries the user_roles row id")
            .to_string();
        // The assign above already bumped once; capture that baseline so the
        // removal's OWN bump is what this test proves.
        let before_remove = users::auth_version(&ctx, &uid).await.unwrap();

        let remove_msg = routed(admin_msg(
            "delete",
            &format!("/b/admin/api/iam/user-roles/{role_row_id}"),
        ));
        let out = handle_remove_role(&ctx, &remove_msg).await;
        assert!(
            !output_is_error(out, "Internal").await,
            "remove must succeed"
        );

        assert_eq!(
            users::auth_version(&ctx, &uid).await.unwrap(),
            before_remove + 1,
            "removing a role must bump the target user's auth_version"
        );
    }
}
