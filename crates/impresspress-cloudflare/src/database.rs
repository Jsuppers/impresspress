//! Async database service backed by Cloudflare D1 (SQLite at the edge).
//!
//! D1 implements only the [`DbExec`] execution *primitives* (prepare/bind/run
//! via [`json_value_to_js`]); all `get/list/count/sum/create/update/delete`
//! orchestration — filter/IN expansion, sorted-key INSERT/UPDATE construction,
//! lazy column-add, table-exists guards — is inherited from the shared
//! `wafer-core` [`DbExec`] defaults, identical to `wafer-block-sqlite` and
//! `wafer-block-postgres`. The `DatabaseService` impl forwards each method into
//! the matching `DbExec` default.
//!
//! ## Lazy column-add
//!
//! Tables themselves must exist before any `create()` — every block ships
//! explicit `migrations/*.sql` applied from the `Init` lifecycle. The shared
//! `DbExec::ensure_data_columns`/`ensure_query_columns` add only *columns* on
//! demand (always `TEXT` on SQLite), matching the native sqlite/postgres
//! backends. Reads against a missing table return empty/NotFound via the
//! `dbx_table_exists` guard the defaults run first.
//!
//! ## Schema cache + STRICT_SCHEMA (wafer-run #313)
//!
//! Each logical op the shared executor runs is fronted by schema introspection
//! — a table-exists check plus, on the lazy-column paths, a column-list query.
//! On D1 each is a *network* round-trip that dwarfs the data query. This
//! adapter therefore opts into the two `DbExec` accessors #313 added:
//!
//! - [`schema_cache`](DbExec::schema_cache) returns a per-isolate
//!   [`SchemaCache`], so a warm backend memoizes those introspection facts and
//!   issues zero introspection round-trips in steady state. The shared defaults
//!   own invalidation (lazy `ALTER TABLE`, `exec_raw`/DDL) — D1's own
//!   schema-*mutation* methods never run on the live path (schema is
//!   migration-owned; a runtime mutation attempt is an explicit error, see the
//!   `DatabaseService` impl) and so touch no cache.
//! - [`strict_schema`](DbExec::strict_schema) reads a flag seeded at
//!   construction from the deploy's `WAFER_RUN__DATABASE__STRICT_SCHEMA` var
//!   (see [`D1DatabaseService::new`]) and re-applied at lifecycle `Init` via
//!   [`set_strict_schema`](DatabaseService::set_strict_schema) for the one
//!   service a Wafer runtime is built around (the shared `wafer-run/database`
//!   handler reads the same var from `ctx.config_get`). When set the executor
//!   trusts the migrated schema: no table-exists probe, no lazy column-add.
//!   Production CF deploys enable it (wrangler `[vars]`); a write/query
//!   referencing an unmigrated column then fails loudly, as intended.
//!
//!   One introspection survives strict mode: a sorted or paged `list` ends its
//!   `ORDER BY` with the table's primary key, which the executor reads with a
//!   `pragma_table_info` the first time it lists that table
//!   ([`DbExec::get_primary_key`]) and memoizes in the schema cache (plus one
//!   table-exists probe for a table with no key). A warm isolate pays nothing;
//!   a request-scoped handle (below) pays it once per table it lists sorted.
//!
//!   Seeding at construction is what covers the D1 services that never reach
//!   an `Init`: the request-log drain's batch handle, built per request inside
//!   `run_with_config` and used from `ctx.wait_until`, and the handle
//!   `build_runtime` reads `block_settings` through before a runtime exists.
//!   Each is discarded with the request, so its `SchemaCache` is always cold —
//!   a drain-only site with strict off pays one `pragma_table_info` per insert
//!   batch forever, which is the cost this seeding removes.

use std::sync::atomic::{AtomicBool, Ordering};

use wafer_block::db::{Filter, ListOptions};
use wafer_core::interfaces::database::{
    codec::{record_from_json_row, scalar_f64, scalar_i64},
    exec::{BatchOp, BatchResult, DbExec},
    mint_record_id,
    schema_cache::SchemaCache,
    service::{
        AggregateSpec, Column, DatabaseError, DatabaseService, Record, RecordList, Table,
        UpsertSpec,
    },
};
use wafer_sql_utils::{introspect, Backend};
use wasm_bindgen::JsValue;
use worker::*;

