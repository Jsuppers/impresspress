//! What a failed read answers on the legal pages.
//!
//! A WRAP `PermissionDenied` — a deployment that never granted the block its
//! own table or settings — is a 403, never the 500 these two sites answered.
//! Each test drives the real route through [`LegalPagesBlock`]'s own dispatch
//! as a caller holding no grants, so the refusal is the one
//! `wrap::check_access` gives, and every read before the site under test
//! (the config reads that fall back to their defaults) runs for real.

use wafer_run::{
    streams::output::TerminalNotResponse, Block, ErrorCode, InputStream, OutputStream,
};

use super::{test_ctx, LegalPagesBlock};
use crate::test_support::{admin_msg, anon_msg, TestContext};

/// A legalpages deployment whose caller holds no WRAP grants.
async fn ungranted() -> TestContext {
    test_ctx().await.with_wrap(
        "test/ungranted",
        Vec::new(),
        Vec::new(),
        "impresspress/admin",
    )
}

async fn dispatch(ctx: &TestContext, msg: wafer_run::Message) -> OutputStream {
    LegalPagesBlock::new()
        .handle(ctx, msg, InputStream::empty())
        .await
}

/// The public terms page read its published document and answered a refusal
/// with `err_internal`: a 500.
#[tokio::test]
async fn a_refused_published_document_read_is_403() {
    let ctx = ungranted().await;
    match dispatch(&ctx, anon_msg("retrieve", "/b/legalpages/terms"))
        .await
        .collect_buffered()
        .await
    {
        Err(TerminalNotResponse::Error(error)) => assert_eq!(
            (error.code, error.message.as_str()),
            (ErrorCode::PermissionDenied, "Access denied")
        ),
        Ok(_) => panic!("expected the door's WRAP denial, got a response"),
        Err(_) => panic!("expected the door's WRAP denial, got another terminal"),
    }
}

/// The settings page renders every value through the config service, which
/// WRAP guards like the database. A refusal was the 500 page; it is the 403
/// page, with none of the denial's own text.
#[tokio::test]
async fn a_refused_settings_read_is_the_403_page() {
    let ctx = ungranted().await;
    let mut msg = admin_msg("retrieve", "/b/legalpages/admin/settings");
    msg.set_meta("http.header.accept", "text/html");
    let parts = wafer_block::http_codec::collect_http_response(dispatch(&ctx, msg).await).await;
    let html = String::from_utf8_lossy(&parts.body);
    assert_eq!(parts.status, 403, "{html}");
    assert!(html.contains("Go home"), "{html}");
    assert!(!html.contains("WRAP"), "{html}");
}
