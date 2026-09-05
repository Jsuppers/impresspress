//! REST endpoint handlers for the messages block.
//!
//! Thin layer: parse HTTP request → call service → format JSON response.
//! Every id-bearing handler reads `{id}` as the block's route table bound it
//! and verifies the caller owns the row through [`owned_record`] before
//! touching it; the pure-CRUD shells compose the id-taking `blocks::crud`
//! primitives on that verified row.

use wafer_core::clients::database::Record;
use wafer_run::{context::Context, ErrorCode, InputStream, Message, OutputStream};

use super::{
    contracts::{AddEntryRequest, CreateContextRequest, UpdateContextRequest},
    service::{self, ListContextsParams, ListEntriesParams},
};
use crate::{
    blocks::crud,
    http::{err_bad_request, err_internal, err_not_found, ok_json},
};

/// Convert empty string to None (msg.query() returns "" for missing params).
fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// The row `{id}` names in `table`, once the caller's ownership of it is
/// verified, or the 400 / 401 / 404 to send instead.
///
/// The id is read only as `endpoint_match::dispatch` bound it. A message
/// that never went through the table binds nothing and is refused here
/// rather than parsed out of the path. `label` is the resource name the
/// error texts use (`"Context"`, `"Entry"`).
async fn owned_record(
    ctx: &dyn Context,
    msg: &Message,
    table: &str,
    label: &str,
) -> Result<Record, OutputStream> {
    let id = msg.var("id");
    if id.is_empty() {
        return Err(err_bad_request(&format!(
            "Missing {} ID",
            label.to_lowercase()
        )));
    }
    crud::verify_owner(ctx, table, id, "owner_id", msg.user_id(), label).await
}

// ---------------------------------------------------------------------------
// Context endpoints
// ---------------------------------------------------------------------------

pub async fn list_contexts(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let (_, page_size, offset) = msg.pagination_params(20);
    let params = ListContextsParams {
        owner_id: Some(msg.user_id().to_string()), // owner scope
        context_type: non_empty(msg.query("type")),
        status: non_empty(msg.query("status")),
        sender_id: non_empty(msg.query("sender_id")),
        parent_id: non_empty(msg.query("parent_id")),
        page_size: page_size as i64,
        offset: offset as i64,
    };
    match service::list_contexts(ctx, &params).await {
        Ok(result) => ok_json(&result),
        Err(e) => err_internal("list_contexts failed", e),
    }
}

// create_context takes &Message to read the authenticated owner.
pub async fn create_context(ctx: &dyn Context, msg: &Message, input: InputStream) -> OutputStream {
    let raw = input.collect_to_bytes().await;
    let body: CreateContextRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };
    match service::create_context(
        ctx,
        msg.user_id(), // owner derived server-side, never from body
        &body.context_type,
        &body.title,
        &body.sender_id,
        &body.recipient_id,
        body.parent_id.as_deref(),
        body.metadata,
    )
    .await
    {
        Ok(record) => ok_json(&record),
        Err(e) => err_internal("create_context failed", e),
    }
}

pub async fn get_context(ctx: &dyn Context, msg: &Message) -> OutputStream {
    match owned_record(ctx, msg, service::CONTEXTS_TABLE, "Context").await {
        Ok(record) => ok_json(&record),
        Err(resp) => resp,
    }
}

pub async fn update_context(ctx: &dyn Context, msg: &Message, input: InputStream) -> OutputStream {
    let id = match owned_record(ctx, msg, service::CONTEXTS_TABLE, "Context").await {
        Ok(record) => record.id,
        Err(resp) => return resp,
    };
    let raw = input.collect_to_bytes().await;
    let body: UpdateContextRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };
    match service::update_context(ctx, &id, body.status, body.title, body.metadata).await {
        Ok(record) => ok_json(&record),
        Err(e) if e.code == ErrorCode::NotFound => err_not_found("Context not found"),
        Err(e) => err_internal("Database error", e),
    }
}

pub async fn delete_context(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let id = match owned_record(ctx, msg, service::CONTEXTS_TABLE, "Context").await {
        Ok(record) => record.id,
        Err(resp) => return resp,
    };
    match service::delete_context(ctx, &id).await {
        Ok(()) => ok_json(&serde_json::json!({"deleted": true})),
        Err(e) if e.code == ErrorCode::NotFound => err_not_found("Context not found"),
        Err(e) => err_internal("delete_context failed", e),
    }
}

// ---------------------------------------------------------------------------
// Entry endpoints
// ---------------------------------------------------------------------------

