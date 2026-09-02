//! HTTP route dispatcher for impresspress/vector.
//!
//! Implemented routes:
//!   - `POST   /b/vector/api/indexes`           → create an index
//!   - `GET    /b/vector/api/indexes`           → list indexes for this project
//!   - `DELETE /b/vector/api/indexes/{name}`    → delete an index
//!   - `POST   /b/vector/api/upsert`            → upsert pre-computed vectors
//!   - `POST   /b/vector/api/query`             → search vectors (vector/keyword/hybrid)
//!   - `DELETE /b/vector/api/{index}/{id}`      → delete a single vector
//!   - `GET    /b/vector/api/stats`             → per-index counts
//!
//! User-facing index names are prefixed with `impresspress__vector__` before
//! being passed to the `wafer-run/vector` runtime block. The prefix is
//! stripped on the way out in list/stats responses.
//!
//! Task 19 implements ingest and embed:
//!   - `POST   /b/vector/api/ingest`            → chunk + embed + upsert
//!   - `POST   /b/vector/api/embed`             → raw text → vectors
//!
//! ### Registry table
//!
//! Per-index metadata (model, dimensions, keyword_search flag) is kept in
//! `impresspress__vector__registry`. This lets the query route look up the
//! correct embedding model when the caller sends text instead of a raw
//! vector. Rows are written on `create_index` and removed on `delete_index`.
//! Pre-registry indexes (created before this table existed) fall back to
//! `DEFAULT_MODEL` + the typed `vector.describe_index` capability probe.
//!
//! ### Route ordering note
//!
//! `DELETE /b/vector/api/indexes/{name}` and `DELETE /b/vector/api/{index}/{id}`
//! both map to the `delete` action and both live under `/b/vector/api/`.
//! The dispatcher checks the `indexes/` prefix first so the more specific
//! route wins; only after that do we fall through to the generic
//! `{index}/{id}` handler.

use wafer_block::{
    db::{Filter, FilterOp, ListOptions},
    wire::database::OnConflict,
};
use wafer_core::{
    clients::{
        database as db,
        vector::{self as vclient, MetadataFilter, SearchMode, VectorEntry, VectorIndexConfig},
    },
    interfaces::vector::{get_model, DEFAULT_MODEL},
};
use wafer_run::{context::Context, ErrorCode, InputStream, Message, OutputStream, WaferError};

use super::{
    contracts::{
        self, AckResponse, CreateIndexRequest, CreateIndexResponse, EmbedRequest, EmbedResponse,
        IndexListResponse, IndexStatsResponse, IndexStatsView, IngestRequest, IngestResponse,
        QueryRequest, QueryResponse, UpsertRequest, VectorMatchView,
    },
    ingestion::{self, DEFAULT_CHUNK_TOKENS, DEFAULT_OVERLAP_RATIO},
    service::{self, REGISTRY_TABLE, TABLE_PREFIX},
};
use crate::{
    http::{err_bad_request, err_internal, err_internal_no_cause, err_not_found, ok_json},
    util::path_param,
};

// Per-route dispatch now lives in `VectorBlock::handle` via the shared
// `endpoint_match` table; the JSON handlers below are called directly from
// there (no in-block `route` shim, no `starts_with` ordering guards).

// ---------------------------------------------------------------------------
// Backend-availability gate
// ---------------------------------------------------------------------------

/// 503 for every op that needs the `wafer-run/vector` backend when it isn't
/// registered on this runtime (native impresspress ships no native vector
/// engine — see `pages_ui.rs` module docs).
///
/// Every `vclient::*` call below (`create_index`, `list_indexes`, `upsert`,
/// `query`, …) targets that one block, so its absence surfaces as
/// `ErrorCode::NotFound: block 'wafer-run/vector' not found` — the *same*
/// error code the per-op branches below already use for a semantically
/// different thing ("this index doesn't exist"). Pattern-matching on the
/// wire error would conflate the two; checking
/// `service::vector_backend_available` up front (the same check the `/b/vector/`
/// UI page uses to render its "backend not available" callout) disambiguates
/// them and lets every handler degrade the same way instead of a handful
/// misreporting "index not found" and the rest 500ing.
fn err_vector_backend_unavailable() -> OutputStream {
    OutputStream::error(WaferError::new(
        ErrorCode::Unavailable,
        "vector backend (wafer-run/vector) is not available on this deployment",
    ))
}

// ---------------------------------------------------------------------------
// POST /b/vector/api/indexes — create an index
// ---------------------------------------------------------------------------

/// Parse the create-index body as JSON or as the admin modal's URL-encoded
/// form. Sniffed the same way `util::parse_body_value` sniffed before the
/// contract type existed — a leading `{` is JSON — so a JSON body sent
/// without a content-type header keeps working. The JSON path deserializes
/// the contract directly and gets no coercions, which is what the published
/// schema says; the form path goes through [`CreateIndexRequest::from_form`]
/// for the ones a form needs (`keyword_search=on`, numbers as strings).
fn parse_create_index_body(raw: &[u8]) -> Result<CreateIndexRequest, String> {
    let first = raw.iter().find(|b| !b.is_ascii_whitespace());
    if first == Some(&b'{') {
        return serde_json::from_slice(raw).map_err(|e| format!("Invalid body: {e}"));
    }
    Ok(CreateIndexRequest::from_form(
        &crate::util::parse_form_body(raw),
    ))
}

