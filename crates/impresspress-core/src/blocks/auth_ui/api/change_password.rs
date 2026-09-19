//! POST /b/auth/api/change-password — relocated from auth/login.rs in Task 5.

use maud::html;
use wafer_core::clients::crypto;
use wafer_run::{context::Context, InputStream, Message, OutputStream};

use crate::{
    blocks::{
        auth::{
            bump_auth_version,
            repo::{local_credentials, tokens, users},
        },
        auth_ui::contracts::MessageResponse,
        errors::{error_response, ErrorCode},
    },
    http::{err_bad_request, err_internal, err_not_found, ok_json},
    ui::{html_response, is_htmx},
    util::parse_body_value,
};

/// The answer to a change that happened, in the shape its caller can use.
///
/// The user portal's Security page posts this form with htmx
/// (`blocks/userportal/pages/security.rs`) and swaps the response into
/// `#change-pw-result`, so a JSON body would be rendered into the page as
/// text. `/b/auth/change-password` posts JSON with `fetch` and parses the
/// answer as JSON, as does every programmatic caller, so they keep the
/// [`MessageResponse`] envelope. Same split as
/// [`super::api_keys::handle_create`].
///
/// The htmx wording names the consequence, because the caller is about to
/// meet it: the change revokes every refresh token AND bumps the user's
/// `auth_version`, which retires the access token the page itself is holding
/// (see the `auth_version` check in [`crate::crypto::verify_access_token`]).
fn changed_response(msg: &Message) -> OutputStream {
    if is_htmx(msg) {
        return html_response(html! {
            p .text-success .m-0 {
                "Password changed successfully — sign in again with your new password."
            }
        });
    }
    ok_json(&MessageResponse {
        message: "Password changed successfully".to_string(),
    })
}

/// A refusal the caller can act on, in the shape that caller can read.
///
/// htmx does not swap a non-2xx response, so an error terminal reaches an
/// htmx caller only as a toast — `ui/assets/chrome.js`'s `htmx:responseError`
/// listener — and leaves `#change-pw-result`, the slot the form declares for
/// exactly this answer, empty. The sibling handler on that same page,
/// [`crate::blocks::userportal::pages::security::handle_unlink`], already
/// answers its refusal as 200 markup carrying the reason, so this one does
/// too: one page, one convention, and the sentence naming a wrong password
/// stays on screen instead of expiring with a four-second toast.
///
/// Only refusals travel this way. A read that could not run or a write that
/// did not land is not a sentence the caller can act on, and stays an error
/// terminal so the status is honest and [`err_internal`]'s correlation id
/// reaches the logs — the same split `handle_unlink` makes.
///
/// JSON callers are untouched: `/b/auth/change-password`'s `fetch` reads
/// `r.ok` and the SDK reads the status, so they keep the refusal codes
/// [`error_response`] maps (`401` for a wrong current password, `400` for a
/// password the policy declines).
fn refused(msg: &Message, code: ErrorCode, reason: &str) -> OutputStream {
    if is_htmx(msg) {
        return html_response(html! { p .form-error .m-0 { (reason) } });
    }
    error_response(code, reason)
}

