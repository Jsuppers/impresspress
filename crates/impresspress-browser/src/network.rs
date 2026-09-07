use std::collections::HashMap;

use impresspress_core::streaming::MAX_NETWORK_RESPONSE_BYTES;
use serde::Deserialize;
use wafer_core::interfaces::network::service::{NetworkError, NetworkService, Request, Response};

use crate::bridge;

pub struct BrowserNetworkService;

// SAFETY: `BrowserNetworkService` is a unit struct with no shared state.
// wasm32-unknown-unknown has no threads, so the `Send`/`Sync` bounds
// required by `Arc<dyn NetworkService>` are satisfied trivially — no
// cross-thread aliasing or data races are possible.
unsafe impl Send for BrowserNetworkService {}
unsafe impl Sync for BrowserNetworkService {}

/// JS object shape returned by bridge.httpFetch (NOT a JSON string):
/// `{ status: number, headers: [[name, value], ...], body: Uint8Array }`.
/// Decoded directly via `serde_wasm_bindgen::from_value` — `serde_wasm_bindgen`
/// deserializes a JS `Uint8Array` straight into `Vec<u8>`.
///
/// `headers` is an **array of pairs**, not an object, because a response may
/// carry the same header name more than once and a JS object cannot hold it
/// twice. `Set-Cookie` is the case that bites: the Fetch spec's header
/// iteration combines repeated names into one comma-joined value *except* for
/// `Set-Cookie`, which it yields once per cookie — so the object this replaced
/// kept only the last one, and a response that set a session cookie and a CSRF
/// cookie silently lost one of them. (Comma-joining `Set-Cookie` is not a fix
/// either: the header's own grammar uses commas, in `Expires` dates among
/// other places, so a joined value cannot be split back apart.)
#[derive(Deserialize)]
struct FetchResponse {
    status: u16,
    #[serde(default)]
    headers: Vec<(String, String)>,
    #[serde(default)]
    body: Vec<u8>,
}

#[async_trait::async_trait(?Send)]
impl NetworkService for BrowserNetworkService {
    /// Dispatch one request through the page's `fetch`.
    ///
    /// **SSRF precheck.** Before anything is dispatched, the request URL goes
    /// through the shared [`is_ssrf_blocked_url`](impresspress_core::ssrf::is_ssrf_blocked_url)
    /// gate — the same one the Cloudflare adapter applies — so a request whose
    /// URL *literally* names an internal target (a
    /// private/loopback/link-local/CGNAT IP literal, `localhost`, an
    /// IPv6-embedded-v4 form, or a well-known cloud-metadata hostname) is
    /// refused here rather than handed to `fetch`.
    ///
    /// This is not theoretical for a browser-hosted runtime. Blocks reach this
    /// service with URLs that come from configuration and from request
    /// payloads (webhook targets, model endpoints, image fetches), and the
    /// worker runs inside the user's own browser: a fetch to
    /// `http://192.168.1.1/` or `http://localhost:8080/` from here is a fetch
    /// from *inside the user's network*, against hosts nothing on the public
    /// internet can reach. That is a wider blast radius than the same bug on a
    /// server, not a narrower one.
    ///
    /// The gate sees the URL the caller asked for and nothing else, so a
    /// followed `3xx` would reach a second URL it never inspected. That half is
    /// closed on the JS side: `bridge.js`'s `httpFetch` issues every request
    /// with `redirect: 'error'` instead of the Fetch API's default `'follow'`,
    /// so `https://evil.example/x` answering `302 Location:
    /// http://169.254.169.254/…` fails the request rather than fetching the
    /// metadata service. `'manual'` would not do: a cross-origin redirect
    /// response is opaque, with no readable `Location` to revalidate. The
    /// native path revalidates per hop instead (reqwest's
    /// `ssrf_revalidating_redirect_policy`) because it has a hook for it; here
    /// a legitimate redirect surfaces as a request error, which is the right
    /// trade against a silent fetch of an internal address.
    ///
    /// Response bytes are capped at
    /// [`MAX_NETWORK_RESPONSE_BYTES`](impresspress_core::streaming::MAX_NETWORK_RESPONSE_BYTES),
    /// the cap shared with the Cloudflare adapter, and enforced on the JS side
    /// because that is where the bytes are read — an advertised
    /// `Content-Length` over the cap is refused before the body is touched, and
    /// the running total is checked per chunk for a chunked response that
    /// advertises nothing. A Service Worker shares one linear memory with
    /// everything else the page's runtime is doing, so the ceiling matters more
    /// here than on a server, not less.
    ///
    /// Honest boundary: this is a URL/host-literal precheck. Like the Worker
    /// path, it does NOT defend against DNS rebinding — a public-looking
    /// hostname that resolves to a private address at connect time still
    /// reaches `fetch`, because the Fetch API exposes no resolve-before-connect
    /// hook. The native backend closes that with an SSRF-filtering resolver;
    /// here it is the browser's own network partitioning that has to. With the
    /// initial URL gated and redirects refused, that is the sole residual.
    async fn do_request(&self, req: &Request) -> Result<Response, NetworkError> {
        if impresspress_core::ssrf::is_ssrf_blocked_url(&req.url) {
            return Err(NetworkError::RequestError(format!(
                "SSRF: refusing request to internal/blocked address: {}",
                req.url
            )));
        }

        let headers_json = serde_json::to_string(&req.headers)
            .map_err(|e| NetworkError::Other(format!("failed to serialize headers: {e}")))?;

        let body_bytes: &[u8] = req.body.as_deref().unwrap_or(&[]);

        let js_val = bridge::http_fetch(
            &req.method,
            &req.url,
            &headers_json,
            body_bytes,
            MAX_NETWORK_RESPONSE_BYTES as f64,
        )
        .await
        .map_err(|e| NetworkError::RequestError(bridge::describe(&e)))?;

        // The bridge resolves a JS object `{ status, headers, body:
        // Uint8Array }` — decode it directly. `serde_wasm_bindgen`
        // deserializes the JS object into `FetchResponse` and the
        // `Uint8Array` body straight into `Vec<u8>` in one step, with no
        // JSON round-trip (previously this called `JSON::stringify` on the
        // resolved value and fed the result to `serde_json::from_str`,
        // which double-encoded every response into a JSON string literal
        // and failed with "invalid type: string, expected struct").
        let fetch_resp: FetchResponse = serde_wasm_bindgen::from_value(js_val).map_err(|e| {
            NetworkError::RequestError(format!("failed to decode fetch response: {e}"))
        })?;

        Ok(Response {
            status_code: fetch_resp.status,
            headers: group_headers(fetch_resp.headers),
            body: fetch_resp.body,
        })
    }
}

