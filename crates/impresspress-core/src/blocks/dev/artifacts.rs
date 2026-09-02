//! Content-addressed store for compiled guest artifacts.
//!
//! The `blocks/` half of the workspace is source; this is what the compiler
//! produced from it — one `wasm32-wasip1` module per stored sha
//! (`impresspress/dev/artifacts/<sha256>.wasm`, design §11.2).
//!
//! Separate from [`super::blobs`] rather than folded into it because the two
//! stores answer different questions and are collected on different rules: a
//! blob is reachable from a workspace path, an artifact is reachable from a
//! generation's block manifest. Sharing a folder would mean the blob collector
//! had to know about block manifests to avoid deleting a live guest.

use wafer_block::hash::sha256_hex;
use wafer_core::clients::storage;
use wafer_run::{context::Context, ErrorCode, WaferError};

/// Storage folder the artifacts live in, relative to the block's own
/// namespace — `wafer-run/storage` rewrites it to
/// `impresspress/dev/artifacts`.
pub const FOLDER: &str = "artifacts";

/// Content type artifacts are stored under.
const ARTIFACT_CONTENT_TYPE: &str = "application/wasm";

/// The storage key an artifact with hash `sha` is filed under.
///
/// The `.wasm` suffix is part of the key design §11.2 specifies, so it is
/// derived here rather than spelled out at each call site — a key built two
/// ways is a key that can be built two *different* ways.
pub fn key_for(sha: &str) -> String {
    format!("{sha}.wasm")
}

/// Store `bytes` and return their sha256.
///
/// Idempotent by construction — the key *is* the content.
pub async fn put(ctx: &dyn Context, bytes: &[u8]) -> Result<String, WaferError> {
    let sha = sha256_hex(bytes);
    storage::put(ctx, FOLDER, &key_for(&sha), bytes, ARTIFACT_CONTENT_TYPE).await?;
    Ok(sha)
}

/// Fetch the artifact stored under `sha`.
pub async fn get(ctx: &dyn Context, sha: &str) -> Result<Vec<u8>, WaferError> {
    let (bytes, _info) = storage::get(ctx, FOLDER, &key_for(sha)).await?;
    Ok(bytes)
}

/// Whether an artifact is stored under `sha`.
///
/// A keyed `get` rather than a prefix `list`, for the reasons
/// [`super::blobs::exists`] documents: `list` of a folder nothing has written
/// yet is not portable across the storage backends this runs on, and it is
/// `O(folder)` on OPFS.
pub async fn exists(ctx: &dyn Context, sha: &str) -> Result<bool, WaferError> {
    match storage::get(ctx, FOLDER, &key_for(sha)).await {
        Ok(_) => Ok(true),
        Err(e) if e.code == ErrorCode::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{blocks::dev::test_support::FakeControl, test_support::TestContext};

    #[tokio::test]
    async fn an_artifact_round_trips_under_the_hash_of_its_content() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let wasm = b"\0asm\x01\0\0\0";
        let sha = put(&ctx, wasm).await.expect("put");
        assert_eq!(sha, sha256_hex(wasm));
        assert_eq!(get(&ctx, &sha).await.expect("get"), wasm.to_vec());
        assert!(exists(&ctx, &sha).await.expect("exists"));
        assert_eq!(key_for(&sha), format!("{sha}.wasm"));
    }

    #[tokio::test]
    async fn an_absent_artifact_is_not_reported_present() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        assert!(!exists(&ctx, &sha256_hex(b"never stored"))
            .await
            .expect("exists"));
    }
}
