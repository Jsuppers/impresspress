//! `impresspress/auth-ui` — SSR pages + JSON API + OAuth flows + bootstrap token
//! redemption for impresspress auth.
//!
//! Plan A2 PR 5 splits the legacy `wafer-run/auth` block into two halves:
//!
//! - **Framework auth** (`wafer-run/auth`, lives in `wafer-run` proper):
//!   service-shaped block exposing `auth@v1` (`require_user`/`require_role`/
//!   token issue+verify). Owns `JWT_SECRET`, `REQUIRE_VERIFICATION`,
//!   `ALLOWED_EMAIL_DOMAINS`, `INTERNAL_SECRET`. No HTTP routes.
//!
//! - **auth-ui** (this module): all `/b/auth/*` HTTP routes. Reads/writes
//!   auth tables via `repo::*` under WRAP grant. Calls the framework auth
//!   block via the `auth@v1` typed client for identity primitives.
//!
//! Declares the full `BlockInfo` (endpoints, requires, OAuth-creds
//! config_keys) from [`ROUTES`], runs the per-user/IP rate-limit check keyed
//! on the matched [`Route`], and dispatches every `/b/auth/*` route to a leaf
//! module under `api/`, `pages/`, or `oauth/`. The framework `wafer-run/auth`
//! block (in `auth/`) owns the auth *service*; this block owns the HTTP
//! surface.

pub mod api;
pub mod contracts;
pub mod oauth;
pub mod pages;
pub mod redirect;

use wafer_run::{
    context::Context, BlockInfo, ConfigVar, HttpMethod, InputType, InstanceMode, Message,
    OutputStream,
};

use super::rate_limit::{
    check_rate_limit, ip_identity, LimitKey, RateLimit, RateLimitOutcome, UserRateLimiter,
};
use crate::{
    endpoint_match::{self, request_schema_of, response_schema_of, EndpointRoute},
    http::err_not_found,
};

pub const AUTH_UI_BLOCK_ID: &str = "impresspress/auth-ui";

/// Handler for one row of [`ROUTES`]. `Verify` serves both the `GET` and the
/// `POST` row of `/b/auth/api/verify` (the token arrives in the query string
/// or in the body).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    AdminSettingsPage,
    AdminSaveSettings,
    LoginPage,
    SignupPage,
    ChangePasswordPage,
    OrgsPage,
    ResetPasswordPage,
    BootstrapPage,
    OauthStart,
    OauthCallback,
    Login,
    Signup,
    Refresh,
    Logout,
    Me,
    UpdateMe,
    ChangePassword,
    ListApiKeys,
    CreateApiKey,
    RevokeApiKey,
    DeleteApiKey,
    Verify,
    ResendVerification,
    ForgotPassword,
    ResetPassword,
    OauthProviders,
    SyncUser,
    Bootstrap,
}

