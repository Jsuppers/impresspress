//! Post-build lifecycle for the [`ImpresspressBuilder`](super::ImpresspressBuilder)
//! runtime: the target-agnostic [`boot`] orchestrator, its [`BootHooks`] seam,
//! the [`post_start`] WRAP-grant injector, and the native-embedding
//! `register_vector_block` helper.

#[cfg(feature = "native-embedding")]
use std::sync::Arc;

use wafer_run::{RuntimeError, Wafer};

use crate::blocks::storage::ImpresspressStorageBlock;

/// Eagerly initialize every registered block in deterministic order and fail
/// on the first error.
///
/// This is stricter than [`Wafer::init_all_blocks`], which deliberately logs
/// and tolerates failures. Stateless runtimes use this before publishing a
/// shared runtime handle: publishing an empty or failed lazy-init slot would
/// let concurrent requests wait on one another's init future, which is not a
/// valid execution model for request-isolated platforms such as Cloudflare
/// Workers.
///
/// The admin block runs first when present, matching the normal boot/deploy
/// ordering. Remaining registration names are already sorted by
/// [`Wafer::block_names`].
pub async fn strict_init_all_blocks(wafer: &Wafer) -> Result<(), String> {
    let admin = crate::blocks::admin::ADMIN_BLOCK_ID;
    let names = wafer.block_names();

    if names.iter().any(|name| name == admin) {
        wafer
            .init_block(admin)
            .await
            .map_err(|error| format!("block `{admin}` Init failed: {error}"))?;
    }

    for name in names {
        if name == admin {
            continue;
        }
        wafer
            .init_block(&name)
            .await
            .map_err(|error| format!("block `{name}` Init failed: {error}"))?;
    }
    Ok(())
}

/// Call after `wafer.start()` or `wafer.seal()` to inject
/// collected WRAP grants into the storage block for cross-block access control.
pub fn post_start(wafer: &Wafer, storage_block: &ImpresspressStorageBlock) {
    storage_block.update_wrap_grants(wafer.wrap_grants());
}

/// Per-target boot I/O for [`boot`]. Implemented by each platform to supply
/// the one step that genuinely differs between them: what to seed and which
/// shared snapshots to publish into once the admin block's `Init` has created
/// the variables / block_settings tables.
///
/// Everything else around it — the invariant `seal → admin-first init →
/// (seed) → init_all_blocks → post_start` ordering — is owned by [`boot`], so
/// the two stateless targets (Cloudflare, browser) collapse to a hook impl
/// plus the platform-specific request/response plumbing.
#[wafer_block::wafer_async_trait]
pub trait BootHooks {
    /// Runs AFTER `init_block(admin)` (so the admin migration has created the
    /// `impresspress__admin__variables` + `block_settings` tables) and BEFORE
    /// `init_all_blocks` (so a block depending on a seeded `auto_generate` key
    /// can't lose the `HashMap::keys()` race and permanent-fail on a missing
    /// secret — the impresspress #209 regression class).
    ///
    /// Implementations call [`crate::platform_state::variables::seed_auto_generated`] /
    /// [`crate::platform_state::variables::seed_and_load`] /
    /// [`crate::features::load_and_seed_block_settings`] as appropriate and
    /// publish the results into the shared `ConfigService` / `BlockSettings`
    /// handle / crypto secret the runtime already holds.
    ///
    /// Errors abort the boot — return `Err` for a genuinely fatal condition
    /// (for example, structural settings could not be persisted consistently,
    /// or a required secret cannot be read). Optional best-effort per-key
    /// seeds should still log and continue inside the implementation.
    /// Receives `&mut Wafer` so a target that could not know its settings
    /// before admin migration (browser/Cloudflare first deploy) can publish the
    /// seeded values into the config snapshot before any other block initializes.
    async fn seed_after_admin_init(&self, wafer: &mut Wafer) -> Result<(), String>;
}

