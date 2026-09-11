//! GET/POST /b/auth/api/verify and POST /b/auth/api/resend-verification —
//! relocated from auth/login.rs in Task 5.

use maud::html;
use wafer_core::clients::crypto;
use wafer_run::{context::Context, InputStream, Message, OutputStream};

use crate::{
    blocks::{auth::repo::users, auth_ui::contracts::MessageResponse},
    http::{err_bad_request, err_internal, ok_json},
    ui,
    ui::{components::auth_panel, icons, templates::auth_split},
    util::{hex_encode, sha256_hex},
};

pub async fn handle(ctx: &dyn Context, msg: &Message, input: InputStream) -> OutputStream {
    // Through the async loader, not `ctx.config_get`: that snapshot is frozen
    // at boot, so an admin's saved branding never reached this page without a
    // restart, and on Cloudflare never reached it at all.
    let site = ui::SiteConfig::load_for_auth(ctx).await;
    let logo_url = site.logo_url.clone();
    let app_name = site.app_name.clone();
    let auth_headline = site.auth_headline.clone();
    let auth_tagline = site.auth_tagline.clone();

    // Token comes from query param or body
    let token = {
        let q = msg.get_meta("req.query.token").to_string();
        if !q.is_empty() {
            q
        } else {
            #[derive(serde::Deserialize)]
            struct Req {
                token: String,
            }
            let raw = input.collect_to_bytes().await;
            match serde_json::from_slice::<Req>(&raw) {
                Ok(r) => r.token,
                Err(_) => return err_bad_request("Missing verification token"),
            }
        }
    };

    if token.is_empty() {
        return err_bad_request("Missing verification token");
    }

    // Find user by verification token. The DB column stores
    // `sha256_hex(raw)`; hash the supplied token the same way before
    // comparing.
    let user = match users::find_by_verification_token(ctx, &sha256_hex(token.as_bytes())).await {
        Ok(Some(user)) => user,
        // No row carries this digest: the token was already used, rotated
        // away, or never minted. That is the real invalid-or-expired link.
        Ok(None) => {
            return html_respond(
                "Invalid Link",
                "This verification link is invalid or has expired. Please request a new one.",
                false,
                &logo_url,
                &app_name,
                &auth_headline,
                &auth_tagline,
            )
        }
        // A read that could not run is not a bad link. This endpoint is not
        // an enumeration surface — the caller already holds the token — so
        // there is nothing to protect by lying, and the page's own advice
        // ("request a new one") sends the holder of a good token to
        // `resend-verification`, which reads the same table and replaces the
        // token they were holding.
        Err(e) => return err_internal("Could not check the verification token", e),
    };

    if user.email_verified {
        return html_respond(
            "Email Already Verified",
            "Your email has already been verified. You can sign in now.",
            true,
            &logo_url,
            &app_name,
            &auth_headline,
            &auth_tagline,
        );
    }

    // Mark as verified + clear token in one typed write.
    if let Err(e) = users::mark_email_verified(ctx, &user.id).await {
        return err_internal("Failed to verify email", e.to_string());
    }

    html_respond(
        "Email Verified",
        "Your email has been verified successfully. You can now sign in.",
        true,
        &logo_url,
        &app_name,
        &auth_headline,
        &auth_tagline,
    )
}

pub async fn handle_resend(ctx: &dyn Context, input: InputStream) -> OutputStream {
    #[derive(serde::Deserialize)]
    struct Req {
        email: String,
    }
    let raw = input.collect_to_bytes().await;
    let body: Req = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    let email_lower = body.email.trim().to_lowercase();
    // The endpoint is public. Every branch below answers this same body so
    // an anonymous caller cannot tell a registered address from an
    // unregistered one, an already-verified account from an unverified one,
    // or an account inside its cooldown from one outside it.
    let safe_msg = "If that email is registered, a verification link has been sent.";
    let constant = || {
        ok_json(&MessageResponse {
            message: safe_msg.to_string(),
        })
    };

    // DELIBERATE, do not "fix": the `Err` arm is folded into the constant
    // response on purpose. It is the same collapse the T4 sweep removes
    // everywhere else, and here it is the feature — an answer that varied
    // with the submitted address, for any reason, is the enumeration oracle
    // the paragraph above closes. The response stays constant; the failure
    // is logged so an outage on this endpoint is still findable, which is
    // the part that was missing.
    let user = match users::find_by_email(ctx, &email_lower).await {
        Ok(Some(user)) => user,
        Ok(None) => return constant(),
        Err(e) => {
            tracing::error!(error = %e, "resend-verification: user lookup failed");
            return constant();
        }
    };

    if user.email_verified {
        return constant();
    }

    // 60 second cooldown: inside it, neither mint a token nor say so.
    let last_sent = users::last_verification_sent(ctx, &user.id)
        .await
        .unwrap_or_default();
    if !last_sent.is_empty() {
        if let Ok(last) = chrono::DateTime::parse_from_rfc3339(&last_sent) {
            let elapsed = chrono::Utc::now() - last.with_timezone(&chrono::Utc);
            if elapsed.num_seconds() < 60 {
                return constant();
            }
        }
    }

    // Generate new token. The raw token goes in the email link; only its
    // SHA-256 hex digest is persisted so a row-read leak doesn't grant
    // verification.
    let new_token = match crypto::random_bytes(ctx, 32).await {
        Ok(bytes) => hex_encode(&bytes),
        Err(e) => return err_internal("Token generation failed", e),
    };
    let new_token_hash = sha256_hex(new_token.as_bytes());

    let now = crate::util::now_rfc3339();
    if let Err(e) = users::set_verification_token(ctx, &user.id, &new_token_hash, &now).await {
        return err_internal("Failed to update token", e.to_string());
    }

    super::send_template_email(ctx, "verification", &email_lower, &new_token).await;

    constant()
}

