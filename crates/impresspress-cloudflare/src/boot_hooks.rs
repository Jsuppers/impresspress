//! The three `BootHooks` impls the Cloudflare boot funnels pick between.
//!
//! `seed_after_admin_init` is the one place a target may write platform state
//! after admin's migration has created the tables and before any other block
//! initializes. Cloudflare answers it three different ways, and the difference
//! is load-bearing rather than incidental: the deploy funnel seeds, the two
//! dynamic request builds read and republish without writing, and a runtime
//! hydrated from a verified prepared plan does nothing at all. Each type
//! documents its own reason; `runtime_build`'s funnels are what bind a path to
//! one of them.

use std::sync::Arc;

use impresspress_core::builder::RuntimeConfig;
use wafer_core::interfaces::{config::service::ConfigService, database::service::DatabaseService};

#[cfg(test)]
use crate::{kv_cached_db, request_services};

/// [`BootHooks`](impresspress_core::builder::BootHooks) impl for the
/// `/_deploy/init` funnel's dynamically built runtime — the one Cloudflare
/// path that **seeds**.
///
/// Every build loads block_settings read-only before `build()` (the router
/// needs its enablement map up front); this hook additionally seeds structural
/// defaults after admin migration has created the table, then publishes the
/// resulting settings onto both config surfaces before any other block
/// initializes. The shared auto-generated-secret pass also belongs after admin
/// migration 002 has added the `variables.block` column.
///
/// Only the deploy funnel gets this. See [`CfRequestBootHooks`] for what the
/// request paths get instead and why the difference is load-bearing.
pub(crate) struct CfDeployBootHooks {
    pub(crate) db: Arc<dyn DatabaseService>,
    pub(crate) block_settings_handle:
        Arc<std::sync::RwLock<impresspress_core::features::BlockSettings>>,
    /// The request-scoped forwarder, so the seeded settings are published onto
    /// the async surface as well as the snapshot. Cloudflare's concrete
    /// `ConfigService` is map-backed and its `set` is a documented no-op, so
    /// only the snapshot actually moves — that is a property of the service,
    /// not a reason to write one surface and skip the other.
    pub(crate) config: Arc<dyn ConfigService>,
    /// The `(block_name, default_enabled)` set this funnel seeds towards, held
    /// as data rather than called for inline. [`CfRequestBootHooks`] has no
    /// such field and that is the point: the seeding path is the one that has
    /// something to seed towards, so re-introducing a request-path seed means
    /// visibly giving the request hook a defaults set, not quietly swapping
    /// one function call for another.
    pub(crate) seed_defaults: Vec<(String, bool)>,
}

impl CfDeployBootHooks {
    /// The database half of [`Self::seed_after_admin_init`]: the auto-generated
    /// secret pass and the hash-gated structural seed, both of which write.
    /// Split out because `seed_after_admin_init` needs a `&mut Wafer` that no
    /// wasm unit test can produce, and what the tests are about is the writes.
    async fn seed_and_load(&self) -> Result<impresspress_core::features::BlockSettings, String> {
        impresspress_core::platform_state::variables::seed_auto_generated(&self.db).await;

        impresspress_core::platform_state::block_settings::load_and_seed(
            &self.db,
            &self.seed_defaults,
        )
        .await
        .map_err(|e| format!("seed block_settings after admin init: {e}"))
    }
}

#[wafer_block::wafer_async_trait]
impl impresspress_core::builder::BootHooks for CfDeployBootHooks {
    async fn seed_after_admin_init(&self, wafer: &mut wafer_run::Wafer) -> Result<(), String> {
        let block_settings = self.seed_and_load().await?;
        publish_block_settings(
            wafer,
            &self.config,
            &self.block_settings_handle,
            block_settings,
        );
        Ok(())
    }
}

