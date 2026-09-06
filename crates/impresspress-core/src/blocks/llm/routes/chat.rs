//! Chat request handling.
//!
//! Both the buffered and streaming chat endpoints share [`dispatch_chat`]:
//! parse the body, persist the user message, load history, resolve the
//! provider + model, and call `wafer-run/llm` via the typed client. The
//! buffered handler ([`handle_chat`]) drains the resulting `ChatChunk`
//! stream itself; the streaming handler ([`handle_chat_stream`]) hands it
//! off to [`super::streaming::sse_chat_response`], which owns the SSE
//! framing.

use futures::StreamExt;
use wafer_core::clients::{
    llm::{
        self as llm_client, ChatChunk, ChatContent, ChatMessage, ChatParams, ChatRequest, ChatRole,
        ChunkDelta,
    },
    NativeTypedFrameStream,
};
use wafer_run::{context::Context, InputStream, Message, OutputStream};

use super::streaming::sse_chat_response;
use crate::{
    blocks::{
        llm::{
            contracts, messages_create, messages_list, record_field, LlmBlock, DEFAULT_PROVIDER,
        },
        messages::contracts::EntryRole,
    },
    http::{err_bad_request, err_internal, ok_json},
};

/// Legacy default provider block name that must be replaced with the first
/// enabled provider from `impresspress__llm__providers` before the request
/// reaches the `wafer-run/llm` service.
const LEGACY_PROVIDER_BLOCK: &str = DEFAULT_PROVIDER;

/// The messages block's role as the LLM service's [`ChatRole`].
///
/// Total by construction, which is the whole of B20's fix. The function this
/// replaces matched `"assistant"` and `"system"` and sent **everything else**
/// to [`ChatRole::User`] — including `"agent"`, the one role the messages
/// composer offers. So an entry an agent posted came back to the model as
/// the user's own next instruction, and adding a role to the messages block
/// would have silently done the same thing again. A new [`EntryRole`]
/// variant now fails to compile here instead.
fn chat_role(role: EntryRole) -> ChatRole {
    match role {
        EntryRole::User => ChatRole::User,
        EntryRole::Assistant => ChatRole::Assistant,
        EntryRole::System => ChatRole::System,
    }
}

/// Build a text-content `ChatMessage` for the given role.
///
/// `ChatRole::Tool` is unreachable via [`chat_role`] (no [`EntryRole`] maps
/// to it), but if it ever bubbles up here a tool-result message would
/// require a `tool_call_id` we don't have — so coerce it to a user turn
/// rather than emit an invalid Tool message.
fn build_text_message(role: ChatRole, content: String) -> ChatMessage {
    let role = match role {
        ChatRole::Tool => ChatRole::User,
        other => other,
    };
    ChatMessage {
        role,
        content: ChatContent::Text(content),
        tool_call_id: None,
        tool_calls: Vec::new(),
    }
}

/// Convert stored message history into the `ChatMessage` vector the service
/// interface expects.
///
/// An entry whose `role` is not an [`EntryRole`] is skipped, which is what
/// already happened to the rows this can still see: `role`'s column default
/// was `''` before the messages block typed it, and an empty role has always
/// been dropped here. Skipping is deliberately not the same as the old
/// fallback — an unreadable role must not become a *user* turn, because that
/// puts words the user never wrote into the model's input.
fn history_to_messages(history: &[serde_json::Value]) -> Vec<ChatMessage> {
    history
        .iter()
        .filter_map(|entry| {
            let role: EntryRole = serde_json::from_value(serde_json::Value::String(
                record_field(entry, "role").to_string(),
            ))
            .ok()?;
            Some(build_text_message(
                chat_role(role),
                record_field(entry, "content").to_string(),
            ))
        })
        .collect()
}

