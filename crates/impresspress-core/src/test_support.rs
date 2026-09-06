//! Test infrastructure for impresspress-core integration tests.
//!
//! [`TestContext`] wires a real in-memory SQLite database (via the production
//! `DatabaseBlock` + `SQLiteDatabaseService::open_in_memory()`) into a minimal
//! [`Context`] implementation so unit and integration tests can exercise the
//! full block client stack without running a server process.
//!
//! Additional capabilities (message helpers, auth state, extra block dispatch)
//! are added in subsequent tasks.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use wafer_run::{
    context::Context,
    streams::output::{BufferedResponse, TerminalNotResponse},
    Block, BlockInfo, ErrorCode, InputStream, Message, OutputStream, ResourceGrant, WaferError,
};

use crate::routing::ExtraRoute;

/// Minimal test context backed by a real in-memory SQLite database.
///
/// Routes `"wafer-run/database"` calls to the production `DatabaseBlock`.
/// Other named blocks can be registered via `blocks` (unused until Task 6).
///
/// `Clone` is shallow — every interior field is already `Arc`/`Mutex`-shared
/// (or trivially copyable), so a clone produces another handle pointing at
/// the same database, blocks map, and config. Used by [`Context::clone_arc`]
/// so service objects (e.g. `AuthServiceImpl`) can stash an owning context
/// handle in a `OnceLock` past the lifetime of a `&TestContext` borrow.
#[derive(Clone)]
pub struct TestContext {
    /// The raw `DatabaseService` behind `database_block`, kept around so
    /// [`Self::break_writes`] can rewrap it in a fault-injecting decorator
    /// without losing the underlying in-memory SQLite data.
    db_service: Arc<dyn wafer_core::interfaces::database::service::DatabaseService>,
    database_block: Arc<dyn Block>,
    /// Config snapshot used by `config_get`. Immutable after construction so
    /// `config_get` can return `Option<&str>` without holding a lock.
    /// Populated via [`set_config`].
    config: Arc<HashMap<String, String>>,
    /// Placeholder for dynamically registered blocks — populated by Task 6.
    pub blocks: Arc<Mutex<HashMap<String, Arc<dyn Block>>>>,
    /// `BlockInfo` for every block registered via [`Self::register_block`],
    /// keyed by (and kept in sync with) `blocks`. Backs
    /// `Context::registered_blocks()` so handler code that gates behavior on
    /// "is block X registered?" (e.g. the vector block's backend-availability
    /// check, `blocks::vector::service::vector_backend_available`) sees the
    /// same signal in tests that a real `RuntimeContext` — whose
    /// `registered_blocks()` reflects its sealed startup snapshot — would
    /// produce in production. A plain `Vec` (not wrapped in the `blocks`
    /// mutex) is enough: `register_block` already takes `&mut self`, so no
    /// interior mutability is needed to update it, and `registered_blocks()`
    /// can return a borrowed slice directly instead of cloning through a
    /// lock guard.
    block_infos: Vec<wafer_run::BlockInfo>,
    /// WRAP-enforcement caller identity. `None` = WRAP checks skipped (the
    /// default — keeps existing tests untouched). Set via [`with_wrap`].
    caller_id: Option<String>,
    /// Grants visible to the WRAP check. Empty unless [`with_wrap`] populates.
    wrap_grants: Vec<ResourceGrant>,
    /// Admin block id for the WRAP check (`""` = no admin override).
    wrap_admin_block: String,
    /// Routes a fixture registered the way a downstream project would, via
    /// `ImpresspressBuilder::add_route`. Fed to [`Self::dispatch`] so a test
    /// exercises the same router path — including its access gate — that the
    /// consumer's registration produces, instead of calling a block's
    /// `handle()` past the gate.
    extra_routes: Vec<ExtraRoute>,
    /// The object store behind the registered `wafer-run/storage` block, when
    /// a constructor installed one. Kept so [`Self::storage_get`] and
    /// [`Self::storage_ops`] can read the store *underneath* the namespacing
    /// wrapper — the only way a test can assert what another block's
    /// namespace actually holds.
    storage: Option<Arc<InMemoryStorageService>>,
    /// The shared state the registered `impresspress/dev` block was built
    /// over, when [`Self::with_dev`] installed one. Handed back by
    /// [`Self::dev_shared`] so a test can drive the activation queue directly
    /// rather than only through HTTP.
    #[cfg(feature = "block-dev")]
    dev_shared: Option<Arc<crate::blocks::dev::DevShared>>,
}

impl TestContext {
    /// Construct a `TestContext` with a fresh in-memory SQLite database.
    pub async fn new() -> Self {
        let svc: Arc<dyn wafer_core::interfaces::database::service::DatabaseService> = Arc::new(
            wafer_block_sqlite::service::SQLiteDatabaseService::open_in_memory()
                .expect("open in-memory sqlite"),
        );
        let database_block: Arc<dyn Block> = Arc::new(
            wafer_core::service_blocks::database::DatabaseBlock::new(svc.clone()),
        );

        Self {
            db_service: svc,
            database_block,
            config: Arc::new(HashMap::new()),
            blocks: Arc::new(Mutex::new(HashMap::new())),
            block_infos: Vec::new(),
            caller_id: None,
            wrap_grants: Vec::new(),
            wrap_admin_block: String::new(),
            extra_routes: Vec::new(),
            storage: None,
            #[cfg(feature = "block-dev")]
            dev_shared: None,
        }
    }

    /// Insert a single config entry into the snapshot.
    ///
    /// Makes a fresh `Arc<HashMap>` clone on each call — fine for tests,
    /// where the number of entries is small and mutations happen at setup time.
    ///
    /// Also (re)registers a real `wafer-run/config` service block backed by the
    /// updated map, so block code that reads config through the typed client
    /// (`wafer_core::clients::config::get`/`get_default`) sees the same values
    /// as `config_get`. Without this, those client calls route to an
    /// unregistered block and silently fall back to their hardcoded default.
    pub fn set_config(&mut self, key: &str, value: &str) {
        let mut map = (*self.config).clone();
        map.insert(key.to_string(), value.to_string());
        self.config = Arc::new(map);

        let svc = wafer_core::service_blocks::config::EnvConfigService::new();
        for (k, v) in self.config.iter() {
            wafer_core::interfaces::config::service::ConfigService::set(&svc, k, v);
        }
        let block: Arc<dyn Block> = Arc::new(wafer_core::service_blocks::config::ConfigBlock::new(
            Arc::new(svc),
        ));
        self.register_block("wafer-run/config", block);
    }

    /// Opt the test into WRAP enforcement on `call_block`.
    ///
    /// Until called, `call_block` ignores `wrap.resource` meta — this matches
    /// pre-existing test behaviour. After calling, the same WRAP rules the
    /// production runtime applies (own-resource, admin override, grant match)
    /// gate every `call_block` invocation that carries `wrap.resource` meta.
    /// Typed clients (`wafer_core::clients::database::*`, etc.) set this
    /// meta automatically, so this is what makes a test exercise grants.
    ///
    /// `caller_id` is the block id the test is acting as — typically the
    /// block whose handler is under test. `grants` is the list visible to
    /// the WRAP check; tests that want to exercise a real block's grants
    /// should source them from `<Block>::default().info().grants` rather
    /// than re-listing grant literals.
    pub fn with_wrap(
        mut self,
        caller_id: &str,
        grants: Vec<ResourceGrant>,
        admin_block: &str,
    ) -> Self {
        self.caller_id = Some(caller_id.to_string());
        self.wrap_grants = grants;
        self.wrap_admin_block = admin_block.to_string();
        self
    }

    /// Apply one block's migrations into this fixture through the same gated
    /// path the runtime uses ([`crate::migration_helper::apply_migrations`]),
    /// sourcing the SQL from the block's single-source `SQLITE_MIGRATIONS` /
    /// `POSTGRES_MIGRATIONS` consts.
    ///
    /// Replaces the per-block `migrations::apply()` wrappers (deleted when the
    /// `impresspress_feature_block!` macro folded each block's `lifecycle(Init)`
    /// into `migration_helper::lifecycle_init`). Test-fixture setup is an
    /// explicit exception to the no-raw-migration-runner rule; it mirrors the
    /// production gate exactly so fixtures exercise the real schema.
    async fn apply_block_migrations(
        &self,
        block_name: &str,
        sqlite: &[(&str, &str)],
        postgres: &[&str],
    ) {
        let sqlite_sql: Vec<&str> = sqlite.iter().map(|(_, sql)| *sql).collect();
        crate::migration_helper::apply_migrations(self, block_name, &sqlite_sql, postgres)
            .await
            .unwrap_or_else(|e| panic!("apply {block_name} migrations in test fixture: {e}"));
    }

    /// Build a `TestContext` with admin + auth block migrations applied.
    ///
    /// Convenience constructor for tests that need the
    /// `wafer_run__auth__{users,orgs,sessions,provider_links,...}` schema
    /// in place — most repo and handler tests do.
    ///
    /// Admin migrations run first so that the
    /// `impresspress__admin__block_settings` tracking table exists before
    /// auth's `apply_if_blessed` upserts its `current_hash` row. In
    /// production this ordering is guaranteed by `register_feature_blocks`
    /// (admin is registered first); here we enforce it explicitly.
    pub async fn with_auth() -> Self {
        let ctx = Self::with_admin().await;
        ctx.apply_block_migrations(
            "wafer-run/auth",
            crate::blocks::auth::migrations::SQLITE_MIGRATIONS,
            crate::blocks::auth::migrations::POSTGRES_MIGRATIONS,
        )
        .await;
        ctx
    }

    /// Add auth's migrations to an EXISTING fixture — the same
    /// `wafer_run__auth__*` schema [`Self::with_auth`] applies to a fresh
    /// context, layered instead on top of whatever `self` already carries.
    ///
    /// Mirrors [`Self::with_dev_added`]'s shape for the same reason: a caller
    /// that needs auth's tables *alongside* another block's own fixture —
    /// e.g. `TestContext::with_products().await.with_auth_added().await`, for
    /// a scenario that seeds a real owner account next to a product — has no
    /// way to reach `with_auth`'s migrations without also re-running
    /// `with_products`'s from scratch. Admin's migrations are idempotent
    /// (`CREATE TABLE IF NOT EXISTS`) but re-registering a block is not
    /// something either constructor needs to redo here.
    pub async fn with_auth_added(self) -> Self {
        self.apply_block_migrations(
            "wafer-run/auth",
            crate::blocks::auth::migrations::SQLITE_MIGRATIONS,
            crate::blocks::auth::migrations::POSTGRES_MIGRATIONS,
        )
        .await;
        self
    }

    /// Seed one `wafer_run__auth__users` row under a caller-chosen id.
    ///
    /// The ONE raw-SQL users fixture in this crate (CLAUDE.md's test-fixture
    /// exception). Ten test modules hand-wrote this same `INSERT` because
    /// `repo::users::insert` mints a UUID, while each of them needs the id
    /// its own authenticated `Message` — or a foreign key on another auth
    /// table — already names. Consolidating them keeps the users table's
    /// wire name and NOT NULL column set spelled in one place, which is what
    /// `tests/repo_door.rs` checks.
    ///
    /// `email` is `{user_id}@example.com` and `display_name` is `user_id`;
    /// the role is `"user"`, so a test that needs an admin sets it through
    /// the repo afterwards.
    pub async fn seed_auth_user(&self, user_id: &str) {
        self.seed_auth_user_verified(user_id, false).await;
    }

