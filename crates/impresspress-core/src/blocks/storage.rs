//! Impresspress storage block wrapper.
//!
//! Wraps the wafer-core `StorageBlock` to add:
//! - Per-block path isolation (each block gets its own storage namespace)
//! - Cross-block access control via WRAP grants (default deny)
//! - Storage access logging
//!
//! ## Isolation model
//!
//! Each block's storage is namespaced under its block name:
//! - `wafer-run/web` calling `store::get(ctx, "public", "key")` → `wafer-run/web/public/key`
//! - `impresspress/files` calling `store::put(ctx, "uploads", ...)` → `impresspress/files/uploads/...`
//!
//! ## Cross-block access
//!
//! Blocks can request access to another block's namespace by prefixing the
//! folder with `@`:
//! - `store::get(ctx, "@wafer-run/web/public", "key")` → cross-block read of `wafer-run/web/public/key`
//!
//! Cross-block access is **denied by default** and requires a WRAP grant with
//! `resource_type = Storage` matching the target resource — the object path
//! (`wafer-run/web/public/key`) for object ops, the folder for folder ops.
//!
//! A folder or key with an empty, `.` or `..` segment is refused (and
//! audited as `BLOCKED: path traversal`) before either check runs.

use std::sync::Arc;

use futures::StreamExt;
use wafer_block::{codec, stream::StreamEvent, wire::storage as wire, ResourceAccess, ServiceOp};
use wafer_core::{clients::database as db, interfaces::storage::service::StorageService};
use wafer_run::{
    context::Context, Block, BlockInfo, ErrorCode, InputStream, LifecycleEvent, Message,
    OutputStream, ResourceGrant, ResourceType, WaferError,
};

use super::admin::STORAGE_ACCESS_LOGS_TABLE;
use crate::util::{json_map, now_millis};

/// A storage block that enforces per-block path isolation and WRAP-based
/// cross-block access control.
pub struct ImpresspressStorageBlock {
    inner: wafer_core::service_blocks::storage::StorageBlock,
    /// WRAP grants for cross-block storage access checks.
    /// Updated after runtime startup via `update_wrap_grants()`.
    wrap_grants: std::sync::RwLock<Vec<ResourceGrant>>,
    /// The admin block ID (has full storage access).
    wrap_admin_block: Arc<str>,
}

impl ImpresspressStorageBlock {
    pub fn new(service: Arc<dyn StorageService>, admin_block: Arc<str>) -> Self {
        Self {
            inner: wafer_core::service_blocks::storage::StorageBlock::new(service),
            wrap_grants: std::sync::RwLock::new(Vec::new()),
            wrap_admin_block: admin_block,
        }
    }

    /// Update the WRAP grants used for cross-block access checks.
    /// Called after runtime startup once grants are collected.
    pub fn update_wrap_grants(&self, grants: &[ResourceGrant]) {
        // Recover from poison: the data inside is still valid — the only
        // reason this lock would be poisoned is a panic in a previous
        // writer, and we're replacing the whole `Vec` anyway.
        let mut g = self.wrap_grants.write().unwrap_or_else(|e| e.into_inner());
        *g = grants.to_vec();
    }

    /// The grants [`Self::update_wrap_grants`] last installed. Test-only: it
    /// exists so `builder::boot`'s tests can pin that the closing step of the
    /// funnel actually ran, which is the step a target used to be able to
    /// forget.
    #[cfg(test)]
    pub(crate) fn installed_wrap_grants(&self) -> Vec<ResourceGrant> {
        self.wrap_grants
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Validate that a block name is safe for use as a storage path prefix.
/// Only allows `[a-z0-9-_]` per segment, `/` as separator. No `..`, no dots, no empty segments.
fn is_safe_block_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    for segment in name.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return false;
        }
        if !segment
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return false;
        }
    }
    true
}

/// Result of resolving a storage path.
#[derive(Debug)]
struct ResolvedPath {
    /// The actual storage path after resolution.
    path: String,
    /// Whether this is a cross-block access (folder started with `@`).
    cross_block: bool,
    /// The value the downstream wafer-core handler's SEC-003
    /// cross-validation expects in `wrap.resource` meta. Equals `path`
    /// for folder ops (list, create_folder, delete_folder); equals
    /// `format!("{path}/{key}")` for object ops (put, get, delete).
    wrap_resource: String,
}

/// Resolve a folder name: own-namespace prefixing or cross-block via `@` prefix.
/// Sets `wrap_resource = path` by default; callers handling object ops
/// (put/get/delete) post-process to include the object key.
fn resolve_folder(caller: &str, folder: &str) -> ResolvedPath {
    let (path, cross_block) = if let Some(absolute) = folder.strip_prefix('@') {
        (absolute.to_string(), true)
    } else if folder.is_empty() {
        (caller.to_string(), false)
    } else {
        (format!("{caller}/{folder}"), false)
    };
    ResolvedPath {
        wrap_resource: path.clone(),
        path,
        cross_block,
    }
}

