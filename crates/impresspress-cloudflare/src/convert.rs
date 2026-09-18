//! HTTP ↔ Message conversion for Cloudflare Workers.
//!
//! Thin platform glue: the protocol mapping (method→action table, request
//! meta layout, response-meta classification, terminal-event mapping) lives
//! in `wafer_block::http_codec`, and the response-streaming decision + framing
//! live in `impresspress_core::streaming` — the same implementations the
//! request pipeline and the browser adapter use. Only worker-type I/O lives
//! here: reading the request body/headers and building the Worker `Response`
//! (buffered or `ReadableStream`-backed).

use futures::StreamExt;
use impresspress_core::streaming::{self, CappedCollect};
use wafer_block::{
    http_codec::{self, HttpResponseParts, ResponseMetaPart},
    meta::META_RESP_CONTENT_TYPE,
    stream::StreamEvent,
    MetaEntry, MetaGet,
};
use wafer_run::{InputStream, Message, OutputStream};
use worker::{Headers, Request, Response, ResponseBuilder, Result};

// ---------------------------------------------------------------------------
// Request conversion
// ---------------------------------------------------------------------------

/// What converting a Worker request produced: a message to dispatch, or the
/// one refusal the conversion itself can decide.
pub enum RequestConversion {
    /// The request converted; dispatch it.
    Ready(Message, InputStream),
    /// The body is larger than [`streaming::MAX_REQUEST_BODY_BYTES`] and was
    /// not read into the isolate. The caller answers 413
    /// ([`request_too_large_response`]) without dispatching.
    TooLarge,
}

/// Convert a Cloudflare Worker Request into a WAFER `(Message, InputStream)`.
///
/// The path is passed through as received: `/api` normalization belongs to
/// `impresspress_core::pipeline::handle_request`, which every transport shares,
/// and a second strip here made `/api/api/x` route as `/x` on this adapter
/// alone.
///
/// An oversized body is [`RequestConversion::TooLarge`] rather than an `Err`:
/// a worker error reaches the client as the opaque 500 `run`'s error arm
/// builds, and "your upload is too big" is not an internal error.
pub async fn worker_request_to_message(req: &Request) -> Result<RequestConversion> {
    let method = req.method().to_string();
    let url = req.url()?;
    let path = url.path().to_string();
    let query = url.query().unwrap_or("").to_string();

    // Reject oversized bodies on the declared Content-Length *before* buffering
    // them into the (128 MB) Worker isolate. The post-read check below is the
    // backstop for chunked / absent-length requests where the header can't be
    // trusted.
    if let Some(len) = req
        .headers()
        .get("content-length")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<usize>().ok())
    {
        if len > streaming::MAX_REQUEST_BODY_BYTES {
            return Ok(RequestConversion::TooLarge);
        }
    }
    // Read the body. A read error here would otherwise be swallowed and turned
    // into an empty body, silently corrupting POST/PUT.
    let mut req_clone = req.clone()?;
    let body = req_clone.bytes().await?;
    if body.len() > streaming::MAX_REQUEST_BODY_BYTES {
        return Ok(RequestConversion::TooLarge);
    }

    // Extract remote address
    let remote_addr = req
        .headers()
        .get("cf-connecting-ip")
        .ok()
        .flatten()
        .or_else(|| req.headers().get("x-forwarded-for").ok().flatten())
        .unwrap_or_else(|| "unknown".to_string());

    let msg = http_codec::build_http_message(&method, &path, &query, &remote_addr, req.headers());

    Ok(RequestConversion::Ready(msg, InputStream::from_bytes(body)))
}

/// The 413 answer to [`RequestConversion::TooLarge`], carrying the enforced
/// limit ([`streaming::request_too_large_message`]) so a client is told the
/// number it has to fit.
pub fn request_too_large_response() -> Result<Response> {
    let headers = Headers::new();
    headers.set("Content-Type", "text/plain; charset=utf-8")?;
    Ok(ResponseBuilder::new()
        .with_status(413)
        .with_headers(headers)
        .fixed(streaming::request_too_large_message().into_bytes()))
}

// ---------------------------------------------------------------------------
// Response conversion
// ---------------------------------------------------------------------------

