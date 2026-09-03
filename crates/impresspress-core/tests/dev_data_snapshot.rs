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
        auth::repo::{
            api_keys, bootstrap_tokens, jwt_blocklist, local_credentials, oauth_pkce, orgs, pats,
            provider_links, rate_limits, sessions, tokens, users,
        },
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
const PURCHASES_TABLE: &str = "impresspress__products__purchases";
const ADMIN_USER_ROLES_TABLE: &str = "impresspress__admin__user_roles";

/// Every table `wafer-run/auth` (the framework block, `AuthBlock` in
/// `wafer_core::service_blocks::auth`) owns.
///
/// Unlike `ProductsBlock`/`AdminBlock`, the framework auth block declares no
/// `BlockInfo.collections` at all (its `info()` only adds the grants its
/// `AuthService` reports) — its tables are named directly by each
/// `auth::repo::*` module instead, per that module's own "one module per
/// table" convention (`blocks/auth/repo/mod.rs`). So this list, not a
/// `BlockInfo` reflection, is this file's source of "every table the auth
/// block declares" for the coverage test below.
const AUTH_TABLES: &[&str] = &[
    api_keys::TABLE,
    bootstrap_tokens::TABLE,
    jwt_blocklist::TABLE,
    local_credentials::TABLE,
    oauth_pkce::TABLE,
    orgs::TABLE,
    pats::TABLE,
    provider_links::TABLE,
    rate_limits::TABLE,
    sessions::TABLE,
    tokens::TABLE,
    users::TABLE,
];

// ---------------------------------------------------------------------------
// Coverage: every declared table has a decision.
// ---------------------------------------------------------------------------

#[test]
fn every_declared_table_of_the_three_blocks_has_an_export_decision() {
    let mut declared: Vec<String> = Vec::new();
    for info in [ProductsBlock::new().info(), AdminBlock::new().info()] {
        declared.extend(info.collections.iter().map(|c| c.name.clone()));
    }
    declared.extend(AUTH_TABLES.iter().map(|s| s.to_string()));

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
        "the sensitive variable is filtered out at export"
    );
    assert!(vars.iter().all(|v| v["sensitive"] != json!(true)
        && !v["key"].as_str().unwrap().starts_with("IMPRESSPRESS_")));
    assert!(!vars
        .iter()
        .any(|v| v["key"] == "WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_PASSWORD"));
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

fn one_product_snapshot() -> DataSnapshot {
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
    DataSnapshot {
        schema_version: data_snapshot::SCHEMA_VERSION,
        tables,
    }
}

#[tokio::test]
async fn seed_import_applies_data_json_when_present() {
    let ctx = TestContext::with_products()
        .await
        .with_dev_added(FakeControl::new())
        .await;
    let manifest = SeedManifest {
        schema_version: seed::SCHEMA_VERSION,
        source_generation: None,
        site: vec![index_file()],
        blocks: vec![],
        data: Some("seed/data.json".to_string()),
    };
    let fetch = MapFetch::default()
        .with(&seed::site_url("index.html"), b"<h1>shop</h1>")
        .with(
            "/seed/data.json",
            &serde_json::to_vec(&one_product_snapshot()).unwrap(),
        );

    seed::import(&ctx, &manifest, &fetch).await.unwrap();

    let products = db::list_all(&ctx, PRODUCTS_TABLE, Vec::new())
        .await
        .unwrap();
    assert_eq!(products.len(), 1);
    assert_eq!(products[0].id, "prod_seeded");
}
