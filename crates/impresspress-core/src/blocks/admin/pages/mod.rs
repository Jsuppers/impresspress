//! SSR pages for the admin block.
//!
//! Each page queries the database directly (same patterns as the JSON handlers)
//! and renders HTML via maud.

mod blocks;
mod dashboard;
mod database;
pub(super) mod email;
mod logs;
pub(super) mod network;
pub(super) mod permissions;
pub(super) mod settings;
mod storage;
mod users;
pub(super) mod variables;

// Re-export all public functions so callers can use `pages::dashboard(...)` etc.
pub use blocks::*;
pub use dashboard::*;
pub use database::*;
pub use email::*;
pub use logs::*;
use maud::Markup;
pub use network::*;
pub use permissions::*;
pub use settings::*;
pub use storage::*;
pub use users::*;
pub use variables::*;
use wafer_run::{context::Context, Message, OutputStream};

use crate::ui::{
    self,
    shell::{Crumb, Topbar},
    NavKind, Shell,
};

/// Wrap content in the admin shell: the shared [`ui::shell_page`] with the
/// admin sidebar. The caller passes a `Topbar` describing the page's
/// breadcrumbs, subtitle and optional primary action; site config, the
/// signed-in user and the current path come from `ctx` / `msg` like every
/// other shelled page, and nav entries for blocks that aren't registered on
/// this target are hidden rather than linking into a 404.
pub(crate) async fn admin_page(
    ctx: &dyn Context,
    msg: &Message,
    title: &str,
    topbar: Topbar<'_>,
    content: Markup,
) -> OutputStream {
    ui::shell_page(
        ctx,
        msg,
        Shell {
            title,
            nav: NavKind::Admin,
            crumbs: topbar.crumbs,
            subtitle: topbar.subtitle,
            primary_action: topbar.primary_action,
        },
        content,
    )
    .await
}

/// Convenience: a single top-level breadcrumb with no link.
pub(crate) fn crumb(label: &'static str) -> Vec<Crumb<'static>> {
    vec![Crumb { label, href: None }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{admin_msg, output_body, TestContext};

    /// The admin shell must hide nav entries whose block isn't registered on
    /// this target, exactly as every other shelled page does — otherwise the
    /// Cloudflare and browser builds link to blocks that answer 404.
    #[tokio::test]
    async fn admin_shell_hides_nav_entries_for_unregistered_blocks() {
        // `with_admin` registers no feature blocks, so every block-bound entry
        // (LLM, Vector, Messages, Products, Tickets) must be absent while the
        // plain admin entries stay.
        let ctx = TestContext::with_admin().await;

        let out = logs_page(&ctx, &admin_msg("retrieve", "/b/admin/logs")).await;
        let html = String::from_utf8(output_body(out).await).expect("utf-8 page");

        assert!(
            html.contains("href=\"/b/admin/logs\""),
            "the plain admin entries must still render"
        );
        assert!(
            !html.contains("href=\"/b/llm/\""),
            "an entry for an unregistered block must not render"
        );
    }
}
