mod contracts;
mod database;
mod iam;
mod logs;
pub mod migrations;
mod ops;
mod pages;
mod settings;
mod users;

pub(crate) use iam::{PERMISSIONS_TABLE, ROLES_TABLE};
pub(crate) use logs::{AUDIT_LOGS_TABLE, STORAGE_ACCESS_LOGS_TABLE};

/// Registered name of the admin block.
///
/// Mirror of [`crate::blocks::auth::AUTH_BLOCK_ID`] for callers that need to
/// reference the admin block by name without hardcoding the string (e.g.
/// `impresspress-cloudflare` initialises the admin block first so its migrations
/// have run before the runner seeds `auto_generate` secrets).
pub const ADMIN_BLOCK_ID: &str = "impresspress/admin";

use wafer_run::{
    context::Context, BlockInfo, ErrorCode, HttpMethod, InputStream, InstanceMode, Message,
    OutputStream,
};

use crate::{
    endpoint_match::{self, request_schema_of, response_schema_of, EndpointRoute},
    http::{err_bad_request, err_internal, err_not_found, ok_json},
    platform_state::{block_settings, request_logs, user_roles, variables, wrap_grants},
};

/// Path-parameter schema for the `/iam/roles/{id}` routes.
///
/// Hand-written rather than derived: the handlers read the id through
/// `msg.var("id")` as the table bound it, so a struct declared only to feed
/// `.path_params::<T>()` would have no runtime user — the same reasoning
/// `tickets` records.
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

/// Handler for one row of [`ROUTES`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    // ── JSON API, `/b/admin/api/...` ──
    ListUsersApi,
    GetUserApi,
    UpdateUserApi,
    DeleteUserApi,
    DatabaseInfoApi,
    DatabaseTablesApi,
    DatabaseColumnsApi,
    DatabaseQueryApi,
    ListRolesApi,
    CreateRoleApi,
    UpdateRoleApi,
    DeleteRoleApi,
    ListPermissionsApi,
    CreatePermissionApi,
    DeletePermissionApi,
    ListUserRolesApi,
    AssignRoleApi,
    RemoveRoleApi,
    AuditLogsApi,
    ListSettingsApi,
    ListSettingsFullApi,
    GetSettingApi,
    SetSettingApi,
    CreateSettingApi,
    DeleteSettingApi,
    ExtensionsApi,
    // ── Consolidated settings pages, `/b/admin/settings/...` ──
    SettingsRedirect,
    SettingsEmailPage,
    SettingsNetworkPage,
    SettingsVariablesPage,
    SettingsPermissionsPage,
    // ── htmx mutations and fragments, `/b/admin/...` ──
    UserDisable,
    UserEnable,
    UserDelete,
    CreateRole,
    DeleteRole,
    BlockDetail,
    BlockToggle,
    CreateVariable,
    EditVariableForm,
    UpdateVariable,
    NetworkInboundDetail,
    CreateWrapGrant,
    DeleteWrapGrant,
    SaveEmailSettings,
    DatabaseQuery,
    // ── SSR pages, `/b/admin/...` ──
    Dashboard,
    UsersPage,
    StoragePage,
    BlocksPage,
    DatabasePage,
    LogsPage,
    EmailRedirect,
    NetworkRedirect,
    VariablesRedirect,
    PermissionsRedirect,
    GrantsPage,
}

