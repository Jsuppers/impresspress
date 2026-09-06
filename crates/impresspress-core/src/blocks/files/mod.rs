//! File storage: buckets, objects, shares, quotas (`impresspress/files`).
//!
//! [`ROUTES`] is the block's one description of its HTTP surface: `handle()`
//! dispatches on it and `info().endpoints` is generated from it. Handlers
//! read path variables only as the matcher bound them (`msg.var(..)`).

mod cloud;
mod contracts;
pub(crate) mod migrations;
pub(crate) mod models;
mod pages_admin;
pub(crate) mod pages_user;
mod quota;
pub(crate) mod repo;
mod share;
pub(crate) mod storage;

use wafer_run::{BlockInfo, HttpMethod, InstanceMode};

use super::rate_limit::{check_user_rate_limit_with, RateLimit, RateLimitOutcome, UserRateLimiter};
use crate::{
    endpoint_match::{self, response_schema_of, EndpointRoute},
    http::{err_not_found, err_unauthorized},
};

/// Handler for one row of [`ROUTES`]. `AdminOverview` serves both the
/// `/b/storage/admin` and `/b/storage/admin/` rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    // Admin SSR pages
    AdminOverview,
    AdminBucketsPage,
    AdminSharesPage,
    AdminQuotasPage,
    // Admin JSON API
    AdminListBuckets,
    AdminStats,
    AdminListShares,
    AdminAccessLogs,
    AdminListQuotas,
    AdminUpdateQuota,
    // Public share link
    DirectAccess,
    // User SSR pages
    BucketListPage,
    ObjectListPage,
    FolderListPage,
    CloudStoragePage,
    // User storage JSON API
    ListBuckets,
    CreateBucket,
    DeleteBucket,
    ListObjects,
    UploadObject,
    GetObject,
    DeleteObject,
    Search,
    Recent,
    // User cloud-storage JSON API
    ListShares,
    CreateShare,
    DeleteShare,
    GetQuota,
}

