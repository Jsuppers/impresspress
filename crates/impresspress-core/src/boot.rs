//! Boot-time loader for admin-created WRAP grants.
//!
//! The variable seeders that used to live beside it moved to
//! [`crate::platform_state::variables`]; this loader follows in
//! `platform_state::wrap_grants` (the module then goes).

use std::sync::Arc;

use wafer_block::db::ListOptions;
use wafer_core::interfaces::database::service::DatabaseService;

/// Load admin-created WRAP grants from `impresspress__admin__wrap_grants` via
/// the `DatabaseService`. DB-service twin of the native sqlite-file reader
/// (`cli/server_config.rs::load_wrap_grants`) so stateless targets
/// (Cloudflare) can inject dynamic grants at runtime build. Missing table /
/// read errors degrade to an empty vec — dynamic grants are additive.
pub async fn load_wrap_grants_from_db(
    db: &Arc<dyn DatabaseService>,
) -> Vec<wafer_run::ResourceGrant> {
    // Structured fresh-boot signal: on a first-ever deploy the table does
    // not exist yet (deploy-init builds the runtime BEFORE migrations run)
    // — that's expected and quiet. Any error after the existence check is a
    // real read failure and yields a grant-less, WRAP-denying runtime on
    // the Cloudflare path, so it warns.
    match db
        .schema_table_exists(crate::blocks::admin::WRAP_GRANTS_TABLE)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(
                "wrap_grants table absent (fresh boot before migrations); no dynamic grants"
            );
            return Vec::new();
        }
        Err(e) => {
            tracing::warn!(error = %e, "wrap_grants existence check failed; dynamic grants skipped");
            return Vec::new();
        }
    }
    let opts = ListOptions {
        limit: 10_000,
        skip_count: true,
        ..Default::default()
    };
    let rows = match db
        .list(crate::blocks::admin::WRAP_GRANTS_TABLE, &opts)
        .await
    {
        Ok(list) => list.records,
        Err(e) => {
            tracing::warn!(error = %e, "wrap_grants read failed; dynamic grants skipped");
            return Vec::new();
        }
    };
    rows.into_iter()
        .filter_map(|r| {
            let grantee = match r.data.get("grantee").and_then(|v| v.as_str()) {
                Some(g) => g.to_string(),
                None => {
                    tracing::warn!(id = %r.id, "wrap_grants row missing/non-string `grantee`; dropped");
                    return None;
                }
            };
            let resource = match r.data.get("resource").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => {
                    tracing::warn!(id = %r.id, grantee = %grantee, "wrap_grants row missing/non-string `resource`; dropped");
                    return None;
                }
            };
            // sqlite stores booleans as 0/1 integers.
            let write = match r.data.get("write") {
                Some(v) => match v.as_i64().map(|n| n != 0).or_else(|| v.as_bool()) {
                    Some(w) => w,
                    None => {
                        tracing::warn!(id = %r.id, grantee = %grantee, resource = %resource, "wrap_grants row has non-bool/non-int `write`; dropped");
                        return None;
                    }
                },
                None => {
                    tracing::warn!(id = %r.id, grantee = %grantee, resource = %resource, "wrap_grants row missing `write`; dropped");
                    return None;
                }
            };
            // Stored wire value is the lowercase `ResourceType` Display
            // string ("db", "config", …). Absent/empty = intentional
            // all-types wildcard; a non-empty unrecognized value is a
            // typo'd grant and is dropped (fail-closed) rather than
            // widened to the wildcard.
            let resource_type = match wafer_run::ResourceType::parse_stored(
                r.data.get("resource_type").and_then(|v| v.as_str()),
            ) {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::warn!(id = %r.id, grantee = %grantee, resource = %resource, error = %e, "wrap_grants row dropped");
                    return None;
                }
            };
            Some(wafer_run::ResourceGrant {
                grantee,
                resource,
                write,
                resource_type,
            })
        })
        .collect()
}

#[cfg(test)]
mod wrap_grants_tests {
    use std::collections::HashMap;

    use super::*;

