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
//! add only missing *columns* (always `TEXT` on SQLite) on demand — unless
//! STRICT_SCHEMA is on, which this backend now honours (see [`STRICT_SCHEMA`]).
//!
//! ## Schema cache
//!
//! The shared executor fronts its operations with introspection: a
//! table-exists probe and a column list (off in STRICT_SCHEMA mode), and a
//! primary-key lookup before every sorted or paged `list`, whose `ORDER BY`
//! ends with the key. Each of those is a `bridge::db_query_raw` round trip
//! into sql.js, so this backend memoizes them in [`SCHEMA_CACHE`], one cache
//! for the one database. The shared defaults invalidate it on every schema
//! change they make (`exec_raw`, `ensure_schema_table`, lazy column-add); the
//! schema changes made anywhere else invalidate it themselves: this file's own
//! `schema_drop_table` / `schema_add_column`, `vector::service`'s DDL (through
//! [`forget_table_schema`]), and `db_init` reopening the database (through
//! [`forget_schema`]).
//!
//! ## The `DatabaseService` impl is a ledger, not a list of forwards
//!
//! It is written with [`wafer_core::forward_database_service!`], whose
//! `ops { … }` block must name every operation on the trait or it does not
//! expand. Eight of the trait's operations carry defaults that are NOT
//! pass-throughs, so an implementation that leaves one out does not inherit
//! "the same behaviour" — it inherits a different one, silently. Writing the
//! word `inherit` is how a default gets taken here.
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
//! single statement.
//!
//! The contract itself lives in [`with_flush_mapped`] and its precedence rules
//! in [`resolve_flush_outcome`], because this is not the only writer to that
//! database: `vector::service` writes the same sql.js file through the same
//! bridge and goes through the same helper. There is one durability contract
//! for the crate, not one per service.
//!
//! The multi-write operations — `create_many`, `batch` and the guarded writes —
//! are one logical mutation each, so a `create_many` of a thousand rows is one
//! flush, not a thousand. They reach sql.js through
//! [`DbExec::run_transaction`], which this backend runs between `BEGIN` and
//! `COMMIT` (see [`in_transaction`]).
//!
//! ## A taken key
//!
//! sql.js reports a failed statement as a JS exception carrying SQLite's
//! message and no code, so the primitives classify that text with
//! [`impresspress_core::sqlite_text_error::statement_error`]: a primary- or
//! unique-key violation is [`DatabaseError::AlreadyExists`], as on the native
//! backends, and anything else `Internal`.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        LazyLock,
    },
};

// The `forward_database_service!` ledger below spells every generated
// signature with a fully-qualified path, so only the types the `custom` bodies
// name in their own signatures are imported here.
use wafer_block::db::Filter;
use wafer_core::interfaces::database::{
    codec::{record_from_json_row, scalar_f64, scalar_i64},
    exec::{DbExec, TxOp, TxResult},
    schema_cache::SchemaCache,
    service::{
        CapGuard, Column, DatabaseError, DatabaseService, GuardedInsert, GuardedUpdate, Record,
        Table, UpsertSpec, WriteOp, WriteOutcome,
    },
};
use wafer_sql_utils::{introspect, Backend};

use crate::{bridge, db_codec};

/// The resolved `WAFER_RUN__DATABASE__STRICT_SCHEMA` verdict, written once by
/// `DatabaseService::set_strict_schema` at the shared `DatabaseBlock`'s `Init`
/// and read by [`DbExec::strict_schema`] on every operation.
///
/// A static rather than a field because [`BrowserDatabaseService`] is a unit
/// struct: it carries no handle, and every instance addresses the one global
/// sql.js/OPFS database — `impresspress-web` constructs a second one for its
/// boot hook precisely because they are interchangeable. STRICT_SCHEMA is a
/// property of that database, so a per-instance field would let two handles
/// disagree about the same schema.
static STRICT_SCHEMA: AtomicBool = AtomicBool::new(false);

/// Memoized table-exists / column-list / primary-key facts for the one
/// sql.js database, returned by [`DbExec::schema_cache`].
///
/// A static for the reason [`STRICT_SCHEMA`] is one: every
/// [`BrowserDatabaseService`] handle addresses the same database, so they
/// share what is known about its schema. A per-handle cache would let the
/// handle `impresspress-web`'s boot hook builds keep facts that a migration
/// run through the runtime's handle had already invalidated.
static SCHEMA_CACHE: LazyLock<SchemaCache> = LazyLock::new(SchemaCache::new);