/// The block's HTTP surface: what `handle()` dispatches on and what
/// `info().endpoints` is generated from. Wire paths; `{id}`, `{key}` and
/// `{name}` are bound into `req.param.*` for the handlers' `msg.var` readers.
///
/// Every row is `admin`. That restates, rather than decides, the level the
/// central router already enforces: `routing.rs` gates the whole `/b/admin/`
/// prefix at `RouteAccess::Admin`, and
/// `route_to_block` applies the stricter of the prefix tier and the declared
/// level, so no path under this prefix has ever been reachable without the
/// `admin` role. No handler re-checks `is_admin`; the block relies on the
/// router, as it always has.
///
/// The four JSON reads with an `agent_tool` and the three role writes carry
/// schemas derived from [`contracts`]. They are the block's whole agent
/// surface, and reads by policy: a tool's `execute` runs in the visitor's
/// page with their session cookie and full ambient authority, and any text
/// the agent reads can steer it. `no_admin_write_is_an_agent_tool` enforces
/// this. The remaining JSON rows (users, permissions, user-roles, settings,
/// extensions) still echo raw rows and are declared without a schema until
/// they are typed; every other row answers an SSR page or fragment.
const ROUTES: &[EndpointRoute<Route>] = &[
    // ── JSON API ──
    EndpointRoute::admin(HttpMethod::Get, "/b/admin/api/users", Route::ListUsersApi)
        .summary("List users API")
        .query_params(request_schema_of::<contracts::AdminUserListQuery>)
        .output(response_schema_of::<contracts::AdminUserListResponse>)
        .agent_tool(
            "list_users",
            "List this site's user accounts — email, roles, \
             verification and disabled state — one page at a \
             time. Use it to answer questions about who has an \
             account or who holds a role. Read-only: it cannot \
             create, change or remove a user.",
        ),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/api/users/{id}",
        Route::GetUserApi,
    )
    .summary("Get user API"),
    EndpointRoute::admin(
        HttpMethod::Patch,
        "/b/admin/api/users/{id}",
        Route::UpdateUserApi,
    )
    .summary("Update user API"),
    EndpointRoute::admin(
        HttpMethod::Delete,
        "/b/admin/api/users/{id}",
        Route::DeleteUserApi,
    )
    .summary("Delete user API"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/api/database/info",
        Route::DatabaseInfoApi,
    )
    .summary("Database info API"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/api/database/tables",
        Route::DatabaseTablesApi,
    )
    .summary("List tables API"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/api/database/tables/{name}/columns",
        Route::DatabaseColumnsApi,
    )
    .summary("Table columns API"),
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/admin/api/database/query",
        Route::DatabaseQueryApi,
    )
    .summary("Run read-only SQL API"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/api/iam/roles",
        Route::ListRolesApi,
    )
    .summary("List roles API")
    .output(response_schema_of::<contracts::AdminRoleListResponse>)
    .agent_tool(
        "list_roles",
        "List the roles defined for this site and what each \
             one grants. Call it before answering what a role \
             permits, rather than assuming from its name. \
             Read-only: it cannot create or change a role.",
    ),
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/admin/api/iam/roles",
        Route::CreateRoleApi,
    )
    .summary("Create role API")
    .input(request_schema_of::<contracts::CreateRoleRequest>)
    .output(response_schema_of::<contracts::AdminRoleView>),
    // Matched on the `update` action, which both PUT and PATCH map to;
    // PATCH is what the SDK sends and what is declared.
    EndpointRoute::admin(
        HttpMethod::Patch,
        "/b/admin/api/iam/roles/{id}",
        Route::UpdateRoleApi,
    )
    .summary("Update role API")
    .path_params(role_id_path_schema)
    .input(request_schema_of::<contracts::UpdateRoleRequest>)
    .output(response_schema_of::<contracts::AdminRoleView>),
    EndpointRoute::admin(
        HttpMethod::Delete,
        "/b/admin/api/iam/roles/{id}",
        Route::DeleteRoleApi,
    )
    .summary("Delete role API")
    .path_params(role_id_path_schema)
    .output(response_schema_of::<contracts::AdminRoleDeleteResponse>),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/api/iam/permissions",
        Route::ListPermissionsApi,
    )
    .summary("List permissions API"),
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/admin/api/iam/permissions",
        Route::CreatePermissionApi,
    )
    .summary("Create permission API"),
    EndpointRoute::admin(
        HttpMethod::Delete,
        "/b/admin/api/iam/permissions/{id}",
        Route::DeletePermissionApi,
    )
    .summary("Delete permission API"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/api/iam/user-roles",
        Route::ListUserRolesApi,
    )
    .summary("List user-role assignments API"),
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/admin/api/iam/user-roles",
        Route::AssignRoleApi,
    )
    .summary("Assign role API"),
    EndpointRoute::admin(
        HttpMethod::Delete,
        "/b/admin/api/iam/user-roles/{id}",
        Route::RemoveRoleApi,
    )
    .summary("Remove role assignment API"),
    EndpointRoute::admin(HttpMethod::Get, "/b/admin/api/logs", Route::AuditLogsApi)
        .summary("Audit logs API")
        .query_params(request_schema_of::<contracts::AdminAuditLogListQuery>)
        .output(response_schema_of::<contracts::AdminAuditLogListResponse>)
        .agent_tool(
            "list_audit_log",
            "List recorded admin actions, newest first: which \
             admin changed which user, role or setting, when, \
             and from which client IP. Use it to answer what \
             changed on this site and who changed it. Read-only: \
             it cannot alter or remove an entry.",
        ),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/api/settings",
        Route::ListSettingsApi,
    )
    .summary("List variables API")
    .output(response_schema_of::<contracts::AdminSettingsResponse>)
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
    // Listed before `{key}`: dispatch takes the first matching row.
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/api/settings/all",
        Route::ListSettingsFullApi,
    )
    .summary("List variables with metadata API"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/api/settings/{key}",
        Route::GetSettingApi,
    )
    .summary("Get variable API"),
    EndpointRoute::admin(
        HttpMethod::Patch,
        "/b/admin/api/settings/{key}",
        Route::SetSettingApi,
    )
    .summary("Set variable API"),
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/admin/api/settings",
        Route::CreateSettingApi,
    )
    .summary("Create variable API"),
    EndpointRoute::admin(
        HttpMethod::Delete,
        "/b/admin/api/settings/{key}",
        Route::DeleteSettingApi,
    )
    .summary("Delete variable API"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/api/extensions",
        Route::ExtensionsApi,
    )
    .summary("List registered blocks API"),
    // ── Consolidated settings pages ──
    // The bare `/b/admin/settings` reaches this row through the matcher's
    // trailing-slash retry; `/b/admin/settings/email` does not, because the
    // literal empty final segment does not match `email`.
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/settings/",
        Route::SettingsRedirect,
    )
    .summary("Settings (redirects to the email tab)"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/settings/email",
        Route::SettingsEmailPage,
    )
    .summary("Email settings tab"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/settings/network",
        Route::SettingsNetworkPage,
    )
    .summary("Network settings tab"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/settings/variables",
        Route::SettingsVariablesPage,
    )
    .summary("Variables settings tab"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/settings/permissions",
        Route::SettingsPermissionsPage,
    )
    .summary("Permissions settings tab"),
    // ── htmx mutations and fragments ──
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/admin/users/{id}/disable",
        Route::UserDisable,
    )
    .summary("Disable user"),
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/admin/users/{id}/enable",
        Route::UserEnable,
    )
    .summary("Enable user"),
    EndpointRoute::admin(HttpMethod::Delete, "/b/admin/users/{id}", Route::UserDelete)
        .summary("Delete user"),
    EndpointRoute::admin(HttpMethod::Post, "/b/admin/iam/roles", Route::CreateRole)
        .summary("Create role (form)"),
    EndpointRoute::admin(
        HttpMethod::Delete,
        "/b/admin/iam/roles/{id}",
        Route::DeleteRole,
    )
    .summary("Delete role (form)"),
    // `{name}` is the `--`-encoded block name (`impresspress--files`), see
    // `pages::blocks::encode_block_name`.
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/blocks/{name}/detail",
        Route::BlockDetail,
    )
    .summary("Block detail fragment"),
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/admin/blocks/{name}/toggle",
        Route::BlockToggle,
    )
    .summary("Toggle block"),
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/admin/variables",
        Route::CreateVariable,
    )
    .summary("Create variable (form)"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/variables/{key}/edit",
        Route::EditVariableForm,
    )
    .summary("Edit variable form"),
    // The edit form sends PUT; both PUT and PATCH map to the `update`
    // action the matcher compares.
    EndpointRoute::admin(
        HttpMethod::Patch,
        "/b/admin/variables/{key}",
        Route::UpdateVariable,
    )
    .summary("Update variable (form)"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/network/detail/inbound",
        Route::NetworkInboundDetail,
    )
    .summary("Inbound request detail fragment"),
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/admin/grants/rules",
        Route::CreateWrapGrant,
    )
    .summary("Create WRAP grant"),
    EndpointRoute::admin(
        HttpMethod::Delete,
        "/b/admin/grants/rules/{id}",
        Route::DeleteWrapGrant,
    )
    .summary("Delete WRAP grant"),
    EndpointRoute::admin(HttpMethod::Post, "/b/admin/email", Route::SaveEmailSettings)
        .summary("Save email settings"),
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/admin/database/query",
        Route::DatabaseQuery,
    )
    .summary("Run read-only SQL (SSR)"),
    // ── SSR pages ──
    EndpointRoute::admin(HttpMethod::Get, "/b/admin/", Route::Dashboard).summary("Dashboard"),
    EndpointRoute::admin(HttpMethod::Get, "/b/admin/users", Route::UsersPage)
        .summary("User management"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/variables",
        Route::VariablesRedirect,
    )
    .summary("Config management"),
    EndpointRoute::admin(HttpMethod::Get, "/b/admin/blocks", Route::BlocksPage)
        .summary("Block management"),
    EndpointRoute::admin(HttpMethod::Get, "/b/admin/network", Route::NetworkRedirect)
        .summary("Network monitoring"),
    EndpointRoute::admin(HttpMethod::Get, "/b/admin/storage", Route::StoragePage)
        .summary("Storage isolation and access logs"),
    EndpointRoute::admin(HttpMethod::Get, "/b/admin/logs", Route::LogsPage)
        .summary("System and audit logs"),
    EndpointRoute::admin(HttpMethod::Get, "/b/admin/email", Route::EmailRedirect)
        .summary("Email settings"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/admin/permissions",
        Route::PermissionsRedirect,
    )
    .summary("Permissions management"),
    EndpointRoute::admin(HttpMethod::Get, "/b/admin/grants", Route::GrantsPage)
        .summary("WRAP grants management"),
    EndpointRoute::admin(HttpMethod::Get, "/b/admin/database", Route::DatabasePage)
        .summary("Database admin page"),
];

