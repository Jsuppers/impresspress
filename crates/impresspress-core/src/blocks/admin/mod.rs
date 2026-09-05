mod contracts;
mod database;
mod iam;
mod logs;
pub mod migrations;
mod ops;
mod pages;
mod route;
mod settings;
mod users;

pub(crate) use iam::{PERMISSIONS_TABLE, ROLES_TABLE, USER_ROLES_TABLE};
pub(crate) use logs::{AUDIT_LOGS_TABLE, REQUEST_LOGS_TABLE, STORAGE_ACCESS_LOGS_TABLE};
pub use settings::{BLOCK_SETTINGS_TABLE, VARIABLES_TABLE};

/// Registered name of the admin block.
///
/// Mirror of [`crate::blocks::auth::AUTH_BLOCK_ID`] for callers that need to
/// reference the admin block by name without hardcoding the string (e.g.
/// `impresspress-cloudflare` initialises the admin block first so its migrations
/// have run before the runner seeds `auto_generate` secrets).
pub const ADMIN_BLOCK_ID: &str = "impresspress/admin";

/// WRAP grant rows (block-to-resource access tokens).
pub const WRAP_GRANTS_TABLE: &str = "impresspress__admin__wrap_grants";

use wafer_run::{
    context::Context, BlockEndpoint, BlockInfo, ErrorCode, InputStream, InstanceMode, Message,
    OutputStream,
};

use crate::http::{err_bad_request, err_internal, err_not_found, ok_json};

/// Path-parameter schema for the `/iam/roles/{id}` routes.
///
/// Hand-written rather than derived: the handlers read the id by stripping
/// the path prefix, so a struct declared only to feed `.path_params::<T>()`
/// would have no runtime user — the same reasoning `tickets` records.
fn role_id_path_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["id"],
        "properties": {
            "id": {
                "type": "string",
                "description": "Role id, as returned by the list endpoint (not the role name)."
            }
        }
    })
}