/// The block's HTTP surface: what `handle()` dispatches on and what
/// `info().endpoints` is generated from. Wire paths; `{name}`, `{key...}`,
/// `{id}`, `{token}`, `{bucket}` and `{prefix...}` are bound into
/// `req.param.*` for the handlers' `msg.var` readers.
///
/// Order is dispatch order. The admin rows and the share link come first
/// because `/b/storage/{bucket}/` and `/b/storage/{bucket}/{prefix...}/` at
/// the end would otherwise claim `/b/storage/admin/` and every other
/// slash-terminated path; the router's `endpoint_auth` is order-independent
/// and takes the strictest matching row, so a generic row can never lower the
/// level a specific one declares. The two `{key...}` object rows precede the
/// bare `.../objects` and `.../{name}` rows, as the old sub-table had them.
///
/// Every row names the level the central router enforces. The `/b/storage/`
/// and `/b/cloudstorage/` prefix routes are `Public`, so a row's level is the
/// whole gate: the admin rows moved here from the admin block's delegation
/// (which sat behind the `Admin` `/b/admin/` prefix) are `admin`, and the
/// user rows the block served but never declared are `authenticated`, the
/// level the handler already required through the session preamble
/// ([`user_preamble`]) and its owner checks, and the level the router's
/// fail-closed default already applied to an undeclared path.
const ROUTES: &[EndpointRoute<Route>] = &[
    // ── Admin SSR pages ── declared `Admin` so the central router enforces
    // the tier; the block has no inline `is_admin` check for them.
    //
    // The overview is declared for BOTH the canonical slash form
    // (`/b/storage/admin/`, the `admin_url`) and the bare form
    // (`/b/storage/admin`). The matcher's trailing-slash retry would serve
    // the bare form from the slash row on its own, but the declared surface
    // keeps both lines so the router's gate for the bare form is stated, not
    // inferred.
    EndpointRoute::admin(HttpMethod::Get, "/b/storage/admin", Route::AdminOverview)
        .summary("Storage admin overview"),
    EndpointRoute::admin(HttpMethod::Get, "/b/storage/admin/", Route::AdminOverview)
        .summary("Storage admin overview"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/storage/admin/buckets",
        Route::AdminBucketsPage,
    )
    .summary("All buckets (admin)"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/storage/admin/shares",
        Route::AdminSharesPage,
    )
    .summary("All shares (admin)"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/storage/admin/quotas",
        Route::AdminQuotasPage,
    )
    .summary("Quotas (admin)"),
    // ── Admin JSON API ── reached until this PR through the admin block,
    // which rewrote `/b/admin/api/storage/...` and
    // `/b/admin/api/cloudstorage/...` to synthetic paths and forwarded them
    // via `call_block`. They live under the files prefixes now, gated `Admin`
    // by the router from these rows. Never rate-limited (the delegation
    // returned before the per-user preamble), and still not.
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/storage/admin/api/buckets",
        Route::AdminListBuckets,
    )
    .summary("List every bucket (admin)"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/storage/admin/api/stats",
        Route::AdminStats,
    )
    .summary("Storage totals (admin)"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/cloudstorage/admin/shares",
        Route::AdminListShares,
    )
    .summary("Recent shares, all users (admin)"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/cloudstorage/admin/access-logs",
        Route::AdminAccessLogs,
    )
    .summary("Share access logs (admin)"),
    EndpointRoute::admin(
        HttpMethod::Get,
        "/b/cloudstorage/admin/quotas",
        Route::AdminListQuotas,
    )
    .summary("Per-user quotas (admin)"),
    // PATCH is what clients send; `update` is the action both PUT and PATCH
    // map to, which is what the old delegated arm matched.
    EndpointRoute::admin(
        HttpMethod::Patch,
        "/b/cloudstorage/admin/quotas/{id}",
        Route::AdminUpdateQuota,
    )
    .summary("Set a user's quota (admin)"),
    // ── Public share link ── `share::handle_direct_access` verifies the
    // token's signature, rate-limits per remote IP, and enforces expiry and
    // the access cap itself.
    EndpointRoute::public(
        HttpMethod::Get,
        "/b/storage/direct/{token}",
        Route::DirectAccess,
    )
    .summary("Access shared file"),
    // ── User storage JSON API ──
    EndpointRoute::authenticated(
        HttpMethod::Get,
        "/b/storage/api/buckets",
        Route::ListBuckets,
    )
    .summary("List buckets"),
    EndpointRoute::authenticated(
        HttpMethod::Post,
        "/b/storage/api/buckets",
        Route::CreateBucket,
    )
    .summary("Create bucket"),
    // Never declared before this PR; `storage/search.rs` scopes both by
    // `msg.user_id()`.
    EndpointRoute::authenticated(HttpMethod::Get, "/b/storage/api/search", Route::Search)
        .summary("Search objects"),
    EndpointRoute::authenticated(HttpMethod::Get, "/b/storage/api/recent", Route::Recent)
        .summary("Recently viewed objects"),
    // The object rows bind `{key...}`: keys contain `/`, and dispatch has
    // always matched the rest of the path. The declaration used to say
    // `{key}`, a template no nested key could match.
    //
    // These are the two highest-value developer endpoints for browsing a
    // bucket; full schema coverage of the remaining storage routes (buckets,
    // shares, quotas) is a follow-up.
    //
    // No output schema on the download: the success response is the raw
    // object body (`Content-Type` set from the stored object's MIME type),
    // not JSON — see `handle_get_object`'s
    // `ResponseBuilder::new().body(data, &info.content_type)`. The path
    // schema alone surfaces the request shape in `/openapi.json` without
    // mislabeling the response as `application/json`.
    EndpointRoute::authenticated(
        HttpMethod::Get,
        "/b/storage/api/buckets/{name}/objects/{key...}",
        Route::GetObject,
    )
    .summary("Download file")
    .description("Returns the raw object bytes with the stored Content-Type — not a JSON envelope.")
    .path_params(object_path_schema)
    .tags(&["storage"]),
    EndpointRoute::authenticated(
        HttpMethod::Delete,
        "/b/storage/api/buckets/{name}/objects/{key...}",
        Route::DeleteObject,
    )
    .summary("Delete file"),
    EndpointRoute::authenticated(
        HttpMethod::Get,
        "/b/storage/api/buckets/{name}/objects",
        Route::ListObjects,
    )
    .summary("List objects")
    .path_params(list_objects_path_schema)
    .query_params(list_objects_query_schema)
    .output(response_schema_of::<contracts::ObjectListResponse>)
    .tags(&["storage"]),
    // No input schema: `handle_upload_object` accepts either a raw body
    // (programmatic clients) or a `multipart/form-data` envelope (browser
    // `FormData` uploads) — see `crate::multipart` and the `is_multipart`
    // branch in `storage/objects.rs`. Neither shape is a JSON body, so there
    // is no `T` to derive a request schema from; a JSON Schema here would
    // misdescribe what the endpoint accepts.
    EndpointRoute::authenticated(
        HttpMethod::Post,
        "/b/storage/api/buckets/{name}/objects",
        Route::UploadObject,
    )
    .summary("Upload file"),
    // Never declared before this PR; `storage/buckets.rs` refuses a bucket
    // the caller does not own (`is_bucket_access_denied`).
    EndpointRoute::authenticated(
        HttpMethod::Delete,
        "/b/storage/api/buckets/{name}",
        Route::DeleteBucket,
    )
    .summary("Delete bucket"),
    // ── User cloud-storage JSON API ── never declared before this PR;
    // `cloud.rs` lists, creates and reads quota for `msg.user_id()` and
    // refuses to delete another user's share.
    EndpointRoute::authenticated(HttpMethod::Get, "/b/cloudstorage/shares", Route::ListShares)
        .summary("List my share links"),
    EndpointRoute::authenticated(
        HttpMethod::Post,
        "/b/cloudstorage/shares",
        Route::CreateShare,
    )
    .summary("Create a share link"),
    EndpointRoute::authenticated(
        HttpMethod::Delete,
        "/b/cloudstorage/shares/{id}",
        Route::DeleteShare,
    )
    .summary("Delete a share link"),
    EndpointRoute::authenticated(HttpMethod::Get, "/b/cloudstorage/quota", Route::GetQuota)
        .summary("My quota and usage"),
    // ── User SSR pages ── the two generic bucket rows last (see above).
    EndpointRoute::authenticated(HttpMethod::Get, "/b/storage/", Route::BucketListPage)
        .summary("Bucket list (user)"),
    EndpointRoute::authenticated(HttpMethod::Get, "/b/cloudstorage/", Route::CloudStoragePage)
        .summary("Shares + quota page"),
    EndpointRoute::authenticated(
        HttpMethod::Get,
        "/b/storage/{bucket}/",
        Route::ObjectListPage,
    )
    .summary("Object list"),
    EndpointRoute::authenticated(
        HttpMethod::Get,
        "/b/storage/{bucket}/{prefix...}/",
        Route::FolderListPage,
    )
    .summary("Object list (nested)"),
];