/// Async database service wrapping Cloudflare D1.
pub struct D1DatabaseService {
    db: D1Database,
    /// Memoized table-exists / column-list / primary-key facts (see
    /// [`SchemaCache`]). Consulted by the shared executor before introspection
    /// and invalidated by it on every schema mutation. While `strict_schema`
    /// is set only the primary key a sorted or paged `list` orders by is
    /// looked up, so that is all the cache holds then.
    schema_cache: SchemaCache,
    /// STRICT_SCHEMA flag. Seeded at construction from the deploy's
    /// `WAFER_RUN__DATABASE__STRICT_SCHEMA` var, and re-applied at lifecycle
    /// `Init` via [`DatabaseService::set_strict_schema`] for the one service a
    /// Wafer runtime is built around. When set, the shared executor skips the
    /// table-exists probe and the lazy column-add; a sorted or paged `list`
    /// still looks up the table's primary key. `AtomicBool` (not `Cell`) so
    /// the struct keeps the `Sync` bound `Arc<dyn DatabaseService>` needs; on
    /// wasm32's single thread the ordering is immaterial.
    strict_schema: AtomicBool,
}

impl D1DatabaseService {
    /// Wrap a D1 binding, with this deploy's STRICT_SCHEMA verdict already
    /// applied.
    ///
    /// `strict_schema` is a parameter rather than a `false` default a caller
    /// may later overwrite because not every D1 service reaches a lifecycle
    /// `Init`: the request-log drain handle and `build_runtime`'s pre-`Init`
    /// `block_settings` read are both constructed outside any runtime, and a
    /// default would silently put them on the always-introspect path. Callers
    /// get the verdict from
    /// [`CfEnvironment::strict_schema_enabled`](crate::environment::CfEnvironment::strict_schema_enabled).
    ///
    /// `Init` still calls [`DatabaseService::set_strict_schema`] on the
    /// runtime's own service; it reads the same var through `ctx.config_get`,
    /// so it re-affirms this value rather than contradicting it.
    pub fn new(db: D1Database, strict_schema: bool) -> Self {
        Self {
            db,
            schema_cache: SchemaCache::new(),
            strict_schema: AtomicBool::new(strict_schema),
        }
    }

    /// Bind `params` (the JSON form produced by `sea_values_to_json`) to a
    /// prepared statement, mapping each value to a `JsValue` at the edge.
    fn prepare_bind(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<D1PreparedStatement, DatabaseError> {
        let js_params: Vec<JsValue> = params.iter().map(json_value_to_js).collect();
        self.db.prepare(sql).bind(&js_params).map_err(db_err)
    }

    /// Batch-insert `rows` into `collection` via D1's native `batch()` API —
    /// one D1 round trip for the whole set instead of one `create()` (one
    /// prepare+run each) per row. Used by the Cloudflare audit-log drain
    /// (`lib.rs::run`'s post-dispatch `waitUntil`), which previously issued
    /// one `create()` per queued `request_logs` row — see "Batch audit-log
    /// persistence".
    ///
    /// Every row must resolve to the identical column set after stamping
    /// (audit-log rows always do — `pipeline.rs` builds them from the same
    /// fixed field list) since this method plans one INSERT *shape* for the
    /// whole batch rather than re-planning per row; a row with a different
    /// column set is rejected rather than silently producing a
    /// short/misaligned INSERT.
    ///
    /// Mirrors `DbExec::create`'s per-row policy — mints the `id` with
    /// wafer's [`mint_record_id`] (a UUIDv7, so a batch's ids sort in the
    /// order its rows were queued and a `created_at` tie lists them in that
    /// order) when absent (D1 never overrides `table_autogenerates_id`, so
    /// ids are always supplied by the caller or minted here) and stamps
    /// `created_at`/`updated_at` when absent (see [`prepare_batch_rows`]) —
    /// but adds missing columns only ONCE for
    /// the whole batch (via the first row, which is representative since
    /// every row shares the same shape) rather than per row: the audit-log
    /// table's schema is migration-owned, so this is a safety net, not the
    /// steady-state path.
    pub async fn create_many(
        &self,
        collection: &str,
        rows: Vec<std::collections::HashMap<String, serde_json::Value>>,
    ) -> Result<i64, DatabaseError> {
        if rows.is_empty() {
            return Ok(0);
        }
        let table = wafer_sql_utils::ident::sanitize_ident(collection);

        let prepared = prepare_batch_rows(rows)?;

        // Lazy column-add once (request_logs' schema is migration-owned;
        // this is a safety net, not the steady-state path) — every row has
        // the same shape, so the first is representative.
        if let Some(first) = prepared.first() {
            let sample: std::collections::HashMap<String, serde_json::Value> =
                first.iter().cloned().collect();
            DbExec::ensure_data_columns(self, &table, &sample).await?;
        }

        let mut statements = Vec::with_capacity(prepared.len());
        for pairs in &prepared {
            // D1 is always SQLite — same dialect `DbExec::BACKEND` declares
            // for this backend below.
            let stmt = wafer_sql_utils::query::build_insert(&table, pairs, Backend::Sqlite);
            statements.push(self.prepare_bind(
                &stmt.sql,
                &wafer_sql_utils::value::sea_values_to_json(stmt.values),
            )?);
        }

        let results = self.db.batch(statements).await.map_err(db_err)?;
        for r in &results {
            if !r.success() {
                return Err(DatabaseError::Internal(format!(
                    "batch insert into {collection}: {}",
                    r.error().unwrap_or_else(|| "unknown error".to_string())
                )));
            }
        }
        Ok(results.len() as i64)
    }
}

/// Stamp and shape the rows of one [`D1DatabaseService::create_many`] batch:
/// mint a missing `id` with [`mint_record_id`], stamp a missing
/// `created_at`/`updated_at`, and turn each row into sanitized, column-sorted
/// `(column, value)` pairs. Every row must end up with the same column set,
/// because the batch runs one INSERT shape; a row that differs is an error.
///
/// Ids are minted in row order, so the batch's keys ascend in the order its
/// rows were queued.
fn prepare_batch_rows(
    rows: Vec<std::collections::HashMap<String, serde_json::Value>>,
) -> Result<Vec<Vec<(String, serde_json::Value)>>, DatabaseError> {
    let mut prepared: Vec<Vec<(String, serde_json::Value)>> = Vec::with_capacity(rows.len());
    let mut shape: Option<Vec<String>> = None;
    for mut data in rows {
        if !data.contains_key("id") {
            data.insert(
                "id".to_string(),
                serde_json::Value::String(mint_record_id()),
            );
        }
        let now = chrono::Utc::now().to_rfc3339();
        data.entry("created_at".to_string())
            .or_insert_with(|| serde_json::Value::String(now.clone()));
        data.entry("updated_at".to_string())
            .or_insert_with(|| serde_json::Value::String(now));

        let mut pairs: Vec<(String, serde_json::Value)> = data
            .into_iter()
            .map(|(k, v)| (wafer_sql_utils::ident::sanitize_ident(&k), v))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));

