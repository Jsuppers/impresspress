use maud::{html, Markup};
use wafer_block::db::{ListOptions, SortField};
use wafer_core::clients::database as db;
use wafer_run::{context::Context, InputStream, Message, OutputStream};

use super::{admin_page, crumb};
use crate::{
    blocks::{
        admin::{ops, ROLES_TABLE},
        auth::repo::{
            api_keys,
            users::{self, ActiveUserQuery, UserRow},
        },
    },
    http::ResponseBuilder,
    ui::{
        self,
        components::{self, pagination},
        icons,
        shell::Topbar,
        templates::{list_page, PageHeader},
        UserInfo,
    },
    util::{parse_form_body, RecordExt},
};

pub async fn users_page(ctx: &dyn Context, msg: &Message) -> OutputStream {
    // Still needed by the page body (the current admin's own row is
    // rendered differently); the shell loads its own copy.
    let user = UserInfo::from_message(msg);
    let tab = msg.query("tab");
    let active_tab = match tab {
        "roles" => "roles",
        "api-keys" => "api-keys",
        _ => "users",
    };

    let tabs_markup = components::tab_navigation(vec![
        components::Tab {
            active: active_tab == "users",
            href: "/b/admin/users",
            label: "Users",
            icon: Some(icons::users()),
        },
        components::Tab {
            active: active_tab == "roles",
            href: "/b/admin/users?tab=roles",
            label: "Roles",
            icon: Some(icons::shield()),
        },
        components::Tab {
            active: active_tab == "api-keys",
            href: "/b/admin/users?tab=api-keys",
            label: "API Keys",
            icon: Some(icons::key()),
        },
    ]);

    let current_uid = user
        .as_ref()
        .map(|u| u.id.as_str())
        .unwrap_or("")
        .to_string();
    let tab_content = html! {
        div #users-tab-content {
            @if active_tab == "users" {
                (users_tab(ctx, msg, &current_uid).await)
            } @else if active_tab == "roles" {
                div #iam-content { (roles_tab(ctx).await) }
            } @else {
                (api_keys_tab(ctx).await)
            }
        }
    };

    let body = list_page(
        PageHeader {
            title: "",
            subtitle: None,
            primary_action: None,
        },
        Some(tabs_markup),
        tab_content,
        None,
    );

    admin_page(
        ctx,
        msg,
        "Users",
        Topbar {
            crumbs: crumb("Users"),
            primary_action: None,
            subtitle: Some("Manage accounts, roles, and API keys"),
            show_palette: true,
        },
        body,
    )
    .await
}

/// Users tab content (table + search + pagination).
async fn users_tab(ctx: &dyn Context, msg: &Message, current_user_id: &str) -> Markup {
    let (page, page_size, _) = msg.pagination_params(20);
    let search = msg.query("search").to_string();

    // Both the filter (`deleted_at IS NULL`), the sort and the search shape
    // (email OR id) live in `users::list_active_page`, shared with the JSON
    // list endpoint. `total_count` is now the full matched count across all
    // pages, so the footer below paginates a search correctly instead of
    // reporting the in-page count as the total.
    let result = users::list_active_page(
        ctx,
        &ActiveUserQuery {
            page: page as i64,
            page_size: page_size as i64,
            search: (!search.is_empty()).then(|| search.clone()),
        },
    )
    .await;

    html! {
        div .filter-bar {
            (components::search_input_with_value("search", "Search by email or user ID...", "/b/admin/users", "#content", &search))
        }

        @match &result {
            Ok(list) => {
                (users_table(&list.rows, ctx, current_user_id).await)

                (pagination(list.page as u32, list.page_size as u32, list.total_count as u32, "/b/admin/users"))
            }
            Err(e) => {
                div .login-error { "Failed to load users: " (e) }
            }
        }
    }
}

