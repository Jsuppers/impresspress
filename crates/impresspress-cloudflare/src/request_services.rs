//! Request-current Cloudflare service injection.
//!
//! A cached [`wafer_run::Wafer`] must not retain D1, KV, R2, or other
//! request-derived Workers handles. The service blocks inside that Wafer
//! therefore hold the stateless forwarding proxies in this module. Every poll
//! of the top-level dispatch future installs the current request's concrete
//! [`RequestServices`] bundle, and restores the previous bundle before yielding
//! back to the Workers executor.
//!
//! The poll boundary is the important part. Workers may interleave fetch
//! events whenever one request returns `Poll::Pending`; a plain
//! thread-local "current request" set once around an `.await` would therefore
//! be incorrect. Re-entering the scope on every poll gives nested awaited
//! service calls and lazy block Init the right bundle while allowing another
//! request to use the isolate between polls.
//!
//! This relies on Cloudflare's single-threaded wasm32 isolate execution model.
//! It is adapter-local: native and VM runtimes continue to inject their normal
//! long-lived services directly.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context as TaskContext, Poll},
    time::Duration,
};

use wafer_block::{
    db::{Filter, ListOptions as DbListOptions},
    ConfigVar, InputStream, OutputStream,
};
use wafer_core::interfaces::{
    config::service::ConfigService,
    crypto::service::{CryptoError, CryptoService},
    database::service::{
        AggregateSpec, Column, DatabaseError, DatabaseService, Record, RecordList, Table,
        UpsertSpec,
    },
    logger::service::{Field, LoggerService},
    network::service::{
        NetworkError, NetworkService, Request as NetworkRequest, Response as NetworkResponse,
        ResponseHead,
    },
    storage::service::{
        FolderInfo, ListOptions as StorageListOptions, ObjectInfo, ObjectList, StorageError,
        StorageService,
    },
};
use wafer_run::{ConfigError, ConfigSource, EnvBlockConfig};

pub(crate) const RELEASE_ASSET_ID_VAR: &str = "IMPRESSPRESS_RELEASE_ASSET_ID";
pub(crate) const RELEASE_ASSET_PREFIX_VAR: &str = "IMPRESSPRESS_RELEASE_ASSET_PREFIX";
pub(crate) const RELEASE_ASSET_MANIFEST_VAR: &str = "IMPRESSPRESS_RELEASE_ASSET_MANIFEST";
pub(crate) const RELEASE_ASSET_KEYS_JSON_VAR: &str = "IMPRESSPRESS_RELEASE_ASSET_KEYS_JSON";

const RELEASES_ROOT: &str = ".impresspress/releases/v1";
const RELEASE_KEYS_JSON_MAX_BYTES: usize = 4096;

/// Pure, Worker-version-bound release identity. This contains no R2 handle.
#[derive(Debug)]
#[allow(dead_code)] // Full identity is exposed for control-plane integrations.
pub(crate) struct ReleaseAssetIdentity {
    id: String,
    prefix: String,
    manifest_key: String,
    logical_keys: HashSet<String>,
}

thread_local! {
    /// Parsed pure Worker-version data only; no Env, binding, or I/O object is
    /// retained across requests.
    static RELEASE_IDENTITY_CACHE: RefCell<Option<(String, Arc<ReleaseAssetIdentity>)>> = const { RefCell::new(None) };
}

