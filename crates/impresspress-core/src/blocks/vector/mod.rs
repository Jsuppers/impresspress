pub mod contracts;
pub mod ingestion;
pub(crate) mod migrations;
pub mod pages;
pub mod pages_ui;
pub mod service;
#[cfg(test)]
mod test_support;

use wafer_run::{AuthLevel, BlockEndpoint, BlockInfo, HttpMethod, InstanceMode};

use crate::endpoint_match::{self, EndpointRoute};

/// In-block dispatch targets. UI pages and the JSON API now share ONE matcher
/// table; the per-route access tier comes from the declared endpoint
/// `AuthLevel` and is enforced centrally (UI → Admin, API → Authenticated).
#[derive(Clone, Copy)]
enum Route {
    IndexListPage,
    IndexDetailPage,
    ApiCreateIndex,
    ApiListIndexes,
    ApiDeleteIndex,
    ApiUpsert,
    ApiQuery,
    ApiIngest,
    ApiEmbed,
    ApiStats,
    ApiDeleteSingle,
}

/// Method + path-template dispatch table, mirroring `info().endpoints`. The
/// specific `api/indexes/{name}` delete precedes the generic
/// `api/{index}/{id}` delete so index-deletes win (the old ordering
/// invariant). The matcher binds `{name}`/`{index}`/`{id}` into `req.param.*`.
const ROUTES: &[EndpointRoute<Route>] = &[
    EndpointRoute::new(HttpMethod::Get, "/b/vector/", Route::IndexListPage),
    EndpointRoute::new(HttpMethod::Get, "/b/vector/{name}/", Route::IndexDetailPage),
    EndpointRoute::new(
        HttpMethod::Post,
        "/b/vector/api/indexes",
        Route::ApiCreateIndex,
    ),
    EndpointRoute::new(
        HttpMethod::Get,
        "/b/vector/api/indexes",
        Route::ApiListIndexes,
    ),
    EndpointRoute::new(HttpMethod::Post, "/b/vector/api/upsert", Route::ApiUpsert),
    EndpointRoute::new(HttpMethod::Post, "/b/vector/api/query", Route::ApiQuery),
    EndpointRoute::new(HttpMethod::Post, "/b/vector/api/ingest", Route::ApiIngest),
    EndpointRoute::new(HttpMethod::Post, "/b/vector/api/embed", Route::ApiEmbed),
    EndpointRoute::new(HttpMethod::Get, "/b/vector/api/stats", Route::ApiStats),
    EndpointRoute::new(
        HttpMethod::Delete,
        "/b/vector/api/indexes/{name}",
        Route::ApiDeleteIndex,
    ),
    EndpointRoute::new(
        HttpMethod::Delete,
        "/b/vector/api/{index}/{id}",
        Route::ApiDeleteSingle,
    ),
];

/// Path-parameter schema for `DELETE /b/vector/api/indexes/{name}`.
///
/// Hand-written rather than derived: the handler reads the name with
/// `path_param(msg, "name", ..)` by name, so a struct declared only to feed
/// `.path_params::<T>()` would have no runtime user (the `tickets` /
/// `messages` precedent).
fn index_name_path_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["name"],
        "properties": {
            "name": {
                "type": "string",
                "description": "Index name, as returned by `GET /b/vector/api/indexes`."
            }
        }
    })
}

/// Path-parameter schema for `DELETE /b/vector/api/{index}/{id}`. Hand-written
/// for the same reason as [`index_name_path_schema`]: `pages::extract_index_and_id`
/// reads both by name.
fn vector_id_path_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["index", "id"],
        "properties": {
            "index": {
                "type": "string",
                "description": "Index name, as returned by `GET /b/vector/api/indexes`."
            },
            "id": {
                "type": "string",
                "description": "Row id, as supplied on upsert (or `{document_id}:{n}` for an ingested chunk)."
            }
        }
    })
}