/// Resolve a legacy `impresspress/provider-llm` default into a concrete
/// backend_id by reading the in-memory provider cache (loaded at `Init` and
/// refreshed on every provider CRUD write) via the [`ProviderAdmin`] handle.
/// Returns `Err` if no enabled provider is configured.
///
/// [`ProviderAdmin`]: crate::blocks::llm::provider_admin::ProviderAdmin
fn resolve_backend_id(block: &LlmBlock, provider_block: &str) -> Result<String, &'static str> {
    if provider_block != LEGACY_PROVIDER_BLOCK {
        // `provider_block` is the backend_id directly (non-legacy path).
        return Ok(provider_block.to_string());
    }

    block
        .provider_admin
        .providers_snapshot()
        .into_iter()
        .find(|cfg| cfg.enabled)
        .map(|cfg| cfg.name)
        .ok_or("no enabled provider configured")
}

/// Common prelude for both chat handlers: parse the body, persist the user
/// message, load history, resolve provider + model, build the `ChatRequest`,
/// and call `wafer-run/llm` via the typed client.
///
/// Returns the typed `ChatChunk` stream from the service on success, or a
/// ready-to-return error stream on any failure.
async fn dispatch_chat(
    block: &LlmBlock,
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> Result<DispatchOutcome, OutputStream> {
    let raw = input.collect_to_bytes().await;
    let contracts::ChatRequest {
        thread_id,
        message,
        provider,
        model,
    } = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return Err(err_bad_request(&format!("Invalid body: {e}"))),
    };

    // 1. Persist the user message before calling the model.
    let _ = messages_create(ctx, msg, &thread_id, EntryRole::User, &message).await;

    // 2. Load prior history (which now includes the just-written user msg).
    let history = messages_list(ctx, msg, &thread_id).await;
    let messages = history_to_messages(&history);

    // 3. Resolve the provider block / model via the block's existing logic.
    //    An unreadable per-thread override is an error, not a silent fall
    //    back to the global default: the caller pinned a backend and would
    //    otherwise be billed to another one without ever learning.
    let (provider_block, resolved_model) = match block
        .resolve_provider(ctx, &thread_id, provider.as_deref(), model.as_deref())
        .await
    {
        Ok(resolved) => resolved,
        Err(e) => return Err(err_internal("resolve_provider failed", e)),
    };

    // 4. Map the legacy `impresspress/provider-llm` default into a concrete
    //    backend_id (first enabled provider). Non-legacy values pass through.
    let backend_id = match resolve_backend_id(block, &provider_block) {
        Ok(id) => id,
        Err(e) => return Err(err_internal("resolve_backend_id failed", e)),
    };

    // 5. Build the service request and dispatch via the typed client.
    let chat_req = ChatRequest {
        backend_id,
        model: resolved_model.clone(),
        messages,
        params: ChatParams::default(),
        tools: Vec::new(),
        extra: serde_json::Value::Null,
    };
    let stream = match llm_client::chat_stream(ctx, &chat_req).await {
        Ok(s) => s,
        Err(e) => return Err(err_internal("llm chat dispatch", e.message)),
    };
    Ok(DispatchOutcome {
        thread_id,
        model: resolved_model,
        stream,
    })
}

/// Result of the shared chat prelude — owns the typed stream plus the
/// metadata the buffered + streaming handlers need to echo back.
struct DispatchOutcome {
    thread_id: String,
    /// Resolved model string — what we asked the service to run. Returned to
    /// the client so the UI can label the assistant message with the actual
    /// model used (the service does not echo it back in the chunk stream).
    model: String,
    stream: NativeTypedFrameStream<ChatChunk>,
}

/// Cap (in bytes) on the assistant reply we'll buffer in the JSON chat path.
/// A misbehaving model that streams indefinitely can otherwise hold an entire
/// response in memory before responding. SSE callers (`/chat/stream`) are
/// unaffected — they forward each chunk as it arrives.
///
/// Shared with [`super::streaming::sse_chat_response`], which applies the
/// same cap to the persisted (not the forwarded) assistant text.
pub(super) const MAX_BUFFERED_RESPONSE_BYTES: usize = 1024 * 1024;