/// Group the wire's `(name, value)` pairs into the `name → [values]` map
/// `Response` carries, appending rather than replacing so every value of a
/// repeated header survives. Mirrors `collect_headers` in
/// `impresspress-cloudflare/src/network_service.rs`.
fn group_headers(pairs: Vec<(String, String)>) -> HashMap<String, Vec<String>> {
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for (name, value) in pairs {
        grouped.entry(name).or_default().push(value);
    }
    grouped
}

pub fn make_network_service(
) -> std::sync::Arc<dyn wafer_core::interfaces::network::service::NetworkService> {
    std::sync::Arc::new(BrowserNetworkService)
}

// `bridge::http_fetch` is a `#[wasm_bindgen(module = "/js/bridge.js")]` extern
// import backed by the page's real `fetch`, so the tests below never let a
// request get that far: they either assert on the decode step in isolation or
// on the SSRF gate, which returns before the bridge is touched.
#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    use js_sys::{Array, Object, Reflect, Uint8Array};
    use wafer_core::interfaces::network::service::{NetworkError, NetworkService, Request};
    use wasm_bindgen::JsValue;
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::{BrowserNetworkService, FetchResponse};

    /// Build the JS object shape `bridge.js`'s `httpFetch` resolves:
    /// `{ status: number, headers: [[name, value], ...], body: Uint8Array }`.
    fn make_fetch_response_object(status: u16, headers: &[(&str, &str)], body: &[u8]) -> JsValue {
        let obj = Object::new();
        Reflect::set(
            &obj,
            &JsValue::from_str("status"),
            &JsValue::from_f64(status as f64),
        )
        .unwrap();

        let pairs = Array::new();
        for (name, value) in headers {
            let pair = Array::new();
            pair.push(&JsValue::from_str(name));
            pair.push(&JsValue::from_str(value));
            pairs.push(&pair);
        }
        Reflect::set(&obj, &JsValue::from_str("headers"), &pairs).unwrap();

        Reflect::set(
            &obj,
            &JsValue::from_str("body"),
            &Uint8Array::from(body).into(),
        )
        .unwrap();

        obj.into()
    }

    fn decode(value: JsValue) -> FetchResponse {
        serde_wasm_bindgen::from_value(value).expect("decode fetch response")
    }

    #[wasm_bindgen_test]
    fn decodes_js_object_response_in_one_step() {
        let body = b"hello world";
        let decoded = decode(make_fetch_response_object(
            200,
            &[("content-type", "application/json")],
            body,
        ));

        assert_eq!(decoded.status, 200);
        assert_eq!(decoded.body, body.to_vec());
        assert_eq!(
            decoded.headers,
            vec![("content-type".to_string(), "application/json".to_string())]
        );
    }

    #[wasm_bindgen_test]
    fn decodes_empty_body_and_headers_via_serde_default() {
        let obj = Object::new();
        Reflect::set(
            &obj,
            &JsValue::from_str("status"),
            &JsValue::from_f64(204.0),
        )
        .unwrap();
        // No `headers` or `body` keys at all — `#[serde(default)]` must
        // fill both rather than erroring as "missing field".
        let decoded = decode(obj.into());

        assert_eq!(decoded.status, 204);
        assert!(decoded.body.is_empty());
        assert!(decoded.headers.is_empty());
    }

    /// Regression guard for the double-encode bug: `bridge.js` used to
    /// `JSON.stringify` the response envelope into a JS *string*, and
    /// `network.rs` then called `JSON::stringify` on that string AGAIN before
    /// `serde_json::from_str::<FetchResponse>` — so every response failed with
    /// "invalid type: string, expected struct". If `httpFetch` ever regresses
    /// back to resolving a string instead of an object, decoding must fail
    /// loudly here rather than silently producing a wrong value.
    #[wasm_bindgen_test]
    fn old_json_string_shape_fails_to_decode_as_object() {
        let json_string = JsValue::from_str(r#"{"status":200,"headers":[],"body":[104,105]}"#);

        let result: Result<FetchResponse, _> = serde_wasm_bindgen::from_value(json_string);

        assert!(
            result.is_err(),
            "a JSON string must not decode as the FetchResponse object shape"
        );
    }

    /// **Fails on the pre-fix tree.** `headers` was a
    /// `HashMap<String, String>`, so the second `Set-Cookie` overwrote the
    /// first and a login response that set a session cookie and a CSRF cookie
    /// delivered only one of them. Both must survive, in order.
    #[wasm_bindgen_test]
    fn a_repeated_set_cookie_survives() {
        let decoded = decode(make_fetch_response_object(
            200,
            &[
                ("set-cookie", "session=abc; Path=/; HttpOnly"),
                ("content-type", "text/html"),
                ("set-cookie", "csrf=xyz; Path=/"),
            ],
            b"",
        ));

        let grouped = super::group_headers(decoded.headers);
        assert_eq!(
            grouped.get("set-cookie"),
            Some(&vec![
                "session=abc; Path=/; HttpOnly".to_string(),
                "csrf=xyz; Path=/".to_string(),
            ]),
            "both cookies must survive: {grouped:?}"
        );
        assert_eq!(
            grouped.get("content-type"),
            Some(&vec!["text/html".to_string()])
        );
    }

    async fn refusal_for(url: &str) -> String {
        let req = Request {
            method: "GET".to_string(),
            url: url.to_string(),
            headers: std::collections::HashMap::new(),
            body: None,
        };
        match BrowserNetworkService.do_request(&req).await {
            Err(NetworkError::RequestError(msg)) => msg,
            other => panic!("expected an SSRF refusal for {url}, got {other:?}"),
        }
    }

    /// **Fails on the pre-fix tree**, where every one of these went straight to
    /// `fetch`. A browser-hosted runtime that will fetch a link-local or
    /// loopback address on request is server-side request forgery aimed at the
    /// user's own network — the gate has to run before the bridge, and these
    /// assertions prove it does, because they never reach the bridge at all.
    #[wasm_bindgen_test]
    async fn the_ssrf_gate_refuses_internal_targets_before_fetching() {
        for url in [
            "http://localhost/admin",
            "http://localhost:8080/admin",
            // The RFC 6761 pseudo-domain, which resolves to loopback in Chrome
            // and Firefox: in a browser-hosted runtime `app.localhost:3000` is
            // a live dev server and `api.localhost` an internal service. The
            // upstream classifier matches the bare string only.
            "http://api.localhost:8080/admin",
            "http://app.localhost:3000/",
            "http://localhost./",
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1/",
            "http://192.168.1.1/",
            "http://10.0.0.1/",
            "http://[::1]/",
            "http://metadata.google.internal/computeMetadata/v1/",
            "file:///etc/passwd",
        ] {
            let msg = refusal_for(url).await;
            assert!(
                msg.starts_with("SSRF: refusing request to internal/blocked address:"),
                "{url} was not refused by the SSRF gate: {msg}"
            );
        }
    }
}