    /// [`Self::seed_auth_user`] with an explicit `email_verified` flag,
    /// written as the `0`/`1` INTEGER the migration declares.
    pub async fn seed_auth_user_verified(&self, user_id: &str, email_verified: bool) {
        wafer_core::clients::database::exec_raw(
            self,
            "INSERT INTO wafer_run__auth__users \
             (id, email, display_name, role, email_verified, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            &[
                serde_json::json!(user_id),
                serde_json::json!(format!("{user_id}@example.com")),
                serde_json::json!(user_id),
                serde_json::json!("user"),
                serde_json::json!(i64::from(email_verified)),
                serde_json::json!("2026-01-01T00:00:00Z"),
                serde_json::json!("2026-01-01T00:00:00Z"),
            ],
        )
        .await
        .expect("seed auth user fixture");
    }

    /// Build a `TestContext` with admin migrations applied (only).
    ///
    /// Use this for tests that exercise a block's own `init()` / migration
    /// application directly — the prerequisite is that
    /// `impresspress__admin__block_settings` exists so `apply_if_blessed` can
    /// upsert its tracking row.
    pub async fn with_admin() -> Self {
        let ctx = Self::new().await;
        ctx.apply_block_migrations(
            "impresspress/admin",
            crate::blocks::admin::migrations::SQLITE_MIGRATIONS,
            crate::blocks::admin::migrations::POSTGRES_MIGRATIONS,
        )
        .await;
        ctx
    }

    /// Build a `TestContext` with admin + auth + files migrations applied.
    #[cfg(feature = "block-files")]
    pub async fn with_files() -> Self {
        let ctx = Self::with_auth().await;
        ctx.apply_block_migrations(
            "impresspress/files",
            crate::blocks::files::migrations::SQLITE_MIGRATIONS,
            crate::blocks::files::migrations::POSTGRES_MIGRATIONS,
        )
        .await;
        ctx
    }

    /// Build a `TestContext` with admin + auth + userportal migrations applied.
    #[cfg(feature = "block-userportal")]
    pub async fn with_userportal() -> Self {
        let ctx = Self::with_auth().await;
        ctx.apply_block_migrations(
            "impresspress/userportal",
            crate::blocks::userportal::migrations::SQLITE_MIGRATIONS,
            crate::blocks::userportal::migrations::POSTGRES_MIGRATIONS,
        )
        .await;
        ctx
    }

    /// Build a TestContext with admin, auth, and tickets migrations applied.
    #[cfg(feature = "block-tickets")]
    pub async fn with_tickets() -> Self {
        let mut ctx = Self::with_auth().await;
        ctx.apply_block_migrations(
            "impresspress/tickets",
            crate::blocks::tickets::migrations::SQLITE_MIGRATIONS,
            crate::blocks::tickets::migrations::POSTGRES_MIGRATIONS,
        )
        .await;
        ctx.register_block(
            "impresspress/tickets",
            Arc::new(crate::blocks::tickets::TicketsBlock::new()),
        );
        ctx
    }

    /// Build a `TestContext` with admin + auth + vector migrations applied.
    #[cfg(feature = "block-vector")]
    pub async fn with_vector() -> Self {
        let ctx = Self::with_auth().await;
        ctx.apply_block_migrations(
            "impresspress/vector",
            crate::blocks::vector::migrations::SQLITE_MIGRATIONS,
            crate::blocks::vector::migrations::POSTGRES_MIGRATIONS,
        )
        .await;
        ctx
    }

    /// Build a `TestContext` with admin + llm migrations applied.
    ///
    /// Admin first so the `impresspress__admin__block_settings` tracking
    /// table exists before llm's `apply_if_blessed` upserts its row (the
    /// production ordering). llm's schema does not depend on auth, so auth
    /// migrations are skipped.
    #[cfg(feature = "block-llm")]
    pub async fn with_llm() -> Self {
        let ctx = Self::with_admin().await;
        ctx.apply_block_migrations(
            "impresspress/llm",
            crate::blocks::llm::migrations::SQLITE_MIGRATIONS,
            crate::blocks::llm::migrations::POSTGRES_MIGRATIONS,
        )
        .await;
        ctx
    }

    /// Build a `TestContext` with admin + products migrations applied.
    ///
    /// Admin migrations run first so the `impresspress__admin__block_settings`
    /// tracking table exists before products' `apply_if_blessed` upserts its
    /// `current_hash` row (the production ordering, enforced explicitly here).
    /// Products' schema does not depend on auth, so auth migrations are
    /// skipped.
    #[cfg(feature = "block-products")]
    pub async fn with_products() -> Self {
        let mut ctx = Self::with_admin().await;
        ctx.apply_block_migrations(
            "impresspress/products",
            crate::blocks::products::migrations::SQLITE_MIGRATIONS,
            crate::blocks::products::migrations::POSTGRES_MIGRATIONS,
        )
        .await;
        // Register the real block so `registered_blocks()` reports it — the
        // context IS a products deployment (its migrations just ran). Nav
        // rendering gates the Products sidebar item on this signal.
        ctx.register_block(
            "impresspress/products",
            Arc::new(crate::blocks::products::ProductsBlock::new()),
        );
        ctx
    }

    /// Build a `TestContext` with admin + dev-sandbox migrations applied, the
    /// `impresspress/dev` block registered over `control`, and the `/b/dev`
    /// `Admin` extra route added the way `ImpresspressBuilder::add_route`
    /// would.
    ///
    /// WRAP enforcement is switched on with
    /// [`crate::blocks::dev::wrap_grants`], so a test exercises the real
    /// grant set: the block's own
    /// `impresspress__dev__*` tables self-admit under the own-namespace rule,
    /// and the published site under `wafer-run/web/site/*` is reachable only
    /// because that grant is present. Migrations run before enforcement
    /// starts, exactly as they do at boot.
    #[cfg(feature = "block-dev")]
    pub async fn with_dev(control: Arc<dyn crate::blocks::dev::RuntimeControl>) -> Self {
        Self::with_admin().await.with_dev_added(control).await
    }

    /// Add the `impresspress/dev` block to an EXISTING fixture — the dev
    /// migrations, block registration, storage wiring, `/b/dev` extra route
    /// and WRAP enforcement [`Self::with_dev`] applies to a fresh context,
    /// applied instead on top of whatever `self` already carries.
    ///
    /// For a test that needs `impresspress/dev` alongside another block's
    /// own fixture — e.g. `TestContext::with_products().await.with_dev_added(..)`
    /// for `/b/dev/api/tools.json`, which projects both blocks' endpoints
    /// into one manifest. [`Self::with_dev`] cannot do this: it always starts
    /// from a bare `with_admin()` context, so a caller that needs a second
    /// block already registered would have no way to add it before the dev
    /// block's WRAP enforcement switches on.
    ///
    /// Migrations, not registration order, are what production guarantees
    /// (admin's `block_settings` table before any block's `apply_if_blessed`
    /// upsert) — adding dev's migrations after another block's have already
    /// run is exactly what a deployment that enables the sandbox alongside an
    /// existing block set does, so this is not a fixture-only shortcut.
    #[cfg(feature = "block-dev")]
    pub async fn with_dev_added(
        self,
        control: Arc<dyn crate::blocks::dev::RuntimeControl>,
    ) -> Self {
        // The default shell is a plausible one rather than an empty one: a
        // fixture that had no shell at all could not tell "this test does not
        // exercise the export" apart from "the export produced a folder with
        // no runtime in it", and the second is precisely what
        // `blocks::dev::export` refuses.
        self.with_dev_added_and_shell(
            control,
            Arc::new(crate::blocks::dev::test_support::FakeShell::new()),
        )
        .await
    }

    /// [`Self::with_dev_added`] with an explicit [`crate::blocks::dev::ShellSource`].
    ///
    /// For the export tests, whose subject IS the shell: which files it
    /// carries, what its `sw.js` says, and what happens when it cannot be
    /// listed. Everything else about the fixture is identical — this is the
    /// one function that wires the dev block, and `with_dev_added` is a call
    /// to it with the default shell.
    #[cfg(feature = "block-dev")]
    pub async fn with_dev_added_and_shell(
        mut self,
        control: Arc<dyn crate::blocks::dev::RuntimeControl>,
        shell: Arc<dyn crate::blocks::dev::ShellSource>,
    ) -> Self {
        use crate::blocks::dev;

        self.apply_block_migrations(
            dev::BLOCK_NAME,
            dev::migrations::SQLITE_MIGRATIONS,
            dev::migrations::POSTGRES_MIGRATIONS,
        )
        .await;
        let shared = dev::DevShared::new(control, shell);
        self.dev_shared = Some(shared.clone());
        self.register_block(
            dev::BLOCK_NAME,
            Arc::new(dev::DevBlock::with_workspace(shared)),
        );
        // The workspace store (blobs + `workspace.json`) lives in storage,
        // so the fixture needs a real object store behind the production
        // namespacing wrapper — that wrapper is what turns the block's own
        // `blobs` / `""` folders into `impresspress/dev/…`, and what would
        // refuse a cross-block reach that the grant list did not cover.
        let store = Arc::new(InMemoryStorageService::new());
        self.storage = Some(store.clone());
        let storage = crate::blocks::storage::create(store, Arc::from("impresspress/admin"));
        // The storage block runs its OWN cross-block check against a grant
        // list the runtime injects after startup. Leaving it empty (the
        // constructor's state) would refuse every `@`-prefixed reach
        // regardless of grants, so a fixture that skipped this could not tell
        // a missing grant from an unconfigured block — and the site publish
        // Task 7 builds on would fail here for the wrong reason.
        storage.update_wrap_grants(&dev::wrap_grants());
        self.register_block("wafer-run/storage", storage);
        self.add_extra_route(ExtraRoute::new(
            dev::ROUTE_PREFIX.to_string(),
            dev::BLOCK_NAME.to_string(),
            crate::routing::RouteAccess::Admin,
        ));
        self.with_wrap(dev::BLOCK_NAME, dev::wrap_grants(), "impresspress/admin")
    }

    /// Register a route the way `ImpresspressBuilder::add_route` does, so
    /// [`Self::dispatch`] routes through it.
    pub fn add_extra_route(&mut self, route: ExtraRoute) {
        self.extra_routes.push(route);
    }

    /// The shared state the fixture's `impresspress/dev` block is built over.
    ///
    /// The same `Arc` the block holds — not a copy — so a test that drives
    /// `activation::request` through this handle contends for the very queue
    /// the HTTP handlers use.
    #[cfg(feature = "block-dev")]
    pub fn dev_shared(&self) -> Arc<crate::blocks::dev::DevShared> {
        self.dev_shared
            .clone()
            .expect("this fixture registered no dev block; use TestContext::with_dev")
    }

    /// Read an object out of the fixture's object store *underneath* the
    /// per-block namespacing wrapper.
    ///
    /// `block` is the namespace owner (`"wafer-run/web"`), `folder` its folder
    /// (`"site"`), `key` the object — i.e. exactly the three parts
    /// [`crate::blocks::storage`] concatenates into `{block}/{folder}/{key}`.
    /// Going through the store directly is the point: a test asserting what
    /// the *published site* holds must not be able to satisfy itself by
    /// reading through the dev block's own grants.
    pub async fn storage_get(
        &self,
        block: &str,
        folder: &str,
        key: &str,
    ) -> Result<Vec<u8>, wafer_core::interfaces::storage::service::StorageError> {
        use wafer_core::interfaces::storage::service::StorageService as _;
        let (bytes, _info) = self
            .storage()
            .get(&format!("{block}/{folder}"), key)
            .await?;
        Ok(bytes)
    }

    /// Make the fixture's object store refuse the next `put`.
    ///
    /// The one failure a test cannot otherwise produce: a publish that fails
    /// after the runtime has already been swapped, which the activation queue
    /// has to unwind.
    pub fn fail_next_storage_put(&self, message: &str) {
        self.storage().fail_next_put(message);
    }

    /// Every mutating storage operation the fixture's store has seen, oldest
    /// first, as `"{op} {folder}/{key}"`.
    ///
    /// Ordering is the assertion this exists for: the site publisher must
    /// write `index.html` after the assets it references, and only the order
    /// of the `put`s can show that.
    pub fn storage_ops(&self) -> Vec<String> {
        self.storage().ops()
    }

    /// The fixture's object store as the platform service handle.
    ///
    /// For the code paths that hold a `StorageService` rather than a
    /// [`wafer_run::context::Context`] — the runtime rebuild reads guest
    /// artifacts before there is a runtime to route a `wafer-run/storage` call
    /// through. Deliberately the *same* store [`Self::storage_get`] reads, so
    /// a test can prove the two access paths address the same objects.
    pub fn storage_service(
        &self,
    ) -> Arc<dyn wafer_core::interfaces::storage::service::StorageService> {
        self.storage
            .clone()
            .expect("this fixture registered no storage service")
    }

    fn storage(&self) -> &InMemoryStorageService {
        self.storage
            .as_deref()
            .expect("this fixture registered no storage service")
    }

    /// Route `msg` through [`crate::routing::route_to_block`] using this
    /// context's registered `BlockInfo`s and extra routes.
    ///
    /// This is the whole request path a block sees in production minus the
    /// pipeline's auth/CSRF preamble: in particular the router's access gate
    /// runs, so a test can assert that an admin-only route rejects an
    /// anonymous or non-admin caller without the block containing any
    /// role check of its own.
    pub async fn dispatch(&self, msg: Message) -> OutputStream {
        self.dispatch_with_input(msg, InputStream::empty()).await
    }

    /// [`Self::dispatch`] for a request that carries a body.
    ///
    /// `dispatch` exists for the reads; a `POST`/`PATCH` handler reads its
    /// body off the `InputStream`, which `dispatch` hands it empty. Routing a
    /// write by calling the block's `handle()` directly would skip the
    /// router's access gate — the very thing `dispatch` exists to exercise —
    /// so the body belongs on this path, not on a second one.
    pub async fn dispatch_with_input(&self, msg: Message, input: InputStream) -> OutputStream {
        crate::routing::route_to_block(
            self,
            msg,
            input,
            &crate::features::AllEnabled,
            &self.block_infos,
            &self.extra_routes,
        )
        .await
    }

    /// [`Self::dispatch_with_input`] with `body` serialized as the JSON
    /// request body.
    pub async fn dispatch_json<T: serde::Serialize>(&self, msg: Message, body: &T) -> OutputStream {
        let bytes = serde_json::to_vec(body).expect("serialize test request body");
        self.dispatch_with_input(msg, InputStream::from_bytes(bytes))
            .await
    }

    /// Register a block under `name`. Calls to `ctx.call_block(name, ...)`
    /// will route to this block's `handle()`.
    ///
    /// Used to wire up cross-block call tests — e.g. the dashboard handler
    /// in the auth block calls `"impresspress/userportal"` for the buttons
    /// list; tests register a real or fake `UserPortalBlock` so the call
    /// resolves.
    pub fn register_block(&mut self, name: &str, block: Arc<dyn Block>) {
        // Keep `block_infos` a deduplicated mirror of `blocks`'s keys — a
        // block re-registered under the same name (e.g. `set_config`
        // calling `register_block("wafer-run/config", ..)` again) replaces
        // its old entry rather than appending a duplicate. The registration
        // name (not whatever `block.info().name` happens to report) is
        // authoritative, matching how `blocks` itself is keyed.
        self.block_infos.retain(|b| b.name != name);
        let mut info = block.info();
        info.name = name.to_string();
        self.block_infos.push(info);

        self.blocks
            .lock()
            .expect("blocks mutex poisoned")
            .insert(name.to_string(), block);
    }

    /// Put a `BlockInfo` into `Context::registered_blocks()` without a block
    /// behind it.
    ///
    /// [`Self::register_block`]'s snapshot half, for the one registration a
    /// fixture cannot express through it: a *dynamic* sandbox block. Those
    /// are compiled guests — the fixture drives them through
    /// `blocks::dev::test_support::FakeControl`, which has no `dyn Block` to
    /// hand over — yet the real runtime registers each live one via
    /// `ImpresspressBuilder::extra_block` on every rebuild, so its `BlockInfo`
    /// IS in the sealed snapshot that `registered_blocks()` returns. A test
    /// that wants to reason about what a *rebuilt* runtime looks like has to
    /// be able to say so; without this it can only ever see the built-ins,
    /// which is precisely how the sandbox shipped a rule that refused a block
    /// its own agent tool names on recompile.
    ///
    /// Same de-duplication as `register_block`, and the same authority: the
    /// name passed in wins over whatever `info.name` says, so a re-registered
    /// block replaces its entry instead of appending a second one.
    pub fn register_block_info(&mut self, name: &str, mut info: wafer_run::BlockInfo) {
        self.block_infos.retain(|b| b.name != name);
        info.name = name.to_string();
        self.block_infos.push(info);
    }

    /// Replace the database backing this context with one whose mutating
    /// operations (`create`/`update`/`delete`/`upsert`) always fail with a
    /// simulated operational error, while every read (`get`/`list`/`count`/
    /// schema checks/…) still delegates to the real in-memory SQLite data.
    ///
    /// Used to test that a mutation handler surfaces — rather than
    /// discards — a genuine persistence failure (e.g. it must not write a
    /// success audit-log row or report success to the caller). Reads still
    /// working means a handler's "read current state, then try to persist a
    /// change" shape (block-settings toggle, etc.) exercises the exact
    /// branch under test instead of failing earlier for an unrelated reason.
    pub fn break_writes(mut self) -> Self {
        let broken: Arc<dyn wafer_core::interfaces::database::service::DatabaseService> =
            Arc::new(FailingWritesDb {
                inner: self.db_service.clone(),
            });
        self.db_service = broken.clone();
        self.database_block = Arc::new(wafer_core::service_blocks::database::DatabaseBlock::new(
            broken,
        ));
        self
    }

    /// Replace the database backing this context with one whose read
    /// operations (`get`/`list`/`count`/`sum`/`query_raw`/`aggregate`) always
    /// fail with a simulated operational error, while schema checks and
    /// mutating operations still delegate to the real in-memory SQLite data.
    ///
    /// The mirror image of [`Self::break_writes`]: used to test that a
    /// read/repository function surfaces — rather than swallows — a genuine
    /// read failure instead of collapsing it into the same "not found" /
    /// zero-valued result it uses for a legitimate absence.
    pub fn break_reads(self) -> Self {
        self.with_failing_reads(true)
    }

    /// Like [`Self::break_reads`], but single-row `get` still delegates to the
    /// real data: only the multi-row reads (`list`/`count`/`sum`/`aggregate`/
    /// `query_raw`) fail.
    ///
    /// This is the shape a "load one record, then query for related rows"
    /// handler actually faces when the database wobbles — the single-row read
    /// lands and the follow-up query is the one that fails.
    /// [`Self::break_reads`] cannot reach that branch at all, because it fails
    /// the first read and the handler never gets as far as the query under
    /// test.
    pub fn break_list_reads(self) -> Self {
        self.with_failing_reads(false)
    }

    fn with_failing_reads(mut self, fail_get: bool) -> Self {
        let broken: Arc<dyn wafer_core::interfaces::database::service::DatabaseService> =
            Arc::new(FailingReadsDb {
                inner: self.db_service.clone(),
                fail_get,
            });
        self.db_service = broken.clone();
        self.database_block = Arc::new(wafer_core::service_blocks::database::DatabaseBlock::new(
            broken,
        ));
        self
    }
}

