//! `impresspress/dev` — the browser development sandbox's control plane.
//!
//! The block owns the publication ledger (generations, builds, the activation
//! journal), validates and activates what the `/b/dev` page produces, and
//! publishes the resulting site into `wafer-run/web/site`. It does **not** own
//! the runtime: building and swapping a `Wafer` is the host's job, reached
//! through the [`control::RuntimeControl`] seam.
//!
//! # Why this block is hand-written
//!
//! Every other impresspress feature block goes through
//! [`crate::impresspress_feature_block!`], which generates a zero-argument
//! `new()`. This block's constructor takes the shared state that carries the
//! `RuntimeControl` handle, so the struct and `impl Block` are written out in
//! the same shape the macro would produce (as `impresspress/llm` already is).
//!
//! # Why it is not in the block manifest
//!
//! Registration happens from the consumer, via `ImpresspressBuilder::extra_block`
//! and `add_route`, gated on `impresspress-web`'s `resolve_dev_active` — the
//! `browser-devtools` feature AND the `initialize({ dev })` request, never a
//! stored variable.
//! `feature_block_manifest!` only enumerates blocks whose constructors take no
//! arguments, and — more to the point — the sandbox's security model
//! (design §13) depends on this block being absent from every normal
//! deployment, not merely disabled in one.

pub mod activation;
pub mod artifacts;
pub mod blobs;
pub mod blocks_api;
pub mod contracts;
pub mod control;
pub mod files;
pub mod generation;
pub mod generations_api;
pub mod migrations;
pub mod paths;
pub mod publisher;
pub mod repo;
pub mod seed;
pub mod status;
pub mod validation;
pub mod workspace;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use std::sync::Arc;

use wafer_run::{
    context::Context, AuthLevel, Block, BlockEndpoint, BlockInfo, CollectionSchema, HttpMethod,
    InputStream, InstanceMode, LifecycleEvent, Message, OutputStream, WaferError,
};

pub use self::control::{
    DynamicBlockSpec, DynamicRoute, RouteAccessKind, RuntimeControl, ValidationFailure,
    ValidationStage,
};
use crate::{
    endpoint_match::{self, EndpointRoute},
    http::ResponseBuilder,
};

/// Registered block name.
pub const BLOCK_NAME: &str = "impresspress/dev";

/// The single route prefix the block serves. Registered as one
/// [`crate::routing::ExtraRoute`] at `Admin`, so the router — not any handler
/// in this module — is what keeps the sandbox admin-only.
pub const ROUTE_PREFIX: &str = "/b/dev";

/// `wafer_guest.rs` ABI version the block scaffolder currently writes.
/// Published in the status response so the page can tell whether a block it
/// compiled earlier was built against a stale guest shim.
pub const WAFER_GUEST_VERSION: u32 = 1;

/// In-block dispatch targets, one per declared HTTP endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// `GET /b/dev/api/status`
    ApiStatus,
    /// `GET /b/dev/api/files`
    ApiFilesList,
    /// `POST /b/dev/api/files/read`
    ApiFilesRead,
    /// `POST /b/dev/api/files/write`
    ApiFilesWrite,
    /// `POST /b/dev/api/files/delete`
    ApiFilesDelete,
    /// `GET /b/dev/api/generations`
    ApiGenerations,
    /// `GET /b/dev/api/generations/{id}`
    ApiGenerationDetail,
    /// `POST /b/dev/api/generations/{id}/rollback`
    ApiGenerationRollback,
    /// `POST /b/dev/api/builds/stage`
    ApiBuildStage,
    /// `POST /b/dev/api/blocks/{name}/remove`
    ApiBlockRemove,
}

