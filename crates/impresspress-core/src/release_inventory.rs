//! Release-key inventory and folder-management utilities.
//!
//! Consumed by the Cloudflare runtime; the byte contract is produced by
//! the CLI's `ReleaseManifest::logical_keys_json()`.

use std::{collections::HashSet, sync::Arc};

use wafer_core::interfaces::storage::service::{StorageError, StorageService};

use crate::isolate_cell::IsolateCell;

/// Checks if a logical key is normalized (no empty components, no `.` or `..`, no leading/trailing slash, no backslash).
/// Shared by both CLI and runtime inventory validation.
pub fn is_normalized_logical_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('/')
        && !key.ends_with('/')
        && !key.contains('\\')
        && key
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

/// Parsed release-key inventory fetched from `{prefix}/keys.json`.
/// Digest-checked against the Worker-version-pinned sha before parsing so a
/// tampered or truncated object fails closed, never downgrading a release
/// read to a mutable object at the same key.
pub struct ReleaseInventory {
    logical_keys: HashSet<String>,
    managed_prefixes: HashSet<String>,
}

impl ReleaseInventory {
    pub fn from_json_bytes(bytes: &[u8], expected_sha256: &str) -> Result<Self, String> {
        let actual = format!("sha256:{}", crate::util::sha256_hex(bytes));
        if expected_sha256 != actual {
            return Err(format!(
                "release asset key inventory digest mismatch: expected {expected_sha256}, got {actual}"
            ));
        }
        let keys: Vec<String> = serde_json::from_slice(bytes)
            .map_err(|e| format!("asset key inventory is not a JSON string array: {e}"))?;
        if keys.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err("asset key inventory must be strictly sorted and unique".into());
        }
        let mut managed_prefixes = HashSet::new();
        for key in &keys {
            if !is_normalized_logical_key(key) {
                return Err(format!("asset key is not normalized: {key:?}"));
            }
            let mut end = 0;
            for component in key.split('/') {
                if end + component.len() == key.len() {
                    break; // final component is the object name, not a folder
                }
                end += component.len();
                managed_prefixes.insert(key[..end].to_string());
                end += 1; // the '/'
            }
        }
        Ok(Self {
            logical_keys: keys.into_iter().collect(),
            managed_prefixes,
        })
    }

    pub fn contains(&self, logical_key: &str) -> bool {
        self.logical_keys.contains(logical_key)
    }

    pub fn manages_folder(&self, folder: &str) -> bool {
        self.managed_prefixes.contains(folder)
    }

    pub fn logical_keys_sorted(&self) -> Vec<&str> {
        let mut keys: Vec<_> = self.logical_keys.iter().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }
}

thread_local! {
    /// Digest-keyed parsed inventory; survives across requests in a wasm
    /// isolate (thread-per-isolate) and per-thread in native tests. An
    /// `IsolateCell`, not a `RefCell`: a platform hard stop does not run
    /// destructors, and dropping the previous inventory inside a held borrow
    /// is exactly the wedge `isolate_cell` exists to prevent.
    static RELEASE_INVENTORY_CACHE: IsolateCell<(String, Arc<ReleaseInventory>)> =
        const { IsolateCell::new() };
}