/// `DatabaseService` decorator used by [`TestContext::break_reads`] and
/// [`TestContext::break_list_reads`]. Every read method fails with
/// [`DatabaseError::Internal`]; every mutating/schema method delegates to
/// `inner` unchanged.
///
/// "Mutating" includes the filtered-write family (`update_where*`,
/// `delete_where*`, `increment_field_where`), which this decorator must
/// override explicitly even though it has nothing to change about them. The
/// `DatabaseService` trait ships *read-based default implementations* of
/// those — `update_where_count` counts and then updates, `update_where`
/// lists and then updates by id — so a decorator that leaves them alone
/// inherits a write that begins with a read, and every filtered write fails
/// here for a reason no real backend has. `wafer-block-sqlite`,
/// `wafer-block-postgres` and `D1DatabaseService` all override the family
/// with a single statement carrying no `count` and no `list`, so a test
/// double that keeps the defaults asserts against a database that does not
/// exist: a handler branch reachable only when a filtered write fails looks
/// covered while production can never enter it.
struct FailingReadsDb {
    inner: Arc<dyn wafer_core::interfaces::database::service::DatabaseService>,
    /// `false` exempts single-row `get` from the failure, so a test can fail
    /// a *listing* while a by-id read still succeeds — see
    /// [`TestContext::break_list_reads`].
    fail_get: bool,
}

#[async_trait::async_trait]
impl wafer_core::interfaces::database::service::DatabaseService for FailingReadsDb {
    async fn get(
        &self,
        collection: &str,
        id: &str,
    ) -> Result<
        wafer_core::interfaces::database::service::Record,
        wafer_core::interfaces::database::service::DatabaseError,
    > {
        if self.fail_get {
            return Err(simulated_read_failure());
        }
        self.inner.get(collection, id).await
    }

    async fn list(
        &self,
        _collection: &str,
        _opts: &wafer_block::db::ListOptions,
    ) -> Result<
        wafer_core::interfaces::database::service::RecordList,
        wafer_core::interfaces::database::service::DatabaseError,
    > {
        Err(simulated_read_failure())
    }

    async fn create(
        &self,
        collection: &str,
        data: HashMap<String, serde_json::Value>,
    ) -> Result<
        wafer_core::interfaces::database::service::Record,
        wafer_core::interfaces::database::service::DatabaseError,
    > {
        self.inner.create(collection, data).await
    }

    async fn update(
        &self,
        collection: &str,
        id: &str,
        data: HashMap<String, serde_json::Value>,
    ) -> Result<
        wafer_core::interfaces::database::service::Record,
        wafer_core::interfaces::database::service::DatabaseError,
    > {
        self.inner.update(collection, id, data).await
    }

    async fn delete(
        &self,
        collection: &str,
        id: &str,
    ) -> Result<(), wafer_core::interfaces::database::service::DatabaseError> {
        self.inner.delete(collection, id).await
    }

    // --- the filtered-write family ---------------------------------------
    //
    // Delegated, not failed. Each is one `UPDATE`/`DELETE … WHERE` statement
    // on every real backend; the trait's read-based defaults (see the struct
    // doc above) are what these overrides exist to displace.
    //
    // `take_where` is deliberately NOT here: it returns the rows it removed,
    // so it is a read as much as a write and belongs on the failing side. It
    // is stated there explicitly, for the same reason the family below is
    // stated here — the trait's default reaches the failure through a `list`
    // the real backends' single `DELETE … RETURNING *` never issues, so
    // inheriting it would make the double right by accident.