pub async fn list_entries(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let context_id = match owned_record(ctx, msg, service::CONTEXTS_TABLE, "Context").await {
        Ok(record) => record.id,
        Err(resp) => return resp,
    };
    let (_, page_size, offset) = msg.pagination_params(100);
    let params = ListEntriesParams {
        kind: non_empty(msg.query("kind")),
        role: non_empty(msg.query("role")),
        page_size: page_size as i64,
        offset: offset as i64,
    };
    match service::list_entries(ctx, &context_id, &params).await {
        Ok(result) => ok_json(&result),
        Err(e) => err_internal("list_entries failed", e),
    }
}

pub async fn add_entry(ctx: &dyn Context, msg: &Message, input: InputStream) -> OutputStream {
    let context_id = match owned_record(ctx, msg, service::CONTEXTS_TABLE, "Context").await {
        Ok(record) => record.id,
        Err(resp) => return resp,
    };
    let raw = input.collect_to_bytes().await;
    let body: AddEntryRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };
    match service::add_entry(
        ctx,
        msg.user_id(), // owner derived server-side, never from body
        &context_id,
        &body.kind,
        &body.role,
        &body.sender_id,
        &body.content,
        body.content_type.as_deref(),
        body.metadata,
    )
    .await
    {
        Ok(record) => ok_json(&record),
        Err(e) => err_internal("add_entry failed", e),
    }
}

pub async fn get_entry(ctx: &dyn Context, msg: &Message) -> OutputStream {
    match owned_record(ctx, msg, service::ENTRIES_TABLE, "Entry").await {
        Ok(record) => ok_json(&record),
        Err(resp) => resp,
    }
}

pub async fn delete_entry(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let id = match owned_record(ctx, msg, service::ENTRIES_TABLE, "Entry").await {
        Ok(record) => record.id,
        Err(resp) => return resp,
    };
    match crud::delete_record(ctx, service::ENTRIES_TABLE, &id, "Entry").await {
        Ok(deleted) => ok_json(&deleted),
        Err(resp) => resp,
    }
}

#[cfg(test)]
mod tests {
    // Messages has no shared `tests/harness.rs` module yet (unlike
    // `blocks::products::tests::harness`, which this mirrors) — the block's
    // handlers are exercised directly via `MessagesBlock::handle`, dispatched
    // the same way the central router would after auth (auth itself is
    // enforced centrally, not in `handle()` — see the comment in `mod.rs`).
    use wafer_block::http_codec;
    use wafer_run::{Block, TerminalNotResponse};

    use super::*;
    use crate::{blocks::messages::MessagesBlock, test_support::TestContext};

    /// Build a `TestContext` with admin + auth + messages migrations applied.
    /// No `TestContext::with_messages()` exists yet (only files/products/
    /// userportal/vector have one) — this applies the block's migrations the
    /// same way those constructors do: through the production-gated
    /// `migration_helper::apply_migrations` path, after `with_auth()` so the
    /// `impresspress__admin__block_settings` tracking table exists first.
    async fn messages_ctx() -> TestContext {
        let ctx = TestContext::with_auth().await;
        let sqlite: Vec<&str> = crate::blocks::messages::migrations::SQLITE_MIGRATIONS
            .iter()
            .map(|(_, sql)| *sql)
            .collect();
        crate::migration_helper::apply_migrations(
            &ctx,
            "impresspress/messages",
            &sqlite,
            crate::blocks::messages::migrations::POSTGRES_MIGRATIONS,
        )
        .await
        .expect("apply messages migrations in test fixture");
        ctx
    }

    /// Build a request `Message` + `InputStream`. Mirrors
    /// `blocks::products::tests::harness::request_msg`: `req.action`/
    /// `req.resource` meta drive `endpoint_match::dispatch`, `auth.user_id`
    /// meta is what `msg.user_id()` reads (verified against
    /// `wafer_block::meta::META_AUTH_USER_ID` and `test_support::auth_msg`).
    fn request(
        action: &str,
        path: &str,
        user_id: &str,
        body: serde_json::Value,
    ) -> (Message, InputStream) {
        let mut msg = Message::new("http.request");
        msg.set_meta("req.action", action);
        msg.set_meta("req.resource", path);
        if !user_id.is_empty() {
            msg.set_meta("auth.user_id", user_id);
        }
        let data = serde_json::to_vec(&body).expect("serialize body");
        (msg, InputStream::from_bytes(data))
    }

    /// Dispatch through the real block `handle()` — same in-block routing
    /// (`endpoint_match::dispatch` + the `Route` match) production uses.
    async fn dispatch(ctx: &TestContext, msg: Message, input: InputStream) -> OutputStream {
        MessagesBlock::new().handle(ctx, msg, input).await
    }

