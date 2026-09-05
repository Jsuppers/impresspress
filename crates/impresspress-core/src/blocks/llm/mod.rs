pub mod contracts;
pub mod migrations;
pub mod pages;
pub mod provider_admin;
pub mod providers;
pub mod routes;
pub mod schema;
pub mod ui;

use std::sync::Arc;

use wafer_core::clients::{config, database as db};
use wafer_run::{
    context::Context, Block, BlockInfo, ConfigVar, HttpMethod, InputStream, InstanceMode,
    LifecycleEvent, LifecycleType, Message, OutputStream, WaferError,
};

use self::provider_admin::ProviderAdmin;
use crate::{
    endpoint_match::{self, request_schema_of, response_schema_of, EndpointRoute},
    http::{err_bad_request, err_internal, err_not_found, ok_json},
    util::json_map,
};

/// In-block dispatch targets, one per declared HTTP endpoint.
#[derive(Clone, Copy)]
enum Route {
    ChatPage,
    ThreadPage,
    SettingsPage,
    ProvidersPage,
    ModelsPage,
    Chat,
    ChatStream,
    DiscoverModels,
    ListProviders,
    CreateProvider,
    UpdateProvider,
    DeleteProvider,
    ModelStatus,
    LoadModel,
    UnloadModel,
    ListModels,
    GetConfig,
    PostConfig,
    DeleteConfig,
}

