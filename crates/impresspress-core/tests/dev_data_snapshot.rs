//! The data snapshot: the export allowlist's coverage, what it filters out,
//! and the typed round trip through `seed::import`.
//!
//! Gated on `block-dev` for the same reason `dev_seed.rs` is: the block does
//! not exist in a default-feature build, so these tests must not compile
//! there.
//!
//! `PRODUCTS_TABLE`/`OFFERS_TABLE`/`PURCHASES_TABLE`/`ADMIN_USER_ROLES_TABLE`
//! below restate table names `impresspress-core` keeps `pub(crate)` (the
//! products ones are further gated by that block's own door tests —
//! `blocks/products/tests/repo_door_test.rs` — which refuse a re-export
//! anywhere reachable from outside `repo::products`). That module owns the
//! literal strings; this is a separate compilation unit and cannot reach
//! them, so it restates the ones it needs — the same choice
//! `tests/dev_blocks.rs` already makes for `"impresspress__products__products"`.
#![cfg(feature = "block-dev")]

use std::collections::BTreeSet;

use impresspress_core::{
    admin_schema,
    blocks::{
        admin::AdminBlock,
        auth::repo::users,
        dev::{
            data_snapshot::{self, DataSnapshot},
            seed::{self, SeedManifest},
            test_support::{seed_file, FakeControl, MapFetch},
        },
        products::ProductsBlock,
    },
    test_support::TestContext,
    util::json_map,
};
use serde_json::json;
use wafer_core::clients::database as db;
use wafer_run::Block;

// ---------------------------------------------------------------------------
// Table names this crate keeps private, restated here — see the module docs.
// ---------------------------------------------------------------------------

const PRODUCTS_TABLE: &str = "impresspress__products__products";
const OFFERS_TABLE: &str = "impresspress__products__offers";
const OFFER_COMPONENTS_TABLE: &str = "impresspress__products__offer_components";
const PRODUCT_VERSIONS_TABLE: &str = "impresspress__products__product_versions";
const CHECKOUT_PRESETS_TABLE: &str = "impresspress__products__checkout_presets";
const PRODUCTS_VARIABLES_TABLE: &str = "impresspress__products__variables";
const PURCHASES_TABLE: &str = "impresspress__products__purchases";
const ADMIN_USER_ROLES_TABLE: &str = "impresspress__admin__user_roles";

// ---------------------------------------------------------------------------
// Coverage: every declared table has a decision.
// ---------------------------------------------------------------------------

/// Every table name created by a `CREATE TABLE [IF NOT EXISTS] <name>`
/// statement anywhere under the three blocks' own migration directories —
/// the ground truth of what tables actually exist, read straight off the
/// `.sql` files rather than off any Rust-side declaration of them.
///
/// This is why the coverage test below does not need (and, for auth, has no
/// other way to get) a `BlockInfo.collections`-style list: `BlockInfo`'s own
/// list is advisory (see the comment on `products/mod.rs`'s `.collections`
/// call) and, as this test caught once already, can fall behind a table a
/// migration created — `impresspress__products__stripe_events` (migration
/// `003_stripe_events`) was never added to `ProductsBlock::info().collections`
/// at all. Scanning the migrations directly cannot have that gap: a table
/// with no `CREATE TABLE` here does not exist, and one that exists has no
/// way to hide from this scan the way it could from a hand-kept list.
fn tables_created_in_migrations() -> BTreeSet<String> {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut tables = BTreeSet::new();
    for block in ["products", "admin", "auth"] {
        let dir = manifest_dir
            .join("src/blocks")
            .join(block)
            .join("migrations");
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("sql") {
                continue;
            }
            let sql = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            for line in sql.lines() {
                let Some(rest) = line.trim_start().strip_prefix("CREATE TABLE") else {
                    continue;
                };
                let rest = rest.trim_start();
                let rest = rest
                    .strip_prefix("IF NOT EXISTS")
                    .map_or(rest, str::trim_start);
                let name: String = rest
                    .chars()
                    .take_while(|c| !c.is_whitespace() && *c != '(')
                    .collect();
                assert!(
                    !name.is_empty(),
                    "{}: a `CREATE TABLE` line with no table name: {line:?}",
                    path.display()
                );
                tables.insert(name);
            }
        }
    }
    tables
}