#[allow(dead_code)]
impl ReleaseAssetIdentity {
    pub(crate) fn from_env(env: &worker::Env) -> Result<Option<Arc<Self>>, String> {
        let id = env.var(RELEASE_ASSET_ID_VAR).ok().map(|v| v.to_string());
        let prefix = env
            .var(RELEASE_ASSET_PREFIX_VAR)
            .ok()
            .map(|v| v.to_string());
        let manifest_key = env
            .var(RELEASE_ASSET_MANIFEST_VAR)
            .ok()
            .map(|v| v.to_string());
        let keys_json = env
            .var(RELEASE_ASSET_KEYS_JSON_VAR)
            .ok()
            .map(|v| v.to_string());
        let keys_sha256 = env
            .var(impresspress_core::RELEASE_ASSET_KEYS_SHA256_VAR)
            .ok()
            .map(|v| v.to_string());

        if id.is_none()
            && prefix.is_none()
            && manifest_key.is_none()
            && keys_json.is_none()
            && keys_sha256.is_none()
        {
            return Ok(None);
        }
        let required = |name: &str, value: Option<String>| {
            value.ok_or_else(|| format!("release asset identity is incomplete: missing {name}"))
        };
        let id = required(RELEASE_ASSET_ID_VAR, id)?;
        let prefix = required(RELEASE_ASSET_PREFIX_VAR, prefix)?;
        let manifest_key = required(RELEASE_ASSET_MANIFEST_VAR, manifest_key)?;
        let keys_json = required(RELEASE_ASSET_KEYS_JSON_VAR, keys_json)?;
        let keys_sha256 = required(
            impresspress_core::RELEASE_ASSET_KEYS_SHA256_VAR,
            keys_sha256,
        )?;

        // Validate exact bytes before consulting the cache. Tampered JSON with
        // unchanged metadata must fail closed, not reuse an older inventory.
        let actual_keys_sha256 = format!(
            "sha256:{}",
            impresspress_core::util::sha256_hex(keys_json.as_bytes())
        );
        if keys_sha256 != actual_keys_sha256 {
            return Err(format!(
                "release asset key inventory digest mismatch: expected {keys_sha256}, got {actual_keys_sha256}"
            ));
        }
        let cache_key = format!("{id}\n{prefix}\n{manifest_key}\n{keys_sha256}");
        if let Some(identity) = RELEASE_IDENTITY_CACHE.with(|slot| {
            slot.borrow()
                .as_ref()
                .filter(|(key, _)| key == &cache_key)
                .map(|(_, identity)| identity.clone())
        }) {
            return Ok(Some(identity));
        }

        let identity = Arc::new(Self::parse(
            id,
            prefix,
            manifest_key,
            keys_json,
            &keys_sha256,
        )?);
        RELEASE_IDENTITY_CACHE.with(|slot| {
            *slot.borrow_mut() = Some((cache_key, identity.clone()));
        });
        Ok(Some(identity))
    }

    fn parse(
        id: String,
        prefix: String,
        manifest_key: String,
        keys_json: String,
        expected_keys_sha256: &str,
    ) -> Result<Self, String> {
        if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("asset id must be a 64-character hexadecimal SHA-256".into());
        }
        let expected_prefix = format!("{RELEASES_ROOT}/{}", id.to_ascii_lowercase());
        if prefix != expected_prefix {
            return Err(format!(
                "asset prefix must be exactly {expected_prefix:?}, got {prefix:?}"
            ));
        }
        let expected_manifest = format!("{prefix}/manifest.json");
        if manifest_key != expected_manifest {
            return Err(format!(
                "asset manifest must be exactly {expected_manifest:?}, got {manifest_key:?}"
            ));
        }
        if keys_json.len() > RELEASE_KEYS_JSON_MAX_BYTES {
            return Err(format!(
                "asset key inventory exceeds {RELEASE_KEYS_JSON_MAX_BYTES} bytes"
            ));
        }
        let actual_keys_sha256 = format!(
            "sha256:{}",
            impresspress_core::util::sha256_hex(keys_json.as_bytes())
        );
        if expected_keys_sha256 != actual_keys_sha256 {
            return Err(format!(
                "asset key inventory digest mismatch: expected {expected_keys_sha256}, got {actual_keys_sha256}"
            ));
        }
        let keys: Vec<String> = serde_json::from_str(&keys_json)
            .map_err(|e| format!("asset key inventory is not a JSON string array: {e}"))?;
        if keys.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err("asset key inventory must be strictly sorted and unique".into());
        }
        for key in &keys {
            if !is_normalized_logical_key(key) {
                return Err(format!("asset key is not normalized: {key:?}"));
            }
        }
        Ok(Self {
            id,
            prefix,
            manifest_key,
            logical_keys: keys.into_iter().collect(),
        })
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn manifest_key(&self) -> &str {
        &self.manifest_key
    }

    pub(crate) fn first_logical_key(&self) -> Option<&str> {
        self.logical_keys.iter().map(String::as_str).min()
    }

    pub(crate) fn logical_keys_sorted(&self) -> Vec<&str> {
        let mut keys: Vec<_> = self.logical_keys.iter().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }

    pub(crate) fn physical_read_location(
        &self,
        folder: &str,
        key: &str,
    ) -> Option<(String, String)> {
        let logical = joined_logical_key(folder, key)?;
        if !self.logical_keys.contains(&logical) {
            return None;
        }
        let physical = format!("{}/{logical}", self.prefix);
        let (physical_folder, physical_key) = physical.rsplit_once('/')?;
        Some((physical_folder.to_string(), physical_key.to_string()))
    }

    pub(crate) fn physical_object_key(&self, logical_key: &str) -> Option<String> {
        (is_normalized_logical_key(logical_key) && self.logical_keys.contains(logical_key))
            .then(|| format!("{}/{logical_key}", self.prefix))
    }

    fn manages_folder(&self, folder: &str) -> bool {
        let prefix = format!("{folder}/");
        self.logical_keys.iter().any(|key| key.starts_with(&prefix))
    }
}

