//! GET/POST /b/auth/api/verify and POST /b/auth/api/resend-verification —
//! relocated from auth/login.rs in Task 5.

use maud::html;
use wafer_core::clients::crypto;
use wafer_run::{context::Context, InputStream, Message, OutputStream};

use crate::{
    blocks::auth::repo::users,
    http::{err_bad_request, err_internal, ok_json},
    ui,
    ui::{components::auth_panel, icons, templates::auth_split},
    util::{hex_encode, sha256_hex},
};

pub async fn handle(ctx: &dyn Context, msg: &Message, input: InputStream) -> OutputStream {
    let logo_url = ctx
        .config_get("WAFER_RUN_SHARED__AUTH_LOGO_URL")
        .unwrap_or("")
        .to_string();
    let app_name = ctx
        .config_get("WAFER_RUN_SHARED__APP_NAME")
        .unwrap_or("Impresspress")
        .to_string();
    let auth_headline = ctx
        .config_get("WAFER_RUN_SHARED__AUTH_HEADLINE")
        .unwrap_or(crate::config_vars::DEFAULT_AUTH_HEADLINE)
        .to_string();
    let auth_tagline = ctx
        .config_get("WAFER_RUN_SHARED__AUTH_TAGLINE")
        .unwrap_or(crate::config_vars::DEFAULT_AUTH_TAGLINE)
        .to_string();

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
    let Ok(Some(user)) =
        users::find_by_verification_token(ctx, &sha256_hex(token.as_bytes())).await
    else {
        return html_respond(
            "Invalid Link",
            "This verification link is invalid or has expired. Please request a new one.",
            false,
            &logo_url,
            &app_name,
            &auth_headline,
            &auth_tagline,
        );
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
    let constant = || ok_json(&serde_json::json!({"message": safe_msg}));

    let Ok(Some(user)) = users::find_by_email(ctx, &email_lower).await else {
        return constant();
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
}