pub(super) async fn create_index(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    if !service::vector_backend_available(ctx) {
        return err_vector_backend_unavailable();
    }
    let raw = input.collect_to_bytes().await;
    let body = match parse_create_index_body(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&e),
    };

    if body.name.is_empty() {
        return err_bad_request("index name is required");
    }
    if let Err(e) = service::validate_index_name(&body.name) {
        return err_bad_request(&e.message);
    }

    // Borrow the caller's `model` when present; only fall back to the static
    // `DEFAULT_MODEL` literal when absent. Avoids a String allocation per call.
    let model_id: &str = body.model.as_deref().unwrap_or(DEFAULT_MODEL);
    let Some(model) = get_model(model_id) else {
        return err_bad_request(&format!("unknown embedding model: {model_id}"));
    };

    if let Some(requested) = body.dimensions {
        if requested != model.dimensions {
            return err_bad_request(&format!(
                "dimensions mismatch: model {} has {} dimensions, got {}",
                model.id, model.dimensions, requested
            ));
        }
    }

    let cfg = VectorIndexConfig {
        name: service::prefixed_index_name(&body.name),
        model: model.id.to_string(),
        dimensions: model.dimensions,
        metric: body
            .metric
            .unwrap_or(contracts::DistanceMetric::Cosine)
            .into(),
        keyword_search: body.keyword_search,
    };

    if let Err(e) = vclient::create_index(ctx, cfg.clone()).await {
        return err_internal("create_index failed", e);
    }

    // Record the index in the registry so queries against it can look up
    // the right embedding model when the caller sends text. A failure here
    // leaves the underlying index created but unregistered — report it as
    // an internal error rather than silently swallowing it; the operator
    // can retry create (idempotent at the registry level via OR REPLACE
    // and harmless at vclient level because the index already exists).
    //
    // The registry table itself is owned by the block's
    // `migrations/001_vector_schema.*.sql` script (run at block Init via
    // `apply_if_blessed`), so no inline CREATE TABLE is required here.
    if let Err(e) = db::upsert(
        ctx,
        REGISTRY_TABLE,
        vec![
            ("prefixed_name".to_string(), serde_json::json!(cfg.name)),
            ("model".to_string(), serde_json::json!(cfg.model)),
            ("dimensions".to_string(), serde_json::json!(cfg.dimensions)),
            (
                "keyword_search".to_string(),
                serde_json::json!(cfg.keyword_search as i64),
            ),
        ],
        vec!["prefixed_name".to_string()],
        OnConflict::SetColumns(vec![
            "model".to_string(),
            "dimensions".to_string(),
            "keyword_search".to_string(),
        ]),
    )
    .await
    {
        return err_internal("registry write failed", e);
    }

    // htmx callers (the admin modal) want HTML back so the swap renders
    // and HX-Trigger can close the modal + show a toast. Programmatic
    // JSON callers still get the JSON payload.
    if !msg.get_meta("http.header.hx-request").is_empty() {
        let body_html = match super::pages_ui::render_index_list_fragment(ctx).await {
            Ok(m) => m,
            Err(e) => return err_internal("Failed to refresh", e),
        };
        let trigger = r#"{"showToast":{"message":"Index created","type":"success"},"closeModal":{"id":"create-vector-index"}}"#;
        return crate::http::ResponseBuilder::new()
            .set_header("HX-Trigger", trigger)
            .body(
                body_html.into_string().into_bytes(),
                "text/html; charset=utf-8",
            );
    }
    ok_json(&CreateIndexResponse {
        name: body.name,
        model: cfg.model,
        dimensions: cfg.dimensions,
        metric: cfg.metric.into(),
        keyword_search: cfg.keyword_search,
    })
}

// ---------------------------------------------------------------------------
// GET /b/vector/api/indexes — list indexes
// ---------------------------------------------------------------------------

pub(super) async fn list_indexes(ctx: &dyn Context) -> OutputStream {
    // No backend registered → no indexes to report. Mirrors the `/b/vector/`
    // UI page's empty state (`pages_ui::index_list_page`): callers that only
    // ever list should see "nothing here" rather than an error, since an
    // empty result is already a legitimate response shape for this endpoint.
    if !service::vector_backend_available(ctx) {
        return ok_json(&IndexListResponse {
            indexes: Vec::new(),
        });
    }
    match discover_indexes(ctx).await {
        Ok(indexes) => ok_json(&IndexListResponse { indexes }),
        Err(e) => err_internal("list indexes failed", e),
    }
}

/// Scan sqlite_master for the per-index `_meta` tables created by
/// `SqliteVecService::create_index` and return the user-facing index
/// names (prefix stripped).
///
/// We ask the vector service's catalog (typed `vector.list_indexes`, which
/// is WRAP-authorized on the namespace prefix) rather than the registry so
/// that indexes created before the registry existed still surface in
/// list/stats. The registry is the source of truth for *per-index metadata*
/// (model, keyword_search flag), not for existence.
async fn discover_indexes(ctx: &dyn Context) -> Result<Vec<String>, WaferError> {
    let stems = vclient::list_indexes(ctx, TABLE_PREFIX).await?;
    let mut indexes: Vec<String> = Vec::with_capacity(stems.len());
    for stem in stems {
        if let Some(user_name) = stem.strip_prefix(TABLE_PREFIX) {
            if !user_name.is_empty() {
                indexes.push(user_name.to_string());
            }
        }
    }
    Ok(indexes)
}

// ---------------------------------------------------------------------------
// DELETE /b/vector/api/indexes/{name} — delete an index
// ---------------------------------------------------------------------------

pub(super) async fn delete_index(ctx: &dyn Context, msg: &Message) -> OutputStream {
    if !service::vector_backend_available(ctx) {
        return err_vector_backend_unavailable();
    }
    let name = path_param(msg, "name", "/b/vector/api/indexes/");
    if name.is_empty() {
        return err_bad_request("index name is required");
    }
    if let Err(e) = service::validate_index_name(name) {
        return err_bad_request(&e.message);
    }

    let prefixed = service::prefixed_index_name(name);
    match vclient::delete_index(ctx, &prefixed).await {
        Ok(()) => {
            // Clear the registry row. Best-effort — a missing registry table
            // (pre-registry deployment) is not a failure, so we only surface
            // errors that aren't about the table itself. The row-level
            // `OR REPLACE` in create_index makes this robustly idempotent.
            let _ = db::delete_by_filters(
                ctx,
                REGISTRY_TABLE,
                vec![Filter {
                    field: "prefixed_name".into(),
                    operator: FilterOp::Equal,
                    value: serde_json::json!(prefixed),
                }],
            )
            .await;
            ok_json(&AckResponse { ok: true })
        }
        Err(e) if e.code == ErrorCode::NotFound => {
            err_not_found(&format!("index not found: {name}"))
        }
        Err(e) => err_internal("delete_index failed", e),
    }
}

// ---------------------------------------------------------------------------
// POST /b/vector/api/upsert — upsert pre-computed vectors
// ---------------------------------------------------------------------------

