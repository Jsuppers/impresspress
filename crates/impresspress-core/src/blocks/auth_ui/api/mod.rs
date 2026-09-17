//! JSON API handlers for the auth-ui block. One handler per leaf module;
//! routed from `auth_ui::AuthUiBlock::handle`.

use wafer_run::{context::Context, InputStream, Message};

use crate::blocks::rate_limit::{
    check_rate_limit, ip_identity, RateLimit, RateLimitOutcome, UserRateLimiter,
};

pub mod api_keys;
pub mod bootstrap;
pub mod change_password;
pub mod forgot_password;
pub mod login;
pub mod logout;
pub mod me;
mod password_policy;
pub mod refresh;
pub mod reset_password;
pub mod signup;
pub mod verify;

/// Why a transactional email did not go out.
///
/// The email block answers a refusal (rate limit, recipient allow-list,
/// malformed address) as an error stream and a failed Mailgun call as a
/// `200 {"sent": false}` body. Both mean "no mail was sent", and neither
/// used to be distinguishable here from a delivery: the old helper checked
/// the stream for an error, logged a single line, and never looked at
/// `sent` at all, so a provider outage read as success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EmailNotSent {
    /// This requester has spent their [`RateLimit::AUTH_EMAIL`] budget for
    /// the window. Nothing left this block; the email block was never
    /// called, so no shared quota was touched.
    RequesterLimited,
    /// The email block refused the message before attempting delivery. Its
    /// `Retry-After`-bearing 429 (both outbound rate limits) arrives here.
    Refused(String),
    /// The email block accepted the message and the provider call failed.
    ProviderFailed,
}

impl EmailNotSent {
    /// Whether this outcome needs an operator.
    ///
    /// A rate-limit refusal — this requester's budget, this recipient's, or
    /// the deployment ceiling — is abuse handling working as designed and is
    /// reported by whichever limiter made the decision at the level that
    /// decision deserves (`email.rs` logs the deployment ceiling at `error`
    /// precisely because that one does affect everybody). A provider failure
    /// is different: the deployment believes it can send mail and cannot.
    fn needs_an_operator(&self) -> bool {
        match self {
            Self::RequesterLimited | Self::Refused(_) => false,
            Self::ProviderFailed => true,
        }
    }
}

impl std::fmt::Display for EmailNotSent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequesterLimited => {
                f.write_str("this requester has sent their limit of transactional mail")
            }
            Self::Refused(reason) => write!(f, "refused by the email block: {reason}"),
            Self::ProviderFailed => f.write_str("accepted but the provider send failed"),
        }
    }
}

/// A limiter and a request `Message` for a test that drives one of the
/// mail-sending handlers (`signup::handle`, `verify::handle_resend`,
/// `forgot_password::handle`) directly rather than through the block.
///
/// The limiter is fresh on every call, so its buckets start empty. That is
/// deliberate: most callers of these handlers in tests are only using them to
/// create a user, and one shared limiter would make the eleventh signup in a
/// file silently stop sending mail. A test that IS about the outbound budget
/// builds its own limiter and holds on to it across calls.
#[cfg(test)]
pub(crate) fn test_mail_request() -> (UserRateLimiter, Message) {
    let mut msg = Message::new("http.request");
    msg.set_meta(wafer_block::meta::META_REQ_CLIENT_IP, "203.0.113.7");
    (UserRateLimiter::new(), msg)
}

/// Record an outcome from [`send_template_email`] against the flow that
/// produced it, at the level that outcome deserves.
///
/// Every caller answers a body that cannot vary with this (see each call
/// site), so this log line is where the failure lives.
pub(crate) fn log_email_not_sent(flow: &str, user_id: &str, failure: &EmailNotSent) {
    if failure.needs_an_operator() {
        tracing::error!(flow, user_id, %failure, "transactional email was not sent");
    } else {
        tracing::warn!(flow, user_id, %failure, "transactional email was not sent");
    }
}