/// Whether a matched route runs the per-user preamble in `handle`: a session
/// is required (belt and braces under the router's `Authenticated` gate) and
/// the per-user rate limit is spent. `false` for the public share link, which
/// limits itself per remote IP inside `share::handle_direct_access`, and for
/// the admin rows, which the router gates `Admin` from the declaration and
/// which were never rate-limited. Exhaustive so a new row is a decision, not
/// an omission; `table_tests` pins it to the declared level.
const fn user_preamble(route: Route) -> bool {
    match route {
        Route::AdminOverview
        | Route::AdminBucketsPage
        | Route::AdminSharesPage
        | Route::AdminQuotasPage
        | Route::AdminListBuckets
        | Route::AdminStats
        | Route::AdminListShares
        | Route::AdminAccessLogs
        | Route::AdminListQuotas
        | Route::AdminUpdateQuota
        | Route::DirectAccess => false,
        Route::BucketListPage
        | Route::ObjectListPage
        | Route::FolderListPage
        | Route::CloudStoragePage
        | Route::ListBuckets
        | Route::CreateBucket
        | Route::DeleteBucket
        | Route::ListObjects
        | Route::UploadObject
        | Route::GetObject
        | Route::DeleteObject
        | Route::Search
        | Route::Recent
        | Route::ListShares
        | Route::CreateShare
        | Route::DeleteShare
        | Route::GetQuota => true,
    }
}

/// Path-parameter schema for `GET /b/storage/api/buckets/{name}/objects`.
///
/// Hand-written (the same call shape as `products::mod::info`'s
/// `id_path_schema`): `storage::params::extract_bucket_name` reads
/// `msg.var("name")` by name and `handle_list_objects` reads `msg.query(..)`
/// / `msg.pagination_params(..)` by name, so nothing here deserializes into a
/// struct. A struct declared only to feed `request_schema_of::<T>` would have
/// no runtime user and would generate a byte-identical parameter list.
fn list_objects_path_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["name"],
        "properties": {
            "name": {"type": "string", "description": "Bucket name"}
        }
    })
}

/// Query-parameter schema for `GET /b/storage/api/buckets/{name}/objects`;
/// hand-written for the reason given on [`list_objects_path_schema`].
fn list_objects_query_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "prefix": {"type": "string", "description": "Key prefix filter"},
            "page": {"type": "integer", "default": 1},
            "page_size": {"type": "integer", "default": 50, "maximum": 100}
        }
    })
}

