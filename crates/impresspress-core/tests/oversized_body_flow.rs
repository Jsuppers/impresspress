//! The 413 for an oversized request body keeps the flow's middleware headers.
//!
//! `pipeline::payload_too_large_response` is a **response** terminal carrying
//! `resp.status: 413` rather than an `err_*` error terminal, and that choice is
//! the only thing standing between a cross-origin uploader and an opaque CORS
//! failure. The site-main flow runs `wafer-run/security-headers` and
//! `wafer-run/cors` before the router, and both work by setting `resp.*` meta
//! on the *message*; the flow executor merges that meta into the answer only on
//! the response path, while an error terminal under `on_error: stop`
//! short-circuits the flow and carries none of it.
//!
//! That is a claim about `wafer-flow`'s executor, so it is tested against the
//! real executor rather than restated in a comment: a two-step flow whose first
//! step is a middleware setting a response header, and whose second step
//! answers with the refusal impresspress actually ships. The second case is the
//! guard — the same flow with an `err_*` terminal loses the header, which is
//! what would happen if someone "simplified" the refusal into one.

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
    async fn handle(&self, _c: &dyn Context, _m: Message, _i: InputStream) -> OutputStream {
        payload_too_large_response()
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