/// Return an HTML page response (for verify endpoints opened in browser).
fn html_respond(
    title: &str,
    message: &str,
    success: bool,
    logo_url: &str,
    app_name: &str,
    auth_headline: &str,
    auth_tagline: &str,
) -> OutputStream {
    // Static modifier rather than an inline `--icon-color`/`--icon-bg` pair:
    // the two states are fixed, so their colours belong in the stylesheet
    // where the contrast guard can see them (see auth-split.css).
    let icon_state = if success {
        "auth-status__icon--success"
    } else {
        "auth-status__icon--failure"
    };
    let config = ui::SiteConfig {
        app_name: app_name.to_string(),
        logo_url: logo_url.to_string(),
        logo_icon_url: String::new(),
        favicon_url: crate::ui::assets::favicon_url(),
        primary_color: String::new(),
        embedded_scripts: Vec::new(),
        auth_headline: auth_headline.to_string(),
        auth_tagline: auth_tagline.to_string(),
    };
    let markup = ui::layout::page(
        title,
        &config,
        auth_split(
            auth_panel(&config, Some("Verify your email.")),
            html! {
                div .login-container {
                    div .auth-status {
                        div class={"auth-status__icon " (icon_state)} aria-hidden="true" {
                            @if success { (icons::check()) } @else { (icons::x()) }
                        }
                        h2 .auth-status__title { (title) }
                        p .auth-status__message { (message) }
                        a .login-button .auth-status__action href="/b/auth/login" {
                            "Go to Sign In"
                        }
                    }
                }
            },
        ),
    );
    ui::html_response(markup)
}

#[cfg(test)]
mod verify_tests {
    use super::*;
    use crate::{
        blocks::auth::repo::users::{self, NewUser},
        test_support::{anon_msg, output_html, output_is_error, TestContext},
    };

    /// A user carrying `token`'s digest in `verification_token`.
    async fn seed_unverified(ctx: &TestContext, token: &str) -> String {
        let user = users::insert(
            ctx,
            NewUser {
                email: "pending@example.com".into(),
                display_name: "Pending".into(),
                avatar_url: None,
                role: "user".into(),
                email_verified: false,
                verification_token_hash: Some(sha256_hex(token.as_bytes())),
            },
        )
        .await
        .expect("insert user");
        user.id
    }

    fn verify_msg(token: &str) -> Message {
        let mut msg = anon_msg("retrieve", "/b/auth/api/verify");
        msg.set_meta("req.query.token", token);
        msg
    }

