//! POST /b/auth/api/login — relocated from auth/login.rs in Task 5.

use wafer_core::clients::{config, crypto};
use wafer_run::{context::Context, InputStream, OutputStream};

use crate::{
    blocks::{
        auth::{
            helpers::{ensure_admin_role, issue_tokens_and_cookie},
            repo::{local_credentials, users},
            timing_equalization_hash,
        },
        auth_ui::{
            contracts::{AuthenticatedUser, LoginRequest, LoginResponse, TokenType},
            redirect::{default_post_login_redirect, is_safe_local_redirect},
        },
        errors::{error_response, ErrorCode},
    },
    http::{err_bad_request, err_internal, ResponseBuilder},
};

pub async fn handle(ctx: &dyn Context, input: InputStream) -> OutputStream {
    let raw = input.collect_to_bytes().await;
    let body: LoginRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    let email_lower = body.email.trim().to_lowercase();

    // Find user by email via typed repo. `users::find_by_email` already
    // collapses NOT_FOUND to `Ok(None)`; any `Err` here is a real failure
    // (WRAP denial, DB outage) that must not collapse to "invalid
    // credentials" — that would mask outages and silently log users out.
    let user_row = match users::find_by_email(ctx, &email_lower).await {
        Ok(opt) => opt,
        Err(e) => return err_internal("User lookup failed", e),
    };

    // The real stored credential, if this login has one at all.
    let stored_hash_owned: String;
    let stored_hash: Option<&str> = match &user_row {
        Some(u) => match local_credentials::find_by_user_id(ctx, &u.id).await {
            Ok(Some(cred)) => {
                stored_hash_owned = cred.password_hash;
                Some(&stored_hash_owned)
            }
            _ => None,
        },
        None => None,
    };

    // With no credential to verify, burn one verification against a hash in
    // the scheme this platform writes, so "no such user" and "wrong password"
    // cost the same and the response time is not a user-enumeration oracle.
    // See `auth::timing_equalization_hash` for why that hash is derived rather
    // than a constant.
    //
    // The equalization comparison's OUTCOME is deliberately discarded: this
    // arm is reached precisely when there is nothing to authenticate against,
    // so it can never be a successful login however it returns. (The
    // placeholder password is a public constant; treating a match as a login
    // would sign the caller in as any account whose local-credentials row is
    // missing.)
    let password_ok = match stored_hash {
        Some(hash) => crypto::compare_hash(ctx, &body.password, hash)
            .await
            .is_ok(),
        None => {
            if let Some(equalizer) = timing_equalization_hash(ctx).await {
                let _ = crypto::compare_hash(ctx, &body.password, equalizer).await;
            }
            false
        }
    };

    // Use the typed UserRow we already have. `disabled` and `email_verified`
    // ride on the row, so no second `db::get` is needed.
    let user = match user_row {
        Some(u) if password_ok => u,
        _ => return error_response(ErrorCode::InvalidCredentials, "Invalid email or password"),
    };

    // [SEC-034] Disabled accounts return the SAME generic invalid-credentials
    // response as a wrong-password attempt. Surfacing "account is disabled"
    // confirms to an attacker that the email exists, gives them a target for
    // a re-enable social-engineering attack, and signals when an admin has
    // taken action on a compromised account.
    if !user.is_active() {
        return error_response(ErrorCode::InvalidCredentials, "Invalid email or password");
    }

    // Check email verification if required
    let require_verification =
        crate::config_vars::get_bool(ctx, "WAFER_RUN__AUTH__REQUIRE_VERIFICATION", false).await;
    if require_verification && !user.email_verified {
        return error_response(ErrorCode::EmailNotVerified, "Please verify your email before logging in. Check your inbox for the verification link.");
    }

    // Get roles, granting admin role idempotently when ADMIN_EMAIL matches.
    // A WRAP denial or DB error here must not silently resolve to "no
    // roles" — that would 403 an admin or double-grant on the next login
    // (SB-3).
    let roles = match ensure_admin_role(ctx, &user.id, &email_lower).await {
        Ok(r) => r,
        Err(e) => return err_internal("Failed to resolve user roles", e),
    };

    // Mint tokens, persist the refresh + session rows, build the cookie.
    let issued =
        match issue_tokens_and_cookie(ctx, &user.id, &email_lower, &roles, "password", None, 0)
            .await
        {
            Ok(i) => i,
            Err(r) => return r,
        };

    // Update last login. Best-effort: the sign-in has already succeeded
    // and the tokens are already minted, so a failed bookkeeping write is
    // logged, not returned.
    if let Err(e) = users::touch_last_login(ctx, &user.id).await {
        tracing::warn!("Failed to update last login time: {e}");
    }

    // Role-aware post-login default (#1 onboarding bug fix). The login PAGE
    // is rendered before credentials are known, so it cannot pick between
    // the admin and user-portal destinations itself; this JSON response is
    // where the caller's role first becomes known, so it's where the
    // single-sourced default (`redirect::default_post_login_redirect`) gets
    // applied. The client only falls back to this when it has no explicit,
    // already-validated `next`/`redirect` param of its own.
    let post_login_raw =
        config::get_default(ctx, "WAFER_RUN_SHARED__POST_LOGIN_REDIRECT", "/b/admin/").await;
    let admin_default = if is_safe_local_redirect(&post_login_raw) {
        post_login_raw
    } else {
        "/b/admin/".to_string()
    };
    let is_admin = roles.iter().any(|r| r == "admin");
    let default_redirect = default_post_login_redirect(is_admin, &admin_default);

    ResponseBuilder::new()
        .set_cookie(&issued.cookie)
        .json(&LoginResponse {
            access_token: issued.access_token,
            refresh_token: issued.refresh_token,
            token_type: TokenType::Bearer,
            expires_in: issued.access_lifetime,
            default_redirect,
            user: AuthenticatedUser {
                id: user.id,
                email: email_lower,
                roles,
                name: user.display_name,
            },
        })
}