fn is_normalized_logical_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('/')
        && !key.ends_with('/')
        && !key.contains('\\')
        && key
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn joined_logical_key(folder: &str, key: &str) -> Option<String> {
    if folder.is_empty() || key.is_empty() {
        return None;
    }
    let logical = format!("{folder}/{key}");
    is_normalized_logical_key(&logical).then_some(logical)
}

/// Concrete services derived from exactly one Workers request `Env`.
///
/// Every field is optional only so the forwarding proxies can fail closed and
/// unit tests can construct marker-only bundles. Production construction
/// always fills every service.
pub(crate) struct RequestServices {
    database: Option<Arc<dyn DatabaseService>>,
    storage: Option<Arc<dyn StorageService>>,
    config: Option<Arc<dyn ConfigService>>,
    crypto: Option<Arc<dyn CryptoService>>,
    network: Option<Arc<dyn NetworkService>>,
    logger: Option<Arc<dyn LoggerService>>,
    config_source: Option<Arc<dyn ConfigSource>>,
    release_assets: Result<Option<Arc<ReleaseAssetIdentity>>, String>,
    #[cfg(test)]
    marker: usize,
}

impl RequestServices {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        env: &worker::Env,
        database: Arc<dyn DatabaseService>,
        storage: Arc<dyn StorageService>,
        config: Arc<dyn ConfigService>,
        crypto: Arc<dyn CryptoService>,
        network: Arc<dyn NetworkService>,
        logger: Arc<dyn LoggerService>,
        config_source: Arc<dyn ConfigSource>,
    ) -> Rc<Self> {
        Rc::new(Self {
            database: Some(database),
            storage: Some(storage),
            config: Some(config),
            crypto: Some(crypto),
            network: Some(network),
            logger: Some(logger),
            config_source: Some(config_source),
            release_assets: ReleaseAssetIdentity::from_env(env),
            #[cfg(test)]
            marker: 0,
        })
    }

    #[cfg(test)]
    fn marker(marker: usize) -> Rc<Self> {
        Rc::new(Self {
            database: None,
            storage: None,
            config: None,
            crypto: None,
            network: None,
            logger: None,
            config_source: None,
            release_assets: Ok(None),
            marker,
        })
    }

    #[cfg(test)]
    fn marker_with_release(marker: usize, release_assets: ReleaseAssetIdentity) -> Rc<Self> {
        Rc::new(Self {
            database: None,
            storage: None,
            config: None,
            crypto: None,
            network: None,
            logger: None,
            config_source: None,
            release_assets: Ok(Some(Arc::new(release_assets))),
            marker,
        })
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<RequestServices>>> = const { RefCell::new(None) };
}

fn current() -> Option<Rc<RequestServices>> {
    CURRENT.with(|slot| slot.borrow().clone())
}

/// Request-current immutable release identity, available only while the
/// request's dispatch future is being polled.
pub(crate) fn current_release_asset_identity(
) -> Result<Option<Arc<ReleaseAssetIdentity>>, StorageError> {
    match current().map(|services| services.release_assets.clone()) {
        Some(Ok(identity)) => Ok(identity),
        Some(Err(error)) => Err(StorageError::Internal(format!(
            "invalid release asset identity: {error}"
        ))),
        None => Ok(None),
    }
}

struct ScopeGuard {
    previous: Option<Rc<RequestServices>>,
}

impl ScopeGuard {
    fn enter(services: Rc<RequestServices>) -> Self {
        let previous = CURRENT.with(|slot| slot.replace(Some(services)));
        Self { previous }
    }
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        CURRENT.with(|slot| {
            *slot.borrow_mut() = self.previous.take();
        });
    }
}