/// Fetch the release key inventory object once per isolate and
/// digest-verify it. Any failure is a hard error — release reads must
/// never silently fall back to mutable objects.
pub async fn load_release_inventory(
    keys_folder: &str,
    keys_name: &str,
    expected_sha256: &str,
    storage: &dyn StorageService,
) -> Result<Arc<ReleaseInventory>, StorageError> {
    if let Some((key, inventory)) = RELEASE_INVENTORY_CACHE.with(IsolateCell::get) {
        if key == expected_sha256 {
            return Ok(inventory);
        }
    }
    let (bytes, _) = storage
        .get(keys_folder, keys_name)
        .await
        .map_err(|error| StorageError::Internal(format!("fetch release key inventory: {error}")))?;
    let inventory = Arc::new(
        ReleaseInventory::from_json_bytes(&bytes, expected_sha256)
            .map_err(StorageError::Internal)?,
    );
    RELEASE_INVENTORY_CACHE.with(|slot| {
        slot.set((expected_sha256.to_string(), inventory.clone()));
    });
    Ok(inventory)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory_json(keys: &[&str]) -> (Vec<u8>, String) {
        let bytes = serde_json::to_vec(keys).unwrap();
        let sha = format!("sha256:{}", crate::util::sha256_hex(&bytes));
        (bytes, sha)
    }

    #[test]
    fn inventory_parses_and_answers_membership_and_folders() {
        let (bytes, sha) = inventory_json(&["gdsf/site/media/hero.webp", "public/app.css"]);
        let inv = ReleaseInventory::from_json_bytes(&bytes, &sha).unwrap();
        assert!(inv.contains("gdsf/site/media/hero.webp"));
        assert!(!inv.contains("gdsf/site/media/upload.webp"));
        assert!(inv.manages_folder("gdsf/site"));
        assert!(inv.manages_folder("gdsf/site/media"));
        assert!(inv.manages_folder("public"));
        assert!(!inv.manages_folder("uploads"));
        assert!(!inv.manages_folder("gdsf/site/media/hero.webp")); // a key, not a folder
        assert_eq!(
            inv.logical_keys_sorted(),
            vec!["gdsf/site/media/hero.webp", "public/app.css"]
        );
    }

    #[test]
    fn inventory_rejects_digest_mismatch_unsorted_duplicates_and_bad_keys() {
        let (bytes, sha) = inventory_json(&["a/b.css"]);
        assert!(ReleaseInventory::from_json_bytes(&bytes, "sha256:0000").is_err());
        let mut tampered = bytes.clone();
        tampered.push(b' ');
        assert!(ReleaseInventory::from_json_bytes(&tampered, &sha).is_err());

        let (unsorted, unsorted_sha) = inventory_json(&["b.css", "a.css"]);
        assert!(ReleaseInventory::from_json_bytes(&unsorted, &unsorted_sha).is_err());
        let (dup, dup_sha) = inventory_json(&["a.css", "a.css"]);
        assert!(ReleaseInventory::from_json_bytes(&dup, &dup_sha).is_err());
        let (bad, bad_sha) = inventory_json(&["../escape.css"]);
        assert!(ReleaseInventory::from_json_bytes(&bad, &bad_sha).is_err());
        let (not_array, na_sha) = {
            let bytes = br#"{"keys":[]}"#.to_vec();
            let sha = format!("sha256:{}", crate::util::sha256_hex(&bytes));
            (bytes, sha)
        };
        assert!(ReleaseInventory::from_json_bytes(&not_array, &na_sha).is_err());
    }

    struct FakeKeysStorage {
        bytes: Vec<u8>,
        gets: std::sync::atomic::AtomicUsize,
    }

    #[wafer_block::wafer_async_trait]
    impl StorageService for FakeKeysStorage {
        async fn put(
            &self,
            _folder: &str,
            _key: &str,
            _data: &[u8],
            _content_type: &str,
        ) -> Result<(), StorageError> {
            Err(StorageError::Internal("unsupported in test".into()))
        }

        async fn put_streaming(
            &self,
            _folder: &str,
            _key: &str,
            _data: wafer_block::InputStream,
            _content_type: &str,
        ) -> Result<(), StorageError> {
            Err(StorageError::Internal("unsupported in test".into()))
        }

        async fn get(
            &self,
            _folder: &str,
            key: &str,
        ) -> Result<
            (
                Vec<u8>,
                wafer_core::interfaces::storage::service::ObjectInfo,
            ),
            StorageError,
        > {
            assert_eq!(key, "keys.json");
            self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok((
                self.bytes.clone(),
                wafer_core::interfaces::storage::service::ObjectInfo {
                    key: key.to_string(),
                    size: self.bytes.len() as i64,
                    content_type: "application/json".to_string(),
                    last_modified: chrono::Utc::now(),
                },
            ))
        }

        async fn get_streaming(
            &self,
            _folder: &str,
            _key: &str,
        ) -> Result<
            (
                wafer_block::OutputStream,
                wafer_core::interfaces::storage::service::ObjectInfo,
            ),
            StorageError,
        > {
            Err(StorageError::Internal("unsupported in test".into()))
        }

        async fn delete(&self, _folder: &str, _key: &str) -> Result<(), StorageError> {
            Err(StorageError::Internal("unsupported in test".into()))
        }

        async fn list(
            &self,
            _folder: &str,
            _opts: &wafer_core::interfaces::storage::service::ListOptions,
        ) -> Result<wafer_core::interfaces::storage::service::ObjectList, StorageError> {
            Err(StorageError::Internal("unsupported in test".into()))
        }

        async fn create_folder(&self, _name: &str, _public: bool) -> Result<(), StorageError> {
            Err(StorageError::Internal("unsupported in test".into()))
        }

        async fn delete_folder(&self, _name: &str) -> Result<(), StorageError> {
            Err(StorageError::Internal("unsupported in test".into()))
        }

        async fn list_folders(
            &self,
        ) -> Result<Vec<wafer_core::interfaces::storage::service::FolderInfo>, StorageError>
        {
            Err(StorageError::Internal("unsupported in test".into()))
        }
    }

    #[test]
    fn inventory_loads_once_per_digest_and_fails_closed_on_mismatch() {
        futures::executor::block_on(async {
            let (bytes, sha) = inventory_json(&["public/app.css"]);
            let storage = FakeKeysStorage {
                bytes,
                gets: std::sync::atomic::AtomicUsize::new(0),
            };

            let first = load_release_inventory("some/prefix", "keys.json", &sha, &storage)
                .await
                .unwrap();
            assert!(first.contains("public/app.css"));
            let _second = load_release_inventory("some/prefix", "keys.json", &sha, &storage)
                .await
                .unwrap();
            assert_eq!(
                storage.gets.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "second load must hit the isolate cache"
            );

            let tampered = FakeKeysStorage {
                bytes: b"[\"public/evil.css\"]".to_vec(),
                gets: std::sync::atomic::AtomicUsize::new(0),
            };
            // Distinct digest so this case can't reuse the cache slot the
            // first assertions populated (or a slot any other test in this
            // thread might have populated).
            let other_sha = format!("sha256:{}", "ab".repeat(32));
            assert!(
                load_release_inventory("some/prefix", "keys.json", &other_sha, &tampered)
                    .await
                    .is_err()
            );
        });
    }
}
