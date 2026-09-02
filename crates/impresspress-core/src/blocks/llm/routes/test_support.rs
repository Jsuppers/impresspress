//! Shared test fixtures for the `routes` submodules: minimal `Context`
//! stubs and message builders used across the chat/streaming/providers/
//! models unit tests.

use std::sync::{Arc, Mutex};

use wafer_block::common::ServiceOp;
use wafer_core::{
    clients::llm::{ChatChunk, ModelInfo, ModelStatus},
    interfaces::llm::service::LlmError,
};
use wafer_run::{
    context::Context, Block, BlockCategory, BlockInfo, ErrorCode, InputStream, LifecycleEvent,
    Message, OutputStream, WaferError,
};

use crate::blocks::llm::{
    provider_admin::{NoopProviderAdmin, ProviderAdmin},
    providers::config::ProviderConfig,
    LlmBlock,
};

/// Minimal Context that panics on `call_block` — the bad-request tests must
/// reject before any block dispatch.
#[derive(Clone)]
pub(super) struct PanicCtx;

#[async_trait::async_trait]
impl Context for PanicCtx {
    async fn call_block(
        &self,
        _block_name: &str,
        _msg: Message,
        _input: InputStream,
    ) -> OutputStream {
        panic!("call_block must not be invoked on a parse-error path");
    }
    fn is_cancelled(&self) -> bool {
        false
    }
    fn config_get(&self, _key: &str) -> Option<&str> {
        None
    }
    fn clone_arc(&self) -> std::sync::Arc<dyn Context> {
        std::sync::Arc::new(self.clone())
    }
}

/// The parse-error tests reject before reaching the provider-admin surface,
/// so the no-op handle suffices.
pub(super) fn stub_block() -> LlmBlock {
    LlmBlock::new(Arc::new(NoopProviderAdmin))
}

/// One recorded `call_block` invocation on a [`RecordingCtx`].
pub(super) struct RecordedCall {
    pub(super) block_name: String,
    pub(super) msg: Message,
    pub(super) body: Vec<u8>,
}

/// Context that records every `call_block` invocation (block name, message,
/// drained input body) and answers with a canned OK JSON body. `clone_arc`
/// hands out a handle sharing the same call log, so a test can inspect calls
/// made through the cloned Arc.
#[derive(Clone, Default)]
pub(super) struct RecordingCtx {
    calls: Arc<std::sync::Mutex<Vec<RecordedCall>>>,
}

impl RecordingCtx {
    pub(super) fn calls(&self) -> std::sync::MutexGuard<'_, Vec<RecordedCall>> {
        self.calls.lock().expect("call log lock")
    }
}

#[async_trait::async_trait]
impl Context for RecordingCtx {
    async fn call_block(&self, block_name: &str, msg: Message, input: InputStream) -> OutputStream {
        let body = input.collect_to_bytes().await;
        self.calls().push(RecordedCall {
            block_name: block_name.to_string(),
            msg,
            body,
        });
        OutputStream::respond(br#"{"id":"entry-1"}"#.to_vec())
    }
    fn is_cancelled(&self) -> bool {
        false
    }
    fn config_get(&self, _key: &str) -> Option<&str> {
        None
    }
    fn clone_arc(&self) -> Arc<dyn Context> {
        Arc::new(self.clone())
    }
}

pub(super) fn admin_msg(action: &str, path: &str) -> Message {
    let mut m = Message::new(format!("{action}:{path}"));
    m.set_meta(wafer_run::META_REQ_ACTION, action);
    m.set_meta(wafer_run::META_REQ_RESOURCE, path);
    m.set_meta(wafer_run::META_AUTH_USER_ID, "admin-user");
    m.set_meta("auth.user_roles", "admin");
    m
}

pub(super) fn user_msg(action: &str, path: &str) -> Message {
    let mut m = Message::new(format!("{action}:{path}"));
    m.set_meta(wafer_run::META_REQ_ACTION, action);
    m.set_meta(wafer_run::META_REQ_RESOURCE, path);
    m.set_meta(wafer_run::META_AUTH_USER_ID, "regular-user");
    m.set_meta("auth.user_roles", "user");
    m
}

/// `ProviderAdmin` that keeps whatever `configure` hands it — including the
/// `api_key` that `reload_provider_service` resolves from `key_var` — and
/// answers `discover_models` with a fixed two-model list.
///
/// The wire-shape tests read the snapshot back to prove the secret reached
/// the handle the handlers hold, and only then check that it never reaches
/// the wire. A leak test whose fixture never had the secret in reach would
/// pass against a leaking handler.
#[derive(Default)]
pub(super) struct RecordingProviderAdmin {
    configured: Mutex<Vec<ProviderConfig>>,
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl ProviderAdmin for RecordingProviderAdmin {
    fn configure(&self, providers: Vec<ProviderConfig>) {
        *self.configured.lock().expect("configured lock") = providers;
    }

    fn providers_snapshot(&self) -> Vec<ProviderConfig> {
        self.configured.lock().expect("configured lock").clone()
    }

    async fn discover_models(&self, provider_name: &str) -> Result<Vec<ModelInfo>, LlmError> {
        Ok(vec![
            ModelInfo::new(provider_name, "gpt-4o", "GPT-4o"),
            ModelInfo::new(provider_name, "gpt-4o-mini", "GPT-4o mini"),
        ])
    }
}

/// Stub `wafer-run/llm` service block with scripted answers: `llm.list_models`
/// returns `models`, `llm.status` returns `status`, `llm.chat` streams
/// `chat_chunks` one frame each, and `llm.unload_model` acknowledges with an
/// empty body. Anything else errors loudly so a test cannot silently exercise
/// an op it did not script.
pub(super) struct StubLlmServiceBlock {
    pub(super) models: Vec<ModelInfo>,
    pub(super) status: ModelStatus,
    pub(super) chat_chunks: Vec<ChatChunk>,
}

impl Default for StubLlmServiceBlock {
    fn default() -> Self {
        Self {
            models: Vec::new(),
            status: ModelStatus::ready(),
            chat_chunks: Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl Block for StubLlmServiceBlock {
    fn info(&self) -> BlockInfo {
        BlockInfo::new(
            "wafer-run/llm",
            "0.0.1",
            "llm@v1",
            "stub llm service block for route tests",
        )
        .category(BlockCategory::Service)
    }

    async fn handle(&self, _ctx: &dyn Context, msg: Message, _input: InputStream) -> OutputStream {
        match msg.kind.as_str() {
            ServiceOp::LLM_LIST_MODELS => OutputStream::respond(
                wafer_block::codec::encode(&self.models).expect("encode models"),
            ),
            ServiceOp::LLM_STATUS => OutputStream::respond(
                wafer_block::codec::encode(&self.status).expect("encode status"),
            ),
            ServiceOp::LLM_UNLOAD_MODEL => OutputStream::respond(Vec::new()),
            ServiceOp::LLM_CHAT => {
                let frames: Vec<Vec<u8>> = self
                    .chat_chunks
                    .iter()
                    .map(|chunk| wafer_block::codec::encode(chunk).expect("encode chunk"))
                    .collect();
                OutputStream::from_producer(move |sink, _cancel| async move {
                    for frame in frames {
                        if sink.send_chunk(frame).await.is_err() {
                            return;
                        }
                    }
                })
            }
            other => OutputStream::error(WaferError::new(
                ErrorCode::Unimplemented,
                format!("StubLlmServiceBlock: unhandled op {other}"),
            )),
        }
    }

    async fn lifecycle(&self, _ctx: &dyn Context, _e: LifecycleEvent) -> Result<(), WaferError> {
        Ok(())
    }
}
