//! GET /b/auth/login — relocated from auth/pages/mod.rs::login_page in Task 5.

use maud::{html, PreEscaped};
use wafer_run::{context::Context, Message, OutputStream};

use super::{
    login_script, oauth_button_script, oauth_provider_configured, oauth_provider_icon,
    oauth_provider_label, pw_field, pw_toggle_js, site_config,
};
use crate::{
    blocks::auth_ui::redirect::is_safe_local_redirect,
    ui::{
        self,
        components::{alert, auth_panel, oauth_button, AlertVariant},
        templates::auth_split,
    },
};

pub async fn handle(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let config = site_config(ctx);
    let allow_signup = ctx
        .config_get("WAFER_RUN_SHARED__ALLOW_SIGNUP")
        .unwrap_or("true")
        == "true";
    let raw_redirect = msg.get_meta("req.query.redirect").to_string();
    // Validate redirect — only allow relative paths (prevent open redirect)
    let redirect = if is_safe_local_redirect(&raw_redirect) {
        raw_redirect
    } else {
        String::new()
    };
    // Signup UX (Fix 2): the signup page can send a brand-new user here with
    // `?email=...` after redirecting them for email verification, so they
    // don't have to retype it. Rendered as an attribute value — maud
    // HTML-escapes it, and an over-long value is simply dropped rather than
    // rendered.
    let raw_email = msg.get_meta("req.query.email").to_string();
    let prefill_email = if raw_email.len() <= 255 {
        raw_email
    } else {
        String::new()
    };
    let signup_redirect = if redirect.is_empty() {
        String::new()
    } else {
        format!("?redirect={redirect}")
    };

    // OAuth buttons appear only when ENABLE_OAUTH is on AND the provider's
    // full credential triple (CLIENT_ID + CLIENT_SECRET + REDIRECT_URL) is
    // present in env. Avoids rendering a "Continue with GitHub" button that
    // would 4xx as soon as it's clicked.
    let oauth_enabled = ctx
        .config_get("WAFER_RUN_SHARED__ENABLE_OAUTH")
        .unwrap_or("false")
        == "true";
    let oauth_providers: Vec<&'static str> = if oauth_enabled {
        ["github", "google", "microsoft"]
            .iter()
            .copied()
            .filter(|p| oauth_provider_configured(ctx, p))
            .collect()
    } else {
        Vec::new()
    };

    let markup = ui::layout::page(
        "Sign In",
        &config,
        auth_split(
            // `None`: the right-column heading pair below (`.auth-form__title`
            // / `.auth-form__subtitle`) already carries "Sign in to
            // continue." -- passing it here too would render the same
            // sentence twice on one screen.
            auth_panel(&config, None),
            html! {
                div .auth-form {
                    h2 .auth-form__title { "Welcome back" }
                    p .auth-form__subtitle { "Sign in to continue." }

                    (alert(AlertVariant::Error, "error", ""))
                    (alert(AlertVariant::Success, "info", ""))

                    @if !oauth_providers.is_empty() {
                        div .oauth-buttons {
                            @for provider in &oauth_providers {
                                (oauth_button(provider, oauth_provider_label(provider), oauth_provider_icon(provider)))
                            }
                        }
                        div .auth-divider { "or" }
                    }

                    form #form .login-form onsubmit="return handleLogin(event)" {
                        input type="hidden" #redirect value=(redirect);

                        div .form-group {
                            label .form-label for="email" { "Email" }
                            input .form-input type="email" #email placeholder="you@example.com" value=(prefill_email) required;
                        }

                        div .form-group {
                            label .form-label for="password" { "Password" }
                            (pw_field("password", "Enter your password", None))
                        }

                        div .auth-actions {
                            button type="button" class="btn btn-ghost btn-sm" onclick="handleForgot()" {
                                "Forgot password?"
                            }
                        }

                        button .login-button type="submit" #btn { "Sign In" }
                    }

                    @if allow_signup {
                        div .signup-link {
                            "Don't have an account? "
                            a href={"/b/auth/signup" (signup_redirect)} { "Sign up" }
                        }
                    }
                }

                script { (PreEscaped(pw_toggle_js())) }
                script { (PreEscaped(login_script())) }
                @if !oauth_providers.is_empty() {
                    script { (PreEscaped(oauth_button_script())) }
                }
            },
        ),
    );

    ui::html_response(markup)
}

#[cfg(test)]
mod tests {
    use wafer_run::Message;