pub async fn handle(ctx: &dyn Context, msg: &Message, input: InputStream) -> OutputStream {
    let user_id = msg.user_id();
    if user_id.is_empty() {
        return error_response(ErrorCode::NotAuthenticated, "Not authenticated");
    }

    #[derive(serde::Deserialize)]
    struct ChangePwReq {
        current_password: String,
        new_password: String,
    }
    let raw = input.collect_to_bytes().await;
    // Two callers, two wire formats: the portal's Security page is an htmx
    // form, so it sends `application/x-www-form-urlencoded`, while
    // `/b/auth/change-password` and programmatic clients send JSON.
    // `parse_body_value` reads either, as `api_keys::handle_create` does for
    // the admin block's key form.
    let parsed = match parse_body_value(&raw) {
        Ok(value) => value,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };
    let body: ChangePwReq = match serde_json::from_value(parsed) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    if let Err((code, reason)) =
        super::password_policy::validate_new_password(ctx, &body.new_password).await
    {
        return refused(msg, code, &reason);
    }

    // Verify user exists. The credential lookup four lines below has always
    // separated "no row" from "the read failed"; this probe folded both into
    // a 404, so an outage told a signed-in caller their account was gone and
    // left the password unchanged.
    match users::find_by_id(ctx, user_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return err_not_found("User not found"),
        Err(e) => return err_internal("Could not load the signed-in user", e),
    };

    // Fetch existing credential row — must have one to change password.
    let cred = match local_credentials::find_by_user_id(ctx, user_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return refused(
                msg,
                ErrorCode::InvalidCredentials,
                "No password set for this account",
            )
        }
        Err(e) => return err_internal("Credential lookup failed", e),
    };

    if crypto::compare_hash(ctx, &body.current_password, &cred.password_hash)
        .await
        .is_err()
    {
        return refused(
            msg,
            ErrorCode::InvalidCredentials,
            "Current password is incorrect",
        );
    }

    let new_hash = match crypto::hash(ctx, &body.new_password).await {
        Ok(h) => h,
        Err(e) => return err_internal("Hash failed", e),
    };

    match local_credentials::update_password(ctx, user_id, &new_hash).await {
        Ok(_) => {
            // Revoke all refresh tokens — force re-login with new password.
            // SEC-032/039: mark rows revoked (don't delete) so the
            // reuse-detection tombstones survive.
            //
            // The credential row has already been updated at this point, so
            // a revocation failure must NOT be reported as success: a
            // refresh token obtained before the password change (e.g. by an
            // attacker who had transient access) would otherwise stay valid
            // even though the user was told their account was secured.
            // There is no cross-op transaction primitive available to block
            // code (`wafer_core::clients::database` has no multi-statement
            // transaction client), so this is the best available durable
            // partial-failure signal: log it and surface a non-success
            // response rather than swallowing it with `.ok()`.
            match tokens::revoke_all_for_user(ctx, user_id).await {
                Ok(()) => {
                    // P2c: invalidate already-issued access JWTs too — refresh
                    // revocation alone doesn't touch a still-live access token,
                    // which would otherwise keep authenticating with the old
                    // password's blessing until its natural expiry. Same
                    // fail-closed treatment as the revocation above: the
                    // credential has already changed, so a failed bump must
                    // not be reported as success.
                    if let Err(e) = bump_auth_version(ctx, user_id).await {
                        tracing::error!(
                            user_id = %user_id,
                            error = %e,
                            "password changed but auth_version bump failed"
                        );
                        return err_internal("Password changed but session invalidation failed", e);
                    }
                    changed_response(msg)
                }
                Err(e) => {
                    tracing::error!(
                        user_id = %user_id,
                        error = %e,
                        "password changed but refresh-token revocation failed"
                    );
                    err_internal("Password changed but session revocation failed", e)
                }
            }
        }
        Err(e) => err_internal("Update failed", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        blocks::auth_ui::api::{login, signup},
        test_support::{
            auth_msg, collect_or_panic, output_is_error, output_json, output_status,
            FailingDbOpContext, TestContext,
        },
    };

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

    fn body(current: &str, new: &str) -> InputStream {
        InputStream::from_bytes(
            serde_json::json!({"current_password": current, "new_password": new})
                .to_string()
                .into_bytes(),
        )
    }

    /// The bytes the user portal's Security form puts on the wire: htmx
    /// serialises a form as `application/x-www-form-urlencoded`, under the
    /// field names those inputs declare in
    /// `blocks/userportal/pages/security.rs`.
    fn form_body(current: &str, new: &str) -> InputStream {
        let encoded = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("current_password", current)
            .append_pair("new_password", new)
            .finish();
        InputStream::from_bytes(encoded.into_bytes())
    }

    /// The request that form arrives in. htmx sets `HX-Request` on every
    /// request it issues, which is what [`crate::ui::is_htmx`] reads.
    fn htmx_msg(user_id: &str) -> Message {
        let mut msg = auth_msg("update", "/b/auth/api/change-password", user_id);
        msg.set_meta("http.header.hx-request", "true");
        msg
    }

    fn credentials(email: &str, password: &str) -> InputStream {
        InputStream::from_bytes(
            serde_json::json!({"email": email, "password": password})
                .to_string()
                .into_bytes(),
        )
    }

    /// The portal's Security page is the surface most users reach this
    /// endpoint from, and it posts a form. Parsing the body as JSON only, the
    /// handler answered `400 Invalid body` to every one of those posts and
    /// the password never changed.
    ///
    /// The new password carries a space, a `+` and a `&` — the characters
    /// form encoding gives its own meaning — so a body that decoded loosely
    /// would store a password nobody could then sign in with.
    #[tokio::test]
    async fn the_portal_form_changes_the_password() {
        const OLD: &str = "original-horse-battery1";
        const NEW: &str = "corr&ct horse+battery 100%";

        let ctx = TestContext::with_auth_and_crypto().await;
        let user_id = signup_user(&ctx, "erin@example.com", OLD).await;

        let out = handle(&ctx, &htmx_msg(&user_id), form_body(OLD, NEW)).await;
        assert_eq!(output_status(out).await, 200, "the form post must succeed");

        let signed_in =
            output_json(login::handle(&ctx, credentials("erin@example.com", NEW)).await).await
                ["access_token"]
                .as_str()
                .map(str::to_string);
        assert!(
            signed_in.is_some_and(|t| !t.is_empty()),
            "the new password must authenticate"
        );
        assert!(
            output_is_error(
                login::handle(&ctx, credentials("erin@example.com", OLD)).await,
                "Unauthenticated"
            )
            .await,
            "the old password must stop authenticating"
        );
    }

    /// The form swaps whatever comes back into `#change-pw-result`, so the
    /// success answer to an htmx caller has to be markup — a JSON envelope
    /// would be rendered into the page as its own text.
    #[tokio::test]
    async fn an_htmx_caller_is_answered_with_markup() {
        let ctx = TestContext::with_auth_and_crypto().await;
        let user_id = signup_user(&ctx, "frank@example.com", "original-horse-battery1").await;

        let out = handle(
            &ctx,
            &htmx_msg(&user_id),
            form_body("original-horse-battery1", "new-horse-battery-2026"),
        )
        .await;

        let buf = collect_or_panic(out).await;
        let content_type = buf
            .meta
            .iter()
            .find(|m| m.key == wafer_block::meta::META_RESP_CONTENT_TYPE)
            .map(|m| m.value.clone())
            .unwrap_or_default();
        assert!(
            content_type.starts_with("text/html"),
            "an htmx caller must be answered as HTML, got {content_type:?}"
        );
        let html = String::from_utf8(buf.body).expect("body was not valid UTF-8");
        assert!(
            html.contains("Password changed successfully"),
            "the fragment must say the change happened, got {html:?}"
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&html).is_err(),
            "a JSON body would be swapped into the page as text, got {html:?}"
        );
    }

    /// A wrong current password is the form's most likely mistake, and until
    /// the body parsed every one of those attempts was reported as a
    /// malformed request instead.
    ///
    /// It comes back as markup, not as an error terminal: htmx does not swap
    /// a non-2xx, so a refusal answered as one leaves `#change-pw-result`
    /// empty and says its piece only in a toast that expires. The sibling
    /// handler on that page (`userportal::pages::security::handle_unlink`)
    /// already answers a refusal as 200 markup.
    #[tokio::test]
    async fn a_form_post_with_the_wrong_current_password_says_so() {
        let ctx = TestContext::with_auth_and_crypto().await;
        let user_id = signup_user(&ctx, "gina@example.com", "original-horse-battery1").await;

        let parts = wafer_block::http_codec::collect_http_response(
            handle(
                &ctx,
                &htmx_msg(&user_id),
                form_body("not-the-current-password", "new-horse-battery-2026"),
            )
            .await,
        )
        .await;

        assert_eq!(
            parts.status, 200,
            "a refusal htmx will not swap never reaches the page"
        );
        let html = String::from_utf8(parts.body).expect("body was not valid UTF-8");
        assert!(
            html.contains("Current password is incorrect") && html.contains("form-error"),
            "the fragment must name the refusal, got {html:?}"
        );

        // The credential is untouched: the account still signs in with what
        // it had.
        assert!(
            output_json(
                login::handle(
                    &ctx,
                    credentials("gina@example.com", "original-horse-battery1")
                )
                .await
            )
            .await["access_token"]
                .as_str()
                .is_some(),
            "a refused change must leave the password alone"
        );
    }

    /// Same for a new password the policy refuses: the caller is told which
    /// rule it broke, in the page, not that their request was malformed.
    #[tokio::test]
    async fn a_form_post_the_policy_refuses_names_the_rule() {
        let ctx = TestContext::with_auth_and_crypto().await;
        let user_id = signup_user(&ctx, "hank@example.com", "original-horse-battery1").await;

        let parts = wafer_block::http_codec::collect_http_response(
            handle(
                &ctx,
                &htmx_msg(&user_id),
                form_body("original-horse-battery1", "123456"),
            )
            .await,
        )
        .await;

        assert_eq!(parts.status, 200);
        let html = String::from_utf8(parts.body).expect("body was not valid UTF-8");
        assert!(
            html.contains("at least") && html.contains("form-error"),
            "the fragment must name the length rule, got {html:?}"
        );
    }

    /// The markup branch is the htmx caller's alone. A JSON client — the
    /// `fetch` on `/b/auth/change-password`, the SDK — reads the status, so
    /// its refusals keep the codes and the envelope they have always had.
    #[tokio::test]
    async fn a_json_caller_still_receives_the_refusal_codes() {
        let ctx = TestContext::with_auth_and_crypto().await;
        let user_id = signup_user(&ctx, "ivan@example.com", "original-horse-battery1").await;
        let msg = auth_msg("update", "/b/auth/api/change-password", &user_id);

        let wrong = wafer_block::http_codec::collect_http_response(
            handle(
                &ctx,
                &msg,
                body("not-the-current-password", "new-horse-battery-2026"),
            )
            .await,
        )
        .await;
        assert_eq!(wrong.status, 401);
        let json: serde_json::Value = serde_json::from_slice(&wrong.body).unwrap_or_default();
        assert_eq!(json["message"], "Current password is incorrect");

        let refused_by_policy = wafer_block::http_codec::collect_http_response(
            handle(&ctx, &msg, body("original-horse-battery1", "123456")).await,
        )
        .await;
        assert_eq!(refused_by_policy.status, 400);
        let json: serde_json::Value =
            serde_json::from_slice(&refused_by_policy.body).unwrap_or_default();
        assert!(
            json["message"]
                .as_str()
                .unwrap_or_default()
                .contains("at least"),
            "a JSON caller keeps the sentence too, got {json}"
        );
    }

    #[tokio::test]
    async fn revocation_failure_does_not_report_success() {
        let ctx = TestContext::with_auth_and_crypto().await;
        let user_id = signup_user(&ctx, "alice@example.com", "original-horse-battery1").await;

        // Fail only the refresh-token revocation write
        // (`tokens::revoke_all_for_user` issues a `database.update_where`
        // against the tokens table); the password credential update itself
        // — a *different* `database.update_where` call, against
        // `local_credentials` — still succeeds.
        let failing = FailingDbOpContext::new(ctx, vec![("database.update_where", tokens::TABLE)]);

        let msg = auth_msg("update", "/b/auth/api/change-password", &user_id);
        let out = handle(
            &failing,
            &msg,
            body("original-horse-battery1", "new-horse-battery-2026"),
        )
        .await;

        assert!(
            output_is_error(out, "Internal").await,
            "a revocation failure must not be reported as success"
        );
    }

    #[tokio::test]
    async fn successful_revocation_still_reports_success() {
        let ctx = TestContext::with_auth_and_crypto().await;
        let user_id = signup_user(&ctx, "bob@example.com", "original-horse-battery1").await;

        let msg = auth_msg("update", "/b/auth/api/change-password", &user_id);
        let out = handle(
            &ctx,
            &msg,
            body("original-horse-battery1", "new-horse-battery-2026"),
        )
        .await;

        let buf = collect_or_panic(out).await;
        let json: serde_json::Value = serde_json::from_slice(&buf.body).unwrap();
        assert_eq!(json["message"], "Password changed successfully");
    }

    /// P2c: a successful password change must bump the user's auth_version
    /// so already-issued access JWTs stop authenticating (see
    /// `crate::crypto::extract_auth_meta`'s `auth_version` check).
    #[tokio::test]
    async fn successful_password_change_bumps_auth_version() {
        use crate::blocks::auth::repo::users;

        let ctx = TestContext::with_auth_and_crypto().await;
        let user_id = signup_user(&ctx, "carol@example.com", "original-horse-battery1").await;
        assert_eq!(users::auth_version(&ctx, &user_id).await.unwrap(), 0);

        let msg = auth_msg("update", "/b/auth/api/change-password", &user_id);
        let out = handle(
            &ctx,
            &msg,
            body("original-horse-battery1", "new-horse-battery-2026"),
        )
        .await;
        let _ = collect_or_panic(out).await;

        assert_eq!(
            users::auth_version(&ctx, &user_id).await.unwrap(),
            1,
            "password change must bump auth_version"
        );
    }

    /// The existence probe folded `Ok(None)` and `Err` into one
    /// `404 User not found`, four lines above a credential lookup that has
    /// always three-way branched. An outage told a signed-in caller their
    /// account was gone and their password was left unchanged.
    #[tokio::test]
    async fn an_unreadable_user_row_is_an_outage_not_a_missing_account() {
        let ctx = TestContext::with_auth_and_crypto().await;
        let user_id = signup_user(&ctx, "dana@example.com", "original-horse-battery1").await;
        // The existence probe is the handler's first database read; the
        // password policy above it reads configuration only.
        let failing = ctx.break_reads();

        let msg = auth_msg("update", "/b/auth/api/change-password", &user_id);
        let out = handle(
            &failing,
            &msg,
            body("original-horse-battery1", "new-horse-battery-2026"),
        )
        .await;

        assert!(
            output_is_error(out, "Internal").await,
            "a failed existence probe must not answer 404"
        );
    }
}