/// The block's HTTP surface: what `handle()` dispatches on and what
/// `info().endpoints` is generated from. Wire paths; `{id}` is bound into
/// `req.param.*` for the api-key handlers' `msg.var` reader.
///
/// Every row names the level the central router enforces. A `public` row is
/// a decision recorded next to the row: the handler gates itself by a token,
/// signature or shared secret, or the endpoint exists precisely for a caller
/// with no session yet (login, signup, "forgot password"). The JSON API
/// schemas are DERIVED from the types the handlers actually deserialize into
/// and serialize out of, declared in [`contracts`], so they cannot drift from
/// the handlers. Those are the core developer-facing auth endpoints; schema
/// coverage of the remaining rows (OAuth, api-keys, password reset,
/// bootstrap) is a follow-up.
const ROUTES: &[EndpointRoute<Route>] = &[
    // ── Admin settings ── declared `Admin` so the central router enforces the
    // tier; the handler re-checks nothing. (The auth-ui prefix route is
    // Public, so this declared level is the gate for the admin surface.)
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/auth/admin/settings",
        Route::AdminSettingsPage,
    )
    .summary("Auth settings page"),
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/auth/admin/settings",
        Route::AdminSaveSettings,
    )
    .summary("Save auth settings"),
    // ── SSR pages ──
    EndpointRoute::public(HttpMethod::Get, "/b/auth/login", Route::LoginPage).summary("Login page"),
    EndpointRoute::public(HttpMethod::Get, "/b/auth/signup", Route::SignupPage)
        .summary("Signup page"),
    EndpointRoute::authenticated(
        HttpMethod::Get,
        "/b/auth/change-password",
        Route::ChangePasswordPage,
    )
    .summary("Change password page"),
    EndpointRoute::authenticated(HttpMethod::Get, "/b/auth/orgs", Route::OrgsPage)
        .summary("Claimed organizations"),
    // Public: logged-out by definition. `pages/reset_password.rs` renders the
    // form only when a `token` query parameter is present; the token itself
    // is verified by `POST /b/auth/api/reset-password`.
    EndpointRoute::public(
        HttpMethod::Get,
        "/b/auth/reset-password",
        Route::ResetPasswordPage,
    )
    .summary("Password reset form"),
    EndpointRoute::public(HttpMethod::Get, "/b/auth/bootstrap", Route::BootstrapPage)
        .summary("Bootstrap token redemption form"),
    // ── OAuth browser redirects ──
    EndpointRoute::public(HttpMethod::Get, "/b/auth/oauth/login", Route::OauthStart)
        .summary("Start OAuth flow"),
    // Public: the provider redirects the browser here with no session by
    // design; `oauth/callback.rs` consumes the single-use PKCE state.
    EndpointRoute::public(
        HttpMethod::Get,
        "/b/auth/oauth/callback",
        Route::OauthCallback,
    )
    .summary("OAuth provider callback"),
    // ── JSON API ──
    EndpointRoute::public(HttpMethod::Post, "/b/auth/api/login", Route::Login)
        .summary("Authenticate with email/password")
        .input(request_schema_of::<contracts::LoginRequest>)
        .output(response_schema_of::<contracts::LoginResponse>)
        .tags(&["auth"]),
    EndpointRoute::public(HttpMethod::Post, "/b/auth/api/signup", Route::Signup)
        .summary("Create account")
        .input(request_schema_of::<contracts::SignupRequest>)
        .output(response_schema_of::<contracts::SignupResponse>)
        .tags(&["auth"]),
    // Public: takes no `Authorization` header, only a `refresh_token` body
    // field.
    EndpointRoute::public(HttpMethod::Post, "/b/auth/api/refresh", Route::Refresh)
        .summary("Rotate an access/refresh token pair")
        .input(request_schema_of::<contracts::RefreshRequest>)
        .output(response_schema_of::<contracts::RefreshResponse>)
        .tags(&["auth"]),
    EndpointRoute::authenticated(HttpMethod::Post, "/b/auth/api/logout", Route::Logout)
        .summary("Sign out")
        .output(response_schema_of::<contracts::LogoutResponse>)
        .tags(&["auth"]),
    EndpointRoute::authenticated(HttpMethod::Get, "/b/auth/api/me", Route::Me)
        .summary("Get current user")
        .output(response_schema_of::<contracts::MeResponse>)
        .tags(&["auth"]),
    // PATCH is what the SDK sends; `update` is the action both PUT and PATCH
    // map to.
    EndpointRoute::authenticated(HttpMethod::Patch, "/b/auth/api/me", Route::UpdateMe)
        .summary("Update current user profile")
        .input(request_schema_of::<contracts::UpdateMeRequest>)
        .output(response_schema_of::<contracts::MeResponse>)
        .tags(&["auth"]),
    EndpointRoute::authenticated(
        HttpMethod::Post,
        "/b/auth/api/change-password",
        Route::ChangePassword,
    )
    .summary("Change password"),
    // ── API keys (admin user-management still hits these via htmx) ──
    EndpointRoute::authenticated(HttpMethod::Get, "/b/auth/api/api-keys", Route::ListApiKeys)
        .summary("List API keys"),
    EndpointRoute::authenticated(
        HttpMethod::Post,
        "/b/auth/api/api-keys",
        Route::CreateApiKey,
    )
    .summary("Create API key"),
    // Authenticated; `api/api_keys.rs` additionally refuses a key the caller
    // does not own.
    EndpointRoute::authenticated(
        HttpMethod::Patch,
        "/b/auth/api/api-keys/{id}",
        Route::RevokeApiKey,
    )
    .summary("Revoke API key"),
    EndpointRoute::authenticated(
        HttpMethod::Delete,
        "/b/auth/api/api-keys/{id}",
        Route::DeleteApiKey,
    )
    .summary("Delete API key"),
    // ── Email verification ── public: `api/verify.rs` consumes a single-use
    // verification token, from the query string on GET or the body on POST.
    EndpointRoute::public(HttpMethod::Get, "/b/auth/api/verify", Route::Verify)
        .summary("Verify email address"),
    EndpointRoute::public(HttpMethod::Post, "/b/auth/api/verify", Route::Verify)
        .summary("Verify email address"),
    // Public: issues a verification token to the address's owner and answers
    // one constant body whatever the account's state (unregistered, already
    // verified, inside its 60 s cooldown), so nothing about an address can be
    // learned from the response; `api::verify::resend_tests` pins that. IP
    // rate-limited (see `rate_limit_for`).
    EndpointRoute::public(
        HttpMethod::Post,
        "/b/auth/api/resend-verification",
        Route::ResendVerification,
    )
    .summary("Re-send the verification email"),
    // ── Password reset ── public for the same reasons as the verification
    // pair: `forgot-password` issues a token to the address's owner behind a
    // constant response; `reset-password` consumes it (hash match + expiry).
    EndpointRoute::public(
        HttpMethod::Post,
        "/b/auth/api/forgot-password",
        Route::ForgotPassword,
    )
    .summary("Request a password reset email"),
    EndpointRoute::public(
        HttpMethod::Post,
        "/b/auth/api/reset-password",
        Route::ResetPassword,
    )
    .summary("Reset password with a reset token"),
    // ── OAuth API ──
    // Public: reports which providers are configured, the same fact the
    // login page renders as buttons for anonymous visitors.
    EndpointRoute::public(
        HttpMethod::Get,
        "/b/auth/api/oauth/providers",
        Route::OauthProviders,
    )
    .summary("Configured OAuth providers"),
    // Public to the router; `api/sync_user.rs` refuses unless the
    // `x-internal-secret` header matches `INTERNAL_SECRET` (constant-time).
    EndpointRoute::public(
        HttpMethod::Post,
        "/b/auth/api/oauth/sync-user",
        Route::SyncUser,
    )
    .summary("Internal OAuth user sync"),
    // ── Bootstrap admin token redemption ──
    EndpointRoute::public(HttpMethod::Post, "/b/auth/api/bootstrap", Route::Bootstrap)
        .summary("Redeem bootstrap admin token"),
];