        let cols: Vec<String> = pairs.iter().map(|(k, _)| k.clone()).collect();
        match &shape {
            None => shape = Some(cols),
            Some(expected) if expected == &cols => {}
            Some(_) => {
                return Err(DatabaseError::Internal(
                    "create_many requires every row to share the same column set".into(),
                ));
            }
        }
        prepared.push(pairs);
    }
    Ok(prepared)
}

// SAFETY: `D1DatabaseService` holds a `D1Database` handle scoped to a single
// Worker isolate. wasm32-unknown-unknown has no threads, so the
// `Send`/`Sync` bounds required by `Arc<dyn DatabaseService>` are satisfied
// trivially — no cross-thread aliasing or data races can occur. The added
// `schema_cache` (`parking_lot::RwLock`) and `strict_schema` (`AtomicBool`)
// fields are themselves `Send + Sync`; the `unsafe impl` remains required
// only because of the `!Send` `D1Database` handle.
unsafe impl Send for D1DatabaseService {}
unsafe impl Sync for D1DatabaseService {}

// ---------------------------------------------------------------------------
// DbExec primitives — the only backend-specific execution code.
// ---------------------------------------------------------------------------

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl DbExec for D1DatabaseService {
    const BACKEND: Backend = Backend::Sqlite;

    fn schema_cache(&self) -> Option<&SchemaCache> {
        Some(&self.schema_cache)
    }

    fn strict_schema(&self) -> bool {
        self.strict_schema.load(Ordering::Relaxed)
    }

    async fn run_fetch(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<Vec<Record>, DatabaseError> {
        let stmt = self.prepare_bind(sql, params)?;
        let results = stmt.all().await.map_err(db_err)?;
        let rows: Vec<serde_json::Value> = results.results().map_err(db_err)?;
        Ok(rows.into_iter().map(record_from_json_row).collect())
    }

    async fn run_fetch_one(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<Record, DatabaseError> {
        let stmt = self.prepare_bind(sql, params)?;
        let row = match stmt.first::<serde_json::Value>(None).await {
            Ok(row) => row,
            // A `get`-by-id against a not-yet-created table is "not found",
            // matching the native backends' `QueryReturnedNoRows` mapping.
            Err(e) if is_no_such_table(&e.to_string()) => return Err(DatabaseError::NotFound),
            Err(e) => return Err(db_err(e)),
        };
        row.map(record_from_json_row).ok_or(DatabaseError::NotFound)
    }

    async fn run_execute(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<i64, DatabaseError> {
        let result = self
            .prepare_bind(sql, params)?
            .run()
            .await
            .map_err(db_err)?;
        // worker-rs 0.7 exposes D1Result::meta().changes (Option<usize>) for
        // mutations — surface a real rows_affected so the shared defaults can
        // map 0-rows to NotFound on update/delete-by-id.
        let changes = result
            .meta()
            .map_err(db_err)?
            .and_then(|m| m.changes)
            .unwrap_or(0);
        Ok(changes as i64)
    }

    /// Delegates to [`run_fetch`](Self::run_fetch): a D1 binding is one
    /// handle, and every statement this adapter issues goes through
    /// `db.prepare()` on it — there is no reader/writer split for a
    /// `DELETE … RETURNING` to land on the wrong side of, and
    /// `D1PreparedStatement::all()` applies a statement's side effects just
    /// as `run()` does while also handing back the `RETURNING` rows. (This
    /// adapter uses no D1 Sessions API, so no read-replica routing exists
    /// here either.) Delegating rather than repeating `prepare_bind` +
    /// `all()` + `record_from_json_row` keeps the two decode paths identical
    /// by construction.
    ///
    /// It stays a distinct trait method rather than riding on `run_fetch`
    /// at the call site because the *contract* differs — a statement with
    /// side effects that also returns rows — and the shared
    /// [`DbExec::take_where`] now routes through it: the day this adapter
    /// grows read-replica routing, only this delegation changes.
    async fn run_execute_returning(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<Vec<Record>, DatabaseError> {
        self.run_fetch(sql, params).await
    }

    async fn run_scalar_i64(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<i64, DatabaseError> {
        let stmt = self.prepare_bind(sql, params)?;
        let row = stmt
            .first::<serde_json::Value>(None)
            .await
            .map_err(db_err)?;
        Ok(scalar_i64(row))
    }

    async fn run_scalar_f64(
        &self,
        sql: &str,
        params: &[serde_json::Value],
    ) -> Result<f64, DatabaseError> {
        let stmt = self.prepare_bind(sql, params)?;
        let row = stmt
            .first::<serde_json::Value>(None)
            .await
            .map_err(db_err)?;
        Ok(scalar_f64(row))
    }

    async fn dbx_table_exists(&self, table: &str) -> Result<bool, DatabaseError> {
        let (sql, params) = introspect::build_table_exists(table, Backend::Sqlite);
        Ok(self.run_scalar_i64(&sql, &params).await? > 0)
    }

    /// Collapse `ops` into ONE native D1 `db.batch()` round-trip.
    ///
    /// The shared `DbExec` default issues each op through its own single-
    /// statement primitive — on D1 that is N separate network round-trips.
    /// This override prepares every op's `(sql, params)` uniformly (the same
    /// `prepare_bind` the primitives use, so binding is byte-identical) and
    /// submits them as a single `db.batch()`, exactly like [`create_many`].
    /// D1 returns one [`D1Result`] per statement **in submission order**
    /// (worker-rs `D1Database::batch` documents this), so each result is
    /// decoded positionally into the [`BatchResult`] variant its op names,
    /// reusing the very helpers the single-statement primitives use:
    ///
    /// - [`BatchOp::Rows`] → `results()` → [`record_from_json_row`] per row (as
    ///   [`run_fetch`](DbExec::run_fetch)).
    /// - [`BatchOp::FetchOne`] → first of `results()` → `record_from_json_row`,
    ///   empty ⇒ [`DatabaseError::NotFound`] (as
    ///   [`run_fetch_one`](DbExec::run_fetch_one)).
    /// - [`BatchOp::Execute`] → `meta().changes` (as
    ///   [`run_execute`](DbExec::run_execute)).
    /// - [`BatchOp::ScalarI64`] → first of `results()` → [`scalar_i64`] (as
    ///   [`run_scalar_i64`](DbExec::run_scalar_i64)).
    /// - [`BatchOp::ScalarF64`] → first of `results()` → [`scalar_f64`] (as
    ///   [`run_scalar_f64`](DbExec::run_scalar_f64)).
    ///
    /// Note `results()` yields the same first row object `first()` returns, so
    /// the scalar/fetch-one decode matches the primitives byte-for-byte.
    ///
    /// **Transactional vs. sequential (all-or-nothing).** D1 `batch()` runs
    /// the statements sequentially inside one implicit transaction: it stops
    /// at the first failing statement and rolls the whole batch back, and the
    /// `batch()` promise rejects — so a failure surfaces here as the outer
    /// `Err` (the whole call fails; results below are only reached when every
    /// statement succeeded). This preserves the sequential default's
    /// first-error identity (the first failing statement's error, later ops
    /// not observed) but is **stricter**: the sequential default leaves an
    /// *earlier* successful statement's side effects committed, whereas the
    /// batch rolls them back. For the read-only `list` count+select batch that
    /// is purely a consistency win (both statements see one snapshot); for
    /// `update`'s UPDATE+re-fetch it means a failing re-fetch would also
    /// undo the UPDATE — a failure that cannot occur on the happy path (a
    /// well-formed by-id SELECT against the just-updated table), and rolling
    /// back rather than half-applying is the safe direction regardless.
    async fn run_batch(&self, ops: &[BatchOp<'_>]) -> Result<Vec<BatchResult>, DatabaseError> {
        // An empty batch is a no-op; `db.batch(vec![])` has nothing to submit.
        // Matches the sequential default (which pushes nothing).
        if ops.is_empty() {
            return Ok(Vec::new());
        }

        // Prepare + bind every statement, then submit as ONE round-trip.
        let mut statements = Vec::with_capacity(ops.len());
        for op in ops {
            let (sql, params) = op.sql_params();
            statements.push(self.prepare_bind(sql, params)?);
        }
        let results = self.db.batch(statements).await.map_err(db_err)?;

        // D1 returns exactly one result per submitted statement, in order. A
        // length mismatch would break positional decoding — surface it rather
        // than silently mis-aligning results with ops.
        if results.len() != ops.len() {
            return Err(DatabaseError::Internal(format!(
                "D1 batch returned {} results for {} statements",
                results.len(),
                ops.len()
            )));
        }

        let mut out = Vec::with_capacity(ops.len());
        for (op, result) in ops.iter().zip(results.iter()) {
            // A transactional batch rejects (the `Err` above) on any statement
            // failure, so a non-success result here is unexpected; guard it
            // like `create_many` and surface D1's own error text rather than
            // decode a failed statement.
            if !result.success() {
                return Err(DatabaseError::Internal(format!(
                    "D1 batch statement failed: {}",
                    result
                        .error()
                        .unwrap_or_else(|| "unknown error".to_string())
                )));
            }
            let decoded = match op {
                BatchOp::Rows { .. } => {
                    let rows: Vec<serde_json::Value> = result.results().map_err(db_err)?;
                    BatchResult::Rows(rows.into_iter().map(record_from_json_row).collect())
                }
                BatchOp::FetchOne { .. } => {
                    let rows: Vec<serde_json::Value> = result.results().map_err(db_err)?;
                    let row = rows.into_iter().next().ok_or(DatabaseError::NotFound)?;
                    BatchResult::FetchOne(record_from_json_row(row))
                }
                BatchOp::Execute { .. } => {
                    // Same source as `run_execute`: D1Result meta's `changes`.
                    let changes = result
                        .meta()
                        .map_err(db_err)?
                        .and_then(|m| m.changes)
                        .unwrap_or(0);
                    BatchResult::Execute(changes as i64)
                }
                BatchOp::ScalarI64 { .. } => {
                    let rows: Vec<serde_json::Value> = result.results().map_err(db_err)?;
                    BatchResult::ScalarI64(scalar_i64(rows.into_iter().next()))
                }
                BatchOp::ScalarF64 { .. } => {
                    let rows: Vec<serde_json::Value> = result.results().map_err(db_err)?;
                    BatchResult::ScalarF64(scalar_f64(rows.into_iter().next()))
                }
            };
            out.push(decoded);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// DatabaseService — forwards into the shared DbExec defaults.
// ---------------------------------------------------------------------------

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl DatabaseService for D1DatabaseService {
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
        data: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<Record, DatabaseError> {
        DbExec::create(self, collection, data).await
    }

    async fn update(
        &self,
        collection: &str,
        id: &str,
        data: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<Record, DatabaseError> {
        DbExec::update(self, collection, id, data).await
    }

    async fn delete(&self, collection: &str, id: &str) -> Result<(), DatabaseError> {
        DbExec::delete(self, collection, id).await
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
        DbExec::exec_raw(self, query, args).await
    }

    async fn delete_where(
        &self,
        collection: &str,
        filters: &[Filter],
    ) -> Result<(), DatabaseError> {
        DbExec::delete_where(self, collection, filters).await
    }

    async fn delete_where_count(
        &self,
        collection: &str,
        filters: &[Filter],
    ) -> Result<i64, DatabaseError> {
        DbExec::delete_where_count(self, collection, filters).await
    }

    async fn take_where(
        &self,
        collection: &str,
        filters: &[Filter],
    ) -> Result<Vec<Record>, DatabaseError> {
        DbExec::take_where(self, collection, filters).await
    }

    async fn update_where(
        &self,
        collection: &str,
        filters: &[Filter],
        data: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<(), DatabaseError> {
        DbExec::update_where(self, collection, filters, data).await
    }

    async fn increment_field_where(
        &self,
        collection: &str,
        col: &str,
        delta: i64,
        filters: &[Filter],
    ) -> Result<i64, DatabaseError> {
        DbExec::increment_field_where(self, collection, col, delta, filters).await
    }

    async fn upsert(&self, collection: &str, spec: UpsertSpec) -> Result<i64, DatabaseError> {
        DbExec::upsert(self, collection, spec).await
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
        data: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<i64, DatabaseError> {
        DbExec::update_where_count(self, collection, filters, data).await
    }

    // --- Schema management: D1 schema is migration-owned ---
    //
    // D1's schema is established *exclusively* by each block's `migrations/*.sql`,
    // applied at lifecycle `Init` through `db::ddl` (the host DDL op → `run_execute`)
    // and gated by the migration-bless / `IMPRESSPRESS_RUN_MIGRATIONS` workflow.
    // The runtime `DatabaseService` schema-*mutation* methods below are therefore
    // never part of the live D1 path:
    //
    // - `ensure_schema_table` runs only from `handler::handle_lifecycle`, and only
    //   when the `wafer-run/database` block is registered *with* a non-empty table
    //   set. Impresspress always registers it via `service_blocks::database::register_with`
    //   (empty `tables`), so `handle_lifecycle` takes its `tables.is_empty()` branch
    //   and never calls this.
    // - `schema_add_column` / `schema_drop_table` have no production caller at all.
    //   The shared `DbExec` lazy column-add issues its `ALTER TABLE ADD COLUMN` via
    //   `run_execute` (see `DbExec::add_column_checked`), not this method, and it is
    //   disabled outright under STRICT_SCHEMA (which live CF deploys set).
    //
    // Returning `Ok(())` here would *claim* a mutation happened when nothing did — a
    // silent success that only surfaces later as a confusing "no such table" / "no
    // such column" from the next query. Instead each returns an explicit error so a
    // mistaken runtime schema mutation on D1 fails loudly and names the fix (edit the
    // block's migration files). `schema_table_exists` is a read and stays live.

    async fn ensure_schema_table(&self, table: &Table) -> Result<(), DatabaseError> {
        Err(schema_mutation_unsupported(
            "ensure_schema_table",
            &table.name,
        ))
    }

    async fn schema_table_exists(&self, name: &str) -> Result<bool, DatabaseError> {
        DbExec::schema_table_exists(self, name).await
    }

    async fn schema_drop_table(&self, name: &str) -> Result<(), DatabaseError> {
        Err(schema_mutation_unsupported("schema_drop_table", name))
    }

    async fn schema_add_column(&self, table: &str, _column: &Column) -> Result<(), DatabaseError> {
        Err(schema_mutation_unsupported("schema_add_column", table))
    }

    fn set_strict_schema(&self, enabled: bool) {
        self.strict_schema.store(enabled, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a serde_json::Value param to a JsValue for D1 binding. Arrays and
/// objects bind as JSON text (D1 stores JSON columns as TEXT), matching the
/// `coerce_param` policy on the browser backend.
fn json_value_to_js(val: &serde_json::Value) -> JsValue {
    match val {
        serde_json::Value::Null => JsValue::NULL,
        serde_json::Value::Bool(b) => JsValue::from(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                JsValue::from(i as f64)
            } else if let Some(f) = n.as_f64() {
                JsValue::from(f)
            } else {
                JsValue::from(n.to_string())
            }
        }
        serde_json::Value::String(s) => JsValue::from(s.as_str()),
        _ => JsValue::from(val.to_string()),
    }
}

/// Convert any Display error into a DatabaseError::Internal.
fn db_err(e: impl std::fmt::Display) -> DatabaseError {
    DatabaseError::Internal(e.to_string())
}

/// The explicit "runtime schema mutation unsupported on D1" error shared by the
/// three schema-*mutation* methods (`ensure_schema_table`, `schema_drop_table`,
/// `schema_add_column`). D1's schema is migration-owned (see those methods'
/// doc comment): a runtime mutation attempt is a genuine misuse, so it fails
/// loudly rather than returning a silent `Ok(())` that hides the no-op. `method`
/// is the trait method name and `target` the table it was asked to mutate — both
/// surfaced so the misuse is diagnosable from the error line alone.
fn schema_mutation_unsupported(method: &str, target: &str) -> DatabaseError {
    DatabaseError::Internal(format!(
        "runtime schema mutation unsupported on Cloudflare D1: `{method}` on \
         `{target}` — D1 schema is migration-owned. Declare the table/column in \
         the block's `migrations/*.sql` (applied at Init via `db::ddl`) instead \
         of mutating the schema at runtime."
    ))
}

/// Whether a D1 error message indicates the target table doesn't exist.
/// D1 surfaces SQLite's `no such table: X` verbatim through the JsValue
/// error; we string-match because the `worker::Error` type doesn't expose
/// SQLite's structured error code.
pub(crate) fn is_no_such_table(msg: &str) -> bool {
    msg.contains("no such table")
}

// Note: unit tests for the pure SQL-planning layer live in `wafer-sql-utils`
// and `wafer-core::interfaces::database::exec` (shared across all SQL
// backends). `impresspress-cloudflare` only compiles on `wasm32-unknown-unknown`
// (the R2/D1 services hold `!Send` JsFutures), so `cargo test
// -p impresspress-cloudflare` errors before reaching any test module. The
// `wasm_bindgen_test`s below run under Node in CI's `cloudflare-wasm-test` job;
// end-to-end validation of the live D1 path comes from a real CF deploy.

#[cfg(test)]
mod tests {
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::*;

    /// B25. SQLite — and therefore D1 — has no array/object storage class, so
    /// the shared write path serialises a structured value to JSON text and
    /// binds it as TEXT. A backend that does not parse it back hands block code
    /// a `Value::String` where the same block gets a `Value::Object` everywhere
    /// else.
    ///
    /// This is the row decoder every D1 read goes through: `run_fetch`,
    /// `run_fetch_one` and the `BatchOp::Rows`/`FetchOne` arms of `run_batch`
    /// all map their rows with it. Before this test D1 stored each value exactly
    /// as it arrived, while `wafer-block-sqlite` and the browser's sql.js bridge
    /// both re-parsed with a byte-identical predicate — so a block reading its
    /// own JSON column got an object on native and in the browser, and a string
    /// on Cloudflare.
    ///
    /// The end-to-end pin is
    /// `wafer_core::interfaces::database::conformance::run_conformance`'s
    /// `check_json_value_round_trip`, which this crate can only *typecheck*
    /// (see `conformance.rs`: a live run needs a workerd D1 binding CI does not
    /// have). This is the closest a runnable test gets to it.
    #[wasm_bindgen_test]
    fn a_json_looking_text_column_decodes_back_into_the_value_that_was_written() {
        let record = record_from_json_row(serde_json::json!({
            "id": "r1",
            "meta": "{\"k\":[1,2],\"nested\":{\"b\":true}}",
            "tags": "[\"a\",\"b\"]",
            "note": "hello world",
            "braced_prose": "{not json at all",
            "count": 3,
        }));

        assert_eq!(record.id, "r1");
        assert_eq!(
            record.data.get("meta"),
            Some(&serde_json::json!({"k": [1, 2], "nested": {"b": true}})),
            "a serialised JSON object in a TEXT column must decode back to the \
             object, as it does on native sqlite and in the browser",
        );
        assert_eq!(
            record.data.get("tags"),
            Some(&serde_json::json!(["a", "b"])),
            "a serialised JSON array must decode back to the array",
        );
        assert_eq!(
            record.data.get("note"),
            Some(&serde_json::json!("hello world")),
            "plain text must stay text, or the decode is not narrow enough",
        );
        assert_eq!(
            record.data.get("braced_prose"),
            Some(&serde_json::json!("{not json at all")),
            "braced text that does not parse must be returned verbatim",
        );
        assert_eq!(record.data.get("count"), Some(&serde_json::json!(3)));
    }

    /// The scalar decoders the aggregate paths use: `run_scalar_i64` /
    /// `run_scalar_f64` read a one-column row whose column the shared builders
    /// alias themselves, so the value has to be taken positionally.
    #[wasm_bindgen_test]
    fn an_aggregate_row_yields_its_single_column_whatever_it_is_aliased_as() {
        assert_eq!(scalar_i64(Some(serde_json::json!({"cnt": 7}))), 7);
        assert_eq!(scalar_i64(None), 0);
        assert_eq!(scalar_f64(Some(serde_json::json!({"total": 2.5}))), 2.5);
        assert_eq!(scalar_f64(None), 0.0);
    }

    /// A batch's minted ids are UUIDv7 and ascend in row order, so request-log
    /// rows drained together — which share a `created_at` to the millisecond —
    /// list newest-first in the reverse of the order they were queued once a
    /// sorted `list` breaks the tie on `id`. A UUIDv4 would order them at random.
    #[wasm_bindgen_test]
    fn a_batch_mints_v7_ids_in_row_order() {
        let rows: Vec<std::collections::HashMap<String, serde_json::Value>> = (0..64)
            .map(|n| {
                std::collections::HashMap::from([
                    ("path".to_string(), serde_json::json!(format!("/r/{n}"))),
                    (
                        "created_at".to_string(),
                        serde_json::json!("2026-09-23T00:00:00.000+00:00"),
                    ),
                ])
            })
            .collect();
        let prepared = prepare_batch_rows(rows).expect("one shape");
        let ids: Vec<String> = prepared
            .iter()
            .map(|pairs| {
                let (_, id) = pairs.iter().find(|(k, _)| k == "id").expect("minted id");
                id.as_str().expect("string id").to_string()
            })
            .collect();
        for id in &ids {
            let parsed = uuid::Uuid::parse_str(id).expect("a UUID");
            assert_eq!(parsed.get_version_num(), 7, "{id} is not a UUIDv7");
        }
        for pair in ids.windows(2) {
            assert!(
                pair[0] < pair[1],
                "{} then {} is out of row order",
                pair[0],
                pair[1]
            );
        }
    }

    /// A `D1Database` that is never queried.
    ///
    /// `unchecked_into` only re-types the `JsValue`; it calls nothing on it.
    /// The tests below read `DbExec::strict_schema`, which is plain Rust state
    /// on the adapter (`AtomicBool`), so the `undefined` handle is never
    /// dereferenced. Constructing a *usable* one needs a workerd D1 binding,
    /// which neither this runner nor CI has — see the module note above and
    /// `conformance.rs`.
    fn never_queried_handle() -> D1Database {
        wasm_bindgen::JsCast::unchecked_into::<D1Database>(JsValue::undefined())
    }

    /// The verdict a D1 service is *born* with is the one the executor reads.
    ///
    /// This is what covers the two services that never reach a lifecycle
    /// `Init` — the request-log drain handle in `run_with_config` and
    /// `build_runtime`'s pre-`Init` `block_settings` read. Both are built and
    /// dropped inside one request, so `set_strict_schema` is never called on
    /// them and their `SchemaCache` never warms: with strict off,
    /// `create_many`'s `DbExec::ensure_data_columns` introspects on every
    /// single drain.
    #[wasm_bindgen_test]
    fn a_d1_service_is_born_with_the_deploys_strict_schema_verdict() {
        let strict = D1DatabaseService::new(never_queried_handle(), true);
        assert!(
            DbExec::strict_schema(&strict),
            "a service constructed with STRICT_SCHEMA on must already skip the \
             table-exists and column introspection — nothing calls \
             `set_strict_schema` on the drain or pre-Init handles",
        );

        let lax = D1DatabaseService::new(never_queried_handle(), false);
        assert!(
            !DbExec::strict_schema(&lax),
            "and a service constructed with it off must still introspect",
        );
    }

    /// `Init` must still be able to speak. `handle_lifecycle` calls
    /// `set_strict_schema` on the runtime's own service after construction; on
    /// Cloudflare it reads the same var, so it normally re-affirms the seeded
    /// value — but the setter has to remain the authority, not be shadowed by
    /// the constructor.
    #[wasm_bindgen_test]
    fn lifecycle_init_still_overrides_the_constructed_verdict() {
        let svc = D1DatabaseService::new(never_queried_handle(), false);
        DatabaseService::set_strict_schema(&svc, true);
        assert!(DbExec::strict_schema(&svc));

        DatabaseService::set_strict_schema(&svc, false);
        assert!(!DbExec::strict_schema(&svc));
    }
}
