//! HTTP ↔ Message conversion for the browser Service Worker adapter.
//!
//! Thin platform glue: the protocol mapping (method→action table, request
//! meta layout, response-meta classification, `ErrorCode`→status table) lives
//! in `wafer_block::http_codec` — the same implementation the native axum
//! listener and the Cloudflare adapter use. Only `web_sys` I/O lives here:
//! reading the request body/headers, the Service-Worker cookie re-injection,
//! and building `web_sys::Response` (buffered or `ReadableStream`-backed).

use futures::{SinkExt, StreamExt};
use js_sys::{ArrayBuffer, Uint8Array};
use wafer_block::{
    http_codec::{self, ResponseMetaPart},
    meta::META_RESP_CONTENT_TYPE,
    stream::StreamEvent,
    streams::{
        input::InputStream,
        output::{BufferedResponse, OutputStream, TerminalNotResponse},
    },
    Message, MetaEntry, MetaGet,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Headers, ResponseInit};

// ---------------------------------------------------------------------------
// Request conversion
// ---------------------------------------------------------------------------

/// Convert a browser `web_sys::Request` into a WAFER `(Message, InputStream)` pair.
///
/// The protocol mapping (kind, `http.*` / `req.*` meta, method→action, header
/// and query decoding) is delegated to `http_codec::build_http_message`; only
/// the `web_sys` body/header reads and the Service-Worker cookie re-injection
/// are browser-specific. The remote address is always `"127.0.0.1"` — in a
/// Service Worker the request comes from the same device.
pub async fn request_to_message(
    request: &web_sys::Request,
) -> Result<(Message, InputStream), JsValue> {
    let method = request.method();
    let url_str = request.url();

    // Parse the URL so we can separate path and query string.
    let url = web_sys::Url::new(&url_str)?;
    let path = url.pathname();
    // search() includes the leading '?' — strip it.
    let search = url.search();
    let raw_query = if let Some(stripped) = search.strip_prefix('?') {
        stripped.to_string()
    } else {
        search
    };

    // Read body bytes via ArrayBuffer.
    const MAX_BODY_SIZE: usize = 10 * 1024 * 1024; // 10MB
    let body: Vec<u8> = {
        let promise = request.array_buffer()?;
        let ab_val = JsFuture::from(promise).await?;
        let ab: ArrayBuffer = ab_val.dyn_into()?;
        let arr = Uint8Array::new(&ab);
        if arr.length() as usize > MAX_BODY_SIZE {
            return Err(JsValue::from_str("Request body too large"));
        }
        arr.to_vec()
    };

    // Collect headers into (name, value) pairs for the codec.
    let mut header_pairs: Vec<(String, String)> = Vec::new();
    let mut saw_host = false;
    let mut saw_fetch_site = false;
    let headers: Headers = request.headers();
    let iter =
        js_sys::try_iter(&headers)?.ok_or_else(|| JsValue::from_str("headers not iterable"))?;
    for item in iter {
        let item = item?;
        // Each entry is a JS Array [name, value].
        let arr: js_sys::Array = item.dyn_into()?;
        let key = arr.get(0).as_string().unwrap_or_default();
        let val = arr.get(1).as_string().unwrap_or_default();
        let lower = key.to_ascii_lowercase();
        saw_host |= lower == "host";
        saw_fetch_site |= lower == "sec-fetch-site";
        header_pairs.push((key, val));
    }

    // Synthesize the request metadata a service worker cannot be sent.
    //
    // `Host` and `Sec-Fetch-Site` are both absent from `FetchEvent.request`:
    // `Host` is a forbidden header name the `Headers` view never exposes, and
    // the Fetch spec appends `Sec-Fetch-*` during HTTP-network-or-cache fetch
    // — *after* service-worker interception. So does `Origin`, and `Referer`.
    // The result is that `impresspress_core::csrf::enforce_origin_policy` —
    // which reads `sec-fetch-site` first and falls back to `origin`/`referer`
    // against `host` — finds nothing at all and takes its fail-closed tail,
    // rejecting **every** cookie-authenticated mutation the browser bundle
    // makes. Not a sandbox problem: it is every `fetch`-driven admin form.
    //
    // The worker can prove what the missing headers would have said, and the
    // proof is the set of requests that can reach a service worker at all:
    //
    // * a subresource request (`fetch`, `XHR`, a form posted by script) is
    //   dispatched to the worker only when the *client that issued it* is
    //   controlled by this worker — which requires that client to be
    //   same-origin with the worker's scope. A cross-site page's request to
    //   this origin is never handed to this worker; it goes straight to the
    //   network (or to that page's own worker). So a same-origin request URL
    //   on a non-navigation really is a same-origin request.
    // * a navigation into the scope is dispatched to the worker whoever
    //   started it, which is exactly the CSRF case a cross-site `<form>`
    //   POST uses. `Request::referrer` — an attribute, not a header, and so
    //   readable here — is the only thing that can separate our own page
    //   from somebody else's, and a referrer that is ABSENT separates
    //   nothing: suppression is attacker-controllable, so it is refused
    //   rather than reported as `Sec-Fetch-Site: none` (which the policy
    //   accepts). See [`fetch_site_for`] for the full argument.
    //
    // Anything that does not positively match one of those is `cross-site`,
    // so a value this cannot prove stays a refusal. A header that really is
    // present is never overwritten — a real `Sec-Fetch-Site` from a client
    // that sends one outranks anything inferred here.
    //
    // Outside a worker global (a unit test, or a main-thread caller) the
    // worker's own origin is unknowable, and nothing is synthesized at all:
    // the policy then fails closed exactly as it did before this ran.
    if let Some(location) = worker_location() {
        if !saw_host {
            // Defence in depth, and **not the live path**. `host` only matters
            // to `csrf::enforce_origin_policy`'s `origin`/`referer` fallback,
            // and that fallback is unreachable whenever this block runs at
            // all: the `sec-fetch-site` synthesized just below is
            // unconditional inside this `if`, and the policy reads
            // `sec-fetch-site` *first* and returns on it. So a future reader
            // should not assume the fallback is exercised by any test here —
            // it is what would carry the policy if the `Sec-Fetch-Site` arm
            // were ever removed, or if a client sent a `host` header of its
            // own that this branch then declines to overwrite.
            header_pairs.push(("host".to_string(), location.host));
        }
        if !saw_fetch_site {
            header_pairs.push((
                "sec-fetch-site".to_string(),
                fetch_site_for(
                    request_mode(request),
                    &request.referrer(),
                    &url.origin(),
                    &location.origin,
                )
                .to_string(),
            ));
        }
    }

    // Re-inject the `Cookie` header from the SW's CookieStore.
    // `FetchEvent.request.headers` filters `Cookie` out per the SW spec, so
    // the header iteration above never sees it even though the browser sends
    // cookies on same-origin requests. CookieStore is the only way to read
    // them back inside the SW.
    let cookie_val = crate::bridge::read_cookie_header().await;
    if let Some(s) = cookie_val.as_string() {
        if !s.is_empty() {
            header_pairs.push(("cookie".to_string(), s));
        }
    }

    // `build_http_message` builds `kind`, `http.*` and normalized `req.*` meta
    // from the method+path. The browser serves paths as-received (no `/api`
    // prefix to strip, unlike the Cloudflare adapter), so no post-fixup.
    let msg = http_codec::build_http_message(
        &method,
        &path,
        &raw_query,
        "127.0.0.1",
        header_pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())),
    );

    Ok((msg, InputStream::from_bytes(body)))
}