/// [`BootHooks`](impresspress_core::builder::BootHooks) impl for the two
/// **dynamically built request-path** runtimes: read `block_settings`,
/// republish it, write nothing.
///
/// # Why it re-reads at all
///
/// `build_runtime` reads `block_settings` before `build()`, but that read
/// happens before `init_block(admin)`, and on a database that has never seen
/// `/_deploy/init` admin's `Init` is a *fresh install* — `apply_if_blessed`
/// bootstraps a fresh install without operator consent, so the table can come
/// into existence, and gain rows, between the pre-build read and this hook.
/// Re-reading here and republishing onto both surfaces is what makes the
/// enablement map every later block's `Init` consults the post-admin-init one.
/// On a settled deployment it returns exactly what `build_runtime` installed,
/// at the cost of one KV-cached list.
///
/// # Why it must not seed
///
/// It is physically write-free, which is an invariant of this path and not a
/// missing feature (`platform_state::block_settings::load` states the same
/// thing at its own definition). Seeding here — as ruling 5.5 originally asked
/// — has three failure modes the deploy funnel does not have, all of them
/// under `InitPolicy::Strict`, where a hook error is a 500 on every request:
///
/// 1. **Missing tables 500 every request.** `read_rows` tolerates a missing
///    table, so the seed planner plans an `Insert` for every block; `db.create`
///    does not tolerate one. Production is shielded by preview 404s and by
///    promotion following `/_deploy/init`, but `impresspress serve --target
///    cloudflare` deliberately keeps serving when the local funnel is
///    unreachable (`flows/embed_cloudflare.rs`: "local /_deploy/init not
///    reachable …; serving anyway"), and that stops working.
/// 2. **A successful seed self-invalidates the fleet.** `block_settings` is in
///    `cache_key::bumps_config_version` and both request paths run with
///    `bump_on_write: true`, so the write rewrites `cfg:v1:config_version` in
///    KV and every *other* isolate takes the multi-second full dynamic rebuild
///    this module's own comments flag for Cloudflare error 1102.
/// 3. **Concurrent isolates race the insert.** `block_name` is `TEXT NOT NULL
///    UNIQUE`; `load_and_seed` is read-then-write with no conflict tolerance,
///    and `hydrate_transient_dynamic_runtime` fires fleet-wide on a generation
///    bump, so the losing isolate answers a unique-constraint 500.
///
/// Auto-generated secrets are seeding too, and are likewise the funnel's job:
/// this hook does not call `variables::seed_auto_generated`.
pub(crate) struct CfRequestBootHooks {
    pub(crate) db: Arc<dyn DatabaseService>,
    pub(crate) block_settings_handle:
        Arc<std::sync::RwLock<impresspress_core::features::BlockSettings>>,
    /// See [`CfDeployBootHooks::config`].
    pub(crate) config: Arc<dyn ConfigService>,
}

impl CfRequestBootHooks {
    /// The database half of [`Self::seed_after_admin_init`]: one read, and
    /// there is no second half. Split out for the same reason as
    /// [`CfDeployBootHooks::seed_and_load`] — see
    /// `boot_hook_tests::the_request_path_hook_writes_nothing_to_the_database`,
    /// which asserts against the database this touches, not against which
    /// function it called.
    async fn load(&self) -> Result<impresspress_core::features::BlockSettings, String> {
        impresspress_core::platform_state::block_settings::load(&self.db)
            .await
            .map_err(|e| format!("read block_settings after admin init: {e}"))
    }
}

#[wafer_block::wafer_async_trait]
impl impresspress_core::builder::BootHooks for CfRequestBootHooks {
    async fn seed_after_admin_init(&self, wafer: &mut wafer_run::Wafer) -> Result<(), String> {
        let block_settings = self.load().await?;
        publish_block_settings(
            wafer,
            &self.config,
            &self.block_settings_handle,
            block_settings,
        );
        Ok(())
    }
}

