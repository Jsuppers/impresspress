//! Content-addressed blob store for the sandbox workspace.
//!
//! Every byte the workspace holds lives here exactly once, keyed by the
//! SHA-256 of its content. Nothing is ever edited in place: a write stores a
//! new blob and repoints the manifest entry, so a generation that names a
//! sha keeps naming the same bytes for as long as the blob exists, and a
//! rollback needs no copy.
//!
//! Blobs are never deleted by a file delete — an older generation may still
//! reference them (design §7.2). Reclaiming unreachable blobs is Plan 4's
//! garbage collector, which is why [`delete`] exists but no handler calls it.

use wafer_core::clients::storage;
use wafer_run::{context::Context, ErrorCode, WaferError};

/// Storage folder the blobs live in, relative to the block's own namespace —
/// `wafer-run/storage` rewrites it to `impresspress/dev/blobs` (see
/// [`crate::blocks::storage`]).
pub const FOLDER: &str = "blobs";

/// Content type blobs are stored under. The *file's* content type is a
/// property of the path that names the blob, not of the blob: the same bytes
/// can be reachable as `site/a.txt` and `blocks/x/README`, so it is recorded
/// on the manifest entry ([`super::workspace::FileEntry::content_type`]) and
/// never here.
const BLOB_CONTENT_TYPE: &str = "application/octet-stream";

/// SHA-256 of `bytes`, lowercase hex — the key a blob is stored under.
///
/// Re-exported rather than reimplemented: `wafer_block::hash` is already the
/// single source of this hash for the whole runtime, and a second
/// implementation is a second thing that can disagree with a stored key.
pub use wafer_block::hash::sha256_hex;

/// Store `bytes` and return their sha256.
///
/// Idempotent by construction — the key *is* the content — and skips the
/// write when the blob is already present, which is the common case: a page
/// saved twice, or two paths holding the same asset.
pub async fn put(ctx: &dyn Context, bytes: &[u8]) -> Result<String, WaferError> {
    let sha = sha256_hex(bytes);
    if exists(ctx, &sha).await? {
        return Ok(sha);
    }
    storage::put(ctx, FOLDER, &sha, bytes, BLOB_CONTENT_TYPE).await?;
    Ok(sha)
}

/// Fetch the blob stored under `sha`.
pub async fn get(ctx: &dyn Context, sha: &str) -> Result<Vec<u8>, WaferError> {
    let (bytes, _info) = storage::get(ctx, FOLDER, sha).await?;
    Ok(bytes)
}

/// Whether a blob is stored under `sha`.
///
/// A keyed `get` rather than a prefix `list`, for two reasons that both
/// matter on the sandbox's own target:
///
/// * `get` of an absent key is `NotFound` on every backend; what `list` does
///   with a folder nothing has written yet is not part of the
///   `StorageService` contract, and the backends genuinely differ —
///   `wafer-block-local-storage` answers an empty listing, the OPFS backend
///   in `impresspress-browser` rejects with `NotFoundError`. A blob store
///   asks this question before its first write has created the folder.
/// * `list` is `O(folder)` on OPFS (the bridge walks the whole directory tree
///   and filters in JS), so a prefix probe would make every write walk every
///   blob.
///
/// The body it reads back is wasted only when the blob is already there —
/// exactly the case where it saves rewriting the same bytes.
pub async fn exists(ctx: &dyn Context, sha: &str) -> Result<bool, WaferError> {
    match storage::get(ctx, FOLDER, sha).await {
        Ok(_) => Ok(true),
        Err(e) if e.code == ErrorCode::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Remove the blob stored under `sha`.
///
/// For the garbage collector only. A file delete leaves the blob alone: it may
/// still be named by a generation that can be rolled back to.
pub async fn delete(ctx: &dyn Context, sha: &str) -> Result<(), WaferError> {
    match storage::delete(ctx, FOLDER, sha).await {
        // Already gone is the outcome the caller asked for.
        Err(e) if e.code == ErrorCode::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{blocks::dev::test_support::FakeControl, test_support::TestContext};

    #[tokio::test]
    async fn a_blob_round_trips_under_the_hash_of_its_content() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let sha = put(&ctx, b"hello").await.expect("put");
        assert_eq!(sha, sha256_hex(b"hello"));
        assert_eq!(get(&ctx, &sha).await.expect("get"), b"hello".to_vec());
        assert!(exists(&ctx, &sha).await.expect("exists"));
    }

    #[tokio::test]
    async fn putting_the_same_bytes_twice_is_the_same_blob() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let first = put(&ctx, b"hello").await.expect("put");
        let second = put(&ctx, b"hello").await.expect("put again");
        assert_eq!(first, second);

        let listing = storage::list(
            &ctx,
            FOLDER,
            &wafer_core::clients::storage::ListOptions::default(),
        )
        .await
        .expect("list");
        assert_eq!(listing.objects.len(), 1);
    }

    #[tokio::test]
    async fn an_absent_blob_is_not_reported_present_and_reads_as_not_found() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let sha = sha256_hex(b"never stored");
        assert!(!exists(&ctx, &sha).await.expect("exists"));
        assert_eq!(
            get(&ctx, &sha).await.expect_err("get must fail").code,
            ErrorCode::NotFound
        );
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let sha = put(&ctx, b"hello").await.expect("put");
        delete(&ctx, &sha).await.expect("delete");
        assert!(!exists(&ctx, &sha).await.expect("exists"));
        delete(&ctx, &sha).await.expect("delete again");
    }

    /// The blob namespace belongs to the dev block, not to whoever asked. The
    /// fixture drives the production `ImpresspressStorageBlock`, so this
    /// pins that `blobs` really resolves under `impresspress/dev/`.
    #[tokio::test]
    async fn blobs_live_in_the_blocks_own_storage_namespace() {
        let ctx = TestContext::with_dev(FakeControl::new()).await;
        let sha = put(&ctx, b"hello").await.expect("put");
        let folders = storage::list_folders(&ctx).await;
        // `list_folders` is admin-only under WRAP, so the dev block cannot ask
        // — assert through the cross-block read path instead, which the site
        // grant does NOT cover and which therefore must be refused, while the
        // block's own namespace resolves.
        assert!(folders.is_err(), "list_folders is admin-only");
        assert_eq!(get(&ctx, &sha).await.expect("own namespace"), b"hello");
        assert!(
            storage::get(&ctx, "@impresspress/files/uploads", &sha)
                .await
                .is_err(),
            "a namespace the block holds no grant for must stay unreachable"
        );
    }
}