/// Target-agnostic boot orchestrator owning the invariant post-build lifecycle
/// sequence shared by the stateless targets (Cloudflare Workers, browser WASM):
///
/// 1. `seal()` — finalize composite/uses/capability/snapshot wiring.
/// 2. `init_block(admin)` FIRST — admin's migrations create the variables /
///    block_settings tables before any other block's `Init` writes to them,
///    and before the seed step reads them. Failure is logged-and-tolerated
///    (matching `init_all_blocks`' resilience contract) so one broken block
///    can't wedge the whole runtime.
/// 3. `hooks.seed_after_admin_init` — seed + publish (see [`BootHooks`]).
/// 4. `init_all_blocks()` — eager-init the rest; admin is a slot-cached no-op.
/// 5. `post_start()` — inject WRAP grants into the storage block.
///
/// The caller must have already wired the pre-seal bits its platform needs
/// (`set_config_snapshot`, `set_asset_loader`, any post-build block
/// registration) onto `wafer` before calling this. After it returns, the
/// caller dispatches requests / stores the runtime handle as appropriate.
///
/// Native uses this funnel too, then runs the native-only
/// [`Wafer::run_start_lifecycle`] + [`Wafer::bind_all`] steps afterwards: its
/// HTTP-listener block binds the TCP socket in the `Start`-lifecycle `bind()`
/// pass, which the stateless targets omit (they dispatch per-request via
/// `wafer.run`). Native still seeds pre-wafer — its immutable
/// `Argon2JwtCryptoService` and config snapshot need the variables before
/// `build()` — so its `seed_after_admin_init` is a no-op, mirroring how
/// Cloudflare reads a non-mutating settings snapshot pre-build, then seeds and
/// republishes structural defaults here after admin migration.
pub async fn boot(
    wafer: &mut Wafer,
    storage_block: &ImpresspressStorageBlock,
    hooks: &dyn BootHooks,
) -> Result<(), RuntimeError> {
    wafer.seal().await?;

    // Admin first — its Init creates the variables / block_settings tables the
    // seed step reads, and migration 002's `block` column the auto-gen seeder
    // writes. Tolerated-and-logged, like every other init.
    if let Err(e) = wafer.init_block(crate::blocks::admin::ADMIN_BLOCK_ID).await {
        tracing::warn!(error = %e, "admin block Init failed before seeding");
    }

    hooks
        .seed_after_admin_init(wafer)
        .await
        .map_err(RuntimeError::Config)?;

    wafer.init_all_blocks().await;

    post_start(wafer, storage_block);
    Ok(())
}