/// Publish a freshly loaded `BlockSettings` everywhere the rest of the boot
/// reads it from: the router's shared handle, and both config surfaces (via
/// `RuntimeConfig`, so neither can be filled without the other).
fn publish_block_settings(
    wafer: &mut wafer_run::Wafer,
    config: &Arc<dyn ConfigService>,
    handle: &Arc<std::sync::RwLock<impresspress_core::features::BlockSettings>>,
    block_settings: impresspress_core::features::BlockSettings,
) {
    let published_json = block_settings.to_config_json();
    *handle
        .write()
        .expect("BlockSettings RwLock poisoned during Cloudflare boot") = block_settings;

    let mut published = RuntimeConfig::new();
    published.both(
        impresspress_core::features::BLOCK_SETTINGS_CONFIG_KEY,
        published_json,
    );
    published.republish(wafer, config);
}

/// [`BootHooks`](impresspress_core::builder::BootHooks) impl for a runtime
/// hydrated from a verified prepared plan: deliberately nothing to do.
///
/// The plan carries the block settings and WRAP grants `/_deploy/init` sealed
/// into it after seeding, and `build_runtime` installs them without touching
/// D1. Re-running [`CfDeployBootHooks`]'s seed here would answer questions the plan
/// has already answered, over the network, on the one path whose entire
/// purpose is to have no D1 structural reads — see [`boot_prepared_runtime`].
///
/// A no-op impl rather than a `None` argument: `BootHooks` is not an `Option`
/// precisely so a target that seeds nothing has to say why, in a place a
/// reader can find.
pub(crate) struct PreparedPlanBootHooks;

#[wafer_block::wafer_async_trait]
impl impresspress_core::builder::BootHooks for PreparedPlanBootHooks {
    async fn seed_after_admin_init(&self, _wafer: &mut wafer_run::Wafer) -> Result<(), String> {
        Ok(())
    }
}

/// Tests for the two Cloudflare boot hooks' database behaviour.
///
/// `seed_after_admin_init` takes `&mut wafer_run::Wafer`, which no wasm unit
/// test can produce, so these exercise the hooks' database halves —
/// [`request_path_block_settings`] and the `load_and_seed` call
/// [`CfDeployBootHooks`] makes — through the real
/// [`kv_cached_db::KvCachedD1DatabaseService`] wrapper the request path runs
/// with, over a database that records every mutation.
///
/// The assertions are about writes that did or did not reach the database and
/// the KV config-version stamp, never about which function was called: a test
/// that asserts a call was not made passes just as happily when the call moves
/// somewhere else.
#[cfg(test)]
mod boot_hook_tests {
    use std::{cell::RefCell, collections::HashMap, sync::Arc};

    use impresspress_core::{cache_key, features::FeatureConfig, kv::KvBackend};
    use wafer_block::db::{Filter, ListOptions};
    use wafer_core::interfaces::database::service::{
        AggregateSpec, Column, DatabaseError, DatabaseService, Record, RecordList, Table,
        UpsertSpec,
    };
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::*;

    /// A `DatabaseService` over an EMPTY `block_settings` table — the state a
    /// first-ever deploy is in, and the one where the seed planner plans an
    /// `Insert` for every block. Records every mutation it is asked to make.
    #[derive(Default)]
    struct RecordingDb {
        writes: RefCell<Vec<String>>,
    }

    impl RecordingDb {
        fn note(&self, write: String) {
            self.writes.borrow_mut().push(write);
        }
    }

    #[wafer_block::wafer_async_trait]
    impl DatabaseService for RecordingDb {
        async fn get(&self, _collection: &str, _id: &str) -> Result<Record, DatabaseError> {
            Err(DatabaseError::NotFound)
        }

        async fn list(
            &self,
            _collection: &str,
            _opts: &ListOptions,
        ) -> Result<RecordList, DatabaseError> {
            Ok(RecordList {
                records: Vec::new(),
                total_count: 0,
                page: 1,
                page_size: 500,
            })
        }

        async fn create(
            &self,
            collection: &str,
            _data: HashMap<String, serde_json::Value>,
        ) -> Result<Record, DatabaseError> {
            self.note(format!("create {collection}"));
            Ok(Record {
                id: "created".to_string(),
                data: HashMap::new(),
            })
        }

