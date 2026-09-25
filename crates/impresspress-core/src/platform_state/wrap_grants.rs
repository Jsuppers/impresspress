//! `impresspress__admin__wrap_grants`: admin-created WRAP grants — the rows
//! the permissions page manages, loaded at every runtime build on top of the
//! grants the blocks declare in code.
//!
//! Two callers, one codec (spec 2.1.2). The boot flavour ([`load`]) runs
//! over [`DatabaseService`] before WRAP exists — it is what the runtime's
//! grant list is built from, so it can hardly run under it; the runtime
//! flavour ([`list`], [`create`], [`delete`]) is the admin block's
//! permissions surface under WRAP over [`Context`]. Both go through
//! [`WrapGrantRow::from_record`] / [`WrapGrantRow::to_data`], and
//! [`WrapGrantRow::into_resource_grant`] is the one place a stored row
//! becomes a [`ResourceGrant`].

use std::{collections::HashMap, sync::Arc};

use serde_json::{json, Value};
use wafer_block::{db::ListOptions, GrantWrite};
use wafer_core::{clients::database as db, interfaces::database::service::DatabaseService};
use wafer_run::{context::Context, ErrorCode, ResourceGrant, ResourceType, WaferError};

use crate::{
    db_read::{self, Bound},
    util::RecordExt,
};

pub const TABLE: &str = "impresspress__admin__wrap_grants";

/// One row of the wrap_grants table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapGrantRow {
    pub id: String,
    /// The block id being granted access, or `*` for every block.
    pub grantee: String,
    /// The table, storage path or other resource pattern being granted.
    pub resource: String,
    /// Stored in the integer column `write` (migration 001) as
    /// [`WRITE_READ`], [`WRITE_READ_WRITE`] or [`WRITE_APPEND`].
    pub write: GrantWrite,
    /// The stored wire value of the grant's [`ResourceType`] (its lowercase
    /// `Display` form: `db`, `config`, …); empty is the all-types wildcard.
    pub resource_type: String,
    pub description: String,
    pub created_at: String,
    pub updated_at: String,
}

impl WrapGrantRow {
    /// Decode one row. `grantee`, `resource` and `write` are required (all
    /// `NOT NULL`); a row without them is not a grant and is refused rather
    /// than defaulted, so a malformed row can never widen access. `write`
    /// is decoded by [`decode_write`]; a value it does not recognise refuses
    /// the row too.
    pub fn from_record(id: &str, data: &HashMap<String, Value>) -> Result<Self, String> {
        let grantee = data
            .opt_str_field("grantee")
            .ok_or_else(|| format!("{TABLE} row `{id}` has no grantee"))?;
        let resource = data
            .opt_str_field("resource")
            .ok_or_else(|| format!("{TABLE} row `{id}` has no resource"))?;
        let Some(write) = data.get("write") else {
            return Err(format!("{TABLE} row `{id}` has no write column"));
        };
        let write = decode_write(write)
            .ok_or_else(|| format!("{TABLE} row `{id}` has an unrecognised write value {write}"))?;
        Ok(Self {
            id: id.to_string(),
            grantee,
            resource,
            write,
            resource_type: data.str_field("resource_type").to_string(),
            description: data.str_field("description").to_string(),
            created_at: data.str_field("created_at").to_string(),
            updated_at: data.str_field("updated_at").to_string(),
        })
    }

    /// The column map this row inserts as.
    pub fn to_data(&self) -> HashMap<String, Value> {
        let mut data = HashMap::new();
        data.insert("id".to_string(), json!(self.id));
        data.insert("grantee".to_string(), json!(self.grantee));
        data.insert("resource".to_string(), json!(self.resource));
        data.insert("write".to_string(), json!(encode_write(self.write)));
        data.insert("resource_type".to_string(), json!(self.resource_type));
        data.insert("description".to_string(), json!(self.description));
        data.insert("created_at".to_string(), json!(self.created_at));
        data.insert("updated_at".to_string(), json!(self.updated_at));
        data
    }