/// The block's HTTP surface: what `handle()` dispatches on and what
/// `info().endpoints` is generated from. Sub-resource templates
/// (`.../discover-models`, `.../load`, `.../status`) precede the generic
/// `.../{id}` / `.../models` templates so the specific route wins.
/// `{id}`/`{backend_id}`/`{model_id}` are bound into `req.param.*`.
///
/// The chat UI is reached from the ADMIN sidebar (nav_groups::admin
/// "Communication" group); the pre-refactor `handle()` gated every non-API
/// page on `is_admin`, so the pages are declared `Admin` to keep that exact
/// outcome as the single, centrally enforced policy.
const ROUTES: &[EndpointRoute<Route>] = &[
    // UI pages
    EndpointRoute::admin(HttpMethod::Get, "/b/llm/", Route::ChatPage).summary("Chat UI"),
    EndpointRoute::admin(HttpMethod::Get, "/b/llm/threads/{id}", Route::ThreadPage)
        .summary("Chat UI (thread permalink)"),
    EndpointRoute::admin(HttpMethod::Get, "/b/llm/settings", Route::SettingsPage)
        .summary("LLM settings page"),
    EndpointRoute::admin(HttpMethod::Get, "/b/llm/providers", Route::ProvidersPage)
        .summary("Providers admin"),
    EndpointRoute::admin(HttpMethod::Get, "/b/llm/models", Route::ModelsPage)
        .summary("Models admin"),
    // Chat API
    EndpointRoute::authenticated(HttpMethod::Post, "/b/llm/api/chat", Route::Chat)
        .summary("Send a chat message")
        .input(request_schema_of::<contracts::ChatRequest>)
        .output(response_schema_of::<contracts::ChatResponse>),
    // Same request as `/api/chat`; the response is `text/event-stream`, one
    // `data:` frame per `ChatChunk`, then `data: [DONE]` (or `event: error`).
    // No `.output(..)`: it would publish an `application/json` schema for a
    // body this endpoint never sends, and the frame type is wafer-run's
    // `ChatChunk`, which carries no JsonSchema derive to mirror.
    EndpointRoute::authenticated(
        HttpMethod::Post,
        "/b/llm/api/chat/stream",
        Route::ChatStream,
    )
    .summary("Send a chat message (SSE streaming)")
    .input(request_schema_of::<contracts::ChatRequest>),
    // Provider CRUD (specific sub-resource first)
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/llm/api/providers/{id}/discover-models",
        Route::DiscoverModels,
    )
    .summary("Discover provider models via /v1/models")
    .path_params(provider_id_path_schema)
    .output(response_schema_of::<contracts::DiscoveredModelsResponse>),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/llm/api/providers",
        Route::ListProviders,
    )
    .summary("List configured LLM providers")
    .output(response_schema_of::<contracts::ProviderListResponse>),
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/llm/api/providers",
        Route::CreateProvider,
    )
    .summary("Create LLM provider")
    .input(request_schema_of::<contracts::CreateProviderRequest>)
    .output(response_schema_of::<contracts::ProviderView>),
    EndpointRoute::admin(
        HttpMethod::Patch,
        "/b/llm/api/providers/{id}",
        Route::UpdateProvider,
    )
    .summary("Update LLM provider")
    .path_params(provider_id_path_schema)
    .input(request_schema_of::<contracts::UpdateProviderRequest>)
    .output(response_schema_of::<contracts::ProviderView>),
    EndpointRoute::admin(
        HttpMethod::Delete,
        "/b/llm/api/providers/{id}",
        Route::DeleteProvider,
    )
    .summary("Delete LLM provider")
    .path_params(provider_id_path_schema)
    .output(response_schema_of::<contracts::ProviderDeleteResponse>),
    // Models (specific sub-resources first)
    EndpointRoute::authenticated(
        HttpMethod::Get,
        "/b/llm/api/models/{backend_id}/{model_id}/status",
        Route::ModelStatus,
    )
    .summary("Model status (ready / loading / unloaded)")
    .path_params(model_path_schema)
    .output(response_schema_of::<contracts::ModelStatusResponse>),
    // Takes no body; answers `text/event-stream`, one `data:` frame per
    // `LoadProgress`, then `data: [DONE]`. No `.output(..)` for the same
    // reason as `/api/chat/stream`.
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/llm/api/models/{backend_id}/{model_id}/load",
        Route::LoadModel,
    )
    .summary("Load a model (SSE progress)")
    .path_params(model_path_schema),
    EndpointRoute::admin(
        HttpMethod::Post,
        "/b/llm/api/models/{backend_id}/{model_id}/unload",
        Route::UnloadModel,
    )
    .summary("Unload a model")
    .path_params(model_path_schema)
    .output(response_schema_of::<contracts::ModelUnloadResponse>),
    EndpointRoute::authenticated(HttpMethod::Get, "/b/llm/api/models", Route::ListModels)
        .summary("List available models (aggregated across backends)")
        .output(response_schema_of::<contracts::ModelListResponse>),
    // Config
    EndpointRoute::authenticated(HttpMethod::Get, "/b/llm/api/config", Route::GetConfig)
        .summary("Get default provider/model config")
        .output(response_schema_of::<contracts::LlmConfigResponse>),
    EndpointRoute::authenticated(HttpMethod::Post, "/b/llm/api/config", Route::PostConfig)
        .summary("Update per-thread provider/model override")
        .input(request_schema_of::<contracts::ConfigUpdateRequest>)
        .output(response_schema_of::<contracts::ConfigUpdateResponse>),
    EndpointRoute::authenticated(
        HttpMethod::Delete,
        "/b/llm/api/config/{id}",
        Route::DeleteConfig,
    )
    .summary("Remove a per-thread provider/model override")
    .path_params(override_id_path_schema)
    .output(response_schema_of::<contracts::ConfigDeleteResponse>),
];

/// LLM feature block. Owns the provider admin UI + chat thread persistence.
///
/// Chat requests go through `ctx.call_block("wafer-run/llm", ...)` — the
/// service block registered at app startup with a `MultiBackendLlmService`
/// router. The block never holds a concrete `LlmService`; it only drives the
/// [`ProviderAdmin`] seam (provider CRUD, discovery, and the
/// `lifecycle(Init)` configure step) against that same router's in-memory
/// provider set. Holding `Arc<dyn ProviderAdmin>` rather than the concrete,
/// `reqwest`/`tokio`-backed `ProviderLlmService` keeps the block buildable on
/// wasm32 (where a [`NoopProviderAdmin`](provider_admin::NoopProviderAdmin)
/// stands in and the browser configures providers in `BrowserLlmService`).
pub struct LlmBlock {
    /// Provider-admin handle for the in-memory router the chat dispatcher
    /// routes to. The provider CRUD endpoints reload it from the DB after
    /// each successful write so the next chat call sees the updated
    /// configuration.
    pub(crate) provider_admin: Arc<dyn ProviderAdmin>,
}

impl LlmBlock {
    pub fn new(provider_admin: Arc<dyn ProviderAdmin>) -> Self {
        Self { provider_admin }
    }
}