/// Buffered chat handler: collects the full `ChatChunk` stream, concatenates
/// all text deltas, persists the assistant message, and returns a JSON body.
pub(in crate::blocks::llm) async fn handle_chat(
    block: &LlmBlock,
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let DispatchOutcome {
        thread_id,
        model: model_used,
        mut stream,
    } = match dispatch_chat(block, ctx, msg, input).await {
        Ok(x) => x,
        Err(err) => return err,
    };

    // Drain the typed `ChatChunk` stream, concatenating `ChunkDelta::Text`
    // bytes into the assistant reply. Propagate any error terminal as a 500.
    let mut content = String::new();
    let mut truncated = false;
    while let Some(item) = stream.next().await {
        let chunk = match item {
            Ok(c) => c,
            Err(e) => return err_internal("llm service error", e.message),
        };
        match chunk.delta {
            ChunkDelta::Text(s) => {
                if content.len() + s.len() > MAX_BUFFERED_RESPONSE_BYTES {
                    // Stop appending but keep draining so the stream can
                    // close cleanly and any usage frame still flows through.
                    truncated = true;
                    continue;
                }
                content.push_str(&s);
            }
            // Tool-call and empty deltas are ignored in the buffered path.
            ChunkDelta::ToolCallStart { .. }
            | ChunkDelta::ToolCallArguments { .. }
            | ChunkDelta::ToolCallComplete { .. }
            | ChunkDelta::Empty => {}
        }
    }
    if truncated {
        tracing::warn!(
            cap = MAX_BUFFERED_RESPONSE_BYTES,
            "llm buffered response exceeded cap — truncated"
        );
    }

    // Persist the assistant reply.
    let saved = messages_create(ctx, msg, &thread_id, EntryRole::Assistant, &content).await;
    let message_id = saved
        .as_ref()
        .and_then(|v| {
            v.get("id")
                .or_else(|| v.get("data").and_then(|d| d.get("id")))
        })
        .and_then(|id| id.as_str())
        .unwrap_or("")
        .to_string();

    ok_json(&contracts::ChatResponse {
        content,
        message_id,
        model: model_used,
        truncated,
    })
}

/// SSE streaming chat handler: forwards each `ChatChunk` (as its JSON
/// encoding) to the HTTP response as a `data:` frame, then persists the
/// accumulated assistant text to the messages block at natural
/// end-of-stream — see [`sse_chat_response`].
pub(in crate::blocks::llm) async fn handle_chat_stream(
    block: &LlmBlock,
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    // Run the shared prelude. On success we own the typed `ChatChunk`
    // stream; we re-emit each chunk as JSON SSE with a body-level
    // content-type.
    let DispatchOutcome {
        thread_id,
        model: _,
        stream,
    } = match dispatch_chat(block, ctx, msg, input).await {
        Ok(x) => x,
        Err(err) => return err,
    };

    // The SSE producer runs in a spawned task, so it can't borrow `ctx` or
    // `msg`. `Context::clone_arc()` yields an owned handle that crosses the
    // spawn boundary, and `Message` is `Clone` — `messages_create` only
    // reads the forwarded auth identity off it.
    sse_chat_response(stream, ctx.clone_arc(), msg.clone(), thread_id)
}

#[cfg(test)]
mod tests {
    use wafer_run::{streams::output::TerminalNotResponse, ErrorCode};

    use super::*;
    use crate::blocks::llm::routes::test_support::{stub_block, PanicCtx};