    /// The runtime grant this row declares. An empty `resource_type` is the
    /// intentional all-types wildcard; a non-empty unrecognized value is a
    /// typo'd grant and is an error (fail-closed) rather than widened to the
    /// wildcard. So is a grant the runtime would refuse to install
    /// ([`ResourceGrant::check_shape`]: an append grant not typed `db`) —
    /// `Wafer::add_wrap_grants` rejects the whole set it is handed when one
    /// grant fails that check, so one bad row must not reach it.
    pub fn into_resource_grant(self) -> Result<ResourceGrant, String> {
        let resource_type = ResourceType::parse_stored(Some(&self.resource_type))
            .map_err(|e| format!("{TABLE} row `{}`: {e}", self.id))?;
        let grant = ResourceGrant {
            grantee: self.grantee,
            resource: self.resource,
            write: self.write,
            resource_type,
        };
        grant
            .check_shape()
            .map_err(|e| format!("{TABLE} row `{}`: {e}", self.id))?;
        Ok(grant)
    }
}

/// `write` column value of a read-only grant.
pub const WRITE_READ: i64 = 0;
/// `write` column value of a read-write grant.
pub const WRITE_READ_WRITE: i64 = 1;
/// `write` column value of an append-only grant.
pub const WRITE_APPEND: i64 = 2;

fn encode_write(write: GrantWrite) -> i64 {
    match write {
        GrantWrite::None => WRITE_READ,
        GrantWrite::Full => WRITE_READ_WRITE,
        GrantWrite::Append => WRITE_APPEND,
    }
}

/// The stored `write` value as a [`GrantWrite`]: the integer SQLite and
/// Postgres store, a bool, or either spelled as a string by a hand-built
/// fixture. `None` for anything else — the caller refuses the row rather
/// than guess which access it meant.
fn decode_write(value: &Value) -> Option<GrantWrite> {
    let code = match value {
        Value::Number(n) => n.as_i64()?,
        Value::Bool(b) => i64::from(*b),
        Value::String(s) => match s.as_str() {
            "true" => WRITE_READ_WRITE,
            "false" => WRITE_READ,
            other => other.parse().ok()?,
        },
        _ => return None,
    };
    match code {
        WRITE_READ => Some(GrantWrite::None),
        WRITE_READ_WRITE => Some(GrantWrite::Full),
        WRITE_APPEND => Some(GrantWrite::Append),
        _ => None,
    }
}

/// A grant to insert, as the permissions page's form supplies it.
#[derive(Debug, Clone)]
pub struct NewWrapGrant {
    pub grantee: String,
    pub resource: String,
    pub write: GrantWrite,
    pub resource_type: String,
    pub description: String,
}

