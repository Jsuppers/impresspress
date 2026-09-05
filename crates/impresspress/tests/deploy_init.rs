//! Integration test for `impresspress_core::deploy_init::deploy_init` over a
//! real, file-backed SQLite `DatabaseService`, built through the same
//! `impresspress::cli::server::build_native_runtime` the binary uses, but
//! calling `deploy_init` instead of `builder::boot` so per-block outcomes
//! are captured into a report.
//!
//! Exercises the full ordering invariant end to end: seal → init_block
//! (admin) → seed hook (no-op on native, which seeds pre-wafer) → init
//! every other registered block → post_start. Then rebuilds a second
//! runtime over the *same* sqlite file (matching a redeploy) and asserts
//! the block-settings hash-gate makes the second `deploy_init` call an
//! all-ok no-op.

use std::{path::Path, sync::Arc};

use impresspress::cli::server::{build_native_runtime, NativeBootHooks, NativeRuntime};
use impresspress_core::{builder::BootHooks, deploy_init::deploy_init};
use impresspress_native::InfraConfig;
use wafer_core::interfaces::database::service::DatabaseService;
use wafer_run::Wafer;

/// The infra config `run()` would read from the environment, pointed at
/// the test's temp paths. `listen` is unused here: `deploy_init` never binds.
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
async fn deploy_init_first_run_ok_and_second_run_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("deploy_init_test.sqlite3");
    let storage_root = tmp.path().join("storage");
    std::fs::create_dir_all(&storage_root).expect("create storage root");

    // --- First run: fresh DB, everything must init ok. ---
    let (mut wafer, storage_block, db) = build_runtime(&db_path, &storage_root).await;
    let report = deploy_init(&mut wafer, &storage_block, &NativeBootHooks)
        .await
        .expect("seal");

    assert!(report.ok, "first deploy_init must succeed: {report:?}");
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
    let report2 = deploy_init(&mut wafer2, &storage_block2, &NativeBootHooks)
        .await
        .expect("seal 2");

    assert!(
        report2.ok,
        "second deploy_init must be a clean no-op: {report2:?}"
    );
    assert!(
        report2
            .blocks
            .iter()
            .any(|b| b.block == impresspress_core::blocks::admin::ADMIN_BLOCK_ID && b.ok),
        "admin block must be ok on second run too: {:?}",
        report2.blocks
    );
}

/// `BootHooks` whose seed step always fails, to exercise `deploy_init`'s
/// capture-and-continue contract: a failing hook must NOT abort the funnel
/// (still `Ok(report)`), and every other block must still get initialized.
struct FailingBootHooks;

#[wafer_block::wafer_async_trait]
impl BootHooks for FailingBootHooks {
    async fn seed_after_admin_init(&self, _wafer: &mut Wafer) -> Result<(), String> {
        Err("boom".to_string())
    }
}

#[tokio::test]
async fn deploy_init_seed_failure_is_captured_not_aborted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("deploy_init_seed_failure_test.sqlite3");
    let storage_root = tmp.path().join("storage");
    std::fs::create_dir_all(&storage_root).expect("create storage root");

    let (mut wafer, storage_block, _db) = build_runtime(&db_path, &storage_root).await;
    let report = deploy_init(&mut wafer, &storage_block, &FailingBootHooks)
        .await
        .expect("deploy_init must still return Ok even when the seed hook errors");

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
