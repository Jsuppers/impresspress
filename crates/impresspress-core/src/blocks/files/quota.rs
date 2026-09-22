use wafer_run::{context::Context, ErrorCode, OutputStream, WaferError};

use super::{contracts::QuotaUsageView, models::QuotaConfig, repo};
use crate::{
    http::{err_bad_request, err_internal},
    streaming::MAX_REQUEST_BODY_BYTES,
};

/// The user's effective quota: their override row when one exists, otherwise
/// the block defaults. Only a missing row means "defaults" — any other lookup
/// failure is returned, because treating an outage as "no override" would
/// silently lift an admin-lowered cap.
///
/// Either way the per-file cap is one an upload can reach:
/// [`repo::quota::QuotaRow::from_record`] clamps a stored row, and
/// [`QuotaConfig::effective_default`] is the clamped form of the defaults.
pub async fn get_user_quota(ctx: &dyn Context, user_id: &str) -> Result<QuotaConfig, WaferError> {
    match repo::quota::find_for_user(ctx, user_id).await {
        Ok(row) => Ok(row.config),
        Err(e) if e.code == ErrorCode::NotFound => Ok(QuotaConfig::effective_default()),
        Err(e) => Err(e),
    }
}

/// Lower `max_file_size_bytes` to [`MAX_REQUEST_BODY_BYTES`] when the stored
/// policy asks for more than an upload can carry.
///
/// The stored cap — default 100 MiB, admin-editable per user — is a policy
/// about stored objects; [`MAX_REQUEST_BODY_BYTES`] is the hard ceiling on a
/// request body, enforced by the transport before this block is reached, and
/// no transport streams a request body today. A stored cap above it is
/// unreachable: the upload is refused with a 413 the block never sees, so the
/// number the block reports and the number it enforces would describe
/// different limits. Clamping makes the advertised cap the enforced one, and
/// it happens where a stored row is decoded
/// ([`repo::quota::QuotaRow::from_record`]) so that every reader agrees: the
/// upload's own size check and its error message, [`check_quota`], the quota
/// endpoint, the admin quotas table, and the row an admin update echoes back.
///
/// Only the per-file cap is clamped. `max_storage_bytes` and the file count
/// are about accumulated objects, which no single request has to carry.
pub fn clamp_to_transport(mut config: QuotaConfig) -> QuotaConfig {
    let transport_ceiling = MAX_REQUEST_BODY_BYTES as i64;
    if config.max_file_size_bytes > transport_ceiling {
        config.max_file_size_bytes = transport_ceiling;
    }
    config
}

/// Total bytes used by `user_id`, computed as `SUM(size)` over the user's
/// object rows ([`repo::objects::sum_size_for_uploader`], no row
/// materialization).
pub async fn get_used_bytes(ctx: &dyn Context, user_id: &str) -> Result<i64, WaferError> {
    Ok(repo::objects::sum_size_for_uploader(ctx, user_id).await? as i64)
}

/// Number of object rows owned by `user_id`, across every bucket — the
/// usage figure the quota endpoint reports. The file-count cap is per bucket
/// and is checked against [`repo::objects::count_for_uploader_in_bucket`].
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

/// Admit or refuse an upload of `file_size` bytes into `bucket` for
/// `user_id`.
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
/// The storage cap is over everything `user_id` stores; the file-count cap
/// is per bucket, over the rows `user_id` holds in `bucket`
/// ([`repo::objects::count_for_uploader_in_bucket`]), so being at the limit
/// in one bucket does not stop an upload into another. Both count `pending`
/// rows: a pending row is an upload in flight that will add a file (and its
/// bytes) once it settles. A pending row left behind by a failed upload stops
/// counting once [`sweep_stale_pending`], which the upload handler runs before
/// this check, removes it.
///
/// Counting pending rows narrows the check-to-write race; it does not close
/// it. The reservation is inserted before the storage write, so an upload
/// whose check runs after another upload's reservation has landed sees it and
/// is refused, and the window shrinks from check → `mark_complete` (which
/// spans the storage write) to check → reservation insert. But this check (a
/// `count` and a `sum`) and [`repo::objects::reserve_upload`] are separate
/// database calls with no transaction or lock between them, and in-flight
/// uploads are exclusive per `(bucket, key)`, not per bucket. Uploads of
/// different keys whose checks all run before any of them reserves each see
/// the same usage and are all admitted, so either cap can be exceeded by the
/// uploads in flight at that moment. Atomic enforcement is tracked in
/// `NICE_TO_HAVE.md`, "Files quota caps are not enforced atomically".
///
/// Fails closed: if the quota or the current usage cannot be read, the
/// upload is refused with an internal error rather than admitted against
/// the defaults or against zero usage.
pub async fn check_quota(
    ctx: &dyn Context,
    user_id: &str,
    bucket: &str,
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
        let file_count = repo::objects::count_for_uploader_in_bucket(ctx, user_id, bucket)
            .await
            .map_err(|e| err_internal("Quota usage lookup failed", e))?;
        if file_count >= quota.max_files_per_bucket {
            return Err(err_bad_request(&format!(
                "File count limit reached for this bucket (max {})",
                quota.max_files_per_bucket
            )));
        }
    }

    Ok(())
}