crate::impresspress_feature_block! {
    /// Admin panel: users, database, IAM, logs, settings (`impresspress/admin`).
    pub struct AdminBlock;
    name: "impresspress/admin",
    info: |_this| {
        use wafer_run::{AuthLevel, CollectionSchema};

        BlockInfo::new("impresspress/admin", "0.0.1", "http-handler@v1", "Admin panel: users, database, IAM, logs, settings")
            .instance_mode(InstanceMode::Singleton)
            .requires(vec![
                "wafer-run/database".into(),
                "wafer-run/config".into(),
                "wafer-run/crypto".into(),
            ])
            // Advisory table list — admin "Database tables" discovery + the
            // WRAP grant-UI read only `CollectionSchema::name`. The schema
            // itself (columns, indexes, FKs) lives solely in the block's
            // hand-authored `migrations/*.sqlite.sql` files (the single
            // source for both runtime `migrations::apply()` and the
            // Cloudflare D1 build).
            .collections(vec![
                CollectionSchema::new(ROLES_TABLE),
                CollectionSchema::new(PERMISSIONS_TABLE),
                CollectionSchema::new(USER_ROLES_TABLE),
                CollectionSchema::new(VARIABLES_TABLE),
                CollectionSchema::new(AUDIT_LOGS_TABLE),
                CollectionSchema::new(REQUEST_LOGS_TABLE),
                CollectionSchema::new(STORAGE_ACCESS_LOGS_TABLE),
                CollectionSchema::new(BLOCK_SETTINGS_TABLE),
                CollectionSchema::new(WRAP_GRANTS_TABLE),
            ])
            .grants(vec![
                wafer_run::ResourceGrant::read_write(super::auth::AUTH_BLOCK_ID, USER_ROLES_TABLE),
                // auth-ui's login/refresh/OAuth-callback handlers call the
                // shared `ensure_admin_role`/`get_user_roles` helpers
                // directly (not via the framework `wafer-run/auth`
                // service), so WRAP authorizes on their own node_id
                // ("impresspress/auth-ui"). Without this grant, admin login
                // in the native server hits PermissionDenied reading/
                // writing user_roles (surfaced as a real error by SB-3;
                // previously silently swallowed into an empty roles list).
                wafer_run::ResourceGrant::read_write(
                    super::auth_ui::AUTH_UI_BLOCK_ID,
                    USER_ROLES_TABLE,
                ),
                wafer_run::ResourceGrant::read(super::auth::AUTH_BLOCK_ID, VARIABLES_TABLE),
                wafer_run::ResourceGrant::read("impresspress/userportal", BLOCK_SETTINGS_TABLE),
                // Every block may upsert its own migration state into block_settings.
                wafer_run::ResourceGrant::read_write("*", BLOCK_SETTINGS_TABLE),
                // Infrastructure logging: storage wrapper + pipeline write logs
                wafer_run::ResourceGrant::read_write("*", STORAGE_ACCESS_LOGS_TABLE),
                wafer_run::ResourceGrant::read_write("*", REQUEST_LOGS_TABLE),
                // Default: allow all blocks to make outbound network requests.
                // Remove this grant via the admin UI to restrict network access.
                wafer_run::ResourceGrant::read("*", "*")
                    .typed(wafer_run::ResourceType::Network),
                // Default: allow all blocks to perform any crypto operation
                // (hash/compare_hash/sign/verify/random_bytes). The runtime
                // already isolates JWT signing keys per caller via HKDF
                // (SEC-016), so this wildcard does not let a block forge
                // another block's tokens. Tighten via the admin UI (e.g.
                // restrict sign/verify to specific blocks) if a deployment
                // wants per-op control.
                wafer_run::ResourceGrant::read_write("*", "*")
                    .typed(wafer_run::ResourceType::Crypto),
                // Wave 26 (c18) made Storage WRAP namespace-aware: every
                // block self-admits its own `{org}/{block}/*` namespace
                // via Rule 3 without any grant. The previous
                // `read_write("impresspress/files", "*")` grant the admin
                // block used to declare on behalf of the files block was
                // removed because the files block now reaches its own
                // storage namespace under the new self-admit rule.
                // Cross-block Storage grants are declared by the owning
                // block, the same way Db grants are.
            ])
            .category(wafer_run::BlockCategory::Feature)
            .description("Administration panel for managing users, roles, variables, blocks, and logs. Provides SSR dashboard with stats, user management with role assignment, IAM (roles and API keys), environment variables editor, block management with feature toggles, and system/audit log viewer.")
            .endpoints(vec![
                BlockEndpoint::get("/b/admin/").summary("Dashboard").auth(AuthLevel::Admin),
                BlockEndpoint::get("/b/admin/users").summary("User management").auth(AuthLevel::Admin),
                BlockEndpoint::get("/b/admin/variables").summary("Config management").auth(AuthLevel::Admin),
                BlockEndpoint::get("/b/admin/blocks").summary("Block management").auth(AuthLevel::Admin),
                BlockEndpoint::get("/b/admin/network").summary("Network monitoring").auth(AuthLevel::Admin),
                BlockEndpoint::get("/b/admin/storage").summary("Storage isolation and access logs").auth(AuthLevel::Admin),
                BlockEndpoint::get("/b/admin/logs").summary("System and audit logs").auth(AuthLevel::Admin),
                BlockEndpoint::get("/b/admin/email").summary("Email settings").auth(AuthLevel::Admin),
                BlockEndpoint::post("/b/admin/email").summary("Save email settings").auth(AuthLevel::Admin),
                BlockEndpoint::get("/b/admin/permissions").summary("Permissions management").auth(AuthLevel::Admin),
                BlockEndpoint::get("/b/admin/grants").summary("WRAP grants management").auth(AuthLevel::Admin),
                BlockEndpoint::get("/b/admin/database").summary("Database admin page").auth(AuthLevel::Admin),
                BlockEndpoint::post("/b/admin/database/query").summary("Run read-only SQL (SSR)").auth(AuthLevel::Admin),
                // The JSON API: four reads and the three role writes. Every
                // other endpoint above and below returns an SSR HTML page, so
                // it carries no schema and never becomes a tool. The remaining
                // JSON writes (users, permissions, user-roles, settings) still
                // echo raw rows and stay undeclared until they are typed.
                //
                // The four reads are the block's whole agent surface, and they
                // are reads by policy, not by accident: a tool's `execute`
                // runs in the visitor's page with their session cookie and
                // full ambient authority, and any text the agent reads can
                // steer it. `no_admin_write_is_an_agent_tool` enforces this.
                BlockEndpoint::get("/b/admin/api/users")
                    .summary("List users API")
                    .auth(AuthLevel::Admin)
                    .query_params::<contracts::AdminUserListQuery>()
                    .output::<contracts::AdminUserListResponse>()
                    .agent_tool(
                        "list_users",
                        "List this site's user accounts — email, roles, \
                         verification and disabled state — one page at a \
                         time. Use it to answer questions about who has an \
                         account or who holds a role. Read-only: it cannot \
                         create, change or remove a user.",
                    ),
                BlockEndpoint::get("/b/admin/api/iam/roles")
                    .summary("List roles API")
                    .auth(AuthLevel::Admin)
                    .output::<contracts::AdminRoleListResponse>()
                    .agent_tool(
                        "list_roles",
                        "List the roles defined for this site and what each \
                         one grants. Call it before answering what a role \
                         permits, rather than assuming from its name. \
                         Read-only: it cannot create or change a role.",
                    ),
                BlockEndpoint::post("/b/admin/api/iam/roles")
                    .summary("Create role API")
                    .auth(AuthLevel::Admin)
                    .input::<contracts::CreateRoleRequest>()
                    .output::<contracts::AdminRoleView>(),
                // `handle()` matches the `update` action, which both PUT and
                // PATCH map to; PATCH is what the SDK sends and what is
                // declared.
                BlockEndpoint::patch("/b/admin/api/iam/roles/{id}")
                    .summary("Update role API")
                    .auth(AuthLevel::Admin)
                    .path_params_schema(role_id_path_schema())
                    .input::<contracts::UpdateRoleRequest>()
                    .output::<contracts::AdminRoleView>(),
                BlockEndpoint::delete("/b/admin/api/iam/roles/{id}")
                    .summary("Delete role API")
                    .auth(AuthLevel::Admin)
                    .path_params_schema(role_id_path_schema())
                    .output::<contracts::AdminRoleDeleteResponse>(),
                BlockEndpoint::get("/b/admin/api/settings")
                    .summary("List variables API")
                    .auth(AuthLevel::Admin)
                    .output::<contracts::AdminSettingsResponse>()
                    .agent_tool(
                        "get_site_settings",
                        "Read this site's configuration variables as a list of \
                         entries, each with its key, its value, and whether \
                         that value is masked. A variable is masked when it \
                         carries the sensitive flag or its key ends in \
                         `_SECRET` or `_KEY`; a masked value reads \
                         `********` and is never the real one. Treat an \
                         unmasked value as readable configuration, not as \
                         proof it holds no secret. Read-only: it cannot \
                         change a setting.",
                    ),
                BlockEndpoint::get("/b/admin/api/logs")
                    .summary("Audit logs API")
                    .auth(AuthLevel::Admin)
                    .query_params::<contracts::AdminAuditLogListQuery>()
                    .output::<contracts::AdminAuditLogListResponse>()
                    .agent_tool(
                        "list_audit_log",
                        "List recorded admin actions, newest first: which \
                         admin changed which user, role or setting, when, \
                         and from which client IP. Use it to answer what \
                         changed on this site and who changed it. Read-only: \
                         it cannot alter or remove an entry.",
                    ),
            ])
    },
    handle: |_this, ctx, msg, input| {
        use route::AdminRoute;

        let path_owned = msg.path().to_string();
        let action_owned = msg.action().to_string();

        // The JSON sub-handlers (users::handle, database::handle, …) match on
        // the normalized `/admin/...` form of the path. That normalized path is
        // computed here and passed to them as an EXPLICIT argument; nothing in
        // this block rewrites `req.resource`.
        let api_norm = path_owned
            .strip_prefix("/b/admin/api")
            .map(|rest| format!("/admin{rest}"))
            .unwrap_or_else(|| path_owned.clone());

        match route::route(&path_owned, &action_owned) {
            // --- /b/admin/api/... ---
            AdminRoute::UsersApi => users::handle(ctx, &msg, &api_norm, input).await,
            AdminRoute::DatabaseApi => database::handle(ctx, &msg, &api_norm, input).await,
            AdminRoute::IamApi => iam::handle(ctx, &msg, &api_norm, input).await,
            AdminRoute::LogsApi => logs::handle(ctx, &msg, &api_norm).await,
            AdminRoute::SettingsApi => settings::handle(ctx, &msg, &api_norm, input).await,
            AdminRoute::ExtensionsApi => {
                let blocks: Vec<_> = ctx
                    .registered_blocks()
                    .iter()
                    .map(|b| {
                        serde_json::json!({
                            "name": b.name,
                            "version": b.version,
                            "interface": b.interface,
                            "summary": b.summary,
                            "enabled": true,
                        })
                    })
                    .collect();
                ok_json(&blocks)
            }
            AdminRoute::ApiNotFound => err_not_found("not found"),

            // --- /b/admin/settings/... ---
            AdminRoute::SettingsRedirect => redirect_308("/b/admin/settings/email"),
            AdminRoute::SettingsPage { tab } => pages::settings_page(ctx, &msg, tab).await,

            // --- /b/admin/... htmx mutations ---
            AdminRoute::UserDisable { user_id } => {
                pages::handle_user_disable(ctx, &msg, user_id).await
            }
            AdminRoute::UserEnable { user_id } => {
                pages::handle_user_enable(ctx, &msg, user_id).await
            }
            AdminRoute::UserDelete { user_id } => {
                pages::handle_user_delete(ctx, &msg, user_id).await
            }
            AdminRoute::CreateRole => pages::handle_create_role(ctx, &msg, input).await,
            AdminRoute::DeleteRole { role_id } => {
                pages::handle_delete_role(ctx, &msg, role_id).await
            }
            AdminRoute::BlockDetail { block_name } => {
                pages::handle_block_detail(ctx, &msg, &block_name).await
            }
            AdminRoute::BlockToggle { block_name } => {
                pages::handle_toggle_feature(ctx, &msg, &block_name).await
            }
            AdminRoute::CreateVariable => pages::handle_create_variable(ctx, &msg, input).await,
            AdminRoute::EditVariableForm { var_key } => {
                pages::handle_edit_variable_form(ctx, &msg, var_key).await
            }
            AdminRoute::UpdateVariable { var_key } => {
                pages::handle_update_variable(ctx, &msg, input, var_key).await
            }
            AdminRoute::NetworkInboundDetail => pages::network_inbound_detail(ctx, &msg).await,
            AdminRoute::CreateWrapGrant => handle_create_wrap_grant(ctx, msg, input).await,
            AdminRoute::DeleteWrapGrant { rule_id } => {
                handle_delete_wrap_grant(ctx, msg, rule_id).await
            }
            AdminRoute::SaveEmailSettings => {
                pages::handle_save_email_settings(ctx, &msg, input).await
            }
            AdminRoute::DatabaseQuery => pages::handle_database_query(ctx, &msg, input).await,

            // --- /b/admin/... SSR pages ---
            AdminRoute::Dashboard => pages::dashboard(ctx, &msg).await,
            AdminRoute::UsersPage => pages::users_page(ctx, &msg).await,
            AdminRoute::StoragePage => pages::storage_page(ctx, &msg).await,
            AdminRoute::BlocksPage => pages::blocks_page(ctx, &msg).await,
            AdminRoute::DatabasePage => pages::database_page(ctx, &msg).await,
            AdminRoute::LogsPage => pages::logs_page(ctx, &msg).await,
            AdminRoute::EmailRedirect => redirect_308("/b/admin/settings/email"),
            AdminRoute::NetworkRedirect => redirect_308("/b/admin/settings/network"),
            AdminRoute::VariablesRedirect => redirect_308("/b/admin/settings/variables"),
            AdminRoute::PermissionsRedirect => {
                // Carry ?tab= as ?subtab= to preserve deep-links.
                let old_tab = msg.query("tab");
                if old_tab.is_empty() {
                    redirect_308("/b/admin/settings/permissions")
                } else {
                    redirect_308(&format!("/b/admin/settings/permissions?subtab={old_tab}"))
                }
            }
            AdminRoute::GrantsPage => pages::grants_page(ctx, &msg).await,

            AdminRoute::NotFound => err_not_found("not found"),
        }
    },
    lifecycle: |_this, ctx, event| {
        crate::migration_helper::lifecycle_init(
            ctx,
            &event,
            "impresspress/admin",
            migrations::SQLITE_MIGRATIONS,
            migrations::POSTGRES_MIGRATIONS,
        )
        .await?;
        // Seed default roles/permissions + shared/default variables after the
        // schema is in place, only on Init.
        if matches!(event.event_type, wafer_run::LifecycleType::Init) {
            iam::seed_defaults(ctx).await;
            settings::seed_defaults(ctx).await;
        }
        Ok(())
    },
}