/// The worker's own `origin` and `host`, as its global `location` reports them.
struct WorkerLocation {
    /// `scheme://host[:port]` — what a request URL's origin is compared to.
    origin: String,
    /// `host[:port]` — the authority `csrf::enforce_origin_policy` compares an
    /// `Origin`/`Referer` header against.
    host: String,
}

/// Read the worker global's `location`, or `None` when there is no worker
/// global (a `wasm-pack test --node` harness, or a main-thread caller).
///
/// `None` is deliberately not a fallback to the request's own origin: that
/// would let the request under inspection supply the authority it is checked
/// against, which is not a check at all.
fn worker_location() -> Option<WorkerLocation> {
    let scope = js_sys::global()
        .dyn_into::<web_sys::WorkerGlobalScope>()
        .ok()?;
    let location = scope.location();
    Some(WorkerLocation {
        origin: location.origin(),
        host: location.host(),
    })
}

/// `Request::mode` as the Fetch spec spells it, or `""` for a value this
/// build of `web-sys` does not name.
fn request_mode(request: &web_sys::Request) -> &'static str {
    match request.mode() {
        web_sys::RequestMode::SameOrigin => "same-origin",
        web_sys::RequestMode::Cors => "cors",
        web_sys::RequestMode::NoCors => "no-cors",
        web_sys::RequestMode::Navigate => "navigate",
        _ => "",
    }
}