/// Run synchronous builder/start work with the same request-current service
/// selection used by async dispatch.
pub(crate) fn scope_sync<T>(services: Rc<RequestServices>, f: impl FnOnce() -> T) -> T {
    let _guard = ScopeGuard::enter(services);
    f()
}

/// A future that re-enters its request service scope on every poll.
pub(crate) struct ScopedFuture<F> {
    services: Rc<RequestServices>,
    inner: Pin<Box<F>>,
}

impl<F: Future> Future for ScopedFuture<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let _guard = ScopeGuard::enter(self.services.clone());
        self.inner.as_mut().poll(cx)
    }
}

pub(crate) fn scope<F: Future>(services: Rc<RequestServices>, future: F) -> ScopedFuture<F> {
    ScopedFuture {
        services,
        inner: Box::pin(future),
    }
}

fn database() -> Result<Arc<dyn DatabaseService>, DatabaseError> {
    current()
        .and_then(|services| services.database.clone())
        .ok_or_else(|| {
            DatabaseError::Internal(
                "Cloudflare database service used outside request poll scope".into(),
            )
        })
}

fn storage() -> Result<Arc<dyn StorageService>, StorageError> {
    current()
        .and_then(|services| services.storage.clone())
        .ok_or_else(|| {
            StorageError::Internal(
                "Cloudflare storage service used outside request poll scope".into(),
            )
        })
}

fn crypto() -> Result<Arc<dyn CryptoService>, CryptoError> {
    current()
        .and_then(|services| services.crypto.clone())
        .ok_or_else(|| {
            CryptoError::Other("Cloudflare crypto service used outside request poll scope".into())
        })
}

fn network() -> Result<Arc<dyn NetworkService>, NetworkError> {
    current()
        .and_then(|services| services.network.clone())
        .ok_or_else(|| {
            NetworkError::Other("Cloudflare network service used outside request poll scope".into())
        })
}

#[derive(Default)]
pub(crate) struct ScopedDatabaseService {
    strict_schema: AtomicBool,
}

impl ScopedDatabaseService {
    fn current(&self) -> Result<Arc<dyn DatabaseService>, DatabaseError> {
        let service = database()?;
        service.set_strict_schema(self.strict_schema.load(Ordering::Relaxed));
        Ok(service)
    }
}

#[wafer_block::wafer_async_trait]
impl DatabaseService for ScopedDatabaseService {
    async fn get(&self, collection: &str, id: &str) -> Result<Record, DatabaseError> {
        self.current()?.get(collection, id).await
    }

    async fn list(
        &self,
        collection: &str,
        opts: &DbListOptions,
    ) -> Result<RecordList, DatabaseError> {
        self.current()?.list(collection, opts).await
    }

    async fn create(
        &self,
        collection: &str,
        data: HashMap<String, serde_json::Value>,
    ) -> Result<Record, DatabaseError> {
        self.current()?.create(collection, data).await
    }

    async fn update(
        &self,
        collection: &str,
        id: &str,
        data: HashMap<String, serde_json::Value>,
    ) -> Result<Record, DatabaseError> {
        self.current()?.update(collection, id, data).await
    }

    async fn delete(&self, collection: &str, id: &str) -> Result<(), DatabaseError> {
        self.current()?.delete(collection, id).await
    }

    async fn count(&self, collection: &str, filters: &[Filter]) -> Result<i64, DatabaseError> {
        self.current()?.count(collection, filters).await
    }

    async fn sum(
        &self,
        collection: &str,
        field: &str,
        filters: &[Filter],
    ) -> Result<f64, DatabaseError> {
        self.current()?.sum(collection, field, filters).await
    }

    async fn query_raw(
        &self,
        query: &str,
        args: &[serde_json::Value],
    ) -> Result<Vec<Record>, DatabaseError> {
        self.current()?.query_raw(query, args).await
    }

    async fn exec_raw(
        &self,
        query: &str,
        args: &[serde_json::Value],
    ) -> Result<i64, DatabaseError> {
        self.current()?.exec_raw(query, args).await
    }

    async fn delete_where(
        &self,
        collection: &str,
        filters: &[Filter],
    ) -> Result<(), DatabaseError> {
        self.current()?.delete_where(collection, filters).await
    }

    async fn delete_where_count(
        &self,
        collection: &str,
        filters: &[Filter],
    ) -> Result<i64, DatabaseError> {
        self.current()?
            .delete_where_count(collection, filters)
            .await
    }

