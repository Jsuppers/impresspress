//! Binds an OAuth round-trip to the browser that started it.
//!
//! The `state` parameter the provider echoes back proves only that *some*
//! flow this deployment started is being completed — not that the browser
//! completing it is the browser that started it. Without a second half, an
//! attacker can run the authorize step in their own browser, keep the
//! resulting `code`/`state` pair, and hand the victim the callback URL: the
//! victim's browser silently signs in to the attacker's account (login CSRF),
//! and anything the victim then saves lands in an account the attacker can
//! read.
//!
//! So `start.rs` also sets a cookie carrying the SHA-256 of the `state_id`,
//! and `callback.rs` refuses any callback whose `state` does not hash to the
//! cookie it arrives with. The attacker's cookie stays in the attacker's
//! browser, so the forged callback has nothing to match.
//!
//! Details that matter:
//!
//! * The cookie holds the *hash*, never the `state_id` itself. The `state_id`
//!   travels in URLs (the authorize redirect, the callback query string) and
//!   so leaks into `Referer` headers, proxy logs and browser history; the
//!   value that binds the flow does not.
//! * `SameSite=Lax`, because the callback arrives as a cross-site top-level
//!   navigation from the provider — `Strict` would withhold the cookie there
//!   and break every sign-in.
//! * `Path=/`, because `IMPRESSPRESS__AUTH_UI__OAUTH_REDIRECT_URI` is
//!   operator-configurable and need not sit under the start endpoint's path.
//! * The start endpoint must therefore be same-origin with the page that
//!   calls it: a cross-origin `fetch` cannot store a `SameSite=Lax` cookie,
//!   and the callback would then have nothing to match against.

use wafer_run::{context::Context, Message};

use crate::{blocks::auth::helpers::cookie_secure_attribute, util::sha256_hex};

/// Cookie name carrying the binding hash.
pub(super) const COOKIE_NAME: &str = "oauth_state";

/// The value stored in the cookie for `state_id`: hex SHA-256, so the cookie
/// never repeats a value that also travels in a URL.
fn binding_hash(state_id: &str) -> String {
    sha256_hex(state_id.as_bytes())
}

/// `Set-Cookie` value binding `state_id` to this browser for `max_age_secs`,
/// which the caller keeps equal to the PKCE state's own TTL so the two halves
/// of a flow expire together.
pub(super) async fn issue(ctx: &dyn Context, state_id: &str, max_age_secs: i64) -> String {
    format!(
        "{COOKIE_NAME}={}; HttpOnly; Path=/; SameSite=Lax; Max-Age={max_age_secs}{}",
        binding_hash(state_id),
        cookie_secure_attribute(ctx).await
    )
}

/// `Set-Cookie` value that removes the binding cookie. Emitted once a callback
/// has redeemed the state, so a single-use binding does not outlive its flow.
pub(super) async fn clear(ctx: &dyn Context) -> String {
    format!(
        "{COOKIE_NAME}=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0{}",
        cookie_secure_attribute(ctx).await
    )
}

/// Whether `msg` carries the binding cookie for `state_id`.
///
/// A missing cookie is a mismatch: the whole point is that a browser which
/// never ran the start endpoint cannot complete the flow.
pub(super) fn matches(msg: &Message, state_id: &str) -> bool {
    let presented = msg.cookie(COOKIE_NAME);
    !presented.is_empty() && presented == binding_hash(state_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestContext;

    fn msg_with_cookie(value: &str) -> Message {
        let mut msg = Message::new("auth.oauth.callback");
        msg.set_meta("http.header.cookie", format!("{COOKIE_NAME}={value}"));
        msg
    }

    #[test]
    fn matches_only_the_hash_of_the_state_id() {
        let state = "abc123";
        assert!(matches(&msg_with_cookie(&binding_hash(state)), state));
        // The raw state id is NOT the cookie value — a caller that echoed the
        // URL parameter into the cookie would not satisfy the binding.
        assert!(!matches(&msg_with_cookie(state), state));
        assert!(!matches(&msg_with_cookie(&binding_hash("other")), state));
    }

    #[test]
    fn a_message_without_the_cookie_never_matches() {
        let msg = Message::new("auth.oauth.callback");
        assert!(!matches(&msg, "abc123"));
        assert!(!matches(&msg_with_cookie(""), "abc123"));
    }

    #[tokio::test]
    async fn issued_cookie_carries_the_hash_and_the_lax_http_only_attributes() {
        let ctx = TestContext::new().await;
        let cookie = issue(&ctx, "abc123", 600).await;
        assert!(
            cookie.starts_with(&format!("{COOKIE_NAME}={}", binding_hash("abc123"))),
            "cookie must carry the hash, not the state id: {cookie}"
        );
        assert!(
            !cookie.contains("=abc123"),
            "state id must not be in the cookie: {cookie}"
        );
        assert!(cookie.contains("; HttpOnly"), "{cookie}");
        assert!(cookie.contains("; SameSite=Lax"), "{cookie}");
        assert!(cookie.contains("; Path=/"), "{cookie}");
        assert!(cookie.contains("; Max-Age=600"), "{cookie}");
    }

    #[tokio::test]
    async fn clear_expires_the_same_cookie() {
        let ctx = TestContext::new().await;
        let cookie = clear(&ctx).await;
        assert!(cookie.starts_with(&format!("{COOKIE_NAME}=;")), "{cookie}");
        assert!(cookie.contains("; Max-Age=0"), "{cookie}");
        assert!(cookie.contains("; Path=/"), "{cookie}");
    }
}