/// Render the users table body. Async because it enriches each user with roles.
async fn users_table(records: &[UserRow], ctx: &dyn Context, current_user_id: &str) -> Markup {
    // Bulk-fetch all roles for the visible users in a single query (was N+1:
    // one `list_all` per row), via the shared `ops::fetch_roles` helper.
    let user_ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
    let user_roles = ops::fetch_roles(ctx, &user_ids).await;

    html! {
        div .table-container {
            table .table {
                thead {
                    tr {
                        th { "Email" }
                        th { "Roles" }
                        th { "Status" }
                        th { "Created" }
                        th { "Actions" }
                    }
                }
                tbody {
                    @if records.is_empty() {
                        tr {
                            td colspan="5" .text-center .text-muted .p-8 { "No users found" }
                        }
                    }
                    @for record in records {
                        @let roles: &[String] = user_roles.get(&record.id).map(Vec::as_slice).unwrap_or(&[]);
                        (single_user_row(record, roles, current_user_id))
                    }
                }
            }
        }
    }
}

/// Render one row of the users table. Shared between the multi-row table
/// renderer and `user_row_fragment` (htmx outerHTML swap target for the
/// enable/disable mutations).
///
/// `current_uid` is `""` when the caller is rendering a single-row update
/// fragment (no "(you)" affordance) — the mutation endpoints reject
/// self-disable before reaching this path.
fn single_user_row(record: &UserRow, roles: &[String], current_uid: &str) -> Markup {
    let email = record.email.as_str();
    let disabled = record.disabled;
    let created = record.created_at.as_str();
    let is_self = !current_uid.is_empty() && record.id == current_uid;
    html! {
        tr #{"user-row-" (record.id)} {
            td { (email) }
            td {
                @for role in roles {
                    span .badge .badge-primary .mr-1 { (role) }
                }
                @if roles.is_empty() {
                    span .text-muted { "\u{2014}" }
                }
            }
            td {
                @if disabled {
                    (components::status_badge("disabled"))
                } @else {
                    (components::status_badge("active"))
                }
            }
            td .text-muted .text-sm { (created.get(..10).unwrap_or(created)) }
            td {
                @if is_self {
                    span .text-muted .text-sm { "(you)" }
                } @else {
                    @if disabled {
                        button .btn .btn--sm .btn--success
                            hx-post={"/b/admin/users/" (record.id) "/enable"}
                            hx-target={"#user-row-" (record.id)}
                            hx-swap="outerHTML"
                            title="Enable user"
                        { "Enable" }
                    } @else {
                        button .btn .btn--sm .btn--secondary
                            hx-post={"/b/admin/users/" (record.id) "/disable"}
                            hx-target={"#user-row-" (record.id)}
                            hx-swap="outerHTML"
                            hx-confirm={"Disable " (email) "?"}
                            title="Disable user"
                        { "Disable" }
                    }
                    " "
                    button .btn .btn--sm .btn--danger
                        hx-delete={"/b/admin/users/" (record.id)}
                        hx-target={"#user-row-" (record.id)}
                        hx-swap="outerHTML"
                        hx-confirm={"Delete " (email) "? This cannot be undone."}
                        title="Delete user"
                    { (icons::trash()) }
                }
            }
        }
    }
}

/// Render a single user table row (used by enable/disable mutations).
async fn user_row_fragment(ctx: &dyn Context, user_id: &str) -> Markup {
    let Ok(Some(record)) = users::find_by_id(ctx, user_id).await else {
        return html! {};
    };

    // Single-user lookup via the shared roles helper (the `[one]` case).
    let roles = ops::fetch_roles(ctx, &[user_id])
        .await
        .remove(user_id)
        .unwrap_or_default();

    single_user_row(&record, &roles, "")
}

/// `POST /b/admin/users/{id}/disable`. `{id}` is read only as the route
/// table bound it.
pub async fn handle_user_disable(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let user_id = msg.var("id");
    // Self-disable guard, update, and audit-log write live in the shared ops
    // layer (single source of truth shared with the JSON surface).
    if let Err(out) = ops::set_user_disabled(ctx, msg, user_id, true).await {
        return out;
    }
    let row = user_row_fragment(ctx, user_id).await;
    ui::html_response_with_toast(row, "User disabled", "success")
}

/// `POST /b/admin/users/{id}/enable`. `{id}` is read only as the route
/// table bound it.
pub async fn handle_user_enable(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let user_id = msg.var("id");
    if let Err(out) = ops::set_user_disabled(ctx, msg, user_id, false).await {
        return out;
    }
    let row = user_row_fragment(ctx, user_id).await;
    ui::html_response_with_toast(row, "User enabled", "success")
}