/// What `Sec-Fetch-Site` a service worker can *prove* for a request it was
/// handed. Pure, so every arm is testable without a browser.
///
/// * `mode` — `Request::mode`, as [`request_mode`] spells it.
/// * `referrer` — `Request::referrer`; `""` when the request has none.
/// * `request_origin` — the origin of the request's own URL.
/// * `self_origin` — the worker's own origin ([`worker_location`]).
///
/// The security argument is at the call site. The rule, restated: a
/// non-navigation reaching this worker came from a client this worker
/// controls, so a same-origin URL makes it same-origin; a navigation is
/// same-origin only when its referrer says so; everything else — **including
/// a navigation with no referrer at all** — is `cross-site`, which is what an
/// unprovable case must resolve to.
///
/// # Why a referrer-less navigation is not `none`
///
/// `Sec-Fetch-Site: none` means "no initiator" — a typed URL, a bookmark — and
/// `csrf::enforce_origin_policy` accepts it. A service worker cannot tell that
/// apart from a navigation whose referrer was **suppressed**, and suppression
/// is attacker-controllable: `<form referrerpolicy="no-referrer">`, a
/// `<meta name="referrer">` on the attacking page, or a redirect that drops
/// it. A same-site sibling's top-level `<form>` POST is exactly the shape
/// `SameSite=Lax` still attaches the `auth_token` cookie to, so answering
/// `none` here would hand that request the CSRF check's approval.
///
/// Nothing legitimate is lost by refusing it. This value is only ever
/// consulted for a cookie-authenticated **unsafe** method (the policy returns
/// early otherwise), and a same-origin form POST carries a referrer under the
/// `Referrer-Policy: strict-origin-when-cross-origin` the security-headers
/// block sets — a bookmark or typed URL is a `GET`, which never reaches the
/// check at all.
fn fetch_site_for(
    mode: &str,
    referrer: &str,
    request_origin: &str,
    self_origin: &str,
) -> &'static str {
    // An empty `self_origin` would make `"" == ""` true for a request whose
    // origin is also unreadable, turning two unknowns into a same-origin
    // verdict. Refuse before any comparison can do that.
    if self_origin.is_empty() {
        return "cross-site";
    }
    match mode {
        // The client asked for a same-origin-only fetch and got one; the
        // browser would have failed it otherwise.
        "same-origin" => "same-origin",
        // The default for `fetch()` is `cors`, so this is the ordinary
        // same-origin API call. `no-cors` covers `<img>`, `<script>` and
        // `sendBeacon` from the same controlled client.
        "cors" | "no-cors" => {
            if request_origin == self_origin {
                "same-origin"
            } else {
                "cross-site"
            }
        }
        // A navigation is judged on its referrer, and ONLY a referrer that
        // is provably ours passes. An absent one is refused rather than
        // reported as `none` — see the note above; this function never
        // returns `none`.
        "navigate" => {
            if !referrer.is_empty() && origin_of(referrer) == self_origin {
                "same-origin"
            } else {
                "cross-site"
            }
        }
        _ => "cross-site",
    }
}

