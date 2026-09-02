//! The WebMCP demo Worker: impresspress with the products block, deployed
//! to Cloudflare so a WebMCP-capable browser can register the storefront
//! tools from a public URL.
//!
//! There is deliberately nothing here but the two Worker entry points.
//! Every page of this site carries `ui/assets/webmcp.js`, which fetches
//! `/b/webmcp/manifest.json` (filtered to the visitor's auth level) and
//! registers each tool with `document.modelContext`. The tools themselves
//! are the products block's storefront endpoints, annotated with
//! `.agent_tool(...)` in `impresspress-core/src/blocks/products/mod.rs`.

/// Cloudflare Worker `fetch` entrypoint. Defers everything to
/// [`impresspress_cloudflare::run`]: D1-backed config, R2 storage, the
/// `/_deploy/*` funnel, then WAFER dispatch. No consumer blocks are
/// registered and no post-build wiring is needed, so both hooks are no-ops.
#[cfg(feature = "target-cloudflare")]
#[worker::event(fetch)]
async fn fetch_main(
    req: worker::Request,
    env: worker::Env,
    ctx: worker::Context,
) -> worker::Result<worker::Response> {
    impresspress_cloudflare::run(req, env, ctx, Ok, |_wafer, _storage| Ok(())).await
}

/// Cloudflare Worker `start` entrypoint: one-time isolate initialization
/// before the first fetch event.
#[cfg(feature = "target-cloudflare")]
#[worker::event(start)]
fn start() {
    impresspress_cloudflare::init_isolate();
}
