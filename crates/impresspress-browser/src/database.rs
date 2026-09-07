//! Browser-side `DatabaseService` backed by sql.js via the JS bridge.
//!
//! The browser backend implements only the [`DbExec`] execution *primitives*
//! (synchronous `bridge::db_query_raw` / `bridge::db_exec_raw`, marshaling
//! params/rows across the bridge as structured `JsValue`s via
//! `db_codec`/`serde_wasm_bindgen` — no JSON-string round trip) and then
//! decoding each row with the shared
//! [`wafer_core::interfaces::database::codec`], the same policy the native
//! SQLite and Cloudflare D1 backends decode with. All
//! `get/list/count/sum/create/update/delete` orchestration — filter/IN
//! expansion, sorted-key INSERT/UPDATE construction, lazy column-add,
//! table-exists guards — is inherited from the shared `wafer-core` [`DbExec`]
//! defaults, identical to `wafer-block-sqlite`, `wafer-block-postgres`, and the
//! Cloudflare D1 backend.
//!
//! Tables must already exist via the owning block's migration files (applied
//! at `lifecycle(Init)`); the shared `ensure_data_columns`/`ensure_query_columns`
//! add only missing *columns* (always `TEXT` on SQLite) on demand.
//!
//! ## OPFS flush durability contract
//!
//! `run_execute` (the `DbExec` primitive) does NOT flush to OPFS — it only
//! mutates sql.js's in-memory database. Flushing is done exactly once per
//! *logical* [`DatabaseService`] mutation, by [`BrowserDatabaseService::with_flush`],
//! which wraps every mutating `DatabaseService` method. A logical mutation
//! (e.g. `create`) may issue several SQL statements internally (a lazy
//! column-add ALTER, then the INSERT) — those all share the ONE flush at the
//! end of the call, instead of the previous behavior of flushing after every
//! single statement. See `with_flush`'s doc comment for the full contract,
//! including why the flush still happens when the wrapped operation itself
//! returns an error.

use std::collections::HashMap;

use wafer_block::db::{Filter, ListOptions};
use wafer_core::interfaces::database::{
    codec::{record_from_json_row, scalar_f64, scalar_i64},
    exec::DbExec,
    service::{
        AggregateSpec, Column, DatabaseError, DatabaseService, Record, RecordList, Table,
        UpsertSpec,
    },
};
use wafer_sql_utils::{introspect, Backend};

use crate::{bridge, db_codec};

/// Browser-side DatabaseService backed by sql.js via the JS bridge.
pub struct BrowserDatabaseService;

// SAFETY: `BrowserDatabaseService` is a unit struct with no shared state.
// wasm32-unknown-unknown has no threads, so the `Send`/`Sync` bounds
// required by `Arc<dyn DatabaseService>` are satisfied trivially — no
// cross-thread aliasing or data races are possible.
unsafe impl Send for BrowserDatabaseService {}
unsafe impl Sync for BrowserDatabaseService {}

impl BrowserDatabaseService {
    /// Run a mutating `op`, then flush the sql.js DB to OPFS exactly once —
    /// this is the coalescing point described in the module doc comment.
    ///
    /// Flushes even when `op` itself resolves to `Err`: the shared
    /// `DbExec` defaults can issue more than one statement per logical
    /// operation (e.g. `create`'s lazy column-add ALTER before its INSERT),
    /// so an operation that ultimately fails may still have mutated the
    /// in-memory sql.js DB. Skipping the flush in that case would silently
    /// discard an already-applied statement until some *later* mutation
    /// happens to flush it — an unnecessary, avoidable durability gap.
    ///
    /// Outcome precedence:
    /// - `op` succeeds, flush succeeds → `Ok` (the common case: durable).
    /// - `op` succeeds, flush fails → `Err` (the flush error). The mutation
    ///   is only sitting in memory at this point (quota exceeded, OPFS
    ///   permission revoked, etc.) — reporting success here would tell the
    ///   caller data is durable when a Service Worker eviction could lose
    ///   it, so this must surface as a failure.
    /// - `op` fails (regardless of flush outcome) → `Err` (the operation's
    ///   own error) — more specific/actionable than whatever the flush
    ///   attempt did; we still attempt the flush as a best-effort capture
    ///   of any partial writes the failed operation may have already made.
    async fn with_flush<T>(
        &self,
        op: impl std::future::Future<Output = Result<T, DatabaseError>>,
    ) -> Result<T, DatabaseError> {
        let result = op.await;
        let flush = bridge::dbFlush().await.map(|_| ()).map_err(|e| {
            DatabaseError::Internal(format!("flush to OPFS: {}", bridge::describe(&e)))
        });
        match (result, flush) {
            (Ok(v), Ok(())) => Ok(v),
            (Ok(_), Err(flush_err)) => Err(flush_err),
            (Err(op_err), _) => Err(op_err),
        }
    }