    async fn update_where(
        &self,
        collection: &str,
        filters: &[wafer_block::db::Filter],
        data: HashMap<String, serde_json::Value>,
    ) -> Result<(), wafer_core::interfaces::database::service::DatabaseError> {
        self.inner.update_where(collection, filters, data).await
    }

    async fn update_where_count(
        &self,
        collection: &str,
        filters: &[wafer_block::db::Filter],
        data: HashMap<String, serde_json::Value>,
    ) -> Result<i64, wafer_core::interfaces::database::service::DatabaseError> {
        self.inner
            .update_where_count(collection, filters, data)
            .await
    }

    async fn delete_where(
        &self,
        collection: &str,
        filters: &[wafer_block::db::Filter],
    ) -> Result<(), wafer_core::interfaces::database::service::DatabaseError> {
        self.inner.delete_where(collection, filters).await
    }

    async fn delete_where_count(
        &self,
        collection: &str,
        filters: &[wafer_block::db::Filter],
    ) -> Result<i64, wafer_core::interfaces::database::service::DatabaseError> {
        self.inner.delete_where_count(collection, filters).await
    }

    async fn increment_field_where(
        &self,
        collection: &str,
        col: &str,
        delta: i64,
        filters: &[wafer_block::db::Filter],
    ) -> Result<i64, wafer_core::interfaces::database::service::DatabaseError> {
        self.inner
            .increment_field_where(collection, col, delta, filters)
            .await
    }

    // The one member of that family on the failing side: it hands back the
    // rows it removed.
    async fn take_where(
        &self,
        _collection: &str,
        _filters: &[wafer_block::db::Filter],
    ) -> Result<
        Vec<wafer_core::interfaces::database::service::Record>,
        wafer_core::interfaces::database::service::DatabaseError,
    > {
        Err(simulated_read_failure())
    }

    async fn count(
        &self,
        _collection: &str,
        _filters: &[wafer_block::db::Filter],
    ) -> Result<i64, wafer_core::interfaces::database::service::DatabaseError> {
        Err(simulated_read_failure())
    }

    async fn sum(
        &self,
        _collection: &str,
        _field: &str,
        _filters: &[wafer_block::db::Filter],
    ) -> Result<f64, wafer_core::interfaces::database::service::DatabaseError> {
        Err(simulated_read_failure())
    }

    async fn query_raw(
        &self,
        _query: &str,
        _args: &[serde_json::Value],
    ) -> Result<
        Vec<wafer_core::interfaces::database::service::Record>,
        wafer_core::interfaces::database::service::DatabaseError,
    > {
        Err(simulated_read_failure())
    }

    async fn exec_raw(
        &self,
        query: &str,
        args: &[serde_json::Value],
    ) -> Result<i64, wafer_core::interfaces::database::service::DatabaseError> {
        self.inner.exec_raw(query, args).await
    }

    async fn upsert(
        &self,
        collection: &str,
        spec: wafer_core::interfaces::database::service::UpsertSpec,
    ) -> Result<i64, wafer_core::interfaces::database::service::DatabaseError> {
        self.inner.upsert(collection, spec).await
    }

    async fn aggregate(
        &self,
        _collection: &str,
        _spec: wafer_core::interfaces::database::service::AggregateSpec,
    ) -> Result<
        Vec<wafer_core::interfaces::database::service::Record>,
        wafer_core::interfaces::database::service::DatabaseError,
    > {
        Err(simulated_read_failure())
    }

    async fn ensure_schema_table(
        &self,
        table: &wafer_core::interfaces::database::service::Table,
    ) -> Result<(), wafer_core::interfaces::database::service::DatabaseError> {
        self.inner.ensure_schema_table(table).await
    }

    async fn schema_table_exists(
        &self,
        name: &str,
    ) -> Result<bool, wafer_core::interfaces::database::service::DatabaseError> {
        self.inner.schema_table_exists(name).await
    }

    async fn schema_drop_table(
        &self,
        name: &str,
    ) -> Result<(), wafer_core::interfaces::database::service::DatabaseError> {
        self.inner.schema_drop_table(name).await
    }

    async fn schema_add_column(
        &self,
        table: &str,
        column: &wafer_core::interfaces::database::service::Column,
    ) -> Result<(), wafer_core::interfaces::database::service::DatabaseError> {
        self.inner.schema_add_column(table, column).await
    }
}

fn simulated_read_failure() -> wafer_core::interfaces::database::service::DatabaseError {
    wafer_core::interfaces::database::service::DatabaseError::Internal(
        "simulated operational read failure (TestContext::break_reads)".into(),
    )
}

/// `DatabaseService` decorator used by [`TestContext::break_writes`]. Every
/// mutating method fails with [`DatabaseError::Internal`]; every read/schema
/// method delegates to `inner` unchanged.
///
/// "Mutating" includes the filtered-write family (`update_where*`,
/// `delete_where*`, `take_where`, `increment_field_where`), which this
/// decorator must override explicitly — the mirror of the same paragraph on
/// [`FailingReadsDb`]. The `DatabaseService` trait ships *read-based default
/// implementations* of those: `update_where_count` counts and then updates,
/// `update_where` lists and then updates by id, `delete_where` lists and then
/// deletes by id, `take_where` lists and then deletes, and
/// `increment_field_where` reports that the backend does not implement it at
/// all. `wafer-block-sqlite`, `wafer-block-postgres` and `D1DatabaseService`
/// all override the family with a single statement carrying no `count` and no
/// `list`.
///
/// Inheriting the defaults here did not merely reach the right answer by the
/// wrong route — it reached the WRONG answer whenever the filter matched no
/// rows: the default lists nothing, writes nothing, and returns `Ok`, so a
/// filtered write SUCCEEDED on a database whose writes are supposed to be
/// failing. `repo::products::restore` of an already-live product is exactly
/// that write. A handler branch reachable only when a filtered write fails
/// then looks covered while production can never enter it; `restore_fails_
/// loudly_when_the_slug_collision_probe_cannot_run` was that test on
/// [`FailingReadsDb`], and this double kept the same defect armed for the
/// next one.
struct FailingWritesDb {
    inner: Arc<dyn wafer_core::interfaces::database::service::DatabaseService>,
}

#[async_trait::async_trait]
impl wafer_core::interfaces::database::service::DatabaseService for FailingWritesDb {
    async fn get(
        &self,
        collection: &str,
        id: &str,
    ) -> Result<
        wafer_core::interfaces::database::service::Record,
        wafer_core::interfaces::database::service::DatabaseError,
    > {
        self.inner.get(collection, id).await
    }

    async fn list(
        &self,
        collection: &str,
        opts: &wafer_block::db::ListOptions,
    ) -> Result<
        wafer_core::interfaces::database::service::RecordList,
        wafer_core::interfaces::database::service::DatabaseError,
    > {
        self.inner.list(collection, opts).await
    }

    async fn create(
        &self,
        _collection: &str,
        _data: HashMap<String, serde_json::Value>,
    ) -> Result<
        wafer_core::interfaces::database::service::Record,
        wafer_core::interfaces::database::service::DatabaseError,
    > {
        Err(simulated_write_failure())
    }

    async fn update(
        &self,
        _collection: &str,
        _id: &str,
        _data: HashMap<String, serde_json::Value>,
    ) -> Result<
        wafer_core::interfaces::database::service::Record,
        wafer_core::interfaces::database::service::DatabaseError,
    > {
        Err(simulated_write_failure())
    }

    async fn delete(
        &self,
        _collection: &str,
        _id: &str,
    ) -> Result<(), wafer_core::interfaces::database::service::DatabaseError> {
        Err(simulated_write_failure())
    }

    // --- the filtered-write family ---------------------------------------
    //
    // Failed, not inherited. See the struct doc above: every one of these is
    // one `UPDATE`/`DELETE … WHERE` statement on every real backend, and the
    // trait's read-based defaults answer `Ok` for a filter that matches
    // nothing — a successful write on a database whose writes are failing.

    async fn update_where(
        &self,
        _collection: &str,
        _filters: &[wafer_block::db::Filter],
        _data: HashMap<String, serde_json::Value>,
    ) -> Result<(), wafer_core::interfaces::database::service::DatabaseError> {
        Err(simulated_write_failure())
    }

    async fn update_where_count(
        &self,
        _collection: &str,
        _filters: &[wafer_block::db::Filter],
        _data: HashMap<String, serde_json::Value>,
    ) -> Result<i64, wafer_core::interfaces::database::service::DatabaseError> {
        Err(simulated_write_failure())
    }

    async fn delete_where(
        &self,
        _collection: &str,
        _filters: &[wafer_block::db::Filter],
    ) -> Result<(), wafer_core::interfaces::database::service::DatabaseError> {
        Err(simulated_write_failure())
    }

    async fn delete_where_count(
        &self,
        _collection: &str,
        _filters: &[wafer_block::db::Filter],
    ) -> Result<i64, wafer_core::interfaces::database::service::DatabaseError> {
        Err(simulated_write_failure())
    }

    async fn take_where(
        &self,
        _collection: &str,
        _filters: &[wafer_block::db::Filter],
    ) -> Result<
        Vec<wafer_core::interfaces::database::service::Record>,
        wafer_core::interfaces::database::service::DatabaseError,
    > {
        Err(simulated_write_failure())
    }

    async fn increment_field_where(
        &self,
        _collection: &str,
        _col: &str,
        _delta: i64,
        _filters: &[wafer_block::db::Filter],
    ) -> Result<i64, wafer_core::interfaces::database::service::DatabaseError> {
        Err(simulated_write_failure())
    }

    async fn count(
        &self,
        collection: &str,
        filters: &[wafer_block::db::Filter],
    ) -> Result<i64, wafer_core::interfaces::database::service::DatabaseError> {
        self.inner.count(collection, filters).await
    }

    async fn sum(
        &self,
        collection: &str,
        field: &str,
        filters: &[wafer_block::db::Filter],
    ) -> Result<f64, wafer_core::interfaces::database::service::DatabaseError> {
        self.inner.sum(collection, field, filters).await
    }

    async fn query_raw(
        &self,
        query: &str,
        args: &[serde_json::Value],
    ) -> Result<
        Vec<wafer_core::interfaces::database::service::Record>,
        wafer_core::interfaces::database::service::DatabaseError,
    > {
        self.inner.query_raw(query, args).await
    }

    async fn exec_raw(
        &self,
        _query: &str,
        _args: &[serde_json::Value],
    ) -> Result<i64, wafer_core::interfaces::database::service::DatabaseError> {
        Err(simulated_write_failure())
    }

    async fn upsert(
        &self,
        _collection: &str,
        _spec: wafer_core::interfaces::database::service::UpsertSpec,
    ) -> Result<i64, wafer_core::interfaces::database::service::DatabaseError> {
        Err(simulated_write_failure())
    }

    async fn aggregate(
        &self,
        collection: &str,
        spec: wafer_core::interfaces::database::service::AggregateSpec,
    ) -> Result<
        Vec<wafer_core::interfaces::database::service::Record>,
        wafer_core::interfaces::database::service::DatabaseError,
    > {
        self.inner.aggregate(collection, spec).await
    }