// ---------------------------------------------------------------------------
// Redirect helper
// ---------------------------------------------------------------------------

/// Build a 308 Permanent Redirect to `target`. Preserves method + body
/// per RFC 7538, so POST/PUT htmx requests redirect correctly.
fn redirect_308(target: &str) -> OutputStream {
    crate::http::ResponseBuilder::new()
        .status(308)
        .set_header("Location", target)
        .body(Vec::new(), "text/plain")
}

// ---------------------------------------------------------------------------
// WRAP grant handlers
// ---------------------------------------------------------------------------

use wafer_core::clients::database as db;

use crate::util::parse_form_body;

async fn handle_create_wrap_grant(
    ctx: &dyn Context,
    mut msg: Message,
    input: InputStream,
) -> OutputStream {
    let raw = input.collect_to_bytes().await;
    let form = parse_form_body(&raw);
    let grantee = form.get("grantee").cloned().unwrap_or_default();
    let resource = form.get("resource").cloned().unwrap_or_default();
    let write = form
        .get("write")
        .map(|v| v == "on" || v == "true" || v == "1")
        .unwrap_or(false);
    let resource_type = form.get("resource_type").cloned().unwrap_or_default();
    let description = form.get("description").cloned().unwrap_or_default();

    if grantee.is_empty() || resource.is_empty() {
        return err_bad_request("Grantee and resource are required");
    }

    let mut data = std::collections::HashMap::new();
    data.insert("grantee".into(), serde_json::json!(grantee));
    data.insert("resource".into(), serde_json::json!(resource));
    data.insert("write".into(), serde_json::json!(if write { 1 } else { 0 }));
    data.insert("resource_type".into(), serde_json::json!(resource_type));
    data.insert("description".into(), serde_json::json!(description));

    // Persist first; only render the (now-updated) page and write the audit
    // event after a confirmed successful write. Previously `let _ =
    // db::create(..)` discarded the result, so a failed insert still
    // re-rendered the permissions page as if the grant had been added.
    let record = match db::create(ctx, WRAP_GRANTS_TABLE, data).await {
        Ok(record) => record,
        Err(e) => return err_internal("Database error", e),
    };

    logs::audit_log(
        ctx,
        msg.user_id(),
        "wrap_grant.create",
        &format!("wrap_grants/{}", record.id),
        msg.remote_addr(),
    )
    .await;

    msg.set_meta("req.query.subtab", "database");
    pages::permissions_page(ctx, &msg).await
}

