//! Integration tests for `impresspress_core::builder::boot` over a real,
//! file-backed SQLite `DatabaseService`, built through the same
//! `impresspress::cli::server::build_native_runtime` the binary uses.
//!
//! `boot` is the one post-`build()` lifecycle for every target; these tests
//! drive it under [`InitPolicy::Reported`], the policy `/_deploy/init` uses,
//! because that is the one that captures per-block outcomes into a
//! [`BootReport`] a test can read. The ordering it exercises end to end is the
//! invariant one: grants → seal → `init_block(admin)` → seed hook (a no-op on
//! native, which seeds pre-wafer) → every other registered block → the WRAP
//! grants into the storage block.
//!
//! The first test then rebuilds a second runtime over the *same* sqlite file
//! (matching a redeploy) and asserts the block-settings hash-gate makes the
//! second `boot` an all-ok no-op.

use std::{collections::HashMap, path::Path, sync::Arc};

use impresspress::cli::server::{build_native_runtime, NativeBootHooks, NativeRuntime};
use impresspress_core::builder::{boot, BootHooks, GrantSource, InitPolicy};
use impresspress_native::InfraConfig;
use wafer_core::interfaces::database::service::DatabaseService;
use wafer_run::Wafer;

/// The reason native passes [`GrantSource::PreInstalled`]: `build_native_runtime`
/// reads the admin-created rows out of the platform database and hands them to
/// `ImpresspressBuilder::wrap_grants` before `build()`, because native seeds and
/// reads everything pre-wafer. Spelled once here so every call below states the
/// same thing the binary states.
const NATIVE_GRANTS: GrantSource<'static> = GrantSource::PreInstalled(
    "build_native_runtime loads them from the platform database into \
     ImpresspressBuilder::wrap_grants before build()",
);

/// The infra config `run()` would read from the environment, pointed at
/// the test's temp paths. `listen` is unused here: `boot` never binds.
fn infra_for(db_path: &Path, storage_root: &Path) -> InfraConfig {
    InfraConfig {
        listen: "127.0.0.1:0".to_string(),
        db_type: "sqlite".to_string(),
        db_path: db_path
            .to_str()
            .expect("db path is valid utf-8")
            .to_string(),
        db_url: None,
        storage_type: "local".to_string(),
        storage_root: storage_root
            .to_str()
            .expect("storage root is valid utf-8")
            .to_string(),
    }
}

/// Build one WAFER runtime over the sqlite file at `db_path` through the
/// binary's own `build_native_runtime` (no process-env vars to seed in this
/// harness; auto-generated secrets, including the JWT secret, are still
/// seeded). Returns the built-but-not-yet-inited `Wafer`, its
/// `ImpresspressStorageBlock`, and the `DatabaseService` handle so the test
/// can inspect `block_settings` rows directly afterwards.
async fn build_runtime(
    db_path: &Path,
    storage_root: &Path,
) -> (
    Wafer,
    Arc<impresspress_core::blocks::storage::ImpresspressStorageBlock>,
    Arc<dyn DatabaseService>,
) {
    let infra = infra_for(db_path, storage_root);
    let database = impresspress_native::make_database_service(&infra.db_type, &infra.db_path, None)
        .await
        .expect("construct sqlite database service");

    let NativeRuntime {
        wafer,
        storage_block,
    } = build_native_runtime(&infra, database.clone(), &[], false)
        .await
        .expect("build impresspress runtime");

    (wafer, storage_block, database)
}