    use super::handle;
    use crate::test_support::{output_html, TestContext};

    fn login_msg(query: &[(&str, &str)]) -> Message {
        let mut msg = Message::new("http.request");
        for (k, v) in query {
            msg.set_meta(format!("req.query.{k}"), *v);
        }
        msg
    }

    /// Explicit, safe `?redirect=` params must still be honored — rendered
    /// into the hidden `#redirect` field the login script reads first,
    /// ahead of the role-aware `default_redirect` the JSON API returns.
    #[tokio::test]
    async fn renders_safe_redirect_into_hidden_field() {
        let ctx = TestContext::new().await;
        let msg = login_msg(&[("redirect", "/b/userportal/profile")]);
        let html = output_html(handle(&ctx, &msg).await).await;
        assert!(
            html.contains(r#"id="redirect" type="hidden" value="/b/userportal/profile""#),
            "safe redirect must be rendered into the hidden field: {html}"
        );
    }

    /// Open-redirect protection is unchanged by this fix: an unsafe
    /// `?redirect=` (protocol-relative, foreign scheme, etc.) must still be
    /// dropped rather than rendered.
    #[tokio::test]
    async fn rejects_unsafe_redirect_renders_empty_hidden_field() {
        let ctx = TestContext::new().await;
        let msg = login_msg(&[("redirect", "//evil.com")]);
        let html = output_html(handle(&ctx, &msg).await).await;
        assert!(
            html.contains(r#"id="redirect" type="hidden" value="""#),
            "unsafe redirect must not be rendered: {html}"
        );
        assert!(!html.contains("evil.com"));
    }

    /// Signup UX (Fix 2): the signup page can send a brand-new user here
    /// with `?email=...` so they don't have to retype it.
    #[tokio::test]
    async fn prefills_email_from_query_param() {
        let ctx = TestContext::new().await;
        let msg = login_msg(&[("email", "alice@example.com")]);
        let html = output_html(handle(&ctx, &msg).await).await;
        assert!(
            html.contains(r#"value="alice@example.com""#),
            "email query param must prefill the email input: {html}"
        );
    }

    /// Defensive cap — an absurd `?email=` value is dropped rather than
    /// rendered (mirrors the 255-char cap `api/signup.rs` enforces on input).
    #[tokio::test]
    async fn ignores_overlong_email_query_param() {
        let ctx = TestContext::new().await;
        let long_email = format!("{}@example.com", "a".repeat(300));
        let msg = login_msg(&[("email", &long_email)]);
        let html = output_html(handle(&ctx, &msg).await).await;
        assert!(!html.contains(&long_email));
    }

    /// A11y: the visible "Email"/"Password" labels must be programmatically
    /// associated with their inputs via `<label for>` + matching `id`, not
    /// bare `<div>`s a screen reader can't tie to the field.
    #[tokio::test]
    async fn email_and_password_labels_are_associated_with_their_inputs() {
        let ctx = TestContext::new().await;
        let msg = login_msg(&[]);
        let html = output_html(handle(&ctx, &msg).await).await;

        assert!(
            html.contains(r#"<label class="form-label" for="email">Email</label>"#),
            "email label must be a <label for=\"email\"> tied to the #email input: {html}"
        );
        assert!(
            html.contains(r#"id="email""#),
            "input must carry the id the label's for= references: {html}"
        );

        assert!(
            html.contains(r#"<label class="form-label" for="password">Password</label>"#),
            "password label must be a <label for=\"password\"> tied to the #password input: {html}"
        );
        assert!(
            html.contains(r#"id="password""#),
            "input must carry the id the label's for= references: {html}"
        );
    }

    /// A11y: the icon-only password-reveal toggle must have an accessible
    /// name (aria-label), since it renders no visible text.
    #[tokio::test]
    async fn password_toggle_button_has_non_empty_aria_label() {
        let ctx = TestContext::new().await;
        let msg = login_msg(&[]);
        let html = output_html(handle(&ctx, &msg).await).await;

        let marker = "class=\"pw-toggle\"";
        let idx = html
            .find(marker)
            .expect("password toggle button must be present");
        let tag_end = html[idx..]
            .find('>')
            .map(|end| idx + end)
            .unwrap_or(html.len());
        let button_tag = &html[idx..tag_end];

        assert!(
            button_tag.contains("aria-label=\"") && !button_tag.contains("aria-label=\"\""),
            "password toggle button must have a non-empty aria-label: {button_tag}"
        );
    }
}
