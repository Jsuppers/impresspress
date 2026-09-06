use maud::html;
use wafer_block::db::{ListOptions, SortField};
use wafer_core::clients::{config, database as db};
use wafer_run::{
    context::Context, BlockInfo, CollectionSchema, HttpMethod, InputStream, InstanceMode, Message,
    OutputStream,
};

use crate::{
    endpoint_match::{self, EndpointRoute},
    http::{err_forbidden, err_internal, err_not_found, ok_json},
    ui::{self, components, icons, settings_form},
    util::parse_form_body,
};

pub(crate) mod migrations;
// `pub(crate)`: `ui::sidebar`'s ICON_OPTIONS-coverage test reads
// `pages::admin_buttons::ICON_OPTIONS` to keep the icon dropdown and the
// `nav_icon` resolver in lockstep.
pub(crate) mod pages;

const TABLE: &str = "impresspress__userportal__buttons";

/// Handler for one row of [`ROUTES`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Dashboard,
    Profile,
    UpdateProfile,
    Sessions,
    RevokeSession,
    Security,
    Config,
    AdminSettingsPage,
    AdminSaveSettings,
    AdminButtonsPage,
    AdminCreateButton,
    AdminEditButtonForm,
    AdminUpdateButton,
    AdminDeleteButton,
}

/// The block's HTTP surface: what `handle()` dispatches on and what
/// `info().endpoints` is generated from. Wire paths; `{hash}` / `{id}` are
/// bound into `req.param.*` for the handlers' `msg.var` readers. The
/// `/admin/*` rows are declared `Admin` so the central router enforces the
/// tier; the block hand-checks nothing.
const ROUTES: &[EndpointRoute<Route>] = &[
    EndpointRoute::authenticated(HttpMethod::Get, "/b/userportal/", Route::Dashboard)
        .summary("Portal home (apps + orgs)"),
    EndpointRoute::authenticated(HttpMethod::Get, "/b/userportal/profile", Route::Profile)
        .summary("Profile page"),
    EndpointRoute::authenticated(
        HttpMethod::Post,
        "/b/userportal/update-profile",
        Route::UpdateProfile,
    )
    .summary("Update profile"),
    EndpointRoute::authenticated(HttpMethod::Get, "/b/userportal/sessions", Route::Sessions)
        .summary("Active sessions"),
    EndpointRoute::authenticated(
        HttpMethod::Delete,
        "/b/userportal/sessions/{hash}",
        Route::RevokeSession,
    )
    .summary("Revoke session"),
    EndpointRoute::authenticated(HttpMethod::Get, "/b/userportal/security", Route::Security)
        .summary("Account security"),
    EndpointRoute::public(HttpMethod::Get, "/b/userportal/config", Route::Config)
        .summary("Portal configuration"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/userportal/admin/settings",
        Route::AdminSettingsPage,
    )
    .summary("Branding settings"),
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/userportal/admin/settings",
        Route::AdminSaveSettings,
    )
    .summary("Save branding settings"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/userportal/admin/buttons",
        Route::AdminButtonsPage,
    )
    .summary("Manage portal buttons"),
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/userportal/admin/buttons",
        Route::AdminCreateButton,
    )
    .summary("Create button"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/userportal/admin/buttons/{id}/edit",
        Route::AdminEditButtonForm,
    )
    .summary("Edit button form"),
    EndpointRoute::admin(
        HttpMethod::Patch,
        "/b/userportal/admin/buttons/{id}",
        Route::AdminUpdateButton,
    )
    .summary("Update button"),
    EndpointRoute::admin(
        HttpMethod::Delete,
        "/b/userportal/admin/buttons/{id}",
        Route::AdminDeleteButton,
    )
    .summary("Delete button"),
];