/// Determine access type from the storage operation kind.
fn access_type_for_op(kind: &str) -> &'static str {
    match kind {
        // `get_streaming` is the streaming form of `get` — the same read of
        // `{folder}/{key}`, so it is classified as a read here too. Left out,
        // it fell through to "write" and a cross-block download would have
        // been checked against a WRITE grant.
        ServiceOp::STORAGE_GET
        | ServiceOp::STORAGE_GET_STREAMING
        | ServiceOp::STORAGE_LIST
        | ServiceOp::STORAGE_LIST_FOLDERS => "read",
        // Everything else — including any op added upstream after this was
        // written — is classified as a write, which is the fail-closed answer:
        // it demands the stricter grant rather than silently admitting a
        // mutation under a read grant.
        _ => "write",
    }
}

/// A `wire::storage` request whose path field needs namespace rewriting.
///
/// Implemented by every op carrying a folder/name field: object ops
/// (put/get/delete) expose their object `key` for the SEC-003 `wrap_resource`
/// composite; folder/list ops return `None`, leaving `wrap_resource = path`.
trait PathRewrite: serde::Serialize + serde::de::DeserializeOwned {
    /// Mutable access to the request's path field (`folder` or `name`).
    fn path_field_mut(&mut self) -> &mut String;
    /// The object key for the `wrap_resource = "{path}/{key}"` composite, or
    /// `None` for ops with no object key (folder ops, list).
    fn wrap_key(&self) -> Option<&str> {
        None
    }
}

impl PathRewrite for wire::PutRequest {
    fn path_field_mut(&mut self) -> &mut String {
        &mut self.folder
    }
    fn wrap_key(&self) -> Option<&str> {
        Some(&self.key)
    }
}
impl PathRewrite for wire::GetRequest {
    fn path_field_mut(&mut self) -> &mut String {
        &mut self.folder
    }
    fn wrap_key(&self) -> Option<&str> {
        Some(&self.key)
    }
}
impl PathRewrite for wire::DeleteRequest {
    fn path_field_mut(&mut self) -> &mut String {
        &mut self.folder
    }
    fn wrap_key(&self) -> Option<&str> {
        Some(&self.key)
    }
}
impl PathRewrite for wire::ListRequest {
    fn path_field_mut(&mut self) -> &mut String {
        &mut self.folder
    }
}
impl PathRewrite for wire::CreateFolderRequest {
    fn path_field_mut(&mut self) -> &mut String {
        &mut self.name
    }
}
impl PathRewrite for wire::DeleteFolderRequest {
    fn path_field_mut(&mut self) -> &mut String {
        &mut self.name
    }
}

/// Decode `body` as `T`, namespace-resolve its path field, recompute the
/// SEC-003 `wrap_resource` (object ops include the object key; folder/list ops
/// keep `wrap_resource = path`), re-encode, and return the rewritten bytes.
///
/// One generic body shared by every path-carrying op — keeps the per-op
/// `wrap_resource` rule in exactly one place. (Deliberately uncounted: an arm
/// added to [`rewrite_request_body`] must not be able to re-stale this line.)
fn rewrite_op<T: PathRewrite>(
    body: &[u8],
    caller: &str,
) -> Result<(Vec<u8>, ResolvedPath), WaferError> {
    let mut req: T = codec::decode(body).map_err(|e: wafer_block::WaferError| {
        WaferError::new(
            ErrorCode::InvalidArgument,
            format!("invalid storage request: {}", e.message),
        )
    })?;
    let mut resolved = resolve_folder(caller, req.path_field_mut());
    if let Some(key) = req.wrap_key() {
        resolved.wrap_resource = format!("{}/{}", resolved.path, key);
    }
    *req.path_field_mut() = resolved.path.clone();
    let bytes = codec::encode(&req).map_err(|e: wafer_block::WaferError| {
        WaferError::new(
            ErrorCode::Internal,
            format!("encoding storage request: {}", e.message),
        )
    })?;
    Ok((bytes, resolved))
}

/// Answer to `storage.put_streaming`: recognised, and deliberately not served.
///
/// `wafer_core::clients::storage::put_stream` frames its request as a
/// `PutStreamingHeader` chunk (folder / key / content_type) followed by the
/// raw body chunks. This shim buffers its whole input
/// (`input.collect_to_bytes()`) and rewrites ONE decoded request struct, so
/// arming `put_streaming` as another [`rewrite_op`] arm would do two wrong
/// things at once: decode the header-plus-body concatenation as a single
/// `wire::` request (garbage), and buffer the very upload the caller chose the
/// streaming op to avoid buffering. Forwarding it needs frame-preserving
/// rewriting this shim does not have.
///
/// So it is refused by name. A caller that reaches for it gets a message that
/// says what happened and what to do instead, rather than the
/// [`UNKNOWN_OP`] fallthrough that made `storage.get_streaming` look like a
/// backend fault for 60 PRs.
const PUT_STREAMING_UNSUPPORTED: &str = "storage.put_streaming is not served by the impresspress \
     storage shim: the shim buffers a request body to rewrite its folder into the caller's \
     namespace, and a streaming upload's PutStreamingHeader framing needs frame-preserving \
     support it does not have. Use storage.put (wafer_core::clients::storage::put) until this \
     shim can forward frames.";

