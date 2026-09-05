//! Provider CRUD (admin-only).
//!
//! These endpoints back the LLM admin UI's provider management. All writes
//! reload the in-memory `ProviderLlmService` from the DB so chat requests
//! pick up the new configuration without restarting the process.

use wafer_core::clients::{config, database as db};
use wafer_run::{context::Context, InputStream, Message, OutputStream};

use crate::{
    blocks::llm::{
        contracts::{
            CreateProviderRequest, DiscoveredModelsResponse, ProviderDeleteResponse,
            ProviderListResponse, ProviderView, UpdateProviderRequest,
        },
        provider_admin::ProviderAdmin,
        providers::config::ProviderConfig,
        schema::{config_to_row, row_to_config, TABLE as PROVIDERS_TABLE},
        LlmBlock,
    },
    http::{err_bad_request, err_internal, err_not_found, ok_json},
};

/// Reload all enabled providers from the DB and push the snapshot into the
/// in-memory provider router via [`ProviderAdmin::configure`].
///
/// This is the single choke point where stored rows become live
/// `ProviderConfig`s: rows are decoded via [`row_to_config`] (which never
/// yields an `api_key`) and each config's `key_var` is resolved into
/// `api_key` here, via the config client, before `configure()`. Secret
/// rotation therefore takes effect on the next reload (boot or any provider
/// CRUD write), not per chat request.
///
/// Shared by the provider CRUD handlers, `LlmBlock::lifecycle(Init)`, and
/// the one-shot legacy-provider migration (which is why it takes the
/// provider-admin handle rather than the whole block).
///
/// Errors are returned to the caller; callers translate to 500. We do not
/// silently swallow — a failure here means the in-memory service is stale
/// and the admin needs to know.
pub(in crate::blocks::llm) async fn reload_provider_service(
    ctx: &dyn Context,
    provider_admin: &dyn ProviderAdmin,
) -> Result<(), String> {
    let records = db::list_all(ctx, PROVIDERS_TABLE, vec![])
        .await
        .map_err(|e| format!("provider reload list failed: {e}"))?;
    let mut configs: Vec<ProviderConfig> = Vec::with_capacity(records.len());
    for rec in &records {
        match row_to_config(rec) {
            Ok(mut cfg) if cfg.enabled => {
                resolve_provider_key(ctx, &mut cfg).await;
                configs.push(cfg);
            }
            Ok(_) => {} // disabled — skip
            Err(e) => {
                // A malformed row should not poison the whole reload —
                // drop just that one.
                tracing::warn!("skipping malformed provider row {}: {e}", rec.id);
            }
        }
    }
    provider_admin.configure(configs);
    Ok(())
}

/// Resolve a provider's `key_var` into its plaintext `api_key` via the
/// config client. `key_var` takes precedence over any inline `api_key`;
/// with no `key_var` the config is left untouched.
///
/// Resolution failure (unset var, empty value, denied read) is logged and
/// leaves `api_key` as-is — the provider then runs unauthenticated, and the
/// per-protocol encoder decides whether that's an error (`MissingApiKey` →
/// 401) on the next chat call. Local OpenAI-compatible servers legitimately
/// run without a key.
async fn resolve_provider_key(ctx: &dyn Context, cfg: &mut ProviderConfig) {
    let Some(var) = cfg.key_var.as_deref() else {
        return;
    };
    match config::get(ctx, var).await {
        Ok(value) if !value.is_empty() => cfg.api_key = Some(value),
        Ok(_) => tracing::warn!(
            "provider '{}': key_var `{var}` is set but empty — provider will run unauthenticated",
            cfg.name
        ),
        Err(e) => tracing::warn!(
            "provider '{}': failed to resolve key_var `{var}`: {e} — provider will run unauthenticated",
            cfg.name
        ),
    }
}

/// `GET /b/llm/api/providers` — list all rows. Admin-only.
pub(in crate::blocks::llm) async fn list_providers(
    _block: &LlmBlock,
    ctx: &dyn Context,
    _msg: &Message,
) -> OutputStream {
    let records = match db::list_all(ctx, PROVIDERS_TABLE, vec![]).await {
        Ok(r) => r,
        Err(e) => return err_internal("Database error", e),
    };
    let providers: Vec<ProviderView> = records
        .iter()
        .filter_map(|rec| {
            row_to_config(rec)
                .ok()
                .map(|cfg| ProviderView::from_config(&rec.id, &cfg))
        })
        .collect();
    ok_json(&ProviderListResponse { providers })
}