pub(super) async fn upsert(ctx: &dyn Context, input: InputStream) -> OutputStream {
    if !service::vector_backend_available(ctx) {
        return err_vector_backend_unavailable();
    }
    let raw = input.collect_to_bytes().await;
    let body: UpsertRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    if body.index.is_empty() {
        return err_bad_request("index is required");
    }
    if let Err(e) = service::validate_index_name(&body.index) {
        return err_bad_request(&e.message);
    }

    let prefixed = service::prefixed_index_name(&body.index);
    let entries: Vec<VectorEntry> = body.entries.into_iter().map(VectorEntry::from).collect();
    match vclient::upsert(ctx, &prefixed, entries).await {
        Ok(()) => ok_json(&AckResponse { ok: true }),
        Err(e) if e.code == ErrorCode::NotFound => {
            err_not_found(&format!("index not found: {}", body.index))
        }
        Err(e) if e.code == ErrorCode::InvalidArgument => err_bad_request(&e.message),
        Err(e) => err_internal("upsert failed", e),
    }
}

// ---------------------------------------------------------------------------
// DELETE /b/vector/api/{index}/{id} — delete a single vector by ID
// ---------------------------------------------------------------------------

pub(super) async fn delete_single(ctx: &dyn Context, msg: &Message) -> OutputStream {
    if !service::vector_backend_available(ctx) {
        return err_vector_backend_unavailable();
    }
    let (index, id) = extract_index_and_id(msg);
    if index.is_empty() {
        return err_bad_request("index is required");
    }
    if id.is_empty() {
        return err_bad_request("id is required");
    }
    if let Err(e) = service::validate_index_name(index) {
        return err_bad_request(&e.message);
    }

    let prefixed = service::prefixed_index_name(index);
    match vclient::delete(ctx, &prefixed, vec![id.to_string()]).await {
        Ok(()) => ok_json(&AckResponse { ok: true }),
        Err(e) if e.code == ErrorCode::NotFound => {
            err_not_found(&format!("index not found: {index}"))
        }
        Err(e) => err_internal("delete failed", e),
    }
}

/// Extract `{index}` and `{id}` from `/b/vector/api/{index}/{id}`.
///
/// Prefers the router-populated path variables when available, falling back
/// to string-splitting for direct handler invocation (e.g. in tests).
fn extract_index_and_id(msg: &Message) -> (&str, &str) {
    let index = msg.var("index");
    let id = msg.var("id");
    if !index.is_empty() && !id.is_empty() {
        return (index, id);
    }

    let rest = msg.path().strip_prefix("/b/vector/api/").unwrap_or("");
    let mut parts = rest.split('/');
    let index_part = parts.next().unwrap_or("");
    let id_part = parts.next().unwrap_or("");
    (index_part, id_part)
}

// ---------------------------------------------------------------------------
// GET /b/vector/api/stats — per-index counts
// ---------------------------------------------------------------------------

pub(super) async fn stats(ctx: &dyn Context) -> OutputStream {
    // Same "no backend → nothing to report" empty-list shape as
    // `list_indexes` above — `stats` is a per-index-count listing, not a
    // write/query op, so an absent backend just means zero indexes to count.
    if !service::vector_backend_available(ctx) {
        return ok_json(&IndexStatsResponse {
            indexes: Vec::new(),
        });
    }
    let indexes = match discover_indexes(ctx).await {
        Ok(v) => v,
        Err(e) => return err_internal("stats failed", e),
    };

    let mut out: Vec<IndexStatsView> = Vec::with_capacity(indexes.len());
    for name in indexes {
        let prefixed = service::prefixed_index_name(&name);
        // If count fails for a single index (e.g. table was dropped between
        // discovery and count), fall back to 0 and keep going — stats should
        // not 500 on a transient partial-state issue.
        let count = vclient::count(ctx, &prefixed).await.unwrap_or(0);
        out.push(IndexStatsView { name, count });
    }

    ok_json(&IndexStatsResponse { indexes: out })
}

// ---------------------------------------------------------------------------
// POST /b/vector/api/query — search vectors
// ---------------------------------------------------------------------------

const DEFAULT_TOP_K: usize = 10;

pub(super) async fn query(ctx: &dyn Context, input: InputStream) -> OutputStream {
    if !service::vector_backend_available(ctx) {
        return err_vector_backend_unavailable();
    }
    let raw = input.collect_to_bytes().await;
    let mut body: QueryRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    if body.index.is_empty() {
        return err_bad_request("index is required");
    }
    if let Err(e) = service::validate_index_name(&body.index) {
        return err_bad_request(&e.message);
    }

    let prefixed = service::prefixed_index_name(&body.index);

    // Look up model + keyword_search from the registry, falling back to
    // DEFAULT_MODEL + sqlite_master scan for indexes created before the
    // registry table existed.
    let (model_id, keyword_search) = match load_index_metadata(ctx, &prefixed).await {
        Ok(m) => m,
        Err(e) => return err_internal("load index metadata failed", e),
    };

    // Default mode reflects the index's declared capabilities. An index
    // created with keyword_search=true gets Hybrid by default; everyone
    // else gets plain Vector.
    let mode = body
        .mode
        .map(SearchMode::from)
        .unwrap_or(if keyword_search {
            SearchMode::Hybrid
        } else {
            SearchMode::Vector
        });

    // Resolve the query vector. If the caller provided a vector directly
    // we use it; otherwise we embed `text` through the model the index
    // was created with. Exactly one of the two must be present. We `take`
    // both Options so the large `Vec<f32>` / `String` move rather than clone.
    let vector = match (body.vector.take(), body.text.as_deref()) {
        (Some(v), _) if !v.is_empty() => v,
        (_, Some(text)) if !text.is_empty() => {
            let block = embedding_block_for_model(&model_id);
            match vclient::embed(ctx, block, vec![text.to_string()]).await {
                Ok((_, _, mut vectors)) => match vectors.pop() {
                    Some(v) => v,
                    None => return err_internal_no_cause("embedding block returned no vectors"),
                },
                Err(e) => return err_internal("embed failed", e),
            }
        }
        _ => return err_bad_request("either 'text' or 'vector' is required"),
    };

    // For modes that use keyword search, default the keyword query to the
    // raw text when the caller didn't supply an explicit one. This lets
    // hybrid-mode callers pass just `text` and get both halves for free.
    let keyword_query = match (mode, body.keyword_query.take(), body.text.take()) {
        (SearchMode::Vector, kq, _) => kq,
        (_, Some(kq), _) => Some(kq),
        (_, None, Some(text)) => Some(text),
        (_, None, None) => None,
    };

    let top_k = body.top_k.unwrap_or(DEFAULT_TOP_K);

    match vclient::query(
        ctx,
        &prefixed,
        vector,
        top_k,
        body.filter.map(MetadataFilter::from),
        mode,
        keyword_query,
    )
    .await
    {
        Ok(matches) => ok_json(&QueryResponse {
            matches: matches.into_iter().map(VectorMatchView::from).collect(),
        }),
        Err(e) if e.code == ErrorCode::NotFound => {
            err_not_found(&format!("index not found: {}", body.index))
        }
        Err(e) if e.code == ErrorCode::InvalidArgument => err_bad_request(&e.message),
        Err(e) => err_internal("query failed", e),
    }
}