    /// The buffered reply is the contract's four fields and nothing else.
    /// `message_id` stays present (empty) when persistence is skipped — here
    /// no messages block is registered — because the schema says it is
    /// always there.
    #[tokio::test]
    async fn handle_chat_publishes_exactly_the_contract_fields() {
        use std::sync::Arc;

        use wafer_core::clients::llm::FinishReason;

        use crate::{
            blocks::llm::{
                routes::test_support::StubLlmServiceBlock, DEFAULT_MODEL_VAR, DEFAULT_PROVIDER_VAR,
            },
            test_support::{output_json, TestContext},
        };

        let mut ctx = TestContext::with_llm().await;
        ctx.set_config(DEFAULT_PROVIDER_VAR, "stub-backend");
        ctx.set_config(DEFAULT_MODEL_VAR, "stub-model");
        ctx.register_block(
            "wafer-run/llm",
            Arc::new(StubLlmServiceBlock {
                chat_chunks: vec![
                    ChatChunk::text("Hel"),
                    ChatChunk::text("lo"),
                    ChatChunk::finish(FinishReason::Stop, None),
                ],
                ..Default::default()
            }),
        );

        let body = output_json(
            handle_chat(
                &stub_block(),
                &ctx,
                &Message::new("create:/b/llm/api/chat"),
                InputStream::from_bytes(br#"{"thread_id":"t1","message":"hi"}"#.to_vec()),
            )
            .await,
        )
        .await;

        let mut got: Vec<&str> = body
            .as_object()
            .expect("chat response object")
            .keys()
            .map(String::as_str)
            .collect();
        got.sort_unstable();
        assert_eq!(
            got,
            ["content", "message_id", "model", "truncated"],
            "the wire field set must equal ChatResponse's"
        );
        assert_eq!(body["content"], "Hello");
        assert_eq!(body["model"], "stub-model");
        assert_eq!(body["truncated"], false);
        assert_eq!(body["message_id"], "");
    }

    #[tokio::test]
    async fn handle_chat_returns_bad_request_on_invalid_json() {
        let block = stub_block();
        let ctx = PanicCtx;
        let msg = Message::new("create:/b/llm/api/chat");
        let input = InputStream::from_bytes(b"not json".to_vec());

        let out = handle_chat(&block, &ctx, &msg, input).await;
        let result = out.collect_buffered().await;
        match result {
            Err(TerminalNotResponse::Error(e)) => {
                assert_eq!(e.code, ErrorCode::InvalidArgument);
                assert!(
                    e.message.contains("Invalid body"),
                    "expected Invalid body message, got: {}",
                    e.message
                );
            }
            other => panic!("expected InvalidArgument error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_chat_stream_returns_bad_request_on_invalid_json() {
        let block = stub_block();
        let ctx = PanicCtx;
        let msg = Message::new("create:/b/llm/api/chat/stream");
        let input = InputStream::from_bytes(b"{".to_vec());

        let out = handle_chat_stream(&block, &ctx, &msg, input).await;
        let result = out.collect_buffered().await;
        match result {
            Err(TerminalNotResponse::Error(e)) => {
                assert_eq!(e.code, ErrorCode::InvalidArgument);
            }
            other => panic!("expected InvalidArgument error, got {other:?}"),
        }
    }

    /// B20. The messages block documents `agent` as a role, its composer is
    /// the only control in the tree that offers one, and every entry stored
    /// with it was replayed to the model as if the *user* had written it —
    /// so an agent's own turn came back as the user's next instruction.
    ///
    /// The four roles a stored entry can hold map onto three `ChatRole`s,
    /// and `agent` is an alias of `assistant`, not of `user`.
    #[test]
    fn an_agent_turn_is_replayed_as_an_assistant_turn() {
        let history: Vec<serde_json::Value> = ["user", "assistant", "agent", "system"]
            .iter()
            .map(|role| serde_json::json!({ "role": role, "content": "t" }))
            .collect();

        let roles: Vec<ChatRole> = history_to_messages(&history)
            .iter()
            .map(|m| m.role)
            .collect();

        assert_eq!(
            roles,
            vec![
                ChatRole::User,
                ChatRole::Assistant,
                ChatRole::Assistant,
                ChatRole::System,
            ],
            "an entry stored with the messages block's `agent` role must not \
             be replayed to the model as a user turn"
        );
    }

    /// B20, end to end and through the real wire.
    ///
    /// A human posts `role=agent` — the value the messages composer offered
    /// — into a thread, and the next chat request rebuilds its history from
    /// that column. Before `EntryRole`, `role_from_str` sent every value it
    /// did not recognise to `ChatRole::User`, so the agent's own turn came
    /// back to the model as the user's next instruction. The unit test above
    /// pins the mapping; this pins that the two blocks agree on the value
    /// travelling between them.
    #[tokio::test]
    async fn an_entry_posted_as_agent_reaches_the_model_as_an_assistant_turn() {
        use crate::blocks::messages::{service, test_support::ctx_with_messages};

        let ctx = ctx_with_messages().await;
        let thread =
            service::create_context(&ctx, "user-a", "conversation", "T", "", "", None, None)
                .await
                .expect("create the thread");

        // Posted the way the composer posts it: through the messages block's
        // own HTTP surface, not through a repo call that could not see the
        // request parsing.
        let mut post = crate::util::block_request(
            "create",
            "POST",
            &format!("/b/messages/api/contexts/{}/entries", thread.id),
            &crate::test_support::auth_msg("create", "/b/llm/api/chat", "user-a"),
        );
        post.set_meta("req.content_type", "application/json");
        let body = serde_json::to_vec(&serde_json::json!({
            "kind": "message",
            "role": "agent",
            "content": "I did the thing",
        }))
        .expect("body");
        let stored = ctx
            .call_block("impresspress/messages", post, InputStream::from_bytes(body))
            .await;
        assert_eq!(
            crate::test_support::output_json(stored).await["data"]["role"],
            "assistant"
        );

        let history = messages_list(
            &ctx,
            &crate::test_support::auth_msg("retrieve", "/b/llm/api/chat", "user-a"),
            &thread.id,
        )
        .await;
        assert_eq!(
            history_to_messages(&history)
                .iter()
                .map(|m| m.role)
                .collect::<Vec<_>>(),
            vec![ChatRole::Assistant],
            "an entry the agent posted must not reach the model as a user turn"
        );
    }

    #[test]
    fn every_entry_role_has_its_own_chat_role() {
        assert_eq!(chat_role(EntryRole::User), ChatRole::User);
        assert_eq!(chat_role(EntryRole::Assistant), ChatRole::Assistant);
        assert_eq!(chat_role(EntryRole::System), ChatRole::System);
    }

    /// The replacement for `role_from_str_unknown_falls_back_to_user`, which
    /// asserted that `"tool"`, `""` and `"random"` were all replayed as the
    /// *user*. That fallback is the bug: an entry whose role cannot be read
    /// is left out of the history rather than attributed to the person who
    /// did not write it.
    #[test]
    fn an_unreadable_role_is_left_out_of_the_history() {
        for role in ["tool", "", "random"] {
            let history = vec![serde_json::json!({ "role": role, "content": "t" })];
            assert!(
                history_to_messages(&history).is_empty(),
                "role {role:?} must not be replayed as a user turn"
            );
        }
    }

    #[test]
    fn history_to_messages_prefers_data_object() {
        let history = vec![serde_json::json!({
            "data": { "role": "user", "content": "hi" }
        })];
        let msgs = history_to_messages(&history);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, ChatRole::User);
        assert!(
            matches!(&msgs[0].content, wafer_block::wire::llm::ChatContent::Text(t) if t == "hi")
        );
    }

    #[test]
    fn history_to_messages_falls_back_to_flat_fields() {
        let history = vec![serde_json::json!({
            "role": "assistant",
            "content": "yes"
        })];
        let msgs = history_to_messages(&history);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, ChatRole::Assistant);
        assert!(
            matches!(&msgs[0].content, wafer_block::wire::llm::ChatContent::Text(t) if t == "yes")
        );
    }

    #[test]
    fn history_to_messages_skips_entries_without_role() {
        let history = vec![
            serde_json::json!({ "content": "orphan" }),
            serde_json::json!({ "role": "system", "content": "kept" }),
        ];
        let msgs = history_to_messages(&history);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, ChatRole::System);
    }
}