/// `DELETE /b/admin/users/{id}`. `{id}` is read only as the route table
/// bound it.
pub async fn handle_user_delete(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let user_id = msg.var("id");
    // Self-delete guard, soft-delete, and audit-log write live in the shared
    // ops layer.
    if let Err(out) = ops::delete_user(ctx, msg, user_id).await {
        return out;
    }
    ui::html_response_with_toast(html! {}, "User deleted", "success")
}

/// POST /b/admin/iam/roles (create role from modal form)
pub async fn handle_create_role(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let bytes = input.collect_to_bytes().await;
    let body = parse_form_body(&bytes);

    let name = body.get("name").map(|s| s.as_str()).unwrap_or("");
    let description = body.get("description").map(|s| s.as_str());

    // Name-required guard, create, and audit-log write live in the shared ops
    // layer (single source of truth shared with the JSON surface).
    if let Err(out) = ops::create_role(ctx, msg, name, description, None).await {
        return out;
    }

    // Return the updated roles tab + close modal + toast
    let content = roles_tab(ctx).await;
    let trigger = r#"{"showToast":{"message":"Role created","type":"success"},"closeModal":{"id":"create-role"}}"#;
    ResponseBuilder::new()
        .set_header("HX-Trigger", trigger)
        .body(
            content.into_string().into_bytes(),
            "text/html; charset=utf-8",
        )
}

/// `DELETE /b/admin/iam/roles/{id}` (from the roles tab). `{id}` is read only
/// as the route table bound it.
pub async fn handle_delete_role(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let role_id = msg.var("id");
    // System-role guard, delete, and audit-log write live in the shared ops
    // layer.
    if let Err(out) = ops::delete_role(ctx, msg, role_id).await {
        return out;
    }
    let content = roles_tab(ctx).await;
    ui::html_response_with_toast(content, "Role deleted", "success")
}