/// The rate-limit bucket a matched route spends, `(key, category, default
/// limit)`, or `None` for a route this layer does not limit. Applied after
/// `dispatch` has chosen the variant, so the block matches a path exactly
/// once. IP-keyed buckets guard endpoints a caller reaches without a session;
/// user-keyed buckets guard authenticated ones. The match is exhaustive so a
/// new row is a rate-limit decision, not an omission.
const fn rate_limit_for(route: Route) -> Option<(LimitKey, &'static str, RateLimit)> {
    match route {
        // Login / signup / bootstrap redemption, and the token-issuing and
        // token-consuming password-reset and verification endpoints, share
        // the `auth` bucket.
        Route::Login
        | Route::Signup
        | Route::Bootstrap
        | Route::ForgotPassword
        | Route::ResetPassword
        | Route::ResendVerification
        | Route::Verify => Some((LimitKey::Ip, "auth", RateLimit::AUTH)),
        // Token refresh has its own (looser) category.
        Route::Refresh => Some((LimitKey::Ip, "refresh", RateLimit::REFRESH)),
        Route::Me | Route::ListApiKeys => Some((LimitKey::User, "auth_read", RateLimit::API_READ)),
        Route::UpdateMe
        | Route::ChangePassword
        | Route::CreateApiKey
        | Route::RevokeApiKey
        | Route::DeleteApiKey => Some((LimitKey::User, "auth_write", RateLimit::API_WRITE)),
        Route::AdminSettingsPage
        | Route::AdminSaveSettings
        | Route::LoginPage
        | Route::SignupPage
        | Route::ChangePasswordPage
        | Route::OrgsPage
        | Route::ResetPasswordPage
        | Route::BootstrapPage
        | Route::OauthStart
        | Route::OauthCallback
        | Route::Logout
        | Route::OauthProviders
        | Route::SyncUser => None,
    }
}