/// Drop every memoized schema fact. Called when the whole database changes
/// under the cache — `db_init` reopening it — rather than one table's shape.
pub(crate) fn forget_schema() {
    SCHEMA_CACHE.clear();
}

/// Drop the memoized schema facts for one table, for a schema change the
/// shared executor did not make and so did not invalidate for: the DDL
/// `vector::service` runs straight through the bridge.
pub(crate) fn forget_table_schema(table: &str) {
    SCHEMA_CACHE.invalidate(table);
}

/// Browser-side DatabaseService backed by sql.js via the JS bridge.
pub struct BrowserDatabaseService;

/// The crate's ONE durability contract: run a mutating `op`, then flush the
/// sql.js database to OPFS exactly once, whatever `op` returned.
///
/// [`BrowserDatabaseService::with_flush`] is this with `E = DatabaseError`;
/// `vector::service` is the other caller, with `E = VectorError`. `map_flush`
/// turns the JS rejection into the caller's error type, which is the only
/// thing that ever differed between them — the precedence rules below are the
/// contract and must not be restated per caller. Before this was shared, the
/// vector service ran `bridge::dbFlush()` itself with `?`, which skipped the
/// flush entirely whenever the operation that mutated the database failed.
///
/// See [`resolve_flush_outcome`] for the precedence and why each arm is what
/// it is.
pub(crate) async fn with_flush_mapped<T, E>(
    op: impl std::future::Future<Output = Result<T, E>>,
    map_flush: impl FnOnce(String) -> E,
) -> Result<T, E> {
    with_flush_through(op, flush_through_bridge, map_flush).await
}

/// The one flush this crate performs: hand the sql.js database to `bridge.js`
/// to write out to OPFS.
async fn flush_through_bridge() -> Result<(), String> {
    bridge::dbFlush()
        .await
        .map(|_| ())
        .map_err(|e| format!("flush to OPFS: {}", bridge::describe(&e)))
}

/// [`with_flush_mapped`] with the flush supplied by the caller.
///
/// `flush` is a closure, not a future, so it cannot be started before `op`
/// finishes — and so the ONE property the whole contract rests on, that the
/// flush runs whatever `op` returned, is assertable without a bridge or an
/// OPFS. That property is the regression this shape exists to prevent: the
/// vector service used to run `bridge::dbFlush()` with `?`, which skipped the
/// flush entirely whenever the mutating operation failed.
async fn with_flush_through<T, E, Fut>(
    op: impl std::future::Future<Output = Result<T, E>>,
    flush: impl FnOnce() -> Fut,
    map_flush: impl FnOnce(String) -> E,
) -> Result<T, E>
where
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let result = op.await;
    let flush = flush().await.map_err(map_flush);
    resolve_flush_outcome(result, flush)
}

/// Which of an operation's outcome and its flush's outcome the caller is told
/// about. Pure, so it is testable without a bridge (`flush_precedence`).
///
/// - `op` succeeds, flush succeeds → `Ok`. The common case: durable.
/// - `op` succeeds, flush fails → the flush error. The mutation is sitting in
///   memory only (quota exceeded, OPFS permission revoked); reporting success
///   would tell the caller data is durable when a Service Worker eviction
///   could still lose it.
/// - `op` fails → the operation's own error, whatever the flush did. It is the
///   more specific and more actionable of the two. The flush is still
///   *attempted* — a failed logical operation may have applied some of its
///   statements already (a lazy column-add ALTER before a rejected INSERT),
///   and skipping the flush would leave those in memory until some unrelated
///   later mutation happened to write them out.
pub(crate) fn resolve_flush_outcome<T, E>(op: Result<T, E>, flush: Result<(), E>) -> Result<T, E> {
    match (op, flush) {
        (Ok(v), Ok(())) => Ok(v),
        (Ok(_), Err(flush_err)) => Err(flush_err),
        (Err(op_err), _) => Err(op_err),
    }
}

// SAFETY: `BrowserDatabaseService` is a unit struct; the state its handles
// share (`STRICT_SCHEMA`, `SCHEMA_CACHE`) is in statics that are `Sync`.
// wasm32-unknown-unknown has no threads, so the `Send`/`Sync` bounds
// required by `Arc<dyn DatabaseService>` are satisfied trivially — no
// cross-thread aliasing or data races are possible.
unsafe impl Send for BrowserDatabaseService {}
unsafe impl Sync for BrowserDatabaseService {}