/// Load `(model_id, keyword_search)` for an index.
///
/// Registry-first: if the row exists we trust it. If it doesn't (pre-registry
/// index, or registry table missing entirely) we fall back to
/// `DEFAULT_MODEL` and infer `keyword_search` by checking `sqlite_master`
/// for the per-index `_fts` table. Existence of the index is validated by
/// `vclient::query` itself, which returns `NotFound` when the underlying
/// tables are missing — so this helper always returns `Ok` on a successful
/// database roundtrip.
async fn load_index_metadata(
    ctx: &dyn Context,
    prefixed_index: &str,
) -> Result<(String, bool), WaferError> {
    // First try the registry. An error here (e.g. the table doesn't exist)
    // is treated as "no row", not fatal — we fall through to the scan.
    let rows = db::list(
        ctx,
        REGISTRY_TABLE,
        &ListOptions {
            columns: Some(vec!["model".into(), "keyword_search".into()]),
            filters: vec![Filter {
                field: "prefixed_name".into(),
                operator: FilterOp::Equal,
                value: serde_json::json!(prefixed_index),
            }],
            limit: 1,
            skip_count: true,
            ..Default::default()
        },
    )
    .await;

    if let Ok(rows) = rows {
        if let Some(row) = rows.records.into_iter().next() {
            let model = row
                .data
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_MODEL)
                .to_string();
            let kw = row
                .data
                .get("keyword_search")
                .and_then(|v| v.as_i64())
                .map(|n| n != 0)
                .unwrap_or(false);
            return Ok((model, kw));
        }
    }

    // Fallback path: infer keyword_search from the typed describe op.
    // `wafer-block-sqlite::SqliteVecService::create_index` creates
    // `{prefixed}_fts` when keyword_search is enabled and nothing when it
    // isn't; `describe_index` reports that as `keyword_search`. Absence of
    // the whole index is data too (`exists: false` → false), not an error.
    let desc = vclient::describe_index(ctx, prefixed_index).await?;

    Ok((DEFAULT_MODEL.to_string(), desc.keyword_search))
}

/// Map a model id to the embedding block that serves it on this runtime.
///
/// On native we route everything to `impresspress/fastembed` — fastembed's
/// catalog covers every model in our native-embed support matrix. Plan 2
/// (Workers AI) and Plan 3 (browser/transformers) will split this by
/// `model_id` so different models dispatch to different embedding blocks.
fn embedding_block_for_model(_model_id: &str) -> &'static str {
    #[cfg(target_arch = "wasm32")]
    {
        "impresspress/transformers-embed"
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        "impresspress/fastembed"
    }
}

// ---------------------------------------------------------------------------
// POST /b/vector/api/ingest — chunk + (optionally add context) + embed + upsert
// ---------------------------------------------------------------------------