/// `POST /b/llm/api/providers` — create. The typed body requires `name`,
/// `protocol` (one of the `ProviderProtocol` tokens) and `endpoint`;
/// `key_var`, `models`, `enabled` are optional. Admin-only.
pub(in crate::blocks::llm) async fn create_provider(
    block: &LlmBlock,
    ctx: &dyn Context,
    _msg: &Message,
    input: InputStream,
) -> OutputStream {
    let raw = input.collect_to_bytes().await;
    let body: CreateProviderRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    // Presence is enforced by the type; emptiness still has to be, because
    // `""` is a valid JSON string and neither a usable name nor a URL.
    if body.name.is_empty() {
        return err_bad_request("`name` is required");
    }
    if body.endpoint.is_empty() {
        return err_bad_request("`endpoint` is required");
    }
    // SSRF: an admin must not be able to point a provider at internal infra.
    // Same gate the config `_URL` write surfaces use; the outbound client
    // re-checks at call time (resolve-before-connect), this fails fast on save.
    if let Err(e) = crate::util::validate_url_value(&body.endpoint) {
        return err_bad_request(&format!("invalid `endpoint`: {e}"));
    }

    let mut cfg = ProviderConfig::new(body.name, body.protocol, body.endpoint);
    if let Some(k) = body.key_var.filter(|s| !s.is_empty()) {
        cfg.key_var = Some(k);
    }
    if let Some(m) = body.models {
        cfg.models = m;
    }
    if let Some(e) = body.enabled {
        cfg.enabled = e;
    }

    let mut data = config_to_row(&cfg);
    crate::util::stamp_created(&mut data);

    let record = match db::create(ctx, PROVIDERS_TABLE, data).await {
        Ok(r) => r,
        Err(e) => return err_internal("Database error", e),
    };

    if let Err(e) = reload_provider_service(ctx, block.provider_admin.as_ref()).await {
        return err_internal("reload_provider_service failed", e);
    }

    ok_json(&ProviderView::from_config(&record.id, &cfg))
}

/// `PATCH /b/llm/api/providers/:id` — partial update. Admin-only.
pub(in crate::blocks::llm) async fn update_provider(
    block: &LlmBlock,
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let id = msg.var("id").to_string();
    if id.is_empty() {
        return err_bad_request("Missing provider ID");
    }

    let raw = input.collect_to_bytes().await;
    let body: UpdateProviderRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    // Load existing record so we can apply the patch on top of stored values.
    let existing = match db::get(ctx, PROVIDERS_TABLE, &id).await {
        Ok(r) => r,
        Err(e) if e.code == wafer_run::ErrorCode::NotFound => {
            return err_not_found("Provider not found")
        }
        Err(e) => return err_internal("Database error", e),
    };
    let mut cfg = match row_to_config(&existing) {
        Ok(c) => c,
        Err(e) => return err_internal("Stored provider row invalid", e),
    };

    if let Some(n) = body.name.filter(|s| !s.is_empty()) {
        cfg.name = n;
    }
    if let Some(p) = body.protocol {
        cfg.protocol = p;
    }
    if let Some(e) = body.endpoint.filter(|s| !s.is_empty()) {
        // SSRF: re-validate on edit (see create_provider) so an update can't
        // smuggle in an internal endpoint that create rejected.
        if let Err(err) = crate::util::validate_url_value(&e) {
            return err_bad_request(&format!("invalid `endpoint`: {err}"));
        }
        cfg.endpoint = e;
    }
    if let Some(k) = body.key_var {
        cfg.key_var = if k.is_empty() { None } else { Some(k) };
    }
    if let Some(m) = body.models {
        cfg.models = m;
    }
    if let Some(e) = body.enabled {
        cfg.enabled = e;
    }

    let mut data = config_to_row(&cfg);
    crate::util::stamp_updated(&mut data);

    let record = match db::update(ctx, PROVIDERS_TABLE, &id, data).await {
        Ok(r) => r,
        Err(e) if e.code == wafer_run::ErrorCode::NotFound => {
            return err_not_found("Provider not found")
        }
        Err(e) => return err_internal("Database error", e),
    };

    if let Err(e) = reload_provider_service(ctx, block.provider_admin.as_ref()).await {
        return err_internal("reload_provider_service failed", e);
    }

    ok_json(&ProviderView::from_config(&record.id, &cfg))
}

