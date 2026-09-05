//! The admin stats endpoint, `GET /b/storage/admin/api/stats`: declared
//! `Admin` in the block's route table and gated by the router from that
//! declaration. Until this PR it was reached through the admin block's
//! `call_block` delegation on a synthetic path that never existed on the wire.

use wafer_run::{context::Context, Message, OutputStream};

use crate::{blocks::files::repo, http::ok_json};

pub(in crate::blocks::files) async fn handle_stats(
    ctx: &dyn Context,
    _msg: &Message,
) -> OutputStream {
    let total_objects = repo::objects::count_completed(ctx).await.unwrap_or(0);
    let total_size = repo::objects::sum_size_completed(ctx).await.unwrap_or(0.0);
    // Count buckets from the metadata table (single source of truth), the same
    // way the admin SSR overview does, rather than enumerating storage folders.
    let bucket_count = repo::buckets::count_all(ctx).await.unwrap_or(0);

    ok_json(&serde_json::json!({
        "total_objects": total_objects,
        "total_size_bytes": total_size as i64,
        "bucket_count": bucket_count
    }))
}

#[cfg(test)]
mod integration_tests {
    use super::{super::test_helpers::seed_bucket, *};
    use crate::test_support::{admin_msg, output_json, TestContext};

    /// `handle_stats` counts buckets from [`repo::buckets::TABLE`] (the same source
    /// admin SSR overview uses), not by enumerating storage folders.
    #[tokio::test]
    async fn stats_counts_buckets_from_metadata_table() {
        let ctx = TestContext::with_files().await;
        seed_bucket(&ctx, "one", "alice").await;
        seed_bucket(&ctx, "two", "bob").await;

        let out = handle_stats(&ctx, &admin_msg("retrieve", "/b/storage/admin/api/stats")).await;
        let body = output_json(out).await;
        assert_eq!(body.get("bucket_count").and_then(|v| v.as_i64()), Some(2));
    }
}