async fn handle_delete_wrap_grant(
    ctx: &dyn Context,
    mut msg: Message,
    grant_id: &str,
) -> OutputStream {
    // Persist first; only render the page and write the audit event after a
    // confirmed successful delete. Previously `let _ = db::delete(..)`
    // discarded the result, so deleting an already-gone (or unwritable)
    // grant still re-rendered the page as a success.
    match db::delete(ctx, WRAP_GRANTS_TABLE, grant_id).await {
        Ok(()) => {}
        Err(e) if e.code == ErrorCode::NotFound => return err_not_found("Grant not found"),
        Err(e) => return err_internal("Database error", e),
    }

    logs::audit_log(
        ctx,
        msg.user_id(),
        "wrap_grant.delete",
        &format!("wrap_grants/{grant_id}"),
        msg.remote_addr(),
    )
    .await;

    msg.set_meta("req.query.subtab", "database");
    pages::permissions_page(ctx, &msg).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use wafer_block::Block;
    use wafer_run::HttpMethod;

    use super::*;

    #[tokio::test]
    async fn redirect_308_sets_location_and_status() {
        let out = redirect_308("/b/admin/settings/email");
        let buf = out.collect_buffered().await.unwrap();
        let status = buf
            .meta
            .iter()
            .find(|e| e.key == "resp.status")
            .map(|e| e.value.as_str())
            .unwrap_or("");
        let location = buf
            .meta
            .iter()
            .find(|e| e.key == "resp.header.Location")
            .map(|e| e.value.as_str())
            .unwrap_or("");
        assert_eq!(status, "308");
        assert_eq!(location, "/b/admin/settings/email");
    }

    /// The four admin JSON reads are the block's whole agent surface.
    ///
    /// The design spec scopes admin to read tools: a tool's `execute` runs in
    /// the visitor's page with their session cookie and full ambient
    /// authority, and any text the agent reads — including user-generated
    /// content — can steer it. Reads are recoverable; an agent-invocable
    /// admin write is not, so none exists.
    #[test]
    fn admin_json_reads_are_exposed_as_agent_tools() {
        let info = AdminBlock::default().info();
        let named: std::collections::HashMap<&str, &str> = info
            .endpoints
            .iter()
            .filter_map(|ep| {
                ep.agent_tool
                    .as_ref()
                    .map(|t| (t.name.as_str(), ep.path.as_str()))
            })
            .collect();

        assert_eq!(named.get("list_users"), Some(&"/b/admin/api/users"));
        assert_eq!(named.get("list_roles"), Some(&"/b/admin/api/iam/roles"));
        assert_eq!(
            named.get("get_site_settings"),
            Some(&"/b/admin/api/settings")
        );
        assert_eq!(named.get("list_audit_log"), Some(&"/b/admin/api/logs"));
        assert_eq!(
            named.len(),
            4,
            "admin's agent surface is exactly the four JSON reads: {named:?}"
        );
    }

    /// Every admin tool must be a read. This is the structural half of the
    /// policy above: annotating a future admin write fails here rather than
    /// being caught in review.
    #[test]
    fn no_admin_write_is_an_agent_tool() {
        let info: BlockInfo = AdminBlock::default().info();
        for ep in &info.endpoints {
            if ep.is_agent_tool() {
                assert_eq!(
                    ep.method,
                    HttpMethod::Get,
                    "{} is a {:?} and must not be an agent tool: admin tools \
                     are read-only",
                    ep.path,
                    ep.method
                );
            }
        }
    }
}