#[test]
fn every_declared_table_of_the_three_blocks_has_an_export_decision() {
    let mut declared: BTreeSet<String> = tables_created_in_migrations();
    // Cross-checked, not replaced, against the two feature blocks' own
    // advisory `BlockInfo.collections` — a table one of them lists that no
    // migration actually creates would be exactly the "declared but not
    // real" mirror image of the `stripe_events` gap the migration scan
    // exists to catch, and belongs in this failure too.
    for info in [ProductsBlock::new().info(), AdminBlock::new().info()] {
        declared.extend(info.collections.iter().map(|c| c.name.clone()));
    }
    assert!(
        declared.len() > 40,
        "the migration scan found {} tables — it lost its way",
        declared.len()
    );

    let decided: BTreeSet<&str> = data_snapshot::TABLE_ALLOWLIST
        .iter()
        .map(|(table, _)| *table)
        .chain(data_snapshot::TABLE_EXCLUDED.iter().copied())
        .collect();
    let undecided: Vec<&String> = declared
        .iter()
        .filter(|table| !decided.contains(table.as_str()))
        .collect();
    assert!(
        undecided.is_empty(),
        "tables with no export decision: {undecided:?} — add each to TABLE_ALLOWLIST or \
         TABLE_EXCLUDED deliberately"
    );
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Insert one row directly, honoring the supplied `id` — the same
/// direct-write pattern `blocks::products::tests::harness::seed` uses inside
/// the crate, reimplemented here because that helper is `#[cfg(test)]`
/// private to `impresspress-core` and this file is a separate crate.
async fn seed_row(ctx: &TestContext, table: &str, id: &str, data: serde_json::Value) {
    let mut map = json_map(data);
    map.insert("id".to_string(), serde_json::Value::String(id.to_string()));
    db::create(ctx, table, map)
        .await
        .unwrap_or_else(|e| panic!("seed into {table} failed: {} ({:?})", e.message, e.code));
}

/// One active product with an offer, one purchase against it, and one
/// sensitive admin variable alongside a non-sensitive one. Returns the
/// seeded owner's user id.
async fn seed_product_and_order(ctx: &TestContext) -> String {
    let user_id = "user_owner".to_string();
    seed_row(
        ctx,
        users::TABLE,
        &user_id,
        json!({
            "email": "owner@example.com",
            "display_name": "Owner",
            "role": "user",
            "email_verified": true,
        }),
    )
    .await;

    seed_row(
        ctx,
        PRODUCTS_TABLE,
        "prod_widget",
        json!({
            "name": "Widget",
            "status": "active",
            "created_by": user_id,
            // Provider-linkage columns, deliberately non-default here:
            // `export_carries_products_but_never_secrets_or_orders` asserts
            // these come back reset (see `data_snapshot::reset_provider_linkage`).
            "stripe_product_id": "prod_stripe_source_owns_this",
            "seller_account_id": "seller_source_owns_this",
        }),
    )
    .await;

    seed_row(
        ctx,
        OFFERS_TABLE,
        "offer_standard",
        json!({
            "product_id": "prod_widget",
            "name": "Standard",
            "stripe_product_id": "prod_stripe_source_owns_this",
            "stripe_price_id": "price_source_owns_this",
            "sync_status": "synced",
        }),
    )
    .await;

    seed_row(
        ctx,
        OFFER_COMPONENTS_TABLE,
        "component_base",
        json!({
            "offer_id": "offer_standard",
            "component_key": "base",
            "label": "Base",
            "stripe_price_id": "price_component_source_owns_this",
        }),
    )
    .await;

    seed_row(
        ctx,
        PURCHASES_TABLE,
        "purchase_1",
        json!({ "user_id": user_id }),
    )
    .await;

    seed_row(
        ctx,
        ADMIN_USER_ROLES_TABLE,
        "role_owner_admin",
        json!({ "user_id": user_id, "role": "admin" }),
    )
    .await;

    // Never exported: the `sensitive` flag is set.
    seed_row(
        ctx,
        admin_schema::VARIABLES_TABLE,
        "var_secret",
        json!({
            "key": "WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_PASSWORD",
            "value": "hunter2",
            "sensitive": true,
        }),
    )
    .await;
    // Exported: ordinary site config.
    seed_row(
        ctx,
        admin_schema::VARIABLES_TABLE,
        "var_public",
        json!({
            "key": "WAFER_RUN_SHARED__APP_NAME",
            "value": "Acme Shop",
            "sensitive": false,
        }),
    )
    .await;
    // Never exported: `IMPRESSPRESS_`-prefixed, even with a clear flag —
    // CLAUDE.md reserves that prefix for infrastructure config.
    seed_row(
        ctx,
        admin_schema::VARIABLES_TABLE,
        "var_infra",
        json!({
            "key": "IMPRESSPRESS_INTERNAL_FLAG",
            "value": "true",
            "sensitive": false,
        }),
    )
    .await;

    user_id
}

// ---------------------------------------------------------------------------
// export()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn export_carries_products_but_never_secrets_or_orders() {
    let ctx = TestContext::with_products().await.with_auth_added().await;
    seed_product_and_order(&ctx).await;

    let snap = data_snapshot::export(&ctx).await.unwrap();

    assert_eq!(snap.tables[PRODUCTS_TABLE].len(), 1);
    assert!(!snap.tables.contains_key(PURCHASES_TABLE));

    let vars = &snap.tables[admin_schema::VARIABLES_TABLE];
    assert_eq!(
        vars.len(),
        1,
        "the sensitive and IMPRESSPRESS_-prefixed variables are filtered out at export"
    );
    assert!(vars.iter().all(|v| v["sensitive"] != json!(true)
        && !v["key"].as_str().unwrap().starts_with("IMPRESSPRESS_")));
    assert!(!vars
        .iter()
        .any(|v| v["key"] == "WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_PASSWORD"));
    // The vacuous half of the check above, made real: the row really was
    // seeded, and really is absent, not merely never present to begin with.
    assert!(!vars
        .iter()
        .any(|v| v["key"] == "IMPRESSPRESS_INTERNAL_FLAG"));

    // Provider-linkage columns come back reset — this destination has no
    // Stripe account the source's ids belong to.
    let product = &snap.tables[PRODUCTS_TABLE][0];
    assert_eq!(product["stripe_product_id"], json!(""));
    assert_eq!(product["seller_account_id"], json!(""));
    let offer = &snap.tables[OFFERS_TABLE][0];
    assert_eq!(offer["stripe_product_id"], json!(""));
    assert_eq!(offer["stripe_price_id"], json!(""));
    assert_eq!(offer["sync_status"], json!("not_synced"));
    assert_eq!(offer["sync_error"], json!(""));
    let component = &snap.tables[OFFER_COMPONENTS_TABLE][0];
    assert_eq!(component["stripe_price_id"], json!(""));
}

