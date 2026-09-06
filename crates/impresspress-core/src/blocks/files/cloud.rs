//! The cloud-storage JSON API: a user's share links and quota, and the admin
//! views over every user's shares, access logs and quotas. Dispatch lives in
//! the block's one route table (`blocks/files/mod.rs`); the admin handlers
//! are declared `Admin` there and gated by the router from that declaration
//! (until this PR they were reached through the admin block's `call_block`
//! delegation on synthetic paths that never existed on the wire).

use std::collections::HashMap;

use wafer_run::{context::Context, InputStream, Message, OutputStream};

use super::{
    contracts::{RecordListView, RecordView},
    repo,
};
use crate::{
    blocks::crud,
    http::{err_bad_request, err_forbidden, err_internal, err_not_found, ok_json},
};

pub(super) async fn handle_list_shares(ctx: &dyn Context, msg: &Message) -> OutputStream {
    match repo::shares::list_for_user(ctx, msg.user_id(), 100).await {
        Ok(page) => ok_json(&RecordListView::from_page(&page)),
        Err(e) => err_internal("Database error", e),
    }
}

/// Upper bound on `expires_in_hours` for a share link (one year). Caller
/// input is otherwise unbounded, and both `chrono::Duration::hours` and
/// `DateTime + Duration` panic on overflow in chrono 0.4.44 — a huge value
/// (e.g. `i64::MAX`) would panic the handler on this reachable request path.
/// Non-positive values are rejected too since they'd mint an
/// already-expired share.
const MAX_SHARE_EXPIRY_HOURS: i64 = 24 * 365;

pub(super) async fn handle_create_share(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    #[derive(serde::Deserialize)]
    struct Req {
        bucket: String,
        key: String,
        expires_in_hours: Option<i64>,
        max_access_count: Option<i64>,
    }
    let raw = input.collect_to_bytes().await;
    let body: Req = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    // Validate bucket/key through the shared storage validators so the share
    // path enforces exactly the same rules as upload/download (SEC-064: the
    // old inline copy here omitted the backslash check, letting a share be
    // created for a key the storage path would reject).
    if body.bucket.is_empty() || body.key.is_empty() {
        return err_bad_request("Bucket and key are required");
    }
    if !super::storage::is_valid_bucket_name(&body.bucket) {
        return err_bad_request("Invalid bucket name");
    }
    if !super::storage::is_valid_storage_key(&body.key) {
        return err_bad_request("Invalid object key");
    }

    // Verify the user owns this bucket (or is admin) — shared helper from
    // storage.rs so the two modules stay in lockstep on what "access
    // denied" means.
    if super::storage::is_bucket_access_denied(ctx, msg, &body.bucket).await {
        return err_forbidden("Access denied to this bucket");
    }

    // Verify the file actually exists before creating a share
    // audit-allow: bucket arg is &body.bucket (request-supplied); the storage block @-rewrites cross-block paths and the runtime grant check at impresspress-core/src/blocks/storage.rs:256 enforces the actual access against typed Storage grants
    if wafer_core::clients::storage::get(ctx, &body.bucket, &body.key)
        .await
        .is_err()
    {
        return err_not_found("File not found in storage");
    }

    // Generate share token
    let token = super::share::generate_share_token(ctx, &body.bucket, &body.key).await;
    let token = match token {
        Ok(t) => t,
        Err(r) => return r,
    };

    let now = chrono::Utc::now();
    let expires_at = match body.expires_in_hours {
        None => None,
        Some(h) if !(1..=MAX_SHARE_EXPIRY_HOURS).contains(&h) => {
            return err_bad_request(&format!(
                "expires_in_hours must be between 1 and {MAX_SHARE_EXPIRY_HOURS}"
            ));
        }
        Some(h) => {
            // `try_hours` + `checked_add_signed` instead of `Duration::hours`
            // + `+` — both of the latter panic on overflow in chrono 0.4.44.
            // The range check above already excludes anything that would
            // overflow; these keep the arithmetic itself panic-free even if
            // that bound is ever loosened.
            let Some(duration) = chrono::Duration::try_hours(h) else {
                return err_bad_request("expires_in_hours out of range");
            };
            let Some(expiry) = now.checked_add_signed(duration) else {
                return err_bad_request("expires_in_hours out of range");
            };
            Some(expiry.to_rfc3339())
        }
    };

    let created_at = now.to_rfc3339();
    let new_share = repo::shares::NewShare {
        token: &token,
        bucket: &body.bucket,
        key: &body.key,
        created_by: msg.user_id(),
        created_at: &created_at,
        expires_at: expires_at.as_deref(),
        max_access_count: body.max_access_count,
    };
    match repo::shares::insert(ctx, new_share).await {
        Ok(row) => ok_json(&serde_json::json!({
            "id": row.id,
            "token": token,
            "direct_url": format!("/b/storage/direct/{}", token)
        })),
        Err(e) => err_internal("Database error", e),
    }
}