/// Regression coverage for the swallowed-failure finding: WRAP grant
/// create/delete must check the persistence result instead of discarding it
/// (`let _ = db::create(..)` / `let _ = db::delete(..)`), and must only
/// write the audit-log row after a confirmed successful write.
#[cfg(test)]
mod wrap_grant_mutation_tests {
    use wafer_core::clients::database as db;
    use wafer_run::InputStream;

    use super::*;
    use crate::test_support::{admin_msg, output_is_error, TestContext};

    /// Count audit-log rows whose `action` matches.
    async fn audit_count(ctx: &dyn Context, action: &str) -> usize {
        db::list_all(
            ctx,
            AUDIT_LOGS_TABLE,
            vec![wafer_block::db::Filter {
                field: "action".to_string(),
                operator: wafer_block::db::FilterOp::Equal,
                value: serde_json::Value::String(action.to_string()),
            }],
        )
        .await
        .map(|rows| rows.len())
        .unwrap_or(0)
    }

    #[tokio::test]
    async fn create_wrap_grant_success_persists_and_audits() {
        let ctx = TestContext::with_admin().await;
        let msg = admin_msg("create", "/admin/grants/rules");
        let form = "grantee=impresspress%2Ffiles&resource=impresspress__foo__bar&write=on";
        let input = InputStream::from_bytes(form.as_bytes().to_vec());

        let out = handle_create_wrap_grant(&ctx, msg, input).await;
        let _ = out
            .collect_buffered()
            .await
            .expect("a valid grant create must succeed, not error");

        let rows = db::list_all(&ctx, WRAP_GRANTS_TABLE, vec![])
            .await
            .expect("list wrap grants");
        assert_eq!(rows.len(), 1, "the grant must have been persisted");
        assert_eq!(
            rows[0].data.get("grantee").and_then(|v| v.as_str()),
            Some("impresspress/files")
        );
        assert_eq!(audit_count(&ctx, "wrap_grant.create").await, 1);
    }