/// Answer to a `storage.*` op this shim does not know at all, prefixed onto
/// the op name. Named because the enumeration guard
/// (`shim_answers_every_upstream_storage_op`) recognises the fallthrough by
/// it rather than by a re-typed literal.
const UNKNOWN_OP: &str = "unknown storage op: ";

/// Rewrite the folder/name field in the request body bytes.
///
/// Returns the rewritten body bytes plus the resolved path info.
///
/// Bodies are MessagePack-encoded `wire::storage` request types (matching
/// the binary transport overhaul). We decode the relevant typed request,
/// rewrite the path field, and re-encode — no `serde_json::Value`
/// round-trip, because that would lose the byte-fidelity guarantees
/// needed for the schema-locked wire types (and silently strip
/// non-string-encodable fields like `PutRequest.data`).
fn rewrite_request_body(
    kind: &str,
    body: &[u8],
    caller: &str,
) -> Result<(Vec<u8>, ResolvedPath), WaferError> {
    match kind {
        // No folder field to rewrite — handled by filtering results.
        // list_folders has no `wrap.resource` cross-check in the wafer-core
        // handler, so wrap_resource is unused; populate it for consistency.
        ServiceOp::STORAGE_LIST_FOLDERS => Ok((
            body.to_vec(),
            ResolvedPath {
                wrap_resource: caller.to_string(),
                path: caller.to_string(),
                cross_block: false,
            },
        )),
        ServiceOp::STORAGE_CREATE_FOLDER => rewrite_op::<wire::CreateFolderRequest>(body, caller),
        ServiceOp::STORAGE_DELETE_FOLDER => rewrite_op::<wire::DeleteFolderRequest>(body, caller),
        ServiceOp::STORAGE_PUT => rewrite_op::<wire::PutRequest>(body, caller),
        ServiceOp::STORAGE_GET => rewrite_op::<wire::GetRequest>(body, caller),
        // `storage.get_streaming` is `storage.get`'s streaming twin, not a
        // separate capability: `wafer-core`'s handler decodes it as the SAME
        // `wire::GetRequest` and authorizes the SAME `{folder}/{key}` read
        // (`interfaces/storage/handler.rs`, `ServiceOp::STORAGE_GET_STREAMING`),
        // and `StorageService::get_streaming` has a default that calls `get`
        // and wraps the buffered body as a single-chunk stream — so every
        // backend answers it. The folder rewriting therefore applies
        // unchanged.
        //
        // Omitting it made this shim answer `unknown storage op` to the only
        // op the block's two download paths issue (`store::get_stream`, from
        // `blocks/files/storage/objects.rs` and `blocks/files/share.rs`), so
        // EVERY object download and every share link 500'd on the native
        // backend. The callers stream deliberately — a multi-GB object must
        // not be buffered into the isolate — so the missing arm is the bug,
        // not the streaming.
        ServiceOp::STORAGE_GET_STREAMING => rewrite_op::<wire::GetRequest>(body, caller),
        // `storage.put_streaming` is the one upstream op with no rewrite: see
        // [`PUT_STREAMING_UNSUPPORTED`] for why arming it here would be worse
        // than refusing it.
        ServiceOp::STORAGE_PUT_STREAMING => Err(WaferError::new(
            ErrorCode::Unimplemented,
            PUT_STREAMING_UNSUPPORTED,
        )),
        ServiceOp::STORAGE_DELETE => rewrite_op::<wire::DeleteRequest>(body, caller),
        ServiceOp::STORAGE_LIST => rewrite_op::<wire::ListRequest>(body, caller),
        other => Err(WaferError::new(
            ErrorCode::InvalidArgument,
            format!("{UNKNOWN_OP}{other}"),
        )),
    }
}

#[wafer_block::wafer_async_trait]
impl Block for ImpresspressStorageBlock {
    fn info(&self) -> BlockInfo {
        self.inner.info()
    }