pub(crate) const SETTINGS_TABLE: &str = "impresspress__llm__settings";

pub(super) const DEFAULT_PROVIDER_VAR: &str = "IMPRESSPRESS__LLM__DEFAULT_PROVIDER";
pub(super) const DEFAULT_MODEL_VAR: &str = "IMPRESSPRESS__LLM__DEFAULT_MODEL";
pub(super) const DEFAULT_PROVIDER: &str = "impresspress/provider-llm";

// The previous in-process `default_target()` helper has moved to a
// `GET /b/llm/api/internal/default-target` route — see
// `handle_default_target` below. Other blocks (e.g. vector contextual
// retrieval) now fetch the target via `ctx.call_block("impresspress/llm", ...)`
// rather than importing this module directly. That keeps the cross-block
// dependency at the wire level (call_block) instead of the link level
// (Rust use-path), which is what unblocks per-block Cargo features in
// Phase 0b PR-2.

// ---------------------------------------------------------------------------
// Inter-block call helpers
// ---------------------------------------------------------------------------

/// Call the messages block to create an entry in a context.
pub(super) async fn messages_create(
    ctx: &dyn Context,
    original_msg: &Message,
    context_id: &str,
    role: &str,
    content: &str,
) -> Option<serde_json::Value> {
    // Serializing a plain `{kind, role, content}` map can only fail on a JSON
    // serializer bug. Surface it via tracing rather than sending an empty
    // body to the messages block, which would 400 with a confusing error.
    let body = match serde_json::to_vec(&serde_json::json!({
        "kind": "message",
        "role": role,
        "content": content,
    })) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("messages_create: failed to encode entry body: {e}");
            return None;
        }
    };

    let resource = format!("/b/messages/api/contexts/{context_id}/entries");
    let mut msg = crate::util::block_request("create", "POST", &resource, original_msg);
    msg.set_meta("req.content_type", "application/json");

    let out = ctx
        .call_block("impresspress/messages", msg, InputStream::from_bytes(body))
        .await;
    if let Ok(buf) = out.collect_buffered().await {
        return serde_json::from_slice::<serde_json::Value>(&buf.body).ok();
    }
    None
}

/// Call the messages block to list entries in a context.
pub(super) async fn messages_list(
    ctx: &dyn Context,
    original_msg: &Message,
    context_id: &str,
) -> Vec<serde_json::Value> {
    let resource = format!("/b/messages/api/contexts/{context_id}/entries?kind=message");
    let msg = crate::util::block_request("retrieve", "GET", &resource, original_msg);

    let out = ctx
        .call_block("impresspress/messages", msg, InputStream::empty())
        .await;
    if let Ok(buf) = out.collect_buffered().await {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf.body) {
            if let Some(records) = v.get("records").and_then(|r| r.as_array()) {
                return records.clone();
            }
        }
    }
    vec![]
}

// ---------------------------------------------------------------------------
// Handler implementations
// ---------------------------------------------------------------------------

impl LlmBlock {
    /// Resolve which provider block and model to use for a request.
    pub(super) async fn resolve_provider(
        &self,
        ctx: &dyn Context,
        thread_id: &str,
        req_provider: Option<&str>,
        req_model: Option<&str>,
    ) -> (String, String) {
        // Check per-thread override first
        let thread_setting = self.get_thread_setting(ctx, thread_id).await;

        let provider_block = thread_setting
            .as_ref()
            .and_then(|s| s.data.get("provider_block").and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| req_provider.map(|s| s.to_string()))
            .unwrap_or_else(|| {
                // Will be filled below from config
                String::new()
            });

        let model = thread_setting
            .as_ref()
            .and_then(|s| s.data.get("model").and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| req_model.map(|s| s.to_string()))
            .unwrap_or_default();

        let default_provider =
            config::get_default(ctx, DEFAULT_PROVIDER_VAR, DEFAULT_PROVIDER).await;
        let default_model = config::get_default(ctx, DEFAULT_MODEL_VAR, "").await;

        let final_provider = if provider_block.is_empty() {
            default_provider
        } else {
            provider_block
        };

        let final_model = if model.is_empty() {
            default_model
        } else {
            model
        };

        (final_provider, final_model)
    }