async fn roles_tab(ctx: &dyn Context) -> Markup {
    let opts = ListOptions {
        sort: vec![SortField {
            field: "name".into(),
            desc: false,
        }],
        limit: 100,
        ..Default::default()
    };
    let result = db::list(ctx, ROLES_TABLE, &opts).await;

    html! {
        div .flex .items-center .justify-between .mb-4 {
            h3 .font-semibold { "Roles" }
            button .btn .btn--primary .btn--sm onclick="openModal('create-role')" {
                (icons::plus()) " Create Role"
            }
        }

        @match &result {
            Ok(list) => {
                div .table-container {
                    table .table {
                        thead {
                            tr {
                                th { "Name" }
                                th { "Description" }
                                th { "Type" }
                                th { "Actions" }
                            }
                        }
                        tbody {
                            @for record in &list.records {
                                @let name = record.str_field("name");
                                @let description = record.str_field("description");
                                @let is_system = record.bool_field("is_system");
                                tr {
                                    td .font-medium { (name) }
                                    td .text-muted .text-sm { (description) }
                                    td {
                                        @if is_system {
                                            span .badge .badge-info { "System" }
                                        } @else {
                                            span .badge .badge-primary { "Custom" }
                                        }
                                    }
                                    td {
                                        @if !is_system {
                                            button .btn .btn--sm .btn--danger
                                                hx-delete={"/b/admin/iam/roles/" (record.id)}
                                                hx-target="#iam-content"
                                                hx-confirm={"Delete role \"" (name) "\"?"}
                                            { (icons::trash()) }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                div .login-error { "Failed to load roles: " (e.message) }
            }
        }

        // Create role modal
        (components::modal("create-role", "Create Role", html! {
            form hx-post="/b/admin/iam/roles" hx-target="#iam-content" {
                div .form-group {
                    label .form-label .required for="role-name" { "Name" }
                    input .form-input type="text" #role-name name="name" placeholder="e.g. editor" required;
                }
                div .form-group {
                    label .form-label for="role-desc" { "Description" }
                    input .form-input type="text" #role-desc name="description" placeholder="Optional description";
                }
                div .form-actions {
                    button .btn .btn--secondary type="button" onclick="closeModal('create-role')" { "Cancel" }
                    button .btn .btn--primary type="submit" { "Create" }
                }
            }
        }))
    }
}

async fn api_keys_tab(ctx: &dyn Context) -> Markup {
    // Every key in the deployment, newest first — this tab is the operator's
    // view, not one account's (`api_keys::list_for_user` is what the
    // userportal and the auth-ui CRUD endpoints use).
    let result = api_keys::list_recent(ctx, 100).await;

    html! {
        div .flex .items-center .justify-between .mb-4 {
            h3 .font-semibold { "API Keys" }
            button .btn .btn--primary .btn--sm onclick="openModal('create-api-key')" {
                (icons::plus()) " Create API Key"
            }
        }

        @match &result {
            Ok(list) => {
                div .table-container {
                    table .table {
                        thead {
                            tr {
                                th { "Prefix" }
                                th { "Name" }
                                th { "User" }
                                th { "Created" }
                                th { "Status" }
                                th { "Actions" }
                            }
                        }
                        tbody {
                            @if list.is_empty() {
                                tr {
                                    td colspan="6" .text-center .text-muted .p-8 { "No API keys" }
                                }
                            }
                            @for record in list {
                                @let prefix = record.key_prefix.as_str();
                                @let name = record.name.as_str();
                                @let user_id = record.user_id.as_str();
                                @let created = record.created_at.as_str();
                                @let revoked = record.revoked_at.as_deref().unwrap_or("");
                                tr {
                                    td { code { (prefix) "..." } }
                                    td { (name) }
                                    td .text-muted .text-sm { (user_id.get(..8).unwrap_or(user_id)) }
                                    td .text-muted .text-sm { (created.get(..10).unwrap_or(created)) }
                                    td {
                                        @if revoked.is_empty() {
                                            (components::status_badge("active"))
                                        } @else {
                                            (components::status_badge("disabled"))
                                        }
                                    }
                                    td {
                                        @if revoked.is_empty() {
                                            // Revocation is auth-ui's
                                            // `PATCH /b/auth/api/api-keys/{id}`
                                            // (`Route::RevokeApiKey`); an admin
                                            // may revoke another user's key.
                                            button .btn .btn--sm .btn--secondary
                                                hx-patch={"/b/auth/api/api-keys/" (record.id)}
                                                hx-target="#users-tab-content"
                                                hx-confirm="Revoke this API key?"
                                            { "Revoke" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                div .login-error { "Failed to load API keys: " (e) }
            }
        }

        // Create API key modal
        (components::modal("create-api-key", "Create API Key", html! {
            form hx-post="/b/auth/api/api-keys" hx-target="#users-tab-content" {
                div .form-group {
                    label .form-label for="key-name" { "Name" }
                    input .form-input type="text" #key-name name="name" placeholder="e.g. CI/CD key" required;
                }
                div .form-actions {
                    button .btn .btn--secondary type="button" onclick="closeModal('create-api-key')" { "Cancel" }
                    button .btn .btn--primary type="submit" { "Create" }
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        blocks::auth::repo::{api_keys, users},
        test_support::{admin_msg, output_html, TestContext},
    };

    /// The API-keys tab must revoke through the route auth-ui declares
    /// (`PATCH /b/auth/api/api-keys/{id}`, `Route::RevokeApiKey`). The old
    /// control posted to `.../{id}/revoke`, a path no block ever served, so
    /// the button answered 404 in every deployment.
    #[tokio::test]
    async fn api_keys_tab_revokes_through_the_declared_patch_route() {
        let ctx = TestContext::with_auth().await;
        let owner = users::insert(
            &ctx,
            users::NewUser {
                email: "owner@example.com".into(),
                display_name: "Owner".into(),
                avatar_url: None,
                role: "user".into(),
                email_verified: false,
                verification_token_hash: None,
            },
        )
        .await
        .expect("seed user");
        let key = api_keys::insert(
            &ctx,
            api_keys::NewApiKey {
                user_id: &owner.id,
                name: "ci",
                key_hash: "hash-1",
                key_prefix: "ipk_abc",
                expires_at: None,
            },
        )
        .await
        .expect("seed api key");

        let mut msg = admin_msg("retrieve", "/b/admin/users");
        msg.set_meta("req.query.tab", "api-keys");
        let html = output_html(users_page(&ctx, &msg).await).await;

        let expected = format!("hx-patch=\"/b/auth/api/api-keys/{}\"", key.id);
        assert!(
            html.contains(&expected),
            "the revoke control must PATCH the declared api-key route: {html}"
        );
        assert!(
            !html.contains("/revoke"),
            "no admin control may target the unserved `/revoke` path: {html}"
        );
    }
}