    /// Unlike `resend`, this endpoint is not an enumeration surface: the
    /// caller already holds the token. A failed lookup used to render the
    /// same "This verification link is invalid or has expired" page as a
    /// genuinely bad token — a 200 that tells the holder of a good link to
    /// throw it away, and whose advice sends them to `resend-verification`,
    /// which reads the same table and replaces the token they were holding.
    #[tokio::test]
    async fn an_unreadable_verification_token_is_an_outage_not_a_bad_link() {
        let ctx = TestContext::with_auth().await;
        let token = "raw-verification-token-0123456789";
        let user_id = seed_unverified(&ctx, token).await;

        // The positive control first, on the same fixture: this token really
        // does verify, so the assertion below is about the outage and not
        // about a token the handler would have refused anyway.
        let verified = output_html(
            handle(
                &ctx,
                &verify_msg(token),
                InputStream::from_bytes(Vec::new()),
            )
            .await,
        )
        .await;
        assert!(
            verified.contains("Email Verified"),
            "the seeded token must verify on a healthy database: {verified}"
        );
        assert!(
            users::find_by_id(&ctx, &user_id)
                .await
                .expect("read back")
                .expect("the row is there")
                .email_verified
        );

        // The same fixture again, with a database whose reads all fail. The
        // token lookup is the handler's first read, so it is the one that
        // fails.
        let ctx = TestContext::with_auth().await;
        seed_unverified(&ctx, token).await;
        let failing = ctx.break_reads();

        let out = handle(
            &failing,
            &verify_msg(token),
            InputStream::from_bytes(Vec::new()),
        )
        .await;

        assert!(
            output_is_error(out, "Internal").await,
            "a failed token lookup must not render the invalid-link page"
        );
    }
}

#[cfg(test)]
mod resend_tests {
    use wafer_run::InputStream;

    use super::*;
    use crate::{
        blocks::auth::repo::users::{self, NewUser},
        test_support::{output_json, TestContext},
    };

    fn body(email: &str) -> InputStream {
        InputStream::from_bytes(
            serde_json::to_vec(&serde_json::json!({ "email": email })).expect("serialize body"),
        )
    }

    async fn seed(ctx: &TestContext, email: &str, verified: bool) -> String {
        let user = users::insert(
            ctx,
            NewUser {
                email: email.into(),
                display_name: "U".into(),
                avatar_url: None,
                role: "user".into(),
                email_verified: false,
                verification_token_hash: None,
            },
        )
        .await
        .expect("insert user");
        users::set_email_verified(ctx, &user.id, verified)
            .await
            .expect("set email_verified");
        user.id
    }

    /// The endpoint is public. An anonymous caller must not be able to tell
    /// a registered address from an unregistered one by the response, so
    /// every branch answers the same constant body: no "already verified",
    /// no "please wait", no `retry_after`.
    #[tokio::test]
    async fn resend_answers_the_same_body_whatever_the_account_state() {
        let ctx = TestContext::with_auth_and_crypto().await;
        seed(&ctx, "verified@example.com", true).await;
        let cooling = seed(&ctx, "cooling@example.com", false).await;
        users::set_verification_token(&ctx, &cooling, "hash", &crate::util::now_rfc3339())
            .await
            .expect("set token");

        let unregistered = output_json(handle_resend(&ctx, body("nobody@example.com")).await).await;
        let already = output_json(handle_resend(&ctx, body("verified@example.com")).await).await;
        let cooldown = output_json(handle_resend(&ctx, body("cooling@example.com")).await).await;

        assert_eq!(
            already, unregistered,
            "a verified account must not be distinguishable from an unregistered one"
        );
        assert_eq!(
            cooldown, unregistered,
            "an account inside its cooldown must not be distinguishable from an unregistered one"
        );
        assert!(unregistered.get("retry_after").is_none());
    }

    /// Constant responses do not relax the cooldown: a request inside the
    /// window neither mints a new token nor moves the cooldown clock.
    #[tokio::test]
    async fn resend_inside_the_cooldown_does_not_rotate_the_token() {
        let ctx = TestContext::with_auth_and_crypto().await;
        let id = seed(&ctx, "cooling@example.com", false).await;
        let sent_at = crate::util::now_rfc3339();
        users::set_verification_token(&ctx, &id, "hash-before", &sent_at)
            .await
            .expect("set token");

        let _ = handle_resend(&ctx, body("cooling@example.com"))
            .await
            .collect_buffered()
            .await;

        assert_eq!(
            users::last_verification_sent(&ctx, &id)
                .await
                .expect("read cooldown"),
            sent_at
        );
    }

    /// The one place in this sweep where a failed read must NOT be
    /// distinguishable from a negative answer. An error here would vary the
    /// response by the submitted address, which is the enumeration oracle
    /// the constant body exists to close. Pinned so a later sweep cannot
    /// "fix" it back into an oracle.
    #[tokio::test]
    async fn resend_answers_the_constant_body_even_when_the_lookup_fails() {
        let ctx = TestContext::with_auth_and_crypto().await;
        seed(&ctx, "known@example.com", false).await;
        let expected = output_json(handle_resend(&ctx, body("known@example.com")).await).await;
        let failing = ctx.break_reads();

        let outage = output_json(handle_resend(&failing, body("known@example.com")).await).await;

        assert_eq!(
            outage, expected,
            "a failed lookup must answer the same constant body as any other state"
        );
    }
}