impl NewWrapGrant {
    /// The row this becomes: a synthesised `wg_<uuid>` id and both
    /// timestamps set to now.
    pub fn into_row(self) -> WrapGrantRow {
        let now = crate::util::now_rfc3339();
        WrapGrantRow {
            id: format!("wg_{}", uuid::Uuid::new_v4()),
            grantee: self.grantee,
            resource: self.resource,
            write: self.write,
            resource_type: self.resource_type,
            description: self.description,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

fn decode_error(e: String) -> WaferError {
    WaferError::new(ErrorCode::Internal, e)
}

// ---------------------------------------------------------------------------
// Boot flavour: over `DatabaseService`, before WRAP.
// ---------------------------------------------------------------------------

/// Load the admin-created grants for a runtime build.
///
/// DB-service reader shared by every target (the native CLI, the Cloudflare
/// adapter's per-isolate cache and its `/_deploy/init`), so dynamic grants
/// are injected the same way everywhere. Missing table / read errors degrade
/// to an empty vec — dynamic grants are additive — and a row that does not
/// decode or names an unknown resource type is warned about and dropped,
/// never widened.
pub async fn load(db: &Arc<dyn DatabaseService>) -> Vec<ResourceGrant> {
    // Structured fresh-boot signal: on a first-ever deploy the table does
    // not exist yet (deploy-init builds the runtime BEFORE migrations run)
    // — that's expected and quiet. Any error after the existence check is a
    // real read failure and yields a grant-less, WRAP-denying runtime on
    // the Cloudflare path, so it warns.
    match db.schema_table_exists(TABLE).await {
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
        limit: Some(10_000),
        skip_count: true,
        ..Default::default()
    };
    let rows = match db.list(TABLE, &opts).await {
        Ok(list) => list.records,
        Err(e) => {
            tracing::warn!(error = %e, "wrap_grants read failed; dynamic grants skipped");
            return Vec::new();
        }
    };
    rows.into_iter()
        .filter_map(|r| {
            WrapGrantRow::from_record(&r.id, &r.data)
                .and_then(WrapGrantRow::into_resource_grant)
                .map_err(|e| tracing::warn!(error = %e, "wrap_grants row dropped"))
                .ok()
        })
        .collect()
}

/// The resource one test fixture grants over: a real table name on the wire,
/// which is why it is spelled here and not in the tests that use it.
#[cfg(test)]
pub(crate) const FIXTURE_RESOURCE: &str = "impresspress__files__objects";

/// Test-only: insert one admin-created grant over [`FIXTURE_RESOURCE`], the
/// row [`load`] is then expected to return. The caller applies the admin DDL
/// first.
///
/// It lives in this module rather than in each test that needs a seeded grant
/// because this module owns both the table name and that resource literal, and
/// `tests/repo_door.rs` is the gate that keeps it that way: the same three
/// lines written in `builder/boot.rs` would have to spell [`TABLE`] there,
/// which is exactly the bypass the door exists to catch.
#[cfg(test)]
pub(crate) async fn seed_fixture_grant(db: &Arc<dyn DatabaseService>) {
    let row = NewWrapGrant {
        grantee: "impresspress/files".to_string(),
        resource: FIXTURE_RESOURCE.to_string(),
        write: GrantWrite::Full,
        resource_type: "db".to_string(),
        description: String::new(),
    }
    .into_row();
    db.create(TABLE, row.to_data())
        .await
        .expect("seed fixture grant");
}

// ---------------------------------------------------------------------------
// Runtime flavour: over `Context`, under WRAP.
// ---------------------------------------------------------------------------

/// Every row, for the permissions page. A row that does not decode is an
/// error here rather than silently omitted: the page renders these as the
/// custom grants in force, and a listing that hides a row it cannot read
/// would misstate what the runtime loaded.
pub async fn list(ctx: &dyn Context) -> Result<Vec<WrapGrantRow>, WaferError> {
    let records = db_read::list_bounded(
        ctx,
        TABLE,
        vec![],
        Bound::Curated("custom WRAP grants are created by an operator in the admin UI"),
    )
    .await?;
    records
        .iter()
        .map(|r| WrapGrantRow::from_record(&r.id, &r.data).map_err(decode_error))
        .collect()
}

/// Insert a new grant and return it as stored.
pub async fn create(ctx: &dyn Context, new: NewWrapGrant) -> Result<WrapGrantRow, WaferError> {
    let row = new.into_row();
    let rec = db::create(ctx, TABLE, row.to_data()).await?;
    WrapGrantRow::from_record(&rec.id, &rec.data).map_err(decode_error)
}

/// Delete the grant with `id`. `NotFound` when there is none.
pub async fn delete(ctx: &dyn Context, id: &str) -> Result<(), WaferError> {
    db::delete(ctx, TABLE, id).await
}
#[cfg(test)]
mod tests {
    use wafer_run::ResourceType;

    use super::*;
    use crate::test_support::{FailingDbOpContext, TestContext};

    fn new_grant(resource_type: &str) -> NewWrapGrant {
        NewWrapGrant {
            grantee: "impresspress/files".to_string(),
            resource: "impresspress__foo__bar".to_string(),
            write: GrantWrite::Full,
            resource_type: resource_type.to_string(),
            description: "probe".to_string(),
        }
    }

    /// The codec: every column `create` writes comes back through `list`
    /// unchanged, `write` as a [`GrantWrite`] from the integer column.
    #[tokio::test]
    async fn create_and_list_round_trip_every_column() {
        let ctx = TestContext::with_admin().await;
        let created = create(&ctx, new_grant("db")).await.expect("create");
        assert!(created.id.starts_with("wg_"), "{}", created.id);
        assert_eq!(created.grantee, "impresspress/files");
        assert_eq!(created.resource, "impresspress__foo__bar");
        assert_eq!(created.write, GrantWrite::Full);
        assert_eq!(created.resource_type, "db");
        assert_eq!(created.description, "probe");
        assert!(!created.created_at.is_empty());
        assert_eq!(created.created_at, created.updated_at);

        let rows = list(&ctx).await.expect("list");
        assert_eq!(rows, vec![created.clone()]);

        let again = WrapGrantRow::from_record(&created.id, &created.to_data()).expect("decode");
        assert_eq!(again, created);

        let grant = created
            .into_resource_grant()
            .expect("a stored `db` type parses");
        assert_eq!(grant.grantee, "impresspress/files");
        assert_eq!(grant.resource, "impresspress__foo__bar");
        assert_eq!(grant.write, GrantWrite::Full);
        assert_eq!(grant.resource_type, Some(ResourceType::Db));
    }

    /// An append-only grant survives the table and becomes an append-only
    /// runtime grant — the column is an integer, not a bool, so it cannot
    /// collapse into read-only or read-write on the way through.
    #[tokio::test]
    async fn an_append_grant_round_trips_through_the_table() {
        let ctx = TestContext::with_admin().await;
        let created = create(
            &ctx,
            NewWrapGrant {
                write: GrantWrite::Append,
                ..new_grant("db")
            },
        )
        .await
        .expect("create");
        assert_eq!(created.to_data()["write"], serde_json::json!(WRITE_APPEND));
        let rows = list(&ctx).await.expect("list");
        assert_eq!(rows[0].write, GrantWrite::Append);
        let grant = rows[0]
            .clone()
            .into_resource_grant()
            .expect("a db append grant");
        assert_eq!(grant.write, GrantWrite::Append);
    }

    /// An append grant not typed `db` is one the runtime refuses to install,
    /// and `Wafer::add_wrap_grants` refuses the WHOLE set it is handed when
    /// one fails — so the row is refused here, where [`load`] drops it alone.
    #[tokio::test]
    async fn an_append_grant_not_typed_db_is_refused_as_a_row() {
        let ctx = TestContext::with_admin().await;
        for resource_type in ["", "storage"] {
            let created = create(
                &ctx,
                NewWrapGrant {
                    write: GrantWrite::Append,
                    ..new_grant(resource_type)
                },
            )
            .await
            .expect("create");
            let err = created
                .into_resource_grant()
                .expect_err("append grant not typed db");
            assert!(err.contains("append"), "{err}");
        }
    }

    /// An empty `resource_type` is the intentional all-types wildcard; a
    /// non-empty unrecognized value is a typo'd grant and is refused rather
    /// than widened to the wildcard.
    #[tokio::test]
    async fn resource_type_parses_wildcard_and_refuses_typos() {
        let ctx = TestContext::with_admin().await;
        let wildcard = create(&ctx, new_grant("")).await.expect("create");
        assert_eq!(
            wildcard
                .into_resource_grant()
                .expect("wildcard")
                .resource_type,
            None
        );
        let typo = create(&ctx, new_grant("databsae")).await.expect("create");
        assert!(typo.into_resource_grant().is_err());
    }

    #[tokio::test]
    async fn delete_removes_the_row_and_reports_a_missing_one() {
        let ctx = TestContext::with_admin().await;
        let created = create(&ctx, new_grant("db")).await.expect("create");
        delete(&ctx, &created.id).await.expect("delete");
        assert!(list(&ctx).await.expect("list").is_empty());
        let err = delete(&ctx, &created.id)
            .await
            .expect_err("deleting a gone row is NotFound");
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    /// The listing must not read as "no custom grants" on an outage: the
    /// permissions page renders whatever this returns.
    #[tokio::test]
    async fn list_surfaces_read_errors() {
        let ctx = TestContext::with_admin().await;
        create(&ctx, new_grant("db")).await.expect("create");
        let failing = FailingDbOpContext::new(ctx, vec![("database.list", TABLE)]);
        assert!(list(&failing).await.is_err());
    }

    /// `write` arrives as an integer from the database, or as a bool or a
    /// string from a hand-built fixture; a row without it, or with a value
    /// that names no access, is not a grant.
    #[test]
    fn write_decodes_from_every_backend_shape_and_is_required() {
        for (shape, want) in [
            (serde_json::json!(1), GrantWrite::Full),
            (serde_json::json!(0), GrantWrite::None),
            (serde_json::json!(2), GrantWrite::Append),
            (serde_json::json!(true), GrantWrite::Full),
            (serde_json::json!(false), GrantWrite::None),
            (serde_json::json!("1"), GrantWrite::Full),
            (serde_json::json!("2"), GrantWrite::Append),
            (serde_json::json!("true"), GrantWrite::Full),
        ] {
            let mut data = HashMap::new();
            data.insert("grantee".to_string(), serde_json::json!("a/b"));
            data.insert("resource".to_string(), serde_json::json!("a__b__c"));
            data.insert("write".to_string(), shape.clone());
            let row = WrapGrantRow::from_record("wg_1", &data).expect("decode");
            assert_eq!(row.write, want, "{shape}");
            assert_eq!(
                row.resource_type, "",
                "absent resource_type reads as the wildcard"
            );
        }
        for missing in ["grantee", "resource", "write"] {
            let mut data = HashMap::new();
            data.insert("grantee".to_string(), serde_json::json!("a/b"));
            data.insert("resource".to_string(), serde_json::json!("a__b__c"));
            data.insert("write".to_string(), serde_json::json!(1));
            data.remove(missing);
            let err = WrapGrantRow::from_record("wg_1", &data).expect_err(missing);
            assert!(err.contains(missing) && err.contains("wg_1"), "{err}");
        }
        for unknown in [
            serde_json::json!(3),
            serde_json::json!(-1),
            serde_json::json!("append"),
            serde_json::json!(null),
        ] {
            let mut data = HashMap::new();
            data.insert("grantee".to_string(), serde_json::json!("a/b"));
            data.insert("resource".to_string(), serde_json::json!("a__b__c"));
            data.insert("write".to_string(), unknown.clone());
            let err = WrapGrantRow::from_record("wg_1", &data).expect_err("unknown write");
            assert!(
                err.contains("write") && err.contains("wg_1"),
                "{unknown}: {err}"
            );
        }
    }
}

/// The boot flavour, over [`DatabaseService`]: the tests `boot.rs` carried
/// for the grant loader, against the moved name.
#[cfg(test)]
mod boot_tests {
    use wafer_block::db::Filter;
    use wafer_core::interfaces::database::service::{
        AggregateSpec, Column, DatabaseError, Record, RecordList, Table, UpsertSpec,
    };

    use super::*;

    /// Open a fresh in-memory SQLite [`DatabaseService`] with no migrations
    /// applied.
    async fn bare_db() -> Arc<dyn DatabaseService> {
        Arc::new(
            wafer_block_sqlite::service::SQLiteDatabaseService::open_in_memory()
                .expect("open in-memory sqlite"),
        )
    }

    async fn seed(
        db: &Arc<dyn DatabaseService>,
        grantee: &str,
        resource: &str,
        write: GrantWrite,
        resource_type: &str,
    ) {
        let row = NewWrapGrant {
            grantee: grantee.to_string(),
            resource: resource.to_string(),
            write,
            resource_type: resource_type.to_string(),
            description: String::new(),
        }
        .into_row();
        db.create(TABLE, row.to_data()).await.expect("seed grant");
    }

    #[tokio::test]
    async fn load_maps_rows_and_tolerates_missing_table() {
        let db = bare_db().await;

        // Missing table → empty, no error.
        assert!(load(&db).await.is_empty());

        // Apply admin migrations (creates the wrap_grants table among the
        // other admin tables) through the same pre-wafer DDL runner native's
        // `server.rs::run` uses — the migration-file-runner exception to the
        // no-raw-SQL rule (CLAUDE.md), reusing the real embedded schema.
        crate::migration_helper::apply_ddl_via_service(
            &db,
            crate::blocks::admin::migrations::ddl_files("sqlite"),
        )
        .await
        .expect("apply admin migrations");

        seed(
            &db,
            "impresspress/files",
            "impresspress__files__objects",
            GrantWrite::Full,
            "db",
        )
        .await;
        seed(&db, "wafer-run/auth", "bucket/x", GrantWrite::None, "").await;

        let grants = load(&db).await;
        assert_eq!(grants.len(), 2);
        let g1 = grants
            .iter()
            .find(|g| g.grantee == "impresspress/files")
            .unwrap();
        assert_eq!(g1.write, GrantWrite::Full);
        assert_eq!(g1.resource_type, Some(wafer_run::ResourceType::Db));
        let g2 = grants
            .iter()
            .find(|g| g.grantee == "wafer-run/auth")
            .unwrap();
        assert_eq!(g2.write, GrantWrite::None);
        assert_eq!(g2.resource_type, None);

        // Unrecognized resource_type → the ROW is dropped (fail-closed),
        // never widened to the all-types wildcard.
        seed(
            &db,
            "impresspress/products",
            "impresspress__products__items",
            GrantWrite::Full,
            "databsae",
        )
        .await;
        // Empty-string resource_type → kept as an intentional wildcard.
        seed(&db, "impresspress/files", "bucket/y", GrantWrite::None, "").await;
        // An append grant not typed `db` → the ROW is dropped, so the rest of
        // the set still reaches `Wafer::add_wrap_grants`, which would refuse
        // all of it over this one.
        seed(
            &db,
            "impresspress/legalpages",
            "impresspress__legalpages__docs",
            GrantWrite::Append,
            "",
        )
        .await;

        let grants = load(&db).await;
        assert_eq!(
            grants.len(),
            3,
            "typo'd resource_type and untyped append rows must be dropped"
        );
        assert!(grants.iter().all(|g| g.grantee != "impresspress/products"));
        assert!(grants
            .iter()
            .all(|g| g.grantee != "impresspress/legalpages"));
        let g4 = grants
            .iter()
            .find(|g| g.resource == "bucket/y")
            .expect("empty resource_type row kept");
        assert_eq!(g4.resource_type, None);
    }

    /// A [`DatabaseService`] whose existence check fails hard and whose every
    /// other method is [`unreachable!`]. Isolates the fail-closed `Err` arm of
    /// [`load`]: the only method it should reach is `schema_table_exists`, so
    /// a real read error there must short-circuit to an empty grant set
    /// without ever touching `list`.
    struct ErroringDb;

    #[async_trait::async_trait]
    impl DatabaseService for ErroringDb {
        async fn schema_table_exists(&self, _name: &str) -> Result<bool, DatabaseError> {
            Err(DatabaseError::Internal(
                "simulated wrap_grants existence-check failure".into(),
            ))
        }

        async fn schema_columns(&self, _table: &str) -> Result<Vec<String>, DatabaseError> {
            unreachable!()
        }

        async fn get(&self, _collection: &str, _id: &str) -> Result<Record, DatabaseError> {
            unreachable!("load must not read rows after an existence-check error")
        }

        async fn list(
            &self,
            _collection: &str,
            _opts: &ListOptions,
        ) -> Result<RecordList, DatabaseError> {
            unreachable!("load must not list rows after an existence-check error")
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

        async fn create_many(
            &self,
            _collection: &str,
            _rows: Vec<std::collections::HashMap<String, serde_json::Value>>,
        ) -> Result<i64, wafer_core::interfaces::database::service::DatabaseError> {
            unreachable!()
        }

        async fn take_where(
            &self,
            _collection: &str,
            _filters: &[Filter],
        ) -> Result<Vec<Record>, DatabaseError> {
            unreachable!()
        }

        async fn update_where(
            &self,
            _collection: &str,
            _filters: &[Filter],
            _data: HashMap<String, serde_json::Value>,
        ) -> Result<(), DatabaseError> {
            unreachable!()
        }

        async fn batch(
            &self,
            _ops: Vec<wafer_core::interfaces::database::service::WriteOp>,
        ) -> Result<
            Vec<wafer_core::interfaces::database::service::WriteOutcome>,
            wafer_core::interfaces::database::service::DatabaseError,
        > {
            unreachable!()
        }

        async fn insert_guarded(
            &self,
            _collection: &str,
            _data: std::collections::HashMap<String, serde_json::Value>,
            _guards: &[wafer_core::interfaces::database::service::CapGuard],
        ) -> Result<
            wafer_core::interfaces::database::service::GuardedInsert,
            wafer_core::interfaces::database::service::DatabaseError,
        > {
            unreachable!()
        }

        async fn update_guarded(
            &self,
            _collection: &str,
            _filters: &[wafer_block::db::Filter],
            _data: std::collections::HashMap<String, serde_json::Value>,
            _guards: &[wafer_core::interfaces::database::service::CapGuard],
        ) -> Result<
            wafer_core::interfaces::database::service::GuardedUpdate,
            wafer_core::interfaces::database::service::DatabaseError,
        > {
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
    async fn load_existence_check_error_fails_closed() {
        let db: Arc<dyn DatabaseService> = Arc::new(ErroringDb);
        assert!(
            load(&db).await.is_empty(),
            "a schema_table_exists error must degrade to an empty grant set"
        );
    }
}