/// Path-parameter schema for the `{name}/objects/{key...}` rows. The
/// parameter is `key`; the template's `...` marks that it binds the rest of
/// the path, since keys contain `/`.
fn object_path_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["name", "key"],
        "properties": {
            "name": {"type": "string", "description": "Bucket name"},
            "key": {"type": "string", "description": "Object key (may contain '/')"}
        }
    })
}

crate::impresspress_feature_block! {
    /// File storage: buckets, objects, shares, quotas (`impresspress/files`).
    pub struct FilesBlock;
    fields: { limiter: UserRateLimiter },
    name: "impresspress/files",
    info: |_this| {
        use wafer_run::CollectionSchema;

        BlockInfo::new("impresspress/files", "0.0.1", "http-handler@v1", "File storage, sharing, quotas, and access logging")
            .instance_mode(InstanceMode::Singleton)
            .requires(vec!["wafer-run/database".into(), "wafer-run/storage".into(), "wafer-run/config".into()])
            // No explicit Storage grant needed. Wave 26 (c18) made WRAP
            // namespace-aware for Storage; this block self-admits its
            // own `impresspress/files/*` namespace via Rule 3.
            // Advisory table list — admin "Database tables" discovery + the
            // WRAP grant-UI read only `CollectionSchema::name`. The schema
            // itself (columns, indexes, FKs, quota defaults) lives solely in
            // the block's hand-authored `migrations/*.sqlite.sql` files (the
            // single source for both runtime `migrations::apply()` and the
            // Cloudflare D1 build).
            .collections(vec![
                CollectionSchema::new(repo::buckets::TABLE),
                CollectionSchema::new(repo::objects::TABLE),
                CollectionSchema::new(repo::views::TABLE),
                CollectionSchema::new(repo::shares::TABLE),
                CollectionSchema::new(repo::shares::ACCESS_LOGS_TABLE),
                CollectionSchema::new(repo::quota::TABLE),
            ])
            .category(wafer_run::BlockCategory::Feature)
            .description("File storage and management with bucket-based organization. Supports file upload, download, deletion, search, and sharing via public links with expiration and access counting. Includes per-user storage quotas.")
            .endpoints(endpoint_match::declare(ROUTES))
            .admin_url("/b/storage/admin/")
            .can_disable(true)
    },
    handle: |this, ctx, msg, input| {
        // Auth is enforced centrally by `route_to_block` from each row's
        // declared `AuthLevel`; the matcher binds the path variables the
        // handlers read through `msg.var(..)`.
        let Some(route) = endpoint_match::dispatch(&mut msg, ROUTES) else {
            return err_not_found("not found");
        };

        if user_preamble(route) {
            // Belt and braces under the router's `Authenticated` gate.
            if msg.user_id().is_empty() {
                return err_unauthorized("Authentication required");
            }
            // Per-user rate limiting. `create` (upload) gets its own bucket;
            // `retrieve`/everything-else fall back to the read/write split.
            // The Allowed(headers) outcome is discarded here: attaching
            // X-RateLimit-* to a streaming response would need platform-side
            // middleware to inject the headers after the handler returns its
            // OutputStream.
            if let RateLimitOutcome::Limited(r) = check_user_rate_limit_with(
                &this.limiter,
                ctx,
                &msg,
                Some((RateLimit::UPLOAD, "upload")),
            )
            .await
            {
                return r;
            }
        }

        match route {
            Route::AdminOverview => pages_admin::overview(ctx, &msg).await,
            Route::AdminBucketsPage => pages_admin::buckets(ctx, &msg).await,
            Route::AdminSharesPage => pages_admin::shares(ctx, &msg).await,
            Route::AdminQuotasPage => pages_admin::quotas(ctx, &msg).await,
            Route::AdminListBuckets => storage::handle_list_buckets(ctx, &msg).await,
            Route::AdminStats => storage::handle_stats(ctx, &msg).await,
            Route::AdminListShares => cloud::handle_admin_list_shares(ctx, &msg).await,
            Route::AdminAccessLogs => cloud::handle_access_logs(ctx, &msg).await,
            Route::AdminListQuotas => cloud::handle_admin_quotas(ctx, &msg).await,
            Route::AdminUpdateQuota => cloud::handle_update_quota(ctx, &msg, input).await,
            Route::DirectAccess => share::handle_direct_access(ctx, &msg, &this.limiter).await,
            Route::BucketListPage => pages_user::bucket_list_page(ctx, &msg).await,
            Route::ObjectListPage => {
                pages_user::object_list_page(ctx, &msg, msg.var("bucket"), "").await
            }
            Route::FolderListPage => {
                // The bound prefix carries no trailing slash; the page's
                // prefix convention is `dir/`.
                let prefix = format!("{}/", msg.var("prefix"));
                pages_user::object_list_page(ctx, &msg, msg.var("bucket"), &prefix).await
            }
            Route::CloudStoragePage => pages_user::cloudstorage_page(ctx, &msg).await,
            Route::ListBuckets => storage::handle_list_buckets(ctx, &msg).await,
            Route::CreateBucket => storage::handle_create_bucket(ctx, &msg, input).await,
            Route::DeleteBucket => storage::handle_delete_bucket(ctx, &msg).await,
            Route::ListObjects => storage::handle_list_objects(ctx, &msg).await,
            Route::UploadObject => storage::handle_upload_object(ctx, &msg, input).await,
            Route::GetObject => storage::handle_get_object(ctx, &msg).await,
            Route::DeleteObject => storage::handle_delete_object(ctx, &msg).await,
            Route::Search => storage::handle_search(ctx, &msg).await,
            Route::Recent => storage::handle_recent(ctx, &msg).await,
            Route::ListShares => cloud::handle_list_shares(ctx, &msg).await,
            Route::CreateShare => cloud::handle_create_share(ctx, &msg, input).await,
            Route::DeleteShare => cloud::handle_delete_share(ctx, &msg).await,
            Route::GetQuota => cloud::handle_get_quota(ctx, &msg).await,
        }
    },
    lifecycle: |_this, ctx, event| {
        crate::migration_helper::lifecycle_init(
            ctx,
            &event,
            "impresspress/files",
            migrations::SQLITE_MIGRATIONS,
            migrations::POSTGRES_MIGRATIONS,
        )
        .await
    },
}