        async fn update(
            &self,
            collection: &str,
            id: &str,
            _data: HashMap<String, serde_json::Value>,
        ) -> Result<Record, DatabaseError> {
            self.note(format!("update {collection}/{id}"));
            Ok(Record {
                id: id.to_string(),
                data: HashMap::new(),
            })
        }

        async fn delete(&self, collection: &str, id: &str) -> Result<(), DatabaseError> {
            self.note(format!("delete {collection}/{id}"));
            Ok(())
        }

        async fn count(
            &self,
            _collection: &str,
            _filters: &[Filter],
        ) -> Result<i64, DatabaseError> {
            Ok(0)
        }

        async fn sum(
            &self,
            _collection: &str,
            _field: &str,
            _filters: &[Filter],
        ) -> Result<f64, DatabaseError> {
            Ok(0.0)
        }

        async fn query_raw(
            &self,
            _query: &str,
            _args: &[serde_json::Value],
        ) -> Result<Vec<Record>, DatabaseError> {
            unreachable!("block_settings never goes through raw SQL")
        }

        async fn exec_raw(
            &self,
            query: &str,
            _args: &[serde_json::Value],
        ) -> Result<i64, DatabaseError> {
            self.note(format!("exec_raw {query}"));
            Ok(0)
        }

        async fn upsert(&self, collection: &str, _spec: UpsertSpec) -> Result<i64, DatabaseError> {
            self.note(format!("upsert {collection}"));
            Ok(0)
        }

        async fn aggregate(
            &self,
            _collection: &str,
            _spec: AggregateSpec,
        ) -> Result<Vec<Record>, DatabaseError> {
            Ok(Vec::new())
        }

        async fn ensure_schema_table(&self, table: &Table) -> Result<(), DatabaseError> {
            self.note(format!("ensure_schema_table {}", table.name));
            Ok(())
        }

        async fn schema_table_exists(&self, _name: &str) -> Result<bool, DatabaseError> {
            Ok(true)
        }

        async fn schema_drop_table(&self, name: &str) -> Result<(), DatabaseError> {
            self.note(format!("schema_drop_table {name}"));
            Ok(())
        }

        async fn schema_add_column(
            &self,
            table: &str,
            column: &Column,
        ) -> Result<(), DatabaseError> {
            self.note(format!("schema_add_column {table}.{}", column.name));
            Ok(())
        }
    }

    /// KV backend that records every key it is asked to write, so the
    /// config-generation bump is observable.
    #[derive(Default)]
    struct RecordingKv {
        puts: RefCell<Vec<String>>,
    }

    #[async_trait::async_trait(?Send)]
    impl KvBackend for RecordingKv {
        async fn get(&self, _key: &str) -> Result<Option<String>, String> {
            Ok(None)
        }

        async fn put_with_ttl(
            &self,
            key: &str,
            _value: &str,
            _ttl_secs: u64,
        ) -> Result<(), String> {
            self.puts.borrow_mut().push(key.to_string());
            Ok(())
        }

        async fn put(&self, key: &str, _value: &str) -> Result<(), String> {
            self.puts.borrow_mut().push(key.to_string());
            Ok(())
        }