/// Method + path-template dispatch table, mirroring `info().endpoints`.
///
/// Reading and deleting are `POST`s, not a `GET` with a query and a `DELETE`
/// with one: a workspace path is a `/`-separated string with its own
/// separators, and putting it in the URL would mean every client had to
/// percent-encode it correctly to name a file in a subdirectory. The path
/// travels in the JSON body, where it needs no encoding at all.
pub const ROUTES: &[EndpointRoute<Route>] = &[
    EndpointRoute::new(HttpMethod::Get, "/b/dev/api/status", Route::ApiStatus),
    EndpointRoute::new(HttpMethod::Get, "/b/dev/api/files", Route::ApiFilesList),
    EndpointRoute::new(
        HttpMethod::Post,
        "/b/dev/api/files/read",
        Route::ApiFilesRead,
    ),
    EndpointRoute::new(
        HttpMethod::Post,
        "/b/dev/api/files/write",
        Route::ApiFilesWrite,
    ),
    EndpointRoute::new(
        HttpMethod::Post,
        "/b/dev/api/files/delete",
        Route::ApiFilesDelete,
    ),
    EndpointRoute::new(
        HttpMethod::Get,
        "/b/dev/api/generations",
        Route::ApiGenerations,
    ),
    EndpointRoute::new(
        HttpMethod::Get,
        "/b/dev/api/generations/{id}",
        Route::ApiGenerationDetail,
    ),
    EndpointRoute::new(
        HttpMethod::Post,
        "/b/dev/api/generations/{id}/rollback",
        Route::ApiGenerationRollback,
    ),
    EndpointRoute::new(
        HttpMethod::Post,
        "/b/dev/api/builds/stage",
        Route::ApiBuildStage,
    ),
    EndpointRoute::new(
        HttpMethod::Post,
        "/b/dev/api/blocks/{name}/remove",
        Route::ApiBlockRemove,
    ),
];

/// A response builder pre-seeded with `Cache-Control: no-store`.
///
/// Design §12 requires every `/b/dev` response to be `no-store`. Seeding it at
/// construction — rather than post-filtering the returned `OutputStream` — is
/// what makes that unconditional: an `OutputStream`'s meta is fixed when it is
/// built, so a wrapper would have to buffer the whole (possibly streaming)
/// response to change it.
pub(crate) fn no_store() -> ResponseBuilder {
    ResponseBuilder::new().set_header("Cache-Control", "no-store")
}

/// The error-terminal counterpart of [`no_store`].
///
/// An error terminal is still a response on the wire: the HTTP boundary turns
/// `WaferError::meta` into headers before rendering the
/// `{"error", "message"}` body (`wafer_block::http_codec`'s
/// `collect_http_response`). So a refusal from this block can carry the header
/// too, which is what keeps "every `/b/dev` response is `no-store`" true for
/// the 404 as well as the 200 — a cached 404 for a route a later generation
/// adds would look permanently missing.
///
/// The one deliberate exception is [`crate::http::err_internal`]: it owns the
/// correlation-id logging and message sanitizing that must not be reproduced
/// here, and returns an already-sealed `OutputStream`. A 5xx is not cached by
/// any client without explicit headers, so the block keeps the shared
/// sanitizer rather than hand-rolling a header-carrying copy of it.
pub(crate) fn no_store_error(code: wafer_run::ErrorCode, message: &str) -> OutputStream {
    OutputStream::error(no_store_wafer_error(code, message))
}

/// [`no_store_error`] for a refusal whose HTTP status is not the one its
/// [`wafer_run::ErrorCode`] maps to.
///
/// The quota refusals in [`files`] are `413`s, and `ErrorCode` has no
/// payload-too-large member. `wafer_block::http_codec::resolve_error_status`
/// lets an explicit `resp.status` on the error's meta win over the
/// code-derived default, which is how a status outside the enum is expressed
/// without inventing a code the rest of the runtime would not understand.
pub(crate) fn no_store_error_status(
    code: wafer_run::ErrorCode,
    status: u16,
    message: &str,
) -> OutputStream {
    let mut error = no_store_wafer_error(code, message);
    error.meta.push(wafer_run::MetaEntry {
        key: wafer_block::meta::META_RESP_STATUS.to_string(),
        value: status.to_string(),
    });
    OutputStream::error(error)
}