    async fn ensure_schema_table(
        &self,
        table: &wafer_core::interfaces::database::service::Table,
    ) -> Result<(), wafer_core::interfaces::database::service::DatabaseError> {
        self.inner.ensure_schema_table(table).await
    }

    async fn schema_table_exists(
        &self,
        name: &str,
    ) -> Result<bool, wafer_core::interfaces::database::service::DatabaseError> {
        self.inner.schema_table_exists(name).await
    }

    async fn schema_drop_table(
        &self,
        name: &str,
    ) -> Result<(), wafer_core::interfaces::database::service::DatabaseError> {
        self.inner.schema_drop_table(name).await
    }

    async fn schema_add_column(
        &self,
        table: &str,
        column: &wafer_core::interfaces::database::service::Column,
    ) -> Result<(), wafer_core::interfaces::database::service::DatabaseError> {
        self.inner.schema_add_column(table, column).await
    }
}

fn simulated_write_failure() -> wafer_core::interfaces::database::service::DatabaseError {
    wafer_core::interfaces::database::service::DatabaseError::Internal(
        "simulated operational write failure (TestContext::break_writes)".into(),
    )
}

#[async_trait::async_trait]
impl Context for TestContext {
    /// Host-side WRAP enforcement (only when the test opted in via
    /// `with_wrap`). Overrides the fail-closed trait default; mirrors
    /// `RuntimeContext::check_resource_access` — keyed on `caller_id`, same
    /// `check_access` callsite shape — so tests see identical permission
    /// behaviour to production. Without a caller the context is permissive,
    /// matching the pre-WRAP test default.
    fn check_resource_access(
        &self,
        resource: &str,
        resource_type: wafer_run::ResourceType,
        is_write: bool,
    ) -> Result<(), WaferError> {
        if let Some(ref caller) = self.caller_id {
            wafer_block::wrap::check_access(
                Some(caller.as_str()),
                resource,
                is_write,
                Some(&resource_type),
                &self.wrap_grants,
                &self.wrap_admin_block,
            )?;
        }
        Ok(())
    }

    async fn call_block(&self, name: &str, msg: Message, input: InputStream) -> OutputStream {
        // WRAP enforcement (only when the test opted in via `with_wrap`).
        // Mirrors `RuntimeContext::call_block` in wafer-run/crates/wafer-run/
        // src/context.rs:138-163 — same `check_access` callsite shape so
        // tests see identical permission behaviour to production.
        if let Some(ref caller) = self.caller_id {
            let resource = msg.get_meta(wafer_block::meta::META_WRAP_RESOURCE);
            if !resource.is_empty() {
                let is_write = msg.get_meta(wafer_block::meta::META_WRAP_ACCESS) == "write";
                let rt_str = msg.get_meta(wafer_block::meta::META_WRAP_RESOURCE_TYPE);
                let rt = wafer_run::ResourceType::parse_stored(if rt_str.is_empty() {
                    None
                } else {
                    Some(rt_str)
                })
                .unwrap_or_else(|e| panic!("test WRAP resource_type meta {rt_str:?}: {e}"));
                if let Err(e) = wafer_block::wrap::check_access(
                    Some(caller.as_str()),
                    resource,
                    is_write,
                    rt.as_ref(),
                    &self.wrap_grants,
                    &self.wrap_admin_block,
                ) {
                    return OutputStream::error(e);
                }
            }
        }

        match name {
            "wafer-run/database" => self.database_block.handle(self, msg, input).await,
            other => {
                // Check the dynamically registered blocks map before giving up.
                let block = {
                    let guard = self.blocks.lock().expect("blocks mutex poisoned");
                    guard.get(other).cloned()
                };
                match block {
                    Some(b) => b.handle(self, msg, input).await,
                    None => OutputStream::error(WaferError::new(
                        ErrorCode::NotFound,
                        format!("block '{other}' not registered in TestContext"),
                    )),
                }
            }
        }
    }

    /// The block identity a test opted into via [`Self::with_wrap`].
    ///
    /// The same field already backs `check_resource_access` and the
    /// `call_block` grant check; publishing it here is what makes handler code
    /// that *reads* its caller — `blocks::storage::ImpresspressStorageBlock`
    /// namespaces every path under `ctx.caller_id()` — behave in a test the
    /// way it does in production. Without this it saw `None`, filed every
    /// object under `unknown/…`, and the per-block isolation the storage
    /// wrapper exists for went untested.
    fn caller_id(&self) -> Option<&str> {
        self.caller_id.as_deref()
    }

    fn is_cancelled(&self) -> bool {
        false
    }

    fn registered_blocks(&self) -> &[wafer_run::BlockInfo] {
        &self.block_infos
    }

    fn config_get(&self, key: &str) -> Option<&str> {
        self.config.get(key).map(String::as_str)
    }

    fn clone_arc(&self) -> Arc<dyn Context> {
        // Cheap — all interior state is `Arc`/`Mutex`-shared.
        Arc::new(self.clone())
    }
}

/// Test double that wraps a [`TestContext`] and turns specific
/// `"wafer-run/database"` service-op calls, scoped to a specific collection
/// (table), into a simulated infra failure — while every other call
/// (including other tables under the same op, and every other database op)
/// passes through to the real in-memory SQLite context untouched.
///
/// Scoping by collection (not just op) matters because many different repo
/// calls share the same wire op kind — e.g. every `update_by_filters` caller
/// sends `"database.update_where"` regardless of which table it targets, so
/// failing the op alone would also break unrelated calls to other tables in
/// the same request flow.
///
/// Used to reproduce "a downstream DB call fails" fault-injection scenarios
/// — e.g. a guard read that must fail closed, or a revocation write whose
/// failure must not be swallowed as success — without needing a fake
/// database backend. Match `(op, collection)` against [`Message::action`]
/// (the wire service-op string, e.g. `"database.get"`,
/// `"database.update_where"` — see `wafer_block::common::ServiceOp`) and the
/// request body's `collection` field.
#[derive(Clone)]
pub struct FailingDbOpContext {
    inner: TestContext,
    failing: Vec<(&'static str, &'static str)>,
    /// The error the matched op answers with. `Internal` (via
    /// [`FailingDbOpContext::new`]) for an infra outage;
    /// [`FailingDbOpContext::failing_with`] picks another code so a test can
    /// reproduce a *caller* error arriving from below — an
    /// `InvalidArgument` a repository guard raised, say — and pin how the
    /// handler above translates it.
    error: WaferError,
    /// How many matching calls to let through before failing (see
    /// [`FailingDbOpContext::after_passing`]). Shared across clones so a
    /// handler's `clone_arc` sees the same countdown.
    passes_before_failing: Arc<std::sync::atomic::AtomicUsize>,
}

/// Request-body shape shared by every `wafer-run/database` wire request:
/// they all carry a `collection` field, and serde ignores the other
/// (irrelevant) fields on decode.
#[derive(serde::Deserialize)]
struct CollectionPeek {
    collection: String,
}

impl FailingDbOpContext {
    /// Wrap `inner`, failing every `"wafer-run/database"` call whose
    /// `(msg.action(), request.collection)` matches an entry in `failing`
    /// with a simulated [`ErrorCode::Internal`] error. All other calls pass
    /// through untouched.
    pub fn new(inner: TestContext, failing: Vec<(&'static str, &'static str)>) -> Self {
        Self::failing_with(
            inner,
            failing,
            WaferError::new(ErrorCode::Internal, "simulated database outage"),
        )
    }

