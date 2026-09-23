//! POST /b/auth/api/signup — relocated from auth/login.rs in Task 5.

use wafer_core::clients::{config, crypto};
use wafer_run::{context::Context, InputStream, Message, OutputStream, WaferError};

use crate::{
    blocks::{
        auth::{
            helpers::{
                email_domain_allowed, initial_role_for, issue_tokens_and_cookie, signup_allowed,
            },
            repo::{local_credentials, users},
        },
        auth_ui::{
            contracts::{
                AuthenticatedUser, EmailVerified, PendingSignupUser, SignupRequest, SignupResponse,
                TokenType,
            },
            redirect::{default_post_login_redirect, is_safe_local_redirect},
        },
        crud,
        errors::{error_response, ErrorCode},
        rate_limit::UserRateLimiter,
    },
    http::{err_bad_request, err_internal, ResponseBuilder},
    util::{hex_encode, sha256_hex},
};

/// Returns `Ok(true)` when a user with `email_lower` already exists, `Ok(false)`
/// when not. Any DB failure other than NOT_FOUND propagates — see [SEC-035]
/// note below; collapsing a WRAP denial or connection blip to "email is free"
/// would let a duplicate insert race in past the unique-email constraint.
async fn user_exists(ctx: &dyn Context, email_lower: &str) -> Result<bool, WaferError> {
    Ok(users::find_by_email(ctx, email_lower).await?.is_some())
}

/// The no-auto-login signup response. Under `REQUIRE_VERIFICATION` a fresh
/// signup and an already-registered address both answer exactly this, byte
/// for byte, so the reply cannot tell a caller whether the address has an
/// account ([SEC-035]). That is why it takes nothing but the address the
/// caller sent: no account id, which exists on only one of the two paths,
/// and nothing read back from a row.
///
/// Without `REQUIRE_VERIFICATION` a fresh signup is signed in on the spot and
/// answers tokens instead, so an address that answers this is one that is
/// already registered. That channel is the price of auto-login, not an
/// oversight; a deployment that needs signup not to reveal registered
/// addresses turns verification on.
fn pending_verification(email: String) -> SignupResponse {
    SignupResponse::PendingVerification {
        email_verified: EmailVerified,
        message: "Account created. Please verify your email before signing in.".to_string(),
        user: PendingSignupUser { email },
    }
}

