//! Row-level access over `wafer_run__auth__rate_limits`.
//!
//! Sliding-window counters keyed by user/IP, written on the Cloudflare
//! (`wasm32`) path only — the native `UserRateLimiter` keeps its counters in
//! an in-memory `Mutex<HashMap>` and never touches the database.
//!
//! [`windowed_increment`] is the single fixed-window upsert: the identical
//! `db::upsert` + read-back pair used to be copied between
//! `blocks/rate_limit.rs` and `blocks/tickets/abuse.rs`. It takes `now` from
//! the caller rather than reading a clock, because both call sites are
//! `cfg(target_arch = "wasm32")` (`std::time` panics there and `js_sys` does
//! not exist on the host) — a parameter is what lets the shared upsert be
//! compiled, and tested, on the host.
use serde_json::{json, Value};
use wafer_block::{
    db::{Filter, FilterOp},
    wire::database::OnConflict,
};
use wafer_core::clients::database as db;
use wafer_run::{context::Context, WaferError};

use super::db_failed;

pub const TABLE: &str = "wafer_run__auth__rate_limits";

/// Increment `key`'s counter inside the fixed window that ends at `now`, and
/// return the counter's value afterwards.
///
/// One atomic `INSERT … ON CONFLICT DO UPDATE` (the server renders the
/// dialect-portable `CASE WHEN` from `OnConflict::WindowedCounter`), then one
/// read-back of the current window's row. A window older than
/// `window_cutoff` resets the counter instead of incrementing it.
///
/// `id` is the caller's row id for a brand-new counter; on conflict the
/// existing row's id wins, so it only has to be unique, and the two callers
/// derive it from their own key namespace.
///
/// An absent read-back row is `Ok(0)`, not an error: the upsert can only have
/// landed outside the window, which is the same as "no requests in this
/// window yet". A failure of either round-trip surfaces as `Err`, naming
/// which half failed, and the caller decides whether to fail open.
pub async fn windowed_increment(
    ctx: &dyn Context,
    id: &str,
    key: &str,
    now: i64,
    window_cutoff: i64,
) -> Result<i64, WaferError> {
    db::upsert(
        ctx,
        TABLE,
        vec![
            ("id".to_string(), json!(id)),
            ("key".to_string(), json!(key)),
        ],
        vec!["key".to_string()],
        OnConflict::WindowedCounter {
            count_field: "count".to_string(),
            window_field: "window_start".to_string(),
            now,
            window_cutoff,
            created_fields: vec!["created_at".to_string()],
            updated_fields: vec!["updated_at".to_string()],
        },
    )
    .await
    .map_err(|e| prefixed(e, "rate_limits windowed upsert"))?;

    let rows = db::list_all(
        ctx,
        TABLE,
        vec![
            Filter {
                field: "key".into(),
                operator: FilterOp::Equal,
                value: json!(key),
            },
            Filter {
                field: "window_start".into(),
                operator: FilterOp::GreaterEqual,
                value: json!(window_cutoff),
            },
        ],
    )
    .await
    .map_err(|e| prefixed(e, "rate_limits count read-back"))?;

    Ok(rows
        .first()
        .and_then(|r| r.data.get("count"))
        .and_then(Value::as_i64)
        .unwrap_or(0))
}

/// Drop counter rows whose `updated_at` is strictly before `cutoff`, and
/// report how many went. The retention sweep behind ticket maintenance.
pub async fn delete_updated_before(ctx: &dyn Context, cutoff: &str) -> Result<i64, WaferError> {
    db::delete_by_filters_count(
        ctx,
        TABLE,
        vec![Filter {
            field: "updated_at".to_string(),
            operator: FilterOp::LessThan,
            value: json!(cutoff),
        }],
    )
    .await
    .map_err(|e| db_failed("rate_limits prune", e))
}

fn prefixed(e: WaferError, what: &str) -> WaferError {
    WaferError {
        code: e.code,
        message: format!("{what}: {}", e.message),
        meta: e.meta,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::test_support::TestContext;

    async fn ctx() -> TestContext {
        TestContext::with_auth().await
    }

    /// The behaviour both call sites depend on and neither could test:
    /// repeated hits inside one window accumulate, and a hit whose window
    /// has rolled over starts again from one.
    #[tokio::test]
    async fn increments_within_the_window_and_resets_after_it() {
        let ctx = ctx().await;
        let now = 1_000_000i64;
        let window = 60i64;

        assert_eq!(
            windowed_increment(&ctx, "id-1", "k", now, now - window)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            windowed_increment(&ctx, "id-2", "k", now + 1, now + 1 - window)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            windowed_increment(&ctx, "id-3", "k", now + 5, now + 5 - window)
                .await
                .unwrap(),
            3
        );

        // Same key, a window that started after the stored `window_start`:
        // the counter resets rather than carrying the old total forward.
        let later = now + 10 * window;
        assert_eq!(
            windowed_increment(&ctx, "id-4", "k", later, later - window)
                .await
                .unwrap(),
            1,
            "a hit past the window cutoff must reset the counter"
        );
    }

    #[tokio::test]
    async fn keys_are_counted_independently() {
        let ctx = ctx().await;
        let now = 2_000_000i64;
        let cutoff = now - 60;
        windowed_increment(&ctx, "a1", "a", now, cutoff)
            .await
            .unwrap();
        windowed_increment(&ctx, "a2", "a", now, cutoff)
            .await
            .unwrap();
        assert_eq!(
            windowed_increment(&ctx, "b1", "b", now, cutoff)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn prune_deletes_only_rows_older_than_the_cutoff() {
        let ctx = ctx().await;
        let mut old: HashMap<String, Value> = HashMap::new();
        old.insert("id".into(), json!("old"));
        old.insert("key".into(), json!("old-key"));
        old.insert("count".into(), json!(1));
        old.insert("window_start".into(), json!(0));
        old.insert("created_at".into(), json!("2026-01-01T00:00:00Z"));
        old.insert("updated_at".into(), json!("2026-01-01T00:00:00Z"));
        db::create(&ctx, TABLE, old).await.unwrap();

        let mut fresh: HashMap<String, Value> = HashMap::new();
        fresh.insert("id".into(), json!("fresh"));
        fresh.insert("key".into(), json!("fresh-key"));
        fresh.insert("count".into(), json!(1));
        fresh.insert("window_start".into(), json!(0));
        fresh.insert("created_at".into(), json!("2026-06-01T00:00:00Z"));
        fresh.insert("updated_at".into(), json!("2026-06-01T00:00:00Z"));
        db::create(&ctx, TABLE, fresh).await.unwrap();

        assert_eq!(
            delete_updated_before(&ctx, "2026-03-01T00:00:00Z")
                .await
                .unwrap(),
            1
        );
        let left = db::list_all(&ctx, TABLE, vec![]).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, "fresh");
    }
}