        async fn delete(&self, _key: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// The request path's exact database wiring: the KV row cache in front of
    /// D1 with `bump_on_write` ON, which is what turns any write to
    /// `block_settings` into a fleet-wide config-generation bump.
    fn request_path_db() -> (Arc<dyn DatabaseService>, Arc<RecordingDb>, Arc<RecordingKv>) {
        let inner = Arc::new(RecordingDb::default());
        let kv = Arc::new(RecordingKv::default());
        let service = kv_cached_db::KvCachedD1DatabaseService::with_mode(
            inner.clone() as Arc<dyn DatabaseService>,
            kv.clone() as Arc<dyn KvBackend>,
            kv_cached_db::CacheMode {
                read_through: true,
                bump_on_write: true,
            },
        );
        #[allow(clippy::arc_with_non_send_sync)]
        let service: Arc<dyn DatabaseService> = Arc::new(service);
        (service, inner, kv)
    }

    fn settings_handle() -> Arc<std::sync::RwLock<impresspress_core::features::BlockSettings>> {
        Arc::new(std::sync::RwLock::new(
            impresspress_core::features::BlockSettings::default(),
        ))
    }

    /// One block that ships enabled. Held here rather than taken from
    /// `blocks::block_enabled_defaults()` so the fixture proves the same thing
    /// in every feature build: this crate's default feature set has no
    /// `can_disable` block at all, so the real defaults list is empty there and
    /// a seeding call over it would write nothing for a reason that has nothing
    /// to do with the property under test.
    fn seed_defaults_fixture() -> Vec<(String, bool)> {
        vec![("impresspress/fixture".to_string(), true)]
    }

    /// **The request path is physically write-free.** On the empty table a
    /// database that has never seen `/_deploy/init` is in — the state where
    /// the seed planner plans an `Insert` for every block — the request-path
    /// hook must issue none of them.
    ///
    /// Asserted as the absence of a database write and of a KV
    /// config-generation bump, not as the absence of a call. Those writes are
    /// what 500s a `serve --target cloudflare` session on a missing table,
    /// what makes every other isolate take a full dynamic rebuild, and what
    /// two concurrent isolates race into a unique-constraint failure; a test
    /// that only checked "the seeder was not called" would keep passing the
    /// moment the write arrived by another route.
    ///
    /// The second half is the anti-vacuity control: the SAME fixture, asked to
    /// seed, records writes. So "no writes" above is a fact about the request
    /// path, not about a fixture that cannot see one.
    ///
    /// Note which build catches a regression here. This crate's DEFAULT
    /// feature set enables no `can_disable` block, so
    /// `blocks::block_enabled_defaults()` is empty and a seeding hook wired
    /// back onto the request path would write nothing in that build and keep
    /// this green. The `--features full` lane is where it fails, which is why
    /// CI now runs this suite twice.
    #[wasm_bindgen_test]
    async fn the_request_path_hook_writes_nothing_to_the_database() {
        let (db, inner, kv) = request_path_db();
        let hooks = CfRequestBootHooks {
            db: db.clone(),
            block_settings_handle: settings_handle(),
            config: request_services::config_proxy(),
        };

        let settings = hooks
            .load()
            .await
            .expect("an empty block_settings table is not an error");

        assert!(
            inner.writes.borrow().is_empty(),
            "the request path must not write to the database during boot; it issued {:?}",
            inner.writes.borrow(),
        );
        assert!(
            !kv.puts
                .borrow()
                .iter()
                .any(|key| key == cache_key::CONFIG_VERSION_KEY),
            "a request-path boot must not bump the config generation: every other \
             isolate would take a full dynamic rebuild. KV writes: {:?}",
            kv.puts.borrow(),
        );
        // It still answers with usable settings: a block with no row reads as
        // enabled, which is why not seeding here is safe.
        assert!(
            settings.is_block_enabled(impresspress_core::blocks::admin::ADMIN_BLOCK_ID),
            "a block with no row must read as enabled",
        );

        // The control. Same database, same wrapper, the deploy funnel's hook.
        let deploy = CfDeployBootHooks {
            db,
            block_settings_handle: settings_handle(),
            config: request_services::config_proxy(),
            seed_defaults: seed_defaults_fixture(),
        };
        deploy
            .seed_and_load()
            .await
            .expect("seeding an empty table succeeds");
        let seeded = format!(
            "create {}",
            impresspress_core::platform_state::block_settings::TABLE
        );
        assert!(
            inner.writes.borrow().contains(&seeded),
            "the fixture must be able to observe the structural seed write, or \
             the assertion above proves nothing; it saw {:?}",
            inner.writes.borrow(),
        );
        assert!(
            kv.puts
                .borrow()
                .iter()
                .any(|key| key == cache_key::CONFIG_VERSION_KEY),
            "and it must be able to observe the config-generation bump that \
             write causes: {:?}",
            kv.puts.borrow(),
        );
    }
}
