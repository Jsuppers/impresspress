//! Pluggable KV backend trait + config-version-stamp retry helper for the
//! Cloudflare config-var hot path.
//!
//! Lives in `impresspress-core` (not `impresspress-cloudflare`) so the retry logic is
//! host-testable: `impresspress-cloudflare` is wasm32-only and excluded from
//! `cargo test --workspace`. Follows the `cache_key` extraction precedent —
//! pure logic that a wasm-only crate would otherwise leave untested moves
//! here.
//!
//! The production [`KvBackend`] impl (`WorkerKvBackend`, over
//! `worker::kv::KvStore`) and the `KvCachedD1DatabaseService` that consume this
//! trait stay in `impresspress-cloudflare`, which owns the `worker` dependency.

use wafer_block::{MaybeSend, MaybeSync};

/// Pluggable KV backend. Production (`impresspress-cloudflare`) uses
/// `worker::kv::KvStore` via `WorkerKvBackend`; host tests use the in-module
/// `MockKvBackend` (see the `#[cfg(test)] mod tests` below).
///
/// All errors are returned as `String` and never propagate as a hard
/// failure to callers — `KvCachedD1DatabaseService` treats every KV
/// error as a cache miss and falls through to the underlying database.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait KvBackend: MaybeSend + MaybeSync {
    /// Returns `Ok(Some(value))` on cache hit, `Ok(None)` on miss,
    /// `Err(msg)` on KV transport error.
    async fn get(&self, key: &str) -> Result<Option<String>, String>;

    /// Writes `value` under `key` with the given TTL (seconds).
    async fn put_with_ttl(&self, key: &str, value: &str, ttl_secs: u64) -> Result<(), String>;

    /// Writes `value` under `key` with no expiration. Reserved for the
    /// config-version stamp — row-cache entries always carry a TTL.
    async fn put(&self, key: &str, value: &str) -> Result<(), String>;

    /// Deletes `key`. Deleting a non-existent key is not an error.
    async fn delete(&self, key: &str) -> Result<(), String>;
}

/// PUT `stamp` under [`crate::cache_key::CONFIG_VERSION_KEY`] persistently (no
/// TTL — a quiet day must not expire the stamp and trigger a fleet-wide
/// restamp), retrying once on KV transport error.
///
/// On a double failure the returned error names both attempts
/// (`"first attempt: …; retry: …"`) so log correlation can distinguish a
/// one-off transient blip (which the retry papers over silently) from a
/// sustained KV outage.
pub async fn put_version_stamp_with_retry(kv: &dyn KvBackend, stamp: &str) -> Result<(), String> {
    match kv.put(crate::cache_key::CONFIG_VERSION_KEY, stamp).await {
        Ok(()) => Ok(()),
        Err(first) => kv
            .put(crate::cache_key::CONFIG_VERSION_KEY, stamp)
            .await
            .map_err(|second| format!("first attempt: {first}; retry: {second}")),
    }
}