    async fn handle(&self, ctx: &dyn Context, msg: Message, input: InputStream) -> OutputStream {
        let caller = ctx.caller_id().unwrap_or("unknown").to_string();

        // Validate caller name is safe for storage paths
        if caller != "unknown" && !is_safe_block_name(&caller) {
            return OutputStream::error(WaferError::new(
                ErrorCode::PermissionDenied,
                format!("block name '{caller}' is not safe for storage paths"),
            ));
        }

        let access = access_type_for_op(&msg.kind);
        let body = input.collect_to_bytes().await;

        // Rewrite folder/name in the request body, resolving own vs cross-block
        let (rewritten_body, resolved) = match rewrite_request_body(&msg.kind, &body, &caller) {
            Ok(r) => r,
            Err(e) => return OutputStream::error(e),
        };

        // Refuse a traversal shape before anything authorizes on it.
        // `wrap_resource` is the full resource the op touches — the resolved
        // folder for folder/list ops, `{folder}/{key}` for object ops — so
        // this one check covers the folder AND the object key. A path with an
        // empty, `.` or `..` segment is refused outright rather than
        // normalized: authorization is textual and prefix-based, so
        // `uploads/../../impresspress/auth/x` sits under the caller's own
        // namespace as text while naming another block's object. A segment
        // that merely contains dots (`a..b`) is a plain name and passes.
        if !wafer_block::wrap::is_traversal_safe_path(&resolved.wrap_resource) {
            let _ = log_storage_access(
                ctx,
                &caller,
                &msg.kind,
                &resolved.wrap_resource,
                "BLOCKED: path traversal".to_string(),
            )
            .await;
            return OutputStream::error(WaferError::new(
                ErrorCode::PermissionDenied,
                "storage path traversal not allowed",
            ));
        }

        // Cross-block access requires a WRAP grant with resource_type =
        // Storage covering the object itself, not just its folder: a grant
        // like `wafer-run/web/site/*` is written against object paths.
        //
        // The resource is passed `@`-prefixed because that marker is what
        // `check_access` reads as "cross-block": an unprefixed Storage
        // resource is the caller's own namespace to it and is admitted for
        // any attributable caller without looking at a grant.
        if resolved.cross_block {
            let access = if access == "write" {
                ResourceAccess::Write
            } else {
                ResourceAccess::Read
            };
            let grants = self
                .wrap_grants
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Err(e) = wafer_run::wrap::check_access(
                Some(&caller),
                &format!("@{}", resolved.wrap_resource),
                access,
                Some(&ResourceType::Storage),
                &grants,
                &self.wrap_admin_block,
            ) {
                let _ = log_storage_access(
                    ctx,
                    &caller,
                    &msg.kind,
                    &resolved.wrap_resource,
                    format!("BLOCKED: {}", e.message),
                )
                .await;
                return OutputStream::error(e);
            }
        }

        // SEC-003: keep wrap.resource meta in sync with the namespacing
        // rewrite so the downstream wafer-core handler's cross-validation
        // passes. The expected value depends on the op (folder vs
        // folder/key composite); rewrite_request_body computes the right
        // value per-op as `resolved.wrap_resource`. The WRAP grant check
        // at the call_block boundary has already validated the caller's
        // original wrap.resource against their grants; this is a
        // payload-meta sync, not a grant bypass.
        let mut msg = msg;
        msg.set_meta(
            wafer_block::meta::META_WRAP_RESOURCE,
            &resolved.wrap_resource,
        );

        // Execute the actual storage operation. Forward events to the caller
        // as they arrive — previously we drained the whole stream into a
        // `Vec<StreamEvent>` first, which defeated streaming and buffered
        // the entire body in memory. Frame boundaries are preserved because
        // we forward each event individually rather than `collect_buffered`
        // (which would concatenate header + body into a single chunk and
        // break downstream `buffered_header_and_body` decoding).
        let start = now_millis();
        let inner_out = self
            .inner
            .handle(ctx, msg.clone(), InputStream::from_bytes(rewritten_body))
            .await;
        let caller_log = caller.clone();
        let path_log = resolved.path.clone();
        let kind_log = msg.kind.clone();
        let ctx_arc = ctx.clone_arc();

        OutputStream::from_producer(move |sink, _cancel| async move {
            let mut inner = inner_out;
            // Best-effort terminal-success log shared by the Complete, Halt, and
            // stream-ended-without-terminal paths. (An `Error` event logs its
            // own `ERROR: …` status in-arm and returns, so it never reaches
            // these paths.)
            let log_ok = || {
                let status = format!("OK ({}ms)", (now_millis() - start) as i64);
                log_storage_access(ctx_arc.as_ref(), &caller_log, &kind_log, &path_log, status)
            };
            while let Some(evt) = inner.next().await {
                match evt {
                    StreamEvent::Chunk(bytes) => {
                        if sink.send_chunk(bytes).await.is_err() {
                            return;
                        }
                    }
                    StreamEvent::Meta(entry) => {
                        let _ = sink.send_meta(entry).await;
                    }
                    StreamEvent::Complete { meta } => {
                        let _ = log_ok().await;
                        let _ = sink.complete(meta).await;
                        return;
                    }
                    StreamEvent::Error(e) => {
                        let _ = log_storage_access(
                            ctx_arc.as_ref(),
                            &caller_log,
                            &kind_log,
                            &path_log,
                            format!("ERROR: {}", e.message),
                        )
                        .await;
                        let _ = sink.error(*e).await;
                        return;
                    }
                    StreamEvent::Drop => {
                        let _ = sink.drop_request().await;
                        return;
                    }
                    StreamEvent::Continue(m) => {
                        let _ = sink.continue_with(m).await;
                        return;
                    }
                    StreamEvent::Halt { body, meta } => {
                        let _ = log_ok().await;
                        let _ = sink.halt(body, meta).await;
                        return;
                    }
                }
            }
            // Stream ended without a terminal event — best-effort log.
            let _ = log_ok().await;
        })
    }