    /// Get the per-thread settings record from the DB, if any.
    ///
    /// Returns the whole [`db::Record`] (not just its `data`) so callers that
    /// need the record id for a follow-up update don't have to re-query.
    async fn get_thread_setting(&self, ctx: &dyn Context, thread_id: &str) -> Option<db::Record> {
        db::get_by_field(
            ctx,
            SETTINGS_TABLE,
            "thread_id",
            serde_json::Value::String(thread_id.to_string()),
        )
        .await
        .ok()
    }

    /// `DELETE /b/llm/api/config/{id}` — remove one per-thread override. The
    /// settings page renders a delete control for every override row; this
    /// is the route it targets.
    async fn handle_delete_config(&self, ctx: &dyn Context, msg: &Message) -> OutputStream {
        let id = msg.var("id").to_string();
        if id.is_empty() {
            return err_bad_request("Missing override ID");
        }
        match db::delete(ctx, SETTINGS_TABLE, &id).await {
            Ok(()) => ok_json(&contracts::ConfigDeleteResponse { deleted: true }),
            Err(e) if e.code == wafer_run::ErrorCode::NotFound => {
                err_not_found("Override not found")
            }
            Err(e) => err_internal("Database error", e),
        }
    }

    // --- Config ---

    /// Inter-block discovery: returns the default `(provider, model)` target
    /// other blocks should use when they have no caller-supplied preference.
    ///
    /// Wire format:
    /// * `200 {"provider": "...", "model": "..."}` when configured
    /// * `200 {"provider": null, "model": null}` when no model is configured
    ///   (callers should take a degraded path — same contract as the previous
    ///   in-process `default_target()` returning `None`).
    async fn handle_default_target(&self, ctx: &dyn Context) -> OutputStream {
        let provider = config::get_default(ctx, DEFAULT_PROVIDER_VAR, DEFAULT_PROVIDER).await;
        let model = config::get_default(ctx, DEFAULT_MODEL_VAR, "").await;
        if model.is_empty() || provider.is_empty() {
            return ok_json(&serde_json::json!({
                "provider": serde_json::Value::Null,
                "model": serde_json::Value::Null,
            }));
        }
        ok_json(&serde_json::json!({
            "provider": provider,
            "model": model,
        }))
    }

    async fn handle_get_config(&self, ctx: &dyn Context) -> OutputStream {
        let default_provider =
            config::get_default(ctx, DEFAULT_PROVIDER_VAR, DEFAULT_PROVIDER).await;
        let default_model = config::get_default(ctx, DEFAULT_MODEL_VAR, "").await;
        ok_json(&contracts::LlmConfigResponse {
            default_provider,
            default_model,
        })
    }

    /// `POST /b/llm/api/config`. Three outcomes, two of them successful:
    /// a body naming a global default is refused first (those come from
    /// the environment); otherwise a `thread_id` creates or updates that
    /// thread's override and returns the row as
    /// [`contracts::ThreadOverrideView`]; anything else is acknowledged
    /// without a write.
    async fn handle_post_config(&self, ctx: &dyn Context, input: InputStream) -> OutputStream {
        use contracts::{ConfigAcknowledgement, ConfigUpdateResponse, ThreadOverrideView};

        let raw = input.collect_to_bytes().await;
        let body: contracts::ConfigUpdateRequest = match serde_json::from_slice(&raw) {
            Ok(b) => b,
            Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
        };

        // The global defaults come from the environment, never from this
        // endpoint, and the published contract says sending one is refused.
        // Checked before the thread branch: a request carrying both a
        // `thread_id` and a default is refused whole, not stripped of the
        // default and written as an override.
        if body.default_provider.is_some() || body.default_model.is_some() {
            return err_bad_request(
                "Global default provider/model must be set via environment variables: IMPRESSPRESS__LLM__DEFAULT_PROVIDER and IMPRESSPRESS__LLM__DEFAULT_MODEL",
            );
        }

        // Per-thread override update
        if let Some(thread_id) = body.thread_id {
            let existing = self.get_thread_setting(ctx, &thread_id).await;

            if let Some(record) = existing {
                // Update the existing record in place — the single fetch above
                // already gave us both the id and the current data.
                let mut data = record.data;
                if let Some(pb) = body.provider_block {
                    data.insert("provider_block".to_string(), serde_json::json!(pb));
                }
                if let Some(m) = body.model {
                    data.insert("model".to_string(), serde_json::json!(m));
                }
                crate::util::stamp_updated(&mut data);
                match db::update(ctx, SETTINGS_TABLE, &record.id, data).await {
                    Ok(r) => {
                        return ok_json(&ConfigUpdateResponse::Override(
                            ThreadOverrideView::from_record(&r),
                        ))
                    }
                    Err(e) => return err_internal("Database error", e),
                }
            } else {
                // Create new per-thread setting
                let mut data = json_map(serde_json::json!({
                    "thread_id": thread_id,
                    "provider_block": body.provider_block.unwrap_or_default(),
                    "model": body.model.unwrap_or_default(),
                }));
                crate::util::stamp_created(&mut data);
                match db::create(ctx, SETTINGS_TABLE, data).await {
                    Ok(r) => {
                        return ok_json(&ConfigUpdateResponse::Override(
                            ThreadOverrideView::from_record(&r),
                        ))
                    }
                    Err(e) => return err_internal("Database error", e),
                }
            }
        }

        ok_json(&ConfigUpdateResponse::Acknowledged(ConfigAcknowledgement {
            updated: true,
        }))
    }

