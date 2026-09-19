//! A fake Anthropic-protocol provider on loopback, plus the `wafer-run/llm`
//! service block that routes to it.
//!
//! Shared by the chat-route tests and the vector block's contextual-retrieval
//! test. Both need a chat request to travel through the real
//! [`ProviderLlmService`] and the real Anthropic encoder, because that
//! encoder is the only place an absent `max_tokens` is visible: it refuses
//! the request with [`anthropic::EncodeError::MissingMaxTokens`] before any
//! byte reaches a provider. A stubbed `wafer-run/llm` block never encodes
//! anything, so it cannot see that at all — which is why every existing chat
//! test passed while every Anthropic chat in production failed.
//!
//! [`anthropic::EncodeError::MissingMaxTokens`]: super::anthropic::EncodeError::MissingMaxTokens

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wafer_run::Block;

use super::{
    config::{ProviderConfig, ProviderProtocol},
    ProviderLlmService,
};
use crate::blocks::llm::provider_admin::ProviderAdmin;

/// Provider name the fake is configured under — the `backend_id` a chat
/// request routes on.
pub(crate) const BACKEND_ID: &str = "anthropic-main";

/// Model the fake answers for. Never inspected by the fake; it is what the
/// encoder puts in the request body.
pub(crate) const MODEL: &str = "claude-sonnet-4-5";

/// A one-provider Anthropic deployment: an HTTP server on loopback that
/// answers every `POST /messages` with a scripted reply, and a record of the
/// request bodies it was sent.
pub(crate) struct FakeAnthropic {
    endpoint: String,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl FakeAnthropic {
    /// Start a provider that answers every request with `text` as a single
    /// `text_delta`, followed by `end_turn` and `message_stop`.
    ///
    /// Loopback is addressed as `localhost` rather than `127.0.0.1` because
    /// that is the only plain-HTTP host `crate::util::validate_url_value`
    /// allows — the affordance that exists for self-hosted models, and the
    /// one the real `chat_stream` re-checks before dispatching.
    pub(crate) async fn answering(text: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind("localhost:0")
            .await
            .expect("bind a loopback port");
        let port = listener.local_addr().expect("listener address").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    // Read the whole request. Stopping early leaves unread
                    // bytes in the socket, and closing on those sends an RST
                    // that discards the response we are about to write.
                    let mut req = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !ends_request(&req) {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    if let Some(body) = request_body(&req) {
                        recorded.lock().expect("recorded requests lock").push(body);
                    }
                    let _ = sock
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n\
                                 event: content_block_delta\n\
                                 data: {}\n\n\
                                 event: message_delta\n\
                                 data: {{\"delta\":{{\"stop_reason\":\"end_turn\"}},\
                                 \"usage\":{{\"output_tokens\":3}}}}\n\n\
                                 event: message_stop\n\
                                 data: {{}}\n\n",
                                serde_json::json!({
                                    "index": 0,
                                    "delta": { "type": "text_delta", "text": text },
                                })
                            )
                            .as_bytes(),
                        )
                        .await;
                    let _ = sock.flush().await;
                    // Half-close so the client sees a clean end of body.
                    let _ = sock.shutdown().await;
                });
            }
        });
        Self {
            endpoint: format!("http://localhost:{port}"),
            requests,
        }
    }

    /// Every request body the provider was sent, as the JSON the encoder
    /// produced. This is where `max_tokens` is observable as a *value* rather
    /// than as the absence of an error.
    pub(crate) fn requests(&self) -> Vec<serde_json::Value> {
        self.requests
            .lock()
            .expect("recorded requests lock")
            .clone()
    }

    /// The `wafer-run/llm` service block a test registers: the production
    /// service-block wrapper around a real [`ProviderLlmService`] holding one
    /// enabled Anthropic provider pointed at this fake.
    pub(crate) fn llm_service_block(&self) -> Arc<dyn Block> {
        let svc = ProviderLlmService::try_new().expect("build the provider service");
        svc.configure(vec![ProviderConfig::new(
            BACKEND_ID,
            ProviderProtocol::Anthropic,
            &self.endpoint,
        )
        .with_api_key("sk-ant-test")
        .with_models(vec![MODEL.to_string()])])
            .expect("the provider router accepts configuration");
        Arc::new(wafer_core::service_blocks::llm::LlmBlock::new(Arc::new(
            svc,
        )))
    }
}

/// Whether `req` contains a complete HTTP request: headers, plus the body
/// named by its `content-length` (the chat POST always has one).
fn ends_request(req: &[u8]) -> bool {
    body_span(req).is_some_and(|(start, len)| req.len() >= start + len)
}

/// The request body, decoded as JSON.
fn request_body(req: &[u8]) -> Option<serde_json::Value> {
    let (start, len) = body_span(req)?;
    serde_json::from_slice(req.get(start..start + len)?).ok()
}

/// `(body offset, content-length)` once the headers have arrived.
fn body_span(req: &[u8]) -> Option<(usize, usize)> {
    let text = String::from_utf8_lossy(req);
    let head_end = text.find("\r\n\r\n")?;
    let len: usize = text[..head_end]
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0);
    Some((head_end + 4, len))
}