    /// Open a fresh in-memory SQLite [`DatabaseService`] with no migrations
    /// applied — the same construction `features.rs`'s
    /// `db_with_block_settings_table` and `test_support::TestContext::new`
    /// use for a real, host-runnable `DatabaseService`.
    async fn bare_db() -> Arc<dyn DatabaseService> {
        Arc::new(
            wafer_block_sqlite::service::SQLiteDatabaseService::open_in_memory()
                .expect("open in-memory sqlite"),
        )
    }

    #[tokio::test]
    async fn load_wrap_grants_maps_rows_and_tolerates_missing_table() {
        let db = bare_db().await;

        // Missing table → empty, no error.
        assert!(load_wrap_grants_from_db(&db).await.is_empty());

        // Apply admin migrations (creates `impresspress__admin__wrap_grants`
        // among the other admin tables) through the same pre-wafer DDL
        // runner native's `server.rs::run` uses
        // (`migration_helper::apply_ddl_via_service` +
        // `blocks::admin::migrations::ddl_files`) — the migration-file-
        // runner exception to the no-raw-SQL rule (CLAUDE.md), reusing the
        // real embedded schema rather than a hand-rolled CREATE TABLE.
        crate::migration_helper::apply_ddl_via_service(
            &db,
            crate::blocks::admin::migrations::ddl_files("sqlite"),
        )
        .await
        .expect("apply admin migrations");

        // Create + seed two rows via the service (no raw SQL).
        let mut r1 = HashMap::new();
        r1.insert("grantee".into(), serde_json::json!("impresspress/files"));
        r1.insert(
            "resource".into(),
            serde_json::json!("impresspress__files__objects"),
        );
        r1.insert("write".into(), serde_json::json!(1));
        r1.insert("resource_type".into(), serde_json::json!("db"));
        let mut r2 = HashMap::new();
        r2.insert("grantee".into(), serde_json::json!("wafer-run/auth"));
        r2.insert("resource".into(), serde_json::json!("bucket/x"));
        r2.insert("write".into(), serde_json::json!(0));
        db.create(crate::blocks::admin::WRAP_GRANTS_TABLE, r1)
            .await
            .unwrap();
        db.create(crate::blocks::admin::WRAP_GRANTS_TABLE, r2)
            .await
            .unwrap();

        let grants = load_wrap_grants_from_db(&db).await;
        assert_eq!(grants.len(), 2);
        let g1 = grants
            .iter()
            .find(|g| g.grantee == "impresspress/files")
            .unwrap();
        assert!(g1.write);
        assert_eq!(g1.resource_type, Some(wafer_run::ResourceType::Db));
        let g2 = grants
            .iter()
            .find(|g| g.grantee == "wafer-run/auth")
            .unwrap();
        assert!(!g2.write);
        assert_eq!(g2.resource_type, None);

        // Unrecognized resource_type → the ROW is dropped (fail-closed),
        // never widened to the all-types wildcard.
        let mut r3 = HashMap::new();
        r3.insert("grantee".into(), serde_json::json!("impresspress/products"));
        r3.insert(
            "resource".into(),
            serde_json::json!("impresspress__products__items"),
        );
        r3.insert("write".into(), serde_json::json!(1));
        r3.insert("resource_type".into(), serde_json::json!("databsae"));
        db.create(crate::blocks::admin::WRAP_GRANTS_TABLE, r3)
            .await
            .unwrap();
        // Empty-string resource_type → kept as an intentional wildcard.
        let mut r4 = HashMap::new();
        r4.insert("grantee".into(), serde_json::json!("impresspress/files"));
        r4.insert("resource".into(), serde_json::json!("bucket/y"));
        r4.insert("write".into(), serde_json::json!(0));
        r4.insert("resource_type".into(), serde_json::json!(""));
        db.create(crate::blocks::admin::WRAP_GRANTS_TABLE, r4)
            .await
            .unwrap();

        let grants = load_wrap_grants_from_db(&db).await;
        assert_eq!(grants.len(), 3, "typo'd resource_type row must be dropped");
        assert!(grants.iter().all(|g| g.grantee != "impresspress/products"));
        let g4 = grants
            .iter()
            .find(|g| g.resource == "bucket/y")
            .expect("empty resource_type row kept");
        assert_eq!(g4.resource_type, None);
    }