/// Handle `POST /b/vector/api/ingest`.
///
/// Flow: prefix the index, look up the embedding model, clear any prior
/// chunks for this `document_id` (re-ingestion safety), chunk the text,
/// optionally add context summaries, embed, and upsert. The response tells
/// the caller how many chunks landed.
pub(super) async fn ingest(ctx: &dyn Context, input: InputStream) -> OutputStream {
    if !service::vector_backend_available(ctx) {
        return err_vector_backend_unavailable();
    }
    let raw = input.collect_to_bytes().await;
    let body: IngestRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    if body.index.is_empty() {
        return err_bad_request("index is required");
    }
    if let Err(e) = service::validate_index_name(&body.index) {
        return err_bad_request(&e.message);
    }
    if body.document_id.is_empty() {
        return err_bad_request("document_id is required");
    }

    let prefixed = service::prefixed_index_name(&body.index);

    // We need the index's model so we can re-embed the chunks with the same
    // one that was declared at create_index time. `load_index_metadata`
    // falls back to DEFAULT_MODEL for pre-registry indexes — same as the
    // query route does.
    let (model_id, _keyword_search) = match load_index_metadata(ctx, &prefixed).await {
        Ok(m) => m,
        Err(e) => return err_internal("load index metadata failed", e),
    };

    // Re-ingestion safety: wipe any chunks we previously wrote for this
    // document_id before we add the new ones, via the typed `vector.list_ids`
    // metadata-equality op. If the index isn't there yet (first-ever ingest,
    // or fresh index) the lookup errors with NotFound and we take that as
    // "no prior chunks", not as a fatal error. Any other error (e.g. WRAP
    // PermissionDenied, a transient DB failure) must abort the request —
    // silently treating it as "no prior chunks" would skip cleanup and leave
    // stale tail chunks in the index that queries then serve.
    let mut prior_filter = MetadataFilter::default();
    prior_filter.equals.insert(
        "document_id".into(),
        serde_json::Value::String(body.document_id.clone()),
    );
    let prior_ids = match vclient::list_ids(ctx, &prefixed, prior_filter).await {
        Ok(ids) => ids,
        Err(e) if e.code == ErrorCode::NotFound => Vec::new(),
        Err(e) => return err_internal("failed to list prior chunks", e),
    };
    if !prior_ids.is_empty() {
        if let Err(e) = vclient::delete(ctx, &prefixed, prior_ids).await {
            return err_internal("failed to clear prior chunks", e);
        }
    }

    // Split into chunks. Empty / whitespace-only text produces no chunks;
    // return early rather than inventing an empty entry.
    //
    // The chunker counts whitespace-words as a proxy for tokens. To size
    // chunks against the embedder's real BPE limit (bge-m3 produces ~1.3-1.5
    // BPE tokens per whitespace word on English prose, more on CJK and heavy
    // punctuation), ask the embedding block to count tokens on the whole
    // document up front and ratio-adjust DEFAULT_CHUNK_TOKENS by
    // bpe / whitespace. One wire call per ingest, not one per chunk
    // boundary. Falls back to DEFAULT_CHUNK_TOKENS if the embedder's
    // count_tokens returns 0 or the call errors — chunks may run slightly
    // over BPE-budget, which is the same approximation in use before this
    // change.
    let embedding_block = embedding_block_for_model(&model_id);
    let whitespace_tokens = body.text.split_whitespace().count() as u64;
    let effective_chunk_tokens = if whitespace_tokens == 0 {
        DEFAULT_CHUNK_TOKENS
    } else {
        match vclient::count_tokens(ctx, embedding_block, body.text.clone()).await {
            Ok(bpe) if bpe > 0 => {
                let ratio = (bpe as f32) / (whitespace_tokens as f32);
                ((DEFAULT_CHUNK_TOKENS as f32) / ratio.max(1.0)).round() as usize
            }
            _ => DEFAULT_CHUNK_TOKENS,
        }
    };

    let mut chunks = ingestion::chunk(&body.text, effective_chunk_tokens, DEFAULT_OVERLAP_RATIO);
    if body.contextual {
        match ingestion::add_context(ctx, &body.text, chunks).await {
            Ok(c) => chunks = c,
            Err(e) => return err_internal("add_context failed", e),
        }
    }
    if chunks.is_empty() {
        return ok_json(&IngestResponse { chunks_created: 0 });
    }

    // Embed via the right block for this model. On native today that's
    // always `impresspress/fastembed`; see `embedding_block_for_model`.
    let (_model_name, _dims, vectors) =
        match vclient::embed(ctx, embedding_block, chunks.clone()).await {
            Ok(tuple) => tuple,
            Err(e) => return err_internal("embed failed", e),
        };

    if vectors.len() != chunks.len() {
        // Sanity check — embedding block violated its contract. Surface
        // the mismatch instead of silently upserting a truncated set.
        return err_internal(
            "embedding/chunk count mismatch",
            format!("vectors={} chunks={}", vectors.len(), chunks.len()),
        );
    }

    // Build VectorEntry list. Ids are `{document_id}:{i}` so re-ingestion
    // of the same document is idempotent at the row level too (overwrites
    // the same ids). Metadata carries `document_id` and `chunk_index` so
    // the SELECT-by-document_id query above keeps working on re-ingest.
    let entries: Vec<VectorEntry> = chunks
        .into_iter()
        .zip(vectors.into_iter())
        .enumerate()
        .map(|(i, (chunk_text, vector))| VectorEntry {
            id: format!("{}:{}", body.document_id, i),
            vector,
            metadata: Some(serde_json::json!({
                "document_id": body.document_id,
                "chunk_index": i,
                "user_metadata": body.metadata,
            })),
            text: Some(chunk_text),
        })
        .collect();

    let n = entries.len();
    match vclient::upsert(ctx, &prefixed, entries).await {
        Ok(()) => ok_json(&IngestResponse { chunks_created: n }),
        Err(e) if e.code == ErrorCode::NotFound => {
            err_not_found(&format!("index not found: {}", body.index))
        }
        Err(e) if e.code == ErrorCode::InvalidArgument => err_bad_request(&e.message),
        Err(e) => err_internal("upsert failed", e),
    }
}

// ---------------------------------------------------------------------------
// POST /b/vector/api/embed — generate embeddings for raw text
// ---------------------------------------------------------------------------

/// Handle `POST /b/vector/api/embed`.
///
/// Thin shim over `vclient::embed` — we look up which block serves the
/// requested model on this runtime and dispatch. Empty `texts` is allowed
/// (the embedding block returns an empty vector list).
pub(super) async fn embed(ctx: &dyn Context, input: InputStream) -> OutputStream {
    let raw = input.collect_to_bytes().await;
    let body: EmbedRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    let model = body.model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let block = embedding_block_for_model(&model);

    match vclient::embed(ctx, block, body.texts).await {
        Ok((model, dimensions, vectors)) => ok_json(&EmbedResponse {
            model,
            dimensions,
            vectors,
        }),
        Err(e) if e.code == ErrorCode::InvalidArgument => err_bad_request(&e.message),
        Err(e) => err_internal("embed failed", e),
    }
}