/// A [`WaferError`] carrying the block-wide `Cache-Control: no-store`.
fn no_store_wafer_error(code: wafer_run::ErrorCode, message: &str) -> WaferError {
    let mut error = WaferError::new(code, message);
    error.meta.push(wafer_run::MetaEntry {
        key: format!(
            "{}Cache-Control",
            wafer_block::meta::META_RESP_HEADER_PREFIX
        ),
        value: "no-store".to_string(),
    });
    error
}

/// The WRAP grants the sandbox needs beyond its own namespace.
///
/// The dev block's tables (`impresspress__dev__*`) self-admit under the
/// own-namespace rule, and its blobs and artifacts live under its own storage
/// prefix. The one resource it must be *granted* is the published site, which
/// `wafer-run/web` owns — so this grant cannot be declared in
/// `BlockInfo::grants` (a block may only grant what it owns) and is handed to
/// the runtime by whoever registers the block.
pub fn wrap_grants() -> Vec<wafer_run::ResourceGrant> {
    vec![
        wafer_run::ResourceGrant::read_write(BLOCK_NAME, "wafer-run/web/site/*")
            .typed(wafer_run::ResourceType::Storage),
    ]
}

/// State shared by every `/b/dev` handler.
///
/// Held behind an `Arc` because the activation queue is shared mutable state
/// that outlives any one request — and outlives the runtime an activation
/// rebuilds.
pub struct DevShared {
    /// The host's half of activation: builds and swaps the live runtime.
    pub control: Arc<dyn RuntimeControl>,
    /// The one serialized path from a desired state to a live one.
    ///
    /// On the shared state rather than on [`DevBlock`] because activation is
    /// what re-instantiates the block: a queue owned by the block would be
    /// dropped halfway through the operation that dropped it.
    pub activation: activation::ActivationQueue,
    /// Serializes the read-modify-write of `workspace.json`.
    ///
    /// Every workspace mutation is load-whole-manifest, change one entry,
    /// save-whole-manifest. Two of those interleaving between the load and the
    /// save would each save a manifest built from a snapshot that predates the
    /// other, and the later save would drop the earlier writer's entry — even
    /// though both passed their own per-path `expected_sha256` check, because
    /// that check is about the path each writer names and says nothing about
    /// the rest of the file.
    ///
    /// An **async** mutex, because the critical section spans storage calls;
    /// a `std::sync::Mutex` cannot be held across an `await` at all. It guards
    /// the manifest only: it is released before the activation the mutation
    /// requests, so ordinary editing never waits behind a publish.
    pub workspace: futures::lock::Mutex<()>,
    /// Serializes every change to the *block* set.
    ///
    /// A block change is resolved as a whole desired set —
    /// `ActivationIntent::BlockSet` carries every block the generation runs,
    /// not a delta — from the set that is active when the request is made.
    /// Two of those interleaving between their read and their
    /// `activation::request` would each compose a set from a snapshot that
    /// predates the other, and the later one would drop the earlier block
    /// without anything having failed.
    ///
    /// Design §6.6 allows one compile at a time, and this is what upholds it.
    /// A future that lifts the restriction has to carry the block *delta* in
    /// the intent instead; the lock is the cheaper half of that trade while
    /// the sandbox has one agent and one page.
    ///
    /// An **async** mutex for the same reason [`Self::workspace`] is: the
    /// critical section spans storage, the ledger and the runtime rebuild. It
    /// is a separate lock because it guards a different thing — a site write
    /// must never wait behind a compile, and `ActivationIntent::SiteOnly`
    /// composes its block half at dequeue precisely so it does not have to.
    pub compile: futures::lock::Mutex<()>,
}