pub(super) async fn handle_delete_share(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let id = match crud::path_id(msg, "Share") {
        Ok(value) => value,
        Err(response) => return response,
    };

    // Verify ownership. This lookup is the only authorization on the path,
    // so a failed read stops the request instead of skipping the check.
    match repo::shares::find_by_id(ctx, id).await {
        Ok(share) => {
            if share.created_by != msg.user_id() && !crate::util::is_admin(msg) {
                return err_forbidden("Cannot delete another user's share");
            }
        }
        Err(e) => return crud::db_error(e, "Share not found", "Database error"),
    }

    match repo::shares::delete(ctx, id).await {
        Ok(()) => ok_json(&serde_json::json!({"deleted": true})),
        Err(e) => crud::db_error(e, "Share not found", "Database error"),
    }
}

pub(super) async fn handle_get_quota(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let quota = match super::quota::get_user_quota(ctx, msg.user_id()).await {
        Ok(quota) => quota,
        Err(e) => return err_internal("Quota lookup failed", e),
    };
    let usage = match super::quota::get_user_usage(ctx, msg.user_id()).await {
        Ok(usage) => usage,
        Err(e) => return err_internal("Quota usage lookup failed", e),
    };
    ok_json(&serde_json::json!({
        "quota": quota,
        "usage": usage
    }))
}

pub(super) async fn handle_admin_list_shares(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let (page, page_size, _) = msg.pagination_params(20);
    let offset = ((page - 1) * page_size) as i64;
    match repo::shares::list_recent(ctx, page_size as i64, offset).await {
        Ok(page) => ok_json(&RecordListView::from_page(&page)),
        Err(e) => err_internal("Database error", e),
    }
}

pub(super) async fn handle_access_logs(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let (page, page_size, _) = msg.pagination_params(50);
    let share_id = msg.query("share_id").to_string();
    let share_id = (!share_id.is_empty()).then_some(share_id.as_str());
    let offset = ((page - 1) * page_size) as i64;

    match repo::shares::list_access_logs(ctx, share_id, page_size as i64, offset).await {
        Ok(page) => ok_json(&RecordListView::from_page(&page)),
        Err(e) => err_internal("Database error", e),
    }
}

pub(super) async fn handle_admin_quotas(ctx: &dyn Context, _msg: &Message) -> OutputStream {
    match repo::quota::list(ctx, 1000).await {
        Ok(page) => ok_json(&RecordListView::from_page(&page)),
        Err(e) => err_internal("Database error", e),
    }
}

