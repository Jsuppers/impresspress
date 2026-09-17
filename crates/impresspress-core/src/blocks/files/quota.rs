use wafer_run::{context::Context, ErrorCode, OutputStream, WaferError};

use super::{contracts::QuotaUsageView, models::QuotaConfig, repo};
use crate::http::{err_bad_request, err_internal};

/// The user's effective quota: their override row when one exists,
/// otherwise the block defaults. Only a missing row means "defaults" — any
/// other lookup failure is returned, because treating an outage as "no
/// override" would silently lift an admin-lowered cap.
pub async fn get_user_quota(ctx: &dyn Context, user_id: &str) -> Result<QuotaConfig, WaferError> {
    match repo::quota::find_for_user(ctx, user_id).await {
        Ok(row) => Ok(row.config),
        Err(e) if e.code == ErrorCode::NotFound => Ok(QuotaConfig::default()),
        Err(e) => Err(e),
    }
}

/// Total bytes used by `user_id`, computed as `SUM(size)` over the user's
/// object rows ([`repo::objects::sum_size_for_uploader`], no row
/// materialization).
pub async fn get_used_bytes(ctx: &dyn Context, user_id: &str) -> Result<i64, WaferError> {
    Ok(repo::objects::sum_size_for_uploader(ctx, user_id).await? as i64)
}

/// Number of object rows owned by `user_id`.
pub async fn get_file_count(ctx: &dyn Context, user_id: &str) -> Result<i64, WaferError> {
    repo::objects::count_for_uploader(ctx, user_id).await
}

/// Usage summary as exposed by the `/b/cloudstorage/quota` JSON endpoint.
pub async fn get_user_usage(
    ctx: &dyn Context,
    user_id: &str,
) -> Result<QuotaUsageView, WaferError> {
    Ok(QuotaUsageView {
        total_bytes: get_used_bytes(ctx, user_id).await?,
        file_count: get_file_count(ctx, user_id).await?,
    })
}

/// Admit or refuse an upload of `file_size` bytes for `user_id`.
///
/// `replaces_own_bytes` is `Some(size)` when the upload overwrites an object
/// this same user already stores under the key — the size of that object, as
/// it is still counted in `user_id`'s usage. It is what makes an overwrite
/// cost the *difference*: replacing a 100 MB file with a 1 MB one frees
/// 99 MB, and neither one adds a file to the count. `None` — a new key, or a
/// key whose current row belongs to someone else, whose bytes are in THAT
/// user's usage and not in this one's — charges the full size and one more
/// file.
///
/// Fails closed: if the quota or the current usage cannot be read, the
/// upload is refused with an internal error rather than admitted against
/// the defaults or against zero usage.
pub async fn check_quota(
    ctx: &dyn Context,
    user_id: &str,
    file_size: i64,
    replaces_own_bytes: Option<i64>,
) -> Result<(), OutputStream> {
    let quota = get_user_quota(ctx, user_id)
        .await
        .map_err(|e| err_internal("Quota lookup failed", e))?;

    // The per-file cap is about this file, not about the net change: a file
    // over the limit is refused however much the one it replaces frees.
    if file_size > quota.max_file_size_bytes {
        return Err(err_bad_request(&format!(
            "File exceeds maximum size of {} bytes",
            quota.max_file_size_bytes
        )));
    }

    let current_bytes = get_used_bytes(ctx, user_id)
        .await
        .map_err(|e| err_internal("Quota usage lookup failed", e))?;
    if current_bytes + file_size - replaces_own_bytes.unwrap_or(0) > quota.max_storage_bytes {
        return Err(err_bad_request("Storage quota exceeded"));
    }

    // An overwrite adds no row, so a user already at the file-count limit can
    // still replace what they have.
    if quota.max_files_per_bucket > 0 && replaces_own_bytes.is_none() {
        let file_count = get_file_count(ctx, user_id)
            .await
            .map_err(|e| err_internal("Quota usage lookup failed", e))?;
        if file_count >= quota.max_files_per_bucket {
            return Err(err_bad_request(&format!(
                "File count limit reached (max {})",
                quota.max_files_per_bucket
            )));
        }
    }

    Ok(())
}