    /// Resolve an `OutputStream`'s HTTP status, including error terminals
    /// (`err_not_found`/`err_bad_request`/etc. return `OutputStream::error`,
    /// which `test_support::output_status` would panic on — this instead maps
    /// the `ErrorCode` to its canonical status via `wafer_block::http_codec`,
    /// the same mapping the real HTTP boundary uses).
    async fn status_of(out: OutputStream) -> u16 {
        match out.collect_buffered().await {
            Ok(buf) => http_codec::resolve_status(&buf.meta, 200),
            Err(TerminalNotResponse::Halt(buf)) => http_codec::resolve_status(&buf.meta, 200),
            Err(TerminalNotResponse::Error(e)) => http_codec::resolve_error_status(&e),
            Err(other) => panic!("unexpected terminal state: {other:?}"),
        }
    }

    fn listed_ids(listed: &serde_json::Value) -> Vec<String> {
        listed["records"]
            .as_array()
            .expect("records array")
            .iter()
            .map(|r| r["id"].as_str().expect("record id").to_string())
            .collect()
    }

    // --- Context request helpers ---

    async fn create_as(
        ctx: &TestContext,
        user_id: &str,
        body: serde_json::Value,
    ) -> serde_json::Value {
        let (msg, input) = request("create", "/b/messages/api/contexts", user_id, body);
        crate::test_support::output_json(dispatch(ctx, msg, input).await).await
    }

    async fn get_as(ctx: &TestContext, user_id: &str, id: &str) -> OutputStream {
        let (msg, input) = request(
            "retrieve",
            &format!("/b/messages/api/contexts/{id}"),
            user_id,
            serde_json::json!({}),
        );
        dispatch(ctx, msg, input).await
    }

    async fn list_as(ctx: &TestContext, user_id: &str) -> serde_json::Value {
        let (msg, input) = request(
            "retrieve",
            "/b/messages/api/contexts",
            user_id,
            serde_json::json!({}),
        );
        crate::test_support::output_json(dispatch(ctx, msg, input).await).await
    }

    async fn delete_as(ctx: &TestContext, user_id: &str, id: &str) -> OutputStream {
        let (msg, input) = request(
            "delete",
            &format!("/b/messages/api/contexts/{id}"),
            user_id,
            serde_json::json!({}),
        );
        dispatch(ctx, msg, input).await
    }

    async fn update_context_as(
        ctx: &TestContext,
        user_id: &str,
        id: &str,
        body: serde_json::Value,
    ) -> OutputStream {
        let (msg, input) = request(
            "update",
            &format!("/b/messages/api/contexts/{id}"),
            user_id,
            body,
        );
        dispatch(ctx, msg, input).await
    }

    // --- Entry request helpers ---

    async fn add_entry_as(
        ctx: &TestContext,
        user_id: &str,
        context_id: &str,
        body: serde_json::Value,
    ) -> OutputStream {
        let (msg, input) = request(
            "create",
            &format!("/b/messages/api/contexts/{context_id}/entries"),
            user_id,
            body,
        );
        dispatch(ctx, msg, input).await
    }

    async fn list_entries_as(ctx: &TestContext, user_id: &str, context_id: &str) -> OutputStream {
        let (msg, input) = request(
            "retrieve",
            &format!("/b/messages/api/contexts/{context_id}/entries"),
            user_id,
            serde_json::json!({}),
        );
        dispatch(ctx, msg, input).await
    }

    async fn get_entry_as(ctx: &TestContext, user_id: &str, id: &str) -> OutputStream {
        let (msg, input) = request(
            "retrieve",
            &format!("/b/messages/api/entries/{id}"),
            user_id,
            serde_json::json!({}),
        );
        dispatch(ctx, msg, input).await
    }

    async fn delete_entry_as(ctx: &TestContext, user_id: &str, id: &str) -> OutputStream {
        let (msg, input) = request(
            "delete",
            &format!("/b/messages/api/entries/{id}"),
            user_id,
            serde_json::json!({}),
        );
        dispatch(ctx, msg, input).await
    }

    // --- Tests ---

    /// Handlers read the id the table bound, nothing else: an unrouted
    /// message with an id in its path is refused, and the same message
    /// routed through `ROUTES` reaches the row.
    #[tokio::test]
    async fn handlers_read_only_the_bound_id() {
        use crate::{blocks::messages::test_support::routed, test_support::auth_msg};

        let ctx = messages_ctx().await;
        let created = create_as(&ctx, "user-a", serde_json::json!({"type": "task"})).await;
        let ctx_id = created["id"].as_str().expect("id").to_string();
        let path = format!("/b/messages/api/contexts/{ctx_id}");

        let unrouted = get_context(&ctx, &auth_msg("retrieve", &path, "user-a")).await;
        assert_eq!(
            status_of(unrouted).await,
            400,
            "an unrouted message binds no id and must be refused, not parsed"
        );

        let bound = get_context(&ctx, &routed(auth_msg("retrieve", &path, "user-a"))).await;
        assert_eq!(status_of(bound).await, 200);
    }

