//! POST /b/auth/api/forgot-password — relocated from auth/login.rs in Task 5.

use wafer_core::clients::crypto;
use wafer_run::{context::Context, InputStream, OutputStream};

use crate::{
    blocks::auth::repo::users,
    http::{err_bad_request, err_internal, ok_json},
    util::{hex_encode, sha256_hex},
};

pub async fn handle(ctx: &dyn Context, input: InputStream) -> OutputStream {
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

    // DELIBERATE, do not "fix": like `verify::handle_resend`, this endpoint
    // is public and answers one constant body for every account state, so a
    // failed lookup must answer it too. Separating "no such account" from
    // "the lookup failed" would hand an anonymous caller a signal that
    // varies with the address they submitted — the account-enumeration
    // oracle `safe_msg` exists to close. The response stays constant; the
    // failure is logged so an outage on this endpoint is still findable.
    let user = match users::find_by_email(ctx, &email_lower).await {
        Ok(Some(user)) => user,
        Ok(None) => return ok_json(&serde_json::json!({"message": safe_msg})),
        Err(e) => {
            tracing::error!(error = %e, "forgot-password: user lookup failed");
            return ok_json(&serde_json::json!({"message": safe_msg}));
        }
    };

    // Generate reset token (expires in 1 hour). The raw token goes in the
    // email link; only its SHA-256 hex digest is persisted, so a leak of
    // the row (admin SQL explorer, backup, log dump, any block with read
    // grant on the users table) does not become a password-reset oracle.
    let reset_token = match crypto::random_bytes(ctx, 32).await {
        Ok(bytes) => hex_encode(&bytes),
        Err(e) => return err_internal("Token generation failed", e),
    };
    let reset_token_hash = sha256_hex(reset_token.as_bytes());

    let expires = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    if let Err(e) = users::set_reset_token(ctx, &user.id, &reset_token_hash, &expires).await {
        return err_internal("Failed to store reset token", e.to_string());
    }

    // Send the raw token in the email; the hash lives only in the DB.
    super::send_template_email(ctx, "password_reset", &email_lower, &reset_token).await;

    ok_json(&serde_json::json!({"message": safe_msg}))
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

        let unregistered = output_json(handle(&ctx, body("nobody@example.com")).await).await;
        let failing = ctx.break_reads();
        let outage = output_json(handle(&failing, body("known@example.com")).await).await;

        assert_eq!(
            outage, unregistered,
            "a failed lookup must answer the same constant body as an unregistered address"
        );
    }
}
