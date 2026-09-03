//! `GET /b/dev` — the workspace document, and the two static assets it pulls.
//!
//! Gated on `block-dev` for the same reason `dev_status.rs` is: the block
//! does not exist in a default-feature build, so these tests must not even
//! compile there.
#![cfg(feature = "block-dev")]

use impresspress_core::{
    blocks::dev::{assets, test_support::FakeControl},
    test_support::{
        admin_msg, anon_msg, auth_msg, output_html, output_http_header, output_http_status,
        TestContext,
    },
};
use wafer_run::Message;

/// Turn a request into an HTML *navigation* — what a browser sends when the
/// visitor types the URL, as opposed to what `fetch` sends for the JSON API.
///
/// The distinction is load-bearing and not something `anon_msg` can carry on
/// its own: [`impresspress_core::ui::unauthenticated_response`] keys the
/// login redirect off `Accept: text/html`, and answers a plain `403` to
/// everything else so an API caller keeps the JSON contract. A message with
/// no `Accept` at all is therefore the API case, not the page case.
fn navigation(mut msg: Message) -> Message {
    msg.set_meta(
        "http.header.accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    );
    msg
}

// ---------------------------------------------------------------------------
// The document
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dev_page_is_admin_only_cross_origin_isolated_and_uncached() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;

    // Anonymous navigation → the login page, not the workspace. The router
    // is the gate (the page route is declared `Admin` like every other
    // `/b/dev` route), so this is the same 302 any admin page answers.
    assert_eq!(
        output_http_status(
            ctx.dispatch(navigation(anon_msg("retrieve", "/b/dev")))
                .await
        )
        .await,
        302
    );
    // Signed in but not an admin is a genuine refusal, not a login problem.
    assert_eq!(
        output_http_status(
            ctx.dispatch(navigation(auth_msg("retrieve", "/b/dev", "u1")))
                .await
        )
        .await,
        403
    );

    // An `OutputStream` is consumed by reading it, so each header assertion
    // needs its own request.
    for (header, expected) in [
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-embedder-policy", "require-corp"),
        ("cache-control", "no-store"),
    ] {
        assert_eq!(
            output_http_header(ctx.dispatch(admin_msg("retrieve", "/b/dev")).await, header)
                .await
                .as_deref(),
            Some(expected),
            "{header}"
        );
    }

    let html = output_html(ctx.dispatch(admin_msg("retrieve", "/b/dev")).await).await;
    for id in [
        "dev-guide",
        "dev-files",
        "dev-editor",
        "dev-preview",
        "dev-progress",
        "dev-actions",
    ] {
        assert!(html.contains(&format!("id=\"{id}\"")), "{id}");
    }
    // The preview frame's exact opening tag: the `sandbox` attribute is what
    // keeps a page the agent wrote (or a page a shopper's content ends up
    // in) from reaching this document's tools, so it is pinned here rather
    // than left to the markup's shape.
    assert!(html.contains(
        r#"<iframe id="dev-preview-frame" src="/" sandbox="allow-scripts allow-same-origin allow-forms allow-popups""#
    ));
    assert!(html.contains("/b/dev/static/dev.js"));
    assert!(html.contains("/b/dev/static/dev.css"));
    assert!(
        html.contains("admin@example.com"),
        "the guide shows the local credentials"
    );
}

/// COOP/COEP belong to the workspace document alone. Cross-origin isolation
/// is what `SharedArrayBuffer` (and therefore the in-browser compiler) needs;
/// putting it on the whole block would isolate the JSON API for no reason and
/// make the header look like a deployment-wide policy it is not.
#[tokio::test]
async fn other_pages_are_not_cross_origin_isolated() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;
    assert_eq!(
        output_http_header(
            ctx.dispatch(admin_msg("retrieve", "/b/dev/api/status"))
                .await,
            "cross-origin-opener-policy"
        )
        .await,
        None
    );
}

// ---------------------------------------------------------------------------
// The page's own assets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_page_assets_are_served_admin_only_and_revalidated() {
    let ctx = TestContext::with_dev(FakeControl::new()).await;

    for (path, content_type) in [
        (
            "/b/dev/static/dev.js",
            "application/javascript; charset=utf-8",
        ),
        ("/b/dev/static/dev.css", "text/css; charset=utf-8"),
    ] {
        assert_eq!(
            output_http_status(ctx.dispatch(anon_msg("retrieve", path)).await).await,
            403,
            "{path} must be admin-only like the page it belongs to"
        );
        assert_eq!(
            output_http_status(ctx.dispatch(admin_msg("retrieve", path)).await).await,
            200,
            "{path}"
        );
        assert_eq!(
            output_http_header(
                ctx.dispatch(admin_msg("retrieve", path)).await,
                "content-type"
            )
            .await
            .as_deref(),
            Some(content_type),
            "{path}"
        );
        // `no-cache` (revalidate), not `no-store`: these bytes are fixed for
        // a given build, so a conditional request is the right cost — but
        // they carry no content hash, so they must never be held past a
        // rebuild the way the hashed `/b/static/*` bundle is.
        assert_eq!(
            output_http_header(
                ctx.dispatch(admin_msg("retrieve", path)).await,
                "cache-control"
            )
            .await
            .as_deref(),
            Some("no-cache"),
            "{path}"
        );
    }

    assert_eq!(
        output_html(
            ctx.dispatch(admin_msg("retrieve", "/b/dev/static/dev.js"))
                .await
        )
        .await,
        assets::dev_js()
    );
    assert_eq!(
        output_html(
            ctx.dispatch(admin_msg("retrieve", "/b/dev/static/dev.css"))
                .await
        )
        .await,
        assets::dev_css()
    );
}

// ---------------------------------------------------------------------------
// The script itself
// ---------------------------------------------------------------------------

#[test]
fn dev_js_registers_only_from_tools_json_and_never_touches_the_global_manifest() {
    let js = assets::dev_js();
    assert!(js.contains("/b/dev/api/tools.json"));
    assert!(
        !js.contains("/b/webmcp/manifest.json"),
        "the global manifest is webmcp.js's job"
    );
    assert!(js.contains("AbortController"));
    assert!(js.contains("pagehide"));
}

/// The tail is authored as a fragment and only ever served composed with
/// `webmcp-core.js` inside one IIFE — so `buildRequest`/`toolOptions` are in
/// scope, and nothing the tail declares leaks onto `window`.
#[test]
fn dev_js_is_one_composed_iife_over_the_shared_webmcp_core() {
    let js = assets::dev_js();
    assert!(
        js.starts_with("(function () {\n  'use strict';\n"),
        "{js:.40}"
    );
    assert!(js.ends_with("})();\n"));
    // The core half…
    assert!(js.contains("function buildRequest(invocation, args)"));
    assert!(js.contains("function toolOptions(tool)"));
    // …and the tail half, which must not re-open an IIFE of its own. The
    // strict directive is counted WITH its semicolon: both files talk about
    // `'use strict'` in prose, and only the wrapper emits the statement.
    assert!(js.contains("'/b/dev/api/files/write'"));
    assert_eq!(js.matches("'use strict';").count(), 1);
}
