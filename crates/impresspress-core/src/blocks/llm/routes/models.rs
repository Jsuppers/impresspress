//! Models endpoints (aggregated via wafer-run/llm service block).
//!
//! The service block aggregates `list_models` across every registered
//! `LlmService` impl in its router. `status` / `load` / `unload` are
//! per-(backend_id, model_id) ops forwarded verbatim. These handlers only
//! marshal HTTP ⇄ service-block JSON — no business logic here.

use wafer_core::clients::llm::{
    self as llm_client, LoadModelRequest, StatusRequest, UnloadModelRequest,
};
use wafer_run::{context::Context, Message, OutputStream};

use super::streaming::sse_json_response;
use crate::{
    blocks::llm::{
        contracts::{
            ModelInfoView, ModelListResponse, ModelStatusResponse, ModelStatusView,
            ModelUnloadResponse,
        },
        LlmBlock,
    },
    http::{err_bad_request, err_internal, ok_json},
};

/// `(backend_id, model_id)` as bound by the block's route table for
/// `/b/llm/api/models/{backend_id}/{model_id}/...`. Either is empty when the
/// request matched no row.
fn extract_model_path(msg: &Message) -> (String, String) {
    (
        msg.var("backend_id").to_string(),
        msg.var("model_id").to_string(),
    )
}

/// `GET /b/llm/api/models` — aggregated list across all registered LLM
/// backends. Authenticated (any logged-in user).
pub(in crate::blocks::llm) async fn list_models(
    _block: &LlmBlock,
    ctx: &dyn Context,
    _msg: &Message,
) -> OutputStream {
    match llm_client::list_models(ctx).await {
        Ok(models) => ok_json(&ModelListResponse {
            models: models.into_iter().map(ModelInfoView::from).collect(),
        }),
        Err(e) => err_internal("llm list_models failed", e.message),
    }
}

/// `GET /b/llm/api/models/:backend_id/:model_id/status` — per-(backend, model)
/// status. Authenticated.
pub(in crate::blocks::llm) async fn model_status(
    _block: &LlmBlock,
    ctx: &dyn Context,
    msg: &Message,
) -> OutputStream {
    let (backend_id, model_id) = extract_model_path(msg);
    if backend_id.is_empty() || model_id.is_empty() {
        return err_bad_request("Missing backend_id or model_id");
    }
    let req = StatusRequest {
        backend_id,
        model_id,
    };
    match llm_client::status(ctx, &req).await {
        Ok(status) => ok_json(&ModelStatusResponse {
            status: ModelStatusView::from(status),
        }),
        Err(e) => err_internal("llm status failed", e.message),
    }
}

/// `POST /b/llm/api/models/:backend_id/:model_id/load` — start a model
/// load, streaming `LoadProgress` events as SSE. Admin-only.
pub(in crate::blocks::llm) async fn load_model(
    _block: &LlmBlock,
    ctx: &dyn Context,
    msg: &Message,
) -> OutputStream {
    let (backend_id, model_id) = extract_model_path(msg);
    if backend_id.is_empty() || model_id.is_empty() {
        return err_bad_request("Missing backend_id or model_id");
    }
    let req = LoadModelRequest {
        backend_id,
        model_id,
    };
    let stream = match llm_client::load_model_stream(ctx, &req).await {
        Ok(s) => s,
        Err(e) => return err_internal("llm load_model failed", e.message),
    };

    sse_json_response(stream)
}