impl DevShared {
    /// Build the shared state around a [`RuntimeControl`] handle.
    ///
    /// `arc_with_non_send_sync`: [`RuntimeControl`] is bounded on
    /// `MaybeSend + MaybeSync`, which is unbounded on wasm32 — that is what
    /// lets the browser control hold the live `Rc<Wafer>` and its factory. On
    /// wasm32 the resulting `Arc` therefore is not `Send`/`Sync`, and on that
    /// single-threaded target it does not need to be; on native the same
    /// bounds are `Send + Sync` and the lint does not fire at all. The same
    /// allowance the block registration path already carries.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new(control: Arc<dyn RuntimeControl>) -> Arc<Self> {
        Arc::new(Self {
            control,
            activation: activation::ActivationQueue::new(),
            workspace: futures::lock::Mutex::new(()),
            compile: futures::lock::Mutex::new(()),
        })
    }
}

/// The browser development sandbox control plane (`impresspress/dev`).
pub struct DevBlock {
    shared: Arc<DevShared>,
}

impl DevBlock {
    /// Construct a [`DevBlock`] over shared state.
    pub fn new(shared: Arc<DevShared>) -> Self {
        Self { shared }
    }
}

#[wafer_block::wafer_async_trait]
impl Block for DevBlock {
    fn info(&self) -> BlockInfo {
        BlockInfo::new(
            BLOCK_NAME,
            "0.1.0",
            "http-handler@v1",
            "Browser development sandbox control plane",
        )
        .instance_mode(InstanceMode::Singleton)
        .requires(vec![
            "wafer-run/database".into(),
            "wafer-run/storage".into(),
            "wafer-run/config".into(),
        ])
        // Advisory table list — the schema itself lives solely in
        // `migrations/001_dev_schema.{sqlite,postgres}.sql`.
        .collections(vec![
            CollectionSchema::new(repo::generations::TABLE),
            CollectionSchema::new(repo::builds::TABLE),
            CollectionSchema::new(repo::runtime_state::TABLE),
        ])
        .category(wafer_run::BlockCategory::Feature)
        .endpoints(vec![
            BlockEndpoint::get("/b/dev/api/status")
                .summary("Sandbox status")
                .auth(AuthLevel::Admin)
                .output::<contracts::StatusResponse>(),
            BlockEndpoint::get("/b/dev/api/files")
                .summary("List workspace files")
                .auth(AuthLevel::Admin)
                .query_params::<contracts::FileListQuery>()
                .output::<contracts::FileListResponse>(),
            BlockEndpoint::post("/b/dev/api/files/read")
                .summary("Read a workspace file")
                .auth(AuthLevel::Admin)
                .input::<contracts::FileReadRequest>()
                .output::<contracts::FileReadResponse>(),
            BlockEndpoint::post("/b/dev/api/files/write")
                .summary("Write a workspace file")
                .auth(AuthLevel::Admin)
                .input::<contracts::FileWriteRequest>()
                .output::<contracts::FileWriteResponse>(),
            BlockEndpoint::post("/b/dev/api/files/delete")
                .summary("Delete a workspace file")
                .auth(AuthLevel::Admin)
                .input::<contracts::FileDeleteRequest>()
                .output::<contracts::FileDeleteResponse>(),
            BlockEndpoint::get("/b/dev/api/generations")
                .summary("List generations")
                .auth(AuthLevel::Admin)
                .query_params::<contracts::GenerationListQuery>()
                .output::<contracts::GenerationListResponse>(),
            BlockEndpoint::get("/b/dev/api/generations/{id}")
                .summary("Read one generation")
                .auth(AuthLevel::Admin)
                .path_params::<contracts::GenerationPathParams>()
                .output::<contracts::GenerationDetail>(),
            BlockEndpoint::post("/b/dev/api/generations/{id}/rollback")
                .summary("Republish an earlier generation")
                .auth(AuthLevel::Admin)
                .path_params::<contracts::GenerationPathParams>()
                .output::<contracts::ActivationResponse>(),
            BlockEndpoint::post("/b/dev/api/builds/stage")
                .summary("Stage and activate a compiled block")
                .auth(AuthLevel::Admin)
                .input::<contracts::StageBuildRequest>()
                .output::<contracts::StageBuildResponse>(),
            BlockEndpoint::post("/b/dev/api/blocks/{name}/remove")
                .summary("Remove a block from the runtime")
                .auth(AuthLevel::Admin)
                .path_params::<contracts::BlockPathParams>()
                .output::<contracts::ActivationResponse>(),
        ])
        .admin_url(ROUTE_PREFIX)
        // The sandbox is registered only where it is meant to exist; a
        // deployment turns it off by not building it, not by an admin toggle
        // that would leave a half-live control plane behind.
        .can_disable(false)
    }

