//! POST /b/auth/api/reset-password — relocated from auth/login.rs in Task 5.

use wafer_run::{context::Context, InputStream, OutputStream};

use crate::{
    blocks::{
        auth::{
            end_sessions_after_password_change, hash_new_password,
            repo::{local_credentials, users},
        },
        auth_ui::contracts::MessageResponse,
        crud,
        errors::{error_response, ErrorCode},
    },
    http::{err_bad_request, ok_json},
    util::sha256_hex,
};

pub async fn handle(ctx: &dyn Context, input: InputStream) -> OutputStream {
    #[derive(serde::Deserialize)]
    struct Req {
        token: String,
        new_password: String,
    }
    let raw = match input.collect_to_bytes().await {
        Ok(bytes) => bytes,
        Err(e) => return OutputStream::error(e),
    };
    let body: Req = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    match super::password_policy::validate_new_password(ctx, &body.new_password).await {
        Ok(Ok(())) => {}
        Ok(Err((code, msg))) => return error_response(code, &msg),
        Err(response) => return response,
    }

    // Find user by reset token. The DB column stores `sha256_hex(raw)`;
    // hash the supplied token the same way before comparing.
    let user = match users::find_by_reset_token(ctx, &sha256_hex(body.token.as_bytes())).await {
        Ok(Some(user)) => user,
        // No row carries this digest: the token was already spent or never
        // minted. That is the real invalid token.
        Ok(None) => {
            return error_response(ErrorCode::InvalidToken, "Invalid or expired reset token")
        }
        // A read that could not run is not a bad token. Unlike
        // `forgot_password`, this endpoint is not an enumeration surface —
        // the caller already holds the token — so nothing is protected by
        // calling an outage an expired link, and the advice that follows
        // ("request a new one") destroys the token they are holding.
        Err(e) => return crud::db_error_internal(e, "Could not check the reset token"),
    };

    // Check expiry — reject if missing or malformed (tokens must have an expiry)
    if user.reset_token_expires.is_empty() {
        return error_response(
            ErrorCode::TokenExpired,
            "Reset token has expired. Please request a new one.",
        );
    }
    match chrono::DateTime::parse_from_rfc3339(&user.reset_token_expires) {
        Ok(exp) => {
            if chrono::Utc::now() > exp.with_timezone(&chrono::Utc) {
                return error_response(
                    ErrorCode::TokenExpired,
                    "Reset token has expired. Please request a new one.",
                );
            }
        }
        Err(_) => {
            return error_response(
                ErrorCode::TokenExpired,
                "Reset token has expired. Please request a new one.",
            );
        }
    }

    // Hash new password
    let new_hash = match hash_new_password(ctx, &body.new_password).await {
        Ok(hash) => hash,
        Err(response) => return response,
    };

    // Update credential row (typed path, no password_hash on users table).
    if let Err(e) = local_credentials::update_password(ctx, &user.id, &new_hash).await {
        return crud::db_error_internal(e, "Failed to update password");
    }

    // The credential has changed, so every session the old password opened
    // must end — this is the account-recovery path, where a session that
    // survives is the attacker's. It runs before anything below can return,
    // so no later failure leaves those sessions live; its own failure is
    // answered once the reset token is spent.
    let ended = end_sessions_after_password_change(ctx, &user.id).await;

    // Clear reset token on the users row.
    if let Err(e) = users::clear_reset_token(ctx, &user.id).await {
        return crud::db_error_internal(e, "Failed to clear reset token");
    }

    // A redeemed reset link is mailbox proof of exactly the same strength as
    // a redeemed verification link: this caller received a secret sent to the
    // address and returned it. Recording it here is what gives an account
    // whose address nobody ever proved — every password account on a
    // deployment that does not require verification — a route to becoming
    // one an OAuth identity may join, and it is the recovery path for an
    // address someone else registered first.
    //
    // `record_email_proof` sets `email_verified` along with the proof, so on a
    // deployment that requires verification a reset also satisfies the login
    // gate for a user who never clicked a verification link. That is correct
    // — they just demonstrated the same control that link demonstrates — and
    // it is stated because it is not obvious from the call.
    //
    // Not fatal on failure: the password has already changed, the reset
    // succeeded, and the proof is recorded for the sake of a later sign-in,
    // not this one.
    if let Err(e) = users::record_email_proof(ctx, &user.id, users::proof::EMAIL_TOKEN).await {
        tracing::warn!(
            user_id = %user.id,
            error = %e,
            "password reset succeeded but the address proof was not recorded"
        );
    }

    // A reset whose sessions did not end must not be answered as success.
    if let Err(e) = ended {
        return crud::db_error_internal(e, "Password reset but session invalidation failed");
    }

    ok_json(&MessageResponse {
        message: "Password reset successfully".to_string(),
    })
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::{
        blocks::{
            auth::repo::tokens,
            auth_ui::api::{login, refresh, signup},
        },
        test_support::{
            output_http_status, output_is_error, output_json, FailingDbOpContext, TestContext,
        },
    };

    async fn ctx_with_crypto() -> TestContext {
        TestContext::with_auth_and_crypto().await
    }

    /// Sign a user up through the real signup handler and return their id.
    async fn signup_user(ctx: &TestContext, email: &str, password: &str) -> String {
        let body = serde_json::json!({"email": email, "password": password}).to_string();
        let (limiter, msg) = crate::blocks::auth_ui::api::test_mail_request();
        let out = signup::handle(
            &limiter,
            ctx,
            &msg,
            InputStream::from_bytes(body.into_bytes()),
        )
        .await;
        let json = output_json(out).await;
        json["user"]["id"]
            .as_str()
            .expect("signup response carries user.id")
            .to_string()
    }

    /// Issue a raw reset token for `user_id`, persisting only its hash (this
    /// is exactly what `forgot_password::handle` does), and return the raw
    /// token for submission to `reset_password::handle`.
    async fn issue_reset_token(ctx: &TestContext, user_id: &str) -> String {
        let raw = "test-raw-reset-token-0123456789abcdef";
        let expires = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        users::set_reset_token(ctx, user_id, &sha256_hex(raw.as_bytes()), &expires)
            .await
            .unwrap();
        raw.to_string()
    }

    fn body(token: &str, new_password: &str) -> InputStream {
        InputStream::from_bytes(
            serde_json::json!({"token": token, "new_password": new_password})
                .to_string()
                .into_bytes(),
        )
    }

    /// P2c: a successful password reset must bump the user's auth_version so
    /// an access JWT minted before the reset stops authenticating (mirrors
    /// `change_password.rs`'s `successful_password_change_bumps_auth_version`).
    /// This is the account-recovery path — the MORE security-critical of the
    /// two, since a stale-token attacker surviving a reset is exactly the
    /// threat this feature exists to close.
    #[tokio::test]
    async fn successful_password_reset_bumps_auth_version() {
        let ctx = ctx_with_crypto().await;
        let user_id = signup_user(&ctx, "dave@example.com", "original-horse-battery1").await;
        assert_eq!(users::auth_version(&ctx, &user_id).await.unwrap(), 0);

        let token = issue_reset_token(&ctx, &user_id).await;
        let out = handle(&ctx, body(&token, "new-horse-battery-2026")).await;
        let json = output_json(out).await;
        assert_eq!(json["message"], "Password reset successfully");

        assert_eq!(
            users::auth_version(&ctx, &user_id).await.unwrap(),
            1,
            "password reset must bump auth_version"
        );
    }

    /// A refresh token the account holds, from a sign-in through the real
    /// handler.
    async fn sign_in(ctx: &TestContext, email: &str, password: &str) -> String {
        let creds = serde_json::json!({"email": email, "password": password});
        let signed_in = output_json(
            login::handle(ctx, InputStream::from_bytes(creds.to_string().into_bytes())).await,
        )
        .await;
        signed_in["refresh_token"]
            .as_str()
            .unwrap_or_else(|| panic!("the sign-in succeeded: {signed_in}"))
            .to_string()
    }

    /// Whether `token` still refreshes, through the real refresh handler.
    async fn refreshes(ctx: &TestContext, token: &str) -> bool {
        let body = serde_json::json!({ "refresh_token": token });
        let out =
            refresh::handle(ctx, InputStream::from_bytes(body.to_string().into_bytes())).await;
        !output_is_error(out, "Unauthenticated").await
    }

    /// A refresh-row revocation that fails must not keep the account's
    /// sessions alive — on the recovery path least of all, where a surviving
    /// session is the attacker's. The `auth_version` bump is what ends them,
    /// so it runs whatever the revocation did.
    #[tokio::test]
    async fn a_failed_revocation_still_ends_every_session() {
        let ctx = ctx_with_crypto().await;
        let user_id = signup_user(&ctx, "erin@example.com", "original-horse-battery1").await;
        let old_session = sign_in(&ctx, "erin@example.com", "original-horse-battery1").await;
        let token = issue_reset_token(&ctx, &user_id).await;

        // Fail only the refresh-token revocation write; the credential
        // update itself (a different `database.update_where` call, against
        // `local_credentials`) still succeeds.
        let failing =
            FailingDbOpContext::new(ctx.clone(), vec![("database.update_where", tokens::TABLE)]);
        let status =
            output_http_status(handle(&failing, body(&token, "new-horse-battery-2026")).await)
                .await;

        assert!(
            !tokens::find_by_token(&ctx, &old_session)
                .await
                .expect("token lookup")
                .expect("the row is kept")
                .revoked,
            "precondition: the revocation really failed, so the row alone is live"
        );
        assert_eq!(
            users::auth_version(&ctx, &user_id).await.unwrap(),
            1,
            "a failed revocation must not skip the auth_version bump"
        );
        assert!(
            !refreshes(&ctx, &old_session).await,
            "a refresh token the old password opened must not outlive the reset"
        );
        assert_eq!(
            status, 200,
            "the bump ended every session, so the reset is answered as done"
        );
    }

    /// Spending the reset token comes after the password write, and a
    /// failure there is answered as one — but the sessions the old password
    /// opened have ended by then.
    #[tokio::test]
    async fn a_reset_token_that_cannot_be_spent_still_ends_every_session() {
        let ctx = ctx_with_crypto().await;
        let user_id = signup_user(&ctx, "ella@example.com", "original-horse-battery1").await;
        let old_session = sign_in(&ctx, "ella@example.com", "original-horse-battery1").await;
        let token = issue_reset_token(&ctx, &user_id).await;

        // `users::clear_reset_token` is the reset's only `database.update`
        // on the users table before it returns.
        let failing = FailingDbOpContext::new(ctx.clone(), vec![("database.update", users::TABLE)]);
        let out = handle(&failing, body(&token, "new-horse-battery-2026")).await;
        assert!(
            output_is_error(out, "Internal").await,
            "a reset token left unspent is reported"
        );

        assert_eq!(users::auth_version(&ctx, &user_id).await.unwrap(), 1);
        assert!(
            !refreshes(&ctx, &old_session).await,
            "a refresh token the old password opened must not outlive the reset"
        );
    }

    /// The bump is the invalidation, so a bump that fails is the failure the
    /// caller hears about.
    #[tokio::test]
    async fn a_failed_bump_is_not_reported_as_success() {
        let ctx = ctx_with_crypto().await;
        let user_id = signup_user(&ctx, "edna@example.com", "original-horse-battery1").await;
        let token = issue_reset_token(&ctx, &user_id).await;
        let failing = FailingDbOpContext::new(
            ctx.clone(),
            vec![("database.increment_field_where", users::TABLE)],
        );

        let out = handle(&failing, body(&token, "new-horse-battery-2026")).await;

        assert!(
            output_is_error(out, "Internal").await,
            "a reset whose sessions did not end must not report success"
        );
        assert_eq!(users::auth_version(&ctx, &user_id).await.unwrap(), 0);
    }

    /// The token lookup collapsed a failed read into "Invalid or expired
    /// reset token". A user holding a link that is valid for another 59
    /// minutes was told it had expired; the advice that follows is to
    /// request a new one, which lands on the same outage — and the old token
    /// is destroyed by the new request. This is not an anti-enumeration
    /// answer: the caller already holds the token, so nothing is disclosed
    /// by saying the lookup failed.
    #[tokio::test]
    async fn an_unreadable_reset_token_is_an_outage_not_an_expired_link() {
        let ctx = ctx_with_crypto().await;
        let user_id = signup_user(&ctx, "frank@example.com", "original-horse-battery1").await;
        let token = issue_reset_token(&ctx, &user_id).await;
        // The token lookup is the handler's first database read.
        let failing = ctx.break_reads();

        let out = handle(&failing, body(&token, "new-horse-battery-2026")).await;

        assert!(
            output_is_error(out, "Internal").await,
            "a failed token lookup must not be answered as an invalid token"
        );
    }

    /// A redeemed reset link is mailbox proof: the caller received a secret
    /// sent to the address and returned it. Recording it is what gives an
    /// account whose address nobody proved — every password account on a
    /// deployment that does not require verification — a route to becoming
    /// one an OAuth identity may join, and it is how the owner of an address
    /// someone else registered first recovers it.
    #[tokio::test]
    async fn a_successful_reset_records_the_address_proof() {
        let ctx = ctx_with_crypto().await;
        let user_id = signup_user(&ctx, "gina@example.com", "original-horse-battery1").await;
        let before = users::find_by_id(&ctx, &user_id)
            .await
            .unwrap()
            .expect("row present");
        assert!(
            before.email_verified,
            "precondition: verification is off, so signup flags the row verified"
        );
        assert!(
            !before.email_is_proven(),
            "precondition: nobody proved the address — no mail was ever sent"
        );

        let token = issue_reset_token(&ctx, &user_id).await;
        let out = handle(&ctx, body(&token, "new-horse-battery-2026")).await;
        assert_eq!(
            output_json(out).await["message"],
            "Password reset successfully"
        );

        assert_eq!(
            users::find_by_id(&ctx, &user_id)
                .await
                .unwrap()
                .expect("row present")
                .email_verified_by
                .as_deref(),
            Some(users::proof::EMAIL_TOKEN),
            "the reset must record the proof its link demonstrates"
        );
    }

    /// A password hasher that cannot be reached is a 503 the caller may
    /// retry with the same link: nothing is written, the reset token is not
    /// spent, and once the hasher answers the same token resets the password.
    #[tokio::test]
    async fn an_unreachable_password_hasher_is_a_503_and_keeps_the_link() {
        use crate::test_support::HasherFault;

        let (ctx, hasher) = TestContext::with_auth_and_faulty_hasher().await;
        let user_id = signup_user(&ctx, "hugo@example.com", "original-horse-battery1").await;
        let stored = local_credentials::find_by_user_id(&ctx, &user_id)
            .await
            .unwrap()
            .expect("credential")
            .password_hash;
        let token = issue_reset_token(&ctx, &user_id).await;
        hasher.fail_hash(Some(HasherFault::Unreachable));

        let out = handle(&ctx, body(&token, "new-horse-battery-2026")).await;

        assert_eq!(output_http_status(out).await, 503);
        assert_eq!(
            local_credentials::find_by_user_id(&ctx, &user_id)
                .await
                .unwrap()
                .expect("credential")
                .password_hash,
            stored,
            "the stored password is untouched"
        );
        assert_eq!(users::auth_version(&ctx, &user_id).await.unwrap(), 0);

        hasher.fail_hash(None);
        let json = output_json(handle(&ctx, body(&token, "new-horse-battery-2026")).await).await;
        assert_eq!(json["message"], "Password reset successfully");
    }

    /// Guard: a hashing fault retrying does not fix stays a 500 (passes
    /// before and after the `Unavailable` classification by design).
    #[tokio::test]
    async fn a_broken_password_hasher_is_a_500() {
        use crate::test_support::HasherFault;

        let (ctx, hasher) = TestContext::with_auth_and_faulty_hasher().await;
        let user_id = signup_user(&ctx, "hana@example.com", "original-horse-battery1").await;
        let token = issue_reset_token(&ctx, &user_id).await;
        hasher.fail_hash(Some(HasherFault::Broken));

        let out = handle(&ctx, body(&token, "new-horse-battery-2026")).await;

        assert_eq!(output_http_status(out).await, 500);
    }
}