/// `DELETE /b/llm/api/providers/:id` — remove. Admin-only.
pub(in crate::blocks::llm) async fn delete_provider(
    block: &LlmBlock,
    ctx: &dyn Context,
    msg: &Message,
) -> OutputStream {
    let id = msg.var("id").to_string();
    if id.is_empty() {
        return err_bad_request("Missing provider ID");
    }
    match db::delete(ctx, PROVIDERS_TABLE, &id).await {
        Ok(()) => {}
        Err(e) if e.code == wafer_run::ErrorCode::NotFound => {
            return err_not_found("Provider not found")
        }
        Err(e) => return err_internal("Database error", e),
    }

    if let Err(e) = reload_provider_service(ctx, block.provider_admin.as_ref()).await {
        return err_internal("reload_provider_service failed", e);
    }

    ok_json(&ProviderDeleteResponse { deleted: true })
}

/// `POST /b/llm/api/providers/:id/discover-models` — call the provider's
/// `/v1/models` endpoint, persist the discovered list back to the row, and
/// return the new model list. Admin-only.
pub(in crate::blocks::llm) async fn discover_models(
    block: &LlmBlock,
    ctx: &dyn Context,
    msg: &Message,
) -> OutputStream {
    let id = msg.var("id").to_string();
    if id.is_empty() {
        return err_bad_request("Missing provider ID");
    }

    // Resolve the provider name from the row — discover_models is keyed by
    // provider name (== ProviderConfig::name), not by row id.
    let existing = match db::get(ctx, PROVIDERS_TABLE, &id).await {
        Ok(r) => r,
        Err(e) if e.code == wafer_run::ErrorCode::NotFound => {
            return err_not_found("Provider not found")
        }
        Err(e) => return err_internal("Database error", e),
    };
    let mut cfg = match row_to_config(&existing) {
        Ok(c) => c,
        Err(e) => return err_internal("Stored provider row invalid", e),
    };

    // Make sure the in-memory service knows about this provider — discover
    // looks up by name, and the service may be empty if the process just
    // started or the row is disabled (and so was excluded from the last
    // configure call).
    if let Err(e) = reload_provider_service(ctx, block.provider_admin.as_ref()).await {
        return err_internal("reload_provider_service failed", e);
    }

    let models = match block.provider_admin.discover_models(&cfg.name).await {
        Ok(m) => m,
        Err(e) => return err_internal("discover_models failed", format!("{e:?}")),
    };
    cfg.models = models.into_iter().map(|m| m.model_id).collect();

    let mut data = config_to_row(&cfg);
    crate::util::stamp_updated(&mut data);
    if let Err(e) = db::update(ctx, PROVIDERS_TABLE, &id, data).await {
        return err_internal("Database error", e);
    }

    if let Err(e) = reload_provider_service(ctx, block.provider_admin.as_ref()).await {
        return err_internal("reload_provider_service failed", e);
    }

    ok_json(&DiscoveredModelsResponse { models: cfg.models })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use wafer_run::{streams::output::TerminalNotResponse, ErrorCode};

    use super::*;
    use crate::{
        blocks::llm::{
            providers::config::ProviderProtocol,
            routes::test_support::{
                admin_msg, routed, stub_block, PanicCtx, RecordingProviderAdmin,
            },
        },
        test_support::{output_json, TestContext},
    };

    #[tokio::test]
    async fn create_provider_returns_bad_request_on_invalid_json() {
        let block = stub_block();
        let ctx = PanicCtx;
        let msg = admin_msg("create", "/b/llm/api/providers");
        let input = InputStream::from_bytes(b"not json".to_vec());

        let out = create_provider(&block, &ctx, &msg, input).await;
        match out.collect_buffered().await {
            Err(TerminalNotResponse::Error(e)) => {
                assert_eq!(e.code, ErrorCode::InvalidArgument);
                assert!(
                    e.message.contains("Invalid body"),
                    "expected Invalid body, got: {}",
                    e.message
                );
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_provider_requires_name() {
        let block = stub_block();
        let ctx = PanicCtx;
        let msg = admin_msg("create", "/b/llm/api/providers");
        let input =
            InputStream::from_bytes(br#"{"protocol":"open_ai","endpoint":"https://x"}"#.to_vec());

        let out = create_provider(&block, &ctx, &msg, input).await;
        match out.collect_buffered().await {
            Err(TerminalNotResponse::Error(e)) => {
                assert_eq!(e.code, ErrorCode::InvalidArgument);
                assert!(e.message.contains("name"), "got: {}", e.message);
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    /// `protocol` is typed as the `ProviderProtocol` enum on the way in, so an
    /// alias is refused by deserialization and the refusal names the accepted
    /// values — which is what a caller who sent `openai` needs to see.
    #[tokio::test]
    async fn create_provider_rejects_unknown_protocol() {
        let block = stub_block();
        let ctx = PanicCtx;
        let msg = admin_msg("create", "/b/llm/api/providers");
        let input = InputStream::from_bytes(
            br#"{"name":"x","protocol":"openai","endpoint":"https://x"}"#.to_vec(),
        );

        let out = create_provider(&block, &ctx, &msg, input).await;
        match out.collect_buffered().await {
            Err(TerminalNotResponse::Error(e)) => {
                assert_eq!(e.code, ErrorCode::InvalidArgument);
                assert!(
                    e.message.contains("open_ai_compatible"),
                    "the refusal must name the accepted values, got: {}",
                    e.message
                );
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    /// Same enum on the patch body: `""` is outside it and is refused before
    /// any row is read. The untyped body used to treat an empty `protocol` as
    /// "not provided" and silently apply the rest of the patch.
    #[tokio::test]
    async fn update_provider_rejects_an_empty_protocol() {
        let block = stub_block();
        let ctx = PanicCtx;
        let msg = routed(admin_msg("update", "/b/llm/api/providers/row-1"));
        let input = InputStream::from_bytes(br#"{"protocol":""}"#.to_vec());

        let out = update_provider(&block, &ctx, &msg, input).await;
        match out.collect_buffered().await {
            Err(TerminalNotResponse::Error(e)) => {
                assert_eq!(e.code, ErrorCode::InvalidArgument);
                assert!(
                    e.message.contains("open_ai_compatible"),
                    "the refusal must name the accepted values, got: {}",
                    e.message
                );
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    /// SSRF: an admin must not be able to point a provider endpoint at
    /// internal infrastructure. The rejection happens before any DB write, so
    /// `PanicCtx` (which panics if the store is touched) staying quiet also
    /// proves the check fails fast, ahead of `db::create`.
    #[tokio::test]
    async fn create_provider_rejects_internal_endpoint() {
        for endpoint in [
            "https://169.254.169.254/latest/meta-data/", // cloud metadata (link-local)
            "http://10.0.0.1/v1",                        // RFC1918 private
            "https://100.64.0.1/v1",                     // CGNAT
            "https://metadata.google.internal/v1",       // metadata hostname
        ] {
            let block = stub_block();
            let ctx = PanicCtx;
            let msg = admin_msg("create", "/b/llm/api/providers");
            let body = format!(r#"{{"name":"x","protocol":"open_ai","endpoint":"{endpoint}"}}"#);
            let input = InputStream::from_bytes(body.into_bytes());

            let out = create_provider(&block, &ctx, &msg, input).await;
            match out.collect_buffered().await {
                Err(TerminalNotResponse::Error(e)) => {
                    assert_eq!(e.code, ErrorCode::InvalidArgument, "endpoint {endpoint}");
                    assert!(
                        e.message.contains("endpoint"),
                        "endpoint {endpoint}: got: {}",
                        e.message
                    );
                }
                other => panic!("expected InvalidArgument for {endpoint}, got {other:?}"),
            }
        }
    }

    /// The API key is referenced by variable name (`key_var`), never sent
    /// inline. A body carrying `api_key` must be refused by name, not
    /// silently dropped after the secret transited the request — the admin
    /// would otherwise get a 200 and a provider that runs unauthenticated.
    #[tokio::test]
    async fn create_provider_refuses_an_inline_api_key() {
        let block = stub_block();
        let ctx = PanicCtx;
        let msg = admin_msg("create", "/b/llm/api/providers");
        let input = InputStream::from_bytes(
            br#"{"name":"x","protocol":"open_ai","endpoint":"https://api.openai.com/v1","api_key":"sk-inline"}"#
                .to_vec(),
        );

        let out = create_provider(&block, &ctx, &msg, input).await;
        match out.collect_buffered().await {
            Err(TerminalNotResponse::Error(e)) => {
                assert_eq!(e.code, ErrorCode::InvalidArgument);
                assert!(
                    e.message.contains("api_key"),
                    "the refusal must name the unknown field, got: {}",
                    e.message
                );
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    /// Same on the patch body.
    #[tokio::test]
    async fn update_provider_refuses_an_inline_api_key() {
        let block = stub_block();
        let ctx = PanicCtx;
        let msg = routed(admin_msg("update", "/b/llm/api/providers/row-1"));
        let input = InputStream::from_bytes(br#"{"api_key":"sk-inline"}"#.to_vec());

        let out = update_provider(&block, &ctx, &msg, input).await;
        match out.collect_buffered().await {
            Err(TerminalNotResponse::Error(e)) => {
                assert_eq!(e.code, ErrorCode::InvalidArgument);
                assert!(
                    e.message.contains("api_key"),
                    "the refusal must name the unknown field, got: {}",
                    e.message
                );
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_provider_requires_id() {
        let block = stub_block();
        let ctx = PanicCtx;
        // Path has no id segment after the prefix.
        let msg = admin_msg("update", "/b/llm/api/providers/");
        let input = InputStream::from_bytes(b"{}".to_vec());

        let out = update_provider(&block, &ctx, &msg, input).await;
        match out.collect_buffered().await {
            Err(TerminalNotResponse::Error(e)) => {
                assert_eq!(e.code, ErrorCode::InvalidArgument);
                assert!(e.message.contains("provider ID"), "got: {}", e.message);
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_provider_requires_id() {
        let block = stub_block();
        let ctx = PanicCtx;
        let msg = admin_msg("delete", "/b/llm/api/providers/");

        let out = delete_provider(&block, &ctx, &msg).await;
        match out.collect_buffered().await {
            Err(TerminalNotResponse::Error(e)) => {
                assert_eq!(e.code, ErrorCode::InvalidArgument);
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    /// Provider handlers read the id the table bound, nothing else.
    #[test]
    fn provider_id_is_bound_by_the_table() {
        let m = routed(admin_msg("update", "/b/llm/api/providers/abc123"));
        assert_eq!(m.var("id"), "abc123");

        let m2 = routed(admin_msg(
            "create",
            "/b/llm/api/providers/abc123/discover-models",
        ));
        assert_eq!(m2.var("id"), "abc123");

        // A path with no id segment matches no row and binds nothing; the
        // handler then answers InvalidArgument (see `update_provider_requires_id`).
        let mut m3 = admin_msg("delete", "/b/llm/api/providers/");
        assert!(crate::endpoint_match::dispatch(&mut m3, crate::blocks::llm::ROUTES).is_none());
        assert_eq!(m3.var("id"), "");
    }

    // -----------------------------------------------------------------
    // Wire shape — what the published schema is derived from
    // -----------------------------------------------------------------

    const KEY_VAR: &str = "IMPRESSPRESS__LLM__TEST_OPENAI_KEY";

    /// Shaped like a real key so a substring search over a response body is
    /// a meaningful leak check rather than a match on a common word.
    const SECRET: &str = "sk-live-0123456789abcdefABCDEF";

    /// A block over the recording provider-admin handle, on a context where
    /// `KEY_VAR` resolves to `SECRET`. The handle is returned separately so
    /// a test can read back what the reload resolved into it.
    async fn keyed_fixture() -> (TestContext, Arc<RecordingProviderAdmin>, LlmBlock) {
        let mut ctx = TestContext::with_llm().await;
        ctx.set_config(KEY_VAR, SECRET);
        let admin = Arc::new(RecordingProviderAdmin::default());
        let block = LlmBlock::new(admin.clone());
        (ctx, admin, block)
    }

    fn json_input(value: serde_json::Value) -> InputStream {
        InputStream::from_bytes(serde_json::to_vec(&value).expect("serialize body"))
    }

    fn create_body() -> InputStream {
        json_input(serde_json::json!({
            "name": "openai-main",
            "protocol": "open_ai",
            "endpoint": "https://api.openai.com/v1",
            "key_var": KEY_VAR,
            "models": ["gpt-4o"],
        }))
    }

    /// The field set a provider row publishes, on every endpoint that returns
    /// one. This is the assertion the `/openapi.json` schema rests on.
    fn assert_provider_view(label: &str, row: &serde_json::Value) {
        let mut got: Vec<&str> = row
            .as_object()
            .unwrap_or_else(|| panic!("{label}: expected an object, got {row}"))
            .keys()
            .map(String::as_str)
            .collect();
        got.sort_unstable();
        assert_eq!(
            got,
            ["enabled", "endpoint", "id", "key_var", "models", "name", "protocol"],
            "{label}: the wire field set must equal ProviderView's, or the published \
             schema describes something the handler does not emit"
        );
    }

    /// The resolved key sits on the very `ProviderConfig`s the handlers hold
    /// (`block.provider_admin`), one field away from every response. No
    /// endpoint may carry it, or an `api_key` field of any kind.
    #[tokio::test]
    async fn provider_endpoints_never_emit_the_resolved_api_key() {
        let (ctx, admin, block) = keyed_fixture().await;

        let created = output_json(
            create_provider(
                &block,
                &ctx,
                &admin_msg("create", "/b/llm/api/providers"),
                create_body(),
            )
            .await,
        )
        .await;
        let id = created["id"].as_str().expect("created id").to_string();

        // Control: after the create's reload the secret is on the handle the
        // handlers read. Without this the assertions below could pass because
        // nothing was ever within reach.
        assert_eq!(
            admin
                .providers_snapshot()
                .first()
                .and_then(|p| p.api_key.as_deref()),
            Some(SECRET),
            "fixture must resolve the key into the handlers' provider handle"
        );

        let listed = output_json(
            list_providers(&block, &ctx, &admin_msg("retrieve", "/b/llm/api/providers")).await,
        )
        .await;
        let updated = output_json(
            update_provider(
                &block,
                &ctx,
                &routed(admin_msg("update", &format!("/b/llm/api/providers/{id}"))),
                json_input(serde_json::json!({ "models": ["gpt-4o-mini"] })),
            )
            .await,
        )
        .await;
        let discovered = output_json(
            discover_models(
                &block,
                &ctx,
                &routed(admin_msg(
                    "create",
                    &format!("/b/llm/api/providers/{id}/discover-models"),
                )),
            )
            .await,
        )
        .await;

        for (label, body) in [
            ("create", &created),
            ("list", &listed),
            ("update", &updated),
            ("discover-models", &discovered),
        ] {
            let raw = body.to_string();
            assert!(
                !raw.contains(SECRET),
                "{label} leaked the resolved key: {raw}"
            );
            assert!(
                !raw.to_lowercase().contains("api_key"),
                "{label} published an api_key field: {raw}"
            );
        }
    }

    /// Every provider endpoint publishes the one row projection, and the
    /// acknowledgement shapes are exactly what their schemas say.
    #[tokio::test]
    async fn provider_endpoints_publish_exactly_the_view_fields() {
        let (ctx, _admin, block) = keyed_fixture().await;

        let created = output_json(
            create_provider(
                &block,
                &ctx,
                &admin_msg("create", "/b/llm/api/providers"),
                create_body(),
            )
            .await,
        )
        .await;
        assert_provider_view("create", &created);
        assert_eq!(created["name"], "openai-main");
        assert_eq!(created["protocol"], "open_ai");
        assert_eq!(created["endpoint"], "https://api.openai.com/v1");
        assert_eq!(created["key_var"], KEY_VAR);
        assert_eq!(created["models"], serde_json::json!(["gpt-4o"]));
        assert_eq!(created["enabled"], true);
        let id = created["id"].as_str().expect("created id").to_string();

        let listed = output_json(
            list_providers(&block, &ctx, &admin_msg("retrieve", "/b/llm/api/providers")).await,
        )
        .await;
        let rows = listed["providers"].as_array().expect("providers array");
        assert_eq!(rows.len(), 1);
        assert_provider_view("list", &rows[0]);
        assert_eq!(
            rows[0], created,
            "list must publish the row the create returned"
        );

        let updated = output_json(
            update_provider(
                &block,
                &ctx,
                &routed(admin_msg("update", &format!("/b/llm/api/providers/{id}"))),
                json_input(serde_json::json!({
                    "models": ["gpt-4o-mini"],
                    "enabled": false,
                })),
            )
            .await,
        )
        .await;
        assert_provider_view("update", &updated);
        assert_eq!(updated["id"], id);
        assert_eq!(updated["models"], serde_json::json!(["gpt-4o-mini"]));
        assert_eq!(updated["enabled"], false);
        assert_eq!(
            updated["key_var"], KEY_VAR,
            "fields absent from the patch are retained"
        );

        let discovered = output_json(
            discover_models(
                &block,
                &ctx,
                &routed(admin_msg(
                    "create",
                    &format!("/b/llm/api/providers/{id}/discover-models"),
                )),
            )
            .await,
        )
        .await;
        assert_eq!(
            discovered,
            serde_json::json!({ "models": ["gpt-4o", "gpt-4o-mini"] })
        );

        let deleted = output_json(
            delete_provider(
                &block,
                &ctx,
                &routed(admin_msg("delete", &format!("/b/llm/api/providers/{id}"))),
            )
            .await,
        )
        .await;
        assert_eq!(deleted, serde_json::json!({ "deleted": true }));
    }

    // -----------------------------------------------------------------
    // reload_provider_service — key_var resolution
    // -----------------------------------------------------------------

    /// End-to-end reload over a real in-memory DB + config block:
    /// a row whose `key_var` resolves gets its `api_key` populated, a row
    /// without `key_var` stays unauthenticated, and an unresolvable
    /// `key_var` degrades to no key (warn) instead of failing the reload.
    #[tokio::test]
    async fn reload_provider_service_resolves_key_var_into_api_key() {
        use wafer_core::{
            interfaces::config::service::ConfigService,
            service_blocks::config::{ConfigBlock, EnvConfigService},
        };

        let mut ctx = TestContext::with_admin().await;
        {
            use crate::blocks::llm::migrations;
            let sqlite: Vec<&str> = migrations::SQLITE_MIGRATIONS
                .iter()
                .map(|(_, sql)| *sql)
                .collect();
            crate::migration_helper::apply_migrations(
                &ctx,
                "impresspress/llm",
                &sqlite,
                migrations::POSTGRES_MIGRATIONS,
            )
            .await
            .expect("apply llm migrations");
        }

        let config_svc = Arc::new(EnvConfigService::new());
        config_svc.set("IMPRESSPRESS__LLM__OPENAI_KEY", "sk-resolved");
        ctx.register_block("wafer-run/config", Arc::new(ConfigBlock::new(config_svc)));

        for cfg in [
            ProviderConfig::new(
                "with-key-var",
                ProviderProtocol::OpenAi,
                "https://api.openai.com/v1",
            )
            .with_key_var("IMPRESSPRESS__LLM__OPENAI_KEY"),
            ProviderConfig::new(
                "no-key-var",
                ProviderProtocol::OpenAiCompatible,
                "http://localhost:11434/v1",
            ),
            ProviderConfig::new(
                "unresolvable-key-var",
                ProviderProtocol::OpenAi,
                "https://api.openai.com/v1",
            )
            .with_key_var("IMPRESSPRESS__LLM__TEST_MISSING_KEY"),
        ] {
            let mut data = config_to_row(&cfg);
            crate::util::stamp_created(&mut data);
            db::create(&ctx, PROVIDERS_TABLE, data)
                .await
                .expect("create provider row");
        }

        let svc = crate::blocks::llm::providers::ProviderLlmService::try_new()
            .expect("build provider service");
        reload_provider_service(&ctx, &svc)
            .await
            .expect("reload succeeds");

        let by_name = |name: &str| {
            svc.providers_snapshot()
                .into_iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("provider '{name}' missing from snapshot"))
        };
        assert_eq!(
            by_name("with-key-var").api_key.as_deref(),
            Some("sk-resolved"),
            "key_var must resolve into api_key at reload"
        );
        assert_eq!(by_name("no-key-var").api_key, None);
        assert_eq!(
            by_name("unresolvable-key-var").api_key,
            None,
            "unresolvable key_var degrades to no key, not a reload failure"
        );
    }
}