#[cfg(test)]
mod schema_tests {
    use super::{migrations::SQLITE_MIGRATIONS, models::QuotaConfig};

    /// The quota column defaults in the migration SQL (now the single schema
    /// source) must match the `QuotaConfig` consts. If you change a const,
    /// change `migrations/001_initial_schema.*.sql` too (and remember
    /// `IMPRESSPRESS_RUN_MIGRATIONS=1`).
    #[test]
    fn quota_sql_defaults_match_quota_config_consts() {
        let sql = SQLITE_MIGRATIONS
            .iter()
            .map(|(_, s)| *s)
            .collect::<Vec<_>>()
            .join("\n");

        let asserts: &[(&str, i64)] = &[
            ("max_storage_bytes", QuotaConfig::DEFAULT_MAX_STORAGE_BYTES),
            (
                "max_file_size_bytes",
                QuotaConfig::DEFAULT_MAX_FILE_SIZE_BYTES,
            ),
            (
                "max_files_per_bucket",
                QuotaConfig::DEFAULT_MAX_FILES_PER_BUCKET,
            ),
            ("reset_period_days", QuotaConfig::DEFAULT_RESET_PERIOD_DAYS),
        ];

        for (column, expected) in asserts {
            // Match the `<column> ... DEFAULT <value>` line in the DDL.
            let line = sql
                .lines()
                .find(|l| l.trim_start().starts_with(column))
                .unwrap_or_else(|| panic!("column {column} declared in migration SQL"));
            let needle = format!("DEFAULT {expected}");
            assert!(
                line.contains(&needle),
                "column {column}: migration SQL `{line}` must carry `{needle}` to match \
                 QuotaConfig::{}",
                column.to_uppercase(),
            );
        }
    }
}

#[cfg(test)]
mod grant_tests {
    use wafer_run::{Block, ResourceType};

    use super::FilesBlock;

    #[test]
    fn files_block_does_not_declare_typed_storage() {
        // Wave 26 (c18): files block doesn't need a typed Storage grant
        // for its own namespace because Rule 3 self-admit covers it. A
        // grant here would also be redundant for *cross-block* access:
        // any block that wants to expose its storage to files declares
        // the grant from its own side.
        let files = FilesBlock::new();
        let grants = files.info().grants;

        let typed_storage = grants
            .iter()
            .find(|g| g.resource_type == Some(ResourceType::Storage));

        assert!(
            typed_storage.is_none(),
            "files block must not declare a typed Storage grant — own-namespace \
             access is covered by WRAP Rule 3 self-admit (Wave 26 / c18). \
             Cross-block grants belong on the owning block's side. (got: {typed_storage:?})",
        );
    }
}

