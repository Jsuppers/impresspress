//! `/b/userportal/profile` — profile info + display-name edit form, in
//! the shared single-card layout. Sign Out lives in the card footer;
//! Change Password lives on the security page.

use maud::html;
use wafer_run::{context::Context, Message, OutputStream};

use crate::{
    blocks::auth::repo::users,
    http::redirect,
    ui::{components, SiteConfig, UserInfo},
};

pub async fn profile_page(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let user_id = msg.user_id().to_string();
    if user_id.is_empty() {
        return redirect(302, "/b/auth/login");
    }

    let site_config = SiteConfig::load(ctx).await;
    let user = UserInfo::from_message(msg);
    // `UserRow.display_name`, not the `name` alias this page used to read:
    // both are written together by `users::insert` and
    // `users::update_profile`, and `display_name` is the column migration
    // 001 declares NOT NULL, so it is the one that is always populated.
    let row = users::find_by_id(ctx, &user_id).await.ok().flatten();
    let display_name = row
        .as_ref()
        .map(|u| u.display_name.clone())
        .unwrap_or_default();
    let avatar_url = row
        .as_ref()
        .and_then(|u| u.avatar_url.clone())
        .unwrap_or_default();
    let email = user.as_ref().map(|u| u.email.as_str()).unwrap_or("");

    let body = html! {
        section .account-section {
            div .profile-header {
                div .user-avatar .user-avatar--lg {
                    @if !avatar_url.is_empty() {
                        img src=(avatar_url) alt="Avatar";
                    } @else if let Some(u) = &user {
                        (u.avatar_initial())
                    }
                }
                div .profile-header__meta {
                    div .font-semibold .text-16 {
                        @if display_name.is_empty() { (email) } @else { (display_name) }
                    }
                    div .text-muted .text-sm { (email) }
                    @if let Some(u) = &user {
                        div .profile-header__roles {
                            @for role in &u.roles {
                                (components::status_badge(role))
                            }
                        }
                    }
                }
            }
            form action="/b/userportal/update-profile" method="post" {
                (crate::csrf::hidden_field(ctx, msg))

                div .form-group {
                    label .form-label for="display-name" { "Display name" }
                    input .form-input #display-name type="text" name="name"
                        value=(display_name) placeholder="Enter your name";
                }
                button .btn .btn--primary type="submit" .w-full { "Save" }
            }
        }
    };

    super::account_page(&site_config, "Profile", Some("/b/userportal/"), body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{anon_msg, auth_msg, output_html, output_status, TestContext};

    #[tokio::test]
    async fn anonymous_redirects_to_login() {
        let ctx = TestContext::with_auth().await;
        let msg = anon_msg("retrieve", "/b/userportal/profile");
        let resp = profile_page(&ctx, &msg).await;
        assert_eq!(output_status(resp).await, 302);
    }

    #[tokio::test]
    async fn authenticated_renders_profile_form() {
        let ctx = TestContext::with_auth().await;
        let msg = auth_msg("retrieve", "/b/userportal/profile", "user-a");
        let resp = profile_page(&ctx, &msg).await;
        let html = output_html(resp).await;
        assert!(html.contains("Display name"), "missing edit-name form");
        assert!(
            html.contains(r#"name="name""#),
            "missing display-name field"
        );
    }

    #[tokio::test]
    async fn renders_back_link_to_dashboard() {
        let ctx = TestContext::with_auth().await;
        let msg = auth_msg("retrieve", "/b/userportal/profile", "user-a");
        let resp = profile_page(&ctx, &msg).await;
        let html = output_html(resp).await;
        assert!(
            html.contains(r#"href="/b/userportal/""#) && html.contains("account-card__back"),
            "missing back link to dashboard"
        );
    }

    #[tokio::test]
    async fn shell_chrome_is_absent() {
        let ctx = TestContext::with_auth().await;
        let msg = auth_msg("retrieve", "/b/userportal/profile", "user-a");
        let resp = profile_page(&ctx, &msg).await;
        let html = output_html(resp).await;
        assert!(
            !html.contains(r#"class="sidebar""#) && !html.contains(r#"class="topbar""#),
            "single-card layout must not render shell sidebar/topbar"
        );
    }
}
