//! Storage HANDLERS for the `impresspress/files` block: the user-facing
//! `/b/storage/api/...` JSON API and the admin stats endpoint. (This is the
//! impresspress-core files storage-handlers module — NOT the Cloudflare R2
//! adapter.) Dispatch lives in the block's one route table
//! (`blocks/files/mod.rs`); this module owns only the handlers.
//!
//! Split by domain responsibility:
//! - [`params`] — the bound `{name}` / `{key...}` path variables.
//! - [`validation`] — bucket-name / storage-key validation rules, shared
//!   with the share-creation path (`cloud.rs`).
//! - [`access`] — bucket-ownership / access-control predicates.
//! - [`buckets`] — bucket lifecycle: list, create, delete.
//! - [`objects`] — object lifecycle: list, download (streamed), upload,
//!   delete.
//! - [`search`] — object search + recently-viewed listing.
//! - [`admin`] — the aggregate stats endpoint.
//!
//! Every handler is re-exported here at `pub(in crate::blocks::files)` so the
//! block's `handle` names them as `storage::handle_*`.

mod access;
mod admin;
mod buckets;
mod objects;
mod params;
mod search;
mod validation;

pub(in crate::blocks::files) use access::{bucket_owned_by, is_bucket_access_denied};
pub(in crate::blocks::files) use admin::handle_stats;
pub(in crate::blocks::files) use buckets::{
    handle_create_bucket, handle_delete_bucket, handle_list_buckets,
};
pub(in crate::blocks::files) use objects::{
    handle_delete_object, handle_get_object, handle_list_objects, handle_upload_object,
};
pub(in crate::blocks::files) use search::{handle_recent, handle_search};
pub(in crate::blocks::files) use validation::{
    is_valid_bucket_name, is_valid_storage_key, BUCKET_NAME_MAX_LEN, BUCKET_NAME_MIN_LEN,
    BUCKET_NAME_PATTERN,
};

/// Test-only fixture shared by more than one domain submodule's integration
/// tests (buckets, objects, and admin/stats all seed buckets the same way).
#[cfg(test)]
mod test_helpers {
    use std::{
        collections::{HashMap, HashSet},
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use serde_json::json;
    use wafer_core::{
        interfaces::storage::service::{
            FolderInfo, ListOptions as StoreListOptions, ObjectInfo, ObjectList, StorageError,
            StorageService,
        },
        service_blocks::storage::StorageBlock,
    };

    use crate::{blocks::files::repo, test_support::TestContext};

    pub(super) async fn seed_bucket(ctx: &TestContext, name: &str, owner: &str) {
        let data = crate::util::json_map(json!({
            "name": name,
            "public": false,
            "created_by": owner,
            "created_at": crate::util::now_rfc3339(),
        }));
        repo::buckets::seed(ctx, data).await.expect("seed bucket");
    }

    /// Seed a completed object-metadata row, as a finished upload leaves it.
    pub(super) async fn seed_object_row(
        ctx: &TestContext,
        bucket: &str,
        key: &str,
        owner: &str,
        size: i64,
    ) {
        let mut row: HashMap<String, serde_json::Value> = HashMap::new();
        row.insert("bucket".into(), json!(bucket));
        row.insert("key".into(), json!(key));
        row.insert("size".into(), json!(size));
        row.insert("uploaded_by".into(), json!(owner));
        row.insert("status".into(), json!("complete"));
        repo::objects::seed(ctx, row)
            .await
            .expect("seed object row");
    }

    /// `(folder, key)` → `(bytes, content_type)`.
    type MemObjects = HashMap<(String, String), (Vec<u8>, String)>;

    /// In-memory [`StorageService`] so handler tests exercise the production
    /// `wafer-run/storage` [`StorageBlock`] wire protocol end-to-end (the
    /// typed `store::*` clients round-trip through the real handler) without
    /// touching the filesystem.
    ///
    /// Folders are tracked, and `delete` / `delete_folder` answer `NotFound`
    /// for what was never stored, the way the real backends do — the delete
    /// handlers' retry behaviour depends on that distinction.
    #[derive(Default)]
    pub(super) struct MemStorage {
        objects: Mutex<MemObjects>,
        folders: Mutex<HashSet<String>>,
    }

    #[async_trait]
    impl StorageService for MemStorage {
        async fn put(
            &self,
            folder: &str,
            key: &str,
            data: &[u8],
            content_type: &str,
        ) -> Result<(), StorageError> {
            self.objects.lock().unwrap().insert(
                (folder.to_string(), key.to_string()),
                (data.to_vec(), content_type.to_string()),
            );
            Ok(())
        }

        async fn get(
            &self,
            folder: &str,
            key: &str,
        ) -> Result<(Vec<u8>, ObjectInfo), StorageError> {
            let guard = self.objects.lock().unwrap();
            let (data, content_type) = guard
                .get(&(folder.to_string(), key.to_string()))
                .ok_or(StorageError::NotFound)?;
            Ok((
                data.clone(),
                ObjectInfo {
                    key: key.to_string(),
                    size: data.len() as i64,
                    content_type: content_type.clone(),
                    last_modified: chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0)
                        .expect("epoch"),
                },
            ))
        }

        async fn delete(&self, folder: &str, key: &str) -> Result<(), StorageError> {
            self.objects
                .lock()
                .unwrap()
                .remove(&(folder.to_string(), key.to_string()))
                .map(|_| ())
                .ok_or(StorageError::NotFound)
        }

        async fn list(
            &self,
            _folder: &str,
            _opts: &StoreListOptions,
        ) -> Result<ObjectList, StorageError> {
            Ok(ObjectList {
                objects: vec![],
                total_count: 0,
                next_cursor: None,
            })
        }

        async fn create_folder(&self, name: &str, _public: bool) -> Result<(), StorageError> {
            self.folders.lock().unwrap().insert(name.to_string());
            Ok(())
        }

        async fn delete_folder(&self, name: &str) -> Result<(), StorageError> {
            if !self.folders.lock().unwrap().remove(name) {
                return Err(StorageError::NotFound);
            }
            self.objects
                .lock()
                .unwrap()
                .retain(|(folder, _), _| folder != name);
            Ok(())
        }

        async fn list_folders(&self) -> Result<Vec<FolderInfo>, StorageError> {
            Ok(vec![])
        }
    }

    /// [`TestContext::with_files`] plus a real `wafer-run/storage` block over
    /// [`MemStorage`], so handlers can complete their `store::*` calls.
    pub(super) async fn ctx_with_storage() -> TestContext {
        let mut ctx = TestContext::with_files().await;
        ctx.register_block(
            "wafer-run/storage",
            Arc::new(StorageBlock::new(Arc::new(MemStorage::default()))),
        );
        ctx
    }
}