#[cfg(test)]
mod test_support {
    use wafer_run::Message;

    /// Run `msg` through the block's own route table so `{name}`, `{key}`,
    /// `{id}`, `{token}`, `{bucket}` and `{prefix}` are bound the way they
    /// are on the wire, then hand the message to a handler directly. Panics
    /// when no row matches: a test that sends an unroutable path would
    /// otherwise exercise the handler's "missing id" branch by accident.
    pub(super) fn routed(mut msg: Message) -> Message {
        let route = crate::endpoint_match::dispatch(&mut msg, super::ROUTES);
        assert!(
            route.is_some(),
            "no files route matches {} {}",
            msg.action(),
            msg.path()
        );
        msg
    }
}

#[cfg(test)]
mod table_tests {
    use wafer_run::{AuthLevel, Block as _, Message};

    use super::*;
    use crate::{endpoint_match::endpoint_auth, test_support::anon_msg};

    /// `info().endpoints` is generated from `ROUTES`; nothing else declares
    /// an endpoint for this block.
    #[test]
    fn info_endpoints_come_from_the_table() {
        let declared = FilesBlock::new().info().endpoints;
        assert_eq!(declared.len(), ROUTES.len());
        for (ep, row) in declared.iter().zip(ROUTES) {
            assert_eq!(ep.method, row.method, "{}", row.template);
            assert_eq!(ep.path, row.template);
            assert_eq!(ep.auth, row.auth, "{}", row.template);
        }
    }

    fn resolve(action: &str, path: &str) -> (Option<Route>, Message) {
        let mut msg = anon_msg(action, path);
        let route = endpoint_match::dispatch(&mut msg, ROUTES);
        (route, msg)
    }