    async fn take_where(
        &self,
        collection: &str,
        filters: &[Filter],
    ) -> Result<Vec<Record>, DatabaseError> {
        self.current()?.take_where(collection, filters).await
    }

    async fn update_where(
        &self,
        collection: &str,
        filters: &[Filter],
        data: HashMap<String, serde_json::Value>,
    ) -> Result<(), DatabaseError> {
        self.current()?
            .update_where(collection, filters, data)
            .await
    }

    async fn update_where_count(
        &self,
        collection: &str,
        filters: &[Filter],
        data: HashMap<String, serde_json::Value>,
    ) -> Result<i64, DatabaseError> {
        self.current()?
            .update_where_count(collection, filters, data)
            .await
    }

    async fn increment_field_where(
        &self,
        collection: &str,
        col: &str,
        delta: i64,
        filters: &[Filter],
    ) -> Result<i64, DatabaseError> {
        self.current()?
            .increment_field_where(collection, col, delta, filters)
            .await
    }

    async fn upsert(&self, collection: &str, spec: UpsertSpec) -> Result<i64, DatabaseError> {
        self.current()?.upsert(collection, spec).await
    }

    async fn aggregate(
        &self,
        collection: &str,
        spec: AggregateSpec,
    ) -> Result<Vec<Record>, DatabaseError> {
        self.current()?.aggregate(collection, spec).await
    }

    async fn ensure_schema_table(&self, table: &Table) -> Result<(), DatabaseError> {
        self.current()?.ensure_schema_table(table).await
    }

    async fn ensure_schema_tables(&self, tables: &[Table]) -> Result<(), DatabaseError> {
        self.current()?.ensure_schema_tables(tables).await
    }

    async fn schema_table_exists(&self, name: &str) -> Result<bool, DatabaseError> {
        self.current()?.schema_table_exists(name).await
    }

    async fn schema_drop_table(&self, name: &str) -> Result<(), DatabaseError> {
        self.current()?.schema_drop_table(name).await
    }

    async fn schema_add_column(&self, table: &str, column: &Column) -> Result<(), DatabaseError> {
        self.current()?.schema_add_column(table, column).await
    }

    fn set_strict_schema(&self, enabled: bool) {
        self.strict_schema.store(enabled, Ordering::Relaxed);
        if let Ok(service) = database() {
            service.set_strict_schema(enabled);
        }
    }
}

#[derive(Default)]
pub(crate) struct ScopedStorageService;

impl ScopedStorageService {
    fn read_location(folder: &str, key: &str) -> Result<(String, String), StorageError> {
        Ok(current_release_asset_identity()?
            .and_then(|identity| identity.physical_read_location(folder, key))
            .unwrap_or_else(|| (folder.to_string(), key.to_string())))
    }

    fn reject_managed_object_mutation(folder: &str, key: &str) -> Result<(), StorageError> {
        if current_release_asset_identity()?
            .and_then(|identity| identity.physical_read_location(folder, key))
            .is_some()
        {
            return Err(StorageError::Internal(format!(
                "release-managed object {folder}/{key} is immutable"
            )));
        }
        Ok(())
    }

    fn reject_managed_folder_listing(folder: &str) -> Result<(), StorageError> {
        if current_release_asset_identity()?.is_some_and(|identity| identity.manages_folder(folder))
        {
            return Err(StorageError::Internal(format!(
                "listing release-managed folder {folder:?} is unsupported; use the release manifest"
            )));
        }
        Ok(())
    }
}

#[wafer_block::wafer_async_trait]
impl StorageService for ScopedStorageService {
    async fn put(
        &self,
        folder: &str,
        key: &str,
        data: &[u8],
        content_type: &str,
    ) -> Result<(), StorageError> {
        Self::reject_managed_object_mutation(folder, key)?;
        storage()?.put(folder, key, data, content_type).await
    }

    async fn put_streaming(
        &self,
        folder: &str,
        key: &str,
        data: InputStream,
        content_type: &str,
    ) -> Result<(), StorageError> {
        Self::reject_managed_object_mutation(folder, key)?;
        storage()?
            .put_streaming(folder, key, data, content_type)
            .await
    }