    async fn handle(
        &self,
        ctx: &dyn Context,
        mut msg: Message,
        input: InputStream,
    ) -> OutputStream {
        let Some(route) = endpoint_match::dispatch(&mut msg, ROUTES) else {
            return no_store_error(wafer_run::ErrorCode::NotFound, "endpoint not found");
        };
        // Every route is `RouteAccess::Admin` at the router; handlers do not
        // re-check the caller's role.
        match route {
            Route::ApiStatus => status::handle(ctx, &self.shared).await,
            Route::ApiFilesList => files::handle_list(ctx, &msg).await,
            Route::ApiFilesRead => files::handle_read(ctx, input).await,
            Route::ApiFilesWrite => files::handle_write(ctx, &self.shared, input).await,
            Route::ApiFilesDelete => files::handle_delete(ctx, &self.shared, input).await,
            Route::ApiGenerations => generations_api::handle_list(ctx, &msg).await,
            Route::ApiGenerationDetail => generations_api::handle_detail(ctx, &msg).await,
            Route::ApiGenerationRollback => {
                generations_api::handle_rollback(ctx, &self.shared, &msg).await
            }
            Route::ApiBuildStage => blocks_api::handle_stage(ctx, &self.shared, input).await,
            Route::ApiBlockRemove => blocks_api::handle_remove(ctx, &self.shared, &msg).await,
        }
    }