#[tokio::test]
async fn boot_first_run_ok_and_second_run_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("boot_lifecycle_test.sqlite3");
    let storage_root = tmp.path().join("storage");
    std::fs::create_dir_all(&storage_root).expect("create storage root");

    // --- First run: fresh DB, everything must init ok. ---
    let (mut wafer, storage_block, db) = build_runtime(&db_path, &storage_root).await;
    let report = boot(
        &mut wafer,
        &storage_block,
        &NativeBootHooks,
        NATIVE_GRANTS,
        InitPolicy::Reported,
    )
    .await
    .expect("seal");

    assert!(report.ok, "first boot must succeed: {report:?}");
    assert!(report.sealed);
    assert!(
        report
            .blocks
            .iter()
            .any(|b| b.block == impresspress_core::blocks::admin::ADMIN_BLOCK_ID && b.ok),
        "admin block must be present and ok: {:?}",
        report.blocks
    );
    // More than just admin got initialized (the default feature set
    // registers several other feature blocks).
    assert!(
        report.blocks.len() > 1,
        "expected more than admin to be initialized: {:?}",
        report.blocks
    );

    // --- Stamp format: block_settings rows carry 64-hex current_hash == blessed_hash. ---
    let opts = wafer_block::db::ListOptions {
        limit: 10_000,
        skip_count: true,
        ..Default::default()
    };
    let rows = db
        .list(
            impresspress_core::platform_state::block_settings::TABLE,
            &opts,
        )
        .await
        .expect("list block_settings")
        .records;
    let admin_row = rows
        .iter()
        .find(|r| {
            r.data["block_name"]
                == serde_json::json!(impresspress_core::blocks::admin::ADMIN_BLOCK_ID)
        })
        .expect("admin row stamped");
    let cur = admin_row.data["current_hash"]
        .as_str()
        .expect("current_hash is a string");
    assert_eq!(cur.len(), 64, "raw sha256 hex, got: {cur}");
    assert!(
        cur.chars().all(|c| c.is_ascii_hexdigit()),
        "current_hash must be hex: {cur}"
    );
    assert_eq!(
        admin_row.data["current_hash"],
        admin_row.data["blessed_hash"]
    );

    // --- Idempotency: second run over the same DB, via a REBUILT runtime, is all-ok. ---
    let (mut wafer2, storage_block2, _db2) = build_runtime(&db_path, &storage_root).await;
    let report2 = boot(
        &mut wafer2,
        &storage_block2,
        &NativeBootHooks,
        NATIVE_GRANTS,
        InitPolicy::Reported,
    )
    .await
    .expect("seal 2");

    assert!(report2.ok, "second boot must be a clean no-op: {report2:?}");
    assert!(
        report2
            .blocks
            .iter()
            .any(|b| b.block == impresspress_core::blocks::admin::ADMIN_BLOCK_ID && b.ok),
        "admin block must be ok on second run too: {:?}",
        report2.blocks
    );
}

/// `BootHooks` whose seed step always fails, to exercise
/// [`InitPolicy::Reported`]'s capture-and-continue contract: a failing hook
/// must NOT abort the funnel (still `Ok(report)`), and every other block must
/// still get initialized.
struct FailingBootHooks;

#[wafer_block::wafer_async_trait]
impl BootHooks for FailingBootHooks {
    async fn seed_after_admin_init(&self, _wafer: &mut Wafer) -> Result<(), String> {
        Err("boom".to_string())
    }
}

#[tokio::test]
async fn reported_boot_captures_a_seed_failure_without_aborting() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("boot_seed_failure_test.sqlite3");
    let storage_root = tmp.path().join("storage");
    std::fs::create_dir_all(&storage_root).expect("create storage root");

    let (mut wafer, storage_block, _db) = build_runtime(&db_path, &storage_root).await;
    let report = boot(
        &mut wafer,
        &storage_block,
        &FailingBootHooks,
        NATIVE_GRANTS,
        InitPolicy::Reported,
    )
    .await
    .expect("a Reported boot must still return Ok when the seed hook errors");

    assert!(
        !report.ok,
        "overall report must be not-ok when the seed hook fails: {report:?}"
    );
    assert!(
        !report.seed.ok,
        "seed step outcome must be not-ok: {:?}",
        report.seed
    );
    assert_eq!(report.seed.error.as_deref(), Some("boom"));

    // Seed failure must not prevent the rest of the funnel: blocks still
    // get initialized.
    assert!(
        !report.blocks.is_empty(),
        "blocks must still be initialized after a seed failure: {:?}",
        report.blocks
    );
    assert!(
        report.blocks.iter().all(|b| b.ok),
        "every block must still init ok despite the seed failure: {:?}",
        report.blocks
    );
}

/// The same seed failure under the policies the long-lived and the
/// request-isolated targets use is fatal instead: only `/_deploy/init`, whose
/// product is the report, keeps going.
#[tokio::test]
async fn a_seed_failure_aborts_a_tolerant_or_strict_boot() {
    for policy in [InitPolicy::Tolerant, InitPolicy::Strict] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("boot_seed_fatal_test.sqlite3");
        let storage_root = tmp.path().join("storage");
        std::fs::create_dir_all(&storage_root).expect("create storage root");

        let (mut wafer, storage_block, _db) = build_runtime(&db_path, &storage_root).await;
        let error = boot(
            &mut wafer,
            &storage_block,
            &FailingBootHooks,
            NATIVE_GRANTS,
            policy,
        )
        .await
        .expect_err(&format!("a seed failure must abort under {policy:?}"));
        assert!(error.to_string().contains("boom"), "{policy:?}: {error}");
    }
}