    async fn get(&self, folder: &str, key: &str) -> Result<(Vec<u8>, ObjectInfo), StorageError> {
        let (read_folder, read_key) = Self::read_location(folder, key)?;
        let (bytes, mut info) = storage()?.get(&read_folder, &read_key).await?;
        info.key = key.to_string();
        Ok((bytes, info))
    }

    async fn get_streaming(
        &self,
        folder: &str,
        key: &str,
    ) -> Result<(OutputStream, ObjectInfo), StorageError> {
        let (read_folder, read_key) = Self::read_location(folder, key)?;
        let (stream, mut info) = storage()?.get_streaming(&read_folder, &read_key).await?;
        info.key = key.to_string();
        Ok((stream, info))
    }

    async fn delete(&self, folder: &str, key: &str) -> Result<(), StorageError> {
        Self::reject_managed_object_mutation(folder, key)?;
        storage()?.delete(folder, key).await
    }

    async fn list(
        &self,
        folder: &str,
        opts: &StorageListOptions,
    ) -> Result<ObjectList, StorageError> {
        Self::reject_managed_folder_listing(folder)?;
        storage()?.list(folder, opts).await
    }

    async fn create_folder(&self, name: &str, public: bool) -> Result<(), StorageError> {
        storage()?.create_folder(name, public).await
    }

    async fn delete_folder(&self, name: &str) -> Result<(), StorageError> {
        storage()?.delete_folder(name).await
    }

    async fn list_folders(&self) -> Result<Vec<FolderInfo>, StorageError> {
        storage()?.list_folders().await
    }
}

#[derive(Default)]
pub(crate) struct ScopedConfigService;

impl ConfigService for ScopedConfigService {
    fn get(&self, key: &str) -> Option<String> {
        current()
            .and_then(|services| services.config.clone())
            .and_then(|service| service.get(key))
    }

    fn set(&self, key: &str, value: &str) {
        if let Some(service) = current().and_then(|services| services.config.clone()) {
            service.set(key, value);
        }
    }
}

#[derive(Default)]
pub(crate) struct ScopedCryptoService;

impl CryptoService for ScopedCryptoService {
    fn hash(&self, password: &str) -> Result<String, CryptoError> {
        crypto()?.hash(password)
    }

    fn compare_hash(&self, password: &str, hash: &str) -> Result<(), CryptoError> {
        crypto()?.compare_hash(password, hash)
    }

    fn sign(
        &self,
        claims: HashMap<String, serde_json::Value>,
        expiry: Duration,
    ) -> Result<String, CryptoError> {
        crypto()?.sign(claims, expiry)
    }

    fn verify(&self, token: &str) -> Result<HashMap<String, serde_json::Value>, CryptoError> {
        crypto()?.verify(token)
    }

    fn sign_for(
        &self,
        block_id: &str,
        claims: HashMap<String, serde_json::Value>,
        expiry: Duration,
    ) -> Result<String, CryptoError> {
        crypto()?.sign_for(block_id, claims, expiry)
    }

    fn verify_for(
        &self,
        block_id: &str,
        token: &str,
    ) -> Result<HashMap<String, serde_json::Value>, CryptoError> {
        crypto()?.verify_for(block_id, token)
    }

    fn random_bytes(&self, n: usize) -> Result<Vec<u8>, CryptoError> {
        crypto()?.random_bytes(n)
    }
}

#[derive(Default)]
pub(crate) struct ScopedNetworkService;

#[wafer_block::wafer_async_trait]
impl NetworkService for ScopedNetworkService {
    async fn do_request(&self, req: &NetworkRequest) -> Result<NetworkResponse, NetworkError> {
        network()?.do_request(req).await
    }

    async fn do_request_streaming(
        &self,
        req: &NetworkRequest,
    ) -> Result<(ResponseHead, OutputStream), NetworkError> {
        network()?.do_request_streaming(req).await
    }
}

#[derive(Default)]
pub(crate) struct ScopedLoggerService;

impl ScopedLoggerService {
    fn with_logger(&self, f: impl FnOnce(&dyn LoggerService)) {
        if let Some(logger) = current().and_then(|services| services.logger.clone()) {
            f(logger.as_ref());
        }
    }
}

impl LoggerService for ScopedLoggerService {
    fn debug(&self, msg: &str, fields: &[Field]) {
        self.with_logger(|logger| logger.debug(msg, fields));
    }

    fn info(&self, msg: &str, fields: &[Field]) {
        self.with_logger(|logger| logger.info(msg, fields));
    }