/// Products are read live-only, and everything that hangs off a product has
/// to be read the same way or the export carries rows pointing at nothing.
///
/// A soft-deleted product with an offer is the shape that breaks it: the
/// product is filtered out by `list_live_products`, while `offers`,
/// `product_versions` and — two links down — `offer_components`, `variables`
/// and `checkout_presets` were read whole. The imported shop then holds an
/// offer whose `product_id` names no row, and inputs and presets naming that
/// offer — inert (the catalog reads active products) but data the export
/// never decided to carry.
///
/// A variable with a BLANK `offer_id` is the deliberate exception and is
/// asserted here too: that column is `NOT NULL DEFAULT ''`, so an unowned
/// variable is a legitimate row and not an orphan.
#[tokio::test]
async fn a_soft_deleted_products_offers_and_versions_do_not_travel() {
    let ctx = TestContext::with_products().await.with_auth_added().await;
    seed_product_and_order(&ctx).await;

    // A second product, its own offer, that offer's component and a version
    // row — then the product is soft-deleted, exactly as
    // `DELETE /b/products/api/admin/products/{id}` leaves it.
    seed_row(
        &ctx,
        PRODUCTS_TABLE,
        "prod_retired",
        json!({ "name": "Retired", "status": "active" }),
    )
    .await;
    seed_row(
        &ctx,
        OFFERS_TABLE,
        "offer_retired",
        json!({ "product_id": "prod_retired", "name": "Retired" }),
    )
    .await;
    seed_row(
        &ctx,
        OFFER_COMPONENTS_TABLE,
        "component_retired",
        json!({
            "offer_id": "offer_retired",
            "component_key": "base",
            "label": "Base",
        }),
    )
    .await;
    seed_row(
        &ctx,
        PRODUCT_VERSIONS_TABLE,
        "version_retired",
        json!({ "product_id": "prod_retired", "version": 1 }),
    )
    .await;
    seed_row(
        &ctx,
        PRODUCT_VERSIONS_TABLE,
        "version_live",
        json!({ "product_id": "prod_widget", "version": 1 }),
    )
    .await;
    // Two links from the product: a preset and a typed input on the retired
    // offer, the same pair on the live one, and one variable owned by no
    // offer at all.
    for (id, offer) in [
        ("preset_retired", "offer_retired"),
        ("preset_live", "offer_standard"),
    ] {
        seed_row(
            &ctx,
            CHECKOUT_PRESETS_TABLE,
            id,
            json!({ "offer_id": offer, "name": "Preset", "slug": id }),
        )
        .await;
    }
    for (id, offer) in [
        ("var_retired", "offer_retired"),
        ("var_live", "offer_standard"),
    ] {
        seed_row(
            &ctx,
            PRODUCTS_VARIABLES_TABLE,
            id,
            json!({ "offer_id": offer, "name": "pages", "var_type": "number" }),
        )
        .await;
    }
    seed_row(
        &ctx,
        PRODUCTS_VARIABLES_TABLE,
        "var_global",
        json!({ "offer_id": "", "name": "legacy", "var_type": "number" }),
    )
    .await;
    db::update(
        &ctx,
        PRODUCTS_TABLE,
        "prod_retired",
        json_map(json!({ "deleted_at": "2026-09-01T00:00:00Z" })),
    )
    .await
    .expect("soft-delete the second product");

    let snap = data_snapshot::export(&ctx).await.unwrap();

    // Sorted: which rows a table carries is the subject, and their order is
    // the database's listing order rather than anything this asserts.
    let ids = |table: &str| -> Vec<String> {
        let mut ids: Vec<String> = snap.tables[table]
            .iter()
            .map(|row| row["id"].as_str().unwrap_or_default().to_string())
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(ids(PRODUCTS_TABLE), vec!["prod_widget"]);
    // The live product's rows all travel…
    assert_eq!(ids(OFFERS_TABLE), vec!["offer_standard"]);
    assert_eq!(ids(OFFER_COMPONENTS_TABLE), vec!["component_base"]);
    assert_eq!(ids(PRODUCT_VERSIONS_TABLE), vec!["version_live"]);
    assert_eq!(ids(CHECKOUT_PRESETS_TABLE), vec!["preset_live"]);
    // …the unowned variable travels beside the live offer's own…
    assert_eq!(
        ids(PRODUCTS_VARIABLES_TABLE),
        vec!["var_global", "var_live"]
    );
    // …and the retired product's rows — including the three that are two
    // links from the product that orphaned them — travel with it or not at
    // all.
    for (table, id) in [
        (OFFERS_TABLE, "offer_retired"),
        (OFFER_COMPONENTS_TABLE, "component_retired"),
        (PRODUCT_VERSIONS_TABLE, "version_retired"),
        (CHECKOUT_PRESETS_TABLE, "preset_retired"),
        (PRODUCTS_VARIABLES_TABLE, "var_retired"),
    ] {
        assert!(
            !ids(table).contains(&id.to_string()),
            "{id} travelled in {table} without the product it belongs to"
        );
    }
}

// ---------------------------------------------------------------------------
// import()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn import_replaces_users_and_upserts_products_so_ownership_survives() {
    let src = TestContext::with_products().await.with_auth_added().await;
    let admin_id = seed_product_and_order(&src).await;

    let snap = data_snapshot::export(&src).await.unwrap();
    assert_eq!(
        snap.tables[users::TABLE][0]["id"].as_str().unwrap(),
        admin_id
    );

    // Fresh: a different context, its own (differently-id'd) rows if it had
    // any — here, none at all, which is the more common "first import"
    // shape than a context that already has a bootstrap admin.
    let dst = TestContext::with_products().await.with_auth_added().await;
    seed_row(
        &dst,
        users::TABLE,
        "user_fresh_bootstrap",
        json!({ "email": "fresh@example.com", "display_name": "Fresh Admin" }),
    )
    .await;
    seed_row(
        &dst,
        ADMIN_USER_ROLES_TABLE,
        "role_fresh_bootstrap",
        json!({ "user_id": "user_fresh_bootstrap", "role": "admin" }),
    )
    .await;

    let report = data_snapshot::import(&dst, &snap).await.unwrap();
    assert_eq!(report.tables[users::TABLE], 1);
    assert_eq!(report.tables[ADMIN_USER_ROLES_TABLE], 1);

    let users_rows = db::list_all(&dst, users::TABLE, Vec::new()).await.unwrap();
    assert_eq!(
        users_rows.len(),
        1,
        "replace semantics: the fresh bootstrap admin is gone"
    );
    assert_eq!(users_rows[0].id, admin_id);

    let role_rows = db::list_all(&dst, ADMIN_USER_ROLES_TABLE, Vec::new())
        .await
        .unwrap();
    assert_eq!(
        role_rows.len(),
        1,
        "replace semantics: the fresh bootstrap admin's role assignment is gone too"
    );
    assert_eq!(role_rows[0].data["user_id"], json!(admin_id));

    let products = db::list_all(&dst, PRODUCTS_TABLE, Vec::new())
        .await
        .unwrap();
    assert_eq!(products.len(), 1);
    assert_eq!(products[0].data["created_by"], json!(admin_id));

    // Importing again is idempotent.
    data_snapshot::import(&dst, &snap).await.unwrap();
    assert_eq!(
        db::list_all(&dst, PRODUCTS_TABLE, Vec::new())
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        db::list_all(&dst, users::TABLE, Vec::new())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn import_refuses_a_table_outside_this_builds_allowlist() {
    let ctx = TestContext::with_products().await;
    let mut snap = DataSnapshot {
        schema_version: data_snapshot::SCHEMA_VERSION,
        tables: std::collections::BTreeMap::new(),
    };
    snap.tables.insert(
        "impresspress__products__stripe_events".to_string(),
        vec![json_map(json!({ "id": "evt_1" })).into_iter().collect()],
    );
    let err = data_snapshot::import(&ctx, &snap).await.unwrap_err();
    assert_eq!(err.code, wafer_run::ErrorCode::InvalidArgument);
}

#[tokio::test]
async fn import_refuses_a_schema_version_this_build_does_not_read() {
    let ctx = TestContext::with_products().await;
    let snap = DataSnapshot {
        schema_version: data_snapshot::SCHEMA_VERSION + 1,
        tables: std::collections::BTreeMap::new(),
    };
    let err = data_snapshot::import(&ctx, &snap).await.unwrap_err();
    assert_eq!(err.code, wafer_run::ErrorCode::InvalidArgument);
}

// ---------------------------------------------------------------------------
// seed::import wiring
// ---------------------------------------------------------------------------

fn index_file() -> seed::SeedFile {
    seed_file("index.html", b"<h1>shop</h1>")
}

/// One product (`Mode::Upsert`), one admin variable (`Mode::Upsert`) and one
/// user (`Mode::Replace`) — at least one table from each of the three
/// blocks, so a test importing this under a WRAP-enforced dev context (see
/// `seed_import_applies_data_json_when_present`) exercises the typed Db
/// grant `dev::wrap_grants` adds per `TABLE_ALLOWLIST` table on more than
/// just the specially-routed products path.
fn mixed_snapshot() -> DataSnapshot {
    let mut tables = std::collections::BTreeMap::new();
    tables.insert(
        PRODUCTS_TABLE.to_string(),
        // `db::upsert` (the path `Mode::Upsert` writes through) issues the
        // insert exactly as given — unlike `db::create`, it does not
        // synthesize `created_at`/`updated_at` for a caller who omits them,
        // so a real export's row (read back off an existing one, which
        // always carries both) is what this fixture has to imitate.
        vec![json_map(json!({
            "id": "prod_seeded",
            "name": "Seeded Widget",
            "status": "active",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        }))
        .into_iter()
        .collect()],
    );
    tables.insert(
        admin_schema::VARIABLES_TABLE.to_string(),
        vec![json_map(json!({
            "id": "var_seeded",
            "key": "WAFER_RUN_SHARED__APP_NAME",
            "value": "Seeded Shop",
            "sensitive": false,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        }))
        .into_iter()
        .collect()],
    );
    tables.insert(
        users::TABLE.to_string(),
        vec![json_map(json!({
            "id": "user_seeded",
            "email": "seeded@example.com",
            "display_name": "Seeded Owner",
        }))
        .into_iter()
        .collect()],
    );
    DataSnapshot {
        schema_version: data_snapshot::SCHEMA_VERSION,
        tables,
    }
}

/// A [`seed::SeedFile`] declaring `bytes` at `path`, the way an exporter
/// would — the same derivation [`seed_file`] uses for workspace files,
/// reimplemented here because `manifest.data` is a bare file, not one that
/// lands in the workspace tree `seed_file` targets.
fn data_file(path: &str, bytes: &[u8]) -> seed::SeedFile {
    seed::SeedFile {
        path: path.to_string(),
        sha256: impresspress_core::blocks::dev::blobs::sha256_hex(bytes),
        size: bytes.len() as u64,
        content_type: "application/json".to_string(),
    }
}

#[tokio::test]
async fn seed_import_applies_data_json_when_present() {
    // The importer takes the runtime seam explicitly, so a test binds the
    // control it built the context over rather than passing a second one.
    let control = FakeControl::new();
    let ctx = TestContext::with_products()
        .await
        .with_auth_added()
        .await
        .with_dev_added(control.clone())
        .await;
    let data_bytes = serde_json::to_vec(&mixed_snapshot()).unwrap();
    let manifest = SeedManifest {
        schema_version: seed::SCHEMA_VERSION,
        source_generation: None,
        site: vec![index_file()],
        blocks: vec![],
        data: Some(data_file("data.json", &data_bytes)),
    };
    let fetch = MapFetch::default()
        .with(&seed::site_url("index.html"), b"<h1>shop</h1>")
        .with(&seed::data_url("data.json"), &data_bytes);

    seed::import(&ctx, control.as_ref(), &manifest, &fetch)
        .await
        .unwrap();

    let products = db::list_all(&ctx, PRODUCTS_TABLE, Vec::new())
        .await
        .unwrap();
    assert_eq!(products.len(), 1);
    assert_eq!(products[0].id, "prod_seeded");

    let vars = db::list_all(&ctx, admin_schema::VARIABLES_TABLE, Vec::new())
        .await
        .unwrap();
    assert_eq!(vars.len(), 1);
    assert_eq!(vars[0].id, "var_seeded");

    let users_rows = db::list_all(&ctx, users::TABLE, Vec::new()).await.unwrap();
    assert_eq!(users_rows.len(), 1);
    assert_eq!(users_rows[0].id, "user_seeded");
}

/// A `data.json` whose declared hash doesn't match its bytes fails the same
/// way a corrupted `site`/block-source file would (design §10.2) — and the
/// failure propagates out of `seed::import` as a whole, not just out of the
/// data-snapshot step.
#[tokio::test]
async fn seed_import_fails_when_data_json_does_not_verify() {
    let control = FakeControl::new();
    let ctx = TestContext::with_products()
        .await
        .with_dev_added(control.clone())
        .await;
    let data_bytes = serde_json::to_vec(&mixed_snapshot()).unwrap();
    let mut declared = data_file("data.json", &data_bytes);
    declared.sha256 = "0".repeat(64); // does not match `data_bytes`
    let manifest = SeedManifest {
        schema_version: seed::SCHEMA_VERSION,
        source_generation: None,
        site: vec![index_file()],
        blocks: vec![],
        data: Some(declared),
    };
    let fetch = MapFetch::default()
        .with(&seed::site_url("index.html"), b"<h1>shop</h1>")
        .with(&seed::data_url("data.json"), &data_bytes);

    let err = seed::import(&ctx, control.as_ref(), &manifest, &fetch)
        .await
        .expect_err("a hash mismatch on data.json must fail the whole seed import");
    assert!(
        err.contains("hashes to"),
        "expected a hash-mismatch message, got: {err}"
    );

    // Nothing from the (unverified) snapshot was applied.
    assert_eq!(
        db::list_all(&ctx, PRODUCTS_TABLE, Vec::new())
            .await
            .unwrap()
            .len(),
        0
    );
}

// ---------------------------------------------------------------------------
// Identity: the destination mints its own ids for some allowlisted tables
// ---------------------------------------------------------------------------

const ADMIN_ROLES_TABLE: &str = "impresspress__admin__roles";

/// `created_at`/`updated_at` are `TEXT NOT NULL` with no default on these
/// tables, and a real exported row always carries them — it came out of
/// `db::list_all`. A fixture row that omitted them would fail the insert for
/// a reason that has nothing to do with what these tests are about.
const STAMP: &str = "2026-09-03T00:00:00Z";

/// An import into an instance that has ALREADY seeded its own copies of the
/// rows the snapshot carries must succeed.
///
/// This is the case design §10.2 is entirely about — a bundle importing into
/// a fresh instance — and "fresh" does not mean empty: by the time the seed
/// import runs, admin's migration has seeded `roles` and the boot hook has
/// seeded `variables`, each with an id this instance minted for itself. The
/// exporting instance minted different ids for the same rows. Both tables
/// mark their natural key `UNIQUE` (`roles.name`, `variables.key`), so an
/// upsert keyed on `id` does not conflict on the id at all: it is an INSERT
/// that then violates that index, and `import` fails wholesale with a bare
/// "internal database error".
///
/// It is written with two rows per table on purpose — one whose natural key
/// the destination already has (the collision) and one it does not (a plain
/// insert) — because a fix that simply skipped conflicting rows would pass a
/// test that only had the first.
#[tokio::test]
async fn an_import_lands_on_rows_the_destination_seeded_with_its_own_ids() {
    let ctx = TestContext::with_products().await;

    // What the DESTINATION seeded for itself, with its own ids.
    seed_row(
        &ctx,
        ADMIN_ROLES_TABLE,
        "role_minted_here",
        json!({ "name": "admin", "description": "this instance's own admin role" }),
    )
    .await;
    seed_row(
        &ctx,
        admin_schema::VARIABLES_TABLE,
        "var_minted_here",
        json!({
            "key": "WAFER_RUN_SHARED__APP_NAME",
            "value": "Untitled",
            "sensitive": false,
        }),
    )
    .await;

    // What the SNAPSHOT carries: the same natural keys under the exporting
    // instance's ids, plus one row of each that is genuinely new.
    let mut tables = std::collections::BTreeMap::new();
    tables.insert(
        ADMIN_ROLES_TABLE.to_string(),
        vec![
            json_map(json!({
                "id": "role_minted_over_there",
                "name": "admin",
                "description": "the exporting instance's admin role",
                "created_at": STAMP,
                "updated_at": STAMP,
            }))
            .into_iter()
            .collect(),
            json_map(json!({
                "id": "role_editor",
                "name": "editor",
                "description": "a role the destination has never heard of",
                "created_at": STAMP,
                "updated_at": STAMP,
            }))
            .into_iter()
            .collect(),
        ],
    );
    tables.insert(
        admin_schema::VARIABLES_TABLE.to_string(),
        vec![
            json_map(json!({
                "id": "var_minted_over_there",
                "key": "WAFER_RUN_SHARED__APP_NAME",
                "value": "The print shop",
                "sensitive": false,
                "created_at": STAMP,
                "updated_at": STAMP,
            }))
            .into_iter()
            .collect(),
            json_map(json!({
                "id": "var_new",
                "key": "WAFER_RUN_SHARED__HAS_LANDING_PAGE",
                "value": "true",
                "sensitive": false,
                "created_at": STAMP,
                "updated_at": STAMP,
            }))
            .into_iter()
            .collect(),
        ],
    );
    let snapshot = DataSnapshot {
        schema_version: data_snapshot::SCHEMA_VERSION,
        tables,
    };

    data_snapshot::import(&ctx, &snapshot)
        .await
        .expect("an import must land on rows the destination seeded for itself");

    // One `admin` role, not two — and it carries the SNAPSHOT's description
    // under the DESTINATION's id. Keeping the destination's id is what stops
    // an import from orphaning every `user_roles.role_id` already pointing at
    // it.
    let roles = db::list_all(&ctx, ADMIN_ROLES_TABLE, Vec::new())
        .await
        .unwrap();
    let admin_roles: Vec<_> = roles
        .iter()
        .filter(|r| r.data["name"] == json!("admin"))
        .collect();
    assert_eq!(admin_roles.len(), 1, "{roles:?}");
    assert_eq!(admin_roles[0].id, "role_minted_here");
    assert_eq!(
        admin_roles[0].data["description"],
        json!("the exporting instance's admin role")
    );
    // …and the role the destination had never heard of was inserted, under
    // the id the snapshot gave it.
    assert!(roles.iter().any(|r| r.id == "role_editor"), "{roles:?}");

    let vars = db::list_all(&ctx, admin_schema::VARIABLES_TABLE, Vec::new())
        .await
        .unwrap();
    let app_name: Vec<_> = vars
        .iter()
        .filter(|v| v.data["key"] == json!("WAFER_RUN_SHARED__APP_NAME"))
        .collect();
    assert_eq!(app_name.len(), 1, "{vars:?}");
    assert_eq!(app_name[0].id, "var_minted_here");
    assert_eq!(app_name[0].data["value"], json!("The print shop"));
    assert!(vars.iter().any(|v| v.id == "var_new"), "{vars:?}");
}

/// Re-importing the SAME snapshot converges rather than duplicating — the
/// idempotence the module docs claim, now that the conflict target is the
/// natural key rather than the id.
#[tokio::test]
async fn re_importing_one_snapshot_converges_on_the_natural_key() {
    let ctx = TestContext::with_products().await;
    let mut tables = std::collections::BTreeMap::new();
    tables.insert(
        ADMIN_ROLES_TABLE.to_string(),
        vec![json_map(json!({
            "id": "role_a",
            "name": "admin",
            "created_at": STAMP,
            "updated_at": STAMP,
        }))
        .into_iter()
        .collect()],
    );
    let snapshot = DataSnapshot {
        schema_version: data_snapshot::SCHEMA_VERSION,
        tables,
    };

    data_snapshot::import(&ctx, &snapshot).await.unwrap();
    data_snapshot::import(&ctx, &snapshot).await.unwrap();

    let roles = db::list_all(&ctx, ADMIN_ROLES_TABLE, Vec::new())
        .await
        .unwrap();
    assert_eq!(roles.len(), 1, "{roles:?}");
}

/// Every `Upsert` table's declared conflict columns must be columns the table
/// actually has a `UNIQUE` constraint on — an upsert whose conflict target is
/// not unique is not an upsert, it is an insert that will one day collide.
///
/// Read off the migration SQL, like
/// `every_declared_table_of_the_three_blocks_has_an_export_decision` above:
/// the schema is the ground truth, and a `UNIQUE` added or dropped there
/// without a matching change here should fail rather than wait for an export
/// to fail in someone's browser.
#[test]
fn every_upsert_target_is_a_unique_key_of_its_table() {
    // Whitespace-normalised so the checks below are about the SQL rather than
    // about how it happens to be laid out.
    let sql: String = sqlite_migration_sql()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for (table, mode) in data_snapshot::TABLE_ALLOWLIST {
        let data_snapshot::Mode::Upsert(conflict) = mode else {
            continue;
        };
        for column in *conflict {
            if *column == "id" {
                // Every allowlisted table declares `id TEXT PRIMARY KEY`, so
                // an id conflict target is unique by construction. Assert the
                // table exists at all, which is what would break first.
                assert!(
                    sql.contains(&format!("CREATE TABLE IF NOT EXISTS {table} (")),
                    "{table} has no CREATE TABLE in the migrations"
                );
                continue;
            }
            // Either an inline `<column> TEXT … UNIQUE` in the CREATE TABLE,
            // or a `CREATE UNIQUE INDEX … ON <table>(<column>)`.
            let create = format!("CREATE TABLE IF NOT EXISTS {table} (");
            let body = sql
                .split_once(&create)
                .map(|(_, rest)| rest.split_once(");").map_or(rest, |(body, _)| body))
                .unwrap_or_else(|| panic!("{table} has no CREATE TABLE in the migrations"));
            let inline = body.split(',').any(|col| {
                let col = col.trim();
                col.starts_with(&format!("{column} ")) && col.contains("UNIQUE")
            });
            let indexed = sql.contains(&format!("ON {table}({column})"))
                || sql.contains(&format!("ON {table} ({column})"));
            assert!(
                inline || indexed,
                "{table}'s upsert conflicts on {column:?}, but no UNIQUE constraint on it \
                 appears in the migrations — the upsert would insert and then collide"
            );
        }
    }
}

/// The `product_id` / `offer_id` columns each allowlisted table declares,
/// read off the migrations.
///
/// Both shapes count, because both are how the schema got here: a column in
/// the `CREATE TABLE` body, and a later `ALTER TABLE … ADD COLUMN` (which is
/// how `variables` got its `offer_id` in migration 005, and the reason a scan
/// of `CREATE TABLE`s alone would have missed exactly the table this test
/// exists to catch).
fn owner_columns_declared_in_migrations() -> BTreeSet<(String, String)> {
    const OWNER_COLUMNS: &[&str] = &["product_id", "offer_id"];
    let sql: String = sqlite_migration_sql()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut found = BTreeSet::new();
    for (table, _mode) in data_snapshot::TABLE_ALLOWLIST {
        let create = format!("CREATE TABLE IF NOT EXISTS {table} (");
        let body = sql
            .split_once(&create)
            .map(|(_, rest)| rest.split_once(");").map_or(rest, |(body, _)| body))
            .unwrap_or_else(|| panic!("{table} has no CREATE TABLE in the migrations"));
        // The column NAME is the first token of each comma-separated
        // declaration, compared whole: `stripe_product_id` is not
        // `product_id`, and a substring match would call it one.
        for column in body.split(',') {
            let name = column.split_whitespace().next().unwrap_or_default();
            if OWNER_COLUMNS.contains(&name) {
                found.insert((table.to_string(), name.to_string()));
            }
        }
        let alter = format!("ALTER TABLE {table} ADD COLUMN ");
        let mut rest = sql.as_str();
        while let Some((_, tail)) = rest.split_once(&alter) {
            let name = tail.split_whitespace().next().unwrap_or_default();
            if OWNER_COLUMNS.contains(&name) {
                found.insert((table.to_string(), name.to_string()));
            }
            rest = tail;
        }
    }
    found
}

/// `OWNED_TABLES` is closed against the schema, in both directions.
///
/// The export filters an owned table's rows against the ids its owner
/// actually exported, and products are read live-only — so a table with a
/// `product_id`/`offer_id` that is NOT on that list exports rows pointing at
/// products the archive does not carry. `M9` was three such tables; the point
/// of this test is that the fourth one to be added to `TABLE_ALLOWLIST` fails
/// the build instead of shipping orphans.
///
/// The reverse direction matters too: an entry naming a column its table does
/// not have would filter every row of it out (`None` is dropped), silently
/// emptying the table in every export.
#[test]
fn owned_tables_covers_every_allowlisted_table_with_an_owner_column() {
    let declared = owner_columns_declared_in_migrations();
    // The scan found something at all — a broken parser that found nothing
    // would make both assertions below vacuous.
    assert!(
        declared.len() >= 5,
        "the owner-column scan found {declared:?} — it lost its way"
    );
    let listed: BTreeSet<(String, String)> = data_snapshot::OWNED_TABLES
        .iter()
        .map(|(table, column, _)| (table.to_string(), column.to_string()))
        .collect();

    let unowned: Vec<&(String, String)> = declared.difference(&listed).collect();
    assert!(
        unowned.is_empty(),
        "allowlisted tables with an owner column that OWNED_TABLES does not filter on: \
         {unowned:?} — add each with its owner, or their rows travel orphaned when the owner \
         is soft-deleted"
    );
    let imaginary: Vec<&(String, String)> = listed.difference(&declared).collect();
    assert!(
        imaginary.is_empty(),
        "OWNED_TABLES filters on columns the schema does not declare: {imaginary:?} — every \
         row of those tables would be dropped from every export"
    );

    // And each entry's OWNER is itself exported, or the filter reads against
    // a set that is never filled.
    let allowlisted: BTreeSet<&str> = data_snapshot::TABLE_ALLOWLIST
        .iter()
        .map(|(table, _)| *table)
        .collect();
    for (table, _column, owner) in data_snapshot::OWNED_TABLES {
        assert!(
            allowlisted.contains(owner),
            "{table:?} is filtered against {owner:?}, which is not on TABLE_ALLOWLIST"
        );
    }
}

/// Every SQLite migration of the three blocks, concatenated.
///
/// The same three directories `tables_created_in_migrations` reads, and for
/// the same reason: the schema is the ground truth for what the import can
/// actually do, and a Rust-side declaration of it is a second copy that can
/// fall behind.
fn sqlite_migration_sql() -> String {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = String::new();
    for block in ["products", "admin", "auth"] {
        let dir = manifest_dir
            .join("src/blocks")
            .join(block)
            .join("migrations");
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("sql")
                || !path.to_string_lossy().contains("sqlite")
            {
                continue;
            }
            out.push_str(
                &std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("read {}: {e}", path.display())),
            );
            out.push('\n');
        }
    }
    out
}