    /// [`Self::new`] with the answered error chosen by the caller, for the
    /// codes that are not an outage — a repository guard's `InvalidArgument`,
    /// a `FailedPrecondition`, and so on. A handler that funnels every
    /// non-`NotFound` error into a 500 discards exactly these.
    pub fn failing_with(
        inner: TestContext,
        failing: Vec<(&'static str, &'static str)>,
        error: WaferError,
    ) -> Self {
        Self {
            inner,
            failing,
            error,
            passes_before_failing: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Let the first `n` matching calls through untouched and fail only from
    /// the `n + 1`th on. A handler whose guarded call is preceded by another
    /// call of the same op on the same table (a lookup, then a check) needs
    /// this to isolate the second one.
    pub fn after_passing(self, n: usize) -> Self {
        self.passes_before_failing
            .store(n, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Consume one allowed pass. `true` when this matching call should still
    /// reach the inner context.
    fn let_one_pass(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.passes_before_failing
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
    }
}

#[async_trait::async_trait]
impl Context for FailingDbOpContext {
    fn check_resource_access(
        &self,
        resource: &str,
        resource_type: wafer_run::ResourceType,
        is_write: bool,
    ) -> Result<(), WaferError> {
        self.inner
            .check_resource_access(resource, resource_type, is_write)
    }

    async fn call_block(&self, name: &str, msg: Message, input: InputStream) -> OutputStream {
        if name == "wafer-run/database" && self.failing.iter().any(|(op, _)| *op == msg.action()) {
            let bytes = input.collect_to_bytes().await;
            let collection = wafer_block::codec::decode::<CollectionPeek>(&bytes)
                .map(|p| p.collection)
                .unwrap_or_default();
            if self
                .failing
                .iter()
                .any(|(op, table)| *op == msg.action() && *table == collection)
                && !self.let_one_pass()
            {
                return OutputStream::error(self.error.clone());
            }
            return self
                .inner
                .call_block(name, msg, InputStream::from_bytes(bytes))
                .await;
        }
        self.inner.call_block(name, msg, input).await
    }

    fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    fn registered_blocks(&self) -> &[wafer_run::BlockInfo] {
        self.inner.registered_blocks()
    }

    fn config_get(&self, key: &str) -> Option<&str> {
        self.inner.config_get(key)
    }

    fn clone_arc(&self) -> Arc<dyn Context> {
        Arc::new(self.clone())
    }
}

impl TestContext {
    /// [`TestContext::with_auth`] plus a real `wafer-run/crypto` block, so
    /// handlers that mint or verify JWTs (login, signup, refresh) run end to
    /// end against a fixed test secret.
    pub async fn with_auth_and_crypto() -> Self {
        let mut ctx = Self::with_auth().await;
        let svc = Arc::new(
            wafer_block_crypto::service::Argon2JwtCryptoService::new(
                "test-jwt-secret-padded-to-min-32-bytes-aaaa".to_string(),
            )
            .expect("test secret is long enough"),
        );
        let crypto_block: Arc<dyn wafer_run::Block> =
            Arc::new(wafer_core::service_blocks::crypto::CryptoBlock::new(svc));
        ctx.register_block("wafer-run/crypto", crypto_block);
        ctx
    }
}

/// Build an anonymous request `Message`. No `auth.user_id` meta set.
pub fn anon_msg(action: &str, path: &str) -> Message {
    let mut m = Message::new("http.request");
    m.set_meta("req.action", action);
    m.set_meta("req.resource", path);
    m
}

/// Build an authenticated request `Message` for `user_id`. No admin role.
pub fn auth_msg(action: &str, path: &str, user_id: &str) -> Message {
    let mut m = anon_msg(action, path);
    m.set_meta("auth.user_id", user_id);
    m
}

/// Build an admin request `Message` (user_id `"admin_1"`, role `admin`).
pub fn admin_msg(action: &str, path: &str) -> Message {
    let mut m = auth_msg(action, path, "admin_1");
    m.set_meta("auth.user_roles", "admin");
    m
}

/// Drain an `OutputStream` to a `BufferedResponse`. Panics if the stream
/// terminates with anything other than `Complete` or `Halt`.
///
/// `Halt` is a legitimate success-shaped terminal (used e.g. by CORS
/// preflight to short-circuit with a 204 + headers), so tests treat it
/// the same as `Complete` — the body+meta are returned for assertion.
///
/// Tests should not see errors from handlers under test unless they're
/// explicitly asserting on error paths — use `output_is_error` for that.
pub async fn collect_or_panic(out: OutputStream) -> BufferedResponse {
    match out.collect_buffered().await {
        Ok(buf) => buf,
        Err(TerminalNotResponse::Halt(buf)) => buf,
        Err(TerminalNotResponse::Error(e)) => {
            panic!("handler returned error: {} ({:?})", e.message, e.code)
        }
        Err(TerminalNotResponse::Drop) => panic!("handler dropped the request"),
        Err(TerminalNotResponse::Continue(_)) => panic!("handler returned Continue"),
        Err(TerminalNotResponse::Malformed) => panic!("handler returned malformed stream"),
    }
}

/// Read the HTTP status from an `OutputStream`. Defaults to 200 if the
/// handler didn't set a `resp.status` meta entry.
pub async fn output_status(out: OutputStream) -> u16 {
    let buf = collect_or_panic(out).await;
    buf.meta
        .iter()
        .find(|m| m.key == "resp.status")
        .and_then(|m| m.value.parse::<u16>().ok())
        .unwrap_or(200)
}

/// The HTTP status an adapter would send for `out`, **including** for the
/// error terminals the router and the `err_*` helpers produce.
///
/// [`output_status`] deliberately panics on an error terminal, because a
/// handler under test erroring is normally a bug. A test asserting the
/// router's access gate is the opposite case: a 403 there arrives as
/// `TerminalNotResponse::Error(PermissionDenied)`, never as `resp.status`
/// meta. The mapping is `wafer_block::http_codec`'s, the same one the real
/// adapters use, so this reports the status the caller would actually see.
pub async fn output_http_status(out: OutputStream) -> u16 {
    match out.collect_buffered().await {
        Ok(buf) => wafer_block::http_codec::resolve_status(&buf.meta, 200),
        Err(TerminalNotResponse::Halt(buf)) => {
            wafer_block::http_codec::resolve_status(&buf.meta, 200)
        }
        Err(TerminalNotResponse::Error(e)) => wafer_block::http_codec::resolve_error_status(&e),
        Err(other) => panic!("unexpected terminal: {other:?}"),
    }
}

/// Read a named response header (e.g. `"Location"` for redirects).
/// The lookup is case-sensitive — pass the exact name handlers used in
/// `set_header(name, _)`.
pub async fn output_header(out: OutputStream, name: &str) -> Option<String> {
    let key = format!("resp.header.{name}");
    let buf = collect_or_panic(out).await;
    buf.meta
        .iter()
        .find(|m| m.key == key)
        .map(|m| m.value.clone())
}

/// A named response header as the HTTP boundary would send it, **including**
/// for error terminals.
///
/// The error-terminal sibling of [`output_header`], for the same reason
/// [`output_http_status`] is [`output_status`]'s: a refusal carries its
/// headers in `WaferError::meta`, which `collect_buffered` surfaces as an
/// `Err` rather than a `BufferedResponse`. Reads through
/// `wafer_block::http_codec`, so the answer is the header the caller actually
/// receives. Header names are matched case-insensitively, as HTTP does.
pub async fn output_http_header(out: OutputStream, name: &str) -> Option<String> {
    let headers = wafer_block::http_codec::collect_http_response(out)
        .await
        .headers;
    headers
        .into_iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value)
}

/// Read the body as a UTF-8 string. Panics if the body is not valid UTF-8.
pub async fn output_html(out: OutputStream) -> String {
    let buf = collect_or_panic(out).await;
    String::from_utf8(buf.body).expect("body was not valid UTF-8")
}

/// Read the body as raw bytes.
pub async fn output_body(out: OutputStream) -> Vec<u8> {
    collect_or_panic(out).await.body
}

/// Read the body as JSON. Returns `Value::Null` if the body fails to parse.
pub async fn output_json(out: OutputStream) -> serde_json::Value {
    let buf = collect_or_panic(out).await;
    serde_json::from_slice(&buf.body).unwrap_or(serde_json::Value::Null)
}

/// True if the OutputStream terminated with an error matching `code`.
/// The code string should match the ErrorCode debug format (e.g., "NotFound", "Internal").
pub async fn output_is_error(out: OutputStream, code: &str) -> bool {
    matches!(
        out.collect_buffered().await,
        Err(TerminalNotResponse::Error(e)) if format!("{:?}", e.code) == code
    )
}

/// `BlockInfo` for every Worker-shipping block, fetched from the real block
/// structs (not hand-rolled fixtures) so `discovery_json`/`openapi_document`
/// exercise the actual declarations shipped in `blocks/*/mod.rs`.
///
/// This is the block list that backs the generated `/openapi.json` document
/// in tests. A block absent from this list never appears in the document at
/// all — regardless of how correct its own schema declarations are — so the
/// per-block openapi snapshot gate (`tests/openapi_snapshot.rs`) depends on
/// this staying in sync with every block that carries schema-bearing
/// endpoints (or is expected to soon).
#[cfg(all(
    feature = "block-files",
    feature = "block-messages",
    feature = "block-products",
    feature = "block-tickets",
    feature = "block-llm",
    feature = "block-vector"
))]
pub fn real_block_infos() -> Vec<BlockInfo> {
    #[allow(unused_mut)]
    let mut infos = vec![
        crate::blocks::auth_ui::AuthUiBlock::new().info(),
        crate::blocks::files::FilesBlock::new().info(),
        crate::blocks::products::ProductsBlock::new().info(),
        crate::blocks::admin::AdminBlock::new().info(),
        crate::blocks::messages::MessagesBlock::new().info(),
        crate::blocks::tickets::TicketsBlock::new().info(),
        // `info()` is declarative; the provider-admin handle it is built
        // with never runs here, so the no-op one suffices (same as
        // `blocks::feature_block_infos`).
        crate::blocks::llm::LlmBlock::new(Arc::new(
            crate::blocks::llm::provider_admin::NoopProviderAdmin,
        ))
        .info(),
        crate::blocks::vector::VectorBlock::new().info(),
    ];

    // The dev sandbox ships only under its own (non-default) feature, so its
    // `BlockInfo` joins the document only when the block is compiled in. Like
    // `llm` above, `info()` is declarative — neither the `RuntimeControl` nor
    // the `ShellSource` handle it is built with is ever called here, so the
    // test doubles suffice.
    #[cfg(feature = "block-dev")]
    infos.push(
        crate::blocks::dev::DevBlock::with_workspace(crate::blocks::dev::DevShared::new(
            crate::blocks::dev::test_support::FakeControl::new(),
            Arc::new(crate::blocks::dev::test_support::FakeShell::new()),
        ))
        .info(),
    );

    infos
}

/// The JWT secret every `handle_request` test call passes.
pub const TEST_JWT_SECRET: &str = "test-jwt-secret";

/// A `Bearer` access token carrying `roles`, signed the way `auth_ui` signs
/// one (block-derived key from [`TEST_JWT_SECRET`], the default issuer), so
/// `pipeline::handle_request`'s step 2 resolves it to a real identity with
/// those roles. This is how a test asks for a document *as* an authenticated
/// or admin caller.
pub fn bearer_for_roles(roles: &[&str]) -> String {
    use std::{collections::HashMap, time::Duration};

    use wafer_block_crypto::primitives;

    let derived = primitives::derive_block_key(
        TEST_JWT_SECRET.as_bytes(),
        crate::blocks::auth_ui::AUTH_UI_BLOCK_ID,
    );
    let mut claims = HashMap::new();
    claims.insert("sub".to_string(), serde_json::json!("user-test-1"));
    claims.insert("type".to_string(), serde_json::json!("access"));
    // Must match `expected_issuer`'s default
    // (`crate::blocks::auth::helpers::expected_issuer`): a `TestContext` has
    // no `WAFER_RUN_SHARED__FRONTEND_URL` configured.
    claims.insert(
        "iss".to_string(),
        serde_json::json!("http://localhost:5173"),
    );
    claims.insert("roles".to_string(), serde_json::json!(roles));
    let token = primitives::jwt_sign(claims, Duration::from_secs(3600), derived.as_bytes())
        .expect("test jwt_sign");
    format!("Bearer {token}")
}

/// Fetch a discovery document (`/openapi.json` or `/.well-known/agent.json`)
/// generated from [`real_block_infos`], as the caller `roles` describes:
/// `None` is anonymous, `Some(&["user"])` an authenticated user,
/// `Some(&["admin"])` an admin. The documents are filtered by the caller's
/// tier the same way the WebMCP manifest is, so which caller asks matters.
///
/// The identity is pre-resolved on the message (the same `auth.*` meta
/// [`admin_msg`] sets), not minted as a JWT, so this works on a bare
/// [`TestContext::new`] — no auth tables needed. What these tests check is
/// the filter given a caller; that step 2 resolves a real bearer into that
/// caller *before* the filter runs is pinned separately, through
/// [`bearer_for_roles`] on a [`TestContext::with_auth`]
/// (`pipeline::discovery_tests::openapi_describes_admin_endpoints_to_an_admin`).
#[cfg(all(
    feature = "block-files",
    feature = "block-messages",
    feature = "block-products",
    feature = "block-tickets",
    feature = "block-llm",
    feature = "block-vector"
))]
pub async fn discovery_json_as(
    ctx: &TestContext,
    path: &str,
    host: &str,
    roles: Option<&[&str]>,
) -> serde_json::Value {
    let mut msg = match roles {
        None => anon_msg("retrieve", path),
        Some(roles) => {
            let mut msg = auth_msg("retrieve", path, "user-test-1");
            msg.set_meta("auth.user_roles", roles.join(","));
            msg
        }
    };
    msg.set_meta("http.header.host", host);
    let out = crate::pipeline::handle_request(
        ctx,
        msg,
        InputStream::from_bytes(Vec::new()),
        None,
        TEST_JWT_SECRET,
        false,
        &crate::features::AllEnabled,
        &real_block_infos(),
        &[],
    )
    .await;
    let buf = collect_or_panic(out).await;
    serde_json::from_slice(&buf.body).expect("discovery response is valid JSON")
}

/// The *complete* discovery document — fetched as an admin, the one caller
/// who sees every endpoint. Shared by `pipeline.rs`'s discovery tests and the
/// per-block openapi snapshot gate, both of which assert on privileged
/// endpoints; a test about what a lower tier receives uses
/// [`discovery_json_as`] directly.
#[cfg(all(
    feature = "block-files",
    feature = "block-messages",
    feature = "block-products",
    feature = "block-tickets",
    feature = "block-llm",
    feature = "block-vector"
))]
pub async fn discovery_json(ctx: &TestContext, path: &str, host: &str) -> serde_json::Value {
    discovery_json_as(ctx, path, host, Some(&["admin"])).await
}

/// Fetch the generated `/openapi.json` document. Shared by pipeline tests
/// and the per-block snapshot gate.
#[cfg(feature = "test-support")]
#[cfg(all(
    feature = "block-files",
    feature = "block-messages",
    feature = "block-products",
    feature = "block-tickets",
    feature = "block-llm",
    feature = "block-vector"
))]
pub async fn openapi_document(ctx: &TestContext) -> serde_json::Value {
    discovery_json(ctx, "/openapi.json", "impresspress.example.com").await
}