    // Models aggregation now lives in `routes::list_models`, sourcing data
    // from the `wafer-run/llm` service block via `ctx.call_block`. The
    // legacy `/b/provider-llm/api/models` proxy was removed in Task 16.
}

// ---------------------------------------------------------------------------
// Block trait implementation
// ---------------------------------------------------------------------------

/// Path parameters of `DELETE /b/llm/api/config/{id}`.
fn override_id_path_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["id"],
        "properties": {
            "id": {
                "type": "string",
                "description": "Override row id, as returned by `POST /b/llm/api/config`."
            }
        }
    })
}

/// Path-parameter schema for the `/b/llm/api/providers/{id}…` routes.
///
/// Hand-written rather than derived: every handler reads the id with
/// `msg.var("id")` by name, so a struct declared only to feed a derived
/// path-params schema would have no runtime user (the `tickets` /
/// `messages` precedent).
fn provider_id_path_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["id"],
        "properties": {
            "id": {
                "type": "string",
                "description": "Provider row id, as returned by `GET /b/llm/api/providers`."
            }
        }
    })
}

/// Path-parameter schema for the `/b/llm/api/models/{backend_id}/{model_id}…`
/// routes. Hand-written for the same reason as [`provider_id_path_schema`]:
/// `routes::models::extract_model_path` reads both by name.
fn model_path_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["backend_id", "model_id"],
        "properties": {
            "backend_id": {
                "type": "string",
                "description": "Backend (provider name) hosting the model, as listed by `GET /b/llm/api/models`."
            },
            "model_id": {
                "type": "string",
                "description": "Model id within that backend."
            }
        }
    })
}

#[wafer_block::wafer_async_trait]
impl Block for LlmBlock {
    fn info(&self) -> BlockInfo {
        BlockInfo::new(
            "impresspress/llm",
            "0.0.1",
            "http-handler@v1",
            "LLM orchestrator — routes to provider or local backends",
        )
        .instance_mode(InstanceMode::Singleton)
        .requires(vec![
            "impresspress/messages".into(),
            "wafer-run/llm".into(),
            "wafer-run/database".into(),
            "wafer-run/config".into(),
        ])
        // Tables (`impresspress__llm__settings`, `impresspress__llm__providers`)
        // are owned by `migrations/001_llm_schema.{sqlite,postgres}.sql` and
        // applied via `migrations::apply` in `lifecycle(Init)` below. No
        // `.collections(...)` declaration — schema is no longer materialised
        // implicitly via `ensure_table` on first insert.
        .category(wafer_run::BlockCategory::Feature)
        .description(
            "LLM orchestrator. Routes chat requests to provider-llm or local-llm backends, \
             manages thread history via the messages block, and provides the main chat UI.",
        )
        .endpoints(endpoint_match::declare(ROUTES))
        .config_keys(vec![
            ConfigVar::new(
                DEFAULT_PROVIDER_VAR,
                "Default LLM provider block (impresspress/provider-llm or impresspress/local-llm)",
                DEFAULT_PROVIDER,
            )
            .name("Default Provider"),
            ConfigVar::new(
                DEFAULT_MODEL_VAR,
                "Default model to use (empty = provider default)",
                "",
            )
            .name("Default Model")
            .optional(),
        ])
        .can_disable(true)
        .default_enabled(true)
    }