    fn warn(&self, msg: &str, fields: &[Field]) {
        self.with_logger(|logger| logger.warn(msg, fields));
    }

    fn error(&self, msg: &str, fields: &[Field]) {
        self.with_logger(|logger| logger.error(msg, fields));
    }
}

#[derive(Default)]
pub(crate) struct ScopedConfigSource;

#[wafer_block::wafer_async_trait]
impl ConfigSource for ScopedConfigSource {
    async fn load_for_block(
        &self,
        block: &str,
        declared_keys: &[ConfigVar],
    ) -> Result<EnvBlockConfig, ConfigError> {
        let source = current()
            .and_then(|services| services.config_source.clone())
            .ok_or_else(|| ConfigError::Transient {
                block: block.to_string(),
                source: Box::new(std::io::Error::other(
                    "Cloudflare config source used outside request poll scope",
                )),
            })?;
        source.load_for_block(block, declared_keys).await
    }
}

pub(crate) fn database_proxy() -> Arc<dyn DatabaseService> {
    Arc::new(ScopedDatabaseService::default())
}

pub(crate) fn storage_proxy() -> Arc<dyn StorageService> {
    Arc::new(ScopedStorageService)
}

pub(crate) fn config_proxy() -> Arc<dyn ConfigService> {
    Arc::new(ScopedConfigService)
}

pub(crate) fn crypto_proxy() -> Arc<dyn CryptoService> {
    Arc::new(ScopedCryptoService)
}

pub(crate) fn network_proxy() -> Arc<dyn NetworkService> {
    Arc::new(ScopedNetworkService)
}

pub(crate) fn logger_proxy() -> Arc<dyn LoggerService> {
    Arc::new(ScopedLoggerService)
}