    /// Run `sql` and hand back the raw per-column JSON row objects sql.js
    /// resolved, undecoded.
    ///
    /// The only browser-specific step in a read is crossing the bridge; what
    /// a row *means* is [`wafer_core::interfaces::database::codec`]'s job, and
    /// every caller below feeds these rows straight into it. `bridge::
    /// db_query_raw` is synchronous, so this is not `async`.
    fn query_json_rows(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<Vec<serde_json::Value>, DatabaseError> {
        let params_js = db_codec::params_to_js(params).map_err(DatabaseError::Internal)?;
        let value = bridge::db_query_raw(sql, params_js)
            .map_err(|e| DatabaseError::Internal(format!("sql exec: {e:?}")))?;
        db_codec::rows_from_js(value).map_err(DatabaseError::Internal)
    }

    /// The first row of a single-row query (the scalar-aggregate shape), or
    /// `None` for an empty result — the argument shape
    /// [`scalar_i64`]/[`scalar_f64`] take.
    fn query_first_json_row(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<Option<serde_json::Value>, DatabaseError> {
        Ok(self.query_json_rows(sql, params)?.into_iter().next())
    }
}

// ─── DbExec primitives — the only backend-specific execution code ─────────────

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl DbExec for BrowserDatabaseService {
    const BACKEND: Backend = Backend::Sqlite;

    /// Decoding is [`record_from_json_row`], the one policy every SQL-family
    /// backend now shares — the private `db_codec::build_records` this
    /// replaced was the last of the three copies.
    ///
    /// **Behaviour difference, taken deliberately:** `build_records` returned
    /// `Err("expected row object")` for a row that was not a JSON object,
    /// where `record_from_json_row` returns an empty `Record`. The shared
    /// answer is the right one. The bridge cannot produce a non-object row —
    /// `bridge.js`'s `dbQueryRaw` builds every row from sql.js's column-name
    /// array, so the error arm was unreachable in production and only ever
    /// diverged the browser from the other two adapters. Where it *could*
    /// fire it is also the worse answer: it fails the whole query (every row,
    /// including the well-formed ones) with a message that names no table, no
    /// column and no row, and it makes one platform report a hard error for a
    /// shape the other two report as an empty record. A decode policy that
    /// three backends share is only worth anything if all three answer the
    /// same; keeping a fourth answer here is what unification is for.
    async fn run_fetch(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<Vec<Record>, DatabaseError> {
        Ok(self
            .query_json_rows(sql, params)?
            .into_iter()
            .map(record_from_json_row)
            .collect())
    }

    async fn run_fetch_one(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<Record, DatabaseError> {
        let records = self.run_fetch(sql, params).await?;
        records.into_iter().next().ok_or(DatabaseError::NotFound)
    }

    async fn run_execute(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<i64, DatabaseError> {
        let params_js = db_codec::params_to_js(params).map_err(DatabaseError::Internal)?;
        let rows_modified = bridge::db_exec_raw(sql, params_js)
            .map_err(|e| DatabaseError::Internal(format!("sql exec: {e:?}")))?;
        // NOTE: deliberately no `bridge::dbFlush()` here — flushing is
        // coalesced at the `DatabaseService` method boundary via
        // `with_flush`. See the module doc comment.
        Ok(rows_modified as i64)
    }

    async fn run_scalar_i64(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<i64, DatabaseError> {
        Ok(scalar_i64(self.query_first_json_row(sql, params)?))
    }

    async fn run_scalar_f64(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<f64, DatabaseError> {
        Ok(scalar_f64(self.query_first_json_row(sql, params)?))
    }

    async fn dbx_table_exists(&self, table: &str) -> Result<bool, DatabaseError> {
        let (sql, params) = introspect::build_table_exists(table, Backend::Sqlite);
        Ok(self.run_scalar_i64(&sql, &params).await? > 0)
    }
}

// ─── DatabaseService — forwards into the shared DbExec defaults ───────────────
//
// Every method that can mutate the sql.js DB wraps its `DbExec` default call
// in `with_flush` so exactly one OPFS flush happens per logical call,
// regardless of how many `run_execute` statements the shared default issued
// internally. Read-only methods (`get`/`list`/`count`/`sum`/`query_raw`/
// `aggregate`) forward directly — nothing to flush. `take_where` is a
// mutator (`DELETE ... RETURNING`) despite its read-shaped return value, so
// it is flushed like the other mutators below.

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl DatabaseService for BrowserDatabaseService {
    async fn get(&self, collection: &str, id: &str) -> Result<Record, DatabaseError> {
        DbExec::get(self, collection, id).await
    }

    async fn list(
        &self,
        collection: &str,
        opts: &ListOptions,
    ) -> Result<RecordList, DatabaseError> {
        DbExec::list(self, collection, opts).await
    }

    async fn create(
        &self,
        collection: &str,
        data: HashMap<String, serde_json::Value>,
    ) -> Result<Record, DatabaseError> {
        self.with_flush(DbExec::create(self, collection, data))
            .await
    }

    async fn update(
        &self,
        collection: &str,
        id: &str,
        data: HashMap<String, serde_json::Value>,
    ) -> Result<Record, DatabaseError> {
        self.with_flush(DbExec::update(self, collection, id, data))
            .await
    }

    async fn delete(&self, collection: &str, id: &str) -> Result<(), DatabaseError> {
        self.with_flush(DbExec::delete(self, collection, id)).await
    }

    async fn count(&self, collection: &str, filters: &[Filter]) -> Result<i64, DatabaseError> {
        DbExec::count(self, collection, filters).await
    }

    async fn sum(
        &self,
        collection: &str,
        field: &str,
        filters: &[Filter],
    ) -> Result<f64, DatabaseError> {
        DbExec::sum(self, collection, field, filters).await
    }

    async fn query_raw(
        &self,
        query: &str,
        args: &[serde_json::Value],
    ) -> Result<Vec<Record>, DatabaseError> {
        DbExec::query_raw(self, query, args).await
    }

    async fn exec_raw(
        &self,
        query: &str,
        args: &[serde_json::Value],
    ) -> Result<i64, DatabaseError> {
        self.with_flush(DbExec::exec_raw(self, query, args)).await
    }

    async fn delete_where(
        &self,
        collection: &str,
        filters: &[Filter],
    ) -> Result<(), DatabaseError> {
        self.with_flush(DbExec::delete_where(self, collection, filters))
            .await
    }

    async fn delete_where_count(
        &self,
        collection: &str,
        filters: &[Filter],
    ) -> Result<i64, DatabaseError> {
        self.with_flush(DbExec::delete_where_count(self, collection, filters))
            .await
    }

    async fn take_where(
        &self,
        collection: &str,
        filters: &[Filter],
    ) -> Result<Vec<Record>, DatabaseError> {
        self.with_flush(DbExec::take_where(self, collection, filters))
            .await
    }

    async fn update_where(
        &self,
        collection: &str,
        filters: &[Filter],
        data: HashMap<String, serde_json::Value>,
    ) -> Result<(), DatabaseError> {
        self.with_flush(DbExec::update_where(self, collection, filters, data))
            .await
    }

    async fn increment_field_where(
        &self,
        collection: &str,
        col: &str,
        delta: i64,
        filters: &[Filter],
    ) -> Result<i64, DatabaseError> {
        self.with_flush(DbExec::increment_field_where(
            self, collection, col, delta, filters,
        ))
        .await
    }

    async fn upsert(&self, collection: &str, spec: UpsertSpec) -> Result<i64, DatabaseError> {
        self.with_flush(DbExec::upsert(self, collection, spec))
            .await
    }

    async fn aggregate(
        &self,
        collection: &str,
        spec: AggregateSpec,
    ) -> Result<Vec<Record>, DatabaseError> {
        DbExec::aggregate(self, collection, spec).await
    }

    async fn update_where_count(
        &self,
        collection: &str,
        filters: &[Filter],
        data: HashMap<String, serde_json::Value>,
    ) -> Result<i64, DatabaseError> {
        self.with_flush(DbExec::update_where_count(self, collection, filters, data))
            .await
    }

    // --- Schema management ---

    async fn ensure_schema_table(&self, table: &Table) -> Result<(), DatabaseError> {
        self.with_flush(async {
            // Blocks own their schema via migration files; runtime callers
            // may still ask for a one-off table. Build the DDL via the
            // shared ddl builders and run it through the execution
            // primitive.
            let create = wafer_sql_utils::ddl::build_create_table(table, Backend::Sqlite)
                .map_err(|e| DatabaseError::Internal(format!("build create table: {e}")))?;
            self.run_execute(&create.sql, &[]).await?;

            let existing = DbExec::get_columns(self, &table.name).await?;
            for col in &table.columns {
                if !existing.contains(&col.name.to_lowercase()) {
                    let alter =
                        wafer_sql_utils::ddl::build_add_column(&table.name, col, Backend::Sqlite);
                    // `add_column_checked` (the shared `DbExec` default that
                    // every other backend's lazy column-add path already
                    // goes through — see `ensure_data_columns`) runs the
                    // ALTER and, only on failure, re-queries the table's
                    // actual columns: if `col` is now present, a concurrent
                    // writer raced us to add the same column and the
                    // failure is benign; otherwise the failure is real
                    // (quota exceeded, malformed DDL, an OPFS write error
                    // surfacing through `run_execute`, a flush error, …) and
                    // propagates instead of being silently swallowed like
                    // every `run_execute` error here used to be.
                    DbExec::add_column_checked(self, &table.name, &col.name, &alter).await?;
                }
            }

            for idx in &table.indexes {
                let stmt =
                    wafer_sql_utils::ddl::build_create_index(&table.name, idx, Backend::Sqlite)
                        .map_err(|e| DatabaseError::Internal(format!("build create index: {e}")))?;
                self.run_execute(&stmt.sql, &[]).await?;
            }
            for stmt in wafer_sql_utils::ddl::build_fk_indexes(table, Backend::Sqlite)
                .map_err(|e| DatabaseError::Internal(format!("build FK indexes: {e}")))?
            {
                self.run_execute(&stmt.sql, &[]).await?;
            }
            Ok(())
        })
        .await
    }

    async fn schema_table_exists(&self, name: &str) -> Result<bool, DatabaseError> {
        DbExec::schema_table_exists(self, name).await
    }

    async fn schema_drop_table(&self, name: &str) -> Result<(), DatabaseError> {
        self.with_flush(async {
            let stmt = wafer_sql_utils::ddl::build_drop_table(name, Backend::Sqlite);
            self.run_execute(&stmt.sql, &[]).await?;
            Ok(())
        })
        .await
    }

    async fn schema_add_column(&self, table: &str, column: &Column) -> Result<(), DatabaseError> {
        self.with_flush(async {
            let stmt = wafer_sql_utils::ddl::build_add_column(table, column, Backend::Sqlite);
            self.run_execute(&stmt.sql, &[]).await?;
            Ok(())
        })
        .await
    }
}

/// Factory: returns an `Arc<dyn DatabaseService>` backed by the
/// browser's sql.js + OPFS integration. Call after `crate::db_init()`
/// has completed.
pub fn make_database_service() -> std::sync::Arc<dyn DatabaseService> {
    std::sync::Arc::new(BrowserDatabaseService)
}

// ─── Shared DatabaseService conformance wiring ────────────────────────────────
//
// wafer-run #319 ships a backend-agnostic conformance suite
// (`wafer_core::interfaces::database::conformance::run_conformance`) that drives
// every `DatabaseService` op against a live service and asserts the concrete
// observable behavior (CRUD round-trips, the full `FilterOp` surface, sorted /
// paginated / projected / OR-grouped `list`, atomic `increment_field_where`,
// insert-then-update and windowed-counter `upsert`, grouped aggregates, raw
// SQL, schema management). It is the anti-drift mechanism: a `DatabaseService`
// impl that silently no-ops or fails-open on any single op fails an assertion
// instead of passing silently. Native SQLite and (gated) PostgreSQL already run
// it inside wafer-run; this module wires the browser adapter in.
//
// ── Coverage achieved here: COMPILE-TIME conformance (no live run) ──
//
// `_browser_adapter_is_conformable` typechecks — for the real, shipping
// `wasm32-unknown-unknown` target — that `BrowserDatabaseService` satisfies the
// exact `DatabaseService` surface `run_conformance` drives, and that the suite
// entry point exists under the enabled `conformance` feature. It is compiled by
// an unconditional wasm32 CI step —
// `cargo check --tests -p impresspress-browser --target wasm32-unknown-unknown`,
// in both `ci.yml` and `ci-main.yml`. The `--tests` flag is required: this
// `#[cfg(all(test, target_arch = "wasm32"))]` module needs the wasm32 dev-dep
// `conformance` feature, which a plain `cargo check` (no `--tests`) does not
// activate — so the plain check does NOT compile it. The dedicated
// `wasm-pack test --node crates/impresspress-browser` job also builds it, but is
// path-filtered to browser-crate changes, so the unconditional step above is
// what catches a wafer-run pin bump (root-only diff) that drifts the trait
// surface. If the trait surface drifts (a new required op, or a changed
// signature) or the suite entry is removed/renamed/re-gated, this stops
// compiling — surfacing the drift at the consumer rather than only inside
// wafer-run. The assertion is never
// executed and constructs no trait object, so it pulls in no sql.js/OPFS bridge
// call: it stays green under Node, where that bridge does not exist.
//
// ── Gap: a live behavioral run is not feasible in the current CI ──
//
// A real `run_conformance(&BrowserDatabaseService).await` needs the JS bridge
// this adapter is hardwired to (`crate::bridge` → `/js/bridge.js`) to be
// functional, which requires BOTH:
//   1. sql.js loaded — `bridge.js` STATICALLY imports the vendored ESM wrapper
//      `/vendor/sql-wasm-esm.js` (dynamic `import()` is forbidden in Service
//      Workers, so it cannot be lazified) plus `/vendor/sql-wasm.wasm`; and
//   2. OPFS — `dbInit` reads and `dbFlush` (invoked by every mutating
//      `DatabaseService` method via `with_flush`) writes the DB through
//      `navigator.storage.getDirectory()`.
// The CI job runs under Node (`wasm-pack test --node`), which has neither a
// server serving `/vendor/*` nor OPFS, so a live run is infeasible without new
// test infrastructure. Smallest change that would close the gap: a Node/headless
// test double for the `bridge` DB fns — sql.js instantiated in-memory (the
// vendored `sql-wasm.wasm` already lives at
// `crates/impresspress-bundle/assets/vendor/`) with `dbFlush` shimmed to a
// resolved no-op — then this module can call
// `run_conformance(&BrowserDatabaseService).await` under a
// `#[wasm_bindgen_test]` for a full behavioral run. The adapter itself needs no
// change; only the JS half is swapped for a memory-backed one in the test.
#[cfg(all(test, target_arch = "wasm32"))]
mod conformance {
    use wafer_core::interfaces::database::{
        conformance::run_conformance, service::DatabaseService,
    };

    use super::BrowserDatabaseService;

    /// Compile-time proof (never executed) that the browser adapter is a valid
    /// argument to the shared conformance suite. Typechecking the call — with
    /// the `&BrowserDatabaseService` → `&dyn DatabaseService` coercion
    /// `run_conformance` requires — is what enforces the trait-surface
    /// conformance; awaiting it here would need the sql.js/OPFS bridge, which
    /// the module doc explains is unavailable under `wasm-pack test --node`.
    #[allow(dead_code)]
    async fn _browser_adapter_is_conformable(svc: &BrowserDatabaseService) {
        run_conformance(svc as &dyn DatabaseService).await;
    }
}

/// The row-decode policy this adapter now shares with native SQLite and
/// Cloudflare D1. `database.rs` is wasm32-only, so these run under
/// `wasm-pack test --node`; the codec itself is pure and needs no bridge.
#[cfg(all(test, target_arch = "wasm32"))]
mod codec_policy {
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::{record_from_json_row, scalar_f64, scalar_i64};

    /// sql.js stores JSON columns as TEXT; the shared codec restores the
    /// structure the writer put in, so a block reading this column sees a
    /// `Value::Object` on all three adapters.
    #[wasm_bindgen_test]
    fn json_text_columns_are_reparsed() {
        let rec = record_from_json_row(
            serde_json::json!({"id": "1", "meta": "{\"k\":\"v\"}", "tags": "[1,2]"}),
        );
        assert_eq!(rec.id, "1");
        assert_eq!(rec.data.get("meta").unwrap(), &serde_json::json!({"k":"v"}));
        assert_eq!(rec.data.get("tags").unwrap(), &serde_json::json!([1, 2]));
    }

    /// A plain string that does not look like JSON stays a string, and one
    /// that looks like JSON but does not parse stays a string too.
    #[wasm_bindgen_test]
    fn non_json_text_is_left_alone() {
        let rec = record_from_json_row(
            serde_json::json!({"id": "1", "note": "hello world", "broken": "{not json"}),
        );
        assert_eq!(
            rec.data.get("note").unwrap(),
            &serde_json::json!("hello world")
        );
        assert_eq!(
            rec.data.get("broken").unwrap(),
            &serde_json::json!("{not json")
        );
    }

    /// An integer primary key is stringified into `Record::id` and kept in
    /// `data` — the row decoders in block repositories read the whole column
    /// map.
    #[wasm_bindgen_test]
    fn numeric_id_is_stringified_and_retained() {
        let rec = record_from_json_row(serde_json::json!({"id": 7, "v": "x"}));
        assert_eq!(rec.id, "7");
        assert_eq!(rec.data.get("id").unwrap(), &serde_json::json!(7));
    }

    /// The one behaviour the private copy did differently: a non-object row.
    /// `db_codec::build_records` returned `Err("expected row object")`, which
    /// failed the whole query on one platform for a shape the other two
    /// report as an empty record. See `run_fetch`'s doc for why the shared
    /// answer wins. This fails against the pre-unification tree.
    #[wasm_bindgen_test]
    fn a_non_object_row_is_an_empty_record_not_an_error() {
        let rec = record_from_json_row(serde_json::json!(42));
        assert_eq!(rec.id, "");
        assert!(rec.data.is_empty());
    }

    /// `SELECT COUNT(*) AS "cnt"` — the shared builders alias their scalar
    /// column themselves, so the scalar accessors must not look it up by name.
    #[wasm_bindgen_test]
    fn scalars_read_the_aliased_aggregate_column() {
        assert_eq!(scalar_i64(Some(serde_json::json!({"cnt": 5}))), 5);
        assert!((scalar_f64(Some(serde_json::json!({"total": 12.5}))) - 12.5).abs() < f64::EPSILON);
    }

    /// An absent row counts as zero — the same answer the SQL aggregate gives
    /// for an empty table, so "no row" can never be mistaken for a count.
    #[wasm_bindgen_test]
    fn an_absent_scalar_row_is_zero() {
        assert_eq!(scalar_i64(None), 0);
        assert!(scalar_f64(None).abs() < f64::EPSILON);
    }
}