    async fn handle(
        &self,
        ctx: &dyn Context,
        mut msg: Message,
        input: InputStream,
    ) -> OutputStream {
        // Inter-block discovery endpoint: returns the configured default
        // `(provider, model)` target. Only accessible from another block (the
        // caller_id is set by `ctx.call_block`); never reachable from external
        // HTTP because the shared pipeline strips the caller id. It is NOT a
        // declared HTTP endpoint (declaring it would publish it), so it stays
        // a handler-owned guard ahead of the matcher; this is the one path
        // read in this block outside `endpoint_match::dispatch`.
        if msg.action() == "retrieve" && msg.path() == "/b/llm/api/internal/default-target" {
            if ctx.caller_id().is_none() {
                return crate::http::err_not_found("not found");
            }
            return self.handle_default_target(ctx).await;
        }

        // Auth is enforced centrally by `route_to_block` from the declared
        // endpoint `AuthLevel` (chat/config/models-list → Authenticated; UI
        // pages, provider CRUD, model load/unload → Admin). The block holds
        // no `user_id`/`is_admin` preamble and the provider/model handlers no
        // longer re-check `is_admin`. `{id}`/`{backend_id}`/`{model_id}` are
        // bound into `req.param.*` for the handlers' `msg.var` readers.
        let Some(route) = endpoint_match::dispatch(&mut msg, ROUTES) else {
            return err_not_found("not found");
        };
        match route {
            Route::ChatPage | Route::ThreadPage => pages::page(ctx, &msg).await,
            Route::SettingsPage => pages::settings_page(ctx, &msg).await,
            Route::ProvidersPage => ui::providers_page(self, ctx, &msg).await,
            Route::ModelsPage => ui::models_page(self, ctx, &msg).await,
            Route::Chat => routes::handle_chat(self, ctx, &msg, input).await,
            Route::ChatStream => routes::handle_chat_stream(self, ctx, &msg, input).await,
            Route::DiscoverModels => routes::discover_models(self, ctx, &msg).await,
            Route::ListProviders => routes::list_providers(self, ctx, &msg).await,
            Route::CreateProvider => routes::create_provider(self, ctx, &msg, input).await,
            Route::UpdateProvider => routes::update_provider(self, ctx, &msg, input).await,
            Route::DeleteProvider => routes::delete_provider(self, ctx, &msg).await,
            Route::ModelStatus => routes::model_status(self, ctx, &msg).await,
            Route::LoadModel => routes::load_model(self, ctx, &msg).await,
            Route::UnloadModel => routes::unload_model(self, ctx, &msg).await,
            Route::ListModels => routes::list_models(self, ctx, &msg).await,
            Route::GetConfig => self.handle_get_config(ctx).await,
            Route::PostConfig => self.handle_post_config(ctx, input).await,
            Route::DeleteConfig => self.handle_delete_config(ctx, &msg).await,
        }
    }

