//! The 413 for an oversized request body keeps the flow's middleware headers.
//!
//! `pipeline::payload_too_large_response` is a `Halt` carrying the message's
//! meta, rather than an `err_*` error terminal, and that choice is the only
//! thing standing between a cross-origin uploader and an opaque CORS failure.
//! The site-main flow runs `wafer-run/security-headers` and `wafer-run/cors`
//! before the router, and both work by setting `resp.*` meta on the *message*;
//! a `Halt` takes that meta to the wire (as `wafer-block-cors` does for its own
//! preflight 204), while an error terminal under `on_error: stop`
//! short-circuits the flow and carries none of it.
//!
//! That is a claim about `wafer-flow`'s executor, so it is tested against the
//! real executor rather than restated in a comment: a two-step flow whose first
//! step is a middleware setting a response header, and whose second step
//! answers with the refusal impresspress actually ships. The second case is the
//! guard — the same flow with an `err_*` terminal loses the header, which is
//! what would happen if someone "simplified" the refusal into one.
//!
//! The second half of the file drives the **real** `site-main` flow and route
//! table over the real middleware blocks, with the two terminal blocks stubbed,
//! because where the refusal happens decides which requests it covers: a
//! marked request to a path the router hands to `wafer-run/web` must be a 413
//! and not the SPA.

use std::sync::Arc;

use impresspress_core::{http::err_bad_request, pipeline::payload_too_large_response};
use wafer_block::{
    core_types::{LifecycleEvent, WaferError},
    http_codec,
};
use wafer_run::{
    Block, BlockInfo, Context, InputStream, Message, OutputStream, StaticConfigSource, Wafer,
};

/// The header a middleware step puts on the message, standing in for the
/// `Access-Control-Allow-Origin` the real `wafer-run/cors` block sets the same
/// way (`out_msg.set_meta("resp.header.Access-Control-Allow-Origin", …)`).
const MIDDLEWARE_HEADER: &str = "resp.header.Access-Control-Allow-Origin";
const MIDDLEWARE_VALUE: &str = "https://app.example";

/// Middleware: annotate the message and continue, exactly as the CORS and
/// security-headers blocks do.
struct HeaderMiddleware;

#[wafer_block::wafer_async_trait]
impl Block for HeaderMiddleware {
    fn info(&self) -> BlockInfo {
        BlockInfo::new("test/headers", "0.1.0", "test/mw@v1", "sets a resp header")
    }
    async fn handle(&self, _c: &dyn Context, mut msg: Message, _i: InputStream) -> OutputStream {
        msg.set_meta(MIDDLEWARE_HEADER, MIDDLEWARE_VALUE);
        OutputStream::continue_with(msg)
    }
    async fn lifecycle(&self, _c: &dyn Context, _e: LifecycleEvent) -> Result<(), WaferError> {
        Ok(())
    }
}

/// Terminal step: the refusal impresspress serves for an oversized body.
struct RefusingBlock;

#[wafer_block::wafer_async_trait]
impl Block for RefusingBlock {
    fn info(&self) -> BlockInfo {
        BlockInfo::new("test/refuse", "0.1.0", "test/refuse@v1", "413s")
    }
    async fn handle(&self, _c: &dyn Context, m: Message, _i: InputStream) -> OutputStream {
        payload_too_large_response(&m)
    }
    async fn lifecycle(&self, _c: &dyn Context, _e: LifecycleEvent) -> Result<(), WaferError> {
        Ok(())
    }
}

/// The same refusal expressed as an error terminal — what an `err_*` helper
/// would produce.
struct ErrorTerminalBlock;

#[wafer_block::wafer_async_trait]
impl Block for ErrorTerminalBlock {
    fn info(&self) -> BlockInfo {
        BlockInfo::new("test/err", "0.1.0", "test/err@v1", "errors")
    }
    async fn handle(&self, _c: &dyn Context, _m: Message, _i: InputStream) -> OutputStream {
        err_bad_request("request body too large")
    }
    async fn lifecycle(&self, _c: &dyn Context, _e: LifecycleEvent) -> Result<(), WaferError> {
        Ok(())
    }
}

/// A flow shaped like site-main's middleware-then-terminal chain, with the same
/// `on_error: stop`.
fn flow_json(terminal_block: &str) -> String {
    format!(
        r#"{{
            "id": "test-oversized",
            "name": "Oversized body",
            "version": "0.1.0",
            "description": "middleware then terminal",
            "steps": [
                {{ "id": "headers", "block": "test/headers" }},
                {{ "id": "terminal", "block": "{terminal_block}" }}
            ],
            "config": {{ "on_error": "stop" }}
        }}"#
    )
}