/// Regression tests for the #1 onboarding bug: every successful login used
/// to compute `default_redirect` as a fixed `/b/admin/` regardless of the
/// caller's role, so a brand-new non-admin's login script sent them straight
/// into an admin-only route and hit a 403 dead-end. `default_redirect` is
/// now role-aware (`redirect::default_post_login_redirect`) — these tests
/// drive the real [`handle`] end-to-end through a seeded user (via the real
/// signup handler, so the password hash + role assignment are the same code
/// path production uses).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        blocks::{auth::TIMING_EQUALIZATION_PASSWORD, auth_ui::api::signup},
        test_support::{collect_or_panic, output_json, TestContext},
    };

    /// A context with a real crypto block — login verifies passwords via
    /// `crypto::compare_hash` and mints tokens via `crypto::sign`/
    /// `random_bytes`. Without one the handler trips on
    /// `block 'wafer-run/crypto' not registered`.
    async fn ctx_with_crypto() -> TestContext {
        TestContext::with_auth_and_crypto().await
    }

    /// Sign a new user up through the real signup handler (REQUIRE_VERIFICATION
    /// is unset/false by default, so this also auto-logs them in — irrelevant
    /// here, we only need the user + local_credentials rows it creates).
    async fn signup_user(ctx: &TestContext, email: &str, password: &str) {
        let body = serde_json::json!({"email": email, "password": password}).to_string();
        let out = signup::handle(ctx, InputStream::from_bytes(body.into_bytes())).await;
        collect_or_panic(out).await;
    }

    async fn login(ctx: &TestContext, email: &str, password: &str) -> serde_json::Value {
        let body = serde_json::json!({"email": email, "password": password}).to_string();
        let out = handle(ctx, InputStream::from_bytes(body.into_bytes())).await;
        output_json(out).await
    }

    #[tokio::test]
    async fn non_admin_login_defaults_to_userportal_not_admin() {
        let ctx = ctx_with_crypto().await;
        signup_user(&ctx, "regular@example.com", "correct-horse-battery").await;

        let resp = login(&ctx, "regular@example.com", "correct-horse-battery").await;

        assert_eq!(
            resp["user"]["roles"],
            serde_json::json!(["user"]),
            "fixture user must not be admin: {resp}"
        );
        assert_eq!(
            resp["default_redirect"], "/b/userportal/",
            "non-admin login must default to the user portal, not the admin-only \
             route (#1 onboarding bug): {resp}"
        );
    }

    #[tokio::test]
    async fn admin_login_still_defaults_to_configured_admin_default() {
        let mut ctx = ctx_with_crypto().await;
        // Matches the signup-time `initial_role_for` rule, so the seeded user
        // is created with role "admin" directly.
        ctx.set_config(
            "WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_EMAIL",
            "admin@example.com",
        );
        signup_user(&ctx, "admin@example.com", "correct-horse-battery").await;

        let resp = login(&ctx, "admin@example.com", "correct-horse-battery").await;

        assert_eq!(
            resp["user"]["roles"],
            serde_json::json!(["admin"]),
            "fixture user must be admin: {resp}"
        );
        assert_eq!(
            resp["default_redirect"], "/b/admin/",
            "admin login must keep defaulting to the admin home: {resp}"
        );
    }

    #[tokio::test]
    async fn admin_login_honors_custom_configured_admin_default() {
        let mut ctx = ctx_with_crypto().await;
        ctx.set_config(
            "WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_EMAIL",
            "admin@example.com",
        );
        ctx.set_config("WAFER_RUN_SHARED__POST_LOGIN_REDIRECT", "/b/admin/reports");
        signup_user(&ctx, "admin@example.com", "correct-horse-battery").await;

        let resp = login(&ctx, "admin@example.com", "correct-horse-battery").await;

        assert_eq!(resp["default_redirect"], "/b/admin/reports");
    }

    /// The equalization password is a public constant in this repository, and
    /// a user row whose `local_credentials` row is missing (an interrupted
    /// signup, a half-restored backup) takes the equalization arm. If that
    /// comparison's result were treated as a login, anyone could sign in as
    /// such an account by typing the constant.
    #[tokio::test]
    async fn the_timing_equalization_password_cannot_log_anyone_in() {
        use crate::blocks::auth::repo::users;

        let ctx = ctx_with_crypto().await;
        users::insert(
            &ctx,
            users::NewUser {
                email: "credentialless@example.com".into(),
                display_name: "No Credentials".into(),
                avatar_url: None,
                role: "user".into(),
                email_verified: true,
                verification_token_hash: None,
            },
        )
        .await
        .expect("seed a user with no local-credentials row");

        let body = serde_json::json!({
            "email": "credentialless@example.com",
            "password": TIMING_EQUALIZATION_PASSWORD,
        })
        .to_string();
        let out = handle(&ctx, InputStream::from_bytes(body.into_bytes())).await;

        match out.collect_buffered().await {
            Err(wafer_run::TerminalNotResponse::Error(err)) => assert_eq!(
                err.detail_code(),
                Some("invalid_credentials"),
                "expected the generic invalid-credentials answer, got {err:?}"
            ),
            other => panic!(
                "the timing-equalization password signed a credential-less user in: {other:?}"
            ),
        }
    }
}