    /// The empty-field guard must reject before ever calling `db::create`, so
    /// it writes no row and no audit event (a real, if minor, instance of the
    /// same "failure must not look like success" contract — the previous
    /// code silently re-rendered the page as if nothing was wrong).
    #[tokio::test]
    async fn create_wrap_grant_rejects_empty_fields_without_persisting() {
        let ctx = TestContext::with_admin().await;
        let msg = admin_msg("create", "/admin/grants/rules");
        let input = InputStream::from_bytes(b"grantee=&resource=".to_vec());

        let out = handle_create_wrap_grant(&ctx, msg, input).await;
        assert!(
            output_is_error(out, "InvalidArgument").await,
            "empty grantee/resource must be rejected as a bad request"
        );

        let rows = db::list_all(&ctx, WRAP_GRANTS_TABLE, vec![])
            .await
            .expect("list wrap grants");
        assert!(rows.is_empty(), "no grant row must be persisted");
        assert_eq!(audit_count(&ctx, "wrap_grant.create").await, 0);
    }

    /// The core regression: deleting a grant that was never persisted (or is
    /// already gone) must return an error — not silently re-render the
    /// permissions page as if the delete had succeeded — and must not write
    /// a success audit row. Previously `let _ = db::delete(..)` discarded
    /// this `NotFound`.
    #[tokio::test]
    async fn delete_wrap_grant_missing_row_errors_without_audit() {
        let ctx = TestContext::with_admin().await;
        let msg = admin_msg("delete", "/admin/grants/rules/does-not-exist");

        let out = handle_delete_wrap_grant(&ctx, msg, "does-not-exist").await;
        assert!(
            output_is_error(out, "NotFound").await,
            "deleting a nonexistent grant must surface NotFound, not a fabricated success"
        );
        assert_eq!(audit_count(&ctx, "wrap_grant.delete").await, 0);
    }