    async fn lifecycle(&self, ctx: &dyn Context, event: LifecycleEvent) -> Result<(), WaferError> {
        crate::migration_helper::lifecycle_init(
            ctx,
            &event,
            BLOCK_NAME,
            migrations::SQLITE_MIGRATIONS,
            migrations::POSTGRES_MIGRATIONS,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        control::{RouteAccessKind, ValidationStage},
        repo::{
            builds::BuildStatus,
            generations::{GenerationCause, GenerationStatus},
            runtime_state::ActivationPhase,
        },
        *,
    };

    // Every closed enum below must round-trip through `as_str` / `parse`,
    // **and** `as_str` must be the same spelling serde uses.
    //
    // The two halves are the point: `as_str` is what the SQL `CHECK`
    // constraints and the stored columns see, serde is what the HTTP
    // contracts and the generation manifest see. A value with two spellings
    // is exactly the implicit mapping layer this repo forbids, and it would
    // stay invisible until a stored row failed to load or a manifest failed
    // to match.

    #[test]
    fn activation_phase_spellings_agree() {
        for phase in [
            ActivationPhase::Idle,
            ActivationPhase::Validating,
            ActivationPhase::BuildingRuntime,
            ActivationPhase::Publishing,
            ActivationPhase::Active,
            ActivationPhase::Failed,
        ] {
            assert_eq!(ActivationPhase::parse(phase.as_str()), Some(phase));
            assert_eq!(
                serde_json::to_value(phase).expect("serialize"),
                serde_json::json!(phase.as_str()),
            );
        }
        assert_eq!(ActivationPhase::parse("Idle"), None);
        assert_eq!(ActivationPhase::parse(""), None);
    }

    #[test]
    fn generation_status_spellings_agree() {
        for status in [
            GenerationStatus::Staged,
            GenerationStatus::Validating,
            GenerationStatus::Activating,
            GenerationStatus::Active,
            GenerationStatus::Failed,
            GenerationStatus::Superseded,
        ] {
            assert_eq!(GenerationStatus::parse(status.as_str()), Some(status));
            assert_eq!(
                serde_json::to_value(status).expect("serialize"),
                serde_json::json!(status.as_str()),
            );
        }
        assert_eq!(GenerationStatus::parse("archived"), None);
    }

    #[test]
    fn generation_cause_spellings_agree_and_classify_rebuilds() {
        for cause in [
            GenerationCause::SiteWrite,
            GenerationCause::SiteDelete,
            GenerationCause::BlockCompile,
            GenerationCause::BlockRemove,
            GenerationCause::Rollback,
            GenerationCause::Seed,
        ] {
            assert_eq!(GenerationCause::parse(cause.as_str()), Some(cause));
            assert_eq!(
                serde_json::to_value(cause).expect("serialize"),
                serde_json::json!(cause.as_str()),
            );
        }
        // Site edits republish without touching the runtime (design §7.2);
        // everything else changes the block set.
        assert!(!GenerationCause::SiteWrite.rebuilds_runtime());
        assert!(!GenerationCause::SiteDelete.rebuilds_runtime());
        assert!(GenerationCause::BlockCompile.rebuilds_runtime());
        assert!(GenerationCause::BlockRemove.rebuilds_runtime());
        assert!(GenerationCause::Rollback.rebuilds_runtime());
        assert!(GenerationCause::Seed.rebuilds_runtime());
    }

    #[test]
    fn build_status_and_validation_stage_spellings_agree() {
        for status in [
            BuildStatus::Staged,
            BuildStatus::Valid,
            BuildStatus::Invalid,
        ] {
            assert_eq!(BuildStatus::parse(status.as_str()), Some(status));
            assert_eq!(
                serde_json::to_value(status).expect("serialize"),
                serde_json::json!(status.as_str()),
            );
        }
        for stage in [
            ValidationStage::Load,
            ValidationStage::Info,
            ValidationStage::Init,
            ValidationStage::Start,
            ValidationStage::Probe,
        ] {
            assert_eq!(ValidationStage::parse(stage.as_str()), Some(stage));
            assert_eq!(
                serde_json::to_value(stage).expect("serialize"),
                serde_json::json!(stage.as_str()),
            );
        }
    }

    #[test]
    fn route_access_kind_matches_the_manifest_spelling_and_the_router_tier() {
        // Design §11.3 writes `"access": "Public"` — PascalCase, unlike the
        // snake_case status/cause columns. Pin it: a rename would silently
        // invalidate every stored manifest.
        for (kind, spelling, tier) in [
            (
                RouteAccessKind::Public,
                "Public",
                crate::routing::RouteAccess::Public,
            ),
            (
                RouteAccessKind::Authenticated,
                "Authenticated",
                crate::routing::RouteAccess::Authenticated,
            ),
            (
                RouteAccessKind::Admin,
                "Admin",
                crate::routing::RouteAccess::Admin,
            ),
        ] {
            assert_eq!(kind.as_str(), spelling);
            assert_eq!(RouteAccessKind::parse(spelling), Some(kind));
            assert_eq!(
                serde_json::to_value(kind).expect("serialize"),
                serde_json::json!(spelling),
            );
            assert_eq!(kind.to_route_access(), tier);
        }
    }

    #[test]
    fn wrap_grants_cover_only_the_published_site() {
        let grants = wrap_grants();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].grantee, BLOCK_NAME);
        assert_eq!(grants[0].resource, "wafer-run/web/site/*");
        assert!(grants[0].write);
        // Typed to Storage: an untyped grant would also admit a database
        // collection or config key that happened to match the pattern.
        assert_eq!(
            grants[0].resource_type,
            Some(wafer_run::ResourceType::Storage)
        );
    }

    #[tokio::test]
    async fn every_response_the_block_builds_is_no_store() {
        let buf = no_store()
            .json(&serde_json::json!({}))
            .collect_buffered()
            .await
            .expect("respond");
        assert_eq!(
            wafer_run::MetaGet::get(&buf.meta, "resp.header.Cache-Control"),
            Some("no-store"),
        );
    }
}