/// Sweep the given user's `pending`-status object rows older than
/// [`repo::objects::PENDING_RESERVATION_TTL_SECONDS`]. A row is claimed
/// `pending` before the actual storage upload so that a later quota check
/// counts it (see [`check_quota`] for what that does and does not bound),
/// and two failures can leave one behind: the upload errored AND
/// `release_reservation` errored too, or the upload succeeded but
/// `mark_complete` could not record it. Either way the row would otherwise
/// inflate that user's quota usage forever. Calling this best-effort on each
/// new upload keeps the table self-healing without a separate cron.
///
/// Until it is swept, such a row also holds the key: `reserve_upload` cannot
/// tell it from an upload still in flight, so an upload of that key is
/// refused as in progress until the row passes the TTL. The uploader's next
/// upload after that sweeps the row first and claims the key afresh.
///
/// It reclaims the ROW, not the blob. A swept row whose upload had in fact
/// reached storage leaves that object behind, unreferenced and charged to
/// nobody until a new upload of the key overwrites it — see
/// `NICE_TO_HAVE.md`, "The files-block pending sweep does not reclaim the
/// blob".
pub async fn sweep_stale_pending(ctx: &dyn Context, user_id: &str) {
    let cutoff = repo::objects::pending_reservation_cutoff();
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
        // materializes them; either way the value matches the const.) The
        // per-file default is above the transport's request-body ceiling, so
        // what comes back is the ceiling — see
        // `the_per_file_cap_is_clamped_to_what_a_request_body_can_carry`.
        assert_eq!(
            quota.max_file_size_bytes, MAX_REQUEST_BODY_BYTES as i64,
            "the default 100 MiB is clamped to the transport ceiling"
        );
    }

    /// **Fails on the pre-fix tree.** The block advertised a 100 MiB per-file
    /// cap that no request body could reach: every transport buffers the body
    /// under `streaming::MAX_REQUEST_BODY_BYTES` and refuses anything larger
    /// before this block runs. Reading the quota now yields the enforced
    /// number, so the upload check, its error message and the admin table all
    /// describe the same limit.
    #[tokio::test]
    async fn the_per_file_cap_is_clamped_to_what_a_request_body_can_carry() {
        let ctx = TestContext::with_files().await;
        let mut row: HashMap<String, serde_json::Value> = HashMap::new();
        row.insert("user_id".into(), json!("u1"));
        // An admin raising the cap cannot raise the transport's.
        row.insert("max_file_size_bytes".into(), json!(500 * 1024 * 1024));
        repo::quota::seed(&ctx, row).await.expect("seed quota");

        let quota = get_user_quota(&ctx, "u1").await.expect("quota lookup");
        assert_eq!(quota.max_file_size_bytes, MAX_REQUEST_BODY_BYTES as i64);
    }

    /// A cap BELOW the ceiling is policy and is left alone — clamping is a
    /// ceiling, not a floor, and an admin-lowered limit still lowers.
    #[tokio::test]
    async fn a_cap_below_the_transport_ceiling_is_untouched() {
        let ctx = TestContext::with_files().await;
        let mut row: HashMap<String, serde_json::Value> = HashMap::new();
        row.insert("user_id".into(), json!("u1"));
        row.insert("max_file_size_bytes".into(), json!(4096));
        repo::quota::seed(&ctx, row).await.expect("seed quota");

        let quota = get_user_quota(&ctx, "u1").await.expect("quota lookup");
        assert_eq!(quota.max_file_size_bytes, 4096);
        // And the other caps are about accumulated storage, not one request,
        // so the transport ceiling has nothing to say about them.
        assert_eq!(
            quota.max_storage_bytes,
            QuotaConfig::DEFAULT_MAX_STORAGE_BYTES
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

        assert!(check_quota(&ctx, "u1", "photos", 1024, None).await.is_ok());
        assert!(
            check_quota(&ctx, "u1", "photos", 4096, None).await.is_err(),
            "file above the override cap must be rejected"
        );
    }

    /// An outage on the override lookup must not admit the upload under
    /// the default quota: an admin-lowered cap would silently revert.
    #[tokio::test]
    async fn check_quota_fails_closed_when_override_lookup_errors() {
        let ctx = TestContext::with_files().await;
        let failing = FailingDbOpContext::new(ctx, vec![("database.list", repo::quota::TABLE)]);

        let out = check_quota(&failing, "u1", "photos", 1, None)
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

        let out = check_quota(&failing, "u1", "photos", 1, None)
            .await
            .expect_err("a usage lookup outage must not admit the upload");

        assert!(
            output_is_error(out, "Internal").await,
            "the outage must surface as an error, not as a quota verdict"
        );
    }
}