    #[tokio::test]
    async fn delete_wrap_grant_success_removes_row_and_audits() {
        let ctx = TestContext::with_admin().await;

        let mut data = std::collections::HashMap::new();
        data.insert("grantee".into(), serde_json::json!("impresspress/files"));
        data.insert("resource".into(), serde_json::json!("some_table"));
        data.insert("write".into(), serde_json::json!(0));
        data.insert("resource_type".into(), serde_json::json!(""));
        data.insert("description".into(), serde_json::json!(""));
        let record = db::create(&ctx, WRAP_GRANTS_TABLE, data)
            .await
            .expect("seed grant row");

        let msg = admin_msg("delete", &format!("/admin/grants/rules/{}", record.id));
        let out = handle_delete_wrap_grant(&ctx, msg, &record.id).await;
        let _ = out
            .collect_buffered()
            .await
            .expect("delete of an existing grant must succeed");

        let rows = db::list_all(&ctx, WRAP_GRANTS_TABLE, vec![])
            .await
            .expect("list wrap grants");
        assert!(rows.is_empty(), "the grant row must have been removed");
        assert_eq!(audit_count(&ctx, "wrap_grant.delete").await, 1);
    }
}

/// The storage and cloud-storage JSON APIs used to be reached through this
/// block: `/b/admin/api/storage/...` and `/b/admin/api/cloudstorage/...` had
/// `req.resource` rewritten to a synthetic path and were forwarded to
/// `impresspress/files` via `call_block`. The files block declares them itself
/// now (`/b/storage/admin/api/...`, `/b/cloudstorage/admin/...`), so the old
/// wire paths must answer 404 from this block rather than reach files.
#[cfg(test)]
mod delegation_tests {
    use std::sync::Arc;

    use wafer_run::{Block, InputStream};

    use super::*;
    use crate::test_support::{admin_msg, output_is_error, TestContext};