/// The `scheme://authority` prefix of an absolute URL, or `""` when the string
/// is not one.
///
/// Deliberately not `web_sys::Url` (a JS global this crate's pure tests must
/// not need) and deliberately not lenient: `about:client` — the referrer
/// placeholder a `Request` can carry before the referrer is resolved — has no
/// authority, returns `""`, and is therefore judged cross-site.
fn origin_of(url: &str) -> &str {
    let Some(after_scheme) = url.find("://").map(|i| i + 3) else {
        return "";
    };
    match url[after_scheme..].find('/') {
        Some(slash) => &url[..after_scheme + slash],
        None => url,
    }
}

// ---------------------------------------------------------------------------
// Response conversion
// ---------------------------------------------------------------------------

/// Apply classified response-meta parts to a `web_sys::Headers`. Status parts
/// are resolved separately (see `http_codec::resolve_status`) and skipped here.
/// Only the canonical `resp.*` meta keys are honored — legacy aliases
/// (`http.status`, `http.resp.header.*`, `http.resp.set-cookie.*`, a literal
/// `Content-Type` meta key) are ignored by `http_codec`.
fn apply_response_meta(headers: &Headers, meta: &[MetaEntry]) -> Result<(), JsValue> {
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

/// True when `meta` carries an explicit `resp.content_type` entry.
fn has_content_type(meta: &[MetaEntry]) -> bool {
    MetaGet::contains_key(meta, META_RESP_CONTENT_TYPE)
}

/// Build a `web_sys::Response` from raw bytes, a status code, and a
/// `web_sys::Headers` object.
fn make_response(
    body: Vec<u8>,
    status: u16,
    headers: Headers,
) -> Result<web_sys::Response, JsValue> {
    let init = ResponseInit::new();
    init.set_status(status);
    init.set_headers(&headers);

    if body.is_empty() {
        web_sys::Response::new_with_opt_str_and_init(None, &init)
    } else {
        // Copy into a Uint8Array then pass as BufferSource.
        let arr = Uint8Array::new_with_length(body.len() as u32);
        arr.copy_from(&body);
        let ab: ArrayBuffer = arr.buffer();
        web_sys::Response::new_with_opt_buffer_source_and_init(Some(&ab.into()), &init)
    }
}

/// Pull `Meta` events off the front of an `OutputStream`, stopping at the
/// first non-Meta event. Returns the accumulated meta and the next event
/// (if any). Used by `output_to_response` to peek the response's headers
/// before deciding whether to stream the body or buffer it.
async fn drain_leading_meta(output: &mut OutputStream) -> (Vec<MetaEntry>, Option<StreamEvent>) {
    let mut meta = Vec::new();
    while let Some(ev) = output.next().await {
        match ev {
            StreamEvent::Meta(entry) => meta.push(entry),
            other => return (meta, Some(other)),
        }
    }
    (meta, None)
}

/// Build a JS `ReadableStream` that yields `first_chunk` and then every
/// subsequent `Chunk` event from `remaining`. Mid-body `Meta` is dropped
/// (too late to apply to HTTP headers); any terminal closes the stream.
fn make_streaming_body(
    first_chunk: Vec<u8>,
    mut remaining: OutputStream,
) -> wasm_streams::ReadableStream {
    use futures::channel::mpsc;
    let (mut tx, rx) = mpsc::channel::<Result<JsValue, JsValue>>(8);

    // Channel cap is 8 and we have one item to send — try_send fits. If it
    // fails (caller dropped the stream before consuming) we just discard.
    let _ = tx.try_send(Ok(JsValue::from(Uint8Array::from(first_chunk.as_slice()))));

    wasm_bindgen_futures::spawn_local(async move {
        while let Some(ev) = remaining.next().await {
            match ev {
                StreamEvent::Chunk(bytes) => {
                    let val: Result<JsValue, JsValue> =
                        Ok(JsValue::from(Uint8Array::from(bytes.as_slice())));
                    if tx.send(val).await.is_err() {
                        // Browser dropped the response stream — stop pumping.
                        return;
                    }
                }
                // Mid-body Meta is too late to apply to HTTP headers; drop it.
                StreamEvent::Meta(_) => {}
                // Any terminal closes the body. Error after partial body has
                // already streamed bytes can't change the HTTP status — log
                // and close cleanly so the browser sees a normal end-of-body.
                StreamEvent::Error(err) => {
                    web_sys::console::warn_1(
                        &format!(
                            "impresspress-browser: streaming response aborted: {}",
                            err.message
                        )
                        .into(),
                    );
                    return;
                }
                StreamEvent::Complete { .. }
                | StreamEvent::Drop
                | StreamEvent::Continue(_)
                | StreamEvent::Halt { .. } => {
                    // Mid-body Halt cannot change the HTTP status (headers
                    // already flushed); treat it as another terminal that
                    // closes the body cleanly. The browser sees a normal
                    // end-of-stream.
                    return;
                }
            }
        }
    });

    wasm_streams::ReadableStream::from_stream(rx)
}

/// Convert a WAFER `OutputStream` into a browser `web_sys::Response`.
///
/// Two paths:
/// 1. **Streaming** — for blocks that emit leading `Meta` events declaring
///    `Content-Type: text/event-stream` (or `application/octet-stream`)
///    BEFORE the first `Chunk`. We classify the leading meta with
///    `http_codec::response_meta_parts` and apply status + headers to a
///    `Response` backed by a `ReadableStream`, piping subsequent chunks
///    straight to the browser — so a multi-minute SSE response isn't held
///    back behind a buffer that flushes at the very end (which Chrome's idle
///    keep-alive treats as a hung fetch and drops with `net::ERR_FAILED`).
///    The meta is applied *before the body finishes*, so this path must NOT
///    route through `collect_http_response` (which buffers).
/// 2. **Buffered** (default) — for blocks that emit `Chunk(bytes),
///    Complete{meta}` via `respond_with_meta`. Status, headers, and body all
///    live in the terminal, so we read the whole stream before building the
///    `Response`. The terminal-event mapping mirrors
///    `http_codec::collect_http_response` (whose drift decisions —
///    `Continue` → empty `200`, default `Content-Type: application/json`,
///    `Ok`/`Halt` identical — are pinned by the codec's tests).
pub async fn output_to_response(mut output: OutputStream) -> Result<web_sys::Response, JsValue> {
    // Peek leading Meta events without consuming Chunks. The streaming path is
    // signalled by an early Content-Type meta; buffered blocks send no Meta
    // before their first Chunk, so this returns an empty vec for them and the
    // buffered branch below handles the terminal.
    let (leading_meta, next_event) = drain_leading_meta(&mut output).await;

    let leading_ct = http_codec::response_meta_parts(&leading_meta).find_map(|part| match part {
        ResponseMetaPart::ContentType(ct) => Some(ct.to_string()),
        _ => None,
    });

    if let (Some(ct), Some(StreamEvent::Chunk(first))) = (leading_ct, &next_event) {
        if is_streaming_content_type(&ct) {
            return build_streaming_response(leading_meta, first.clone(), output);
        }
    }

    // Buffered path — drain the remainder, prepending the leading meta + the
    // event we peeked, then map the terminal to a response exactly as
    // `http_codec::collect_http_response` would for a non-peeked stream.
    let terminal = collect_buffered_with_prelude(output, leading_meta, next_event).await;
    finalise_buffered(terminal)
}

/// True for content-types that should stream body chunks to the browser as
/// they're produced rather than buffer the entire response. Today: SSE and
/// generic byte streams (which feature blocks use for downloads / archives).
fn is_streaming_content_type(ct: &str) -> bool {
    let lower = ct.to_ascii_lowercase();
    lower.starts_with("text/event-stream") || lower.starts_with("application/octet-stream")
}

/// Drain the remaining stream into a buffered terminal, prepending the
/// already-peeked leading meta + next event. Mirrors the contract of
/// `OutputStream::collect_buffered` (a `Halt` terminal replaces any streamed
/// prelude), reproduced here because `impresspress-browser` cannot depend on
/// `impresspress-core`'s `pipeline::collect_buffered_with_prelude`.
async fn collect_buffered_with_prelude(
    rest: OutputStream,
    leading_meta: Vec<MetaEntry>,
    next_event: Option<StreamEvent>,
) -> Result<BufferedResponse, TerminalNotResponse> {
    match next_event {
        Some(StreamEvent::Chunk(first)) => match rest.collect_buffered().await {
            Ok(buf) => {
                let mut body = first;
                body.extend(buf.body);
                let mut meta = leading_meta;
                meta.extend(buf.meta);
                Ok(BufferedResponse { body, meta })
            }
            Err(terminal) => Err(terminal),
        },
        Some(StreamEvent::Meta(_)) => unreachable!("drain_leading_meta consumes Meta events"),
        Some(StreamEvent::Complete { meta }) => {
            let mut all_meta = leading_meta;
            all_meta.extend(meta);
            Ok(BufferedResponse {
                body: Vec::new(),
                meta: all_meta,
            })
        }
        Some(StreamEvent::Halt { body, meta }) => {
            // Halt carries a complete response; per the `collect_buffered`
            // contract any prior streamed events — the prelude included — are
            // replaced by its payload.
            Err(TerminalNotResponse::Halt(BufferedResponse { body, meta }))
        }
        Some(StreamEvent::Error(err)) => Err(TerminalNotResponse::Error(*err)),
        Some(StreamEvent::Drop) => Err(TerminalNotResponse::Drop),
        Some(StreamEvent::Continue(msg)) => Err(TerminalNotResponse::Continue(msg)),
        None => Err(TerminalNotResponse::Malformed),
    }
}

/// Map a buffered terminal to a `web_sys::Response`, mirroring
/// `http_codec::collect_http_response`'s terminal handling (the codec maps to
/// transport-neutral parts; we apply them to `web_sys` types here).
fn finalise_buffered(
    result: Result<BufferedResponse, TerminalNotResponse>,
) -> Result<web_sys::Response, JsValue> {
    match result {
        // Ok and Halt are the single buffered code path (codec finding 55).
        Ok(buf) | Err(TerminalNotResponse::Halt(buf)) => {
            let status = http_codec::resolve_status(&buf.meta, 200);
            let headers = Headers::new()?;
            apply_response_meta(&headers, &buf.meta)?;
            if !has_content_type(&buf.meta) {
                headers.set("Content-Type", http_codec::DEFAULT_RESPONSE_CONTENT_TYPE)?;
            }
            make_response(buf.body, status, headers)
        }

        Err(TerminalNotResponse::Error(err)) => {
            let status = http_codec::resolve_error_status(&err);
            let headers = Headers::new()?;
            apply_response_meta(&headers, &err.meta)?;
            // Error bodies ARE JSON; a `resp.content_type` on the error meta is
            // superseded (exactly one Content-Type, matching the codec).
            headers.set("Content-Type", http_codec::DEFAULT_RESPONSE_CONTENT_TYPE)?;
            // Surface the precise application code (set via
            // `WaferError::with_detail_code`, carried as `error.code` meta) as a
            // machine-readable `code` field; the coarse wafer code stays in
            // `error`. Omitted when no detail code was attached.
            let mut body = serde_json::json!({
                "error": err.code,
                "message": err.message,
            });
            if let Some(detail) = err.detail_code() {
                body["code"] = serde_json::Value::String(detail.to_string());
            }
            let body = body.to_string().into_bytes();
            make_response(body, status, headers)
        }

        Err(TerminalNotResponse::Drop) => make_response(Vec::new(), 204, Headers::new()?),

        Err(TerminalNotResponse::Continue(msg)) => {
            // Codec drift: `Continue` at the HTTP boundary → empty-body 200
            // with the message's response meta applied (nowhere to forward).
            let headers = Headers::new()?;
            apply_response_meta(&headers, &msg.meta)?;
            headers.set("Content-Type", http_codec::DEFAULT_RESPONSE_CONTENT_TYPE)?;
            make_response(Vec::new(), 200, headers)
        }

        Err(TerminalNotResponse::Malformed) => {
            web_sys::console::error_1(
                &"impresspress-browser: stream ended without terminal event".into(),
            );
            let headers = Headers::new()?;
            make_response(b"internal server error".to_vec(), 500, headers)
        }
    }
}

/// Build a streaming `web_sys::Response` from the leading meta (carrying
/// status + headers) and an `OutputStream` whose remaining events are piped
/// into the body. Meta is classified and applied *before* the body finishes —
/// the whole point of the streaming path.
fn build_streaming_response(
    leading_meta: Vec<MetaEntry>,
    first_chunk: Vec<u8>,
    remaining: OutputStream,
) -> Result<web_sys::Response, JsValue> {
    let status = http_codec::resolve_status(&leading_meta, 200);
    let headers = Headers::new()?;
    apply_response_meta(&headers, &leading_meta)?;

    if !has_content_type(&leading_meta) {
        // Streaming bodies without an explicit Content-Type fall back to
        // octet-stream rather than the JSON default the buffered path uses.
        headers.set("Content-Type", "application/octet-stream")?;
    }

    let stream = make_streaming_body(first_chunk, remaining);
    let raw_js = stream.into_raw();
    let init = ResponseInit::new();
    init.set_status(status);
    init.set_headers(&headers);
    web_sys::Response::new_with_opt_readable_stream_and_init(Some(&raw_js), &init)
}

// ---------------------------------------------------------------------------
// The `Sec-Fetch-Site` mapping
// ---------------------------------------------------------------------------

/// `impresspress-browser` only compiles for `wasm32`, so these run under
/// `wasm-pack test --node` — the same harness `storage.rs` and `bridge.rs`
/// use. [`fetch_site_for`] and [`origin_of`] are pure, so nothing here needs a
/// worker, a `Request` or a network.
#[cfg(all(test, target_arch = "wasm32"))]
mod fetch_site_tests {
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::{fetch_site_for, origin_of};

    const SELF_ORIGIN: &str = "https://dev.impresspress.org";

    #[wasm_bindgen_test]
    fn a_same_origin_mode_request_is_same_origin() {
        assert_eq!(
            fetch_site_for("same-origin", "", SELF_ORIGIN, SELF_ORIGIN),
            "same-origin"
        );
    }

    /// The ordinary case: `fetch('/b/dev/api/files/write', {method:'POST'})`
    /// from a page this worker controls. `fetch`'s default mode is `cors`.
    #[wasm_bindgen_test]
    fn a_cors_request_for_our_own_origin_is_same_origin() {
        assert_eq!(
            fetch_site_for("cors", "", SELF_ORIGIN, SELF_ORIGIN),
            "same-origin"
        );
        assert_eq!(
            fetch_site_for("no-cors", "", SELF_ORIGIN, SELF_ORIGIN),
            "same-origin"
        );
    }

    #[wasm_bindgen_test]
    fn a_cors_request_for_another_origin_is_cross_site() {
        assert_eq!(
            fetch_site_for("cors", "", "https://evil.example", SELF_ORIGIN),
            "cross-site"
        );
        assert_eq!(
            fetch_site_for("no-cors", "", "https://evil.example", SELF_ORIGIN),
            "cross-site"
        );
    }

    /// Clicking a link on our own page.
    #[wasm_bindgen_test]
    fn a_navigation_referred_by_our_own_page_is_same_origin() {
        assert_eq!(
            fetch_site_for(
                "navigate",
                "https://dev.impresspress.org/b/admin/",
                SELF_ORIGIN,
                SELF_ORIGIN
            ),
            "same-origin"
        );
    }

    /// A referrer-less navigation is REFUSED, not reported as `none`.
    ///
    /// `none` would mean "no initiator" (a typed URL, a bookmark) and
    /// `csrf::enforce_origin_policy` accepts it — but a worker cannot tell
    /// that from a navigation whose referrer an attacker suppressed with
    /// `referrerpolicy="no-referrer"`, a `<meta name=referrer>`, or a
    /// redirect. A same-site sibling's top-level `<form>` POST carries the
    /// `SameSite=Lax` cookie, so `none` here would be a CSRF bypass. Nothing
    /// legitimate is lost: this value is only consulted for a
    /// cookie-authenticated unsafe method, and a real same-origin form POST
    /// has a referrer.
    #[wasm_bindgen_test]
    fn a_navigation_with_no_referrer_is_cross_site() {
        assert_eq!(
            fetch_site_for("navigate", "", SELF_ORIGIN, SELF_ORIGIN),
            "cross-site"
        );
    }

    /// Stated on its own because it is the property the arm above exists for:
    /// nothing this function can answer is ever `none`.
    #[wasm_bindgen_test]
    fn no_input_produces_none() {
        for mode in [
            "same-origin",
            "cors",
            "no-cors",
            "navigate",
            "websocket",
            "",
        ] {
            for referrer in ["", "about:client", SELF_ORIGIN, "https://evil.example/a"] {
                for request_origin in ["", SELF_ORIGIN, "https://evil.example"] {
                    assert_ne!(
                        fetch_site_for(mode, referrer, request_origin, SELF_ORIGIN),
                        "none",
                        "mode {mode}, referrer {referrer}, origin {request_origin}",
                    );
                }
            }
        }
    }

    /// The CSRF case this whole mapping exists to keep refused: a cross-site
    /// page posting a `<form>` at us navigates into the worker's scope, so the
    /// worker *does* see it.
    #[wasm_bindgen_test]
    fn a_navigation_referred_by_someone_else_is_cross_site() {
        assert_eq!(
            fetch_site_for(
                "navigate",
                "https://evil.example/attack.html",
                SELF_ORIGIN,
                SELF_ORIGIN
            ),
            "cross-site"
        );
    }

    /// `about:client` has no authority, so it cannot be shown to be ours.
    #[wasm_bindgen_test]
    fn a_navigation_with_an_unparseable_referrer_is_cross_site() {
        assert_eq!(
            fetch_site_for("navigate", "about:client", SELF_ORIGIN, SELF_ORIGIN),
            "cross-site"
        );
    }

    /// A mode this build of `web-sys` does not name resolves to `""`, and an
    /// unprovable case is a refusal.
    #[wasm_bindgen_test]
    fn an_unknown_mode_is_cross_site() {
        assert_eq!(
            fetch_site_for("", "", SELF_ORIGIN, SELF_ORIGIN),
            "cross-site"
        );
        assert_eq!(
            fetch_site_for("websocket", "", SELF_ORIGIN, SELF_ORIGIN),
            "cross-site"
        );
    }

    /// Two unknowns must not compare equal into a same-origin verdict.
    #[wasm_bindgen_test]
    fn an_unknown_self_origin_is_cross_site_whatever_the_request_says() {
        for mode in ["same-origin", "cors", "no-cors", "navigate"] {
            assert_eq!(
                fetch_site_for(mode, "", "", ""),
                "cross-site",
                "mode {mode} with no worker origin",
            );
        }
    }

    #[wasm_bindgen_test]
    fn origin_of_takes_the_scheme_and_authority_only() {
        assert_eq!(origin_of("https://a.example/b/c?d=e"), "https://a.example");
        assert_eq!(origin_of("http://127.0.0.1:8082/"), "http://127.0.0.1:8082");
        assert_eq!(origin_of("http://127.0.0.1:8082"), "http://127.0.0.1:8082");
        assert_eq!(origin_of("about:client"), "");
        assert_eq!(origin_of(""), "");
    }
}