    async fn lifecycle(
        &self,
        ctx: &dyn Context,
        event: LifecycleEvent,
    ) -> std::result::Result<(), WaferError> {
        // Schema migrations first — must run before any row-level work below,
        // otherwise the provider reload would hit ensure_table fallback
        // paths instead of the indexed table. `lifecycle_init` no-ops on
        // non-Init events.
        crate::migration_helper::lifecycle_init(
            ctx,
            &event,
            "impresspress/llm",
            migrations::SQLITE_MIGRATIONS,
            migrations::POSTGRES_MIGRATIONS,
        )
        .await?;
        if matches!(event.event_type, LifecycleType::Init) {
            // Always load enabled providers into the in-memory service on
            // startup so chat dispatch finds them without waiting for an
            // admin CRUD write. Non-fatal if it fails — admins can trigger
            // a reload via any provider write.
            if let Err(e) = routes::reload_provider_service(ctx, self.provider_admin.as_ref()).await
            {
                tracing::warn!("initial provider reload failed: {e}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod table_tests {
    use std::sync::Arc;

    use super::*;

    /// `info().endpoints` is generated from `ROUTES`; nothing else declares
    /// an endpoint for this block.
    #[test]
    fn info_endpoints_come_from_the_table() {
        let block = LlmBlock::new(Arc::new(provider_admin::NoopProviderAdmin));
        let declared = block.info().endpoints;
        assert_eq!(declared.len(), ROUTES.len());
        for (ep, row) in declared.iter().zip(ROUTES) {
            assert_eq!(ep.method, row.method, "{}", row.template);
            assert_eq!(ep.path, row.template);
            assert_eq!(ep.auth, row.auth, "{}", row.template);
        }
    }
}

#[cfg(test)]
mod config_tests {
    use std::sync::Arc;

    use wafer_run::{streams::output::TerminalNotResponse, ErrorCode, InputStream};

    use super::*;
    use crate::test_support::{admin_msg, output_json, TestContext};

    fn block() -> LlmBlock {
        LlmBlock::new(Arc::new(provider_admin::NoopProviderAdmin))
    }

    /// The settings page renders `hx-delete="/b/llm/api/config/{id}"` for
    /// every per-thread override; that request must reach a route that
    /// removes the row, not the block's 404 fallback.
    #[tokio::test]
    async fn delete_config_removes_the_thread_override() {
        let ctx = TestContext::with_llm().await;
        let created = output_json(
            block()
                .handle_post_config(
                    &ctx,
                    body(serde_json::json!({
                        "thread_id": "t1",
                        "provider_block": "openai-main",
                        "model": "gpt-4o",
                    })),
                )
                .await,
        )
        .await;
        let id = created["id"].as_str().expect("row id").to_string();

        let out = block()
            .handle(
                &ctx,
                admin_msg("delete", &format!("/b/llm/api/config/{id}")),
                InputStream::from_bytes(Vec::new()),
            )
            .await;

        assert_eq!(
            output_json(out).await["deleted"],
            serde_json::json!(true),
            "the settings page's delete button must reach a route"
        );
        let rows = db::list_all(&ctx, SETTINGS_TABLE, vec![])
            .await
            .expect("list overrides");
        assert!(rows.is_empty(), "the override row must be gone");
    }

    fn body(value: serde_json::Value) -> InputStream {
        InputStream::from_bytes(serde_json::to_vec(&value).expect("serialize body"))
    }

    fn sorted_keys(value: &serde_json::Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap_or_else(|| panic!("expected an object, got {value}"))
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    const OVERRIDE_FIELDS: [&str; 6] = [
        "created_at",
        "id",
        "model",
        "provider_block",
        "thread_id",
        "updated_at",
    ];

    /// The override row is published as a flat view, not the database
    /// layer's `{id, data: {…}}` envelope the untyped handler echoed.
    #[tokio::test]
    async fn post_config_creates_a_flat_thread_override_view() {
        let ctx = TestContext::with_llm().await;

        let out = output_json(
            block()
                .handle_post_config(
                    &ctx,
                    body(serde_json::json!({
                        "thread_id": "t1",
                        "provider_block": "openai-main",
                        "model": "gpt-4o",
                    })),
                )
                .await,
        )
        .await;

        assert_eq!(
            sorted_keys(&out),
            OVERRIDE_FIELDS,
            "the wire field set must equal ThreadOverrideView's; a `data` key means \
             the raw row is being echoed again"
        );
        assert_eq!(out["thread_id"], "t1");
        assert_eq!(out["provider_block"], "openai-main");
        assert_eq!(out["model"], "gpt-4o");
        assert!(
            out["id"].as_str().is_some_and(|id| !id.is_empty()),
            "the row id must be published so the settings page can address it"
        );
        for field in ["created_at", "updated_at"] {
            let value = out[field]
                .as_str()
                .unwrap_or_else(|| panic!("{field} must be a string, got {}", out[field]));
            assert!(!value.is_empty(), "{field} must be set");
            chrono::DateTime::parse_from_rfc3339(value).unwrap_or_else(|e| {
                panic!("{field} must be RFC 3339 as the schema promises, got {value:?}: {e}")
            });
        }
    }

    #[tokio::test]
    async fn post_config_updates_the_existing_override_in_place() {
        let ctx = TestContext::with_llm().await;

        let first = output_json(
            block()
                .handle_post_config(
                    &ctx,
                    body(serde_json::json!({
                        "thread_id": "t1",
                        "provider_block": "openai-main",
                        "model": "gpt-4o",
                    })),
                )
                .await,
        )
        .await;
        let second = output_json(
            block()
                .handle_post_config(
                    &ctx,
                    body(serde_json::json!({ "thread_id": "t1", "model": "gpt-4o-mini" })),
                )
                .await,
        )
        .await;

        assert_eq!(sorted_keys(&second), OVERRIDE_FIELDS);
        assert_eq!(
            second["id"], first["id"],
            "a second write updates the same row"
        );
        assert_eq!(
            second["provider_block"], "openai-main",
            "fields absent from the request are retained"
        );
        assert_eq!(second["model"], "gpt-4o-mini");
    }

    /// Without `thread_id` there is nothing to write: the handler
    /// acknowledges and changes nothing. Pinned because the response schema
    /// publishes this branch alongside the override view.
    #[tokio::test]
    async fn post_config_without_a_thread_only_acknowledges() {
        let ctx = TestContext::with_llm().await;

        let out = output_json(
            block()
                .handle_post_config(&ctx, body(serde_json::json!({ "model": "gpt-4o" })))
                .await,
        )
        .await;

        assert_eq!(out, serde_json::json!({ "updated": true }));
        let rows = db::list_all(&ctx, SETTINGS_TABLE, vec![])
            .await
            .expect("list overrides");
        assert!(
            rows.is_empty(),
            "an acknowledgement must not have written an override"
        );
    }

    #[tokio::test]
    async fn post_config_refuses_global_defaults() {
        let ctx = TestContext::with_llm().await;

        let out = block()
            .handle_post_config(&ctx, body(serde_json::json!({ "default_model": "gpt-4o" })))
            .await;

        match out.collect_buffered().await {
            Err(TerminalNotResponse::Error(e)) => {
                assert_eq!(e.code, ErrorCode::InvalidArgument);
                assert!(
                    e.message.contains(DEFAULT_MODEL_VAR),
                    "the refusal must point at the variable to set instead, got: {}",
                    e.message
                );
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    /// The published description says sending a global default is refused.
    /// That must hold on the thread branch too: an override request that
    /// also carries a default is refused whole, not silently stripped of the
    /// default and written.
    #[tokio::test]
    async fn post_config_refuses_global_defaults_even_with_a_thread() {
        for value in [
            serde_json::json!({ "thread_id": "t1", "default_model": "gpt-4o" }),
            serde_json::json!({ "thread_id": "t1", "default_provider": "openai-main" }),
        ] {
            let ctx = TestContext::with_llm().await;

            let out = block().handle_post_config(&ctx, body(value.clone())).await;

            match out.collect_buffered().await {
                Err(TerminalNotResponse::Error(e)) => {
                    assert_eq!(e.code, ErrorCode::InvalidArgument, "{value}");
                    assert!(
                        e.message.contains(DEFAULT_PROVIDER_VAR)
                            && e.message.contains(DEFAULT_MODEL_VAR),
                        "{value}: the refusal must point at the variables to set instead, got: {}",
                        e.message
                    );
                }
                other => panic!("{value}: expected InvalidArgument, got {other:?}"),
            }
            let rows = db::list_all(&ctx, SETTINGS_TABLE, vec![])
                .await
                .expect("list overrides");
            assert!(
                rows.is_empty(),
                "{value}: a refused request must not have written an override"
            );
        }
    }

    #[tokio::test]
    async fn get_config_publishes_the_defaults() {
        let mut ctx = TestContext::with_llm().await;
        ctx.set_config(DEFAULT_PROVIDER_VAR, "openai-main");
        ctx.set_config(DEFAULT_MODEL_VAR, "gpt-4o");

        let out = output_json(block().handle_get_config(&ctx).await).await;

        assert_eq!(
            out,
            serde_json::json!({ "default_provider": "openai-main", "default_model": "gpt-4o" })
        );
    }
}