    /// `(action, path, expected route, bound variables)`.
    type Case<'a> = (&'a str, &'a str, Route, &'a [(&'a str, &'a str)]);

    fn assert_resolves(cases: &[Case<'_>]) {
        for (action, path, expected, vars) in cases {
            let (route, msg) = resolve(action, path);
            assert_eq!(route, Some(*expected), "{action} {path}");
            for (name, value) in *vars {
                assert_eq!(msg.var(name), *value, "{action} {path} binds {name}");
            }
        }
    }

    /// The thirteen rows this PR declares for paths the block already served
    /// (four cloud-storage and three storage-API rows the block never
    /// declared, six admin rows moved from the admin block's delegation),
    /// each resolving to its handler with its path variable bound.
    #[test]
    fn every_new_row_dispatches_to_its_handler() {
        assert_resolves(&[
            ("retrieve", "/b/cloudstorage/shares", Route::ListShares, &[]),
            ("create", "/b/cloudstorage/shares", Route::CreateShare, &[]),
            (
                "delete",
                "/b/cloudstorage/shares/s-1",
                Route::DeleteShare,
                &[("id", "s-1")],
            ),
            ("retrieve", "/b/cloudstorage/quota", Route::GetQuota, &[]),
            ("retrieve", "/b/storage/api/search", Route::Search, &[]),
            ("retrieve", "/b/storage/api/recent", Route::Recent, &[]),
            (
                "delete",
                "/b/storage/api/buckets/photos",
                Route::DeleteBucket,
                &[("name", "photos")],
            ),
            (
                "retrieve",
                "/b/cloudstorage/admin/shares",
                Route::AdminListShares,
                &[],
            ),
            (
                "retrieve",
                "/b/cloudstorage/admin/access-logs",
                Route::AdminAccessLogs,
                &[],
            ),
            (
                "retrieve",
                "/b/cloudstorage/admin/quotas",
                Route::AdminListQuotas,
                &[],
            ),
            (
                "update",
                "/b/cloudstorage/admin/quotas/u-1",
                Route::AdminUpdateQuota,
                &[("id", "u-1")],
            ),
            (
                "retrieve",
                "/b/storage/admin/api/buckets",
                Route::AdminListBuckets,
                &[],
            ),
            (
                "retrieve",
                "/b/storage/admin/api/stats",
                Route::AdminStats,
                &[],
            ),
        ]);
    }

    /// Every path the old `handle` chain, the `/b/storage/api` sub-table and
    /// `cloud::handle` served resolves to a row, with the variables the
    /// handlers read bound. Nested keys and nested folder prefixes keep their
    /// slashes; the bare index paths reach their rows through the matcher's
    /// trailing-slash retry.
    #[test]
    fn every_path_the_block_served_resolves_to_a_row() {
        assert_resolves(&[
            ("retrieve", "/b/storage/admin", Route::AdminOverview, &[]),
            ("retrieve", "/b/storage/admin/", Route::AdminOverview, &[]),
            (
                "retrieve",
                "/b/storage/admin/buckets",
                Route::AdminBucketsPage,
                &[],
            ),
            (
                "retrieve",
                "/b/storage/admin/shares",
                Route::AdminSharesPage,
                &[],
            ),
            (
                "retrieve",
                "/b/storage/admin/quotas",
                Route::AdminQuotasPage,
                &[],
            ),
            (
                "retrieve",
                "/b/storage/direct/tok-1",
                Route::DirectAccess,
                &[("token", "tok-1")],
            ),
            ("retrieve", "/b/storage", Route::BucketListPage, &[]),
            ("retrieve", "/b/storage/", Route::BucketListPage, &[]),
            (
                "retrieve",
                "/b/storage/photos/",
                Route::ObjectListPage,
                &[("bucket", "photos")],
            ),
            (
                "retrieve",
                "/b/storage/photos/nested/",
                Route::FolderListPage,
                &[("bucket", "photos"), ("prefix", "nested")],
            ),
            (
                "retrieve",
                "/b/storage/photos/nested/deep/",
                Route::FolderListPage,
                &[("bucket", "photos"), ("prefix", "nested/deep")],
            ),
            (
                "retrieve",
                "/b/storage/photos/my%20files/",
                Route::FolderListPage,
                &[("bucket", "photos"), ("prefix", "my files")],
            ),
            ("retrieve", "/b/cloudstorage", Route::CloudStoragePage, &[]),
            ("retrieve", "/b/cloudstorage/", Route::CloudStoragePage, &[]),
            (
                "retrieve",
                "/b/storage/api/buckets",
                Route::ListBuckets,
                &[],
            ),
            ("create", "/b/storage/api/buckets", Route::CreateBucket, &[]),
            (
                "retrieve",
                "/b/storage/api/buckets/photos/objects",
                Route::ListObjects,
                &[("name", "photos")],
            ),
            (
                "create",
                "/b/storage/api/buckets/photos/objects",
                Route::UploadObject,
                &[("name", "photos")],
            ),
            (
                "retrieve",
                "/b/storage/api/buckets/photos/objects/nested/b.png",
                Route::GetObject,
                &[("name", "photos"), ("key", "nested/b.png")],
            ),
            (
                "delete",
                "/b/storage/api/buckets/photos/objects/nested/b.png",
                Route::DeleteObject,
                &[("name", "photos"), ("key", "nested/b.png")],
            ),
        ]);
    }

    /// The per-user preamble (a session is required, the per-user rate limit
    /// is spent) runs for exactly the rows declared `Authenticated`. The
    /// public share link limits itself by IP inside its handler; the admin
    /// rows are gated by the router from the declaration and were never
    /// rate-limited.
    #[test]
    fn user_preamble_follows_the_declared_level() {
        for row in ROUTES {
            assert_eq!(
                user_preamble(row.handler),
                row.auth == AuthLevel::Authenticated,
                "{} {}",
                row.method,
                row.template
            );
        }
    }

    /// What the router enforces for each new row, resolved from the
    /// declaration alone through the same `endpoint_auth` it calls. The
    /// `/b/storage/` and `/b/cloudstorage/` prefix rows are `Public`, so the
    /// declared level is the whole gate for the admin JSON API that used to
    /// sit behind the admin block's `Admin` prefix.
    #[test]
    fn declared_levels_gate_the_router() {
        let eps = FilesBlock::new().info().endpoints;
        for (action, path) in [
            ("retrieve", "/b/cloudstorage/admin/shares"),
            ("retrieve", "/b/cloudstorage/admin/access-logs"),
            ("retrieve", "/b/cloudstorage/admin/quotas"),
            ("update", "/b/cloudstorage/admin/quotas/u-1"),
            ("retrieve", "/b/storage/admin/api/buckets"),
            ("retrieve", "/b/storage/admin/api/stats"),
            ("retrieve", "/b/storage/admin"),
            ("retrieve", "/b/storage/admin/"),
        ] {
            assert_eq!(
                endpoint_auth(&eps, action, path),
                Some(AuthLevel::Admin),
                "{action} {path}"
            );
        }
        assert_eq!(
            endpoint_auth(&eps, "retrieve", "/b/storage/direct/tok-1"),
            Some(AuthLevel::Public)
        );
        for (action, path) in [
            ("retrieve", "/b/cloudstorage/shares"),
            ("create", "/b/cloudstorage/shares"),
            ("delete", "/b/cloudstorage/shares/s-1"),
            ("retrieve", "/b/cloudstorage/quota"),
            ("retrieve", "/b/storage/api/search"),
            ("retrieve", "/b/storage/api/recent"),
            ("delete", "/b/storage/api/buckets/photos"),
            ("retrieve", "/b/storage/photos/nested/"),
            (
                "retrieve",
                "/b/storage/api/buckets/photos/objects/nested/b.png",
            ),
        ] {
            assert_eq!(
                endpoint_auth(&eps, action, path),
                Some(AuthLevel::Authenticated),
                "{action} {path}"
            );
        }
        // The slash variant of an admin page matches only the folder row; it
        // was gated `Authenticated` by the fail-closed default before and must
        // not become more permissive now that a row covers it.
        assert!(
            matches!(
                endpoint_auth(&eps, "retrieve", "/b/storage/admin/buckets/"),
                Some(AuthLevel::Authenticated | AuthLevel::Admin)
            ),
            "/b/storage/admin/buckets/ must stay at least Authenticated"
        );
    }
}