    async fn lifecycle(
        &self,
        ctx: &dyn Context,
        event: LifecycleEvent,
    ) -> std::result::Result<(), WaferError> {
        self.inner.lifecycle(ctx, event).await
    }
}

/// Log a storage access event (best-effort).
///
/// `status` is taken by value so callers in the streaming producer can hand
/// off an owned, formatted string without the returned future borrowing a
/// closure-local.
async fn log_storage_access(
    ctx: &dyn Context,
    source_block: &str,
    operation: &str,
    path: &str,
    status: String,
) -> Result<(), WaferError> {
    db::create(
        ctx,
        STORAGE_ACCESS_LOGS_TABLE,
        json_map(serde_json::json!({
            "source_block": source_block,
            "operation": operation,
            "path": path,
            "status": status,
        })),
    )
    .await
    .map(|_| ())
}

/// Create a new ImpresspressStorageBlock (caller must register it with the runtime).
///
/// After the runtime starts, call `update_wrap_grants()` to inject the
/// collected grants for cross-block access checks.
pub fn create(
    service: Arc<dyn StorageService>,
    admin_block: Arc<str>,
) -> Arc<ImpresspressStorageBlock> {
    Arc::new(ImpresspressStorageBlock::new(service, admin_block))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Unit tests for the pure path-rewriting functions.
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_safe_block_name() {
        assert!(is_safe_block_name("wafer-run/web"));
        assert!(is_safe_block_name("impresspress/wafer-run/auth"));
        assert!(is_safe_block_name("my_block"));
        assert!(is_safe_block_name("a/b/c"));

        assert!(!is_safe_block_name(""));
        assert!(!is_safe_block_name("../evil"));
        assert!(!is_safe_block_name("wafer-run/.."));
        assert!(!is_safe_block_name("wafer-run/.hidden"));
        assert!(!is_safe_block_name("UPPER/case"));
        assert!(!is_safe_block_name("has spaces/bad"));
        assert!(!is_safe_block_name("/leading-slash"));
        assert!(!is_safe_block_name("trailing/"));
    }

    #[test]
    fn test_resolve_folder_own_namespace() {
        let r = resolve_folder("wafer-run/web", "public");
        assert_eq!(r.path, "wafer-run/web/public");
        assert!(!r.cross_block);

        let r = resolve_folder("wafer-run/web", "");
        assert_eq!(r.path, "wafer-run/web");
        assert!(!r.cross_block);

        let r = resolve_folder("impresspress/files", "uploads");
        assert_eq!(r.path, "impresspress/files/uploads");
        assert!(!r.cross_block);
    }

    #[test]
    fn test_resolve_folder_cross_block() {
        let r = resolve_folder("impresspress/files", "@wafer-run/web/public");
        assert_eq!(r.path, "wafer-run/web/public");
        assert!(r.cross_block);

        let r = resolve_folder("impresspress/admin", "@impresspress/files/uploads");
        assert_eq!(r.path, "impresspress/files/uploads");
        assert!(r.cross_block);
    }

    #[test]
    fn test_access_type_for_op() {
        assert_eq!(access_type_for_op("storage.get"), "read");
        // The streaming download is a read, like its buffered twin —
        // otherwise a cross-block download is checked against a WRITE grant.
        assert_eq!(access_type_for_op("storage.get_streaming"), "read");
        assert_eq!(access_type_for_op("storage.list"), "read");
        assert_eq!(access_type_for_op("storage.list_folders"), "read");
        assert_eq!(access_type_for_op("storage.put"), "write");
        assert_eq!(access_type_for_op("storage.delete"), "write");
        assert_eq!(access_type_for_op("storage.create_folder"), "write");
        assert_eq!(access_type_for_op("storage.delete_folder"), "write");
    }

    #[test]
    fn test_rewrite_request_body_put() {
        let body = codec::encode(&wire::PutRequest {
            folder: "uploads".into(),
            key: "photo.jpg".into(),
            data: vec![],
            content_type: "image/jpeg".into(),
        })
        .unwrap();
        let (rewritten, resolved) =
            rewrite_request_body("storage.put", &body, "impresspress/files").unwrap();
        assert_eq!(resolved.path, "impresspress/files/uploads");
        assert!(!resolved.cross_block);

        let req: wire::PutRequest = codec::decode(&rewritten).unwrap();
        assert_eq!(req.folder, "impresspress/files/uploads");
    }

    #[test]
    fn test_rewrite_request_body_cross_block() {
        let body = codec::encode(&wire::GetRequest {
            folder: "@wafer-run/web/public".into(),
            key: "index.html".into(),
        })
        .unwrap();
        let (rewritten, resolved) =
            rewrite_request_body("storage.get", &body, "impresspress/files").unwrap();
        assert_eq!(resolved.path, "wafer-run/web/public");
        assert!(resolved.cross_block);

        let req: wire::GetRequest = codec::decode(&rewritten).unwrap();
        assert_eq!(req.folder, "wafer-run/web/public");
    }

    /// `storage.get_streaming` carries the same `wire::GetRequest` as
    /// `storage.get` and must be namespaced identically. Before the fix this
    /// arm was absent, so the shim answered `unknown storage op` and every
    /// object download / share link 500'd.
    #[test]
    fn test_rewrite_request_body_get_streaming() {
        let body = codec::encode(&wire::GetRequest {
            folder: "uploads".into(),
            key: "photo.jpg".into(),
        })
        .unwrap();

        let (rewritten, resolved) =
            rewrite_request_body("storage.get_streaming", &body, "impresspress/files")
                .expect("the streaming download must be a known op");

        assert_eq!(resolved.path, "impresspress/files/uploads");
        assert_eq!(
            resolved.wrap_resource,
            "impresspress/files/uploads/photo.jpg"
        );
        assert!(!resolved.cross_block);
        let req: wire::GetRequest = codec::decode(&rewritten).unwrap();
        assert_eq!(req.folder, "impresspress/files/uploads");
        assert_eq!(req.key, "photo.jpg");
    }

    #[test]
    fn test_rewrite_request_body_create_folder() {
        let body = codec::encode(&wire::CreateFolderRequest {
            name: "uploads".into(),
            public: false,
        })
        .unwrap();
        let (rewritten, resolved) =
            rewrite_request_body("storage.create_folder", &body, "impresspress/files").unwrap();
        assert_eq!(resolved.path, "impresspress/files/uploads");
        assert!(!resolved.cross_block);

        let req: wire::CreateFolderRequest = codec::decode(&rewritten).unwrap();
        assert_eq!(req.name, "impresspress/files/uploads");
    }

    /// SEC-003 regression — folder ops set `wrap_resource = path`;
    /// object ops (put/get/delete) set `wrap_resource = format!("{path}/{key}")`
    /// to match what the wafer-core storage handler's check_wrap_resource
    /// will compare the meta against.
    #[test]
    fn test_rewrite_request_body_wrap_resource_per_op() {
        // Folder op — wrap_resource == path
        let body = codec::encode(&wire::CreateFolderRequest {
            name: "smoke".into(),
            public: false,
        })
        .unwrap();
        let (_, resolved) =
            rewrite_request_body("storage.create_folder", &body, "impresspress/files").unwrap();
        assert_eq!(resolved.wrap_resource, "impresspress/files/smoke");

        // Object op — wrap_resource == path + "/" + key
        let body = codec::encode(&wire::PutRequest {
            folder: "smoke".into(),
            key: "a.png".into(),
            data: vec![],
            content_type: "image/png".into(),
        })
        .unwrap();
        let (_, resolved) =
            rewrite_request_body("storage.put", &body, "impresspress/files").unwrap();
        assert_eq!(resolved.wrap_resource, "impresspress/files/smoke/a.png");

        // Cross-block object op — wrap_resource uses the post-resolution path
        let body = codec::encode(&wire::GetRequest {
            folder: "@wafer-run/web/public".into(),
            key: "index.html".into(),
        })
        .unwrap();
        let (_, resolved) =
            rewrite_request_body("storage.get", &body, "impresspress/files").unwrap();
        assert_eq!(resolved.wrap_resource, "wafer-run/web/public/index.html");
        assert!(resolved.cross_block);

        // list_folders — no folder field; wrap_resource == caller
        let (_, resolved) =
            rewrite_request_body("storage.list_folders", &[], "impresspress/files").unwrap();
        assert_eq!(resolved.wrap_resource, "impresspress/files");
    }

    /// THE GUARD. Every `storage.*` op the upstream client can emit must get a
    /// deliberate answer from this shim — either a namespace rewrite, or a
    /// refusal this file wrote on purpose. Falling through to
    /// [`UNKNOWN_OP`] is not an answer; it is the shape of the outage this PR
    /// fixes.
    ///
    /// `storage.get_streaming` sat in that fallthrough while both of the files
    /// block's download paths issued it, so every object download and every
    /// share link 500'd through 60 green PRs. Nothing in the suite enumerated
    /// the op set, so nothing could see it.
    ///
    /// The enumeration is [`ServiceOp::STORAGE_OPS`] itself — the same list
    /// upstream builds its `storage@v1` action catalog from — so an op added
    /// to the client fails here instead of reaching a user as a 500.
    #[test]
    fn shim_answers_every_upstream_storage_op() {
        for op in ServiceOp::STORAGE_OPS {
            // The body is empty on purpose: what is pinned here is DISPATCH,
            // not decoding. A recognised op may rewrite (`list_folders`
            // ignores the body), may fail to decode an empty body, or may
            // refuse by name — the one answer it must never give is the
            // unknown-op fallthrough.
            if let Err(e) = rewrite_request_body(op, &[], "impresspress/files") {
                assert!(
                    !e.message.starts_with(UNKNOWN_OP),
                    "{op} falls through to the unknown-op arm ({}); \
                     every op in ServiceOp::STORAGE_OPS needs an arm — a \
                     rewrite, or a deliberate refusal that says why",
                    e.message,
                );
            }
        }
    }

    /// `storage.put_streaming` is the deliberate refusal the guard above
    /// accepts. Pinned by name and code so nobody "fixes" it into a
    /// [`rewrite_op`] arm, which would decode the header-plus-body framing as
    /// one request and buffer the upload the caller chose not to buffer.
    #[test]
    fn put_streaming_is_refused_deliberately_not_as_an_unknown_op() {
        let err = rewrite_request_body(ServiceOp::STORAGE_PUT_STREAMING, &[], "impresspress/files")
            .expect_err("the shim cannot forward a framed streaming upload");
        assert_eq!(err.code, ErrorCode::Unimplemented);
        assert_eq!(err.message, PUT_STREAMING_UNSUPPORTED);
        assert!(
            !err.message.starts_with(UNKNOWN_OP),
            "the refusal must not read as a fallthrough",
        );
    }

    /// A streaming upload is a WRITE, like its buffered twin — the fail-closed
    /// default, and the classification the refusal above is logged under.
    #[test]
    fn put_streaming_is_classified_as_a_write() {
        assert_eq!(
            access_type_for_op(ServiceOp::STORAGE_PUT_STREAMING),
            "write"
        );
    }

    /// Negative control for [`shim_answers_every_upstream_storage_op`]: the
    /// fallthrough still exists, so that test cannot pass by the arm having
    /// been deleted.
    #[test]
    fn an_op_outside_the_upstream_set_is_still_refused() {
        let err = rewrite_request_body("storage.teleport", &[], "impresspress/files")
            .expect_err("an op no upstream client emits must be refused");
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert_eq!(err.message, format!("{UNKNOWN_OP}storage.teleport"));
    }

    // -----------------------------------------------------------------------
    // The shim's own checks, driven through `Block::handle` over a real
    // object store and the admin schema its audit rows land in.
    // -----------------------------------------------------------------------

    mod shim {
        use std::sync::Arc;

        use wafer_core::interfaces::storage::service::StorageService as _;
        use wafer_run::{Block as _, ErrorCode, InputStream, Message, ResourceGrant, ResourceType};

        use super::super::{create, ImpresspressStorageBlock, STORAGE_ACCESS_LOGS_TABLE};
        use crate::test_support::{InMemoryStorageService, TestContext};

        const ADMIN: &str = "impresspress/admin";

        /// A context acting as `caller`, with the admin schema (the audit
        /// table) migrated and WRAP enforcing the admin block's own grants —
        /// the grant that lets any block write a storage access row.
        async fn ctx_as(caller: &str) -> TestContext {
            TestContext::with_admin().await.with_wrap(
                caller,
                Vec::new(),
                wafer_run::Block::info(&crate::blocks::admin::AdminBlock::new()).grants,
                ADMIN,
            )
        }

        fn shim(
            grants: &[ResourceGrant],
        ) -> (Arc<ImpresspressStorageBlock>, Arc<InMemoryStorageService>) {
            let store = Arc::new(InMemoryStorageService::new());
            let block = create(store.clone(), Arc::from(ADMIN));
            block.update_wrap_grants(grants);
            (block, store)
        }

        /// Send one encoded request through the shim; `Ok(body)` on success.
        async fn send<T: serde::Serialize>(
            block: &ImpresspressStorageBlock,
            ctx: &TestContext,
            op: &str,
            req: &T,
        ) -> Result<Vec<u8>, wafer_run::WaferError> {
            let body = wafer_block::codec::encode(req).expect("encode request");
            block
                .handle(ctx, Message::new(op), InputStream::from_bytes(body))
                .await
                .collect_buffered()
                .await
                .map(|r| r.body)
                .map_err(wafer_block::WaferError::from)
        }

        fn put(folder: &str, key: &str) -> wafer_block::wire::storage::PutRequest {
            wafer_block::wire::storage::PutRequest {
                folder: folder.into(),
                key: key.into(),
                data: b"payload".to_vec(),
                content_type: "text/plain".into(),
            }
        }

        /// `(path, status)` of every storage access row, oldest first.
        async fn audit_rows(ctx: &TestContext) -> Vec<(String, String)> {
            crate::db_read::list_every(ctx, STORAGE_ACCESS_LOGS_TABLE, Vec::new())
                .await
                .expect("read storage access logs")
                .into_iter()
                .map(|r| {
                    let field = |k: &str| {
                        r.data
                            .get(k)
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string()
                    };
                    (field("path"), field("status"))
                })
                .collect()
        }

        /// A `..` in the object KEY is traversal, and it is the shim's to
        /// refuse and audit: the folder alone (`impresspress/files/uploads`)
        /// is clean, so a check on the folder never sees it. Left to
        /// wafer-core, the request died as an `InvalidArgument` with no
        /// "BLOCKED" row in the audit log the admin storage page reads.
        #[tokio::test]
        async fn a_traversal_key_is_refused_and_audited_by_the_shim() {
            let ctx = ctx_as("impresspress/files").await;
            let (block, store) = shim(&[]);

            let err = send(
                &block,
                &ctx,
                "storage.put",
                &put("uploads", "../../impresspress/auth/x"),
            )
            .await
            .expect_err("a traversal key must be refused");

            assert_eq!(err.code, ErrorCode::PermissionDenied, "{}", err.message);
            assert_eq!(
                audit_rows(&ctx).await,
                vec![(
                    "impresspress/files/uploads/../../impresspress/auth/x".to_string(),
                    "BLOCKED: path traversal".to_string(),
                )],
            );
            assert!(store.ops().is_empty(), "nothing may reach the store");
        }

        /// Empty and `.` segments are traversal shapes too: authorization
        /// is textual, and nothing downstream normalizes them.
        #[tokio::test]
        async fn empty_and_dot_segments_are_refused_by_the_shim() {
            let ctx = ctx_as("impresspress/files").await;
            let (block, store) = shim(&[]);

            for (folder, key) in [
                ("uploads", ""),
                ("uploads", "a//b"),
                ("uploads", "./x"),
                ("uploads/.", "x"),
                ("uploads/", "x"),
            ] {
                let err = send(&block, &ctx, "storage.put", &put(folder, key))
                    .await
                    .expect_err("a traversal shape must be refused");
                assert_eq!(
                    err.code,
                    ErrorCode::PermissionDenied,
                    "folder {folder:?} key {key:?}: {}",
                    err.message,
                );
            }
            let rows = audit_rows(&ctx).await;
            assert_eq!(rows.len(), 5, "{rows:?}");
            assert!(rows.iter().all(|(_, s)| s == "BLOCKED: path traversal"));
            assert!(store.ops().is_empty(), "nothing may reach the store");
        }

        /// A name that merely CONTAINS dots is a plain name, not traversal.
        /// The old guard (`path.contains("..")`) refused `a..b` outright.
        #[tokio::test]
        async fn a_name_containing_dots_is_not_traversal() {
            let ctx = ctx_as("impresspress/files").await;
            let (block, _store) = shim(&[]);

            send(&block, &ctx, "storage.put", &put("a..b", "c..d.txt"))
                .await
                .expect("`a..b` is a plain folder name");
            let got = send(
                &block,
                &ctx,
                "storage.get",
                &wafer_block::wire::storage::GetRequest {
                    folder: "a..b".into(),
                    key: "c..d.txt".into(),
                },
            )
            .await
            .expect("and the object reads back");
            assert!(!got.is_empty());
        }

        /// The shim's cross-block check is its own layer, and it must be able
        /// to say no. Handed the unprefixed resolved path, `check_access`
        /// read the reach as the caller's OWN namespace and admitted every
        /// attributable caller without consulting a grant — so an ungranted
        /// block read another block's object straight through the shim.
        #[tokio::test]
        async fn an_ungranted_cross_block_read_is_refused_and_audited_by_the_shim() {
            let ctx = ctx_as("test/ungranted").await;
            let (block, store) = shim(&[]);
            store
                .put(
                    "impresspress/files/uploads",
                    "secret.txt",
                    b"private",
                    "text/plain",
                )
                .await
                .expect("seed the other block's object");

            let err = send(
                &block,
                &ctx,
                "storage.get",
                &wafer_block::wire::storage::GetRequest {
                    folder: "@impresspress/files/uploads".into(),
                    key: "secret.txt".into(),
                },
            )
            .await
            .expect_err("a cross-block read with no grant must be refused");

            assert_eq!(err.code, ErrorCode::PermissionDenied, "{}", err.message);
            let rows = audit_rows(&ctx).await;
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].0, "impresspress/files/uploads/secret.txt");
            assert!(rows[0].1.starts_with("BLOCKED: "), "{rows:?}");
        }

        /// The counterpart: a grant written against object paths
        /// (`wafer-run/web/site/*`, the dev block's shape) admits an object
        /// under it — the check runs on the object, not on its folder, which
        /// that pattern does not match.
        #[tokio::test]
        async fn a_cross_block_grant_admits_the_objects_under_it() {
            let ctx = ctx_as("test/granted").await;
            let read_write = ResourceGrant::read_write("test/granted", "wafer-run/web/site/*");
            let read_only = ResourceGrant::read("test/granted", "wafer-run/web/site/*");
            let (block, store) = shim(&[read_write.typed(ResourceType::Storage)]);

            send(
                &block,
                &ctx,
                "storage.put",
                &put("@wafer-run/web/site", "index.html"),
            )
            .await
            .expect("the grant covers wafer-run/web/site/index.html");
            let (bytes, _) = store
                .get("wafer-run/web/site", "index.html")
                .await
                .expect("the object landed in the granted namespace");
            assert_eq!(bytes, b"payload".to_vec());

            // A write grant is needed for a write: the same pattern as a
            // read-only grant refuses the put.
            let (read_only, _) = shim(&[read_only.typed(ResourceType::Storage)]);
            let err = send(
                &read_only,
                &ctx,
                "storage.put",
                &put("@wafer-run/web/site", "index.html"),
            )
            .await
            .expect_err("a read grant does not admit a write");
            assert_eq!(err.code, ErrorCode::PermissionDenied);
        }
    }
}