crate::impresspress_feature_block! {
    /// Admin panel: users, database, IAM, logs, settings (`impresspress/admin`).
    pub struct AdminBlock;
    name: "impresspress/admin",
    info: |_this| {
        use wafer_run::CollectionSchema;

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
                CollectionSchema::new(user_roles::TABLE),
                CollectionSchema::new(variables::TABLE),
                CollectionSchema::new(AUDIT_LOGS_TABLE),
                CollectionSchema::new(request_logs::TABLE),
                CollectionSchema::new(STORAGE_ACCESS_LOGS_TABLE),
                CollectionSchema::new(block_settings::TABLE),
                CollectionSchema::new(wrap_grants::TABLE),
            ])
            .grants(vec![
                wafer_run::ResourceGrant::read_write(super::auth::AUTH_BLOCK_ID, user_roles::TABLE),
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
                    user_roles::TABLE,
                ),
                // Every block may upsert its own migration state into block_settings.
                wafer_run::ResourceGrant::read_write("*", block_settings::TABLE),
                // Infrastructure logging: storage wrapper + pipeline write logs
                wafer_run::ResourceGrant::read_write("*", STORAGE_ACCESS_LOGS_TABLE),
                wafer_run::ResourceGrant::read_write("*", request_logs::TABLE),
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
            .endpoints(endpoint_match::declare(ROUTES))
    },
    handle: |_this, ctx, msg, input| {
        // Auth is enforced centrally by `route_to_block` from the `Admin`
        // prefix tier and each row's declared level (both `Admin`). The
        // matcher binds `{id}`, `{key}` and `{name}` into `req.param.*` for
        // the handlers' `msg.var` readers; nothing else in this block reads
        // a path.
        let Some(route) = endpoint_match::dispatch(&mut msg, ROUTES) else {
            return err_not_found("not found");
        };
        match route {
            // ── JSON API ──
            Route::ListUsersApi => users::handle_list(ctx, &msg).await,
            Route::GetUserApi => users::handle_get(ctx, &msg).await,
            Route::UpdateUserApi => users::handle_update(ctx, &msg, input).await,
            Route::DeleteUserApi => users::handle_delete(ctx, &msg).await,
            Route::DatabaseInfoApi => database::handle_info(ctx).await,
            Route::DatabaseTablesApi => database::handle_tables(ctx).await,
            Route::DatabaseColumnsApi => database::handle_columns(ctx, &msg).await,
            Route::DatabaseQueryApi => database::handle_query(ctx, input).await,
            Route::ListRolesApi => iam::handle_list_roles(ctx).await,
            Route::CreateRoleApi => iam::handle_create_role(ctx, &msg, input).await,
            Route::UpdateRoleApi => iam::handle_update_role(ctx, &msg, input).await,
            Route::DeleteRoleApi => iam::handle_delete_role(ctx, &msg).await,
            Route::ListPermissionsApi => iam::handle_list_permissions(ctx).await,
            Route::CreatePermissionApi => iam::handle_create_permission(ctx, input).await,
            Route::DeletePermissionApi => iam::handle_delete_permission(ctx, &msg).await,
            Route::ListUserRolesApi => iam::handle_list_user_roles(ctx, &msg).await,
            Route::AssignRoleApi => iam::handle_assign_role(ctx, &msg, input).await,
            Route::RemoveRoleApi => iam::handle_remove_role(ctx, &msg).await,
            Route::AuditLogsApi => logs::handle_list(ctx, &msg).await,
            Route::ListSettingsApi => settings::handle_list(ctx).await,
            Route::ListSettingsFullApi => settings::handle_list_full(ctx).await,
            Route::GetSettingApi => settings::handle_get(ctx, &msg).await,
            Route::SetSettingApi => settings::handle_set(ctx, &msg, input).await,
            Route::CreateSettingApi => settings::handle_create(ctx, &msg, input).await,
            Route::DeleteSettingApi => settings::handle_delete(ctx, &msg).await,
            Route::ExtensionsApi => handle_extensions(ctx),

            // ── Consolidated settings pages ──
            Route::SettingsRedirect => redirect_308("/b/admin/settings/email"),
            Route::SettingsEmailPage => pages::settings_page(ctx, &msg, "email").await,
            Route::SettingsNetworkPage => pages::settings_page(ctx, &msg, "network").await,
            Route::SettingsVariablesPage => pages::settings_page(ctx, &msg, "variables").await,
            Route::SettingsPermissionsPage => {
                pages::settings_page(ctx, &msg, "permissions").await
            }

            // ── htmx mutations and fragments ──
            Route::UserDisable => pages::handle_user_disable(ctx, &msg).await,
            Route::UserEnable => pages::handle_user_enable(ctx, &msg).await,
            Route::UserDelete => pages::handle_user_delete(ctx, &msg).await,
            Route::CreateRole => pages::handle_create_role(ctx, &msg, input).await,
            Route::DeleteRole => pages::handle_delete_role(ctx, &msg).await,
            Route::BlockDetail => pages::handle_block_detail(ctx, &msg).await,
            Route::BlockToggle => pages::handle_toggle_feature(ctx, &msg).await,
            Route::CreateVariable => pages::handle_create_variable(ctx, &msg, input).await,
            Route::EditVariableForm => pages::handle_edit_variable_form(ctx, &msg).await,
            Route::UpdateVariable => pages::handle_update_variable(ctx, &msg, input).await,
            Route::NetworkInboundDetail => pages::network_inbound_detail(ctx, &msg).await,
            Route::CreateWrapGrant => handle_create_wrap_grant(ctx, msg, input).await,
            Route::DeleteWrapGrant => handle_delete_wrap_grant(ctx, msg).await,
            Route::SaveEmailSettings => pages::handle_save_email_settings(ctx, &msg, input).await,
            Route::DatabaseQuery => pages::handle_database_query(ctx, &msg, input).await,

            // ── SSR pages ──
            Route::Dashboard => pages::dashboard(ctx, &msg).await,
            Route::UsersPage => pages::users_page(ctx, &msg).await,
            Route::StoragePage => pages::storage_page(ctx, &msg).await,
            Route::BlocksPage => pages::blocks_page(ctx, &msg).await,
            Route::DatabasePage => pages::database_page(ctx, &msg).await,
            Route::LogsPage => pages::logs_page(ctx, &msg).await,
            Route::EmailRedirect => redirect_308("/b/admin/settings/email"),
            Route::NetworkRedirect => redirect_308("/b/admin/settings/network"),
            Route::VariablesRedirect => redirect_308("/b/admin/settings/variables"),
            Route::PermissionsRedirect => {
                // Carry ?tab= as ?subtab= to preserve deep-links.
                let old_tab = msg.query("tab");
                if old_tab.is_empty() {
                    redirect_308("/b/admin/settings/permissions")
                } else {
                    redirect_308(&format!("/b/admin/settings/permissions?subtab={old_tab}"))
                }
            }
            Route::GrantsPage => pages::grants_page(ctx, &msg).await,
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

/// `GET /b/admin/api/extensions`: every registered block, as the SDK's
/// extensions service lists them.
fn handle_extensions(ctx: &dyn Context) -> OutputStream {
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

    // Persist first; only render the (now-updated) page and write the audit
    // event after a confirmed successful write. Previously `let _ =
    // db::create(..)` discarded the result, so a failed insert still
    // re-rendered the permissions page as if the grant had been added.
    let record = match wrap_grants::create(
        ctx,
        wrap_grants::NewWrapGrant {
            grantee,
            resource,
            write,
            resource_type,
            description,
        },
    )
    .await
    {
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

/// `DELETE /b/admin/grants/rules/{id}`. `{id}` is read only as the route
/// table bound it.
async fn handle_delete_wrap_grant(ctx: &dyn Context, mut msg: Message) -> OutputStream {
    let grant_id = msg.var("id").to_string();
    if grant_id.is_empty() {
        return err_bad_request("Missing grant ID");
    }
    let grant_id = grant_id.as_str();

    // Persist first; only render the page and write the audit event after a
    // confirmed successful delete. Previously `let _ = db::delete(..)`
    // discarded the result, so deleting an already-gone (or unwritable)
    // grant still re-rendered the page as a success.
    match wrap_grants::delete(ctx, grant_id).await {
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

    use super::{test_support::routed, *};
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
        let msg = admin_msg("create", "/b/admin/grants/rules");
        let form = "grantee=impresspress%2Ffiles&resource=impresspress__foo__bar&write=on";
        let input = InputStream::from_bytes(form.as_bytes().to_vec());

        let out = handle_create_wrap_grant(&ctx, msg, input).await;
        let _ = out
            .collect_buffered()
            .await
            .expect("a valid grant create must succeed, not error");

        let rows = wrap_grants::list(&ctx).await.expect("list wrap grants");
        assert_eq!(rows.len(), 1, "the grant must have been persisted");
        assert_eq!(rows[0].grantee, "impresspress/files");
        assert_eq!(audit_count(&ctx, "wrap_grant.create").await, 1);
    }

    /// The empty-field guard must reject before ever calling `db::create`, so
    /// it writes no row and no audit event (a real, if minor, instance of the
    /// same "failure must not look like success" contract — the previous
    /// code silently re-rendered the page as if nothing was wrong).
    #[tokio::test]
    async fn create_wrap_grant_rejects_empty_fields_without_persisting() {
        let ctx = TestContext::with_admin().await;
        let msg = admin_msg("create", "/b/admin/grants/rules");
        let input = InputStream::from_bytes(b"grantee=&resource=".to_vec());

        let out = handle_create_wrap_grant(&ctx, msg, input).await;
        assert!(
            output_is_error(out, "InvalidArgument").await,
            "empty grantee/resource must be rejected as a bad request"
        );

        let rows = wrap_grants::list(&ctx).await.expect("list wrap grants");
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
        let msg = routed(admin_msg("delete", "/b/admin/grants/rules/does-not-exist"));

        let out = handle_delete_wrap_grant(&ctx, msg).await;
        assert!(
            output_is_error(out, "NotFound").await,
            "deleting a nonexistent grant must surface NotFound, not a fabricated success"
        );
        assert_eq!(audit_count(&ctx, "wrap_grant.delete").await, 0);
    }

    #[tokio::test]
    async fn delete_wrap_grant_success_removes_row_and_audits() {
        let ctx = TestContext::with_admin().await;

        let record = wrap_grants::create(
            &ctx,
            wrap_grants::NewWrapGrant {
                grantee: "impresspress/files".to_string(),
                resource: "some_table".to_string(),
                write: false,
                resource_type: String::new(),
                description: String::new(),
            },
        )
        .await
        .expect("seed grant row");

        let msg = routed(admin_msg(
            "delete",
            &format!("/b/admin/grants/rules/{}", record.id),
        ));
        let out = handle_delete_wrap_grant(&ctx, msg).await;
        let _ = out
            .collect_buffered()
            .await
            .expect("delete of an existing grant must succeed");

        let rows = wrap_grants::list(&ctx).await.expect("list wrap grants");
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

    /// Two grants nothing reads through. The framework auth block reads its
    /// configuration through the config client
    /// (`config_client::get_default`), never the variables table; userportal
    /// reads the enablement map from the config snapshot
    /// (`features::BLOCK_SETTINGS_CONFIG_KEY`), never the block_settings
    /// table. The WRAP audit (`scripts/audit-wrap-grants.sh`), which also
    /// walks `platform_state` references, finds no such access from either
    /// block. A grant with no reader is standing permission for one to
    /// appear without review, so neither row is declared.
    #[test]
    fn admin_declares_no_grant_nothing_reads_through() {
        use crate::{
            blocks::auth::AUTH_BLOCK_ID,
            platform_state::{block_settings, variables},
        };

        let grants = AdminBlock::new().info().grants;
        let stale: Vec<_> = grants
            .iter()
            .filter(|g| {
                (g.grantee == AUTH_BLOCK_ID && g.resource == variables::TABLE)
                    || (g.grantee == "impresspress/userportal"
                        && g.resource == block_settings::TABLE)
            })
            .collect();
        assert!(
            stale.is_empty(),
            "admin must not grant a table nothing reads through: {stale:?}"
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
        use super::super::auth_ui::AUTH_UI_BLOCK_ID;
        use crate::platform_state::user_roles;

        let admin = AdminBlock::new();
        let grants = admin.info().grants;
        let table = user_roles::TABLE;

        let auth_ui_user_roles_grant = grants
            .iter()
            .find(|g| g.grantee == AUTH_UI_BLOCK_ID && g.resource == table);

        assert!(
            auth_ui_user_roles_grant.is_some_and(|g| g.write),
            "admin block must declare a read_write grant for {AUTH_UI_BLOCK_ID} on \
             {table} (login path) — found: {auth_ui_user_roles_grant:?}"
        );
    }
}

#[cfg(test)]
mod test_support {
    use wafer_run::Message;

    /// Run `msg` through the block's own route table so `{id}`, `{key}` and
    /// `{name}` are bound the way they are on the wire, then hand the message
    /// to a handler directly. Panics when no row matches: a test that sends
    /// an unroutable path would otherwise exercise the handler's "missing id"
    /// branch by accident.
    pub(super) fn routed(mut msg: Message) -> Message {
        let route = crate::endpoint_match::dispatch(&mut msg, super::ROUTES);
        assert!(
            route.is_some(),
            "no admin route matches {} {}",
            msg.action(),
            msg.path()
        );
        msg
    }
}

#[cfg(test)]
mod table_tests {
    use wafer_run::{AuthLevel, Block as _, Message};

    use super::*;
    use crate::{endpoint_match::endpoint_auth, test_support::anon_msg};

    /// `info().endpoints` is generated from `ROUTES`; nothing else declares
    /// an endpoint for this block.
    #[test]
    fn info_endpoints_come_from_the_table() {
        let declared = AdminBlock::new().info().endpoints;
        assert_eq!(declared.len(), ROUTES.len());
        for (ep, row) in declared.iter().zip(ROUTES) {
            assert_eq!(ep.method, row.method, "{}", row.template);
            assert_eq!(ep.path, row.template);
            assert_eq!(ep.auth, row.auth, "{}", row.template);
        }
    }

    fn resolve(action: &str, path: &str) -> (Option<Route>, Message) {
        let mut msg = anon_msg(action, path);
        let route = endpoint_match::dispatch(&mut msg, ROUTES);
        (route, msg)
    }

    /// `(action, path, expected route, bound variables)`.
    type Case = (
        &'static str,
        &'static str,
        Route,
        &'static [(&'static str, &'static str)],
    );

    /// Every `(action, wire path)` the block answered before the table: the
    /// arms of `route.rs`'s classifier and of the five sub-handler
    /// `match (action, path)` blocks over the normalized `/admin/...` form
    /// (plan Task 1). Kept as one list so the resolution test and the
    /// router-level test read the same inventory.
    fn served_paths() -> Vec<Case> {
        vec![
            // A. JSON API
            ("retrieve", "/b/admin/api/users", Route::ListUsersApi, &[]),
            (
                "retrieve",
                "/b/admin/api/users/u-1",
                Route::GetUserApi,
                &[("id", "u-1")],
            ),
            (
                "update",
                "/b/admin/api/users/u-1",
                Route::UpdateUserApi,
                &[("id", "u-1")],
            ),
            (
                "delete",
                "/b/admin/api/users/u-1",
                Route::DeleteUserApi,
                &[("id", "u-1")],
            ),
            (
                "retrieve",
                "/b/admin/api/database/info",
                Route::DatabaseInfoApi,
                &[],
            ),
            (
                "retrieve",
                "/b/admin/api/database/tables",
                Route::DatabaseTablesApi,
                &[],
            ),
            (
                "retrieve",
                "/b/admin/api/database/tables/impresspress__admin__roles/columns",
                Route::DatabaseColumnsApi,
                &[("name", "impresspress__admin__roles")],
            ),
            (
                "create",
                "/b/admin/api/database/query",
                Route::DatabaseQueryApi,
                &[],
            ),
            (
                "retrieve",
                "/b/admin/api/iam/roles",
                Route::ListRolesApi,
                &[],
            ),
            (
                "create",
                "/b/admin/api/iam/roles",
                Route::CreateRoleApi,
                &[],
            ),
            (
                "update",
                "/b/admin/api/iam/roles/r-1",
                Route::UpdateRoleApi,
                &[("id", "r-1")],
            ),
            (
                "delete",
                "/b/admin/api/iam/roles/r-1",
                Route::DeleteRoleApi,
                &[("id", "r-1")],
            ),
            (
                "retrieve",
                "/b/admin/api/iam/permissions",
                Route::ListPermissionsApi,
                &[],
            ),
            (
                "create",
                "/b/admin/api/iam/permissions",
                Route::CreatePermissionApi,
                &[],
            ),
            (
                "delete",
                "/b/admin/api/iam/permissions/p-1",
                Route::DeletePermissionApi,
                &[("id", "p-1")],
            ),
            (
                "retrieve",
                "/b/admin/api/iam/user-roles",
                Route::ListUserRolesApi,
                &[],
            ),
            (
                "create",
                "/b/admin/api/iam/user-roles",
                Route::AssignRoleApi,
                &[],
            ),
            (
                "delete",
                "/b/admin/api/iam/user-roles/ur-1",
                Route::RemoveRoleApi,
                &[("id", "ur-1")],
            ),
            ("retrieve", "/b/admin/api/logs", Route::AuditLogsApi, &[]),
            (
                "retrieve",
                "/b/admin/api/settings",
                Route::ListSettingsApi,
                &[],
            ),
            (
                "retrieve",
                "/b/admin/api/settings/all",
                Route::ListSettingsFullApi,
                &[],
            ),
            (
                "retrieve",
                "/b/admin/api/settings/WAFER_RUN_SHARED__APP_NAME",
                Route::GetSettingApi,
                &[("key", "WAFER_RUN_SHARED__APP_NAME")],
            ),
            (
                "update",
                "/b/admin/api/settings/WAFER_RUN_SHARED__APP_NAME",
                Route::SetSettingApi,
                &[("key", "WAFER_RUN_SHARED__APP_NAME")],
            ),
            (
                "create",
                "/b/admin/api/settings",
                Route::CreateSettingApi,
                &[],
            ),
            (
                "delete",
                "/b/admin/api/settings/MY_SETTING",
                Route::DeleteSettingApi,
                &[("key", "MY_SETTING")],
            ),
            (
                "retrieve",
                "/b/admin/api/extensions",
                Route::ExtensionsApi,
                &[],
            ),
            // B. Consolidated settings pages
            (
                "retrieve",
                "/b/admin/settings",
                Route::SettingsRedirect,
                &[],
            ),
            (
                "retrieve",
                "/b/admin/settings/",
                Route::SettingsRedirect,
                &[],
            ),
            (
                "retrieve",
                "/b/admin/settings/email",
                Route::SettingsEmailPage,
                &[],
            ),
            (
                "retrieve",
                "/b/admin/settings/network",
                Route::SettingsNetworkPage,
                &[],
            ),
            (
                "retrieve",
                "/b/admin/settings/variables",
                Route::SettingsVariablesPage,
                &[],
            ),
            (
                "retrieve",
                "/b/admin/settings/permissions",
                Route::SettingsPermissionsPage,
                &[],
            ),
            // C. htmx mutations and fragments
            (
                "create",
                "/b/admin/users/u-1/disable",
                Route::UserDisable,
                &[("id", "u-1")],
            ),
            (
                "create",
                "/b/admin/users/u-1/enable",
                Route::UserEnable,
                &[("id", "u-1")],
            ),
            (
                "delete",
                "/b/admin/users/u-1",
                Route::UserDelete,
                &[("id", "u-1")],
            ),
            ("create", "/b/admin/iam/roles", Route::CreateRole, &[]),
            (
                "delete",
                "/b/admin/iam/roles/r-1",
                Route::DeleteRole,
                &[("id", "r-1")],
            ),
            (
                "retrieve",
                "/b/admin/blocks/wafer-run--auth/detail",
                Route::BlockDetail,
                &[("name", "wafer-run--auth")],
            ),
            (
                "create",
                "/b/admin/blocks/wafer-run--auth/toggle",
                Route::BlockToggle,
                &[("name", "wafer-run--auth")],
            ),
            ("create", "/b/admin/variables", Route::CreateVariable, &[]),
            (
                "retrieve",
                "/b/admin/variables/WAFER_RUN_SHARED__APP_NAME/edit",
                Route::EditVariableForm,
                &[("key", "WAFER_RUN_SHARED__APP_NAME")],
            ),
            (
                "update",
                "/b/admin/variables/WAFER_RUN_SHARED__APP_NAME",
                Route::UpdateVariable,
                &[("key", "WAFER_RUN_SHARED__APP_NAME")],
            ),
            (
                "retrieve",
                "/b/admin/network/detail/inbound",
                Route::NetworkInboundDetail,
                &[],
            ),
            (
                "create",
                "/b/admin/grants/rules",
                Route::CreateWrapGrant,
                &[],
            ),
            (
                "delete",
                "/b/admin/grants/rules/g-1",
                Route::DeleteWrapGrant,
                &[("id", "g-1")],
            ),
            ("create", "/b/admin/email", Route::SaveEmailSettings, &[]),
            (
                "create",
                "/b/admin/database/query",
                Route::DatabaseQuery,
                &[],
            ),
            // D. SSR pages
            ("retrieve", "/b/admin", Route::Dashboard, &[]),
            ("retrieve", "/b/admin/", Route::Dashboard, &[]),
            ("retrieve", "/b/admin/users", Route::UsersPage, &[]),
            ("retrieve", "/b/admin/storage", Route::StoragePage, &[]),
            ("retrieve", "/b/admin/blocks", Route::BlocksPage, &[]),
            ("retrieve", "/b/admin/database", Route::DatabasePage, &[]),
            ("retrieve", "/b/admin/logs", Route::LogsPage, &[]),
            ("retrieve", "/b/admin/email", Route::EmailRedirect, &[]),
            ("retrieve", "/b/admin/network", Route::NetworkRedirect, &[]),
            (
                "retrieve",
                "/b/admin/variables",
                Route::VariablesRedirect,
                &[],
            ),
            (
                "retrieve",
                "/b/admin/permissions",
                Route::PermissionsRedirect,
                &[],
            ),
            ("retrieve", "/b/admin/grants", Route::GrantsPage, &[]),
        ]
    }

    /// Every path the classifier and the sub-handlers served resolves to a
    /// row, with the variable its handler reads bound. The bare index forms
    /// (`/b/admin`, `/b/admin/settings`) reach their rows through the
    /// matcher's trailing-slash retry.
    #[test]
    fn every_path_the_block_served_resolves_to_a_row() {
        for (action, path, expected, vars) in served_paths() {
            let (route, msg) = resolve(action, path);
            assert_eq!(route, Some(expected), "{action} {path}");
            for (name, value) in vars {
                assert_eq!(msg.var(name), *value, "{action} {path} binds {name}");
            }
        }
        // Every variant is reached by at least one inventory entry, so a row
        // whose path nobody lists cannot hide in the table.
        let reached: std::collections::BTreeSet<String> = served_paths()
            .iter()
            .map(|(_, _, route, _)| format!("{route:?}"))
            .collect();
        for row in ROUTES {
            assert!(
                reached.contains(&format!("{:?}", row.handler)),
                "{} {} is a row no served path reaches",
                row.method,
                row.template
            );
        }
    }

    /// The router gates the whole `/b/admin/` prefix at `Admin`
    /// (`routing.rs`), so every row restates that level, and `endpoint_auth`
    /// resolves every served path to `Admin` from the declaration alone.
    #[test]
    fn every_row_is_admin() {
        for row in ROUTES {
            assert_eq!(
                row.auth,
                AuthLevel::Admin,
                "{} {}",
                row.method,
                row.template
            );
        }
        let eps = AdminBlock::new().info().endpoints;
        for (action, path, _, _) in served_paths() {
            assert_eq!(
                endpoint_auth(&eps, action, path),
                Some(AuthLevel::Admin),
                "{action} {path}"
            );
        }
    }

    /// Paths that stay unmatched. Most the classifier already answered 404:
    /// removed APIs, the storage delegation, unknown tabs, and the empty-id
    /// shapes the old `strip_prefix` guards refused. Two are deliberate
    /// narrowings, marked below: the classifier matched them by path alone,
    /// with no action gate, and the rows are method-specific.
    #[test]
    fn paths_the_classifier_refused_stay_unmatched() {
        for (action, path) in [
            ("retrieve", "/b/admin/api"),
            ("retrieve", "/b/admin/api/wafer"),
            ("retrieve", "/b/admin/api/custom-tables"),
            ("retrieve", "/b/admin/api/storage/buckets"),
            ("retrieve", "/b/admin/api/cloudstorage/shares"),
            ("retrieve", "/b/admin/api/cloudstorage"),
            ("retrieve", "/b/admin/api/system-logs"),
            ("retrieve", "/b/admin/api/whatever"),
            // Narrowed: the classifier served the block list for ANY method
            // on `/b/admin/api/extensions`; the row is GET-only.
            ("create", "/b/admin/api/extensions"),
            // Narrowed: the settings tier was action-agnostic (a POST rendered
            // the tab); the four tab rows and the redirect row are GET-only.
            ("create", "/b/admin/settings/email"),
            ("create", "/b/admin/settings"),
            ("retrieve", "/b/admin/settings/foobar"),
            ("create", "/b/admin/custom-blocks/install"),
            ("delete", "/b/admin/custom-blocks/impresspress--foo"),
            ("retrieve", "/b/admin/whatever"),
            ("create", "/b/admin/users//disable"),
            ("delete", "/b/admin/iam/roles/"),
            ("retrieve", "/b/other"),
            ("retrieve", "/"),
        ] {
            let (route, _) = resolve(action, path);
            assert_eq!(route, None, "{action} {path} must not resolve");
        }
    }
}

/// Every link an admin page emits must land on a declared endpoint. The
/// API-key revoke button posted to `/b/auth/api/api-keys/{id}/revoke`, a path
/// auth-ui never served, and nothing noticed: the pages spell their targets
/// by hand and no test compared them with a route table. This module renders
/// every page and fragment the block serves and resolves each `hx-*` URL (and
/// the network rows' `data-detail-url`) against the table of the block that
/// owns it, with the method the attribute implies.
#[cfg(test)]
mod page_link_tests {
    use std::collections::BTreeSet;

    use wafer_core::clients::database as db;
    use wafer_run::{Block as _, BlockCategory, BlockInfo, InputStream};

    use super::*;
    use crate::{
        blocks::{
            auth::repo::{api_keys, users},
            auth_ui::AuthUiBlock,
        },
        endpoint_match::endpoint_auth,
        test_support::{admin_msg, anon_msg, output_html, TestContext},
    };

    /// `(URL prefix in the rendered HTML, action it implies)`. htmx maps
    /// `hx-post` to POST (`create`), `hx-get` to GET (`retrieve`),
    /// `hx-patch`/`hx-put` to PATCH/PUT (both `update`), `hx-delete` to
    /// DELETE (`delete`). `data-detail-url` is fetched with GET by the
    /// network page's script. `fetch("` is the inline script the settings
    /// form emits for its save target (`ui/settings_form.rs`), always a POST;
    /// the URL is JSON-encoded there, so the double-quote form is exact, and
    /// the served `.js` assets never appear in rendered HTML, so this cannot
    /// over-trigger. A non-POST inline `fetch` needs its own entry.
    const LINK_ATTRS: &[(&str, &str)] = &[
        ("hx-get=\"", "retrieve"),
        ("hx-post=\"", "create"),
        ("hx-patch=\"", "update"),
        ("hx-put=\"", "update"),
        ("hx-delete=\"", "delete"),
        ("data-detail-url=\"", "retrieve"),
        ("fetch(\"", "create"),
    ];

    /// `(action, path)` for every link attribute in `html`, query string
    /// stripped and `&amp;` unescaped.
    fn links_in(html: &str) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        for (attr, action) in LINK_ATTRS {
            let mut rest = html;
            while let Some(pos) = rest.find(attr) {
                let after = &rest[pos + attr.len()..];
                let end = after.find('"').expect("attribute value is terminated");
                let url = after[..end].replace("&amp;", "&");
                let path = url.split('?').next().unwrap_or("").to_string();
                out.push((*action, path));
                rest = &after[end..];
            }
        }
        out
    }

    struct Seeds {
        user_id: String,
        role_id: String,
        key_id: String,
        grant_id: String,
    }

    const PROBE_BLOCK: &str = "impresspress/probe";
    const PROBE_VARIABLE: &str = "PROBE_SETTING";

    /// The two blocks an admin page may link to, by the router prefix each
    /// owns (`routing.rs`); a link anywhere else is a new decision.
    const ADMIN_PREFIX: &str = "/b/admin/";
    const AUTH_UI_PREFIX: &str = "/b/auth/";

    /// One row behind every per-record control the pages render: a user
    /// (enable/disable/delete), a custom role (delete), an active API key
    /// (revoke), a variable (edit), a WRAP grant (delete), a request-log
    /// row (network detail) and a Feature block (detail, toggle).
    async fn seeded_ctx() -> (TestContext, Seeds) {
        let mut ctx = TestContext::with_auth().await;
        let user = users::insert(
            &ctx,
            users::NewUser {
                email: "member@example.com".into(),
                display_name: "Member".into(),
                avatar_url: None,
                role: "user".into(),
            },
        )
        .await
        .expect("seed user");
        let key = api_keys::insert(
            &ctx,
            api_keys::NewApiKey {
                user_id: &user.id,
                name: "ci",
                key_hash: "hash-1",
                key_prefix: "ipk_abc",
                expires_at: None,
            },
        )
        .await
        .expect("seed api key");
        let role = db::create(
            &ctx,
            ROLES_TABLE,
            crate::util::json_map(serde_json::json!({
                "name": "editor",
                "description": "",
                "is_system": 0,
                "permissions": "[]",
                "created_at": crate::util::now_rfc3339(),
                "updated_at": crate::util::now_rfc3339(),
            })),
        )
        .await
        .expect("seed role");
        variables::insert(
            &ctx,
            variables::NewVariable {
                key: PROBE_VARIABLE.to_string(),
                value: "1".to_string(),
                name: PROBE_VARIABLE.to_string(),
                description: String::new(),
                warning: String::new(),
                sensitive: false,
                updated_by: String::new(),
                block: variables::block_for_key(PROBE_VARIABLE),
            },
        )
        .await
        .expect("seed variable");
        let grant = wrap_grants::create(
            &ctx,
            wrap_grants::NewWrapGrant {
                grantee: "impresspress/probe".to_string(),
                resource: "impresspress__probe__things".to_string(),
                write: false,
                resource_type: String::new(),
                description: String::new(),
            },
        )
        .await
        .expect("seed grant");
        request_logs::insert(
            &ctx,
            &request_logs::NewRequestLog {
                method: "GET",
                path: "/probe",
                status_label: "OK",
                status_code: 200,
                error_message: "",
                duration_ms: 5,
                client_ip: "203.0.113.7",
                user_id: "",
            },
        )
        .await
        .expect("seed request log");
        // `can_disable` is what makes the detail fragment render the toggle.
        ctx.register_block_info(
            PROBE_BLOCK,
            BlockInfo::new(PROBE_BLOCK, "0.0.1", "http-handler@v1", "probe")
                .category(BlockCategory::Feature)
                .can_disable(true),
        );
        (
            ctx,
            Seeds {
                user_id: user.id,
                role_id: role.id,
                key_id: key.id,
                grant_id: grant.id,
            },
        )
    }

    /// `(action, path, query parameters)` of one page render.
    type Page = (
        &'static str,
        &'static str,
        &'static [(&'static str, &'static str)],
    );

    /// Every page and fragment the block serves as HTML, with the query
    /// parameters that select each tab.
    const PAGES: &[Page] = &[
        ("retrieve", "/b/admin/", &[]),
        ("retrieve", "/b/admin/users", &[]),
        ("retrieve", "/b/admin/users", &[("tab", "roles")]),
        ("retrieve", "/b/admin/users", &[("tab", "api-keys")]),
        ("retrieve", "/b/admin/storage", &[]),
        ("retrieve", "/b/admin/blocks", &[]),
        (
            "retrieve",
            "/b/admin/blocks/impresspress--probe/detail",
            &[],
        ),
        ("retrieve", "/b/admin/database", &[]),
        ("retrieve", "/b/admin/database", &[("tab", "sql")]),
        ("retrieve", "/b/admin/logs", &[]),
        ("retrieve", "/b/admin/logs", &[("tab", "audit")]),
        ("retrieve", "/b/admin/settings/email", &[]),
        ("retrieve", "/b/admin/settings/network", &[]),
        (
            "retrieve",
            "/b/admin/network/detail/inbound",
            &[("method", "GET"), ("path", "/probe")],
        ),
        ("retrieve", "/b/admin/settings/variables", &[]),
        ("retrieve", "/b/admin/settings/variables", &[("tab", "all")]),
        ("retrieve", "/b/admin/variables/PROBE_SETTING/edit", &[]),
        ("retrieve", "/b/admin/settings/permissions", &[]),
        (
            "retrieve",
            "/b/admin/settings/permissions",
            &[("subtab", "database")],
        ),
        ("retrieve", "/b/admin/grants", &[]),
    ];

    #[tokio::test]
    async fn every_link_an_admin_page_emits_resolves_to_a_declared_row() {
        let (ctx, seeds) = seeded_ctx().await;
        let auth_endpoints = AuthUiBlock::new().info().endpoints;
        let block = AdminBlock::new();

        let mut collected: BTreeSet<(String, String)> = BTreeSet::new();
        for (action, path, query) in PAGES {
            let mut msg = admin_msg(action, path);
            for (name, value) in *query {
                msg.set_meta(format!("req.query.{name}"), *value);
            }
            let html = output_html(block.handle(&ctx, msg, InputStream::empty()).await).await;
            for (link_action, link_path) in links_in(&html) {
                if link_path.starts_with(ADMIN_PREFIX) {
                    assert!(
                        endpoint_match::dispatch(&mut anon_msg(link_action, &link_path), ROUTES)
                            .is_some(),
                        "{path} emits {link_action} {link_path}, which no admin row serves"
                    );
                } else if link_path.starts_with(AUTH_UI_PREFIX) {
                    assert!(
                        endpoint_auth(&auth_endpoints, link_action, &link_path).is_some(),
                        "{path} emits {link_action} {link_path}, which auth-ui does not declare"
                    );
                } else {
                    panic!("{path} emits {link_action} {link_path}: not an admin or auth-ui path");
                }
                collected.insert((link_action.to_string(), link_path));
            }
        }

        // The guard is only as good as what the pages rendered: each
        // per-record control must actually have been emitted for its seed.
        let expected = [
            (
                "create",
                format!("/b/admin/users/{}/disable", seeds.user_id),
            ),
            ("delete", format!("/b/admin/users/{}", seeds.user_id)),
            ("create", "/b/admin/iam/roles".to_string()),
            ("delete", format!("/b/admin/iam/roles/{}", seeds.role_id)),
            ("create", "/b/auth/api/api-keys".to_string()),
            ("update", format!("/b/auth/api/api-keys/{}", seeds.key_id)),
            ("retrieve", "/b/admin/storage".to_string()),
            (
                "retrieve",
                "/b/admin/blocks/impresspress--probe/detail".to_string(),
            ),
            (
                "create",
                "/b/admin/blocks/impresspress--probe/toggle".to_string(),
            ),
            ("create", "/b/admin/database/query".to_string()),
            ("retrieve", "/b/admin/network/detail/inbound".to_string()),
            ("create", "/b/admin/variables".to_string()),
            (
                "retrieve",
                format!("/b/admin/variables/{PROBE_VARIABLE}/edit"),
            ),
            ("update", format!("/b/admin/variables/{PROBE_VARIABLE}")),
            ("create", "/b/admin/grants/rules".to_string()),
            (
                "delete",
                format!("/b/admin/grants/rules/{}", seeds.grant_id),
            ),
        ];
        for (action, path) in expected {
            assert!(
                collected.contains(&(action.to_string(), path.clone())),
                "the pages must emit {action} {path}; collected: {collected:#?}"
            );
        }
    }
}
