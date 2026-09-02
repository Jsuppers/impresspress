//! `GET /b/dev/api/status` — the sandbox control plane's read model.
//!
//! The whole file is gated on `block-dev`: the block does not exist in a
//! default-feature build, so these tests must not even compile there (that
//! absence is itself asserted by `cargo test -p impresspress-core` passing
//! with default features).
#![cfg(feature = "block-dev")]

use impresspress_core::{
    blocks::dev::{test_support::FakeControl, DevBlock, DevShared, ROUTES},
    test_support::{
        admin_msg, anon_msg, auth_msg, output_header, output_http_status, output_json,
        output_status, TestContext,
    },
};
use wafer_run::Block as _;

#[tokio::test]
async fn status_reports_no_generation_on_a_fresh_instance() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let out = ctx
        .dispatch(admin_msg("retrieve", "/b/dev/api/status"))
        .await;
    assert_eq!(output_status(out).await, 200);

    let body = output_json(
        ctx.dispatch(admin_msg("retrieve", "/b/dev/api/status"))
            .await,
    )
    .await;
    assert_eq!(body["active_generation"], serde_json::Value::Null);
    assert_eq!(body["runtime_generation"], 0);
    assert_eq!(body["blocks"], serde_json::json!([]));
    assert_eq!(body["activation"], serde_json::Value::Null);
}

#[tokio::test]
async fn status_is_admin_only() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    assert_eq!(
        output_http_status(
            ctx.dispatch(anon_msg("retrieve", "/b/dev/api/status"))
                .await
        )
        .await,
        403
    );
    assert_eq!(
        output_http_status(
            ctx.dispatch(auth_msg("retrieve", "/b/dev/api/status", "u1"))
                .await
        )
        .await,
        403
    );
}

#[tokio::test]
async fn status_is_never_cached() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    let out = ctx
        .dispatch(admin_msg("retrieve", "/b/dev/api/status"))
        .await;
    assert_eq!(
        output_header(out, "Cache-Control").await.as_deref(),
        Some("no-store")
    );
}

#[test]
fn routes_and_endpoints_stay_in_lockstep() {
    let info = DevBlock::new(DevShared::new(FakeControl::new())).info();
    assert_eq!(ROUTES.len(), info.endpoints.len());
    for route in ROUTES {
        assert!(
            info.endpoints
                .iter()
                .any(|e| e.method == route.method && e.path == route.template),
            "route missing from BlockInfo: {:?} {}",
            route.method,
            route.template,
        );
    }
}