pub(super) async fn handle_update_quota(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    // `{id}` in `PATCH /b/cloudstorage/admin/quotas/{id}` is the user whose
    // quota is set.
    let user_id = match crud::path_id(msg, "User") {
        Ok(value) => value,
        Err(response) => return response,
    };

    let raw = input.collect_to_bytes().await;
    let body: HashMap<String, serde_json::Value> = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    // SEC-059: whitelist accepted quota fields — never forward arbitrary
    // caller-controlled keys to the upsert. Reject anything outside the
    // known quota schema. (`user_id` + `updated_at` are stamped by
    // `repo::quota::upsert_for_user`.)
    const ALLOWED_QUOTA_FIELDS: &[&str] = &[
        "max_storage_bytes",
        "max_file_size_bytes",
        "max_files_per_bucket",
        "reset_period_days",
    ];
    for key in body.keys() {
        if !ALLOWED_QUOTA_FIELDS.contains(&key.as_str()) {
            return err_bad_request(&format!("Unknown quota field: {key}"));
        }
    }

    match repo::quota::upsert_for_user(ctx, user_id, body).await {
        Ok(row) => ok_json(&RecordView::from_row(&row)),
        Err(e) => err_internal("Database error", e),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use wafer_core::interfaces::storage::service as storage_service;
    use wafer_run::InputStream;

    use super::{super::test_support::routed, *};
    use crate::test_support::{
        auth_msg, output_is_error, output_json, FailingDbOpContext, TestContext,
    };

    /// Seed one share row owned by `owner` and return its id.
    async fn seed_share(ctx: &TestContext, owner: &str) -> String {
        repo::shares::insert(
            ctx,
            repo::shares::NewShare {
                token: "share-token-1",
                bucket: "photos",
                key: "a.png",
                created_by: owner,
                created_at: "2026-09-05T00:00:00Z",
                expires_at: None,
                max_access_count: None,
            },
        )
        .await
        .expect("seed share")
        .id
    }

    fn share_body(bucket: &str, key: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({ "bucket": bucket, "key": key })).unwrap()
    }

    /// Minimal `StorageService` fake whose `get` always succeeds, so
    /// `handle_create_share`'s file-existence check passes without wiring a
    /// real storage backend (filesystem/S3) into the test. Only `get` needs
    /// a meaningful implementation for the expiry-validation tests below.
    struct AlwaysFoundStorageService;

    #[wafer_block::wafer_async_trait]
    impl storage_service::StorageService for AlwaysFoundStorageService {
        async fn put(
            &self,
            _folder: &str,
            _key: &str,
            _data: &[u8],
            _content_type: &str,
        ) -> Result<(), storage_service::StorageError> {
            Ok(())
        }

        async fn get(
            &self,
            _folder: &str,
            key: &str,
        ) -> Result<(Vec<u8>, storage_service::ObjectInfo), storage_service::StorageError> {
            Ok((
                b"fake body".to_vec(),
                storage_service::ObjectInfo {
                    key: key.to_string(),
                    size: 9,
                    content_type: "text/plain".to_string(),
                    last_modified: chrono::Utc::now(),
                },
            ))
        }

        async fn delete(
            &self,
            _folder: &str,
            _key: &str,
        ) -> Result<(), storage_service::StorageError> {
            Ok(())
        }

        async fn list(
            &self,
            _folder: &str,
            _opts: &storage_service::ListOptions,
        ) -> Result<storage_service::ObjectList, storage_service::StorageError> {
            Ok(storage_service::ObjectList {
                objects: vec![],
                total_count: 0,
                next_cursor: None,
            })
        }

        async fn create_folder(
            &self,
            _name: &str,
            _public: bool,
        ) -> Result<(), storage_service::StorageError> {
            Ok(())
        }

        async fn delete_folder(&self, _name: &str) -> Result<(), storage_service::StorageError> {
            Ok(())
        }

        async fn list_folders(
            &self,
        ) -> Result<Vec<storage_service::FolderInfo>, storage_service::StorageError> {
            Ok(vec![])
        }
    }

    /// Build a `TestContext` with a real crypto block (share-token signing
    /// goes through `crypto::sign`) and a fake storage block whose `get`
    /// always succeeds (the file-existence check needs *some* answer), plus
    /// one bucket owned by `owner`. This is the minimum needed to drive
    /// `handle_create_share` past bucket/key validation, the ownership
    /// check, and the file-existence check, into the `expires_in_hours`
    /// handling under test — without it, every case below would stop early
    /// (PermissionDenied / NotFound) and never exercise the fix.
    async fn ctx_with_owned_bucket(bucket: &str, owner: &str) -> TestContext {
        let mut ctx = TestContext::with_files().await;

        let crypto_svc = Arc::new(
            wafer_block_crypto::service::Argon2JwtCryptoService::new(
                // ≥ 32 bytes for HMAC-SHA256 minimum-length check.
                "test-jwt-secret-padded-to-min-32-bytes-aaaa".to_string(),
            )
            .expect("test secret is long enough"),
        );
        ctx.register_block(
            "wafer-run/crypto",
            Arc::new(wafer_core::service_blocks::crypto::CryptoBlock::new(
                crypto_svc,
            )),
        );

        ctx.register_block(
            "wafer-run/storage",
            crate::blocks::storage::create(
                Arc::new(AlwaysFoundStorageService),
                Arc::from("impresspress/admin"),
            ),
        );

        let data = crate::util::json_map(serde_json::json!({
            "name": bucket,
            "public": false,
            "created_by": owner,
            "created_at": crate::util::now_rfc3339(),
        }));
        repo::buckets::seed(&ctx, data).await.expect("seed bucket");

        ctx
    }

    /// Regression (SEC-064): the share path used to inline its own bucket/key
    /// validation that OMITTED the backslash rejection, so a share could be
    /// created for a key the upload/download path (`is_valid_storage_key`)
    /// rejects. Now it routes through the shared validator and rejects the
    /// key before any ownership/existence lookup.
    ///
    /// The key here is a *backslash-only* key with NO `..` segment. This
    /// pins the actual SEC-064 drift: the old inline check rejected `..` but
    /// accepted a bare backslash, so a `..`-containing key (e.g. `a\..\secret`)
    /// would have been rejected by the old code too and would not prove the
    /// backslash branch. `a\secret` was *accepted* by the old inline check and
    /// is *rejected* only by the shared validator's `!key.contains('\\')` arm.
    #[tokio::test]
    async fn create_share_rejects_backslash_key() {
        let ctx = TestContext::with_files().await;
        let msg = auth_msg("create", "/b/cloudstorage/shares", "u1");
        let out = handle_create_share(
            &ctx,
            &msg,
            InputStream::from_bytes(share_body("photos", "a\\secret")),
        )
        .await;
        assert!(
            output_is_error(out, "InvalidArgument").await,
            "backslash key must be rejected (SEC-064)"
        );
    }

    /// The share path now enforces the same S3-compatible bucket-name rule as
    /// the rest of the block (and the client modal), so an uppercase /
    /// invalid bucket name is rejected up front.
    #[tokio::test]
    async fn create_share_rejects_invalid_bucket_name() {
        let ctx = TestContext::with_files().await;
        let msg = auth_msg("create", "/b/cloudstorage/shares", "u1");
        let out = handle_create_share(
            &ctx,
            &msg,
            InputStream::from_bytes(share_body("Bad/Bucket", "file.txt")),
        )
        .await;
        assert!(
            output_is_error(out, "InvalidArgument").await,
            "invalid bucket name must be rejected"
        );
    }

    /// The ownership check is the only authorization on share deletion. An
    /// outage on that lookup must stop the request, not skip the check.
    #[tokio::test]
    async fn delete_share_lookup_outage_does_not_delete() {
        let ctx = TestContext::with_files().await;
        let id = seed_share(&ctx, "u1").await;
        let failing =
            FailingDbOpContext::new(ctx.clone(), vec![("database.get", repo::shares::TABLE)]);

        let msg = routed(auth_msg(
            "delete",
            &format!("/b/cloudstorage/shares/{id}"),
            "u2",
        ));
        let out = handle_delete_share(&failing, &msg).await;

        assert!(
            output_is_error(out, "Internal").await,
            "an ownership lookup outage must not fall through to the delete"
        );
        assert!(
            repo::shares::find_by_id(&ctx, &id).await.is_ok(),
            "the share must survive a failed ownership check"
        );
    }

    #[tokio::test]
    async fn delete_share_by_non_owner_is_forbidden() {
        let ctx = TestContext::with_files().await;
        let id = seed_share(&ctx, "u1").await;

        let msg = routed(auth_msg(
            "delete",
            &format!("/b/cloudstorage/shares/{id}"),
            "u2",
        ));
        let out = handle_delete_share(&ctx, &msg).await;

        assert!(output_is_error(out, "PermissionDenied").await);
        assert!(repo::shares::find_by_id(&ctx, &id).await.is_ok());
    }

    #[tokio::test]
    async fn delete_missing_share_is_not_found() {
        let ctx = TestContext::with_files().await;
        let msg = routed(auth_msg(
            "delete",
            "/b/cloudstorage/shares/no-such-share",
            "u1",
        ));
        assert!(output_is_error(handle_delete_share(&ctx, &msg).await, "NotFound").await);
    }

    /// `/b/cloudstorage/quota` must not report zero usage during an outage.
    #[tokio::test]
    async fn quota_endpoint_surfaces_usage_outage() {
        let ctx = TestContext::with_files().await;
        let failing = FailingDbOpContext::new(ctx, vec![("database.sum", repo::objects::TABLE)]);

        let msg = auth_msg("retrieve", "/b/cloudstorage/quota", "u1");
        let out = handle_get_quota(&failing, &msg).await;

        assert!(
            output_is_error(out, "Internal").await,
            "a usage outage must surface as an error, not as zero usage"
        );
    }

    /// A valid key/bucket gets past validation and is denied only by the
    /// ownership check (the user owns no such bucket) — confirming the
    /// validator change didn't accidentally reject legitimate input.
    #[tokio::test]
    async fn create_share_valid_input_reaches_ownership_check() {
        let ctx = TestContext::with_files().await;
        let msg = auth_msg("create", "/b/cloudstorage/shares", "u1");
        let out = handle_create_share(
            &ctx,
            &msg,
            InputStream::from_bytes(share_body("my-bucket", "dir/file.txt")),
        )
        .await;
        // No bucket owned by u1 → PermissionDenied, NOT InvalidArgument.
        assert!(
            output_is_error(out, "PermissionDenied").await,
            "valid input should pass validation and hit the ownership check"
        );
    }

    /// SB-4: `expires_in_hours` used to be fed straight into
    /// `chrono::Duration::hours` and `now + duration`, both of which PANIC
    /// on overflow in chrono 0.4.44. A huge value on this authenticated,
    /// reachable request path must produce a 400, not a handler panic.
    #[tokio::test]
    async fn create_share_rejects_huge_expiry_without_panicking() {
        let ctx = ctx_with_owned_bucket("my-bucket", "u1").await;
        let msg = auth_msg("create", "/b/cloudstorage/shares", "u1");
        let body = serde_json::to_vec(&serde_json::json!({
            "bucket": "my-bucket",
            "key": "f",
            "expires_in_hours": i64::MAX,
        }))
        .unwrap();
        let out = handle_create_share(&ctx, &msg, InputStream::from_bytes(body)).await;
        assert!(
            output_is_error(out, "InvalidArgument").await,
            "huge expiry must be a 400, not a handler panic"
        );
    }

    /// Zero/negative hours would mint an already-expired share (or, for
    /// very negative values, also overflow the same arithmetic) — rejected
    /// the same as an out-of-range positive value.
    #[tokio::test]
    async fn create_share_rejects_non_positive_expiry() {
        for hours in [0_i64, -1, i64::MIN] {
            let ctx = ctx_with_owned_bucket("my-bucket", "u1").await;
            let msg = auth_msg("create", "/b/cloudstorage/shares", "u1");
            let body = serde_json::to_vec(&serde_json::json!({
                "bucket": "my-bucket",
                "key": "f",
                "expires_in_hours": hours,
            }))
            .unwrap();
            let out = handle_create_share(&ctx, &msg, InputStream::from_bytes(body)).await;
            assert!(
                output_is_error(out, "InvalidArgument").await,
                "non-positive expires_in_hours ({hours}) must be rejected"
            );
        }
    }

    /// The range/overflow guard must not reject legitimate input: a normal
    /// in-range value still produces a share whose persisted `expires_at`
    /// is a correct ~24h-out timestamp.
    #[tokio::test]
    async fn create_share_valid_expiry_produces_future_timestamp() {
        let ctx = ctx_with_owned_bucket("my-bucket", "u1").await;
        let msg = auth_msg("create", "/b/cloudstorage/shares", "u1");
        let body = serde_json::to_vec(&serde_json::json!({
            "bucket": "my-bucket",
            "key": "f",
            "expires_in_hours": 24,
        }))
        .unwrap();
        let before = chrono::Utc::now();
        let out = handle_create_share(&ctx, &msg, InputStream::from_bytes(body)).await;
        let resp = output_json(out).await;
        let id = resp
            .get("id")
            .and_then(|v| v.as_str())
            .expect("successful create_share returns an id")
            .to_string();

        let row = repo::shares::find_by_id(&ctx, &id)
            .await
            .expect("share row");
        let expires_at = row
            .expires_at
            .as_deref()
            .expect("expires_at set for a 24h share");
        let parsed = chrono::DateTime::parse_from_rfc3339(expires_at)
            .expect("valid rfc3339")
            .with_timezone(&chrono::Utc);
        let expected_min = before + chrono::Duration::hours(23);
        let expected_max = before + chrono::Duration::hours(25);
        assert!(
            parsed > expected_min && parsed < expected_max,
            "expires_at should be ~24h in the future, got {expires_at}"
        );
    }
}