pub(crate) fn config_source_proxy() -> Arc<dyn ConfigSource> {
    Arc::new(ScopedConfigSource)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{executor::LocalPool, task::LocalSpawnExt};
    use std::cell::RefCell;

    fn marker() -> Option<usize> {
        current().map(|services| services.marker)
    }

    fn keys_hash(json: &str) -> String {
        format!(
            "sha256:{}",
            impresspress_core::util::sha256_hex(json.as_bytes())
        )
    }

    struct ObserveAcrossPolls {
        expected: usize,
        observations: Rc<RefCell<Vec<Option<usize>>>>,
        poll: usize,
    }

    impl Future for ObserveAcrossPolls {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
            self.observations.borrow_mut().push(marker());
            assert_eq!(marker(), Some(self.expected));
            self.poll += 1;
            if self.poll < 3 {
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }
    }

    #[test]
    fn interleaved_futures_reenter_their_own_request_scope_on_every_poll() {
        let first_seen = Rc::new(RefCell::new(Vec::new()));
        let second_seen = Rc::new(RefCell::new(Vec::new()));
        let mut pool = LocalPool::new();
        pool.spawner()
            .spawn_local(scope(
                RequestServices::marker(11),
                ObserveAcrossPolls {
                    expected: 11,
                    observations: first_seen.clone(),
                    poll: 0,
                },
            ))
            .unwrap();
        pool.spawner()
            .spawn_local(scope(
                RequestServices::marker(22),
                ObserveAcrossPolls {
                    expected: 22,
                    observations: second_seen.clone(),
                    poll: 0,
                },
            ))
            .unwrap();
        pool.run();

        assert_eq!(&*first_seen.borrow(), &[Some(11), Some(11), Some(11)]);
        assert_eq!(&*second_seen.borrow(), &[Some(22), Some(22), Some(22)]);
        assert_eq!(marker(), None);
    }

    #[test]
    fn nested_scope_restores_outer_request_and_then_fails_closed() {
        let outer = RequestServices::marker(1);
        let inner = RequestServices::marker(2);
        scope_sync(outer, || {
            assert_eq!(marker(), Some(1));
            scope_sync(inner, || assert_eq!(marker(), Some(2)));
            assert_eq!(marker(), Some(1));
        });
        assert_eq!(marker(), None);
        assert!(ScopedConfigService.get("anything").is_none());
        assert!(ScopedCryptoService.random_bytes(1).is_err());
    }

    #[test]
    fn release_identity_redirects_only_exact_normalized_members() {
        let id = "ab".repeat(32);
        let prefix = format!("{RELEASES_ROOT}/{id}");
        let keys_json = r#"["gdsf/site/media/hero.webp","public/app.css"]"#;
        let identity = ReleaseAssetIdentity::parse(
            id.clone(),
            prefix.clone(),
            format!("{prefix}/manifest.json"),
            keys_json.into(),
            &keys_hash(keys_json),
        )
        .unwrap();

        assert_eq!(identity.id(), id);
        assert_eq!(identity.manifest_key(), format!("{prefix}/manifest.json"));
        assert_eq!(
            identity.physical_read_location("gdsf/site", "media/hero.webp"),
            Some((format!("{prefix}/gdsf/site/media"), "hero.webp".to_string()))
        );
        assert_eq!(
            identity.physical_read_location("gdsf/site", "media/user-upload.webp"),
            None
        );
        assert_eq!(
            identity.physical_read_location("gdsf/site", "../public/app.css"),
            None
        );
        assert_eq!(
            identity.physical_object_key("gdsf/site/media/hero.webp"),
            Some(format!("{prefix}/gdsf/site/media/hero.webp"))
        );
        assert!(identity.manages_folder("gdsf/site"));
        assert!(!identity.manages_folder("uploads"));
    }

    #[test]
    fn release_identity_rejects_inconsistent_or_ambiguous_contracts() {
        let id = "cd".repeat(32);
        let prefix = format!("{RELEASES_ROOT}/{id}");
        assert!(ReleaseAssetIdentity::parse(
            id.clone(),
            ".impresspress/releases/v1/wrong".into(),
            format!("{prefix}/manifest.json"),
            "[]".into(),
            &keys_hash("[]"),
        )
        .is_err());
        assert!(ReleaseAssetIdentity::parse(
            id.clone(),
            prefix.clone(),
            format!("{prefix}/manifest.json"),
            r#"["b","a"]"#.into(),
            &keys_hash(r#"["b","a"]"#),
        )
        .is_err());
        assert!(ReleaseAssetIdentity::parse(
            id,
            prefix.clone(),
            format!("{prefix}/manifest.json"),
            r#"["a","a"]"#.into(),
            &keys_hash(r#"["a","a"]"#),
        )
        .is_err());
        let other_id = "ef".repeat(32);
        let other_prefix = format!("{RELEASES_ROOT}/{other_id}");
        assert!(ReleaseAssetIdentity::parse(
            other_id,
            other_prefix.clone(),
            format!("{other_prefix}/manifest.json"),
            "[]".into(),
            "sha256:tampered",
        )
        .is_err());
    }

    #[test]
    fn release_inventory_comparison_fails_closed_on_routing_tamper() {
        let id = "12".repeat(32);
        let prefix = format!("{RELEASES_ROOT}/{id}");
        let keys_json = r#"["assets/a.css","assets/b.js"]"#;
        let identity = ReleaseAssetIdentity::parse(
            id,
            prefix.clone(),
            format!("{prefix}/manifest.json"),
            keys_json.into(),
            &keys_hash(keys_json),
        )
        .unwrap();

        assert_eq!(
            identity.logical_keys_sorted(),
            vec!["assets/a.css", "assets/b.js"]
        );
        assert_ne!(
            identity.logical_keys_sorted(),
            vec!["assets/a.css", "assets/tampered.js"]
        );
        assert!(identity.physical_object_key("assets/tampered.js").is_none());
    }

    #[test]
    fn scoped_storage_rejects_managed_put_delete_and_list_helpers() {
        let id = "34".repeat(32);
        let prefix = format!("{RELEASES_ROOT}/{id}");
        let keys_json = r#"["public/app.css"]"#;
        let identity = ReleaseAssetIdentity::parse(
            id,
            prefix.clone(),
            format!("{prefix}/manifest.json"),
            keys_json.into(),
            &keys_hash(keys_json),
        )
        .unwrap();

        scope_sync(RequestServices::marker_with_release(1, identity), || {
            assert!(
                ScopedStorageService::reject_managed_object_mutation("public", "app.css").is_err()
            );
            // Both put and delete call the same exact-object guard.
            assert!(
                ScopedStorageService::reject_managed_object_mutation("public", "upload.css")
                    .is_ok()
            );
            assert!(ScopedStorageService::reject_managed_folder_listing("public").is_err());
            assert!(ScopedStorageService::reject_managed_folder_listing("uploads").is_ok());
        });
    }
}
