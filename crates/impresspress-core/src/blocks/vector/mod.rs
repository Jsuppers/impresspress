pub mod contracts;
pub mod ingestion;
pub(crate) mod migrations;
pub mod pages;
pub mod pages_ui;
pub mod service;
#[cfg(test)]
mod test_support;

use wafer_run::{BlockInfo, HttpMethod, InstanceMode};

use crate::endpoint_match::{self, request_schema_of, response_schema_of, EndpointRoute};

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

/// The block's HTTP surface: what `handle()` dispatches on and what
/// `info().endpoints` is generated from. The specific `api/indexes/{name}`
/// delete precedes the generic `api/{index}/{id}` delete so index-deletes
/// win (the old ordering invariant). The matcher binds `{name}` / `{index}`
/// / `{id}` into `req.param.*` for the handlers' `msg.var` readers.
///
/// The two SSR pages are `Admin` and the JSON API is `Authenticated`; the
/// central router enforces that from the declaration, so the block holds no
/// `user_id` / `is_admin` preamble.
const ROUTES: &[EndpointRoute<Route>] = &[
    // UI pages
    EndpointRoute::admin(HttpMethod::Get, "/b/vector/", Route::IndexListPage)
        .summary("Vector indexes admin list"),
    EndpointRoute::admin(HttpMethod::Get, "/b/vector/{name}/", Route::IndexDetailPage)
        .summary("Vector index detail"),
    // The admin modal posts this same endpoint as a URL-encoded form with
    // an `HX-Request` header and gets the index list back as HTML. The
    // schemas describe the programmatic JSON path; the form path builds the
    // same request type through `contracts::CreateIndexRequest::from_form`.
    EndpointRoute::authenticated(
        HttpMethod::Post,
        "/b/vector/api/indexes",
        Route::ApiCreateIndex,
    )
    .summary("Create a vector index")
    .input(request_schema_of::<contracts::CreateIndexRequest>)
    .output(response_schema_of::<contracts::CreateIndexResponse>),
    EndpointRoute::authenticated(
        HttpMethod::Get,
        "/b/vector/api/indexes",
        Route::ApiListIndexes,
    )
    .summary("List indexes")
    .output(response_schema_of::<contracts::IndexListResponse>),
    EndpointRoute::authenticated(HttpMethod::Post, "/b/vector/api/upsert", Route::ApiUpsert)
        .summary("Upsert pre-computed vectors")
        .input(request_schema_of::<contracts::UpsertRequest>)
        .output(response_schema_of::<contracts::AckResponse>),
    EndpointRoute::authenticated(HttpMethod::Post, "/b/vector/api/query", Route::ApiQuery)
        .summary("Search vectors")
        .input(request_schema_of::<contracts::QueryRequest>)
        .output(response_schema_of::<contracts::QueryResponse>),
    EndpointRoute::authenticated(HttpMethod::Post, "/b/vector/api/ingest", Route::ApiIngest)
        .summary("Chunk + embed + upsert a document")
        .input(request_schema_of::<contracts::IngestRequest>)
        .output(response_schema_of::<contracts::IngestResponse>),
    EndpointRoute::authenticated(HttpMethod::Post, "/b/vector/api/embed", Route::ApiEmbed)
        .summary("Generate embeddings for raw text")
        .input(request_schema_of::<contracts::EmbedRequest>)
        .output(response_schema_of::<contracts::EmbedResponse>),
    EndpointRoute::authenticated(HttpMethod::Get, "/b/vector/api/stats", Route::ApiStats)
        .summary("Index stats and usage")
        .output(response_schema_of::<contracts::IndexStatsResponse>),
    // Deletes: the specific `indexes/{name}` row before the generic
    // `{index}/{id}` row.
    EndpointRoute::authenticated(
        HttpMethod::Delete,
        "/b/vector/api/indexes/{name}",
        Route::ApiDeleteIndex,
    )
    .summary("Delete an index")
    .path_params(index_name_path_schema)
    .output(response_schema_of::<contracts::AckResponse>),
    EndpointRoute::authenticated(
        HttpMethod::Delete,
        "/b/vector/api/{index}/{id}",
        Route::ApiDeleteSingle,
    )
    .summary("Delete a single vector")
    .path_params(vector_id_path_schema)
    .output(response_schema_of::<contracts::AckResponse>),
];

/// Path-parameter schema for `DELETE /b/vector/api/indexes/{name}`.
///
/// Hand-written rather than derived: the handler reads the name with
/// `msg.var("name")` by name, so a struct declared only to feed
/// `request_schema_of::<T>` would have no runtime user (the `tickets` /
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
        .endpoints(endpoint_match::declare(ROUTES))
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

#[cfg(test)]
mod table_tests {
    use wafer_run::Block as _;

    use super::*;

    /// `info().endpoints` is generated from `ROUTES`; nothing else declares
    /// an endpoint for this block.
    #[test]
    fn info_endpoints_come_from_the_table() {
        let declared = VectorBlock::new().info().endpoints;
        assert_eq!(declared.len(), ROUTES.len());
        for (ep, row) in declared.iter().zip(ROUTES) {
            assert_eq!(ep.method, row.method, "{}", row.template);
            assert_eq!(ep.path, row.template);
            assert_eq!(ep.auth, row.auth, "{}", row.template);
        }
    }
}