#[cfg(test)]
mod handle_tests {
    use wafer_run::{Block as _, InputStream};

    use super::*;
    use crate::test_support::{admin_msg, anon_msg, output_is_error, output_json, TestContext};

    /// The stats endpoint the admin block used to reach by rewriting
    /// `/b/admin/api/storage/stats` to a synthetic path and forwarding it
    /// is served under the files prefix.
    #[tokio::test]
    async fn admin_stats_are_served_under_the_files_prefix() {
        let ctx = TestContext::with_files().await;
        let out = FilesBlock::new()
            .handle(
                &ctx,
                admin_msg("retrieve", "/b/storage/admin/api/stats"),
                InputStream::empty(),
            )
            .await;
        let body = output_json(out).await;
        assert_eq!(
            body.get("bucket_count").and_then(|v| v.as_i64()),
            Some(0),
            "{body}"
        );
    }

    /// The quota update reads `{id}` as the table bound it, end to end.
    #[tokio::test]
    async fn admin_quota_update_reads_the_bound_user_id() {
        let ctx = TestContext::with_files().await;
        let body = serde_json::to_vec(&serde_json::json!({ "max_storage_bytes": 5 })).unwrap();
        let out = FilesBlock::new()
            .handle(
                &ctx,
                admin_msg("update", "/b/cloudstorage/admin/quotas/u-9"),
                InputStream::from_bytes(body),
            )
            .await;
        // The handler publishes the typed `repo::quota::QuotaRow`, so
        // `user_id` is a field of the row rather than of a nested `data` map.
        let row = output_json(out).await;
        assert_eq!(row["user_id"], serde_json::json!("u-9"), "{row}");
        let quota = quota::get_user_quota(&ctx, "u-9").await.expect("quota row");
        assert_eq!(quota.max_storage_bytes, 5);
    }

    /// The share link is the block's one public row, and its handler reads
    /// the `{token}` the row binds. A bogus token is read, fails signature
    /// verification and answers `NotFound`; a handler reading any other
    /// variable name would see an empty token and answer `InvalidArgument`
    /// ("Missing share token") instead, which is what this distinguishes.
    #[tokio::test]
    async fn share_link_handler_reads_the_bound_token() {
        let ctx = TestContext::with_files().await;
        let out = FilesBlock::new()
            .handle(
                &ctx,
                anon_msg("retrieve", "/b/storage/direct/bogus"),
                InputStream::empty(),
            )
            .await;
        assert!(output_is_error(out, "NotFound").await);
    }

    /// The router gates the user rows `Authenticated` from the declaration;
    /// the block keeps refusing an anonymous caller itself as belt and braces.
    #[tokio::test]
    async fn anonymous_user_rows_are_refused_by_the_block_itself() {
        let ctx = TestContext::with_files().await;
        let out = FilesBlock::new()
            .handle(
                &ctx,
                anon_msg("retrieve", "/b/cloudstorage/quota"),
                InputStream::empty(),
            )
            .await;
        assert!(output_is_error(out, "Unauthenticated").await);
    }
}