    #[tokio::test]
    async fn context_is_owner_scoped_across_users() {
        let ctx = messages_ctx().await;

        // User A creates a context; the body's sender_id is spoofed to
        // "user-b" — owner_id must come from the authenticated
        // msg.user_id(), never the body.
        let created = create_as(
            &ctx,
            "user-a",
            serde_json::json!({"type": "conversation", "sender_id": "user-b"}),
        )
        .await;
        let ctx_id = created["id"].as_str().expect("id").to_string();
        assert_eq!(
            created["data"]["owner_id"], "user-a",
            "owner_id must come from msg.user_id, not body"
        );
        assert_eq!(
            created["data"]["sender_id"], "user-b",
            "sender_id remains for A2A addressing only, unrelated to ownership"
        );

        // User B GET → 404 (existence must not leak).
        let got = get_as(&ctx, "user-b", &ctx_id).await;
        assert_eq!(status_of(got).await, 404);

        // User B list → does not include A's context.
        let listed = list_as(&ctx, "user-b").await;
        assert!(!listed_ids(&listed).contains(&ctx_id));

        // User B DELETE → 404 and the row still exists for A.
        assert_eq!(
            status_of(delete_as(&ctx, "user-b", &ctx_id).await).await,
            404
        );
        assert_eq!(status_of(get_as(&ctx, "user-a", &ctx_id).await).await, 200);
    }

    #[tokio::test]
    async fn update_context_is_owner_scoped() {
        let ctx = messages_ctx().await;

        let created = create_as(&ctx, "user-a", serde_json::json!({"type": "task"})).await;
        let ctx_id = created["id"].as_str().expect("id").to_string();

        let out = update_context_as(
            &ctx,
            "user-b",
            &ctx_id,
            serde_json::json!({"title": "hijacked"}),
        )
        .await;
        assert_eq!(status_of(out).await, 404);

        let got = crate::test_support::output_json(get_as(&ctx, "user-a", &ctx_id).await).await;
        assert_ne!(got["data"]["title"], "hijacked");
    }

    #[tokio::test]
    async fn entry_create_binds_owner_and_requires_parent_ownership() {
        let ctx = messages_ctx().await;

        let created = create_as(&ctx, "user-a", serde_json::json!({"type": "conversation"})).await;
        let ctx_id = created["id"].as_str().expect("id").to_string();

        // User B cannot add an entry to A's context, even knowing its id.
        let out = add_entry_as(
            &ctx,
            "user-b",
            &ctx_id,
            serde_json::json!({"content": "sneaky"}),
        )
        .await;
        assert_eq!(status_of(out).await, 404);

        // Nor list A's entries.
        let out = list_entries_as(&ctx, "user-b", &ctx_id).await;
        assert_eq!(status_of(out).await, 404);

        // User A adds an entry with a spoofed body sender_id — owner_id must
        // come from msg.user_id(), never the body.
        let entry = crate::test_support::output_json(
            add_entry_as(
                &ctx,
                "user-a",
                &ctx_id,
                serde_json::json!({"content": "hi", "sender_id": "user-b"}),
            )
            .await,
        )
        .await;
        let entry_id = entry["id"].as_str().expect("id").to_string();
        assert_eq!(entry["data"]["owner_id"], "user-a");
        assert_eq!(entry["data"]["sender_id"], "user-b");

        // User B cannot get or delete A's entry.
        assert_eq!(
            status_of(get_entry_as(&ctx, "user-b", &entry_id).await).await,
            404
        );
        assert_eq!(
            status_of(delete_entry_as(&ctx, "user-b", &entry_id).await).await,
            404
        );

        // The entry is still there for A.
        assert_eq!(
            status_of(get_entry_as(&ctx, "user-a", &entry_id).await).await,
            200
        );
    }

    #[tokio::test]
    async fn authenticated_api_accepts_assistant_role() {
        // Regression test for the LLM block's own assistant-message
        // persistence path: `blocks/llm/mod.rs::messages_create` builds
        // `{role: "assistant"}` and dispatches through this same
        // `rest::add_entry` handler via `ctx.call_block`. A prior commit
        // rejected `role in {assistant, system}` with 400, which silently
        // broke multi-turn chat by dropping every assistant reply. That
        // guard is gone; this locks in that assistant-role entries into a
        // context the caller owns succeed and round-trip the role.
        let ctx = messages_ctx().await;

        let created = create_as(&ctx, "user-a", serde_json::json!({"type": "conversation"})).await;
        let cid = created["id"].as_str().expect("id").to_string();

        for role in ["assistant", "system"] {
            let out = add_entry_as(
                &ctx,
                "user-a",
                &cid,
                serde_json::json!({"role": role, "content": "x"}),
            )
            .await;
            let entry = crate::test_support::output_json(out).await;
            assert_eq!(
                entry["data"]["role"], role,
                "role {role} must be accepted and round-trip"
            );
        }
    }
}