async fn run_flow(terminal_block: &str, terminal: Arc<dyn Block>) -> http_codec::HttpResponseParts {
    let mut wafer =
        Wafer::new(Arc::new(StaticConfigSource::default())).expect("build a bare runtime");
    wafer
        .register_block("test/headers", Arc::new(HeaderMiddleware))
        .expect("register the middleware step");
    wafer
        .register_block(terminal_block, terminal)
        .expect("register the terminal step");
    wafer
        .add_flow_json(&flow_json(terminal_block))
        .expect("register the flow");
    let wafer = wafer.start().await.expect("start the runtime");

    let mut msg = Message::new("http.request");
    msg.set_meta("req.action", "create");
    msg.set_meta("req.resource", "/b/storage/api/buckets/p/objects");
    let out = wafer.run("test-oversized", msg, InputStream::empty()).await;
    http_codec::collect_http_response(out).await
}

/// The shipped refusal keeps the middleware's header and its own 413.
#[tokio::test]
async fn the_413_keeps_the_headers_a_middleware_step_set() {
    let parts = run_flow("test/refuse", Arc::new(RefusingBlock)).await;

    assert_eq!(parts.status, 413);
    // The header first: it is the property this test exists for, so a
    // regression should report it rather than whatever the body became.
    let header = parts
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("Access-Control-Allow-Origin"))
        .map(|(_, value)| value.as_str());
    assert_eq!(
        header,
        Some(MIDDLEWARE_VALUE),
        "a response terminal must carry the flow's middleware headers — without \
         this a browser reports a CORS failure instead of the 413: {:?}",
        parts.headers,
    );
    assert_eq!(
        String::from_utf8(parts.body.clone()).unwrap(),
        impresspress_core::streaming::request_too_large_message(),
        "and the body is the plain-text limit, not an error envelope",
    );
}

/// The guard: the same refusal as an error terminal loses them. This is why
/// `payload_too_large_response` is not an `err_*` helper.
#[tokio::test]
async fn an_error_terminal_would_lose_those_headers() {
    let parts = run_flow("test/err", Arc::new(ErrorTerminalBlock)).await;

    assert!(
        !parts
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("Access-Control-Allow-Origin")),
        "if this starts passing the executor changed and the response-terminal \
         requirement can be revisited: {:?}",
        parts.headers,
    );
}

// ---------------------------------------------------------------------------
// The real site-main flow
// ---------------------------------------------------------------------------

/// Stand-in for `wafer-run/web`: the SPA fallback the flow's `/**` route
/// serves. It answers 200 — which is exactly what an oversized POST to an
/// unclaimed path used to receive, with the site's index page as the body —
/// and records that it was reached, so "refused before the router" is asserted
/// rather than inferred from a status.
///
/// The body is a marker string rather than real markup on purpose:
/// `scripts/grep-guard-html.sh` keeps full-page HTML out of every file outside
/// `impresspress-core/src/ui/`, and nothing here depends on its content.
struct SpaFallbackBlock {
    reached: Arc<std::sync::atomic::AtomicBool>,
}

#[wafer_block::wafer_async_trait]
impl Block for SpaFallbackBlock {
    fn info(&self) -> BlockInfo {
        BlockInfo::new("wafer-run/web", "0.1.0", "test/web@v1", "SPA fallback")
    }
    async fn handle(&self, _c: &dyn Context, _m: Message, _i: InputStream) -> OutputStream {
        self.reached
            .store(true, std::sync::atomic::Ordering::SeqCst);
        OutputStream::respond(b"spa-index-page".to_vec())
    }
    async fn lifecycle(&self, _c: &dyn Context, _e: LifecycleEvent) -> Result<(), WaferError> {
        Ok(())
    }
}

/// Stand-in for `impresspress/router`, the block every declared route resolves
/// to. It records being reached for the same reason.
struct ApiRouterBlock {
    reached: Arc<std::sync::atomic::AtomicBool>,
}

#[wafer_block::wafer_async_trait]
impl Block for ApiRouterBlock {
    fn info(&self) -> BlockInfo {
        BlockInfo::new("impresspress/router", "0.1.0", "test/api@v1", "API router")
    }
    async fn handle(&self, _c: &dyn Context, _m: Message, _i: InputStream) -> OutputStream {
        self.reached
            .store(true, std::sync::atomic::Ordering::SeqCst);
        OutputStream::respond(b"api".to_vec())
    }
    async fn lifecycle(&self, _c: &dyn Context, _e: LifecycleEvent) -> Result<(), WaferError> {
        Ok(())
    }
}

/// What one request through the real `site-main` flow produced.
struct FlowRun {
    parts: http_codec::HttpResponseParts,
    api_reached: bool,
    spa_reached: bool,
}