pub async fn handle(
    limiter: &UserRateLimiter,
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    // Enforce ALLOW_SIGNUP on the API (not just the page)
    if !signup_allowed(ctx).await {
        return error_response(ErrorCode::Forbidden, "Signups are currently disabled");
    }

    let raw = input.collect_to_bytes().await;
    let body: SignupRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    let email_lower = body.email.trim().to_lowercase();
    let parts: Vec<&str> = email_lower.splitn(2, '@').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() || !parts[1].contains('.') {
        return error_response(ErrorCode::InvalidEmail, "Invalid email address");
    }

    // Check allowed email domains (if configured)
    if !email_domain_allowed(ctx, &email_lower).await {
        return error_response(
            ErrorCode::InvalidEmail,
            "Signups from this email domain are not allowed",
        );
    }

    if let Err((code, msg)) =
        super::password_policy::validate_new_password(ctx, &body.password).await
    {
        return error_response(code, &msg);
    }
    if email_lower.len() > 255 {
        return error_response(
            ErrorCode::InvalidEmail,
            "Email must not exceed 255 characters",
        );
    }
    if let Some(ref name) = body.name {
        if name.len() > 200 {
            return error_response(
                ErrorCode::InvalidInput,
                "Name must not exceed 200 characters",
            );
        }
    }

    // [SEC-035] If the email is already registered, do NOT confirm that to
    // the caller — answer the reply a fresh signup under REQUIRE_VERIFICATION
    // produces (see `pending_verification` for when that hides the address's
    // state and when it cannot). The signup endpoint is otherwise a free
    // email-enumeration oracle for password-reset / phishing campaigns.
    // What matches is the response, not the time it takes: a fresh signup
    // also hashes the password, inserts two rows and sends mail, and this
    // branch does none of that.
    //
    // Follow-up: send a "someone tried to sign up with your email" notice
    // to the existing account. Not included in this PR — needs the email
    // block's templating to grow a new template, which is out of scope.
    //
    // Use the typed `users::find_by_email` path (NOT_FOUND → Ok(None));
    // any other Err is a real backend failure (WRAP denial, DB outage)
    // that we must surface, not collapse to "email is free".
    let email_already_taken = match user_exists(ctx, &email_lower).await {
        Ok(t) => t,
        Err(e) => return crud::db_error_internal(e, "User lookup failed"),
    };
    if email_already_taken {
        return ResponseBuilder::new()
            .status(201)
            .json(&pending_verification(email_lower));
    }

    // Hash password
    let password_hash = match crypto::hash(ctx, &body.password).await {
        Ok(h) => h,
        Err(e) => return err_internal("Failed to hash password", e),
    };

    // Check if email verification is required
    let require_verification =
        crate::config_vars::get_bool(ctx, "WAFER_RUN__AUTH__REQUIRE_VERIFICATION", false).await;

    // Generate verification token if needed
    let verification_token = if require_verification {
        match crypto::random_bytes(ctx, 32).await {
            Ok(bytes) => hex_encode(&bytes),
            Err(e) => return err_internal("Failed to generate verification token", e),
        }
    } else {
        String::new()
    };

    // Determine the role: admin if the email matches the configured bootstrap
    // admin email (re-uses the same key as bootstrap for consistency).
    let role = initial_role_for(ctx, &email_lower).await;

    // Insert via typed repo — no password_hash on the users row (those live
    // in `local_credentials`), and no `user_roles` row: the initial role is
    // the inline `users.role` column `NewUser.role` writes, which is what
    // `helpers::get_user_roles` reads first.
    //
    // The verification state rides on the insert. It used to be a second
    // `UPDATE` against the row this call had just created, which meant a
    // signup could leave a user verified-by-default if that write failed
    // (it was only warned about).
    let user = match users::insert(
        ctx,
        users::NewUser {
            email: email_lower.clone(),
            display_name: body.name.unwrap_or_default(),
            avatar_url: None,
            role: role.to_string(),
            email_verified: !require_verification,
            // Persist only `sha256_hex(raw)`; the raw token goes out solely
            // in the verification email below.
            verification_token_hash: (!verification_token.is_empty())
                .then(|| sha256_hex(verification_token.as_bytes())),
        },
    )
    .await
    {
        Ok(u) => u,
        Err(e) => return crud::db_error_internal(e, "Failed to create user"),
    };

    if let Err(e) = local_credentials::insert(ctx, &user.id, &password_hash, false).await {
        return crud::db_error_internal(e, "Failed to store credentials");
    }

    let roles = vec![role.to_string()];

    // Send verification email if required
    if require_verification {
        if let Err(failure) = super::send_template_email(
            limiter,
            ctx,
            msg,
            "verification",
            &email_lower,
            &verification_token,
        )
        .await
        {
            // The response below cannot carry this. It is the same body the
            // "[SEC-035] email already registered" branch above answers, and
            // a body that varied with whether mail actually went out would
            // hand an anonymous caller the enumeration oracle that branch
            // exists to close.
            //
            // The account exists and the resend endpoint can mint a fresh
            // token, so the recoverable half is already in the user's hands;
            // the part that was missing is this line.
            super::log_email_not_sent("signup", &user.id, &failure);
        }
        // Do NOT issue tokens before email is verified
        return ResponseBuilder::new()
            .status(201)
            .json(&pending_verification(email_lower));
    }

    // Mint tokens, persist the refresh + session rows, build the cookie
    // (only when email verification is NOT required) — this is the
    // auto-login path: a brand-new user is fully signed in by the time this
    // response reaches the browser, no separate login step needed.
    let issued =
        match issue_tokens_and_cookie(ctx, &user.id, &email_lower, &roles, "password", None, 0)
            .await
        {
            Ok(i) => i,
            Err(r) => return r,
        };

    // Role-aware post-login default (Fix 2 / signup UX): a brand-new signup
    // is (almost) never an admin, so this sends them to `/b/userportal/`
    // instead of the silent bounce to `/b/auth/login` the page used to do —
    // same single-sourced rule Fix 1 applies to login/OAuth/bootstrap.
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
        .status(201)
        .set_cookie(&issued.cookie)
        .json(&SignupResponse::SignedIn {
            email_verified: EmailVerified,
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

/// Signup UX (Fix 2) regression tests. Before this fix, a successful signup
/// with verification NOT required already auto-logged the caller in
/// (tokens issued, cookie set) but the page's JS ignored that and
/// unconditionally navigated to `/b/auth/login` — a silent bounce with no
/// feedback. These tests drive the real [`handle`] end-to-end and assert on
/// the `default_redirect` the (now role-aware) auto-login response carries,
/// plus that the verification-required path still does NOT auto-login.
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::test_support::{output_json, TestContext};

    async fn ctx_with_crypto() -> TestContext {
        let mut ctx = TestContext::with_auth().await;
        let svc = Arc::new(
            wafer_block_crypto::service::Argon2JwtCryptoService::new(
                "test-jwt-secret-padded-to-min-32-bytes-aaaa".to_string(),
            )
            .expect("test secret is long enough"),
        );
        let crypto_block: Arc<dyn wafer_run::Block> =
            Arc::new(wafer_core::service_blocks::crypto::CryptoBlock::new(svc));
        ctx.register_block("wafer-run/crypto", crypto_block);
        ctx
    }

    async fn signup(ctx: &TestContext, email: &str, password: &str) -> serde_json::Value {
        let body = serde_json::json!({"email": email, "password": password}).to_string();
        let (limiter, msg) = crate::blocks::auth_ui::api::test_mail_request();
        let out = handle(
            &limiter,
            ctx,
            &msg,
            InputStream::from_bytes(body.into_bytes()),
        )
        .await;
        output_json(out).await
    }

    #[tokio::test]
    async fn regular_signup_auto_logs_in_and_defaults_to_userportal() {
        let ctx = ctx_with_crypto().await;

        let resp = signup(&ctx, "newuser@example.com", "correct-horse-battery").await;

        assert_eq!(resp["email_verified"], true);
        assert!(
            resp["access_token"].is_string() && !resp["access_token"].as_str().unwrap().is_empty(),
            "verification not required — signup must auto-login (issue a token): {resp}"
        );
        assert_eq!(
            resp["default_redirect"], "/b/userportal/",
            "brand-new non-admin signup must land on the user portal, not \
             bounce to /b/auth/login with no feedback: {resp}"
        );
    }

    #[tokio::test]
    async fn admin_email_signup_defaults_to_admin_home() {
        let mut ctx = ctx_with_crypto().await;
        ctx.set_config(
            "WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_EMAIL",
            "admin@example.com",
        );

        let resp = signup(&ctx, "admin@example.com", "correct-horse-battery").await;

        assert_eq!(resp["user"]["roles"], serde_json::json!(["admin"]));
        assert_eq!(resp["default_redirect"], "/b/admin/");
    }

    #[tokio::test]
    async fn verification_required_does_not_auto_login() {
        let mut ctx = ctx_with_crypto().await;
        ctx.set_config("WAFER_RUN__AUTH__REQUIRE_VERIFICATION", "true");

        let resp = signup(&ctx, "pending@example.com", "correct-horse-battery").await;

        assert_eq!(resp["email_verified"], false);
        assert!(
            resp.get("access_token").is_none(),
            "verification required — signup must NOT auto-login: {resp}"
        );
        assert!(
            resp.get("default_redirect").is_none(),
            "no redirect target is minted when the user isn't logged in yet: {resp}"
        );

        // The account was still created, unverified.
        let user = users::find_by_email(&ctx, "pending@example.com")
            .await
            .unwrap()
            .expect("user row created even though verification is pending");
        assert!(!user.email_verified);
    }

    /// Everything the HTTP boundary would send for one signup attempt:
    /// status, headers (cookies included) and body bytes.
    async fn signup_on_the_wire(
        ctx: &TestContext,
        email: &str,
        password: &str,
    ) -> wafer_block::http_codec::HttpResponseParts {
        let body = serde_json::json!({"email": email, "password": password}).to_string();
        let (limiter, msg) = crate::blocks::auth_ui::api::test_mail_request();
        let out = handle(
            &limiter,
            ctx,
            &msg,
            InputStream::from_bytes(body.into_bytes()),
        )
        .await;
        wafer_block::http_codec::collect_http_response(out).await
    }

    /// [SEC-035] Under REQUIRE_VERIFICATION, signing up with an address
    /// that is already registered answers exactly what signing it up fresh
    /// answered. Both attempts use the same address, so nothing but the
    /// account's existence differs between them, and the comparison is the
    /// whole response a caller receives: a difference anywhere — an account
    /// id on one side, a header, a status — is an oracle for which addresses
    /// have accounts.
    #[tokio::test]
    async fn verification_required_signup_is_byte_identical_for_new_and_registered_addresses() {
        let mut ctx = ctx_with_crypto().await;
        ctx.set_config("WAFER_RUN__AUTH__REQUIRE_VERIFICATION", "true");

        let fresh = signup_on_the_wire(&ctx, "someone@example.com", "correct-horse-battery").await;
        assert!(
            users::find_by_email(&ctx, "someone@example.com")
                .await
                .unwrap()
                .is_some(),
            "the first attempt must create the account, or the second is not the registered case"
        );
        let registered =
            signup_on_the_wire(&ctx, "someone@example.com", "another-password-entirely").await;

        assert_eq!(fresh.status, registered.status, "status");
        assert_eq!(fresh.headers, registered.headers, "headers");
        assert_eq!(
            String::from_utf8_lossy(&fresh.body),
            String::from_utf8_lossy(&registered.body),
            "body"
        );
    }

    #[tokio::test]
    async fn duplicate_email_signup_response_has_no_default_redirect() {
        let ctx = ctx_with_crypto().await;
        signup(&ctx, "dupe@example.com", "correct-horse-battery").await;

        // Second attempt with the same email — [SEC-035] generic response,
        // no tokens, so no redirect target either.
        let resp = signup(&ctx, "dupe@example.com", "some-other-password").await;
        assert!(resp.get("access_token").is_none());
        assert!(resp.get("default_redirect").is_none());
    }

    /// `email_verified` and the tokens are one fact on every path the
    /// endpoint has: verification off or on, a fresh address or a
    /// registered one. Driven through the block's own route table, so the
    /// body checked is the one a browser receives, and each body must also
    /// decode as the published contract.
    #[tokio::test]
    async fn email_verified_is_true_exactly_when_the_reply_carries_tokens() {
        use wafer_run::Block;

        use crate::blocks::auth_ui::AuthUiBlock;

        async fn post(ctx: &TestContext, email: &str) -> (u16, serde_json::Value) {
            let mut msg = crate::test_support::anon_msg("create", "/b/auth/api/signup");
            msg.set_meta(wafer_block::meta::META_REQ_CLIENT_IP, "203.0.113.7");
            let body = serde_json::json!({"email": email, "password": "correct-horse-battery"});
            let parts = wafer_block::http_codec::collect_http_response(
                AuthUiBlock::default()
                    .handle(
                        ctx,
                        msg,
                        InputStream::from_bytes(serde_json::to_vec(&body).expect("body")),
                    )
                    .await,
            )
            .await;
            let json = serde_json::from_slice(&parts.body).expect("signup answers JSON");
            (parts.status, json)
        }

        for require_verification in [false, true] {
            let mut ctx = ctx_with_crypto().await;
            ctx.set_config(
                "WAFER_RUN__AUTH__REQUIRE_VERIFICATION",
                if require_verification {
                    "true"
                } else {
                    "false"
                },
            );
            let fresh = post(&ctx, "someone@example.com").await;
            let registered = post(&ctx, "someone@example.com").await;

            for (path, (status, body)) in [("fresh", fresh), ("registered", registered)] {
                let case = format!("verification {require_verification}, {path} address");
                assert_eq!(status, 201, "{case}: {body}");
                assert_eq!(
                    body["email_verified"].as_bool(),
                    Some(body.get("access_token").is_some()),
                    "{case}: `email_verified` must say whether tokens were issued: {body}"
                );
                let decoded: SignupResponse = serde_json::from_value(body.clone())
                    .unwrap_or_else(|e| panic!("{case}: not the published contract ({e}): {body}"));
                let signed_in = !require_verification && path == "fresh";
                assert_eq!(
                    matches!(decoded, SignupResponse::SignedIn { .. }),
                    signed_in,
                    "{case}: wrong variant: {body}"
                );
            }
        }
    }
}