impl BrowserDatabaseService {
    /// Run a mutating `op`, then flush the sql.js DB to OPFS exactly once —
    /// this is the coalescing point described in the module doc comment.
    /// [`with_flush_mapped`] owns the contract; this is it at
    /// `E = DatabaseError`.
    async fn with_flush<T>(
        &self,
        op: impl std::future::Future<Output = Result<T, DatabaseError>>,
    ) -> Result<T, DatabaseError> {
        with_flush_mapped(op, DatabaseError::Internal).await
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
        let value = bridge::db_query_raw(sql, params_js).map_err(|e| statement_failed(&e))?;
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

    /// The database-wide [`SCHEMA_CACHE`]. Without it every sorted or paged
    /// `list` would read the table's primary key through the bridge first.
    fn schema_cache(&self) -> Option<&SchemaCache> {
        Some(&SCHEMA_CACHE)
    }

    /// The flag `DatabaseService::set_strict_schema` recorded. When it is on,
    /// the shared orchestration skips the per-operation table-exists probe and
    /// the lazy ADD COLUMN path, trusting the migrated schema.
    fn strict_schema(&self) -> bool {
        STRICT_SCHEMA.load(Ordering::Relaxed)
    }

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
        let rows_modified =
            bridge::db_exec_raw(sql, params_js).map_err(|e| statement_failed(&e))?;
        // NOTE: deliberately no `bridge::dbFlush()` here — flushing is
        // coalesced at the `DatabaseService` method boundary via
        // `with_flush`. See the module doc comment.
        Ok(rows_modified as i64)
    }

    /// Delegates to [`run_fetch`](Self::run_fetch): sql.js is a single
    /// in-process database behind one bridge handle, so there is no
    /// reader/writer split for a `DELETE … RETURNING` to land on the wrong
    /// side of, and `bridge.js`'s `dbQueryRaw` runs the statement through
    /// `_db.exec()`, which applies side effects and returns the `RETURNING`
    /// rows in the same call.
    ///
    /// Like [`run_execute`](Self::run_execute) and every other `DbExec`
    /// primitive here, it deliberately does NOT flush to OPFS — flushing is
    /// coalesced once per logical `DatabaseService` call by
    /// [`BrowserDatabaseService::with_flush`], and `take_where` (this
    /// primitive's one caller, via the shared [`DbExec::take_where`]) is a
    /// `custom` entry in the ledger below precisely so it gets that flush.
    /// See the module doc comment's durability contract.
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

    /// `ops` between `BEGIN` and `COMMIT` on the one sql.js database, rolled
    /// back on the first failure — see [`in_transaction`]. Like every other
    /// primitive here it does not flush; the `DatabaseService` method that
    /// called it does, once.
    async fn run_transaction(&self, ops: &[TxOp<'_>]) -> Result<Vec<TxResult>, DatabaseError> {
        in_transaction(ops, |op| match *op {
            TxOp::Execute { sql, params } => {
                let params_js = db_codec::params_to_js(params).map_err(DatabaseError::Internal)?;
                let rows = bridge::db_exec_raw(sql, params_js).map_err(|e| statement_failed(&e))?;
                Ok(TxResult::Execute(rows as i64))
            }
            TxOp::Returning { sql, params } => Ok(TxResult::Returning(
                self.query_json_rows(sql, params)?
                    .into_iter()
                    .map(record_from_json_row)
                    .collect(),
            )),
        })
    }
}

/// A statement sql.js refused, as a [`DatabaseError`]: its text classified by
/// [`impresspress_core::sqlite_text_error::statement_error`], so a taken key is
/// `AlreadyExists`.
fn statement_failed(e: &wasm_bindgen::JsValue) -> DatabaseError {
    impresspress_core::sqlite_text_error::statement_error(format!("sql exec: {e:?}"))
}

/// Run `ops` through `run` as one transaction: `BEGIN`, every op in order,
/// `COMMIT` — and on the first failure `ROLLBACK` and that failure, so either
/// every op is applied or none is.
///
/// `run` is synchronous, and that is what makes this a transaction on a
/// database other code shares: every bridge call is synchronous, so nothing
/// between `BEGIN` and `COMMIT` yields to the executor, and no other task's
/// statement can land inside it. It is a parameter so the framing can be
/// tested without sql.js (`transaction_framing`).
///
/// A failed `ROLLBACK` is logged, not returned: the caller is owed the
/// statement's own failure. A connection left inside a transaction refuses
/// the next `BEGIN` ("cannot start a transaction within a transaction"), so
/// the log is what names the cause when that happens.
fn in_transaction(
    ops: &[TxOp<'_>],
    mut run: impl FnMut(&TxOp<'_>) -> Result<TxResult, DatabaseError>,
) -> Result<Vec<TxResult>, DatabaseError> {
    const BEGIN: TxOp<'static> = TxOp::Execute {
        sql: "BEGIN",
        params: &[],
    };
    const COMMIT: TxOp<'static> = TxOp::Execute {
        sql: "COMMIT",
        params: &[],
    };
    const ROLLBACK: TxOp<'static> = TxOp::Execute {
        sql: "ROLLBACK",
        params: &[],
    };