crate::impresspress_feature_block! {
    /// Vector search, RAG ingestion, and embedding generation (`impresspress/vector`).
    pub struct VectorBlock;
    name: "impresspress/vector",
    info: |_this| {
        BlockInfo::new(
            "impresspress/vector",
            "0.0.1",
            "http-handler@v1",
            "Vector search, RAG ingestion, and embedding generation",
        )
        .instance_mode(InstanceMode::Singleton)
        .requires(vec![
            // The runtime vector service (typed index/query/introspection ops).
            "wafer-run/vector".into(),
            // Registry table reads/writes + per-index counts go through the
            // database service; without this entry caller_requires denies
            // every db::* call and the admin list silently renders empty.
            "wafer-run/database".into(),
            // Embedding for ingest / query-by-text. Allowlist entries are
            // call-time only, so listing both targets is safe on either
            // target (the unused one is simply never called).
            "impresspress/fastembed".into(),
            "impresspress/transformers-embed".into(),
        ])
        .category(wafer_run::BlockCategory::Feature)
        .endpoints(vec![
            BlockEndpoint::get("/b/vector/")
                .summary("Vector indexes admin list")
                .auth(AuthLevel::Admin),
            BlockEndpoint::get("/b/vector/{name}/")
                .summary("Vector index detail")
                .auth(AuthLevel::Admin),
            // The admin modal posts this same endpoint as a URL-encoded form
            // with an `HX-Request` header and gets the index list back as
            // HTML. The schemas describe the programmatic JSON path; the
            // form path builds the same request type through
            // `contracts::CreateIndexRequest::from_form`.
            BlockEndpoint::post("/b/vector/api/indexes")
                .summary("Create a vector index")
                .auth(AuthLevel::Authenticated)
                .input::<contracts::CreateIndexRequest>()
                .output::<contracts::CreateIndexResponse>(),
            BlockEndpoint::get("/b/vector/api/indexes")
                .summary("List indexes")
                .auth(AuthLevel::Authenticated)
                .output::<contracts::IndexListResponse>(),
            BlockEndpoint::delete("/b/vector/api/indexes/{name}")
                .summary("Delete an index")
                .auth(AuthLevel::Authenticated)
                .path_params_schema(index_name_path_schema())
                .output::<contracts::AckResponse>(),
            BlockEndpoint::post("/b/vector/api/upsert")
                .summary("Upsert pre-computed vectors")
                .auth(AuthLevel::Authenticated)
                .input::<contracts::UpsertRequest>()
                .output::<contracts::AckResponse>(),
            BlockEndpoint::post("/b/vector/api/query")
                .summary("Search vectors")
                .auth(AuthLevel::Authenticated)
                .input::<contracts::QueryRequest>()
                .output::<contracts::QueryResponse>(),
            BlockEndpoint::post("/b/vector/api/ingest")
                .summary("Chunk + embed + upsert a document")
                .auth(AuthLevel::Authenticated)
                .input::<contracts::IngestRequest>()
                .output::<contracts::IngestResponse>(),
            BlockEndpoint::post("/b/vector/api/embed")
                .summary("Generate embeddings for raw text")
                .auth(AuthLevel::Authenticated)
                .input::<contracts::EmbedRequest>()
                .output::<contracts::EmbedResponse>(),
            BlockEndpoint::delete("/b/vector/api/{index}/{id}")
                .summary("Delete a single vector")
                .auth(AuthLevel::Authenticated)
                .path_params_schema(vector_id_path_schema())
                .output::<contracts::AckResponse>(),
            BlockEndpoint::get("/b/vector/api/stats")
                .summary("Index stats and usage")
                .auth(AuthLevel::Authenticated)
                .output::<contracts::IndexStatsResponse>(),
        ])
        .can_disable(true)
        .default_enabled(true)
    },
    handle: |_this, ctx, msg, input| {
        // Auth is enforced centrally by `route_to_block` from the declared
        // endpoint `AuthLevel` (UI pages → Admin, JSON API → Authenticated),
        // so the block holds no `user_id`/`is_admin` preamble. The matcher
        // binds `{name}`/`{index}`/`{id}` into `req.param.*`.
        let Some(route) = endpoint_match::dispatch(&mut msg, ROUTES) else {
            return crate::http::err_not_found("not found");
        };
        match route {
            Route::IndexListPage => pages_ui::index_list_page(ctx, &msg).await,
            Route::IndexDetailPage => {
                let name = msg.var("name").to_string();
                pages_ui::index_detail_page(ctx, &msg, &name).await
            }
            Route::ApiCreateIndex => pages::create_index(ctx, &msg, input).await,
            Route::ApiListIndexes => pages::list_indexes(ctx).await,
            Route::ApiDeleteIndex => pages::delete_index(ctx, &msg).await,
            Route::ApiUpsert => pages::upsert(ctx, input).await,
            Route::ApiQuery => pages::query(ctx, input).await,
            Route::ApiIngest => pages::ingest(ctx, input).await,
            Route::ApiEmbed => pages::embed(ctx, input).await,
            Route::ApiStats => pages::stats(ctx).await,
            Route::ApiDeleteSingle => pages::delete_single(ctx, &msg).await,
        }
    },
    lifecycle: |_this, ctx, event| {
        crate::migration_helper::lifecycle_init(
            ctx,
            &event,
            "impresspress/vector",
            migrations::SQLITE_MIGRATIONS,
            migrations::POSTGRES_MIGRATIONS,
        )
        .await
    },
}