crate::impresspress_feature_block! {
    /// User-facing portal dashboard + admin button config (`impresspress/userportal`).
    pub struct UserPortalBlock;
    name: "impresspress/userportal",
    info: |_this| {
        BlockInfo::new(
            "impresspress/userportal",
            "0.0.1",
            "http-handler@v1",
            "User profile and account hub with admin-configurable navigation buttons",
        )
        .instance_mode(InstanceMode::Singleton)
        .requires(vec!["wafer-run/database".into(), "wafer-run/config".into()])
        // Advisory table list — admin "Database tables" discovery + the WRAP
        // grant-UI read only `CollectionSchema::name`. The schema itself
        // (columns, indexes) lives solely in the block's hand-authored
        // `migrations/*.sqlite.sql` files (the single source for both runtime
        // `migrations::apply()` and the Cloudflare D1 build).
        .collections(vec![CollectionSchema::new(TABLE)])
        .category(wafer_run::BlockCategory::Feature)
        .description("User-facing profile page with editable display name, admin-configurable navigation buttons, and portal configuration endpoint.")
        .endpoints(endpoint_match::declare(ROUTES))
        .config_keys(vec![])
        .admin_url("/b/userportal/admin/settings")
        .can_disable(true)
        // Ships enabled — see the note on `legalpages`. Same divergence, same
        // resolution: the declaration is corrected to the value production has
        // been running, not the other way round.
        .default_enabled(true)
    },
    handle: |this, ctx, msg, input| {
        // Auth is enforced centrally by `route_to_block` from each row's
        // declared `AuthLevel`; the block holds no `user_id` / `is_admin`
        // preamble. `{hash}` / `{id}` are bound into `req.param.*` for the
        // handlers' `msg.var` readers.
        let Some(route) = endpoint_match::dispatch(&mut msg, ROUTES) else {
            return err_not_found("not found");
        };
        match route {
            Route::Dashboard => pages::dashboard::dashboard_page(ctx, &msg).await,
            Route::Profile => pages::profile::profile_page(ctx, &msg).await,
            Route::UpdateProfile => handle_update_profile(ctx, &msg, input).await,
            Route::Sessions => pages::sessions::sessions_page(ctx, &msg).await,
            Route::RevokeSession => pages::sessions::handle_revoke(ctx, &msg).await,
            Route::Security => pages::security::security_page(ctx, &msg).await,
            Route::Config => this.handle_config(ctx).await,
            Route::AdminSettingsPage => admin_settings_page(ctx, &msg).await,
            Route::AdminSaveSettings => handle_save_settings(ctx, input).await,
            Route::AdminButtonsPage => pages::admin_buttons::admin_buttons_page(ctx, &msg).await,
            Route::AdminCreateButton => pages::admin_buttons::handle_create_button(ctx, input).await,
            Route::AdminEditButtonForm => {
                pages::admin_buttons::handle_edit_button_form(ctx, msg.var("id")).await
            }
            Route::AdminUpdateButton => {
                pages::admin_buttons::handle_update_button(ctx, input, msg.var("id")).await
            }
            Route::AdminDeleteButton => {
                pages::admin_buttons::handle_delete_button(ctx, msg.var("id")).await
            }
        }
    },
    lifecycle: |_this, ctx, event| {
        crate::migration_helper::lifecycle_init(
            ctx,
            &event,
            "impresspress/userportal",
            migrations::SQLITE_MIGRATIONS,
            migrations::POSTGRES_MIGRATIONS,
        )
        .await
    },
}