/// Send a transactional email through the `impresspress/email` block.
///
/// Shared by the signup, email-verify and forgot-password handlers — every
/// caller builds the same `email.send_template` envelope `{template, to,
/// token}` and only the template name differs.
///
/// Spends this requester's [`RateLimit::AUTH_EMAIL`] budget first, keyed by
/// client IP, and gives up without calling the email block when it is empty.
/// This is the block that knows who asked: the email block sees only its
/// caller (always `impresspress/auth-ui`) and the recipient, so its two
/// buckets cap one address's share of the deployment ceiling but not one
/// requester's — a caller naming a fresh address every time spends the whole
/// ceiling under those two alone. Charged here, ten minutes of signups from
/// one IP costs ten messages instead of a hundred. The category resolves
/// `WAFER_RUN_SHARED__RATE_LIMIT_AUTH_EMAIL` like every other bucket, so a
/// deployment behind a shared egress IP can raise or disable it.
///
/// Returns why the mail did not go out, so a caller can say so in its own
/// terms instead of assuming delivery. Delivery stays best-effort — a
/// failure here must not turn a successful signup or reset request into an
/// error response — but "best-effort" is not "unrecorded": every caller hands
/// the [`EmailNotSent`] to [`log_email_not_sent`] with the flow that produced
/// it.
pub(crate) async fn send_template_email(
    limiter: &UserRateLimiter,
    ctx: &dyn Context,
    msg: &Message,
    template: &str,
    to: &str,
    token: &str,
) -> Result<(), EmailNotSent> {
    if let RateLimitOutcome::Limited(_) = check_rate_limit(
        limiter,
        ctx,
        &ip_identity(msg),
        "auth_email",
        RateLimit::AUTH_EMAIL,
    )
    .await
    {
        // The 429 `check_rate_limit` built is dropped on purpose: no caller
        // of this helper may answer one. All three flows answer a body that
        // is constant for every account state, and a 429 that appeared only
        // for addresses which reached the send step would be the
        // account-enumeration oracle those bodies exist to close.
        return Err(EmailNotSent::RequesterLimited);
    }

    let req = serde_json::json!({
        "template": template,
        "to": to,
        "token": token,
    });
    let email_msg = Message {
        kind: "email.send_template".to_string(),
        meta: Vec::new(),
    };
    let body_bytes = serde_json::to_vec(&req).unwrap_or_default();
    let out = ctx
        .call_block(
            "impresspress/email",
            email_msg,
            InputStream::from_bytes(body_bytes),
        )
        .await;
    let buffered = match out.collect_buffered().await {
        Ok(buffered) => buffered,
        Err(e) => return Err(EmailNotSent::Refused(format!("{e:?}"))),
    };
    // `{"sent": false}` is the email block's own report that the Mailgun
    // call failed; it rides a 200, so only the body tells the two apart. An
    // unparseable body is not evidence of delivery either.
    let sent = serde_json::from_slice::<serde_json::Value>(&buffered.body)
        .ok()
        .and_then(|body| body.get("sent").and_then(serde_json::Value::as_bool))
        .unwrap_or(false);
    if sent {
        Ok(())
    } else {
        Err(EmailNotSent::ProviderFailed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::test_support::TestContext;

    /// A context with the REAL email block registered, configured to admit
    /// `per_recipient` messages to any one address per window.
    async fn ctx_with_email(per_recipient: &str) -> TestContext {
        let mut ctx = TestContext::new().await;
        ctx.set_config(
            "IMPRESSPRESS__EMAIL__RATE_LIMIT_PER_RECIPIENT_MAX",
            per_recipient,
        );
        ctx.set_config("IMPRESSPRESS__EMAIL__RATE_LIMIT_WINDOW_SECS", "60");
        ctx.register_block(
            "impresspress/email",
            Arc::new(crate::blocks::email::EmailBlock::new()),
        );
        ctx
    }

    /// The email block answers a rate-limit refusal as an error stream. That
    /// refusal reaches the caller as [`EmailNotSent::Refused`], instead of
    /// being swallowed into a log line the flow cannot see.
    #[tokio::test]
    async fn a_rate_limit_refusal_reaches_the_caller() {
        let ctx = ctx_with_email("1").await;
        let (limiter, msg) = test_mail_request();

        // The first send is admitted by both limiters and then fails at the
        // provider (no `wafer-run/network` here) — already a distinguishable
        // outcome, and NOT a claim of delivery.
        assert_eq!(
            send_template_email(
                &limiter,
                &ctx,
                &msg,
                "verification",
                "alice@example.com",
                "t1"
            )
            .await,
            Err(EmailNotSent::ProviderFailed),
        );

        // The second is over the per-recipient limit, so the block never
        // reaches the provider at all.
        match send_template_email(
            &limiter,
            &ctx,
            &msg,
            "verification",
            "alice@example.com",
            "t2",
        )
        .await
        {
            Err(EmailNotSent::Refused(reason)) => assert!(
                reason.to_lowercase().contains("rate"),
                "the refusal must carry the block's reason, got {reason:?}"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// The per-recipient bucket caps one ADDRESS's share of the deployment
    /// ceiling, not one REQUESTER's: a caller naming a fresh address every
    /// time never refills anyone's bucket and would spend the whole ceiling
    /// alone. `RateLimit::AUTH_EMAIL` is what stops that, and it stops it
    /// here — before the email block is called, so nothing is charged to the
    /// shared quota.
    #[tokio::test]
    async fn one_requester_cannot_spend_the_ceiling_by_naming_new_addresses() {
        let ctx = ctx_with_email("10").await;
        let (limiter, msg) = test_mail_request();

        let budget = RateLimit::AUTH_EMAIL.max_requests;
        for i in 0..budget {
            assert_eq!(
                send_template_email(
                    &limiter,
                    &ctx,
                    &msg,
                    "verification",
                    &format!("victim{i}@example.com"),
                    "t",
                )
                .await,
                Err(EmailNotSent::ProviderFailed),
                "send {i} is within this requester's budget and must reach the email block",
            );
        }

        assert_eq!(
            send_template_email(
                &limiter,
                &ctx,
                &msg,
                "verification",
                "victim-past-the-budget@example.com",
                "t",
            )
            .await,
            Err(EmailNotSent::RequesterLimited),
            "a fresh address does not buy a fresh budget",
        );
    }

    /// The budget is per requester, so a second client IP is unaffected by
    /// the first one's flood.
    #[tokio::test]
    async fn the_requester_budget_does_not_follow_the_recipient() {
        let ctx = ctx_with_email("10").await;
        let (limiter, flooder) = test_mail_request();
        let mut other = flooder.clone();
        other.set_meta(wafer_block::meta::META_REQ_CLIENT_IP, "198.51.100.4");

        for i in 0..RateLimit::AUTH_EMAIL.max_requests {
            let _ = send_template_email(
                &limiter,
                &ctx,
                &flooder,
                "verification",
                &format!("a{i}@example.com"),
                "t",
            )
            .await;
        }
        assert_eq!(
            send_template_email(
                &limiter,
                &ctx,
                &flooder,
                "verification",
                "b@example.com",
                "t"
            )
            .await,
            Err(EmailNotSent::RequesterLimited),
        );
        assert_eq!(
            send_template_email(&limiter, &ctx, &other, "verification", "b@example.com", "t").await,
            Err(EmailNotSent::ProviderFailed),
            "another requester still reaches the email block",
        );
    }

    /// A send nobody can perform — the email block is not registered at all —
    /// is a refusal, never a silent success.
    #[tokio::test]
    async fn an_absent_email_block_is_not_reported_as_delivered() {
        let ctx = TestContext::new().await;
        let (limiter, msg) = test_mail_request();
        match send_template_email(
            &limiter,
            &ctx,
            &msg,
            "verification",
            "alice@example.com",
            "t1",
        )
        .await
        {
            Err(EmailNotSent::Refused(_)) => {}
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// A `200 {"sent": true}` — the only shape that means the message left
    /// the building — is the only one reported as a delivery.
    #[tokio::test]
    async fn only_a_sent_true_body_counts_as_delivered() {
        struct SentOk;

        #[wafer_block::wafer_async_trait]
        impl wafer_run::Block for SentOk {
            fn info(&self) -> wafer_run::BlockInfo {
                wafer_run::BlockInfo::new("impresspress/email", "0.0.1", "service@v1", "stub")
            }
            async fn handle(
                &self,
                _ctx: &dyn Context,
                _msg: Message,
                _input: InputStream,
            ) -> wafer_run::OutputStream {
                crate::http::ok_json(&serde_json::json!({ "sent": true }))
            }
        }

        let mut ctx = TestContext::new().await;
        ctx.register_block("impresspress/email", Arc::new(SentOk));
        let (limiter, msg) = test_mail_request();
        assert_eq!(
            send_template_email(
                &limiter,
                &ctx,
                &msg,
                "verification",
                "alice@example.com",
                "t1"
            )
            .await,
            Ok(()),
        );
    }

    /// Only a provider failure pages an operator. A rate-limit refusal —
    /// this requester's budget or this recipient's — is ordinary abuse
    /// handling and is already reported by the limiter that made the call.
    #[test]
    fn only_a_provider_failure_needs_an_operator() {
        assert!(EmailNotSent::ProviderFailed.needs_an_operator());
        assert!(!EmailNotSent::RequesterLimited.needs_an_operator());
        assert!(!EmailNotSent::Refused("429".into()).needs_an_operator());
    }
}