/// Convert a WAFER `OutputStream` into a Cloudflare Worker `Response`.
///
/// Two paths, chosen by the shared [`streaming::wants_streaming`] decision so
/// the adapter can never disagree with the pipeline:
///
/// 1. **Streaming** — the producer declared streaming intent up front via
///    leading `Meta` (the `resp.stream` marker or a streaming content-type,
///    e.g. a large file download or an SSE response). Status + headers are
///    applied from the leading meta, then the body chunks are piped straight
///    into the Worker `Response`'s native `ReadableStream`
///    (`ResponseBuilder::from_stream`) — the object never sits in the isolate
///    whole. This path must NOT route through `collect_http_response` (which
///    buffers).
/// 2. **Buffered** (default) — small SSR pages / JSON / buffered replays. The
///    body is drained under [`streaming::MAX_BUFFERED_RESPONSE_BYTES`]; an
///    over-limit body becomes **HTTP 413** (not a generic 500 / an isolate
///    OOM), and everything within the cap is mapped through the canonical
///    `http_codec::collect_http_response` terminal→status logic (reusing
///    [`streaming::terminal_to_stream`] so the `ErrorCode`→status table is
///    never re-implemented here).
pub async fn output_to_response(mut output: OutputStream) -> Result<Response> {
    let (leading_meta, next_event) = streaming::drain_leading_meta(&mut output).await;

    if streaming::wants_streaming(&leading_meta) {
        return match next_event {
            // Declared streaming AND a body chunk to forward — stream it.
            Some(StreamEvent::Chunk(first)) => {
                build_streaming_response(leading_meta, first, output)
            }
            // Declared streaming but the terminal arrived before any body
            // (empty SSE / empty download) — render the (short) buffered form.
            other => {
                finalise_buffered(
                    streaming::collect_capped_with_prelude(
                        output,
                        leading_meta,
                        other,
                        streaming::MAX_BUFFERED_RESPONSE_BYTES,
                    )
                    .await,
                )
                .await
            }
        };
    }

    finalise_buffered(
        streaming::collect_capped_with_prelude(
            output,
            leading_meta,
            next_event,
            streaming::MAX_BUFFERED_RESPONSE_BYTES,
        )
        .await,
    )
    .await
}

/// Apply classified response-meta parts to a Worker `Headers`. Status parts
/// are resolved separately (`http_codec::resolve_status`) and skipped here.
/// Only the canonical `resp.*` meta keys are honored (the `resp.stream`
/// streaming marker is not a header and is ignored by `classify_response_meta`).
fn apply_meta_to_headers(headers: &Headers, meta: &[MetaEntry]) -> Result<()> {
    for part in http_codec::response_meta_parts(meta) {
        match part {
            ResponseMetaPart::Status(_) => {}
            ResponseMetaPart::Header { name, value } => headers.set(name, value)?,
            ResponseMetaPart::SetCookie(v) => headers.append("Set-Cookie", v)?,
            ResponseMetaPart::ContentType(v) => headers.set("Content-Type", v)?,
        }
    }
    Ok(())
}

/// Build a streaming Worker `Response`: status + headers from the leading meta
/// (applied *before* the body finishes), body piped chunk-by-chunk into the
/// Worker's native `ReadableStream`. A body-read `Error` terminal surfaces as a
/// stream error (aborting the response body) rather than a silent truncation —
/// the HTTP status is already committed, so it cannot be downgraded to 413.
fn build_streaming_response(
    leading_meta: Vec<MetaEntry>,
    first_chunk: Vec<u8>,
    rest: OutputStream,
) -> Result<Response> {
    let status = http_codec::resolve_status(&leading_meta, 200);
    let headers = Headers::new();
    apply_meta_to_headers(&headers, &leading_meta)?;
    if !MetaGet::contains_key(&leading_meta, META_RESP_CONTENT_TYPE) {
        // Streaming bodies without an explicit content-type fall back to
        // octet-stream (not the JSON default the buffered path uses).
        headers.set("Content-Type", "application/octet-stream")?;
    }

    let body = streaming::download_body_stream(first_chunk, rest)
        .map(|chunk| chunk.map_err(|e| worker::Error::RustError(e.message)));

    ResponseBuilder::new()
        .with_status(status)
        .with_headers(headers)
        .from_stream(body)
}

/// Render a capped buffered collection to a Worker `Response`.
async fn finalise_buffered(collected: CappedCollect) -> Result<Response> {
    match collected {
        // The body would have exceeded the isolate buffering cap — return a
        // clean 413 instead of assembling it whole (which the CF runtime would
        // reject as an opaque error, i.e. the "generic 500" this replaces).
        CappedCollect::OverLimit => over_limit_response(),
        // Within the cap: reuse the canonical terminal→status mapping by
        // reconstructing a single-terminal stream and running it back through
        // `collect_http_response` (no duplicated ErrorCode→status table).
        CappedCollect::Terminal(result) => {
            let parts =
                http_codec::collect_http_response(streaming::terminal_to_stream(result)).await;
            parts_to_response(parts)
        }
    }
}

/// A 413 Payload Too Large response for an over-limit buffered body.
fn over_limit_response() -> Result<Response> {
    let headers = Headers::new();
    headers.set("Content-Type", "text/plain; charset=utf-8")?;
    Ok(ResponseBuilder::new()
        .with_status(413)
        .with_headers(headers)
        .fixed(b"payload too large".to_vec()))
}