// ---------------------------------------------------------------------------
// Tests: ingest's prior-chunk cleanup must tolerate NotFound only.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod ingest_cleanup_tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use wafer_run::{Block as RunBlock, BlockCategory, BlockInfo, LifecycleEvent};

    use super::*;
    use crate::test_support::{output_is_error, output_json, TestContext};

    /// Stub `wafer-run/vector` block that answers `vector.list_ids` with a
    /// caller-supplied error and errors loudly on anything else.
    ///
    /// The ingest cleanup path under test never needs a second vector op:
    /// a non-NotFound `list_ids` error must abort before chunking/embedding
    /// even starts, and a NotFound `list_ids` error is only followed by
    /// chunking — which the tests short-circuit to a zero-chunk response by
    /// sending whitespace-only text, so no embed/upsert call ever happens.
    struct StubVectorBlock {
        list_ids_error: WaferError,
    }

    #[async_trait]
    impl RunBlock for StubVectorBlock {
        fn info(&self) -> BlockInfo {
            BlockInfo::new(
                "wafer-run/vector",
                "0.0.1",
                "vector@v1",
                "stub vector block for ingest cleanup tests",
            )
            .category(BlockCategory::Service)
        }

        async fn handle(
            &self,
            _ctx: &dyn Context,
            msg: Message,
            _input: InputStream,
        ) -> OutputStream {
            match msg.kind.as_str() {
                wafer_block::common::ServiceOp::VECTOR_LIST_IDS => {
                    OutputStream::error(self.list_ids_error.clone())
                }
                other => OutputStream::error(WaferError::new(
                    ErrorCode::Unimplemented,
                    format!("StubVectorBlock: unhandled op {other}"),
                )),
            }
        }

        async fn lifecycle(
            &self,
            _ctx: &dyn Context,
            _e: LifecycleEvent,
        ) -> Result<(), WaferError> {
            Ok(())
        }
    }

    /// Seed a registry row directly so `load_index_metadata` resolves
    /// model/keyword_search from the DB without calling the vector block —
    /// `vclient::describe_index` is only the fallback for indexes missing a
    /// registry row, and we want the stub above to see exactly one op
    /// (`vector.list_ids`) for the duration of these tests.
    async fn seed_registry_row(ctx: &dyn Context, prefixed: &str) {
        db::upsert(
            ctx,
            REGISTRY_TABLE,
            vec![
                ("prefixed_name".to_string(), serde_json::json!(prefixed)),
                ("model".to_string(), serde_json::json!(DEFAULT_MODEL)),
                ("dimensions".to_string(), serde_json::json!(384)),
                ("keyword_search".to_string(), serde_json::json!(0)),
            ],
            vec!["prefixed_name".to_string()],
            OnConflict::SetColumns(vec![
                "model".to_string(),
                "dimensions".to_string(),
                "keyword_search".to_string(),
            ]),
        )
        .await
        .expect("seed registry row");
    }

    /// Body for `ingest` with whitespace-only `text`, so `ingestion::chunk`
    /// produces zero chunks and the handler returns success right after the
    /// prior-chunk cleanup step — no embed/upsert vector call needed.
    fn whitespace_ingest_body(index: &str, document_id: &str) -> InputStream {
        InputStream::from_bytes(
            serde_json::to_vec(&serde_json::json!({
                "index": index,
                "document_id": document_id,
                "text": "   ",
            }))
            .expect("serialize ingest body"),
        )
    }

    #[tokio::test]
    async fn ingest_aborts_when_prior_chunk_lookup_errors_non_notfound() {
        let mut ctx = TestContext::with_vector().await;
        let prefixed = service::prefixed_index_name("cleanup_test_idx_denied");
        seed_registry_row(&ctx, &prefixed).await;
        ctx.register_block(
            "wafer-run/vector",
            Arc::new(StubVectorBlock {
                list_ids_error: WaferError::new(
                    ErrorCode::PermissionDenied,
                    "WRAP: caller not authorized for this index",
                ),
            }),
        );

        let out = ingest(
            &ctx,
            whitespace_ingest_body("cleanup_test_idx_denied", "doc-1"),
        )
        .await;

        assert!(
            output_is_error(out, "Internal").await,
            "a non-NotFound list_ids error must abort ingest via err_internal, \
             not be silently swallowed — swallowing it would skip cleanup and \
             leave stale tail chunks that queries then serve"
        );
    }

    #[tokio::test]
    async fn ingest_tolerates_notfound_from_prior_chunk_lookup() {
        let mut ctx = TestContext::with_vector().await;
        let prefixed = service::prefixed_index_name("cleanup_test_idx_missing");
        seed_registry_row(&ctx, &prefixed).await;
        ctx.register_block(
            "wafer-run/vector",
            Arc::new(StubVectorBlock {
                list_ids_error: WaferError::new(ErrorCode::NotFound, "index not found"),
            }),
        );

        let out = ingest(
            &ctx,
            whitespace_ingest_body("cleanup_test_idx_missing", "doc-1"),
        )
        .await;

        assert_eq!(
            output_json(out).await,
            serde_json::json!({ "chunks_created": 0 }),
            "a genuine NotFound from list_ids must still be tolerated as \
             'no prior chunks', not treated as fatal - and the zero-chunk reply \
             is the contract's one field"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests: the JSON shapes the published schemas are derived from.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod contract_tests {
    use std::{collections::HashMap, sync::Arc};

    use wafer_block::wire::vector::VectorMatch;
    use wafer_run::streams::output::TerminalNotResponse;

    use super::*;
    use crate::{
        blocks::vector::test_support::{StubEmbeddingBlock, StubVectorBlock},
        test_support::{auth_msg, output_is_error, output_json, TestContext},
    };

    fn json_input(value: serde_json::Value) -> InputStream {
        InputStream::from_bytes(serde_json::to_vec(&value).expect("serialize body"))
    }

    fn form_input(body: &str) -> InputStream {
        InputStream::from_bytes(body.as_bytes().to_vec())
    }

    fn create_msg() -> Message {
        auth_msg("create", "/b/vector/api/indexes", "user-1")
    }

    async fn ctx_with(stub: StubVectorBlock) -> TestContext {
        let mut ctx = TestContext::with_vector().await;
        ctx.register_block("wafer-run/vector", Arc::new(stub));
        ctx
    }

    /// Registry row for `prefixed`, so `load_index_metadata` resolves the
    /// model from the DB and the stub never has to answer `describe_index`.
    async fn seed_registry_row(ctx: &dyn Context, prefixed: &str) {
        db::upsert(
            ctx,
            REGISTRY_TABLE,
            vec![
                ("prefixed_name".to_string(), serde_json::json!(prefixed)),
                ("model".to_string(), serde_json::json!(DEFAULT_MODEL)),
                ("dimensions".to_string(), serde_json::json!(1024)),
                ("keyword_search".to_string(), serde_json::json!(0)),
            ],
            vec!["prefixed_name".to_string()],
            OnConflict::SetColumns(vec![
                "model".to_string(),
                "dimensions".to_string(),
                "keyword_search".to_string(),
            ]),
        )
        .await
        .expect("seed registry row");
    }

    /// JSON callers get the typed contract: a numeric string is not a
    /// number and a checkbox string is not a boolean. The untyped handler
    /// coerced both, so a client that had drifted onto form-shaped JSON was
    /// silently accepted.
    ///
    /// The message is asserted, not just the code: `"1024"` happens to be
    /// the catalog default's dimensionality, so a coercing handler would
    /// still 400 on a later check for any other number, and the test would
    /// pass for the wrong reason.
    #[tokio::test]
    async fn create_index_json_refuses_string_typed_fields() {
        for (body, expected) in [
            (
                serde_json::json!({ "name": "docs", "dimensions": "1024" }),
                r#"invalid type: string "1024", expected u32"#,
            ),
            (
                serde_json::json!({ "name": "docs", "keyword_search": "on" }),
                r#"invalid type: string "on", expected a boolean"#,
            ),
        ] {
            let ctx = ctx_with(StubVectorBlock::default()).await;

            let out = create_index(&ctx, &create_msg(), json_input(body.clone())).await;

            match out.collect_buffered().await {
                Err(TerminalNotResponse::Error(e)) => {
                    assert_eq!(e.code, ErrorCode::InvalidArgument, "{body}");
                    assert!(
                        e.message.starts_with("Invalid body: ") && e.message.contains(expected),
                        "{body}: the refusal must come from the typed contract, got: {}",
                        e.message
                    );
                }
                other => panic!("{body}: expected InvalidArgument, got {other:?}"),
            }
        }
    }

    /// `metric` is the backend's `DistanceMetric` enum: a value outside it is
    /// refused rather than silently falling back to cosine.
    #[tokio::test]
    async fn create_index_json_refuses_an_unknown_metric() {
        let ctx = ctx_with(StubVectorBlock::default()).await;

        let out = create_index(
            &ctx,
            &create_msg(),
            json_input(serde_json::json!({ "name": "docs", "metric": "manhattan" })),
        )
        .await;

        assert!(output_is_error(out, "InvalidArgument").await);
    }

    #[tokio::test]
    async fn create_index_json_publishes_exactly_the_contract_fields() {
        let ctx = ctx_with(StubVectorBlock::default()).await;

        let body = output_json(
            create_index(
                &ctx,
                &create_msg(),
                json_input(serde_json::json!({
                    "name": "docs",
                    "model": "multilingual-e5-small",
                    "dimensions": 384,
                    "metric": "euclidean",
                    "keyword_search": true,
                })),
            )
            .await,
        )
        .await;

        assert_eq!(
            body,
            serde_json::json!({
                "name": "docs",
                "model": "multilingual-e5-small",
                "dimensions": 384,
                "metric": "euclidean",
                "keyword_search": true,
            })
        );
    }

    /// The admin modal posts a URL-encoded form: the checkbox arrives as
    /// `on` (and only when ticked), the model as an empty string. That path
    /// keeps its coercions and lands on the same contract as JSON.
    #[tokio::test]
    async fn create_index_form_path_coerces_the_checkbox_and_defaults() {
        let ctx = ctx_with(StubVectorBlock::default()).await;
        let default_dims = get_model(DEFAULT_MODEL)
            .expect("catalog default")
            .dimensions;

        let body = output_json(
            create_index(
                &ctx,
                &create_msg(),
                form_input("name=docs&model=&keyword_search=on"),
            )
            .await,
        )
        .await;

        assert_eq!(
            body,
            serde_json::json!({
                "name": "docs",
                "model": DEFAULT_MODEL,
                "dimensions": default_dims,
                "metric": "cosine",
                "keyword_search": true,
            })
        );
    }

    #[tokio::test]
    async fn list_and_stats_publish_the_index_views() {
        let ctx = ctx_with(StubVectorBlock {
            indexes: vec![
                service::prefixed_index_name("docs"),
                service::prefixed_index_name("notes"),
            ],
            counts: HashMap::from([(service::prefixed_index_name("docs"), 3)]),
            ..Default::default()
        })
        .await;

        assert_eq!(
            output_json(list_indexes(&ctx).await).await,
            serde_json::json!({ "indexes": ["docs", "notes"] })
        );
        assert_eq!(
            output_json(stats(&ctx).await).await,
            serde_json::json!({
                "indexes": [
                    { "name": "docs", "count": 3 },
                    { "name": "notes", "count": 0 },
                ]
            })
        );
    }

    /// A hit is id + score + the stored metadata, which is absent (not
    /// `null`) when the entry stored none.
    #[tokio::test]
    async fn query_publishes_the_match_view() {
        let ctx = ctx_with(StubVectorBlock {
            matches: vec![
                VectorMatch {
                    id: "a".into(),
                    score: 0.75,
                    metadata: Some(serde_json::json!({ "k": "v" })),
                },
                VectorMatch {
                    id: "b".into(),
                    score: 0.5,
                    metadata: None,
                },
            ],
            ..Default::default()
        })
        .await;
        seed_registry_row(&ctx, &service::prefixed_index_name("docs")).await;

        let body = output_json(
            query(
                &ctx,
                json_input(serde_json::json!({ "index": "docs", "vector": [0.1, 0.2, 0.3] })),
            )
            .await,
        )
        .await;

        assert_eq!(
            body,
            serde_json::json!({
                "matches": [
                    { "id": "a", "score": 0.75, "metadata": { "k": "v" } },
                    { "id": "b", "score": 0.5 },
                ]
            })
        );
    }

    /// Both `vector` and `text` is a legitimate call — in hybrid mode `text`
    /// feeds the keyword half — and in vector mode the vector is used as is.
    /// No embedding block is registered here, so had the handler tried to
    /// embed `text` the call would have failed with a 500 ("embed failed");
    /// the 200 is the proof that it did not.
    #[tokio::test]
    async fn query_with_both_vector_and_text_uses_the_vector_without_embedding() {
        let ctx = ctx_with(StubVectorBlock {
            matches: vec![VectorMatch {
                id: "a".into(),
                score: 0.75,
                metadata: None,
            }],
            ..Default::default()
        })
        .await;
        seed_registry_row(&ctx, &service::prefixed_index_name("docs")).await;

        let body = output_json(
            query(
                &ctx,
                json_input(serde_json::json!({
                    "index": "docs",
                    "vector": [0.1, 0.2, 0.3],
                    "text": "hello",
                })),
            )
            .await,
        )
        .await;

        assert_eq!(
            body,
            serde_json::json!({ "matches": [{ "id": "a", "score": 0.75 }] })
        );
    }

    #[tokio::test]
    async fn upsert_and_deletes_acknowledge() {
        let ctx = ctx_with(StubVectorBlock::default()).await;

        assert_eq!(
            output_json(
                upsert(
                    &ctx,
                    json_input(serde_json::json!({
                        "index": "docs",
                        "entries": [{ "id": "a", "vector": [0.1, 0.2] }],
                    })),
                )
                .await
            )
            .await,
            serde_json::json!({ "ok": true })
        );
        assert_eq!(
            output_json(
                delete_index(
                    &ctx,
                    &auth_msg("delete", "/b/vector/api/indexes/docs", "user-1")
                )
                .await
            )
            .await,
            serde_json::json!({ "ok": true })
        );
        assert_eq!(
            output_json(
                delete_single(&ctx, &auth_msg("delete", "/b/vector/api/docs/a", "user-1")).await
            )
            .await,
            serde_json::json!({ "ok": true })
        );
    }

    #[tokio::test]
    async fn embed_publishes_exactly_the_contract_fields() {
        let mut ctx = TestContext::with_vector().await;
        ctx.register_block(
            "impresspress/fastembed",
            Arc::new(StubEmbeddingBlock {
                model: "bge-m3",
                dimensions: 3,
            }),
        );

        let body =
            output_json(embed(&ctx, json_input(serde_json::json!({ "texts": ["a", "b"] }))).await)
                .await;

        assert_eq!(
            body,
            serde_json::json!({
                "model": "bge-m3",
                "dimensions": 3,
                "vectors": [[0.5, 0.5, 0.5], [0.5, 0.5, 0.5]],
            })
        );
    }
}

// ---------------------------------------------------------------------------
// Tests: GET /b/vector/api/indexes degrades gracefully when the
// `wafer-run/vector` backend block isn't registered (the live-server 500
// this fix closes), and still lists real indexes when it is.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod backend_availability_tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use wafer_run::{Block as RunBlock, BlockCategory, BlockInfo, LifecycleEvent};

    use super::*;
    use crate::test_support::{output_is_error, output_json, output_status, TestContext};

    /// Minimal `wafer-run/vector` stand-in that answers `vector.list_indexes`
    /// with one fixed storage-prefixed stem, mirroring the shape the real
    /// `wafer-block-sqlite` vector service returns.
    struct FakeListIndexesBlock;

    #[async_trait]
    impl RunBlock for FakeListIndexesBlock {
        fn info(&self) -> BlockInfo {
            BlockInfo::new("wafer-run/vector", "0.0.1", "vector@v1", "test fake")
                .category(BlockCategory::Service)
        }

        async fn handle(
            &self,
            _ctx: &dyn Context,
            msg: Message,
            _input: InputStream,
        ) -> OutputStream {
            match msg.kind.as_str() {
                wafer_block::common::ServiceOp::VECTOR_LIST_INDEXES => {
                    let resp = wafer_block::wire::vector::ListIndexesResponse {
                        indexes: vec!["impresspress__vector__docs".to_string()],
                    };
                    OutputStream::respond(
                        wafer_block::codec::encode(&resp).expect("encode list_indexes response"),
                    )
                }
                other => OutputStream::error(WaferError::new(
                    ErrorCode::Unimplemented,
                    format!("FakeListIndexesBlock has no handler for '{other}'"),
                )),
            }
        }

        async fn lifecycle(
            &self,
            _ctx: &dyn Context,
            _e: LifecycleEvent,
        ) -> Result<(), WaferError> {
            Ok(())
        }
    }

    /// `TestContext::with_vector()` on its own — no `wafer-run/vector` block
    /// registered — is exactly the default native-server configuration
    /// (`docs/checks/...` bug report: native impresspress ships no
    /// `wafer-run/vector` backend). Before this fix, `list_indexes` blindly
    /// propagated the backend's `NotFound: block 'wafer-run/vector' not
    /// found` through `err_internal`, which is what surfaced as the live
    /// 500 `GET /b/vector/api/indexes` returned.
    #[tokio::test]
    async fn list_indexes_returns_200_when_backend_absent() {
        let ctx = TestContext::with_vector().await;

        let out = list_indexes(&ctx).await;

        assert_eq!(
            output_status(out).await,
            200,
            "list_indexes must not 500 when the wafer-run/vector backend isn't registered"
        );
    }

    #[tokio::test]
    async fn list_indexes_returns_empty_list_when_backend_absent() {
        let ctx = TestContext::with_vector().await;

        let out = list_indexes(&ctx).await;

        assert_eq!(
            output_json(out).await,
            serde_json::json!({ "indexes": [] }),
            "backend-absent list must be a clean empty result, matching the \
             /b/vector/ UI page's existing empty-state behavior"
        );
    }

    #[tokio::test]
    async fn list_indexes_still_lists_real_indexes_when_backend_registered() {
        let mut ctx = TestContext::with_vector().await;
        ctx.register_block("wafer-run/vector", Arc::new(FakeListIndexesBlock));

        let out = list_indexes(&ctx).await;

        assert_eq!(
            output_json(out).await,
            serde_json::json!({ "indexes": ["docs"] }),
            "once the backend is registered, list_indexes must go back to \
             reporting real indexes instead of short-circuiting to empty"
        );
    }

    /// Write-shaped ops (as opposed to the list-shaped `list_indexes`) must
    /// surface a clear, typed `Unavailable` error instead of either a raw
    /// 500 or a misleading `NotFound: index not found` — the same
    /// `ErrorCode::NotFound` the backend's "block not found" and an app's
    /// "index not found" both use, which is exactly the ambiguity the
    /// up-front `vector_backend_available` check avoids.
    #[tokio::test]
    async fn create_index_returns_unavailable_when_backend_absent() {
        let ctx = TestContext::with_vector().await;

        let body = InputStream::from_bytes(
            serde_json::to_vec(&serde_json::json!({ "name": "docs" })).unwrap(),
        );
        let msg = crate::test_support::admin_msg("create", "/b/vector/api/indexes");
        let out = create_index(&ctx, &msg, body).await;

        assert!(
            output_is_error(out, "Unavailable").await,
            "create_index must report Unavailable (503), not NotFound or Internal, \
             when the wafer-run/vector backend isn't registered"
        );
    }
}