/// `POST /b/llm/api/models/:backend_id/:model_id/unload` — buffered unload.
/// Admin-only.
pub(in crate::blocks::llm) async fn unload_model(
    _block: &LlmBlock,
    ctx: &dyn Context,
    msg: &Message,
) -> OutputStream {
    let (backend_id, model_id) = extract_model_path(msg);
    if backend_id.is_empty() || model_id.is_empty() {
        return err_bad_request("Missing backend_id or model_id");
    }
    let req = UnloadModelRequest {
        backend_id,
        model_id,
    };
    match llm_client::unload_model(ctx, &req).await {
        Ok(()) => ok_json(&ModelUnloadResponse { unloaded: true }),
        Err(e) => err_internal("llm unload_model failed", e.message),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use wafer_core::clients::llm::{ModelCapabilities, ModelInfo, ModelStatus};
    use wafer_run::{streams::output::TerminalNotResponse, ErrorCode};

    use super::*;
    use crate::{
        blocks::llm::routes::test_support::{
            admin_msg, routed, stub_block, user_msg, PanicCtx, StubLlmServiceBlock,
        },
        test_support::{output_json, TestContext},
    };

    async fn ctx_with(stub: StubLlmServiceBlock) -> TestContext {
        let mut ctx = TestContext::new().await;
        ctx.register_block("wafer-run/llm", Arc::new(stub));
        ctx
    }

    /// The list is the service block's `ModelInfo` rows projected through the
    /// block's own view, field for field — nullable capability limits
    /// included, which the wire carries as explicit `null`s.
    #[tokio::test]
    async fn list_models_publishes_the_model_view() {
        let ctx = ctx_with(StubLlmServiceBlock {
            models: vec![
                ModelInfo::new("openai-main", "gpt-4o", "GPT-4o").with_capabilities(
                    ModelCapabilities {
                        streaming: true,
                        tools: true,
                        vision: false,
                        json_mode: true,
                        max_context_tokens: Some(128_000),
                        max_output_tokens: None,
                    },
                ),
            ],
            ..Default::default()
        })
        .await;

        let body = output_json(
            list_models(
                &stub_block(),
                &ctx,
                &user_msg("retrieve", "/b/llm/api/models"),
            )
            .await,
        )
        .await;

        assert_eq!(
            body,
            serde_json::json!({
                "models": [{
                    "backend_id": "openai-main",
                    "model_id": "gpt-4o",
                    "display_name": "GPT-4o",
                    "capabilities": {
                        "streaming": true,
                        "tools": true,
                        "vision": false,
                        "json_mode": true,
                        "max_context_tokens": 128000,
                        "max_output_tokens": null,
                    },
                }],
            })
        );
    }

    /// `progress` is present only while loading; the error state carries its
    /// message under the variant name. Both are what the schema promises.
    #[tokio::test]
    async fn model_status_publishes_the_status_view() {
        for (status, expected) in [
            (
                ModelStatus::ready(),
                serde_json::json!({ "status": { "state": "Ready" } }),
            ),
            (
                ModelStatus::loading(0.5),
                serde_json::json!({ "status": { "state": "Loading", "progress": 0.5 } }),
            ),
            (
                ModelStatus::error("provider disabled"),
                serde_json::json!({
                    "status": { "state": { "Error": { "message": "provider disabled" } } }
                }),
            ),
        ] {
            let ctx = ctx_with(StubLlmServiceBlock {
                status,
                ..Default::default()
            })
            .await;

            let body = output_json(
                model_status(
                    &stub_block(),
                    &ctx,
                    &routed(user_msg(
                        "retrieve",
                        "/b/llm/api/models/openai-main/gpt-4o/status",
                    )),
                )
                .await,
            )
            .await;

            assert_eq!(body, expected);
        }
    }

    #[tokio::test]
    async fn unload_model_acknowledges() {
        let ctx = ctx_with(StubLlmServiceBlock::default()).await;

        let body = output_json(
            unload_model(
                &stub_block(),
                &ctx,
                &routed(admin_msg(
                    "create",
                    "/b/llm/api/models/openai-main/gpt-4o/unload",
                )),
            )
            .await,
        )
        .await;

        assert_eq!(body, serde_json::json!({ "unloaded": true }));
    }

    #[tokio::test]
    async fn load_model_requires_path_vars() {
        let block = stub_block();
        let ctx = PanicCtx;
        // Admin but missing segments after the prefix.
        let msg = admin_msg("create", "/b/llm/api/models//load");

        let out = load_model(&block, &ctx, &msg).await;
        match out.collect_buffered().await {
            Err(TerminalNotResponse::Error(e)) => {
                assert_eq!(e.code, ErrorCode::InvalidArgument);
                assert!(
                    e.message.contains("backend_id") || e.message.contains("model_id"),
                    "got: {}",
                    e.message
                );
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unload_model_requires_path_vars() {
        let block = stub_block();
        let ctx = PanicCtx;
        let msg = admin_msg("create", "/b/llm/api/models/openai/");

        let out = unload_model(&block, &ctx, &msg).await;
        match out.collect_buffered().await {
            Err(TerminalNotResponse::Error(e)) => {
                assert_eq!(e.code, ErrorCode::InvalidArgument);
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_status_requires_path_vars() {
        let block = stub_block();
        let ctx = PanicCtx;
        let msg = user_msg("retrieve", "/b/llm/api/models//status");

        let out = model_status(&block, &ctx, &msg).await;
        match out.collect_buffered().await {
            Err(TerminalNotResponse::Error(e)) => {
                assert_eq!(e.code, ErrorCode::InvalidArgument);
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    /// Model handlers read the ids the table bound, nothing else.
    #[test]
    fn model_path_is_bound_by_the_table() {
        let m = routed(user_msg(
            "retrieve",
            "/b/llm/api/models/openai/gpt-4o/status",
        ));
        assert_eq!(
            extract_model_path(&m),
            ("openai".to_string(), "gpt-4o".to_string())
        );

        // A model id with dots and dashes is one segment.
        let m2 = routed(admin_msg(
            "create",
            "/b/llm/api/models/webllm/llama-3.1-8b/load",
        ));
        assert_eq!(
            extract_model_path(&m2),
            ("webllm".to_string(), "llama-3.1-8b".to_string())
        );

        // Missing model id: no row matches, nothing is bound, the handler
        // answers InvalidArgument (see `unload_model_requires_path_vars`).
        let mut m3 = admin_msg("create", "/b/llm/api/models/openai/");
        assert!(crate::endpoint_match::dispatch(&mut m3, crate::blocks::llm::ROUTES).is_none());
        assert_eq!(extract_model_path(&m3), (String::new(), String::new()));
    }
}
