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
    http::{err_bad_request, err_forbidden, err_internal, err_not_found, ok_json},
};

/// Collect an `InputStream` into `Vec<u8>` with a hard size cap. Errors out
/// as soon as the running total exceeds `cap_bytes`, so a multi-GB body
/// can't OOM the process before we check quota. Returns `Err(())` when
/// the cap is exceeded.
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
            let content_type = resolved_content_type(stream.info());
            let leading =
                crate::blocks::files::serving::user_object_leading_meta(&content_type, key, &[]);
            crate::streaming::stream_download(stream, leading)
        }
        Err(e) => crud::db_error(e, "Object not found", "Storage error"),
    }
}

/// The object's stored content-type, falling back to `application/octet-stream`
/// when the backend reports none (parity with the buffered `get` path, which
/// R2/S3 default the same way).
fn resolved_content_type(info: &store::ObjectInfo) -> String {
    if info.content_type.is_empty() {
        "application/octet-stream".to_string()
    } else {
        info.content_type.clone()
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

    // Best-effort sweep before quota check: orphan `pending` rows (from
    // previous uploads where the storage put failed AND the compensating
    // delete also failed) would otherwise inflate this user's quota usage
    // and lock them out. 1h cutoff.
    crate::blocks::files::quota::sweep_stale_pending(ctx, msg.user_id(), 3600).await;

    // Stream the upload body chunk-by-chunk so an attacker who streams a
    // multi-GB body can't OOM us before quota check fires. Two bounds:
    //   - per-file `max_file_size_bytes` (cheap to check on the running
    //     total; abort as soon as the chunked total exceeds it)
    //   - total `max_storage_bytes` (depends on current usage; checked once
    //     after we know the body's full size)
    // The chunked check uses the user's *file-size* cap as a hard ceiling
    // since that's the smaller of the two. For multipart bodies the cap
    // applies to the envelope — a slight over-estimate (the extracted file
    // is always smaller than its envelope), never an under-estimate.
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
        // per-user file count for a replacement that is not one.
        Err(e) => return crud::db_error_internal(e, "Object lookup failed"),
    };
    let replaces_own_bytes = existing
        .as_ref()
        .filter(|row| row.uploaded_by == msg.user_id())
        .map(|row| row.size);

    if let Err(r) = crate::blocks::files::quota::check_quota(
        ctx,
        msg.user_id(),
        content.len() as i64,
        replaces_own_bytes,
    )
    .await
    {
        return r;
    }

    // Claim the key BEFORE uploading so concurrent quota checks see the
    // in-flight size. This closes the TOCTOU race between check_quota and the
    // actual upload. On a re-upload the claim takes over the existing row —
    // `(bucket, key)` is UNIQUE, so inserting a second one is refused by the
    // database, which is what every re-upload used to answer 500 with.
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
            // charged to nobody. Report it instead: the blob is in place, and
            // a retry re-claims this same row and completes it.
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
            "the row is still the reservation, which is what the uploader retries against",
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