/// Sweep `pending`-status object rows older than `older_than_seconds` for
/// the given user. A row is claimed `pending` before the actual storage
/// upload to close the quota TOCTOU window, and two failures can leave one
/// behind: the upload errored AND `release_reservation` errored too, or the
/// upload succeeded but `mark_complete` could not record it (which the
/// uploader is told about, so their retry re-claims the same row). Either way
/// the row would otherwise inflate that user's quota usage forever. Calling
/// this best-effort on each new upload keeps the table self-healing without a
/// separate cron.
///
/// 1 hour is a comfortable cutoff: the largest realistic upload finishes
/// inside that window, and anything still pending afterward is almost
/// certainly an orphan.
pub async fn sweep_stale_pending(ctx: &dyn Context, user_id: &str, older_than_seconds: i64) {
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(older_than_seconds)).to_rfc3339();
    if let Err(e) = repo::objects::delete_stale_pending(ctx, user_id, &cutoff).await {
        tracing::warn!(error = %e, user_id = %user_id, "failed to sweep stale pending uploads");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;
    use crate::test_support::{output_is_error, FailingDbOpContext, TestContext};

    #[tokio::test]
    async fn get_user_quota_returns_defaults_without_override_row() {
        let ctx = TestContext::with_files().await;
        let quota = get_user_quota(&ctx, "nobody")
            .await
            .expect("no override row means the defaults, not an error");
        assert_eq!(
            quota.max_storage_bytes,
            QuotaConfig::DEFAULT_MAX_STORAGE_BYTES
        );
    }

    #[tokio::test]
    async fn get_user_quota_applies_override_row() {
        let ctx = TestContext::with_files().await;
        let mut row: HashMap<String, serde_json::Value> = HashMap::new();
        row.insert("user_id".into(), json!("u1"));
        row.insert("max_storage_bytes".into(), json!(2048));
        repo::quota::seed(&ctx, row).await.expect("seed quota");

        let quota = get_user_quota(&ctx, "u1").await.expect("quota lookup");
        assert_eq!(quota.max_storage_bytes, 2048);
        // Fields without an explicit override keep the defaults. (The
        // migration declares DB-side column defaults, so a full row insert
        // materializes them; either way the value matches the const.)
        assert_eq!(
            quota.max_file_size_bytes,
            QuotaConfig::DEFAULT_MAX_FILE_SIZE_BYTES
        );
    }

    #[tokio::test]
    async fn get_used_bytes_sums_object_sizes_per_user() {
        let ctx = TestContext::with_files().await;
        for (key, size, owner) in [("a", 1024, "u1"), ("b", 1024, "u1"), ("c", 4096, "u2")] {
            let mut row: HashMap<String, serde_json::Value> = HashMap::new();
            row.insert("bucket".into(), json!("photos"));
            row.insert("key".into(), json!(key));
            row.insert("size".into(), json!(size));
            row.insert("uploaded_by".into(), json!(owner));
            repo::objects::seed(&ctx, row).await.expect("seed");
        }

        assert_eq!(get_used_bytes(&ctx, "u1").await.expect("usage"), 2048);
        assert_eq!(get_used_bytes(&ctx, "u2").await.expect("usage"), 4096);
        assert_eq!(get_used_bytes(&ctx, "u3").await.expect("usage"), 0);
        assert_eq!(get_file_count(&ctx, "u1").await.expect("count"), 2);
    }

    /// End-to-end: an override row caps enforcement, so a file that fits
    /// the default 1 GiB quota but not the override is rejected.
    #[tokio::test]
    async fn check_quota_enforces_override_storage_cap() {
        let ctx = TestContext::with_files().await;
        let mut row: HashMap<String, serde_json::Value> = HashMap::new();
        row.insert("user_id".into(), json!("u1"));
        row.insert("max_storage_bytes".into(), json!(2048));
        repo::quota::seed(&ctx, row).await.expect("seed quota");

        assert!(check_quota(&ctx, "u1", 1024, None).await.is_ok());
        assert!(
            check_quota(&ctx, "u1", 4096, None).await.is_err(),
            "file above the override cap must be rejected"
        );
    }

    /// An outage on the override lookup must not admit the upload under
    /// the default quota: an admin-lowered cap would silently revert.
    #[tokio::test]
    async fn check_quota_fails_closed_when_override_lookup_errors() {
        let ctx = TestContext::with_files().await;
        let failing = FailingDbOpContext::new(ctx, vec![("database.list", repo::quota::TABLE)]);

        let out = check_quota(&failing, "u1", 1, None)
            .await
            .expect_err("an override lookup outage must not admit the upload");

        assert!(
            output_is_error(out, "Internal").await,
            "the outage must surface as an error, not as a quota verdict"
        );
    }

    /// An outage on the usage sum must not admit the upload as if the user
    /// had nothing stored.
    #[tokio::test]
    async fn check_quota_fails_closed_when_usage_lookup_errors() {
        let ctx = TestContext::with_files().await;
        let failing = FailingDbOpContext::new(ctx, vec![("database.sum", repo::objects::TABLE)]);

        let out = check_quota(&failing, "u1", 1, None)
            .await
            .expect_err("a usage lookup outage must not admit the upload");

        assert!(
            output_is_error(out, "Internal").await,
            "the outage must surface as an error, not as a quota verdict"
        );
    }
}