/// Register the `wafer-run/vector` runtime block backed by native
/// `SqliteVecService` + `FastembedService`.
///
/// - Opens a dedicated `rusqlite::Connection` at `db_path`. SQLite supports
///   multi-connection access with WAL, so sharing the DB file with the
///   platform's `DatabaseService` connection is safe.
/// - `FastembedService::default_model()` triggers an ONNX model download on
///   first run. Failure is logged but does not abort startup — the vector
///   runtime block simply won't be registered, and any attempt to use it
///   will fail via the normal dependency-resolution path.
///
/// This function is only compiled when the `native-embedding` feature is on;
/// the `impresspress/vector` feature block registration in `impresspress-core` is
/// gated by the same feature so the two stay in sync.
#[cfg(feature = "native-embedding")]
pub(super) fn register_vector_block(
    wafer: &mut Wafer,
    db_path: Option<&str>,
) -> Result<(), RuntimeError> {
    use wafer_block_fastembed::FastembedService;
    use wafer_block_sqlite::vector::SqliteVecService;
    use wafer_core::interfaces::vector::service::{EmbeddingService, VectorService};

    let Some(db_path) = db_path else {
        return Err(RuntimeError::Config(
            "native-embedding feature is enabled but no sqlite_db_path was \
             provided to ImpresspressBuilder — call .sqlite_db_path(...) before \
             .build()"
                .to_string(),
        ));
    };

    // Dedicated connection for the vector service — see module docs on
    // `sqlite_db_path` for why a second connection is fine.
    let vec_conn = rusqlite::Connection::open(db_path).map_err(|e| {
        RuntimeError::Config(format!(
            "failed to open SQLite connection at '{db_path}' for vector service: {e}"
        ))
    })?;
    let vec_svc: Arc<dyn VectorService> = Arc::new(SqliteVecService::new(vec_conn));

    let emb_svc: Arc<dyn EmbeddingService> = match FastembedService::default_model() {
        Ok(svc) => Arc::new(svc),
        Err(e) => {
            // Model download can fail offline or on first-run with restricted
            // egress. Log and skip registration so the rest of the runtime
            // boots; `impresspress/vector` registration will fail dep resolution
            // with a clearer error than a half-wired block would.
            tracing::warn!(
                error = ?e,
                "fastembed model unavailable — skipping wafer-run/vector registration"
            );
            return Ok(());
        }
    };

    wafer_core::service_blocks::vector::register_with(wafer, vec_svc, emb_svc)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use wafer_block::{
        core_types::{ErrorCode, LifecycleEvent, LifecycleType, Message, WaferError},
        streams::{input::InputStream, output::OutputStream},
        Block, BlockInfo,
    };
    use wafer_run::{StaticConfigSource, Wafer};

    use super::strict_init_all_blocks;

    struct InitProbeBlock {
        name: &'static str,
        order: Arc<Mutex<Vec<&'static str>>>,
        fail: bool,
    }

    #[wafer_block::wafer_async_trait]
    impl Block for InitProbeBlock {
        fn info(&self) -> BlockInfo {
            BlockInfo::new(self.name, "0.1.0", "test/init@v1", "test")
        }

        async fn lifecycle(
            &self,
            _ctx: &dyn wafer_block::context::Context,
            event: LifecycleEvent,
        ) -> Result<(), WaferError> {
            if event.event_type == LifecycleType::Init {
                self.order
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(self.name);
                if self.fail {
                    return Err(WaferError::new(
                        ErrorCode::Unknown,
                        "deliberate Init failure",
                    ));
                }
            }
            Ok(())
        }

        async fn handle(
            &self,
            _ctx: &dyn wafer_block::context::Context,
            _msg: Message,
            _input: InputStream,
        ) -> OutputStream {
            OutputStream::respond(Vec::new())
        }
    }

    fn register_probe(
        wafer: &mut Wafer,
        name: &'static str,
        order: &Arc<Mutex<Vec<&'static str>>>,
        fail: bool,
    ) {
        wafer
            .register_block(
                name,
                Arc::new(InitProbeBlock {
                    name,
                    order: order.clone(),
                    fail,
                }),
            )
            .unwrap();
    }

    fn empty_wafer() -> Wafer {
        let config: Arc<dyn wafer_run::ConfigSource> = Arc::new(StaticConfigSource::default());
        Wafer::new(config).unwrap()
    }

    #[tokio::test]
    async fn strict_init_is_admin_first_deterministic_and_once_only() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut wafer = empty_wafer();
        register_probe(&mut wafer, "test/zeta", &order, false);
        register_probe(
            &mut wafer,
            crate::blocks::admin::ADMIN_BLOCK_ID,
            &order,
            false,
        );
        register_probe(&mut wafer, "test/alpha", &order, false);

        strict_init_all_blocks(&wafer).await.unwrap();
        strict_init_all_blocks(&wafer).await.unwrap();

        assert_eq!(
            *order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec![
                crate::blocks::admin::ADMIN_BLOCK_ID,
                "test/alpha",
                "test/zeta"
            ]
        );
    }

    #[tokio::test]
    async fn strict_init_fails_closed_without_initializing_later_blocks() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut wafer = empty_wafer();
        register_probe(
            &mut wafer,
            crate::blocks::admin::ADMIN_BLOCK_ID,
            &order,
            false,
        );
        register_probe(&mut wafer, "test/alpha-fails", &order, true);
        register_probe(&mut wafer, "test/zeta-never", &order, false);

        let error = strict_init_all_blocks(&wafer).await.unwrap_err();
        assert!(error.contains("test/alpha-fails"), "{error}");
        assert_eq!(
            *order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec![crate::blocks::admin::ADMIN_BLOCK_ID, "test/alpha-fails"]
        );
    }
}
