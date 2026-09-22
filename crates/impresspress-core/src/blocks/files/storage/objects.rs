//! Object lifecycle: list, download (streamed), upload, delete. Bucket-name
//! extraction/validation and access control are shared with the bucket
//! handlers via [`super::params`] / [`super::validation`] / [`super::access`].

use wafer_core::clients::storage as store;
use wafer_run::{context::Context, ErrorCode, InputStream, Message, OutputStream};

use super::{
    access::is_bucket_access_denied,
    params::{extract_bucket_name, extract_object_key},
    validation::{is_valid_bucket_name, is_valid_storage_key},
};
use crate::{
    blocks::{
        crud,
        files::{
            contracts::{
                DeletedResponse, ObjectInfoResponse, ObjectListResponse, ObjectUploadedResponse,
            },
            repo,
        },
    },
    http::{err_bad_request, err_conflict, err_forbidden, err_internal, err_not_found, ok_json},
};

/// Collect an `InputStream` into `Vec<u8>` with a hard size cap. Errors out
/// as soon as the running total exceeds `cap_bytes`, so the copy this makes is
/// never larger than the cap. Returns `Err(())` when the cap is exceeded.
///
/// The transport-level ceiling on a request body is
/// [`crate::streaming::MAX_REQUEST_BODY_BYTES`], enforced before dispatch;
/// this cap is the caller's quota, which
/// [`crate::blocks::files::quota::get_user_quota`] has already clamped to that
/// ceiling.
async fn collect_with_cap(
    mut input: wafer_run::InputStream,
    cap_bytes: i64,
) -> Result<Vec<u8>, ()> {
    use futures::StreamExt;
    let cap = if cap_bytes <= 0 {
        usize::MAX
    } else {
        cap_bytes as usize
    };
    let mut out = Vec::new();
    while let Some(chunk) = input.next().await {
        if out.len().saturating_add(chunk.len()) > cap {
            return Err(());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

pub(in crate::blocks::files) async fn handle_list_objects(
    ctx: &dyn Context,
    msg: &Message,
) -> OutputStream {
    let bucket = match extract_bucket_name(msg) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !is_valid_bucket_name(bucket) {
        return err_bad_request("Invalid bucket name");
    }
    if is_bucket_access_denied(ctx, msg, bucket).await {
        return err_forbidden("Access denied to this bucket");
    }

    let prefix = msg.query("prefix").to_string();
    let (_, page_size, offset) = msg.pagination_params(50);

    let opts = store::ListOptions {
        prefix,
        limit: page_size as i64,
        offset: offset as i64,
        // Offset-only paging; `None` preserves the existing behavior.
        cursor: None,
    };

    match store::list(ctx, bucket, &opts).await {
        // `store::list` returns `wafer_core::clients::storage::ObjectList`,
        // a wafer-run wire type that doesn't derive `schemars::JsonSchema`.
        // Rebuild it as the local `ObjectListResponse` (see
        // `blocks::files::contracts` for why) so the type the OpenAPI schema
        // is derived from is the same type that gets serialized.
        Ok(list) => ok_json(&ObjectListResponse {
            objects: list
                .objects
                .into_iter()
                .map(|o| ObjectInfoResponse {
                    key: o.key,
                    size: o.size,
                    content_type: o.content_type,
                    last_modified: o.last_modified,
                })
                .collect(),
            total_count: list.total_count,
        }),
        Err(e) => err_internal("Storage error", e),
    }
}

pub(in crate::blocks::files) async fn handle_get_object(
    ctx: &dyn Context,
    msg: &Message,
) -> OutputStream {
    let bucket = match extract_bucket_name(msg) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let key = match extract_object_key(msg) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !is_valid_storage_key(key) {
        return err_bad_request("Invalid object key");
    }
    if is_bucket_access_denied(ctx, msg, bucket).await {
        return err_forbidden("Access denied to this bucket");
    }

    // Track view in DB
    if let Err(e) = repo::views::insert(ctx, bucket, key, msg.user_id()).await {
        tracing::warn!("Failed to track storage object view: {e}");
    }

    // Stream the object body straight from storage (R2 `get_streaming` on CF)
    // rather than buffering the whole object into the isolate: `get_stream`
    // returns the `ObjectInfo` header eagerly, then the body flows chunk by
    // chunk. The leading meta carries the streaming opt-in marker + the real
    // content-type so the pipeline and platform adapter take the streaming
    // response path (see `crate::streaming`).
    //
    // The bytes and the content type are both an uploader's, and this route is
    // on the app's own origin, so the disposition and the security headers
    // come from [`crate::blocks::files::serving`] — the same builder the public
    // share link uses.
    match store::get_stream(ctx, bucket, key).await {
        Ok(stream) => {
            // The `application/octet-stream` a backend reporting no type used
            // to get here is not applied twice: the empty string is not a
            // media type, so `serving` substitutes it along with every other
            // type it cannot read.
            let leading = crate::blocks::files::serving::user_object_leading_meta(
                &stream.info().content_type.clone(),
                key,
                &[],
            );
            crate::streaming::stream_download(stream, leading)
        }
        Err(e) => crud::db_error(e, "Object not found", "Storage error"),
    }
}

pub(in crate::blocks::files) async fn handle_upload_object(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let bucket = match extract_bucket_name(msg) {
        Ok(value) => value,
        Err(response) => return response,
    };

    let request_content_type = msg.get_meta("req.content_type").to_string();
    let is_multipart = crate::multipart::multipart_boundary(&request_content_type).is_some();

    let query_key = msg.query("key").to_string();
    // For raw-body uploads the key can only come from the URL, so its absence
    // is fatal before buffering anything. Multipart bodies carry a fallback
    // (the file part's filename), so that check happens after parsing below.
    if query_key.is_empty() && !is_multipart {
        return err_bad_request("Missing object key (pass as ?key=filename)");
    }
    if !query_key.is_empty() && !is_valid_storage_key(&query_key) {
        return err_bad_request("Invalid object key");
    }
    if is_bucket_access_denied(ctx, msg, bucket).await {
        return err_forbidden("Access denied to this bucket");
    }

    // Best-effort sweep before quota check: orphan `pending` rows (see
    // `sweep_stale_pending`) would otherwise inflate this user's quota usage
    // and lock them out.
    crate::blocks::files::quota::sweep_stale_pending(ctx, msg.user_id()).await;

    // Read the upload body under the user's quota. Two bounds:
    //   - per-file `max_file_size_bytes` (cheap to check on the running
    //     total; abort as soon as the collected total exceeds it)
    //   - total `max_storage_bytes` (depends on current usage; checked once
    //     after we know the body's full size)
    // The per-chunk check uses the user's *file-size* cap as a hard ceiling
    // since that's the smaller of the two. For multipart bodies the cap
    // applies to the envelope — a slight over-estimate (the extracted file
    // is always smaller than its envelope), never an under-estimate.
    //
    // This is not a streaming upload and it is not a defence against a
    // multi-GB body: the transport has already read the whole request body
    // into memory under `streaming::MAX_REQUEST_BODY_BYTES` (and refused
    // anything larger with a 413), so the `InputStream` here replays bytes
    // that are already resident. `get_user_quota` clamps the per-file cap to
    // that same ceiling, so the size this refuses on is one an upload can
    // actually reach.
    let quota = match crate::blocks::files::quota::get_user_quota(ctx, msg.user_id()).await {
        Ok(quota) => quota,
        // Fail closed: reading the body against the default cap during an
        // outage would admit a file an admin-lowered override forbids.
        Err(e) => return err_internal("Quota lookup failed", e),
    };
    let Ok(body_bytes) = collect_with_cap(input, quota.max_file_size_bytes).await else {
        return err_bad_request(&format!(
            "File exceeds maximum size of {} bytes",
            quota.max_file_size_bytes
        ));
    };

    // Browser uploads (`FormData` + fetch) arrive as `multipart/form-data`:
    // the body is a boundary envelope AROUND the file, not the file itself.
    // Extract the file part and store ITS bytes/content type/size — storing
    // the raw body would corrupt the object (the pre-fix behavior). Raw-body
    // uploads (programmatic clients POSTing the bytes directly) keep the
    // body as the content.
    let (content, key, content_type) = if is_multipart {
        let Some(file) =
            crate::multipart::extract_multipart_file(&body_bytes, &request_content_type)
        else {
            return err_bad_request("Multipart body contains no file part");
        };
        let key = if query_key.is_empty() {
            file.filename.unwrap_or_default()
        } else {
            query_key
        };
        if key.is_empty() {
            return err_bad_request("Missing object key (pass as ?key=filename)");
        }
        if !is_valid_storage_key(&key) {
            return err_bad_request("Invalid object key");
        }
        // The part's own Content-Type wins; fall back to extension-based
        // detection on the key (which itself falls back to octet-stream).
        let content_type = file
            .content_type
            .filter(|ct| !ct.is_empty())
            .unwrap_or_else(|| {
                wafer_core::mime::mime_for_ext(std::path::Path::new(&key)).to_string()
            });
        (file.content, key, content_type)
    } else {
        let content_type = if request_content_type.is_empty() {
            "application/octet-stream".to_string()
        } else {
            request_content_type
        };
        (body_bytes, query_key, content_type)
    };

    // An upload to a key that already holds an object REPLACES it — `(bucket,
    // key)` is one object, and `store::put` overwrites the blob — so the
    // quota it has to fit is the difference, not the whole file, and only when
    // the bytes it displaces are already counted against this same user.
    // (Admins can upload into a bucket they do not own; those bytes belong to
    // whoever uploaded them.)
    let existing = match repo::objects::find_by_bucket_key(ctx, bucket, &key).await {
        Ok(row) => row,
        // Fail closed: admitting the upload against "nothing is stored here"
        // would charge the full size to a quota it may not fit, or skip the
        // per-bucket file count for a replacement that is not one.
        Err(e) => return crud::db_error_internal(e, "Object lookup failed"),
    };
    let replaces_own_bytes = existing
        .as_ref()
        .filter(|row| row.uploaded_by == msg.user_id())
        .map(|row| row.size);

    if let Err(r) = crate::blocks::files::quota::check_quota(
        ctx,
        msg.user_id(),
        bucket,
        content.len() as i64,
        replaces_own_bytes,
    )
    .await
    {
        return r;
    }

    // Claim the key BEFORE uploading so a quota check that runs after this
    // insert counts the in-flight size. That narrows the race between
    // check_quota and the upload; it does not close it (see `check_quota`).
    // `(bucket, key)` is UNIQUE: a re-upload takes over the
    // key's one row, and an upload that finds another upload of the key still
    // in flight is refused (see `reserve_upload`).
    let reservation = match repo::objects::reserve_upload(
        ctx,
        bucket,
        &key,
        content.len(),
        &content_type,
        msg.user_id(),
    )
    .await
    {
        Ok(reservation) => reservation,
        // Another upload of the key holds it (see `reserve_upload`): a
        // conflict the client resolves by retrying once that upload settles,
        // not a fault. The message is this handler's own, so no backend text
        // reaches the client.
        Err(e) if e.code == ErrorCode::Aborted => {
            return err_conflict(
                "Another upload of this key is in progress; retry once it finishes",
            )
        }
        // `db_error_internal`, not a bare `err_internal`: a WRAP refusal is a
        // 403 and a quota is a 429, and folding either into a 500 is what left
        // an operator unable to tell a missing grant from a broken row.
        Err(e) => return crud::db_error_internal(e, "Failed to reserve upload slot"),
    };

    match store::put(ctx, bucket, &key, &content, &content_type).await {
        Ok(()) => {
            // The row is what charges quota and what the object listings read,
            // so an upload that cannot be recorded is not an upload. Left as
            // `pending` it is swept within the hour, and answering
            // `uploaded: true` anyway is how a stored object came to be
            // charged to nobody. Report it instead: the blob is in place, but
            // the row stays `pending`, so the key is held as in progress until
            // `sweep_stale_pending` clears it and a retry can claim it.
            if let Err(e) = repo::objects::mark_complete(ctx, &reservation.id).await {
                return crud::db_error_internal(e, "Upload stored but could not be recorded");
            }
            ok_json(&ObjectUploadedResponse {
                bucket: bucket.to_string(),
                key: key.to_string(),
                uploaded: true,
            })
        }
        Err(e) => {
            // Upload failed — give the claim up so it doesn't block quota. For
            // a replacement that means putting the previous object's row back:
            // its blob is still there (`put` failed), so it must keep being
            // described and charged.
            if let Err(release_err) = repo::objects::release_reservation(ctx, &reservation).await {
                tracing::warn!("Failed to release upload reservation: {release_err}");
            }
            err_internal("Upload failed", e)
        }
    }
}

pub(in crate::blocks::files) async fn handle_delete_object(
    ctx: &dyn Context,
    msg: &Message,
) -> OutputStream {
    let bucket = match extract_bucket_name(msg) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let key = match extract_object_key(msg) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !is_valid_storage_key(key) {
        return err_bad_request("Invalid object key");
    }
    if is_bucket_access_denied(ctx, msg, bucket).await {
        return err_forbidden("Access denied to this bucket");
    }

    // Storage first, tolerating "already gone": if an earlier attempt removed
    // the blob but failed the metadata cleanup below, the retry must still
    // reach that cleanup instead of stopping at "not found".
    let blob_existed = match store::delete(ctx, bucket, key).await {
        Ok(()) => true,
        Err(e) if e.code == ErrorCode::NotFound => false,
        Err(e) => return crud::db_error_internal(e, "Delete failed"),
    };

    // The metadata cleanup is reported, never swallowed: a surviving row
    // keeps charging the uploader's quota for a blob that no longer exists.
    let rows_removed = match repo::objects::delete_by_bucket_key(ctx, bucket, key).await {
        Ok(rows) => rows,
        Err(e) => return crud::db_error_internal(e, "Delete failed to clean up object metadata"),
    };

    if !blob_existed && rows_removed == 0 {
        return err_not_found("Object not found");
    }
    ok_json(&DeletedResponse { deleted: true })
}

#[cfg(test)]
mod integration_tests {
    use super::{
        super::test_helpers::{
            ctx_with_storage, ctx_with_storage_handle, seed_bucket, seed_object_row,
        },
        *,
    };
    use crate::{
        blocks::files::contracts::ObjectStatus,
        test_support::{auth_msg, output_is_error, output_json, FailingDbOpContext, TestContext},
    };

    /// Collect the body bytes a download `OutputStream` carried, failing with
    /// the stream's error message when it errored instead of serving.
    async fn download_body(out: OutputStream) -> Vec<u8> {
        use futures::StreamExt;
        use wafer_block::stream::StreamEvent;

        let mut body = Vec::new();
        let mut events = out;
        while let Some(evt) = events.next().await {
            match evt {
                StreamEvent::Chunk(bytes) => body.extend_from_slice(&bytes),
                StreamEvent::Error(e) => {
                    panic!("download errored instead of serving bytes: {}", e.message)
                }
                _ => {}
            }
        }
        body
    }

    /// CRUX regression (found by driving the live app, and the outage the
    /// whole suite was blind to): upload an object through the real upload
    /// handler, then download it through the real download handler and assert
    /// the bytes come back.
    ///
    /// Nothing in 60 merged PRs did this. The existing download tests seeded
    /// the object with `store::put` and read it back with `store::get` — both
    /// buffered ops — while `handle_get_object` issues `storage.get_streaming`
    /// (`store::get_stream`). That op was missing from
    /// `blocks::storage::rewrite_request_body`'s match, so the namespacing
    /// shim answered `InvalidArgument: unknown storage op:
    /// storage.get_streaming` and every `GET
    /// /b/storage/api/buckets/{b}/objects/{k}` was a 500 on the live server.
    ///
    /// Asserting "not an error" would not have been enough either: the bytes
    /// are the contract, so they are what this asserts.
    #[tokio::test]
    async fn uploaded_object_downloads_back_the_same_bytes() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "assets", "alice").await;

        let file_bytes: &[u8] = b"the exact bytes a user uploaded\x00\x01\x02\xff";
        let upload = handle_upload_object(
            &ctx,
            &upload_msg("assets", "report.bin", "application/octet-stream"),
            InputStream::from_bytes(file_bytes.to_vec()),
        )
        .await;
        assert_eq!(
            output_json(upload).await["uploaded"],
            serde_json::json!(true),
            "the upload half of the round trip must succeed"
        );

        let mut download_msg = auth_msg(
            "retrieve",
            "/b/storage/api/buckets/assets/objects/report.bin",
            "alice",
        );
        download_msg.set_meta("req.param.name", "assets");
        download_msg.set_meta("req.param.key", "report.bin");

        let body = download_body(handle_get_object(&ctx, &download_msg).await).await;

        assert_eq!(
            body, file_bytes,
            "the download must return the uploaded bytes"
        );
    }

    /// A download served via `handle_get_object` must take the STREAMING
    /// response shape: the `resp.stream` opt-in marker and the object's real
    /// content-type are emitted as **leading `Meta`** events (before the first
    /// body `Chunk`), and the body bytes are forwarded verbatim. This is what
    /// makes the pipeline + platform adapter stream the object instead of
    /// buffering it whole in the isolate. (`MemStorage` uses the default
    /// `get_streaming`, so this exercises the handler's framing end-to-end
    /// through the real `wafer-run/storage` wire protocol.)
    #[tokio::test]
    async fn get_object_streams_body_with_leading_meta_marker() {
        use futures::StreamExt;
        use wafer_block::stream::StreamEvent;
        use wafer_run::{MetaEntry, MetaGet, META_RESP_CONTENT_TYPE};

        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "assets", "alice").await;
        store::put(&ctx, "assets", "pic.png", b"PNGDATA", "image/png")
            .await
            .expect("seed object");

        let mut msg = auth_msg(
            "retrieve",
            "/b/storage/api/buckets/assets/objects/pic.png",
            "alice",
        );
        msg.set_meta("req.param.name", "assets");
        msg.set_meta("req.param.key", "pic.png");

        let events: Vec<StreamEvent> = handle_get_object(&ctx, &msg).await.collect().await;

        // Leading meta must PRECEDE the first body chunk (the streaming shape).
        let first_chunk = events
            .iter()
            .position(|e| matches!(e, StreamEvent::Chunk(_)))
            .expect("a body chunk must be streamed");
        let leading: Vec<MetaEntry> = events[..first_chunk]
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Meta(m) => Some(m.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            MetaGet::get(&leading, crate::streaming::META_RESP_STREAM),
            Some(crate::streaming::STREAM_MARKER_VALUE),
            "download must emit the streaming opt-in marker as leading meta"
        );
        assert_eq!(
            MetaGet::get(&leading, META_RESP_CONTENT_TYPE),
            Some("image/png"),
            "download must emit the object's real content-type as leading meta"
        );

        let body: Vec<u8> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Chunk(b) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(
            body, b"PNGDATA",
            "the object body must be streamed verbatim"
        );
    }

    /// Build a browser-shaped `multipart/form-data` envelope around
    /// `file_bytes` (one `name="file"` part carrying `filename` +
    /// `Content-Type: text/html`), mirroring what `FormData` + fetch send.
    fn multipart_envelope(boundary: &str, filename: &str, file_bytes: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(b"Content-Type: text/html\r\n\r\n");
        body.extend_from_slice(file_bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        body
    }

    /// Build the upload request message the router would produce for
    /// `POST /b/storage/api/buckets/{bucket}/objects?key={key}`.
    fn upload_msg(bucket: &str, key: &str, content_type: &str) -> Message {
        let mut msg = auth_msg(
            "create",
            &format!("/b/storage/api/buckets/{bucket}/objects"),
            "alice",
        );
        msg.set_meta("req.param.name", bucket);
        if !key.is_empty() {
            msg.set_meta("req.query.key", key);
        }
        msg.set_meta("req.content_type", content_type);
        msg
    }

    /// Build the message the router produces for
    /// `DELETE /b/storage/api/buckets/{bucket}/objects/{key}`.
    fn delete_msg(bucket: &str, key: &str) -> Message {
        let mut msg = auth_msg(
            "delete",
            &format!("/b/storage/api/buckets/{bucket}/objects/{key}"),
            "alice",
        );
        msg.set_meta("req.param.name", bucket);
        msg.set_meta("req.param.key", key);
        msg
    }

    /// One stored object with its metadata row, owned by `alice`.
    async fn ctx_with_stored_object() -> TestContext {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "assets", "alice").await;
        store::put(&ctx, "assets", "pic.png", b"PNGDATA", "image/png")
            .await
            .expect("seed object");
        seed_object_row(&ctx, "assets", "pic.png", "alice", 7).await;
        ctx
    }

    /// Both wire ops a filtered metadata delete can use, so the fault
    /// matches whichever the repository issues.
    fn object_row_delete_ops() -> Vec<(&'static str, &'static str)> {
        vec![
            ("database.delete_where", repo::objects::TABLE),
            ("database.delete_where_count", repo::objects::TABLE),
        ]
    }

    #[tokio::test]
    async fn delete_object_removes_blob_and_metadata_row() {
        let ctx = ctx_with_stored_object().await;

        let out = handle_delete_object(&ctx, &delete_msg("assets", "pic.png")).await;

        assert_eq!(output_json(out).await["deleted"], serde_json::json!(true));
        assert!(
            store::get(&ctx, "assets", "pic.png").await.is_err(),
            "the blob must be gone"
        );
        assert_eq!(
            repo::objects::count_for_uploader(&ctx, "alice")
                .await
                .expect("count"),
            0,
            "the metadata row must be gone"
        );
    }

    /// A surviving row keeps charging the uploader's quota for a blob that
    /// no longer exists, so a failed cleanup is reported, never swallowed.
    #[tokio::test]
    async fn delete_object_reports_metadata_cleanup_failure() {
        let ctx = ctx_with_stored_object().await;
        let failing = FailingDbOpContext::new(ctx.clone(), object_row_delete_ops());

        let out = handle_delete_object(&failing, &delete_msg("assets", "pic.png")).await;

        assert!(
            output_is_error(out, "Internal").await,
            "a metadata cleanup failure must not be reported as a successful delete"
        );
    }

    /// After a failed cleanup the blob may already be gone; a retry must
    /// still finish the cleanup instead of stopping at "object not found".
    #[tokio::test]
    async fn delete_object_retry_finishes_cleanup_after_partial_failure() {
        let ctx = ctx_with_stored_object().await;
        let failing = FailingDbOpContext::new(ctx.clone(), object_row_delete_ops());
        let first = handle_delete_object(&failing, &delete_msg("assets", "pic.png")).await;
        assert!(output_is_error(first, "Internal").await);

        let retry = handle_delete_object(&ctx, &delete_msg("assets", "pic.png")).await;

        assert_eq!(
            output_json(retry).await["deleted"],
            serde_json::json!(true),
            "the retry must complete the cleanup"
        );
        assert_eq!(
            repo::objects::count_for_uploader(&ctx, "alice")
                .await
                .expect("count"),
            0
        );
    }

    #[tokio::test]
    async fn delete_missing_object_is_not_found() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "assets", "alice").await;

        let out = handle_delete_object(&ctx, &delete_msg("assets", "missing.png")).await;

        assert!(output_is_error(out, "NotFound").await);
    }

    /// Fetch the single object-metadata row (asserting there is exactly
    /// one) and return its `(size, content_type, status)`.
    async fn sole_object_row(ctx: &TestContext) -> (i64, String, ObjectStatus) {
        let rows = repo::objects::list_all(ctx)
            .await
            .expect("list object rows");
        assert_eq!(rows.len(), 1, "expected exactly one object metadata row");
        let row = &rows[0];
        (row.size, row.content_type.clone(), row.status)
    }

    /// CRUX regression (found by driving the live app): a browser `FormData`
    /// upload arrives as `multipart/form-data`, and the handler used to store
    /// the RAW multipart envelope as the object content — every browser
    /// upload was corrupted (serving the file returned the envelope, and the
    /// recorded `size` was the envelope size). The handler must store the
    /// extracted FILE PART bytes, the part's content type, and the real
    /// content length.
    #[tokio::test]
    async fn upload_multipart_stores_file_bytes_not_envelope() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "site-assets", "alice").await;

        // An HTML *fragment* (no doctype/page-root tags): storage is
        // content-agnostic, so keeping page-chrome markers out of the fixture
        // keeps the coarse `scripts/grep-guard-html.sh` guard happy.
        let file_bytes: &[u8] = b"<h1>hello from impresspress</h1>\n<p>an uploaded page</p>\n";
        let boundary = "----WebKitFormBoundaryqHHDhrDMqZoc7sHW";
        let envelope = multipart_envelope(boundary, "index.html", file_bytes);
        assert!(
            envelope.len() > file_bytes.len(),
            "envelope must be strictly larger than the file for the size assertion to bite"
        );

        let msg = upload_msg(
            "site-assets",
            "index.html",
            &format!("multipart/form-data; boundary={boundary}"),
        );
        let out = handle_upload_object(&ctx, &msg, InputStream::from_bytes(envelope)).await;
        let resp = output_json(out).await;
        assert_eq!(
            resp.get("uploaded").and_then(|v| v.as_bool()),
            Some(true),
            "upload failed: {resp}"
        );

        let (stored, info) = store::get(&ctx, "site-assets", "index.html")
            .await
            .expect("stored object");
        assert_eq!(
            stored, file_bytes,
            "stored content must be the file bytes, not the multipart envelope"
        );
        assert_eq!(
            info.content_type, "text/html",
            "stored content type must come from the file part, not the multipart request header"
        );

        let (size, content_type, status) = sole_object_row(&ctx).await;
        assert_eq!(
            size,
            file_bytes.len() as i64,
            "metadata size must be the extracted content length, not the envelope length"
        );
        assert_eq!(content_type, "text/html");
        assert_eq!(status, ObjectStatus::Complete);
    }

    /// Non-multipart (raw body) uploads keep the existing behavior: the body
    /// IS the content — programmatic clients that POST raw bytes with a
    /// concrete content type must not regress.
    #[tokio::test]
    async fn upload_raw_body_stores_body_as_is() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "raw-bucket", "alice").await;

        let body: &[u8] = b"plain bytes, no envelope";
        let msg = upload_msg("raw-bucket", "notes.txt", "text/plain");
        let out = handle_upload_object(&ctx, &msg, InputStream::from_bytes(body.to_vec())).await;
        let resp = output_json(out).await;
        assert_eq!(
            resp.get("uploaded").and_then(|v| v.as_bool()),
            Some(true),
            "upload failed: {resp}"
        );

        let (stored, info) = store::get(&ctx, "raw-bucket", "notes.txt")
            .await
            .expect("stored object");
        assert_eq!(stored, body, "raw body must be stored unchanged");
        assert_eq!(info.content_type, "text/plain");

        let (size, content_type, status) = sole_object_row(&ctx).await;
        assert_eq!(size, body.len() as i64);
        assert_eq!(content_type, "text/plain");
        assert_eq!(status, ObjectStatus::Complete);
    }

    /// Build the message the router produces for
    /// `GET /b/storage/api/buckets/{bucket}/objects/{key}`.
    fn download_msg(bucket: &str, key: &str) -> Message {
        let mut msg = auth_msg(
            "retrieve",
            &format!("/b/storage/api/buckets/{bucket}/objects/{key}"),
            "alice",
        );
        msg.set_meta("req.param.name", bucket);
        msg.set_meta("req.param.key", key);
        msg
    }

    /// The response headers a download emitted, as leading meta (the frame
    /// that precedes the first body chunk — the streaming response shape).
    async fn download_headers(out: OutputStream) -> Vec<wafer_run::MetaEntry> {
        use futures::StreamExt;
        use wafer_block::stream::StreamEvent;

        let events: Vec<StreamEvent> = out.collect().await;
        let first_chunk = events
            .iter()
            .position(|e| matches!(e, StreamEvent::Chunk(_)))
            .expect("a body chunk must be streamed");
        events[..first_chunk]
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Meta(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }

    fn header<'m>(meta: &'m [wafer_run::MetaEntry], name: &str) -> Option<&'m str> {
        wafer_run::MetaGet::get(meta, &format!("resp.header.{name}"))
    }

    /// Stored XSS: an uploader picks the content type, the bytes are theirs,
    /// and this route serves both from the app's own origin. Uploading an HTML
    /// page and opening its download URL used to render that page on the
    /// origin — no `Content-Disposition`, no `nosniff` — so any script in it
    /// ran with the viewer's session.
    ///
    /// The upload is the one a browser sends (a `multipart/form-data`
    /// envelope with the part's own `Content-Type: text/html`), and the
    /// download is the real handler.
    #[tokio::test]
    async fn an_uploaded_html_page_is_served_as_an_inert_attachment() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "assets", "alice").await;

        let boundary = "XBOUNDARYX";
        let envelope = multipart_envelope(boundary, "payload.html", b"<h1>not a page</h1>");
        let upload = handle_upload_object(
            &ctx,
            &upload_msg(
                "assets",
                "payload.html",
                &format!("multipart/form-data; boundary={boundary}"),
            ),
            InputStream::from_bytes(envelope),
        )
        .await;
        assert_eq!(
            output_json(upload).await["uploaded"],
            serde_json::json!(true)
        );

        let meta = download_headers(
            handle_get_object(&ctx, &download_msg("assets", "payload.html")).await,
        )
        .await;

        assert_eq!(
            header(&meta, "Content-Disposition"),
            Some("attachment; filename=\"payload.html\""),
            "an uploaded HTML page must be downloaded, never rendered on this origin",
        );
        assert_eq!(
            header(&meta, "X-Content-Type-Options"),
            Some("nosniff"),
            "without nosniff the declared type is only a suggestion",
        );
        assert!(
            header(&meta, "Content-Security-Policy").is_some_and(|csp| csp.contains("sandbox")),
            "an attachment a browser renders anyway must render sandboxed: {meta:?}",
        );
    }

    /// The allowlist is what makes the fix compatible with previews: an image
    /// is still served inline, and still with `nosniff` — which is what stops
    /// an HTML body uploaded as `image/png` from being sniffed back into a
    /// page.
    #[tokio::test]
    async fn an_image_still_previews_inline_with_nosniff() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "assets", "alice").await;
        store::put(&ctx, "assets", "pic.png", b"PNGDATA", "image/png")
            .await
            .expect("seed object");

        let meta =
            download_headers(handle_get_object(&ctx, &download_msg("assets", "pic.png")).await)
                .await;

        assert_eq!(
            header(&meta, "Content-Disposition"),
            Some("inline; filename=\"pic.png\"")
        );
        assert_eq!(header(&meta, "X-Content-Type-Options"), Some("nosniff"));
    }

    /// Nothing restricts an object key to ASCII — `is_valid_storage_key` bans
    /// `..`, backslash, NUL and a leading `/`, and that is all — so a
    /// non-ASCII filename is an ordinary upload. This route had no
    /// `Content-Disposition` at all before it gained one, and on Cloudflare
    /// `Headers.set` throws above U+00FF, so an ASCII-only header would have
    /// turned such a download into a 500. Round-trip one through both real
    /// handlers and assert the header is ASCII and carries the real name.
    #[tokio::test]
    async fn a_non_ascii_key_downloads_with_an_ascii_header_that_still_names_it() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "assets", "alice").await;

        let key = "日本語 memo.txt";
        let upload = handle_upload_object(
            &ctx,
            &upload_msg("assets", key, "text/plain"),
            InputStream::from_bytes(b"bytes".to_vec()),
        )
        .await;
        assert_eq!(
            output_json(upload).await["uploaded"],
            serde_json::json!(true)
        );

        let out = handle_get_object(&ctx, &download_msg("assets", key)).await;
        let events: Vec<wafer_block::stream::StreamEvent> = futures::StreamExt::collect(out).await;
        let first_chunk = events
            .iter()
            .position(|e| matches!(e, wafer_block::stream::StreamEvent::Chunk(_)))
            .expect("the object must still be served, not 500");
        let meta: Vec<wafer_run::MetaEntry> = events[..first_chunk]
            .iter()
            .filter_map(|e| match e {
                wafer_block::stream::StreamEvent::Meta(m) => Some(m.clone()),
                _ => None,
            })
            .collect();

        let disposition = header(&meta, "Content-Disposition").expect("a disposition");
        assert!(
            disposition.contains("filename*=UTF-8''%E6%97%A5%E6%9C%AC%E8%AA%9E%20memo.txt"),
            "the real name must survive as RFC 6266 `filename*`: {disposition}"
        );
        assert!(
            meta.iter().all(|e| e.value.is_ascii()),
            "every header value must be ASCII or the Workers runtime throws: {meta:?}"
        );
    }

    /// **Fails on the pre-fix tree.** The stored per-file quota is 100 MiB and
    /// no transport will carry a request body over
    /// `streaming::MAX_REQUEST_BODY_BYTES` (10 MiB), so an upload between the
    /// two was refused by the transport — as an opaque 500 with a correlation
    /// id on Cloudflare — while this handler, and everything that reports the
    /// limit, still described 100 MiB as allowed. `get_user_quota` now clamps
    /// the per-file cap to the transport ceiling, so the size the block
    /// refuses on and the size it advertises are the same number.
    #[tokio::test]
    async fn an_upload_over_the_transport_cap_is_refused_against_the_enforced_limit() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "assets", "alice").await;

        let body = vec![b'x'; crate::streaming::MAX_REQUEST_BODY_BYTES + 1];
        let out = handle_upload_object(
            &ctx,
            &upload_msg("assets", "big.bin", "application/octet-stream"),
            InputStream::from_bytes(body),
        )
        .await;

        let rendered = crate::test_support::output_http_json(out).await;
        assert_eq!(rendered["error"], serde_json::json!("InvalidArgument"));
        assert_eq!(
            rendered["message"],
            serde_json::json!(format!(
                "File exceeds maximum size of {} bytes",
                crate::streaming::MAX_REQUEST_BODY_BYTES
            )),
            "the refusal must name the limit that is enforced, not the stored 100 MiB: {rendered}"
        );
        assert!(
            store::get(&ctx, "assets", "big.bin").await.is_err(),
            "nothing may be stored for a refused upload"
        );
    }

    /// `(bucket, key)` is one object and `store::put` overwrites the blob, so
    /// re-uploading a key REPLACES what is stored there. The metadata row is
    /// the same row: inserting a second one is refused by the unique index,
    /// which every re-upload used to answer 500 with.
    #[tokio::test]
    async fn re_uploading_an_existing_key_replaces_the_object() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "assets", "alice").await;

        let first = handle_upload_object(
            &ctx,
            &upload_msg("assets", "notes.txt", "text/plain"),
            InputStream::from_bytes(b"version one".to_vec()),
        )
        .await;
        assert_eq!(
            output_json(first).await["uploaded"],
            serde_json::json!(true)
        );

        let second = handle_upload_object(
            &ctx,
            &upload_msg("assets", "notes.txt", "text/markdown"),
            InputStream::from_bytes(b"version two, longer".to_vec()),
        )
        .await;

        assert_eq!(
            output_json(second).await["uploaded"],
            serde_json::json!(true),
            "re-uploading a key the user already owns must replace it, not 500",
        );
        let (stored, info) = store::get(&ctx, "assets", "notes.txt")
            .await
            .expect("stored object");
        assert_eq!(stored, b"version two, longer");
        assert_eq!(info.content_type, "text/markdown");

        let (size, content_type, status) = sole_object_row(&ctx).await;
        assert_eq!(
            size,
            "version two, longer".len() as i64,
            "the row must describe the object that is stored now",
        );
        assert_eq!(content_type, "text/markdown");
        assert_eq!(status, ObjectStatus::Complete);
    }

    /// Two uploads of `assets/same.txt` that the database interleaves: both
    /// are held at the reservation's read until both have made it, so both
    /// reserve on the same view of the key's row.
    ///
    /// Each upload reads the row twice — once for the quota (a replacement is
    /// charged the difference) and once to reserve it — so each racer lets
    /// its own first read through ([`RendezvousDbOpContext::passing_first`])
    /// and is held on the second. Names `repo::objects::TABLE` only to aim
    /// the rendezvous. Answers each racer's HTTP status, in `uploads` order.
    async fn race_two_uploads(
        ctx: &TestContext,
        uploads: [(&'static [u8], &'static str); 2],
    ) -> Vec<u16> {
        use crate::test_support::{output_http_status, RendezvousDbOpContext};

        let gated =
            RendezvousDbOpContext::new(ctx.clone(), "database.list", repo::objects::TABLE, 2);
        let racers: Vec<_> = uploads
            .into_iter()
            .map(|(bytes, content_type)| {
                let racer = gated.passing_first(1);
                tokio::spawn(async move {
                    output_http_status(
                        handle_upload_object(
                            &racer,
                            &upload_msg("assets", "same.txt", content_type),
                            InputStream::from_bytes(bytes.to_vec()),
                        )
                        .await,
                    )
                    .await
                })
            })
            .collect();
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            futures::future::try_join_all(racers),
        )
        .await
        .expect("both uploads must reach the rendezvous and finish")
        .expect("upload task panicked")
    }

    /// After a race: exactly one racer got a 200 and the other a 409 — the
    /// key was taken, not a fault — and the key's one row describes the bytes
    /// that are stored, which are the winner's.
    async fn assert_one_upload_won(
        ctx: &TestContext,
        uploads: [(&'static [u8], &'static str); 2],
        statuses: &[u16],
    ) {
        let mut sorted = statuses.to_vec();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec![200, 409],
            "one upload claims the key; the other is told it is taken, not 500"
        );
        let (winner_bytes, winner_type) = uploads[statuses.iter().position(|s| *s == 200).unwrap()];

        let (stored, info) = store::get(ctx, "assets", "same.txt")
            .await
            .expect("the winner's object is stored");
        assert_eq!(
            stored, winner_bytes,
            "the refused upload must not write the blob"
        );
        assert_eq!(info.content_type, winner_type);

        let rows = repo::objects::list_all(ctx).await.expect("object rows");
        assert_eq!(rows.len(), 1, "one key, one row: {rows:?}");
        let row = &rows[0];
        assert_eq!(row.status, ObjectStatus::Complete);
        assert_eq!(
            (row.size, row.content_type.as_str()),
            (stored.len() as i64, info.content_type.as_str()),
            "the row must describe the bytes that are stored"
        );
        assert_eq!(row.uploaded_by, "alice");
    }

    const RACING_UPLOADS: [(&[u8], &str); 2] = [
        (b"the first racer's bytes", "text/plain"),
        (
            b"# the second racer, a longer markdown file",
            "text/markdown",
        ),
    ];

    /// Two first uploads of the same NEW key. Both read no row; a plain
    /// insert has the unique index refuse the loser (a 500), and a loser that
    /// joined the winner's row leaves it describing one upload's bytes with
    /// the other's size and type.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_first_uploads_of_a_new_key_store_one_object_and_refuse_the_other() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "assets", "alice").await;

        let statuses = race_two_uploads(&ctx, RACING_UPLOADS).await;

        assert_one_upload_won(&ctx, RACING_UPLOADS, &statuses).await;
    }

    /// Two re-uploads of an EXISTING key. Both read the same `Complete` row;
    /// if both took it over, the row would describe whichever wrote last, and
    /// a failure of the other would put the old object's values back over an
    /// upload in flight.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_re_uploads_of_a_key_store_one_object_and_refuse_the_other() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "assets", "alice").await;
        let first = handle_upload_object(
            &ctx,
            &upload_msg("assets", "same.txt", "text/csv"),
            InputStream::from_bytes(b"the original".to_vec()),
        )
        .await;
        assert_eq!(
            output_json(first).await["uploaded"],
            serde_json::json!(true)
        );

        let statuses = race_two_uploads(&ctx, RACING_UPLOADS).await;

        assert_one_upload_won(&ctx, RACING_UPLOADS, &statuses).await;
    }

    /// An upload of a key whose previous upload is still in flight — its row
    /// `Pending` and fresh — is refused, and leaves that upload's row as it
    /// was. Taking the row over is what let a failed second upload put the
    /// first one's in-flight values back over the first one's finished
    /// upload.
    #[tokio::test]
    async fn an_upload_of_a_key_another_upload_holds_is_refused() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "assets", "alice").await;
        let held =
            repo::objects::reserve_upload(&ctx, "assets", "same.txt", 7, "text/plain", "bob")
                .await
                .expect("bob's upload claims the key");

        let out = handle_upload_object(
            &ctx,
            &upload_msg("assets", "same.txt", "text/markdown"),
            InputStream::from_bytes(b"alice's longer bytes".to_vec()),
        )
        .await;

        assert_eq!(crate::test_support::output_http_status(out).await, 409);
        let rows = repo::objects::list_all(&ctx).await.expect("object rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, held.id);
        assert_eq!(
            (rows[0].size, rows[0].uploaded_by.as_str(), rows[0].status),
            (7, "bob", ObjectStatus::Pending),
            "the in-flight upload's row must be left alone"
        );
        assert!(
            store::get(&ctx, "assets", "same.txt").await.is_err(),
            "the refused upload must not write the blob"
        );
    }

    /// Seed another user's `Pending` row for `assets/same.txt` that is past
    /// the reservation TTL — an orphan, not an upload in flight. (Alice's own
    /// orphans would be swept by her upload before it reserves.)
    async fn seed_orphaned_reservation(ctx: &TestContext) {
        let ttl = repo::objects::PENDING_RESERVATION_TTL_SECONDS;
        let stale = (chrono::Utc::now() - chrono::Duration::seconds(2 * ttl)).to_rfc3339();
        repo::objects::seed(
            ctx,
            crate::util::json_map(serde_json::json!({
                "bucket": "assets",
                "key": "same.txt",
                "size": 7,
                "status": ObjectStatus::Pending,
                "uploaded_by": "bob",
                "uploaded_at": stale,
            })),
        )
        .await
        .expect("seed an orphaned reservation");
    }

    /// An upload of a key held only by an orphaned reservation takes the row
    /// over rather than waiting for a sweep that may never reach it.
    #[tokio::test]
    async fn an_orphaned_reservation_is_taken_over() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "assets", "alice").await;
        seed_orphaned_reservation(&ctx).await;

        let out = handle_upload_object(
            &ctx,
            &upload_msg("assets", "same.txt", "text/plain"),
            InputStream::from_bytes(b"fresh bytes".to_vec()),
        )
        .await;

        assert_eq!(output_json(out).await["uploaded"], serde_json::json!(true));
        let rows = repo::objects::list_all(&ctx).await.expect("object rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].size, rows[0].uploaded_by.as_str(), rows[0].status),
            ("fresh bytes".len() as i64, "alice", ObjectStatus::Complete),
        );
    }

    /// A failed upload over an orphaned reservation does not put the orphan
    /// back: there was no stored object to restore, and restoring a `Pending`
    /// snapshot is what can bury a finished upload. The key is left free.
    #[tokio::test]
    async fn a_failed_upload_over_an_orphan_does_not_restore_it() {
        let (ctx, storage) = ctx_with_storage_handle().await;
        seed_bucket(&ctx, "assets", "alice").await;
        seed_orphaned_reservation(&ctx).await;
        storage.refuse("put");

        let out = handle_upload_object(
            &ctx,
            &upload_msg("assets", "same.txt", "text/plain"),
            InputStream::from_bytes(b"fresh bytes".to_vec()),
        )
        .await;

        assert_eq!(crate::test_support::output_http_status(out).await, 500);
        assert!(
            repo::objects::list_all(&ctx)
                .await
                .expect("object rows")
                .is_empty(),
            "a failed upload over an orphan must leave no row"
        );
    }

    /// A context on which another upload of the key claims it just before
    /// this upload's reservation insert, and gives it up again just after:
    /// the insert affects nothing, and the row that caused that is gone by
    /// the time it is read back.
    #[derive(Clone)]
    struct ChurnedKeyContext {
        inner: TestContext,
        bucket: &'static str,
        key: &'static str,
    }

    #[async_trait::async_trait]
    impl wafer_run::context::Context for ChurnedKeyContext {
        fn check_resource_access(
            &self,
            resource: &str,
            resource_type: wafer_run::ResourceType,
            is_write: bool,
        ) -> Result<(), wafer_run::WaferError> {
            self.inner
                .check_resource_access(resource, resource_type, is_write)
        }

        async fn call_block(&self, name: &str, msg: Message, input: InputStream) -> OutputStream {
            if !(name == "wafer-run/database" && msg.action() == "database.upsert") {
                return self.inner.call_block(name, msg, input).await;
            }
            let rival = repo::objects::seed(
                &self.inner,
                crate::util::json_map(serde_json::json!({
                    "bucket": self.bucket,
                    "key": self.key,
                    "status": ObjectStatus::Pending,
                    "uploaded_by": "bob",
                })),
            )
            .await
            .expect("the rival upload claims the key");
            let out = self.inner.call_block(name, msg, input).await;
            // Settled before the rival lets go, so the insert sees its row.
            let answered = out
                .collect_buffered()
                .await
                .unwrap_or_else(|_| panic!("the reservation insert must answer"));
            repo::objects::delete(&self.inner, &rival.id)
                .await
                .expect("the rival upload releases the key");
            OutputStream::respond_with_meta(answered.body, answered.meta)
        }

        fn is_cancelled(&self) -> bool {
            self.inner.is_cancelled()
        }

        fn registered_blocks(&self) -> &[wafer_run::BlockInfo] {
            self.inner.registered_blocks()
        }

        fn config_get(&self, key: &str) -> Option<&str> {
            self.inner.config_get(key)
        }

        fn clone_arc(&self) -> std::sync::Arc<dyn wafer_run::context::Context> {
            std::sync::Arc::new(self.clone())
        }
    }

    /// The reservation lost the key to another upload that has since given it
    /// up: nothing is left to join, so the upload is told to retry — a 409 —
    /// rather than a 500, and nothing is stored.
    #[tokio::test]
    async fn an_upload_whose_rival_released_the_key_is_told_to_retry() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "assets", "alice").await;
        let churned = ChurnedKeyContext {
            inner: ctx.clone(),
            bucket: "assets",
            key: "same.txt",
        };

        let out = handle_upload_object(
            &churned,
            &upload_msg("assets", "same.txt", "text/plain"),
            InputStream::from_bytes(b"bytes".to_vec()),
        )
        .await;

        assert_eq!(
            crate::test_support::output_http_status(out).await,
            409,
            "a key that changed hands mid-reservation is a conflict to retry"
        );
        assert!(
            repo::objects::list_all(&ctx)
                .await
                .expect("object rows")
                .is_empty(),
            "a refused reservation leaves no row"
        );
        assert!(
            store::get(&ctx, "assets", "same.txt").await.is_err(),
            "nothing may be stored for a refused upload"
        );
    }

    /// A replacement costs the DIFFERENCE, not the whole file: the bytes it
    /// displaces are already counted in this user's usage. Charging the full
    /// size would refuse a user who is merely editing a file in place.
    #[tokio::test]
    async fn a_replacement_is_charged_the_difference_not_the_whole_file() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "assets", "alice").await;
        let mut quota: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        quota.insert("user_id".into(), serde_json::json!("alice"));
        quota.insert("max_storage_bytes".into(), serde_json::json!(24));
        repo::quota::seed(&ctx, quota).await.expect("seed quota");

        let first = handle_upload_object(
            &ctx,
            &upload_msg("assets", "notes.txt", "text/plain"),
            InputStream::from_bytes(vec![b'a'; 20]),
        )
        .await;
        assert_eq!(
            output_json(first).await["uploaded"],
            serde_json::json!(true)
        );

        // 22 bytes replacing 20 is +2 against a 24-byte cap: admitted. The
        // same 22 bytes charged whole against 20 already stored would be 42.
        let replace = handle_upload_object(
            &ctx,
            &upload_msg("assets", "notes.txt", "text/plain"),
            InputStream::from_bytes(vec![b'b'; 22]),
        )
        .await;
        assert_eq!(
            output_json(replace).await["uploaded"],
            serde_json::json!(true),
            "a replacement that fits the cap after the displaced bytes must be admitted",
        );
        assert_eq!(
            crate::blocks::files::quota::get_used_bytes(&ctx, "alice")
                .await
                .expect("usage"),
            22,
            "usage must follow the object that is stored, not the sum of every upload",
        );

        // The cap is still a cap: 30 bytes replacing 22 is 30 > 24.
        let too_big = handle_upload_object(
            &ctx,
            &upload_msg("assets", "notes.txt", "text/plain"),
            InputStream::from_bytes(vec![b'c'; 30]),
        )
        .await;
        assert!(
            output_is_error(too_big, "InvalidArgument").await,
            "a replacement that does not fit even after the displaced bytes must be refused",
        );
    }

    /// Give alice a quota override capping her at `max_files_per_bucket`
    /// objects per bucket, and own buckets `a` and `b`.
    async fn alice_capped_at_files_per_bucket(max_files_per_bucket: i64) -> TestContext {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "a", "alice").await;
        seed_bucket(&ctx, "b", "alice").await;
        repo::quota::seed(
            &ctx,
            crate::util::json_map(serde_json::json!({
                "user_id": "alice",
                "max_files_per_bucket": max_files_per_bucket,
            })),
        )
        .await
        .expect("seed quota");
        ctx
    }

    /// Upload a small text file as alice through the real handler.
    async fn alice_uploads(ctx: &TestContext, bucket: &str, key: &str) -> OutputStream {
        alice_uploads_as(ctx, bucket, key).await
    }

    /// [`alice_uploads`] through any context, so a race test can pass a
    /// decorated one.
    async fn alice_uploads_as(ctx: &dyn Context, bucket: &str, key: &str) -> OutputStream {
        handle_upload_object(
            ctx,
            &upload_msg(bucket, key, "text/plain"),
            InputStream::from_bytes(b"hello".to_vec()),
        )
        .await
    }

    /// The file-count cap is per bucket, as its name and the admin table's
    /// "Max Files/Bucket" column say: filling bucket `a` to the cap does not
    /// stop an upload into bucket `b`. Counting the user's files across every
    /// bucket refused it.
    #[tokio::test]
    async fn a_full_bucket_does_not_block_uploads_into_another_bucket() {
        let ctx = alice_capped_at_files_per_bucket(2).await;
        for key in ["one.txt", "two.txt"] {
            let out = alice_uploads(&ctx, "a", key).await;
            assert_eq!(output_json(out).await["uploaded"], serde_json::json!(true));
        }

        let other_bucket = alice_uploads(&ctx, "b", "three.txt").await;
        assert_eq!(
            output_json(other_bucket).await["uploaded"],
            serde_json::json!(true),
            "bucket b holds none of alice's files, so the per-bucket cap admits it",
        );
    }

    /// And the cap is still a cap within one bucket: the upload that would be
    /// the N+1th object in it is refused, while replacing an object already
    /// there adds no row and is admitted. Alice's file in bucket `b` is what
    /// makes the count the bucket's: counted across buckets, her second
    /// upload into `a` would already be refused.
    #[tokio::test]
    async fn the_per_bucket_cap_refuses_the_upload_past_it_in_that_bucket() {
        let ctx = alice_capped_at_files_per_bucket(2).await;
        let elsewhere = alice_uploads(&ctx, "b", "elsewhere.txt").await;
        assert_eq!(
            output_json(elsewhere).await["uploaded"],
            serde_json::json!(true)
        );
        for key in ["one.txt", "two.txt"] {
            let out = alice_uploads(&ctx, "a", key).await;
            assert_eq!(
                output_json(out).await["uploaded"],
                serde_json::json!(true),
                "{key} is within bucket a's cap of two",
            );
        }

        let third = alice_uploads(&ctx, "a", "three.txt").await;
        assert!(
            output_is_error(third, "InvalidArgument").await,
            "a third object in a bucket capped at two must be refused",
        );
        assert!(
            store::get(&ctx, "a", "three.txt").await.is_err(),
            "nothing may be stored for a refused upload"
        );

        let replace = alice_uploads(&ctx, "a", "one.txt").await;
        assert_eq!(
            output_json(replace).await["uploaded"],
            serde_json::json!(true),
            "replacing an object in a full bucket adds no file and is admitted",
        );
    }

    /// An upload still in flight counts against the bucket's cap, as its bytes
    /// count against the storage cap: an upload whose check runs after
    /// another upload's reservation has landed sees that reservation and is
    /// refused. A guard on the chosen semantics — the cross-bucket count
    /// counted pending rows too, so this passes before and after the
    /// per-bucket fix.
    #[tokio::test]
    async fn an_upload_in_flight_counts_against_the_bucket_cap() {
        let ctx = alice_capped_at_files_per_bucket(1).await;
        repo::objects::reserve_upload(&ctx, "a", "in-flight.txt", 5, "text/plain", "alice")
            .await
            .expect("reserve");

        let out = alice_uploads(&ctx, "a", "next.txt").await;
        assert!(
            output_is_error(out, "InvalidArgument").await,
            "the pending reservation is the bucket's one file",
        );
    }

    /// The per-bucket cap is NOT enforced atomically, and this pins that.
    /// Two uploads of different keys into a bucket one short of its cap are
    /// held until both have counted the bucket, so both see one file and
    /// both are admitted: the bucket ends one over its cap. The count and the
    /// reservation insert are separate calls, and a reservation is exclusive
    /// per key, not per bucket. Tracked in `NICE_TO_HAVE.md` ("Files quota
    /// caps are not enforced atomically"); when that lands, this test should
    /// flip to one 200 and one refusal.
    #[tokio::test]
    async fn racing_uploads_of_different_keys_can_overshoot_the_bucket_cap() {
        use crate::test_support::{output_http_status, RendezvousDbOpContext};

        let ctx = alice_capped_at_files_per_bucket(2).await;
        let first = alice_uploads(&ctx, "a", "one.txt").await;
        assert_eq!(
            output_json(first).await["uploaded"],
            serde_json::json!(true)
        );

        // Each upload makes exactly one `count` on the objects table: the
        // per-bucket file count in `check_quota`.
        let gated =
            RendezvousDbOpContext::new(ctx.clone(), "database.count", repo::objects::TABLE, 2);
        let racers: Vec<_> = ["two.txt", "three.txt"]
            .into_iter()
            .map(|key| {
                let racer = gated.clone();
                tokio::spawn(async move {
                    output_http_status(alice_uploads_as(&racer, "a", key).await).await
                })
            })
            .collect();
        let statuses = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            futures::future::try_join_all(racers),
        )
        .await
        .expect("both uploads must reach the rendezvous and finish")
        .expect("upload task panicked");

        assert_eq!(statuses, vec![200, 200], "both racers pass the check");
        assert_eq!(
            repo::objects::count_for_uploader_in_bucket(&ctx, "alice", "a")
                .await
                .expect("count"),
            3,
            "the bucket ends one file over its cap of two",
        );
    }

    /// When the storage write fails, the reservation that took over the
    /// existing row has to put that row back: the previous blob is still
    /// there, so it must keep being described and charged.
    #[tokio::test]
    async fn a_failed_replacement_restores_the_row_of_the_object_it_kept() {
        let (ctx, storage) = ctx_with_storage_handle().await;
        seed_bucket(&ctx, "assets", "alice").await;
        let stored = handle_upload_object(
            &ctx,
            &upload_msg("assets", "notes.txt", "text/plain"),
            InputStream::from_bytes(b"version one".to_vec()),
        )
        .await;
        assert_eq!(
            output_json(stored).await["uploaded"],
            serde_json::json!(true)
        );

        // Same context, same database — only the storage write now fails.
        storage.refuse("put");
        let out = handle_upload_object(
            &ctx,
            &upload_msg("assets", "notes.txt", "text/plain"),
            InputStream::from_bytes(b"much longer replacement".to_vec()),
        )
        .await;
        assert!(output_is_error(out, "Internal").await);

        let (size, content_type, status) = sole_object_row(&ctx).await;
        assert_eq!(
            size,
            "version one".len() as i64,
            "the surviving object's size must be restored, not left charging the failed upload's",
        );
        assert_eq!(content_type, "text/plain");
        assert_eq!(
            status,
            ObjectStatus::Complete,
            "the surviving object must not be left `pending` for the sweep to delete",
        );
    }

    /// The row is what charges quota and what the listings read, so an upload
    /// that could not be recorded is not an upload. It used to answer
    /// `uploaded: true` with the row still `pending`, which the one-hour sweep
    /// then deleted — a stored object charged to nobody, and nothing said so.
    #[tokio::test]
    async fn an_upload_that_cannot_be_recorded_is_reported_not_claimed() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "assets", "alice").await;
        // `mark_complete` is the only `database.update` a fresh upload issues.
        let failing =
            FailingDbOpContext::new(ctx.clone(), vec![("database.update", repo::objects::TABLE)]);

        let out = handle_upload_object(
            &failing,
            &upload_msg("assets", "notes.txt", "text/plain"),
            InputStream::from_bytes(b"bytes".to_vec()),
        )
        .await;

        assert!(
            output_is_error(out, "Internal").await,
            "an upload whose row stayed `pending` must not be reported as uploaded",
        );
        let (_, _, status) = sole_object_row(&ctx).await;
        assert_eq!(
            status,
            ObjectStatus::Pending,
            "the row is still the reservation, held until the sweep clears it",
        );
    }

    /// A multipart upload without `?key=` falls back to the file part's
    /// `filename` as the object key (the URL query param still wins when
    /// present).
    #[tokio::test]
    async fn upload_multipart_without_query_key_uses_part_filename() {
        let ctx = ctx_with_storage().await;
        seed_bucket(&ctx, "site-assets", "alice").await;

        let file_bytes: &[u8] = b"body";
        let boundary = "XBOUNDARYX";
        let envelope = multipart_envelope(boundary, "from-part.html", file_bytes);

        let msg = upload_msg(
            "site-assets",
            "",
            &format!("multipart/form-data; boundary={boundary}"),
        );
        let out = handle_upload_object(&ctx, &msg, InputStream::from_bytes(envelope)).await;
        let resp = output_json(out).await;
        assert_eq!(
            resp.get("key").and_then(|v| v.as_str()),
            Some("from-part.html"),
            "key must fall back to the part filename: {resp}"
        );

        let (stored, _) = store::get(&ctx, "site-assets", "from-part.html")
            .await
            .expect("stored object");
        assert_eq!(stored, file_bytes);
    }
}