// ---------------------------------------------------------------------------
// In-memory storage backend
// ---------------------------------------------------------------------------

/// One object held by [`InMemoryStorageService`].
struct StoredObject {
    data: Vec<u8>,
    content_type: String,
    last_modified: chrono::DateTime<chrono::Utc>,
}

/// In-memory [`StorageService`](wafer_core::interfaces::storage::service::StorageService)
/// for fixtures that need a working object store.
///
/// The counterpart of this module's in-memory SQLite: a *real* backend with
/// real semantics (a `get` of an absent key is `NotFound`, a `list` honours
/// prefix/offset/limit, a `put` overwrites), not a stub that answers `Ok` to
/// everything. Tests that assert content-addressed storage — same bytes
/// stored once, a stale read failing — only mean something against a backend
/// that can actually say no.
///
/// Registered under `wafer-run/storage` behind
/// [`crate::blocks::storage::ImpresspressStorageBlock`], so a fixture also
/// exercises the per-block namespacing (`{caller}/{folder}/{key}`) and the
/// cross-block grant checks that wrapper adds.
#[derive(Default)]
pub struct InMemoryStorageService {
    /// Objects keyed by `(folder, key)`. `BTreeMap` so `list` is ordered
    /// without a sort, matching the lexicographic order object stores return.
    objects: Mutex<std::collections::BTreeMap<(String, String), StoredObject>>,
    /// Folder name → `(public, created_at)`. A `put` auto-creates the folder,
    /// as a filesystem backend's `create_dir_all` and S3's implicit prefixes
    /// both do.
    folders: Mutex<std::collections::BTreeMap<String, (bool, chrono::DateTime<chrono::Utc>)>>,
    /// Set by [`Self::fail_next_put`]: the message the next `put` refuses
    /// with, consumed by that call.
    ///
    /// A publish that fails *after* the runtime has been swapped is the one
    /// path the sandbox has to unwind (design §7.3), and nothing else in a
    /// fixture can produce it: every other failure is refused before anything
    /// has been changed.
    fail_next_put: Mutex<Option<String>>,
    /// Every mutating operation in the order it arrived, as
    /// `"{op} {folder}/{key}"`.
    ///
    /// Content-addressed stores make *what* was written easy to assert and
    /// *when* impossible: two publishes of the same bytes are indistinguishable
    /// in the final state. The site publisher's contract is an ordering one
    /// (`index.html` after everything it references), so the order has to be
    /// recorded as it happens.
    ops: Mutex<Vec<String>>,
}

impl InMemoryStorageService {
    /// A store with no folders and no objects.
    pub fn new() -> Self {
        Self::default()
    }

    /// Make the next `put` refuse with `message`, storing nothing.
    pub fn fail_next_put(&self, message: &str) {
        *self.fail_next_put.lock().expect("fail_next_put mutex") = Some(message.to_string());
    }

    /// Every mutating operation this store has seen, oldest first.
    pub fn ops(&self) -> Vec<String> {
        self.ops.lock().expect("ops mutex poisoned").clone()
    }

    /// Record one mutating operation.
    fn record(&self, op: &str, folder: &str, key: &str) {
        self.ops
            .lock()
            .expect("ops mutex poisoned")
            .push(format!("{op} {folder}/{key}"));
    }
}

#[wafer_block::wafer_async_trait]
impl wafer_core::interfaces::storage::service::StorageService for InMemoryStorageService {
    async fn put(
        &self,
        folder: &str,
        key: &str,
        data: &[u8],
        content_type: &str,
    ) -> Result<(), wafer_core::interfaces::storage::service::StorageError> {
        let now = chrono::Utc::now();
        self.record("put", folder, key);
        if let Some(message) = self
            .fail_next_put
            .lock()
            .expect("fail_next_put mutex")
            .take()
        {
            return Err(wafer_core::interfaces::storage::service::StorageError::Internal(message));
        }
        self.folders
            .lock()
            .expect("folders mutex poisoned")
            .entry(folder.to_string())
            .or_insert((false, now));
        self.objects.lock().expect("objects mutex poisoned").insert(
            (folder.to_string(), key.to_string()),
            StoredObject {
                data: data.to_vec(),
                content_type: content_type.to_string(),
                last_modified: now,
            },
        );
        Ok(())
    }

    async fn get(
        &self,
        folder: &str,
        key: &str,
    ) -> Result<
        (
            Vec<u8>,
            wafer_core::interfaces::storage::service::ObjectInfo,
        ),
        wafer_core::interfaces::storage::service::StorageError,
    > {
        let guard = self.objects.lock().expect("objects mutex poisoned");
        let object = guard
            .get(&(folder.to_string(), key.to_string()))
            .ok_or(wafer_core::interfaces::storage::service::StorageError::NotFound)?;
        Ok((
            object.data.clone(),
            wafer_core::interfaces::storage::service::ObjectInfo {
                key: key.to_string(),
                size: object.data.len() as i64,
                content_type: object.content_type.clone(),
                last_modified: object.last_modified,
            },
        ))
    }

    async fn delete(
        &self,
        folder: &str,
        key: &str,
    ) -> Result<(), wafer_core::interfaces::storage::service::StorageError> {
        self.record("delete", folder, key);
        self.objects
            .lock()
            .expect("objects mutex poisoned")
            .remove(&(folder.to_string(), key.to_string()))
            .map(|_| ())
            .ok_or(wafer_core::interfaces::storage::service::StorageError::NotFound)
    }

    async fn list(
        &self,
        folder: &str,
        opts: &wafer_core::interfaces::storage::service::ListOptions,
    ) -> Result<
        wafer_core::interfaces::storage::service::ObjectList,
        wafer_core::interfaces::storage::service::StorageError,
    > {
        let guard = self.objects.lock().expect("objects mutex poisoned");
        let matched: Vec<wafer_core::interfaces::storage::service::ObjectInfo> = guard
            .iter()
            .filter(|((f, k), _)| f == folder && k.starts_with(&opts.prefix))
            .map(
                |((_, k), object)| wafer_core::interfaces::storage::service::ObjectInfo {
                    key: k.clone(),
                    size: object.data.len() as i64,
                    content_type: object.content_type.clone(),
                    last_modified: object.last_modified,
                },
            )
            .collect();
        let total_count = matched.len() as i64;
        let skipped = matched.into_iter().skip(opts.offset.max(0) as usize);
        let objects = if opts.limit > 0 {
            skipped.take(opts.limit as usize).collect()
        } else {
            skipped.collect()
        };
        Ok(wafer_core::interfaces::storage::service::ObjectList {
            objects,
            total_count,
            // No cursor support: the backend is offset-only, which
            // `ObjectList::next_cursor` documents as the `None` case.
            next_cursor: None,
        })
    }

    async fn create_folder(
        &self,
        name: &str,
        public: bool,
    ) -> Result<(), wafer_core::interfaces::storage::service::StorageError> {
        self.folders
            .lock()
            .expect("folders mutex poisoned")
            .insert(name.to_string(), (public, chrono::Utc::now()));
        Ok(())
    }

    async fn delete_folder(
        &self,
        name: &str,
    ) -> Result<(), wafer_core::interfaces::storage::service::StorageError> {
        self.folders
            .lock()
            .expect("folders mutex poisoned")
            .remove(name);
        self.objects
            .lock()
            .expect("objects mutex poisoned")
            .retain(|(folder, _), _| folder != name);
        Ok(())
    }

    async fn list_folders(
        &self,
    ) -> Result<
        Vec<wafer_core::interfaces::storage::service::FolderInfo>,
        wafer_core::interfaces::storage::service::StorageError,
    > {
        Ok(self
            .folders
            .lock()
            .expect("folders mutex poisoned")
            .iter()
            .map(|(name, (public, created_at))| {
                wafer_core::interfaces::storage::service::FolderInfo {
                    name: name.clone(),
                    public: *public,
                    created_at: *created_at,
                }
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Log capture
// ---------------------------------------------------------------------------

/// Minimal [`tracing::Subscriber`] that records the rendered `message` field
/// of every event it sees, so a test can assert on what was (or was not)
/// logged without pulling in `tracing-subscriber`.
///
/// Shared, because more than one route has had to prove it does NOT log:
/// refusal diagnostics that are static across calls belong at runtime
/// construction, not on a path a caller can loop (`pipeline`'s
/// `/b/webmcp/manifest.json`, `blocks::dev::tools`' `/b/dev/api/tools.json`).
/// Install it with `tracing::subscriber::set_default`, which is scoped to the
/// current thread — so a `#[tokio::test]` on the multi-thread runtime would
/// miss events from work that migrated to another worker. Every use so far
/// runs the awaited call on the test's own thread.
#[derive(Clone, Default)]
pub struct MessageCapture(Arc<Mutex<Vec<String>>>);

struct MessageVisitor<'a> {
    out: &'a mut String,
}

impl tracing::field::Visit for MessageVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            *self.out = format!("{value:?}");
        }
    }
}

impl tracing::Subscriber for MessageCapture {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut message = String::new();
        event.record(&mut MessageVisitor { out: &mut message });
        self.0
            .lock()
            .expect("MessageCapture mutex poisoned")
            .push(message);
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

impl MessageCapture {
    /// How many captured messages contain `needle`.
    pub fn count_containing(&self, needle: &str) -> usize {
        self.0
            .lock()
            .expect("MessageCapture mutex poisoned")
            .iter()
            .filter(|m| m.contains(needle))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use wafer_block::db::ListOptions;
    use wafer_core::clients::database as db;

    use super::*;

    #[tokio::test]
    async fn database_create_and_get_round_trip() {
        let ctx = TestContext::new().await;

        db::exec_raw(
            &ctx,
            "CREATE TABLE round_trip (id TEXT PRIMARY KEY, name TEXT)",
            &[],
        )
        .await
        .expect("create table");

        db::exec_raw(
            &ctx,
            "INSERT INTO round_trip (id, name) VALUES (?, ?)",
            &[serde_json::json!("r1"), serde_json::json!("alpha")],
        )
        .await
        .expect("insert row");

        let rows = db::query_raw(
            &ctx,
            "SELECT id, name FROM round_trip WHERE id = ?",
            &[serde_json::json!("r1")],
        )
        .await
        .expect("select");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "r1");
        assert_eq!(
            rows[0].data.get("name").and_then(|v| v.as_str()),
            Some("alpha")
        );
    }

    #[test]
    fn anon_msg_sets_action_and_path_with_no_user_id() {
        let m = anon_msg("retrieve", "/b/auth/login");
        assert_eq!(m.action(), "retrieve");
        assert_eq!(m.path(), "/b/auth/login");
        assert_eq!(m.user_id(), "");
    }

    #[test]
    fn auth_msg_sets_user_id() {
        let m = auth_msg("retrieve", "/b/userportal/", "user-a");
        assert_eq!(m.action(), "retrieve");
        assert_eq!(m.path(), "/b/userportal/");
        assert_eq!(m.user_id(), "user-a");
    }

    #[test]
    fn admin_msg_marks_admin_role() {
        use crate::util::is_admin;
        let m = admin_msg("retrieve", "/b/admin/users");
        assert_eq!(m.user_id(), "admin_1");
        assert!(is_admin(&m));
    }

    #[tokio::test]
    async fn output_status_reads_status_meta() {
        use crate::http::ResponseBuilder;
        let out = ResponseBuilder::new()
            .status(302)
            .body(Vec::new(), "text/plain");
        assert_eq!(output_status(out).await, 302);
    }