/// Spend `route`'s rate-limit bucket for this request. `Some(response)` is
/// the 429 to return; `None` means proceed: no bucket for this route, the
/// bucket is disabled by config, a user-keyed bucket with no user, or under
/// the limit.
///
/// `RateLimitOutcome::Allowed(headers)` is discarded: injecting X-RateLimit-*
/// response headers needs a streaming-middleware shape we don't have yet.
/// Tracked as a single follow-up, not a per-route TODO.
async fn apply_rate_limit(
    limiter: &UserRateLimiter,
    ctx: &dyn Context,
    msg: &Message,
    route: Route,
) -> Option<OutputStream> {
    let (key, category, limit) = rate_limit_for(route)?;
    let identity = match key {
        LimitKey::Ip => ip_identity(msg),
        LimitKey::User => {
            let user_id = msg.user_id();
            if user_id.is_empty() {
                return None;
            }
            user_id.to_string()
        }
    };
    match check_rate_limit(limiter, ctx, &identity, category, limit).await {
        RateLimitOutcome::Limited(response) => Some(response),
        RateLimitOutcome::Allowed(_) | RateLimitOutcome::Disabled => None,
    }
}

/// The auth-ui block's own declared config vars (OAuth provider creds). Single
/// source of truth for both `BlockInfo::config_keys` and the admin settings
/// page (rendered via `ui::settings_form`, not a parallel tuple table).
///
/// OAuth provider creds live under the auth-ui prefix
/// (`IMPRESSPRESS__AUTH_UI__OAUTH_*`) to keep the prefix-equals-block-name
/// invariant the runtime enforces (see `block_name_to_var_prefix`). The
/// auth-identity vars JWT_SECRET / REQUIRE_VERIFICATION / ALLOWED_EMAIL_DOMAINS
/// are `WAFER_RUN__AUTH__*` and declared in `auth::config` instead.
pub(crate) fn config_vars() -> Vec<ConfigVar> {
    vec![
        ConfigVar::new(
            "IMPRESSPRESS__AUTH_UI__OAUTH_GOOGLE_CLIENT_ID",
            "Google OAuth client ID",
            "",
        )
        .name("Google Client ID")
        .optional(),
        ConfigVar::new(
            "IMPRESSPRESS__AUTH_UI__OAUTH_GOOGLE_CLIENT_SECRET",
            "Google OAuth client secret",
            "",
        )
        .name("Google Client Secret")
        .input_type(InputType::Password)
        .optional(),
        ConfigVar::new(
            "IMPRESSPRESS__AUTH_UI__OAUTH_GITHUB_CLIENT_ID",
            "GitHub OAuth client ID",
            "",
        )
        .name("GitHub Client ID")
        .optional(),
        ConfigVar::new(
            "IMPRESSPRESS__AUTH_UI__OAUTH_GITHUB_CLIENT_SECRET",
            "GitHub OAuth client secret",
            "",
        )
        .name("GitHub Client Secret")
        .input_type(InputType::Password)
        .optional(),
        ConfigVar::new(
            "IMPRESSPRESS__AUTH_UI__OAUTH_MICROSOFT_CLIENT_ID",
            "Microsoft OAuth client ID",
            "",
        )
        .name("Microsoft Client ID")
        .optional(),
        ConfigVar::new(
            "IMPRESSPRESS__AUTH_UI__OAUTH_MICROSOFT_CLIENT_SECRET",
            "Microsoft OAuth client secret",
            "",
        )
        .name("Microsoft Client Secret")
        .input_type(InputType::Password)
        .optional(),
        ConfigVar::new(
            "IMPRESSPRESS__AUTH_UI__OAUTH_REDIRECT_URI",
            "OAuth callback URL",
            "",
        )
        .name("OAuth Redirect URI")
        .input_type(InputType::Url)
        .optional(),
    ]
}

