//! JSON API handlers for the auth-ui block. One handler per leaf module;
//! routed from `auth_ui::AuthUiBlock::handle`.

use wafer_run::{context::Context, InputStream, Message};

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
    /// The email block refused the message before attempting delivery. Its
    /// `Retry-After`-bearing 429 (both outbound rate limits) arrives here.
    Refused(String),
    /// The email block accepted the message and the provider call failed.
    ProviderFailed,
}

impl std::fmt::Display for EmailNotSent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(reason) => write!(f, "refused by the email block: {reason}"),
            Self::ProviderFailed => f.write_str("accepted but the provider send failed"),
        }
    }
}

/// Send a transactional email through the `impresspress/email` block.
///
/// Shared by the signup, email-verify and forgot-password handlers — every
/// caller builds the same `email.send_template` envelope `{template, to,
/// token}` and only the template name differs.
///
/// Returns why the mail did not go out, so a caller can say so in its own
/// terms instead of assuming delivery. Delivery stays best-effort — a
/// failure here must not turn a successful signup or reset request into an
/// error response — but "best-effort" is not "unrecorded": every caller logs
/// the [`EmailNotSent`] it gets back against the flow that produced it.
pub(crate) async fn send_template_email(
    ctx: &dyn Context,
    template: &str,
    to: &str,
    token: &str,
) -> Result<(), EmailNotSent> {
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

        // The first send is admitted by the limiter and then fails at the
        // provider (no `wafer-run/network` here) — already a distinguishable
        // outcome, and NOT a claim of delivery.
        assert_eq!(
            send_template_email(&ctx, "verification", "alice@example.com", "t1").await,
            Err(EmailNotSent::ProviderFailed),
        );

        // The second is over the per-recipient limit, so the block never
        // reaches the provider at all.
        match send_template_email(&ctx, "verification", "alice@example.com", "t2").await {
            Err(EmailNotSent::Refused(reason)) => assert!(
                reason.to_lowercase().contains("rate"),
                "the refusal must carry the block's reason, got {reason:?}"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// A send nobody can perform — the email block is not registered at all —
    /// is a refusal, never a silent success.
    #[tokio::test]
    async fn an_absent_email_block_is_not_reported_as_delivered() {
        let ctx = TestContext::new().await;
        match send_template_email(&ctx, "verification", "alice@example.com", "t1").await {
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
        assert_eq!(
            send_template_email(&ctx, "verification", "alice@example.com", "t1").await,
            Ok(()),
        );
    }
}