/// Drive `site_main::JSON` with `site_main::default_routes()` — the real flow
/// definition and the real route table — over the real middleware blocks,
/// with the two terminal blocks stubbed so the test can see which one a
/// request reached.
async fn run_site_main(path: &str, marked: bool) -> FlowRun {
    let api_reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let spa_reached = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // `disable_inventory` because `Wafer::new` installs the linked
    // `register_static_block!` middleware itself, and this test registers its
    // own stubs for two of the names. Everything the flow names is registered
    // below, so nothing is missing — and the middleware registered here is the
    // real thing, not a stand-in.
    let mut wafer = wafer_run::WaferBuilder::default()
        .disable_inventory()
        .config_source(Arc::new(StaticConfigSource::default()))
        .build()
        .expect("build a bare runtime");
    // The real middleware, in the flow's own order, so the refusal meets what
    // it meets in production.
    wafer
        .register_block(
            "wafer-run/security-headers",
            Arc::new(wafer_block_security_headers::SecurityHeadersBlock::new()),
        )
        .expect("register security-headers");
    wafer
        .register_block(
            "wafer-run/cors",
            Arc::new(wafer_block_cors::CorsBlock::new()),
        )
        .expect("register cors");
    wafer
        .register_block(
            "wafer-run/readonly-guard",
            Arc::new(wafer_block_readonly_guard::ReadonlyGuardBlock::new()),
        )
        .expect("register readonly-guard");
    wafer
        .register_block(
            "wafer-run/router",
            Arc::new(wafer_block_router::RouterBlock::new()),
        )
        .expect("register router");
    wafer
        .register_block(
            impresspress_core::blocks::body_limit::BLOCK_NAME,
            Arc::new(impresspress_core::blocks::body_limit::BodyLimitBlock::new(
                Vec::new(),
                Arc::new(Vec::new()),
            )),
        )
        .expect("register body-limit");
    wafer
        .register_block(
            "impresspress/router",
            Arc::new(ApiRouterBlock {
                reached: api_reached.clone(),
            }),
        )
        .expect("register the api stub");
    wafer
        .register_block(
            "wafer-run/web",
            Arc::new(SpaFallbackBlock {
                reached: spa_reached.clone(),
            }),
        )
        .expect("register the spa stub");

    impresspress_core::flows::register_site_main(&mut wafer, "*", "", &[])
        .expect("register site-main");
    let wafer = wafer.start().await.expect("start the runtime");

    let mut msg = Message::new("http.request");
    msg.set_meta("req.action", "create");
    msg.set_meta("req.resource", path);
    msg.set_meta("http.header.origin", "https://app.example");
    if marked {
        msg.set_meta(
            impresspress_core::streaming::META_REQ_BODY_TOO_LARGE,
            impresspress_core::streaming::BODY_TOO_LARGE_VALUE,
        );
    }

    let out = wafer.run("site-main", msg, InputStream::empty()).await;
    FlowRun {
        parts: http_codec::collect_http_response(out).await,
        api_reached: api_reached.load(std::sync::atomic::Ordering::SeqCst),
        spa_reached: spa_reached.load(std::sync::atomic::Ordering::SeqCst),
    }
}

/// **The regression this guards.** An oversized POST to a path no route claims
/// reaches `wafer-run/web` through the `/**` fallback, which knows nothing
/// about the marker: it answered `index.html` and a **200** for a request whose
/// body had already been thrown away. Refusing ahead of the router covers every
/// path, not the routed subset.
#[tokio::test]
async fn an_oversized_body_to_an_unrouted_path_is_413_not_the_spa() {
    let run = run_site_main("/some/spa/route", true).await;

    assert_eq!(run.parts.status, 413);
    assert!(
        !run.spa_reached,
        "the SPA fallback must not serve a request whose body was dropped"
    );
    assert!(!run.api_reached);
}

/// And on a declared route, where the pipeline would also have refused.
#[tokio::test]
async fn an_oversized_body_to_a_declared_route_is_413_before_the_router() {
    let run = run_site_main("/b/storage/api/buckets/p/objects", true).await;

    assert_eq!(run.parts.status, 413);
    assert!(
        !run.api_reached,
        "no block is dispatched for a request whose body was dropped"
    );
}

/// The gate is invisible to everything else: both terminals still serve.
#[tokio::test]
async fn an_ordinary_request_still_reaches_its_block() {
    let api = run_site_main("/b/storage/api/buckets/p/objects", false).await;
    assert!(api.api_reached, "a declared route still reaches the router");
    assert_eq!(api.parts.status, 200);

    let spa = run_site_main("/some/spa/route", false).await;
    assert!(spa.spa_reached, "an unclaimed path still reaches the SPA");
    assert_eq!(spa.parts.status, 200);
}

/// The refusal carries the real CORS block's header, not just a stand-in's —
/// the flow's own middleware, in the flow's own order.
#[tokio::test]
async fn the_413_carries_the_real_cors_blocks_header() {
    let run = run_site_main("/some/spa/route", true).await;

    // Stated here too: without it this test passes in the broken state, where
    // the SPA answers 200 and its response carries the same header.
    assert_eq!(run.parts.status, 413);
    let allow_origin = run
        .parts
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("Access-Control-Allow-Origin"))
        .map(|(_, value)| value.as_str());
    assert_eq!(
        allow_origin,
        Some("https://app.example"),
        "a cross-origin uploader must be able to read the 413: {:?}",
        run.parts.headers,
    );
}