crate::impresspress_feature_block! {
    /// Impresspress auth HTTP surface — SSR pages + JSON API + OAuth + bootstrap
    /// (`impresspress/auth-ui`). The auth *service* primitive lives in the
    /// framework `wafer-run/auth` block.
    pub struct AuthUiBlock;
    fields: { limiter: UserRateLimiter },
    name: "impresspress/auth-ui",
    info: |_this| {
        BlockInfo::new(
            AUTH_UI_BLOCK_ID,
            "0.0.1",
            "http-handler@v1",
            "SSR auth pages + login/signup/oauth/bootstrap handlers",
        )
        .instance_mode(InstanceMode::Singleton)
        .requires(vec![
            "wafer-run/database".into(),
            "wafer-run/crypto".into(),
            "wafer-run/config".into(),
            "wafer-run/network".into(),
            "impresspress/email".into(),
            "wafer-run/auth".into(),
        ])
        .category(wafer_run::BlockCategory::Feature)
        .description(
            "Impresspress auth HTTP surface (SSR pages, JSON API, OAuth, bootstrap \
             token redemption). Reads/writes auth tables via repo::* under WRAP \
             grant. Calls wafer-run/auth via auth@v1 for require_user/role/token.",
        )
        .endpoints(endpoint_match::declare(ROUTES))
        .config_keys(config_vars())
        .admin_url("/b/auth/admin/settings")
    },
    handle: |this, ctx, msg, input| {
        // Auth is enforced centrally by `route_to_block` from each row's
        // declared `AuthLevel`. `{id}` is bound into `req.param.*` for the
        // api-key handlers' `msg.var` reader.
        let Some(route) = endpoint_match::dispatch(&mut msg, ROUTES) else {
            return err_not_found("not found");
        };
        if let Some(limited) = apply_rate_limit(&this.limiter, ctx, &msg, route).await {
            return limited;
        }
        match route {
            Route::AdminSettingsPage => pages::settings::handle_get(ctx, &msg).await,
            Route::AdminSaveSettings => pages::settings::handle_post(ctx, input).await,
            Route::LoginPage => pages::login::handle(ctx, &msg).await,
            Route::SignupPage => pages::signup::handle(ctx, &msg).await,
            Route::ChangePasswordPage => {
                if msg.user_id().is_empty() {
                    return pages::login::handle(ctx, &msg).await;
                }
                pages::change_password::handle(ctx, &msg).await
            }
            Route::OrgsPage => pages::orgs::handle(ctx, &msg).await,
            Route::ResetPasswordPage => pages::reset_password::handle(ctx, &msg).await,
            Route::BootstrapPage => pages::bootstrap::handle_get(ctx, &msg).await,
            Route::OauthStart => oauth::start::handle(ctx, &msg).await,
            Route::OauthCallback => oauth::callback::handle(ctx, &msg).await,
            Route::Login => api::login::handle(ctx, input).await,
            Route::Signup => api::signup::handle(ctx, input).await,
            Route::Refresh => api::refresh::handle(ctx, input).await,
            Route::Logout => api::logout::handle(ctx, &msg).await,
            Route::Me => api::me::handle_get(ctx, &msg).await,
            Route::UpdateMe => api::me::handle_update(ctx, &msg, input).await,
            Route::ChangePassword => api::change_password::handle(ctx, &msg, input).await,
            Route::ListApiKeys => api::api_keys::handle_list(ctx, &msg).await,
            Route::CreateApiKey => api::api_keys::handle_create(ctx, &msg, input).await,
            Route::RevokeApiKey => api::api_keys::handle_revoke(ctx, &msg).await,
            Route::DeleteApiKey => api::api_keys::handle_delete(ctx, &msg).await,
            Route::Verify => api::verify::handle(ctx, &msg, input).await,
            Route::ResendVerification => api::verify::handle_resend(ctx, input).await,
            Route::ForgotPassword => api::forgot_password::handle(ctx, input).await,
            Route::ResetPassword => api::reset_password::handle(ctx, input).await,
            Route::OauthProviders => oauth::providers::handle(ctx).await,
            Route::SyncUser => api::sync_user::handle(ctx, &msg, input).await,
            Route::Bootstrap => api::bootstrap::handle(ctx, &msg, input).await,
        }
    },
    // No `lifecycle`: auth-ui owns no schema (auth tables belong to the
    // framework `wafer-run/auth` block), so the `Block` no-op default
    // applies.
}

#[cfg(test)]
mod test_support {
    use wafer_run::Message;

    /// Run `msg` through the block's own route table so `{id}` is bound the
    /// way it is on the wire, then hand the message to a handler directly.
    /// Panics when no row matches: a test that sends an unroutable path
    /// would otherwise exercise the handler's "missing id" branch by
    /// accident.
    pub(super) fn routed(mut msg: Message) -> Message {
        let route = crate::endpoint_match::dispatch(&mut msg, super::ROUTES);
        assert!(
            route.is_some(),
            "no auth-ui route matches {} {}",
            msg.action(),
            msg.path()
        );
        msg
    }
}

#[cfg(test)]
mod rate_limit_tests {
    use super::*;