/// The Cloudflare prepared-hydration path passes `GrantSource::PreInstalled`
/// because `ImpresspressBuilder::apply_prepared_plan` has already installed the
/// plan's grants — this pins that the claim is true through `build()` and
/// `seal()`, on the only runtime a host test can actually build.
///
/// `boot` cannot check this for the caller: after `seal()` a grant cannot be
/// added at all, so a prepared path that dropped its grants would seal a
/// runtime whose storage block enforces nothing across blocks, and every
/// cross-block read would start refusing. The `PreInstalled` variant is a
/// claim; this is the test that the claim holds.
#[tokio::test]
async fn a_prepared_plans_grants_reach_the_sealed_runtime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("prepared_grants_test.sqlite3");
    let storage_root = tmp.path().join("storage");
    std::fs::create_dir_all(&storage_root).expect("create storage root");

    let grant = wafer_run::ResourceGrant::read_write(
        "impresspress/files",
        "impresspress__admin__variables",
    );

    // Export a plan carrying one deployment grant, exactly the way
    // `/_deploy/init` does after seeding (`publish_wrap_grants`).
    let source = impresspress_core::builder::ImpresspressBuilder::new();
    let exporter = source
        .prepared_plan_exporter()
        .expect("prepared plan exporter");
    exporter
        .publish_wrap_grants(std::slice::from_ref(&grant))
        .expect("publish wrap grants");
    let build_sha = format!("sha256:{}", "b".repeat(64));
    let lock = impresspress_core::prepared_plan::WaferLockIdentity::absent();
    let assets = impresspress_core::prepared_plan::PreparedReleaseAssets::absent();
    let plan = exporter
        .prepare_runtime_plan("app", build_sha.clone(), lock.clone(), assets.clone())
        .expect("prepare runtime plan");
    assert_eq!(plan.structure.deployment_wrap_grants.len(), 1);

    // Hydrate a real runtime from it, then boot it the way the prepared
    // Cloudflare path does.
    let infra = infra_for(&db_path, &storage_root);
    let database = impresspress_native::make_database_service(&infra.db_type, &infra.db_path, None)
        .await
        .expect("construct sqlite database service");
    let storage = impresspress_native::make_storage_service("local", &infra.storage_root)
        .await
        .expect("construct local storage service");

    let builder = impresspress_core::builder::RuntimeConfig::new()
        .install(
            impresspress_core::builder::ImpresspressBuilder::new()
                .database(database)
                .storage(storage),
            |_empty| Arc::new(wafer_core::service_blocks::config::EnvConfigService::new()),
        )
        .crypto(
            impresspress_native::make_jwt_crypto_service(
                "prepared-grants-test-jwt-secret-value".to_string(),
            )
            .expect("jwt crypto service"),
        )
        .network(impresspress_native::make_fetch_network_service().expect("network service"))
        .logger(impresspress_native::make_tracing_logger())
        .apply_prepared_plan(&plan, "app", &build_sha, &lock, &assets)
        .expect("apply prepared plan");

    let (mut wafer, storage_block) = builder.build().expect("build impresspress runtime");
    boot(
        &mut wafer,
        &storage_block,
        &NativeBootHooks,
        GrantSource::PreInstalled(
            "ImpresspressBuilder::apply_prepared_plan installs the verified \
             plan's wrap_grants and deployment_wrap_grants before build()",
        ),
        InitPolicy::Reported,
    )
    .await
    .expect("boot the hydrated runtime");

    let rendered = format!("{:?}", wafer.wrap_grants());
    assert!(
        rendered.contains("impresspress__admin__variables"),
        "the plan's grant must be registered before seal: {rendered}"
    );
}

/// Native fills both config surfaces from one `RuntimeConfig`, so anything the
/// async `ConfigService` carries is readable synchronously through
/// `ctx.config_get` as well. Before `RuntimeConfig` these were two literals in
/// `build_native_runtime` held together by a comment.
#[tokio::test]
async fn the_native_build_fills_the_synchronous_config_surface() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("native_config_surfaces.sqlite3");
    let storage_root = tmp.path().join("storage");
    std::fs::create_dir_all(&storage_root).expect("create storage root");

    let (wafer, _storage_block, _db) = build_runtime(&db_path, &storage_root).await;
    let snapshot: &HashMap<String, String> = wafer.config_snapshot();

    assert!(
        snapshot.contains_key(impresspress_core::features::BLOCK_SETTINGS_CONFIG_KEY),
        "block settings must reach the synchronous surface: {:?}",
        snapshot.keys().collect::<Vec<_>>()
    );
    assert!(
        !snapshot.contains_key(impresspress_core::migration_helper::RUN_MIGRATIONS_KEY),
        "this harness builds with run_migrations = false",
    );
    // The JWT secret is seeded pre-wafer by `build_native_runtime` and is one
    // of the variables it fans into both surfaces.
    assert!(
        snapshot.contains_key(impresspress_core::blocks::auth::JWT_SECRET_KEY),
        "seeded variables must reach the synchronous surface: {:?}",
        snapshot.keys().collect::<Vec<_>>()
    );
}
