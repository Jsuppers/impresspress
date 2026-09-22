//! `impresspress/auth-ui`: the sign-in, sign-up, password and organisation
//! pages, and the auth admin settings page.

use std::sync::Arc;

use wafer_run::{Block, Message};

use super::{Entry, JSON_API};
use crate::{
    blocks::auth_ui::AuthUiBlock,
    test_support::{
        admin_msg, anon_msg, auth_msg,
        htmx::{Fixture, Page, Site},
        TestContext,
    },
};

/// The signed-in user the authenticated pages are rendered for.
const USER: &str = "auth-page-user";

pub(super) fn entry() -> Entry {
    Entry {
        block: "impresspress/auth-ui",
        fixture: Some(fixture),
        exempt: &[
            ("/b/auth/api/api-keys", JSON_API),
            ("/b/auth/api/verify", JSON_API),
            ("/b/auth/api/oauth/providers", JSON_API),
            (
                "/b/auth/oauth/login",
                "redirects the browser to the OAuth provider; renders no page",
            ),
            (
                "/b/auth/oauth/callback",
                "the OAuth provider's return leg: consumes a one-time state and code no \
                 fixture can mint, then redirects",
            ),
        ],
        must_fire: &[],
    }
}

/// The admin settings page as the admin, the account pages as a signed-in
/// user, and the sign-in pages as a visitor — a signed-in visitor is sent on
/// from those.
fn caller(action: &str, path: &str) -> Message {
    if path.starts_with("/b/auth/admin/") {
        admin_msg(action, path)
    } else if matches!(path, "/b/auth/change-password" | "/b/auth/orgs") {
        auth_msg(action, path, USER)
    } else {
        anon_msg(action, path)
    }
}

fn fixture() -> std::pin::Pin<Box<dyn std::future::Future<Output = Fixture>>> {
    Box::pin(async {
        let ctx = TestContext::with_auth_and_crypto().await;
        ctx.seed_auth_user(USER).await;
        Fixture {
            ctx: Arc::new(ctx),
            site: Site(vec![Arc::new(AuthUiBlock::new()) as Arc<dyn Block>]),
            caller,
            pages: vec![
                Page::at("/b/auth/login"),
                Page::at("/b/auth/signup"),
                Page::at("/b/auth/reset-password"),
                Page::at("/b/auth/bootstrap"),
                Page::at("/b/auth/change-password"),
                Page::at("/b/auth/orgs"),
                Page::at("/b/auth/admin/settings"),
            ],
            operator_input: &[],
        }
    })
}