    #[test]
    fn public_bootstrap_redemption_is_ip_rate_limited() {
        let (key, category, limit) =
            rate_limit_for(Route::Bootstrap).expect("bootstrap redemption spends a bucket");
        assert_eq!(key, LimitKey::Ip);
        assert_eq!(category, "auth");
        assert_eq!(limit.max_requests, RateLimit::AUTH.max_requests);
        assert_eq!(limit.window, RateLimit::AUTH.window);
    }

    #[test]
    fn bootstrap_form_get_does_not_spend_the_redemption_budget() {
        assert!(rate_limit_for(Route::BootstrapPage).is_none());
    }

    /// The assignments the old five-rule `RATE_LIMIT_ROUTES` table made, by
    /// method and wire path (it matched the `/b`-stripped form; the paths
    /// here are the wire spelling of the same rules). Every row's bucket must
    /// be what that table gave it, and every row the table did not match must
    /// spend nothing, so keying on the variant changed no assignment.
    #[test]
    fn rate_limits_are_the_old_route_table_assignments() {
        use HttpMethod::{Delete, Get, Patch, Post};
        let old_assignments: &[(HttpMethod, &str, LimitKey, &str)] = &[
            (Post, "/b/auth/api/login", LimitKey::Ip, "auth"),
            (Post, "/b/auth/api/signup", LimitKey::Ip, "auth"),
            (Post, "/b/auth/api/bootstrap", LimitKey::Ip, "auth"),
            (Post, "/b/auth/api/refresh", LimitKey::Ip, "refresh"),
            (Post, "/b/auth/api/forgot-password", LimitKey::Ip, "auth"),
            (Post, "/b/auth/api/reset-password", LimitKey::Ip, "auth"),
            (
                Post,
                "/b/auth/api/resend-verification",
                LimitKey::Ip,
                "auth",
            ),
            (Get, "/b/auth/api/verify", LimitKey::Ip, "auth"),
            (Post, "/b/auth/api/verify", LimitKey::Ip, "auth"),
            (Get, "/b/auth/api/me", LimitKey::User, "auth_read"),
            (Get, "/b/auth/api/api-keys", LimitKey::User, "auth_read"),
            (Patch, "/b/auth/api/me", LimitKey::User, "auth_write"),
            (
                Patch,
                "/b/auth/api/api-keys/{id}",
                LimitKey::User,
                "auth_write",
            ),
            (
                Delete,
                "/b/auth/api/api-keys/{id}",
                LimitKey::User,
                "auth_write",
            ),
            (
                Post,
                "/b/auth/api/change-password",
                LimitKey::User,
                "auth_write",
            ),
            (Post, "/b/auth/api/api-keys", LimitKey::User, "auth_write"),
        ];
        for (method, path, _, _) in old_assignments {
            assert!(
                ROUTES
                    .iter()
                    .any(|row| row.method == *method && row.template == *path),
                "{method} {path} is not a row"
            );
        }
        for row in ROUTES {
            let expected = old_assignments
                .iter()
                .find(|(method, path, _, _)| *method == row.method && *path == row.template)
                .map(|(_, _, key, category)| (*key, *category));
            let actual = rate_limit_for(row.handler).map(|(key, category, _)| (key, category));
            assert_eq!(actual, expected, "{} {}", row.method, row.template);
        }
    }

    /// The `RateLimit` each category resolves to is the constant the old
    /// table named for it.
    #[test]
    fn rate_limit_categories_keep_their_defaults() {
        for row in ROUTES {
            let Some((_, category, limit)) = rate_limit_for(row.handler) else {
                continue;
            };
            let expected = match category {
                "auth" => RateLimit::AUTH,
                "refresh" => RateLimit::REFRESH,
                "auth_read" => RateLimit::API_READ,
                "auth_write" => RateLimit::API_WRITE,
                other => panic!("unexpected category {other} for {}", row.template),
            };
            assert_eq!(
                limit.max_requests, expected.max_requests,
                "{}",
                row.template
            );
            assert_eq!(limit.window, expected.window, "{}", row.template);
        }
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
        let declared = AuthUiBlock::new().info().endpoints;
        assert_eq!(declared.len(), ROUTES.len());
        for (ep, row) in declared.iter().zip(ROUTES) {
            assert_eq!(ep.method, row.method, "{}", row.template);
            assert_eq!(ep.path, row.template);
            assert_eq!(ep.auth, row.auth, "{}", row.template);
        }
    }
}