/// True when `fresh` must be written to KV because what is cached there
/// (`cached`, `None` for a miss) does not already say the same thing.
///
/// Exists because a read-through row-cache rebuild re-PUTs every key it
/// touches whether or not the contents moved. Config almost never changes
/// across a rebuild, so those PUTs are near-100% redundant — and on
/// Cloudflare's KV free tier writes are the scarce resource (1k/day, versus
/// 100k reads), so a fleet-wide rebuild after a deploy could exhaust the
/// daily write budget rewriting bytes KV already held.
///
/// The compare is SEMANTIC, not textual: `Record::data` is a `HashMap`, so
/// the same rows serialise with different key order in different isolates
/// and a byte compare would report a change every time, restoring the exact
/// amplification this removes. JSON arrays still compare order-sensitively —
/// row order is part of the cached value.
///
/// Fails open: an absent, corrupt, or unparseable entry returns `true` so a
/// broken compare can never suppress a needed write and strand every isolate
/// on stale config.
pub fn needs_cache_write(cached: Option<&str>, fresh: &str) -> bool {
    let Some(cached) = cached else {
        return true;
    };
    match (
        serde_json::from_str::<serde_json::Value>(cached),
        serde_json::from_str::<serde_json::Value>(fresh),
    ) {
        (Ok(cached), Ok(fresh)) => cached != fresh,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Which `put()` calls a [`MockKvBackend`] fails, to drive both retry
    /// arms of [`put_version_stamp_with_retry`].
    enum PutFail {
        /// Every `put()` succeeds.
        None,
        /// Only the first `put()` fails; the retry succeeds.
        FirstOnly,
        /// Every `put()` fails.
        All,
    }

    /// In-memory [`KvBackend`] that counts `put()` calls and fails them per
    /// [`PutFail`]. Only `put()` is exercised by the retry helper; the other
    /// trait methods are unreachable on this path.
    struct MockKvBackend {
        puts: AtomicUsize,
        fail: PutFail,
    }

    impl MockKvBackend {
        fn new(fail: PutFail) -> Self {
            Self {
                puts: AtomicUsize::new(0),
                fail,
            }
        }

        fn put_count(&self) -> usize {
            self.puts.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl KvBackend for MockKvBackend {
        async fn get(&self, _key: &str) -> Result<Option<String>, String> {
            unreachable!("put_version_stamp_with_retry never reads")
        }

        async fn put_with_ttl(
            &self,
            _key: &str,
            _value: &str,
            _ttl_secs: u64,
        ) -> Result<(), String> {
            unreachable!("put_version_stamp_with_retry uses put(), not put_with_ttl()")
        }

        async fn put(&self, _key: &str, _value: &str) -> Result<(), String> {
            // 0-based index of this call (value before the increment).
            let n = self.puts.fetch_add(1, Ordering::SeqCst);
            let fail = match self.fail {
                PutFail::None => false,
                PutFail::FirstOnly => n == 0,
                PutFail::All => true,
            };
            if fail {
                Err(format!("kv transport error on put #{}", n + 1))
            } else {
                Ok(())
            }
        }

        async fn delete(&self, _key: &str) -> Result<(), String> {
            unreachable!("put_version_stamp_with_retry never deletes")
        }
    }

    #[tokio::test]
    async fn retry_recovers_when_first_put_fails() {
        let kv = MockKvBackend::new(PutFail::FirstOnly);
        let result = put_version_stamp_with_retry(&kv, "v-abc123").await;
        assert!(
            result.is_ok(),
            "a single transient failure must be recovered by the retry: {result:?}"
        );
        assert_eq!(
            kv.put_count(),
            2,
            "must attempt exactly twice (initial put + one retry)"
        );
    }

    #[tokio::test]
    async fn both_puts_failing_returns_combined_error() {
        let kv = MockKvBackend::new(PutFail::All);
        let err = put_version_stamp_with_retry(&kv, "v-abc123")
            .await
            .expect_err("both attempts fail → Err");
        assert!(
            err.contains("first attempt"),
            "error must name the first attempt for log correlation: {err}"
        );
        assert!(
            err.contains("retry"),
            "error must name the retry for log correlation: {err}"
        );
        assert_eq!(
            kv.put_count(),
            2,
            "must stop after exactly one retry, not loop"
        );
    }

    #[tokio::test]
    async fn first_put_succeeding_does_not_retry() {
        let kv = MockKvBackend::new(PutFail::None);
        let result = put_version_stamp_with_retry(&kv, "v-abc123").await;
        assert!(result.is_ok());
        assert_eq!(
            kv.put_count(),
            1,
            "a successful first put must not trigger the retry"
        );
    }

    // -----------------------------------------------------------------
    // needs_cache_write
    // -----------------------------------------------------------------

    #[test]
    fn identical_payload_needs_no_write() {
        let payload = r#"[{"block":"WAFER_RUN__WEB","key":"PORT","value":"8080"}]"#;
        assert!(
            !needs_cache_write(Some(payload), payload),
            "re-PUTting a payload KV already holds is the write amplification \
             this function exists to stop"
        );
    }

    #[test]
    fn absent_cache_entry_needs_a_write() {
        let payload = r#"[{"block":"WAFER_RUN__WEB","key":"PORT","value":"8080"}]"#;
        assert!(
            needs_cache_write(None, payload),
            "a cold/expired key must still be populated"
        );
    }

    #[test]
    fn changed_payload_needs_a_write() {
        let cached = r#"[{"block":"WAFER_RUN__WEB","key":"PORT","value":"8080"}]"#;
        let fresh = r#"[{"block":"WAFER_RUN__WEB","key":"PORT","value":"9090"}]"#;
        assert!(
            needs_cache_write(Some(cached), fresh),
            "a real config change must reach KV or every isolate serves stale config"
        );
    }

    #[test]
    fn unparseable_cache_entry_needs_a_write() {
        assert!(
            needs_cache_write(Some("{not json"), "[]"),
            "a corrupt entry must be overwritten, not preserved by a failed compare"
        );
    }

    /// `Record::data` is a `HashMap`, so two isolates serialising the same
    /// rows can emit the same object with different key order. A byte compare
    /// would call that a change and re-PUT on every rebuild — exactly the
    /// amplification this is meant to remove — so the compare is semantic.
    #[test]
    fn key_order_difference_alone_needs_no_write() {
        let cached = r#"[{"block":"WAFER_RUN__WEB","key":"PORT","value":"8080"}]"#;
        let fresh = r#"[{"value":"8080","key":"PORT","block":"WAFER_RUN__WEB"}]"#;
        assert!(
            !needs_cache_write(Some(cached), fresh),
            "map key order is not a config change"
        );
    }

    /// Row order, unlike key order, IS meaningful — the cached payload is a
    /// JSON array and callers hand it back as an ordered `Vec<Record>`.
    #[test]
    fn row_order_difference_needs_a_write() {
        let cached = r#"[{"key":"A"},{"key":"B"}]"#;
        let fresh = r#"[{"key":"B"},{"key":"A"}]"#;
        assert!(
            needs_cache_write(Some(cached), fresh),
            "array order is part of the value; reordered rows must be rewritten"
        );
    }
}