    use wafer_block::db::Filter;
    use wafer_core::interfaces::database::service::{
        AggregateSpec, Column, DatabaseError, Record, RecordList, Table, UpsertSpec,
    };

    /// A [`DatabaseService`] whose existence check fails hard and whose every
    /// other method is [`unreachable!`]. Isolates the fail-closed `Err` arm of
    /// [`load_wrap_grants_from_db`]: the only method it should reach is
    /// `schema_table_exists`, so a real read error there must short-circuit to
    /// an empty grant set without ever touching `list`.
    struct ErroringDb;

    #[async_trait::async_trait]
    impl DatabaseService for ErroringDb {
        async fn schema_table_exists(&self, _name: &str) -> Result<bool, DatabaseError> {
            Err(DatabaseError::Internal(
                "simulated wrap_grants existence-check failure".into(),
            ))
        }

        async fn get(&self, _collection: &str, _id: &str) -> Result<Record, DatabaseError> {
            unreachable!(
                "load_wrap_grants_from_db must not read rows after an existence-check error"
            )
        }

        async fn list(
            &self,
            _collection: &str,
            _opts: &ListOptions,
        ) -> Result<RecordList, DatabaseError> {
            unreachable!(
                "load_wrap_grants_from_db must not list rows after an existence-check error"
            )
        }

        async fn create(
            &self,
            _collection: &str,
            _data: HashMap<String, serde_json::Value>,
        ) -> Result<Record, DatabaseError> {
            unreachable!()
        }

        async fn update(
            &self,
            _collection: &str,
            _id: &str,
            _data: HashMap<String, serde_json::Value>,
        ) -> Result<Record, DatabaseError> {
            unreachable!()
        }

        async fn delete(&self, _collection: &str, _id: &str) -> Result<(), DatabaseError> {
            unreachable!()
        }

        async fn count(
            &self,
            _collection: &str,
            _filters: &[Filter],
        ) -> Result<i64, DatabaseError> {
            unreachable!()
        }

        async fn sum(
            &self,
            _collection: &str,
            _field: &str,
            _filters: &[Filter],
        ) -> Result<f64, DatabaseError> {
            unreachable!()
        }

        async fn query_raw(
            &self,
            _query: &str,
            _args: &[serde_json::Value],
        ) -> Result<Vec<Record>, DatabaseError> {
            unreachable!()
        }

        async fn exec_raw(
            &self,
            _query: &str,
            _args: &[serde_json::Value],
        ) -> Result<i64, DatabaseError> {
            unreachable!()
        }

        async fn upsert(&self, _collection: &str, _spec: UpsertSpec) -> Result<i64, DatabaseError> {
            unreachable!()
        }

        async fn aggregate(
            &self,
            _collection: &str,
            _spec: AggregateSpec,
        ) -> Result<Vec<Record>, DatabaseError> {
            unreachable!()
        }

        async fn ensure_schema_table(&self, _table: &Table) -> Result<(), DatabaseError> {
            unreachable!()
        }

        async fn schema_drop_table(&self, _name: &str) -> Result<(), DatabaseError> {
            unreachable!()
        }

        async fn schema_add_column(
            &self,
            _table: &str,
            _column: &Column,
        ) -> Result<(), DatabaseError> {
            unreachable!()
        }
    }

    /// A hard read error from the existence check is fail-closed: dynamic
    /// grants are additive, so a runtime that cannot confirm the table exists
    /// must build WRAP-denying (empty grants) rather than risk widening access
    /// on a bad read.
    #[tokio::test]
    async fn load_wrap_grants_existence_check_error_fails_closed() {
        let db: Arc<dyn DatabaseService> = Arc::new(ErroringDb);
        assert!(
            load_wrap_grants_from_db(&db).await.is_empty(),
            "a schema_table_exists error must degrade to an empty grant set"
        );
    }
}
