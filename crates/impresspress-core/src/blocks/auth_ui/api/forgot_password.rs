//! POST /b/auth/api/forgot-password — relocated from auth/login.rs in Task 5.

use wafer_core::clients::crypto;
use wafer_run::{context::Context, InputStream, Message, OutputStream};

use crate::{
    blocks::{auth::repo::users, auth_ui::contracts::MessageResponse, rate_limit::UserRateLimiter},
    http::{err_bad_request, ok_json},
    util::{hex_encode, sha256_hex},
};

pub async fn handle(
    limiter: &UserRateLimiter,
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
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
    let safe_msg = "If that email is registered, a password reset link has been sent.";
    let constant = || {
        ok_json(&MessageResponse {
            message: safe_msg.to_string(),
        })
    };

    // DELIBERATE, do not "fix": like `verify::handle_resend`, this endpoint
    // is public and answers one constant body for every account state, so a
    // failed lookup must answer it too. Separating "no such account" from
    // "the lookup failed" would hand an anonymous caller a signal that
    // varies with the address they submitted — the account-enumeration
    // oracle `safe_msg` exists to close. The response stays constant; the
    // failure is logged so an outage on this endpoint is still findable.
    let user = match users::find_by_email(ctx, &email_lower).await {
        Ok(Some(user)) => user,
        Ok(None) => return constant(),
        Err(e) => {
            tracing::error!(code = ?e.code, error = %e, "forgot-password: user lookup failed");
            return constant();
        }
    };

    // Generate reset token (expires in 1 hour). The raw token goes in the
    // email link; only its SHA-256 hex digest is persisted, so a leak of
    // the row (admin SQL explorer, backup, log dump, any block with read
    // grant on the users table) does not become a password-reset oracle.
    //
    // Both failures below are reachable only for a registered address, so
    // they answer `constant()` too, for the reason the lookup's `Err` arm
    // does: a 403 or 500 here would say "this address has an account"
    // whenever the database or the crypto block is refusing. The code is
    // logged, so a WRAP denial still reads as one to an operator.
    let reset_token = match crypto::random_bytes(ctx, 32).await {
        Ok(bytes) => hex_encode(&bytes),
        Err(e) => {
            tracing::error!(
                code = ?e.code,
                error = %e,
                user_id = %user.id,
                "forgot-password: drawing the reset token failed"
            );
            return constant();
        }
    };
    let reset_token_hash = sha256_hex(reset_token.as_bytes());

    let expires = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    if let Err(e) = users::set_reset_token(ctx, &user.id, &reset_token_hash, &expires).await {
        tracing::error!(
            code = ?e.code,
            error = %e,
            user_id = %user.id,
            "forgot-password: storing the reset token failed"
        );
        return constant();
    }

    // Send the raw token in the email; the hash lives only in the DB.
    if let Err(failure) = super::send_template_email(
        limiter,
        ctx,
        msg,
        "password_reset",
        &email_lower,
        &reset_token,
    )
    .await
    {
        // `constant()` below is the answer for every account state by design
        // (see the DELIBERATE note above), so a send failure cannot change
        // it without reintroducing the enumeration oracle — only a caller
        // whose address IS registered can reach this line at all. It is
        // logged at `error` instead: a reset mail that never left is a user
        // locked out, and the email block's own log says which limit or
        // provider refused it.
        super::log_email_not_sent("forgot-password", &user.id, &failure);
    }

    constant()
}

#[cfg(test)]
mod tests {
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

    /// The deliberate counterpart to the rest of this sweep: a failed lookup
    /// must NOT be distinguishable from "no such account". Anything that
    /// varied with the submitted address would be the account-enumeration
    /// oracle the constant body exists to close. Pinned so a later sweep
    /// cannot "fix" it back into one.
    #[tokio::test]
    async fn the_constant_body_survives_a_failed_lookup() {
        let ctx = TestContext::with_auth_and_crypto().await;
        users::insert(
            &ctx,
            NewUser {
                email: "known@example.com".into(),
                display_name: "Known".into(),
                avatar_url: None,
                role: "user".into(),
                email_verified: true,
                verification_token_hash: None,
            },
        )
        .await
        .expect("insert user");

        let (limiter, msg) = crate::blocks::auth_ui::api::test_mail_request();
        let unregistered =
            output_json(handle(&limiter, &ctx, &msg, body("nobody@example.com")).await).await;
        let failing = ctx.break_reads();
        let outage =
            output_json(handle(&limiter, &failing, &msg, body("known@example.com")).await).await;

        assert_eq!(
            outage, unregistered,
            "a failed lookup must answer the same constant body as an unregistered address"
        );
    }

    /// Everything the HTTP boundary sends for one forgot-password request.
    async fn on_the_wire(ctx: &dyn Context, email: &str) -> (u16, Vec<(String, String)>, String) {
        let (limiter, msg) = crate::blocks::auth_ui::api::test_mail_request();
        let parts = wafer_block::http_codec::collect_http_response(
            handle(&limiter, ctx, &msg, body(email)).await,
        )
        .await;
        (
            parts.status,
            parts.headers,
            String::from_utf8_lossy(&parts.body).into_owned(),
        )
    }

    async fn with_known_user(ctx: &TestContext) {
        users::insert(
            ctx,
            NewUser {
                email: "known@example.com".into(),
                display_name: "Known".into(),
                avatar_url: None,
                role: "user".into(),
                email_verified: true,
                verification_token_hash: None,
            },
        )
        .await
        .expect("insert user");
    }

    /// Same shape one step earlier: drawing the token needs the crypto
    /// block, and only a registered address asks it for anything. A
    /// deployment without it must still answer every address alike.
    #[tokio::test]
    async fn a_failed_reset_token_draw_answers_what_an_unregistered_address_does() {
        let ctx = TestContext::with_auth().await;
        with_known_user(&ctx).await;

        let unregistered = on_the_wire(&ctx, "nobody@example.com").await;
        let registered = on_the_wire(&ctx, "known@example.com").await;

        assert_eq!(
            unregistered.0, 200,
            "the unregistered answer is the constant 200"
        );
        assert_eq!(
            registered, unregistered,
            "a failed token draw must not be visible to the caller"
        );
    }
}