impl UserPortalBlock {
    async fn handle_config(&self, ctx: &dyn Context) -> OutputStream {
        let settings = ctx
            .config_get(crate::features::BLOCK_SETTINGS_CONFIG_KEY)
            .map(crate::features::BlockSettings::from_config_json)
            .unwrap_or_else(|| crate::features::BlockSettings::from_map(Default::default()));

        let is_enabled = |name: &str| -> bool {
            use crate::features::FeatureConfig;
            settings.is_block_enabled(name)
        };

        let config_val = serde_json::json!({
            "logo_url": config::get_default(ctx, crate::config_vars::LOGO_URL_KEY, "").await,
            "app_name": config::get_default(ctx, "WAFER_RUN_SHARED__APP_NAME", "Impresspress").await,
            // Blank = "use the built-in brand accent" (same contract as the
            // admin chrome; see layout::page). The old `#6366f1` fallback here
            // was the pre-rebrand indigo leaking into portal clients.
            "primary_color": config::get_default(ctx, "WAFER_RUN_SHARED__PRIMARY_COLOR", "").await,
            "enable_oauth": config::get_default(ctx, "WAFER_RUN_SHARED__ENABLE_OAUTH", "false").await,
            "allow_signup": config::get_default(ctx, "WAFER_RUN_SHARED__ALLOW_SIGNUP", "true").await,
            "show_powered_by": true,
            "features": {
                "files": is_enabled("impresspress/files"),
                "products": is_enabled("impresspress/products"),
                "user_products": config::get_default(ctx, "WAFER_RUN_SHARED__ALLOW_USER_PRODUCTS", "false").await,
                "legal_pages": is_enabled("impresspress/legalpages"),
                "userportal": is_enabled("impresspress/userportal"),
            }
        });
        ok_json(&config_val)
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

async fn load_buttons(ctx: &dyn Context) -> Vec<wafer_core::clients::database::Record> {
    db::list(
        ctx,
        TABLE,
        &ListOptions {
            sort: vec![SortField {
                field: "sort_order".into(),
                desc: false,
            }],
            limit: 50,
            ..Default::default()
        },
    )
    .await
    .map(|r| r.records)
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// User-facing: Update profile
// ---------------------------------------------------------------------------

async fn handle_update_profile(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let user_id = msg.user_id().to_string();
    if user_id.is_empty() {
        return err_forbidden("Not authenticated");
    }

    let raw = input.collect_to_bytes().await;
    let body = parse_form_body(&raw);

    // CSRF defense-in-depth: this is a plain (no-JS) `<form>` POST (see
    // `pages::profile::profile_page`, which embeds the matching token via
    // `crate::csrf::hidden_field`). The Fetch-Metadata/Origin layer
    // (`crate::csrf::enforce_origin_policy`) already covers this request
    // since it's cookie-authenticated and unsafe-method; this is the
    // additional per-form check.
    let submitted_csrf = body
        .get(crate::csrf::FIELD_NAME)
        .map(String::as_str)
        .unwrap_or("");
    if !crate::csrf::verify(ctx, msg, submitted_csrf) {
        return err_forbidden("invalid or missing csrf token");
    }

    let name = body.get("name").map(|s| s.as_str()).unwrap_or("");

    // `update_profile` dual-writes `display_name` and the `name` alias, so
    // the typed row and the raw column cannot drift apart.
    if let Err(e) =
        crate::blocks::auth::repo::users::update_profile(ctx, &user_id, Some(name), None).await
    {
        // Pass the full RepoError so the helper logs the underlying failure
        // instead of just a rendered string.
        return err_internal("Failed to update profile", e);
    }

    // Plain form POST → 303 See Other so the browser follows up with a GET
    // and the back/forward stack stays clean.
    crate::http::redirect(303, "/b/userportal/profile")
}

#[cfg(test)]
mod update_profile_csrf_tests {
    use super::*;
    use crate::test_support::{auth_msg, output_status, TestContext};

    async fn seed_user(ctx: &TestContext, user_id: &str) {
        ctx.seed_auth_user(user_id).await;
        crate::blocks::auth::repo::users::update_profile(ctx, user_id, Some("Old Name"), None)
            .await
            .expect("seed profile name");
    }

    /// The `display_name`/`name` pair as the repo reports it.
    async fn profile_name(ctx: &TestContext, user_id: &str) -> String {
        crate::blocks::auth::repo::users::find_by_id(ctx, user_id)
            .await
            .expect("read user")
            .expect("user exists")
            .display_name
    }

    #[tokio::test]
    async fn valid_csrf_token_is_accepted() {
        let ctx = TestContext::with_userportal().await;
        seed_user(&ctx, "user-1").await;
        let msg = auth_msg("create", "/b/userportal/update-profile", "user-1");

        let form = format!(
            "name=New+Name&csrf_token={}",
            crate::csrf::token(&ctx, &msg)
        );
        let out =
            handle_update_profile(&ctx, &msg, InputStream::from_bytes(form.into_bytes())).await;
        assert_eq!(output_status(out).await, 303, "valid token must succeed");

        assert_eq!(profile_name(&ctx, "user-1").await, "New Name");
    }

    #[tokio::test]
    async fn missing_csrf_token_is_rejected() {
        let ctx = TestContext::with_userportal().await;
        seed_user(&ctx, "user-1").await;
        let msg = auth_msg("create", "/b/userportal/update-profile", "user-1");

        let form = "name=New+Name".to_string();
        let out =
            handle_update_profile(&ctx, &msg, InputStream::from_bytes(form.into_bytes())).await;
        assert!(
            crate::test_support::output_is_error(out, "PermissionDenied").await,
            "a form POST with no csrf_token must be rejected"
        );

        // Row must be unchanged — rejection happens before the update.
        assert_eq!(profile_name(&ctx, "user-1").await, "Old Name");
    }

    #[tokio::test]
    async fn wrong_csrf_token_is_rejected() {
        let ctx = TestContext::with_userportal().await;
        seed_user(&ctx, "user-1").await;
        let msg = auth_msg("create", "/b/userportal/update-profile", "user-1");

        let form = "name=New+Name&csrf_token=not-the-right-value".to_string();
        let out =
            handle_update_profile(&ctx, &msg, InputStream::from_bytes(form.into_bytes())).await;
        assert!(crate::test_support::output_is_error(out, "PermissionDenied").await);
    }

    #[tokio::test]
    async fn another_users_token_is_rejected() {
        // The token is per-identity — user B's valid token must not authorize
        // a mutation submitted as user A.
        let ctx = TestContext::with_userportal().await;
        seed_user(&ctx, "user-1").await;
        let msg_a = auth_msg("create", "/b/userportal/update-profile", "user-1");
        let msg_b = auth_msg("create", "/b/userportal/update-profile", "user-2");

        let form = format!(
            "name=New+Name&csrf_token={}",
            crate::csrf::token(&ctx, &msg_b)
        );
        let out =
            handle_update_profile(&ctx, &msg_a, InputStream::from_bytes(form.into_bytes())).await;
        assert!(crate::test_support::output_is_error(out, "PermissionDenied").await);
    }
}

// ---------------------------------------------------------------------------
// Admin: Branding Settings
// ---------------------------------------------------------------------------

/// The shared branding config vars rendered on the portal settings page,
/// pulled from their central `config_vars::shared_var` declarations (single
/// source of truth — no parallel tuple table that had drifted on the logo-URL
/// input types and the favicon default).
fn branding_vars() -> Vec<wafer_run::ConfigVar> {
    [
        "WAFER_RUN_SHARED__APP_NAME",
        "WAFER_RUN_SHARED__LOGO_URL",
        "WAFER_RUN_SHARED__LOGO_ICON_URL",
        "WAFER_RUN_SHARED__AUTH_LOGO_URL",
        "WAFER_RUN_SHARED__FAVICON_URL",
        "WAFER_RUN_SHARED__PRIMARY_COLOR",
    ]
    .into_iter()
    .map(crate::config_vars::shared_var)
    .collect()
}

async fn admin_settings_page(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let vars = branding_vars();
    let sections = [settings_form::SettingsSection::new(
        "Branding",
        icons::settings(),
        &vars,
    )];
    let content = html! {
        (components::page_header("Branding Settings", Some("Customize your application appearance"), None))
        (settings_form::settings_form(ctx, "/b/userportal/admin/settings", &sections, html! {}).await)
    };
    ui::shell_page(
        ctx,
        msg,
        ui::Shell::simple("Settings", ui::NavKind::Portal, "Settings"),
        content,
    )
    .await
}

async fn handle_save_settings(ctx: &dyn Context, input: InputStream) -> OutputStream {
    settings_form::save_settings(ctx, input, &branding_vars(), "userportal").await
}

#[cfg(test)]
mod test_support {
    use wafer_run::Message;

    /// Run `msg` through the block's own route table so `{hash}` / `{id}` is
    /// bound the way it is on the wire, then hand the message to a handler
    /// directly. Panics when no row matches: a test that sends an unroutable
    /// path would otherwise exercise the handler's "nothing bound" branch by
    /// accident.
    pub(super) fn routed(mut msg: Message) -> Message {
        let route = crate::endpoint_match::dispatch(&mut msg, super::ROUTES);
        assert!(
            route.is_some(),
            "no userportal route matches {} {}",
            msg.action(),
            msg.path()
        );
        msg
    }
}

#[cfg(test)]
mod table_tests {
    use wafer_run::Block as _;

    use super::*;

    /// `info().endpoints` is generated from `ROUTES`; nothing else declares
    /// an endpoint for this block.
    #[test]
    fn info_endpoints_come_from_the_table() {
        let declared = UserPortalBlock::new().info().endpoints;
        assert_eq!(declared.len(), ROUTES.len());
        for (ep, row) in declared.iter().zip(ROUTES) {
            assert_eq!(ep.method, row.method, "{}", row.template);
            assert_eq!(ep.path, row.template);
            assert_eq!(ep.auth, row.auth, "{}", row.template);
        }
    }
}