/// Apply transport-neutral [`HttpResponseParts`] to the Worker types. Headers
/// are appended in application order (`headers` may legitimately repeat a name,
/// e.g. `Set-Cookie`).
fn parts_to_response(parts: HttpResponseParts) -> Result<Response> {
    let headers = Headers::new();
    for (name, value) in &parts.headers {
        headers.append(name, value)?;
    }
    Ok(Response::from_bytes(parts.body)?
        .with_status(parts.status)
        .with_headers(headers))
}

/// Request-conversion tests: the transport body cap and the path pass-through.
///
/// They build a real `worker::Request` (a `web_sys::Request`, which Node ≥18
/// provides) and run it through the real `worker_request_to_message`, so they
/// exercise the header pre-check, the post-read backstop and the meta the
/// pipeline then routes on. They need no `worker::Env`, so they run under the
/// `cloudflare-wasm-test` job like the rest of this crate's wasm tests.
#[cfg(all(test, target_arch = "wasm32"))]
mod request_tests {
    use impresspress_core::streaming::MAX_REQUEST_BODY_BYTES;
    use wafer_block::meta::META_REQ_RESOURCE;
    use wasm_bindgen::JsValue;
    use wasm_bindgen_test::wasm_bindgen_test;
    use worker::{Method, RequestInit};

    use super::{
        request_too_large_response, worker_request_to_message, Request, RequestConversion,
    };

    /// A POST whose body is `len` bytes of zeroes.
    fn post_with_body(url: &str, len: usize) -> Request {
        let body = js_sys::Uint8Array::new_with_length(len as u32);
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_body(Some(JsValue::from(body)));
        Request::new_with_init(url, &init).expect("build request")
    }

    fn ready(conversion: RequestConversion) -> wafer_run::Message {
        match conversion {
            RequestConversion::Ready(msg, _) => msg,
            RequestConversion::TooLarge => panic!("expected a converted request"),
        }
    }

    /// **Fails on the pre-fix tree**, where an over-cap body returned
    /// `Err("request body too large")` — a `worker::Error` the `run` entry
    /// point's catch-all turns into a 500 with a correlation id, telling the
    /// uploader nothing and an operator to go read the isolate log for what is
    /// not an internal error at all.
    #[wasm_bindgen_test]
    async fn a_body_over_the_cap_is_reported_as_too_large() {
        let req = post_with_body(
            "https://example.test/b/storage/api/buckets/photos/objects?key=big.bin",
            MAX_REQUEST_BODY_BYTES + 1,
        );
        assert!(matches!(
            worker_request_to_message(&req).await.expect("convert"),
            RequestConversion::TooLarge
        ));
    }

    /// And the refusal it turns into is a 413 naming the limit.
    #[wasm_bindgen_test]
    fn the_too_large_refusal_is_a_413() {
        let resp = request_too_large_response().expect("build response");
        assert_eq!(resp.status_code(), 413);
        assert_eq!(
            resp.headers().get("content-type").unwrap().as_deref(),
            Some("text/plain; charset=utf-8")
        );
    }

    /// A body exactly at the cap is admitted — the refusal is `>`, not `>=`,
    /// and the boundary is the one the files block's clamped quota reports.
    #[wasm_bindgen_test]
    async fn a_body_at_the_cap_is_admitted() {
        let req = post_with_body(
            "https://example.test/b/storage/api/buckets/photos/objects?key=big.bin",
            MAX_REQUEST_BODY_BYTES,
        );
        let msg = ready(worker_request_to_message(&req).await.expect("convert"));
        assert_eq!(
            msg.get_meta(META_REQ_RESOURCE),
            "/b/storage/api/buckets/photos/objects"
        );
    }

    /// **Fails on the pre-fix tree**: this adapter stripped `/api` itself, on
    /// top of the pipeline's own strip, so a double prefix lost both segments
    /// here and only one on every other transport.
    #[wasm_bindgen_test]
    async fn a_doubled_api_prefix_keeps_the_path_the_client_sent() {
        let req = post_with_body("https://example.test/api/api/x", 0);
        let msg = ready(worker_request_to_message(&req).await.expect("convert"));
        assert_eq!(
            msg.get_meta(META_REQ_RESOURCE),
            "/api/api/x",
            "the adapter passes the path through; the pipeline strips one /api"
        );
    }

    /// **Fails on the pre-fix tree**: the unbounded `starts_with("/api")`
    /// strip turned `/apiary` into `ary`, a path no route matches.
    #[wasm_bindgen_test]
    async fn a_path_that_merely_starts_with_api_is_untouched() {
        let req = post_with_body("https://example.test/apiary/hives", 0);
        let msg = ready(worker_request_to_message(&req).await.expect("convert"));
        assert_eq!(msg.get_meta(META_REQ_RESOURCE), "/apiary/hives");
    }
}