    // Registered as `impresspress/files` and answers 200 to anything, so a
    // request this block still forwarded would come back a success and fail
    // the assertion for the right reason. (`TestContext::call_block` answers
    // `NotFound` for an unregistered block, and the real files block now 404s
    // the synthetic paths itself, so neither would tell forwarding apart from
    // refusing.)
    crate::impresspress_feature_block! {
        struct ProbeFilesBlock;
        name: "impresspress/files",
        info: |_this| BlockInfo::new("impresspress/files", "0.0.1", "http-handler@v1", "probe"),
        handle: |_this, _ctx, _msg, _input| ok_json(&serde_json::json!({ "forwarded": true })),
    }

    async fn ctx_with_probe_files_block() -> TestContext {
        let mut ctx = TestContext::with_admin().await;
        ctx.register_block(
            ProbeFilesBlock::BLOCK_NAME,
            Arc::new(ProbeFilesBlock::new()),
        );
        ctx
    }

    #[tokio::test]
    async fn old_delegated_paths_are_not_found_here() {
        let ctx = ctx_with_probe_files_block().await;
        for (action, path) in [
            ("retrieve", "/b/admin/api/cloudstorage/shares"),
            ("retrieve", "/b/admin/api/cloudstorage/access-logs"),
            ("retrieve", "/b/admin/api/cloudstorage/quotas"),
            ("update", "/b/admin/api/cloudstorage/quotas/u-1"),
            ("retrieve", "/b/admin/api/storage/buckets"),
            ("retrieve", "/b/admin/api/storage/stats"),
        ] {
            let out = AdminBlock::new()
                .handle(&ctx, admin_msg(action, path), InputStream::empty())
                .await;
            assert!(
                output_is_error(out, "NotFound").await,
                "{action} {path} must be NotFound from the admin block"
            );
        }
    }
}

#[cfg(test)]
mod grant_tests {
    use wafer_run::{Block, ResourceType};

    use super::AdminBlock;

    #[test]
    fn admin_block_no_longer_declares_storage_grant_for_files() {
        // Wave 26 (c18): Storage WRAP became namespace-aware. The files
        // block self-admits its own `impresspress/files/*` namespace via
        // Rule 3, so the admin block no longer needs to declare a typed
        // Storage grant on its behalf. This test pins the absence — if a
        // future change re-introduces the grant it's almost certainly a
        // regression from the c18 model.
        let admin = AdminBlock::new();
        let grants = admin.info().grants;

        let storage_grant_for_files = grants.iter().find(|g| {
            g.resource_type == Some(ResourceType::Storage) && g.grantee == "impresspress/files"
        });

        assert!(
            storage_grant_for_files.is_none(),
            "admin block must not declare a typed Storage grant for impresspress/files \
             — the files block self-admits its own namespace via WRAP Rule 3 (Wave 26 \
             / c18). Found: {storage_grant_for_files:?}"
        );
    }

    #[test]
    fn admin_block_grants_auth_ui_read_write_on_user_roles() {
        // auth-ui's login/refresh/OAuth-callback handlers call the shared
        // `ensure_admin_role`/`get_user_roles` helpers directly, so WRAP
        // authorizes on their own node_id ("impresspress/auth-ui"), not the
        // framework `wafer-run/auth` service's. Without this grant, admin
        // login in the native server hits PermissionDenied reading/writing
        // user_roles — this was previously masked because `get_user_roles`
        // swallowed the read error into an empty roles list; SB-3 made it
        // surface as a real error (500 on login), exposing this
        // pre-existing missing grant. Pin the grant's presence so it can't
        // silently regress again.
        use super::{super::auth_ui::AUTH_UI_BLOCK_ID, USER_ROLES_TABLE};

        let admin = AdminBlock::new();
        let grants = admin.info().grants;

        let auth_ui_user_roles_grant = grants
            .iter()
            .find(|g| g.grantee == AUTH_UI_BLOCK_ID && g.resource == USER_ROLES_TABLE);

        assert!(
            auth_ui_user_roles_grant.is_some_and(|g| g.write),
            "admin block must declare a read_write grant for {AUTH_UI_BLOCK_ID} on \
             {USER_ROLES_TABLE} (login path) — found: {auth_ui_user_roles_grant:?}"
        );
    }
}