    #[tokio::test]
    async fn output_status_defaults_to_200_when_unset() {
        use crate::http::ResponseBuilder;
        let out = ResponseBuilder::new().body(Vec::new(), "text/plain");
        assert_eq!(output_status(out).await, 200);
    }

    #[tokio::test]
    async fn output_header_reads_named_header() {
        use crate::http::ResponseBuilder;
        let out = ResponseBuilder::new()
            .status(302)
            .set_header("Location", "/dashboard")
            .body(Vec::new(), "text/plain");
        assert_eq!(
            output_header(out, "Location").await.as_deref(),
            Some("/dashboard")
        );
    }

    #[tokio::test]
    async fn output_html_reads_body_as_utf8() {
        use crate::http::ResponseBuilder;
        let out = ResponseBuilder::new()
            .status(200)
            .body(b"<h1>hi</h1>".to_vec(), "text/html");
        assert_eq!(output_html(out).await, "<h1>hi</h1>");
    }

    #[tokio::test]
    async fn output_json_parses_body() {
        use crate::http::ResponseBuilder;
        let out = ResponseBuilder::new()
            .status(200)
            .body(br#"{"ok":true}"#.to_vec(), "application/json");
        assert_eq!(output_json(out).await, serde_json::json!({"ok": true}));
    }

    #[tokio::test]
    async fn with_auth_applies_orgs_and_users_tables() {
        let ctx = TestContext::with_auth().await;
        // Verify auth tables exist by inserting a user, then an org, then selecting.
        ctx.seed_auth_user("user-a").await;

        db::exec_raw(
            &ctx,
            "INSERT INTO wafer_run__auth__orgs (id, name, owner_user_id, is_reserved, created_at) \
             VALUES (?, ?, ?, 0, ?)",
            &[
                serde_json::json!("org-1"),
                serde_json::json!("acme"),
                serde_json::json!("user-a"),
                serde_json::json!("2026-01-01T00:00:00Z"),
            ],
        )
        .await
        .expect("insert org");

        let rows = db::query_raw(
            &ctx,
            "SELECT name FROM wafer_run__auth__orgs WHERE id = ?",
            &[serde_json::json!("org-1")],
        )
        .await
        .expect("select org");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].data.get("name").and_then(|v| v.as_str()),
            Some("acme")
        );
    }

    #[tokio::test]
    async fn registered_block_is_dispatched_through_call_block() {
        use async_trait::async_trait;
        use wafer_run::{Block as RunBlock, BlockCategory, BlockInfo, LifecycleEvent};

        struct EchoBlock;

        #[async_trait]
        impl RunBlock for EchoBlock {
            fn info(&self) -> BlockInfo {
                BlockInfo::new("test/echo", "0.0.1", "echo@v1", "echoes the request path")
                    .category(BlockCategory::Service)
            }

            async fn handle(
                &self,
                _ctx: &dyn Context,
                msg: Message,
                _input: InputStream,
            ) -> OutputStream {
                crate::http::ResponseBuilder::new()
                    .status(200)
                    .body(msg.path().as_bytes().to_vec(), "text/plain")
            }

            async fn lifecycle(
                &self,
                _ctx: &dyn Context,
                _e: LifecycleEvent,
            ) -> Result<(), WaferError> {
                Ok(())
            }
        }

        let mut ctx = TestContext::new().await;
        ctx.register_block("test/echo", Arc::new(EchoBlock));

        let msg = anon_msg("retrieve", "/echo-me");
        let resp = ctx.call_block("test/echo", msg, InputStream::empty()).await;
        let body = output_html(resp).await;
        assert_eq!(body, "/echo-me");
    }

    #[tokio::test]
    async fn with_wrap_denies_unowned_resource_without_grant() {
        // Caller "block-x" tries to read auth-owned table; no grants → denied.
        let ctx = TestContext::with_auth().await.with_wrap(
            "test/block-x",
            Vec::new(),
            "impresspress/admin",
        );

        let result = db::list(&ctx, "wafer_run__auth__users", &ListOptions::default()).await;

        let err = result.expect_err("WRAP must deny call without grant");
        assert!(
            err.to_string().contains("WRAP"),
            "error must mention WRAP, got: {err}"
        );
    }

    /// Every entry in the filtered-write family, so a new one added to the
    /// `DatabaseService` trait shows up here as a compile error rather than
    /// as a silently inherited default.
    async fn filtered_writes_all_fail(
        svc: &dyn wafer_core::interfaces::database::service::DatabaseService,
        collection: &str,
        filters: &[wafer_block::db::Filter],
        label: &str,
    ) {
        let data = HashMap::from([(
            "name".to_string(),
            serde_json::json!("written despite the fault"),
        )]);
        assert!(
            svc.update_where(collection, filters, data.clone())
                .await
                .is_err(),
            "update_where must fail ({label})"
        );
        assert!(
            svc.update_where_count(collection, filters, data)
                .await
                .is_err(),
            "update_where_count must fail ({label})"
        );
        assert!(
            svc.delete_where(collection, filters).await.is_err(),
            "delete_where must fail ({label})"
        );
        assert!(
            svc.delete_where_count(collection, filters).await.is_err(),
            "delete_where_count must fail ({label})"
        );
        assert!(
            svc.take_where(collection, filters).await.is_err(),
            "take_where must fail ({label})"
        );
        assert!(
            svc.increment_field_where(collection, "hits", 1, filters)
                .await
                .is_err(),
            "increment_field_where must fail ({label})"
        );
    }

    /// `id = ?` for a row that does not exist — the case that separates a
    /// correct double from one riding the trait defaults.
    fn matches_nothing() -> Vec<wafer_block::db::Filter> {
        vec![wafer_block::db::Filter {
            field: "id".to_string(),
            operator: wafer_block::db::FilterOp::Equal,
            value: serde_json::json!("no-such-row"),
        }]
    }

    fn matches_the_seeded_row() -> Vec<wafer_block::db::Filter> {
        vec![wafer_block::db::Filter {
            field: "id".to_string(),
            operator: wafer_block::db::FilterOp::Equal,
            value: serde_json::json!("r1"),
        }]
    }

    async fn seeded_ctx() -> TestContext {
        let ctx = TestContext::new().await;
        db::exec_raw(
            &ctx,
            "CREATE TABLE filtered_writes (id TEXT PRIMARY KEY, name TEXT, hits INTEGER              DEFAULT 0, created_at TEXT, updated_at TEXT)",
            &[],
        )
        .await
        .expect("create table");
        db::exec_raw(
            &ctx,
            "INSERT INTO filtered_writes (id, name, hits) VALUES ('r1', 'alpha', 0)",
            &[],
        )
        .await
        .expect("seed row");
        ctx
    }

    /// [`TestContext::break_writes`] simulates a backend whose writes fail.
    /// The filtered-write family has to fail there too — and it is the family
    /// the `DatabaseService` trait ships READ-BASED defaults for:
    /// `update_where_count` counts and then updates, `update_where` lists and
    /// then updates by id, `delete_where` lists and then deletes by id,
    /// `take_where` lists and then deletes, and `increment_field_where`
    /// reports "not implemented by this database backend".
    ///
    /// `wafer-block-sqlite`, `wafer-block-postgres` and `D1DatabaseService`
    /// every one override the family with a SINGLE statement carrying no
    /// `count` and no `list`, so a double that inherits the defaults is a
    /// database that does not exist. The tell is a write matching zero rows:
    /// the defaults list nothing, update nothing, and return `Ok` — a
    /// *successful* write on a backend whose writes are supposed to be
    /// failing. A handler branch reachable only when a filtered write fails
    /// then looks covered while production can never enter it, which is
    /// exactly how `restore_fails_loudly_when_the_slug_collision_probe_
    /// cannot_run` came to assert an outcome production could not produce
    /// (see its doc comment; that instance was fixed on `FailingReadsDb`,
    /// leaving this one armed for the next test to use it).
    #[tokio::test]
    async fn break_writes_fails_every_filtered_write() {
        let ctx = seeded_ctx().await.break_writes();

        // The zero-match case first: this is the one the trait defaults get
        // wrong, by doing nothing and calling it success.
        filtered_writes_all_fail(
            ctx.db_service.as_ref(),
            "filtered_writes",
            &matches_nothing(),
            "no rows matched",
        )
        .await;
        // And the matching case, which the defaults happen to fail — but
        // only after a read the real backends never issue.
        filtered_writes_all_fail(
            ctx.db_service.as_ref(),
            "filtered_writes",
            &matches_the_seeded_row(),
            "one row matched",
        )
        .await;

        // Reads still delegate, which is the whole point of `break_writes`:
        // a handler's "read current state, then persist a change" shape must
        // reach the branch under test rather than failing earlier.
        let row = ctx
            .db_service
            .get("filtered_writes", "r1")
            .await
            .expect("reads must still resolve under break_writes");
        assert_eq!(
            row.data.get("name").and_then(|v| v.as_str()),
            Some("alpha"),
            "a failed filtered write must not have changed the row"
        );
    }

    /// The mirror of the above for [`TestContext::break_reads`]: its
    /// filtered-write overrides already delegate (a broken read layer must
    /// not fail a write), and `take_where` — which returns the rows it
    /// removed, so it reads as much as it writes — fails.
    ///
    /// `take_where` used to reach that failure through the trait's default
    /// (list, then delete by id), so the double only failed because the
    /// *list* did. Real backends issue one `DELETE … RETURNING *`, so the
    /// answer was right for a reason production does not have. It is stated
    /// directly now, like the rest of the family.
    #[tokio::test]
    async fn break_reads_leaves_filtered_writes_working_and_fails_take_where() {
        let ctx = seeded_ctx().await.break_reads();
        let filters = matches_the_seeded_row();
        let data = HashMap::from([("name".to_string(), serde_json::json!("beta"))]);

        assert_eq!(
            ctx.db_service
                .update_where_count("filtered_writes", &filters, data)
                .await
                .expect("a broken read layer must not fail a filtered write"),
            1,
        );
        assert!(
            ctx.db_service
                .take_where("filtered_writes", &filters)
                .await
                .is_err(),
            "take_where hands back the rows it removed, so a broken read layer must fail it"
        );
        assert_eq!(
            ctx.db_service
                .delete_where_count("filtered_writes", &filters)
                .await
                .expect("a broken read layer must not fail a filtered delete"),
            1,
        );
    }

    #[tokio::test]
    async fn with_wrap_allows_call_when_grant_matches() {
        let grants = vec![ResourceGrant::read(
            "test/block-x",
            "wafer_run__auth__users",
        )];
        let ctx =
            TestContext::with_auth()
                .await
                .with_wrap("test/block-x", grants, "impresspress/admin");

        // Empty users table — listing must succeed (zero rows is success).
        let res = db::list(&ctx, "wafer_run__auth__users", &ListOptions::default())
            .await
            .expect("WRAP must allow listing with matching grant");
        assert_eq!(res.records.len(), 0);
    }

    #[tokio::test]
    async fn without_with_wrap_grants_are_unchecked() {
        // Default TestContext (no `with_wrap`) keeps WRAP-bypassing legacy
        // behaviour so existing tests aren't disturbed.
        let ctx = TestContext::with_auth().await;
        let res = db::list(&ctx, "wafer_run__auth__users", &ListOptions::default())
            .await
            .expect("call must succeed without with_wrap");
        assert_eq!(res.records.len(), 0);
    }
}