    run(&BEGIN)?;
    let mut results = Vec::with_capacity(ops.len());
    let outcome = ops.iter().try_for_each(|op| {
        results.push(run(op)?);
        Ok(())
    });
    let outcome = outcome.and_then(|()| run(&COMMIT).map(|_| ()));
    if let Err(failure) = outcome {
        if let Err(rollback) = run(&ROLLBACK) {
            tracing::error!(
                error = %rollback,
                "ROLLBACK after a failed transaction failed; sql.js may still be inside it"
            );
        }
        return Err(failure);
    }
    Ok(results)
}

// ─── DatabaseService — an explicit ledger over the shared DbExec defaults ─────
//
// Written with `forward_database_service!` rather than by hand. The macro's
// `ops { … }` block names EVERY operation on the trait and refuses to expand
// if one is missing, so the twenty-seven lines below are a ledger of what this
// backend does with each: `forward` = the shared `DbExec` default, `custom` =
// written here, `inherit` = deliberately the `DatabaseService` trait default.
// Eight of those trait defaults are not pass-throughs (`take_where` is a
// list-then-delete loop instead of `DELETE … RETURNING`, `set_strict_schema`
// is a silent no-op, …), and a backend that omits one does not get "the same
// behaviour" — it gets a different, worse one, invisibly. That is the bug the
// ledger makes unrepresentable.
//
// The `custom` entries here are all the same thing: every method that can
// mutate the sql.js database wraps its `DbExec` default in `with_flush`, so
// exactly one OPFS flush happens per logical call however many `run_execute`
// statements the shared default issued internally. `take_where` is a mutator
// (`DELETE … RETURNING`) despite its read-shaped return, so it is flushed too.
// `set_strict_schema` is custom because `DbExec` has no such operation to
// forward to — it is the setter behind `DbExec::strict_schema`.
wafer_core::forward_database_service! {
    impl DatabaseService for BrowserDatabaseService {
        forward_to DbExec;

        ops {
            get: forward,
            list: forward,
            create: custom,
            create_many: custom,
            update: custom,
            delete: custom,
            count: forward,
            sum: forward,
            query_raw: forward,
            exec_raw: custom,
            delete_where: custom,
            delete_where_count: custom,
            take_where: custom,
            update_where: custom,
            update_where_count: custom,
            increment_field_where: custom,
            upsert: custom,
            aggregate: forward,
            batch: custom,
            insert_guarded: custom,
            update_guarded: custom,
            ensure_schema_table: custom,
            ensure_schema_tables: inherit,
            schema_table_exists: forward,
            schema_columns: forward,
            schema_drop_table: custom,
            schema_add_column: custom,
            set_strict_schema: custom,
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

        async fn update_where_count(
            &self,
            collection: &str,
            filters: &[Filter],
            data: HashMap<String, serde_json::Value>,
        ) -> Result<i64, DatabaseError> {
            self.with_flush(DbExec::update_where_count(self, collection, filters, data))
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

        /// One transaction, one flush, however many rows.
        async fn create_many(
            &self,
            collection: &str,
            rows: Vec<HashMap<String, serde_json::Value>>,
        ) -> Result<i64, DatabaseError> {
            self.with_flush(DbExec::create_many(self, collection, rows))
                .await
        }

        /// One transaction, one flush, however many ops.
        async fn batch(&self, ops: Vec<WriteOp>) -> Result<Vec<WriteOutcome>, DatabaseError> {
            self.with_flush(DbExec::batch(self, ops)).await
        }

        async fn insert_guarded(
            &self,
            collection: &str,
            data: HashMap<String, serde_json::Value>,
            guards: &[CapGuard],
        ) -> Result<GuardedInsert, DatabaseError> {
            self.with_flush(DbExec::insert_guarded(self, collection, data, guards))
                .await
        }

        async fn update_guarded(
            &self,
            collection: &str,
            filters: &[Filter],
            data: HashMap<String, serde_json::Value>,
            guards: &[CapGuard],
        ) -> Result<GuardedUpdate, DatabaseError> {
            self.with_flush(DbExec::update_guarded(
                self, collection, filters, data, guards,
            ))
            .await
        }

        /// The DDL sequence itself is [`DbExec::ensure_schema_table`] — the
        /// shared default this file used to carry a copy of. The copy had
        /// drifted in one way that mattered and one that did not: it built the
        /// same CREATE / add-missing-columns / indexes / FK-indexes sequence,
        /// but it hard-coded `Backend::Sqlite` instead of reading
        /// `Self::BACKEND`, and it did not invalidate the schema cache on the
        /// error path. The shared default invalidates [`SCHEMA_CACHE`] on
        /// both paths.
        ///
        /// What stays browser-specific is the one flush: the whole sequence is
        /// several `run_execute` calls and they share a single write to OPFS.
        async fn ensure_schema_table(&self, table: &Table) -> Result<(), DatabaseError> {
            self.with_flush(DbExec::ensure_schema_table(self, table))
                .await
        }

        /// DDL through `run_execute` rather than a shared default, so the
        /// cached facts for `name` are dropped here, whatever the statement
        /// returned.
        async fn schema_drop_table(&self, name: &str) -> Result<(), DatabaseError> {
            self.with_flush(async {
                let stmt = wafer_sql_utils::ddl::build_drop_table(name, Self::BACKEND);
                let dropped = self.run_execute(&stmt.sql, &[]).await;
                SCHEMA_CACHE.invalidate(name);
                dropped.map(|_| ())
            })
            .await
        }

        async fn schema_add_column(
            &self,
            table: &str,
            column: &Column,
        ) -> Result<(), DatabaseError> {
            self.with_flush(async {
                let stmt = wafer_sql_utils::ddl::build_add_column(table, column, Self::BACKEND);
                let added = self.run_execute(&stmt.sql, &[]).await;
                SCHEMA_CACHE.invalidate(table);
                added.map(|_| ())
            })
            .await
        }

        /// Record the resolved STRICT_SCHEMA verdict so [`DbExec::strict_schema`]
        /// can read it. The trait default is a silent no-op, which is the
        /// wrong answer for a backend that DOES run through `DbExec`: the
        /// shared `DatabaseBlock` advertises
        /// `WAFER_RUN__DATABASE__STRICT_SCHEMA` as a config key and applies it
        /// at `Init` on every backend, so inheriting the no-op meant this
        /// target offered an operator a switch that did nothing.
        fn set_strict_schema(&self, enabled: bool) {
            STRICT_SCHEMA.store(enabled, Ordering::Relaxed);
        }
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
// in `.github/workflows/ci-shared.yml`, the one body both gates run. The
// `--tests` flag is required: this
// `#[cfg(all(test, target_arch = "wasm32"))]` module needs the wasm32 dev-dep
// `conformance` feature, which a plain `cargo check` (no `--tests`) does not
// activate — so the plain check does NOT compile it. The dedicated
// `wasm-pack test --node crates/impresspress-browser` job also builds it, but on
// a pull request it is gated on a diff touching this crate, `impresspress-core`,
// `Cargo.toml` or `Cargo.lock` (a merge runs it unconditionally), so on a pull
// request that changes none of those the unconditional step above is what
// catches trait-surface drift. A wafer-run pin bump does reach the gated job,
// via `Cargo.lock`. If the trait surface drifts (a new required op, or a changed
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

/// STRICT_SCHEMA is applied, not silently dropped. `database.rs` is
/// wasm32-only, so these run under `wasm-pack test --node`; they read and
/// write the flag and touch no bridge.
#[cfg(all(test, target_arch = "wasm32"))]
mod strict_schema_policy {
    use wafer_core::interfaces::database::{exec::DbExec, service::DatabaseService};
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::BrowserDatabaseService;

    /// **Fails on the pre-change tree**, where `set_strict_schema` was the
    /// trait's silent no-op default and `DbExec::strict_schema` therefore
    /// always answered `false`. `DatabaseBlock`'s `Init` reads
    /// `WAFER_RUN__DATABASE__STRICT_SCHEMA` and calls the setter on every
    /// backend, so the browser advertised the config key (through the shared
    /// block's `config_keys`) and then ignored whatever an operator set.
    #[wasm_bindgen_test]
    fn setting_strict_schema_is_observed_by_the_shared_executor() {
        let svc = BrowserDatabaseService;
        // The default the shared executor starts from.
        assert!(!DbExec::strict_schema(&svc));

        DatabaseService::set_strict_schema(&svc, true);
        assert!(
            DbExec::strict_schema(&svc),
            "the shared orchestration must see the flag, or the table-exists \
             probe and the lazy ADD COLUMN path stay on the hot path"
        );

        // A second handle sees it too: the service is a unit struct over one
        // global sql.js database, so the flag is a property of that database
        // and not of a handle. `impresspress-web` holds a second handle for
        // its boot hook.
        assert!(DbExec::strict_schema(&BrowserDatabaseService));

        DatabaseService::set_strict_schema(&svc, false);
        assert!(!DbExec::strict_schema(&svc));
    }
}

/// The schema cache the shared executor memoizes introspection in. No bridge:
/// the facts are written straight into the cache the executor reads.
#[cfg(all(test, target_arch = "wasm32"))]
mod schema_cache_policy {
    use wafer_core::interfaces::database::exec::DbExec;
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::{forget_schema, BrowserDatabaseService};

    /// **Fails without the cache**, where `DbExec::schema_cache` was the
    /// trait's `None` default: every sorted or paged `list` then read the
    /// table's primary key through the bridge before its select. Two handles
    /// share one cache, because they address one database, and
    /// `forget_schema` empties it for the schema changes the shared executor
    /// does not see.
    #[wasm_bindgen_test]
    fn every_handle_shares_one_cache_and_forget_schema_empties_it() {
        let runtime = BrowserDatabaseService;
        let boot_hook = BrowserDatabaseService;
        let cache = DbExec::schema_cache(&runtime).expect("the browser backend keeps a cache");
        let other = DbExec::schema_cache(&boot_hook).expect("so does every other handle");
        assert!(std::ptr::eq(cache, other), "one database, one cache");

        cache.set_primary_key_if_gen("cache_policy_t", vec!["id".into()], cache.generation());
        assert_eq!(
            other.primary_key("cache_policy_t"),
            Some(vec!["id".to_string()]),
            "a key one handle learned is served to the other"
        );

        forget_schema();
        assert_eq!(other.primary_key("cache_policy_t"), None);
    }
}

/// The invalidation half of the cache: every schema change the shared executor
/// does not make has to drop what the cache knows about the table it changed.
///
/// Each of these calls a bridge function, which rejects under
/// `wasm-pack test --node` (there is no sql.js there) — which is the point:
/// the invalidation has to happen whatever the statement returned, because a
/// failed DDL may still have applied. The test seeds a fact, calls the real
/// method, and asserts the fact is gone however the call ended.
#[cfg(all(test, target_arch = "wasm32"))]
mod schema_invalidation {
    use wafer_core::interfaces::database::{
        exec::DbExec,
        service::{Column, DatabaseService},
    };
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::BrowserDatabaseService;

    /// Seed a primary key for `table` and hand back the cache it went into.
    fn seed(table: &str) -> &'static wafer_core::interfaces::database::schema_cache::SchemaCache {
        let cache = DbExec::schema_cache(&BrowserDatabaseService).expect("a cache");
        cache.set_primary_key_if_gen(table, vec!["id".into()], cache.generation());
        assert_eq!(cache.primary_key(table), Some(vec!["id".to_string()]));
        cache
    }

    #[wasm_bindgen_test]
    async fn dropping_a_table_forgets_it() {
        let cache = seed("invalidate_drop_t");
        let _ =
            DatabaseService::schema_drop_table(&BrowserDatabaseService, "invalidate_drop_t").await;
        assert_eq!(cache.primary_key("invalidate_drop_t"), None);
    }

    #[wasm_bindgen_test]
    async fn adding_a_column_forgets_its_table() {
        let cache = seed("invalidate_add_t");
        let _ = DatabaseService::schema_add_column(
            &BrowserDatabaseService,
            "invalidate_add_t",
            &Column::new(
                "extra",
                wafer_core::interfaces::database::service::DataType::Text,
            ),
        )
        .await;
        assert_eq!(cache.primary_key("invalidate_add_t"), None);
    }

    /// Reopening the database replaces everything the cache described, so it
    /// drops the lot rather than one table.
    #[wasm_bindgen_test]
    async fn reopening_the_database_forgets_everything() {
        let cache = seed("invalidate_init_t");
        let _ = crate::db_init().await;
        assert_eq!(cache.primary_key("invalidate_init_t"), None);
    }
}

/// The flush precedence every mutating path in this crate shares, and the
/// unconditional flush underneath it. No bridge, no OPFS.
#[cfg(all(test, target_arch = "wasm32"))]
mod flush_precedence {
    use std::{cell::Cell, rc::Rc};

    use wafer_core::interfaces::database::service::DatabaseError;
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::{resolve_flush_outcome, with_flush_through};

    /// A FAILED operation must still flush. The headline of the durability
    /// change was exactly this: four vector-service sites ran the flush with
    /// `?`, so a failed mutation skipped it and left whatever statements had
    /// already applied (a lazy column-add ALTER before a rejected INSERT) in
    /// memory only, until some unrelated later mutation happened to write them
    /// out. `resolve_flush_outcome` cannot see this — it is handed both
    /// outcomes — so the assertion has to be on the wrapper.
    #[wasm_bindgen_test]
    async fn a_failed_operation_still_flushes() {
        let flushes = Rc::new(Cell::new(0u32));
        let counter = flushes.clone();

        let out: Result<u8, DatabaseError> = with_flush_through(
            async { Err(DatabaseError::NotFound) },
            move || {
                counter.set(counter.get() + 1);
                async { Ok(()) }
            },
            DatabaseError::Internal,
        )
        .await;

        assert_eq!(
            flushes.get(),
            1,
            "the flush was skipped because the operation failed"
        );
        assert!(matches!(out, Err(DatabaseError::NotFound)));
    }

    /// …and a successful one flushes exactly once, not once per statement the
    /// operation ran. That coalescing is the other half of the contract.
    #[wasm_bindgen_test]
    async fn a_successful_operation_flushes_exactly_once() {
        let flushes = Rc::new(Cell::new(0u32));
        let counter = flushes.clone();

        let out: Result<u8, DatabaseError> = with_flush_through(
            async { Ok(7) },
            move || {
                counter.set(counter.get() + 1);
                async { Ok(()) }
            },
            DatabaseError::Internal,
        )
        .await;

        assert_eq!(flushes.get(), 1);
        assert_eq!(out.expect("ok"), 7);
    }

    /// The flush's own failure reaches the caller through `map_flush`, in the
    /// caller's error type — the only thing that ever differed between this
    /// helper's two callers.
    #[wasm_bindgen_test]
    async fn a_flush_failure_is_mapped_into_the_callers_error_type() {
        let out: Result<u8, DatabaseError> = with_flush_through(
            async { Ok(7) },
            || async { Err("quota exceeded".to_string()) },
            DatabaseError::Internal,
        )
        .await;

        match out {
            Err(DatabaseError::Internal(msg)) => assert_eq!(msg, "quota exceeded"),
            other => panic!("expected the mapped flush error, got {other:?}"),
        }
    }

    #[wasm_bindgen_test]
    fn a_durable_success_is_a_success() {
        let out: Result<u8, DatabaseError> = resolve_flush_outcome(Ok(7), Ok(()));
        assert_eq!(out.expect("ok"), 7);
    }

    /// A mutation that only reached memory must not be reported as done: a
    /// Service Worker eviction would lose it.
    #[wasm_bindgen_test]
    fn a_failed_flush_beats_a_successful_operation() {
        let out: Result<u8, DatabaseError> =
            resolve_flush_outcome(Ok(7), Err(DatabaseError::Internal("quota".into())));
        match out {
            Err(DatabaseError::Internal(msg)) => assert_eq!(msg, "quota"),
            other => panic!("expected the flush error, got {other:?}"),
        }
    }

    /// …but the operation's own error is the more actionable of the two, so it
    /// wins even when the flush also failed.
    #[wasm_bindgen_test]
    fn the_operations_error_beats_the_flushs() {
        let out: Result<u8, DatabaseError> = resolve_flush_outcome(
            Err(DatabaseError::NotFound),
            Err(DatabaseError::Internal("quota".into())),
        );
        assert!(matches!(out, Err(DatabaseError::NotFound)));
    }

    /// The same three answers for the vector service's error type — the point
    /// of sharing the helper is that the two callers cannot drift.
    #[wasm_bindgen_test]
    fn the_vector_services_error_type_gets_the_same_precedence() {
        use wafer_core::interfaces::vector::service::VectorError;

        let ok: Result<(), VectorError> = resolve_flush_outcome(Ok(()), Ok(()));
        assert!(ok.is_ok());

        let flush_failed: Result<(), VectorError> =
            resolve_flush_outcome(Ok(()), Err(VectorError::Internal("quota".into())));
        assert!(matches!(flush_failed, Err(VectorError::Internal(_))));

        let both_failed: Result<(), VectorError> = resolve_flush_outcome(
            Err(VectorError::IndexNotFound("idx".into())),
            Err(VectorError::Internal("quota".into())),
        );
        assert!(matches!(both_failed, Err(VectorError::IndexNotFound(_))));
    }
}

/// The `BEGIN`/`COMMIT`/`ROLLBACK` framing of [`in_transaction`], driven
/// through a recording statement runner instead of sql.js (which does not
/// exist under `wasm-pack test --node`).
#[cfg(all(test, target_arch = "wasm32"))]
mod transaction_framing {
    use std::cell::RefCell;

    use wafer_core::interfaces::database::{
        exec::{TxOp, TxResult},
        service::DatabaseError,
    };
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::in_transaction;

    /// Three inserts, the second of which may fail the way sql.js reports a
    /// taken key.
    fn inserts() -> [TxOp<'static>; 3] {
        [
            TxOp::Execute {
                sql: "INSERT INTO t (id) VALUES ('a')",
                params: &[],
            },
            TxOp::Execute {
                sql: "INSERT INTO t (id) VALUES ('b')",
                params: &[],
            },
            TxOp::Execute {
                sql: "INSERT INTO t (id) VALUES ('c')",
                params: &[],
            },
        ]
    }

    /// Every statement the runner saw, in order, failing the one whose SQL
    /// contains `fail_on`.
    fn run_recording(
        ops: &[TxOp<'_>],
        fail_on: Option<&str>,
    ) -> (Result<Vec<TxResult>, DatabaseError>, Vec<String>) {
        let seen = RefCell::new(Vec::new());
        let out = in_transaction(ops, |op| {
            let (sql, _) = op.sql_params();
            seen.borrow_mut().push(sql.to_string());
            match fail_on {
                Some(needle) if sql.contains(needle) => {
                    Err(impresspress_core::sqlite_text_error::statement_error(
                        "sql exec: JsValue(Error: UNIQUE constraint failed: t.id)".into(),
                    ))
                }
                _ => Ok(TxResult::Execute(1)),
            }
        });
        (out, seen.into_inner())
    }

    #[wasm_bindgen_test]
    fn every_statement_runs_between_begin_and_commit() {
        let (out, seen) = run_recording(&inserts(), None);
        assert_eq!(out.expect("committed").len(), 3);
        assert_eq!(seen.first().map(String::as_str), Some("BEGIN"));
        assert_eq!(seen.last().map(String::as_str), Some("COMMIT"));
        assert_eq!(seen.len(), 5, "{seen:?}");
    }

    /// **The all-or-nothing half.** A failing statement rolls back the ones
    /// before it and is never followed by a `COMMIT` — without the rollback,
    /// the first insert would stay applied and the next flush would persist
    /// it. The failure reaches the caller as the taken key it was.
    #[wasm_bindgen_test]
    fn a_failing_statement_rolls_back_and_nothing_after_it_runs() {
        let (out, seen) = run_recording(&inserts(), Some("'b'"));
        assert!(
            matches!(out, Err(DatabaseError::AlreadyExists(_))),
            "{out:?}"
        );
        assert_eq!(
            seen,
            [
                "BEGIN",
                "INSERT INTO t (id) VALUES ('a')",
                "INSERT INTO t (id) VALUES ('b')",
                "ROLLBACK",
            ],
        );
    }

    /// A `COMMIT` that fails is a failed transaction too.
    #[wasm_bindgen_test]
    fn a_failed_commit_rolls_back() {
        let (out, seen) = run_recording(&inserts(), Some("COMMIT"));
        assert!(out.is_err());
        assert_eq!(seen.last().map(String::as_str), Some("ROLLBACK"));
    }
}
