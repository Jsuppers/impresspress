//! Per-request queue of `request_logs` audit rows, persisted after the
//! response under the request's own database budget.
//!
//! A platform that must not pay an audit write on the response path (the
//! Cloudflare Worker) runs each request's dispatch inside
//! [`queue_request_logs`] with a fresh [`RequestLogQueue`]. While that future
//! is polled, the pipeline pushes the request's audit rows into its queue
//! instead of inserting them; the platform takes them afterwards and writes
//! them with [`persist`] off the response path. Outside such a scope, the
//! pipeline inserts the row inline (native).
//!
//! The queue is selected per POLL, not per isolate: one Workers isolate
//! interleaves concurrent requests whenever a future returns `Pending`, and
//! each request is its own invocation with its own D1 query limit. A queue
//! shared by the isolate would have one request write, and pay for, rows that
//! a concurrent request produced, and lose them whenever that request had
//! spent its limit.
//!
//! A row whose write is refused because the invocation has run out of
//! statements (`DatabaseError::ResourceExhausted`) is not dropped: it is
//! carried over, and the next [`persist`] in the isolate that has budget to
//! spare writes it after its own rows. Only past [`CARRY_OVER_CAP`] rows are
//! carried rows dropped, and [`PersistReport::dropped`] says how many.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context as TaskContext, Poll},
};

use wafer_core::interfaces::database::service::{DatabaseError, DatabaseService, StatementBudget};

use crate::IsolateCell;

/// One queued audit row (table + column map), ready for
/// `DatabaseService::create_many`.
pub struct QueuedRequestLog {
    pub table: &'static str,
    pub data: HashMap<String, serde_json::Value>,
}

/// One request's queued audit rows.
///
/// [`IsolateCell`] rather than `RefCell` for the reason
/// [`crate::isolate_cell`] gives: a push can reallocate, and a Cloudflare hard
/// stop inside a borrow would strand its flag.
#[derive(Default)]
pub struct RequestLogQueue {
    rows: IsolateCell<Vec<QueuedRequestLog>>,
}

impl RequestLogQueue {
    /// An empty queue for one request.
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    fn push(&self, row: QueuedRequestLog) {
        let mut rows = self.rows.take().unwrap_or_default();
        rows.push(row);
        self.rows.set(rows);
    }

    /// Take every row queued so far, leaving the queue empty.
    pub fn take(&self) -> Vec<QueuedRequestLog> {
        self.rows.take().unwrap_or_default()
    }
}

thread_local! {
    /// The queue of the request whose future is being polled, if any.
    static CURRENT: IsolateCell<Rc<RequestLogQueue>> = const { IsolateCell::new() };
    /// Rows a request could not write within its own budget, waiting for a
    /// later [`persist`] in this isolate.
    static CARRIED: IsolateCell<Vec<QueuedRequestLog>> = const { IsolateCell::new() };
}

/// Queue `row` for the request being polled. Hands the row back when no
/// request queue is in scope, so the caller inserts it itself.
pub(crate) fn enqueue(row: QueuedRequestLog) -> Result<(), QueuedRequestLog> {
    match CURRENT.with(IsolateCell::get) {
        Some(queue) => {
            queue.push(row);
            Ok(())
        }
        None => Err(row),
    }
}

/// Restores the previously current queue when a poll ends, including when
/// the inner poll unwinds.
struct ScopeGuard {
    previous: Option<Rc<RequestLogQueue>>,
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        CURRENT.with(|slot| slot.replace(self.previous.take()));
    }
}

/// A future that makes its request's [`RequestLogQueue`] current on every
/// poll. See [`queue_request_logs`].
pub struct QueuedRequestLogs<F> {
    queue: Rc<RequestLogQueue>,
    inner: Pin<Box<F>>,
}

impl<F: Future> Future for QueuedRequestLogs<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let _guard = ScopeGuard {
            previous: CURRENT.with(|slot| slot.replace(Some(Rc::clone(&self.queue)))),
        };
        self.inner.as_mut().poll(cx)
    }
}

/// Run `future` with the audit rows it produces queued into `queue` rather
/// than inserted.
pub fn queue_request_logs<F: Future>(
    queue: Rc<RequestLogQueue>,
    future: F,
) -> QueuedRequestLogs<F> {
    QueuedRequestLogs {
        queue,
        inner: Box::pin(future),
    }
}

/// The most carried-over rows an isolate holds. Past it, the oldest carried
/// rows are dropped (and counted in [`PersistReport::dropped`]) so a run of
/// requests that each spend their whole budget cannot grow isolate memory
/// without bound.
pub const CARRY_OVER_CAP: usize = 1000;

/// A write [`persist`] could not make and did not carry over.
#[derive(Debug)]
pub struct PersistFailure {
    pub table: &'static str,
    pub rows: usize,
    pub error: String,
}

/// What [`persist`] did not write now.
#[derive(Debug, Default)]
pub struct PersistReport {
    /// Writes that failed for a reason other than the invocation's statement
    /// budget; their rows are not retried.
    pub failures: Vec<PersistFailure>,
    /// Rows now waiting for a later [`persist`] because this invocation had
    /// no statements left for them.
    pub carried_over: usize,
    /// Carried rows discarded because more than [`CARRY_OVER_CAP`] were
    /// waiting.
    pub dropped: usize,
}

/// Write `rows` — one request's queued audit rows — through `db`, which
/// counts against that request's own invocation, then any rows earlier
/// requests in this isolate carried over.
///
/// Rows refused for the invocation's statement budget are carried over, not
/// dropped. Carried rows are attempted only when this request's own rows all
/// fit, so a request is never charged for another's rows at the expense of
/// its own.
pub async fn persist(db: &dyn DatabaseService, rows: Vec<QueuedRequestLog>) -> PersistReport {
    let mut report = PersistReport::default();
    let mut carry = Vec::new();
    write_grouped(db, rows, &mut report, &mut carry).await;

    if carry.is_empty() {
        let earlier = CARRIED.with(IsolateCell::take).unwrap_or_default();
        write_grouped(db, earlier, &mut report, &mut carry).await;
    }

    report.carried_over = carry.len();
    if !carry.is_empty() {
        let mut waiting = CARRIED.with(IsolateCell::take).unwrap_or_default();
        waiting.append(&mut carry);
        let overflow = waiting.len().saturating_sub(CARRY_OVER_CAP);
        waiting.drain(..overflow);
        report.dropped = overflow;
        CARRIED.with(|slot| slot.set(waiting));
    }
    report
}

/// One `create_many` per table, in first-seen order, each no larger than what
/// the invocation has left. Rows that do not fit, or that the budget refuses,
/// go to `carry`; other failures to `report`.
async fn write_grouped(
    db: &dyn DatabaseService,
    rows: Vec<QueuedRequestLog>,
    report: &mut PersistReport,
    carry: &mut Vec<QueuedRequestLog>,
) {
    let mut groups: Vec<(&'static str, Vec<HashMap<String, serde_json::Value>>)> = Vec::new();
    for row in rows {
        match groups.iter_mut().find(|(table, _)| *table == row.table) {
            Some((_, data)) => data.push(row.data),
            None => groups.push((row.table, vec![row.data])),
        }
    }
    for (table, mut data) in groups {
        while !data.is_empty() {
            // Never ask for more statements than the invocation has left: a
            // write past what is left is refused whole, and one past the whole
            // limit is refused as `InvalidArgument`, which carrying it over
            // would not fix. What does not fit now is carried.
            let room = match db.statement_budget() {
                Ok(StatementBudget::Limited { limit, used }) => {
                    usize::try_from(limit.saturating_sub(used)).unwrap_or(usize::MAX)
                }
                Ok(StatementBudget::Unbounded) | Err(_) => data.len(),
            };
            if room == 0 {
                carry.extend(data.drain(..).map(|data| QueuedRequestLog { table, data }));
                break;
            }
            let rest = data.split_off(room.min(data.len()));
            match db.create_many(table, data.clone()).await {
                Ok(_) => {}
                Err(DatabaseError::ResourceExhausted(_)) => {
                    carry.extend(
                        data.into_iter()
                            .chain(rest)
                            .map(|data| QueuedRequestLog { table, data }),
                    );
                    break;
                }
                Err(error) => report.failures.push(PersistFailure {
                    table,
                    rows: data.len(),
                    error: error.to_string(),
                }),
            }
            data = rest;
        }
    }
}

/// Discard every carried-over row. Tests only — a tokio worker thread
/// outlives a fixture.
#[cfg(test)]
fn clear_carried_for_test() {
    CARRIED.with(IsolateCell::clear);
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, Mutex,
        },
    };

    use serde_json::json;

    use super::*;

    const TABLE: &str = "test__audit__rows";

    fn row(label: &str) -> QueuedRequestLog {
        let mut data = HashMap::new();
        data.insert("path".to_string(), json!(label));
        QueuedRequestLog { table: TABLE, data }
    }

    fn labels(rows: &[QueuedRequestLog]) -> Vec<String> {
        rows.iter()
            .map(|row| row.data["path"].as_str().unwrap().to_string())
            .collect()
    }

    /// Yield once, so a `join!` of two requests polls the other one before
    /// this one continues.
    async fn yield_once() {
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
    }

    /// Two requests interleaved in one isolate, each queueing audit rows on
    /// both sides of a yield: each request's queue holds its own rows and no
    /// other's, and nothing is queued outside a request.
    #[tokio::test]
    async fn interleaved_requests_each_queue_only_their_own_rows() {
        let queue_a = RequestLogQueue::new();
        let queue_b = RequestLogQueue::new();
        let order = RefCell::new(Vec::new());
        let request = |name: &'static str| {
            let order = &order;
            async move {
                enqueue(row(&format!("{name}-1"))).unwrap_or_else(|_| panic!("in scope"));
                order.borrow_mut().push(name);
                yield_once().await;
                enqueue(row(&format!("{name}-2"))).unwrap_or_else(|_| panic!("in scope"));
                order.borrow_mut().push(name);
            }
        };
        tokio::join!(
            queue_request_logs(Rc::clone(&queue_a), request("a")),
            queue_request_logs(Rc::clone(&queue_b), request("b")),
        );

        let mut first_halves = order.borrow()[..2].to_vec();
        first_halves.sort_unstable();
        assert_eq!(
            first_halves,
            ["a", "b"],
            "both requests must queue a row before either queues its second, or \
             this test is not interleaving them: {:?}",
            order.borrow()
        );
        assert_eq!(labels(&queue_a.take()), ["a-1", "a-2"]);
        assert_eq!(labels(&queue_b.take()), ["b-1", "b-2"]);
        assert!(
            enqueue(row("outside")).is_err(),
            "outside a request there is no queue, and the row comes back to be inserted"
        );
    }

    /// A database for one invocation: `create_many` is admitted against
    /// `limit` statements, one per row, and records what it wrote in the
    /// shared `written`. Every other operation is the in-memory SQLite
    /// service's and is never called.
    struct InvocationDb {
        inner: Arc<dyn DatabaseService>,
        limit: u64,
        used: AtomicU64,
        written: Written,
    }

    /// What every invocation's database wrote, in order.
    type Written = Arc<Mutex<Vec<String>>>;

    impl InvocationDb {
        fn new(limit: u64, written: &Written) -> Self {
            Self {
                inner: Arc::new(
                    wafer_block_sqlite::service::SQLiteDatabaseService::open_in_memory()
                        .expect("in-memory sqlite"),
                ),
                limit,
                used: AtomicU64::new(0),
                written: Arc::clone(written),
            }
        }

        fn inner_service(&self) -> &dyn DatabaseService {
            self.inner.as_ref()
        }

        fn spent(limit: u64, written: &Written) -> Self {
            let db = Self::new(limit, written);
            db.used.store(limit, Ordering::Relaxed);
            db
        }

        fn used(&self) -> u64 {
            self.used.load(Ordering::Relaxed)
        }
    }

    wafer_core::forward_database_service! {
        impl DatabaseService for InvocationDb {
            forward_to inner_service();

            ops {
                get: forward,
                list: forward,
                create: forward,
                create_many: custom,
                update: forward,
                delete: forward,
                count: forward,
                sum: forward,
                query_raw: forward,
                exec_raw: forward,
                delete_where: forward,
                delete_where_count: forward,
                take_where: forward,
                update_where: forward,
                update_where_count: forward,
                increment_field_where: forward,
                upsert: forward,
                aggregate: forward,
                batch: forward,
                insert_guarded: forward,
                update_guarded: forward,
                ensure_schema_table: forward,
                ensure_schema_tables: forward,
                schema_table_exists: forward,
                schema_columns: forward,
                schema_drop_table: forward,
                schema_add_column: forward,
                set_strict_schema: forward,
                statement_budget: custom,
            }

            async fn create_many(
                &self,
                _collection: &str,
                rows: Vec<HashMap<String, serde_json::Value>>,
            ) -> Result<i64, DatabaseError> {
                self.statement_budget()?.admit(rows.len(), "create_many")?;
                self.used.fetch_add(rows.len() as u64, Ordering::Relaxed);
                for row in &rows {
                    self.written
                        .lock()
                        .unwrap()
                        .push(row["path"].as_str().unwrap().to_string());
                }
                Ok(rows.len() as i64)
            }

            fn statement_budget(&self) -> Result<StatementBudget, DatabaseError> {
                Ok(StatementBudget::Limited {
                    limit: self.limit,
                    used: self.used(),
                })
            }
        }
    }

    /// Each request's rows are written through its own invocation's database,
    /// and a row its invocation had no statements left for is carried to the
    /// next request with budget to spare instead of being dropped.
    #[tokio::test]
    async fn a_row_refused_for_the_budget_is_carried_to_the_next_request() {
        clear_carried_for_test();
        let written = Written::default();

        // Request A has spent its invocation: its row cannot be written now.
        let spent = InvocationDb::spent(1, &written);
        let report = persist(&spent, vec![row("a")]).await;
        assert_eq!(report.carried_over, 1, "{report:?}");
        assert!(report.failures.is_empty(), "{report:?}");
        assert!(written.lock().unwrap().is_empty());

        // Request B has room for its own row and A's.
        let fresh = InvocationDb::new(10, &written);
        let report = persist(&fresh, vec![row("b")]).await;
        assert_eq!(report.carried_over, 0, "{report:?}");
        assert_eq!(
            *written.lock().unwrap(),
            ["b", "a"],
            "own rows first, then the carried one"
        );
        assert_eq!(fresh.used(), 2);

        // Nothing is left waiting.
        let idle = InvocationDb::new(10, &written);
        persist(&idle, Vec::new()).await;
        assert_eq!(idle.used(), 0, "the carried row was written once");
    }

    /// A request with room for only some of its own rows writes those,
    /// carries the rest, and leaves earlier carried rows waiting: it is not
    /// charged for another request's rows while its own do not fit.
    #[tokio::test]
    async fn a_request_out_of_budget_is_not_charged_for_carried_rows() {
        clear_carried_for_test();
        let written = Written::default();

        let spent = InvocationDb::spent(1, &written);
        persist(&spent, vec![row("a")]).await;

        let tight = InvocationDb::new(1, &written);
        let report = persist(&tight, vec![row("b1"), row("b2")]).await;
        assert_eq!(report.carried_over, 1, "{report:?}");
        assert!(report.failures.is_empty(), "{report:?}");
        assert_eq!(*written.lock().unwrap(), ["b1"]);

        let fresh = InvocationDb::new(10, &written);
        persist(&fresh, Vec::new()).await;
        let mut all = written.lock().unwrap().clone();
        all.sort();
        assert_eq!(all, ["a", "b1", "b2"]);
    }

    /// More carried rows than a whole invocation may write are written a
    /// limit's worth at a time, never refused as too large for any
    /// invocation.
    #[tokio::test]
    async fn carried_rows_are_written_no_more_than_the_limit_at_a_time() {
        clear_carried_for_test();
        let written = Written::default();
        let spent = InvocationDb::spent(5, &written);
        let rows = (0..12).map(|i| row(&format!("r{i}"))).collect();
        assert_eq!(persist(&spent, rows).await.carried_over, 12);

        for expected in [5, 10, 12] {
            let report = persist(&InvocationDb::new(5, &written), Vec::new()).await;
            assert!(report.failures.is_empty(), "{report:?}");
            assert_eq!(written.lock().unwrap().len(), expected);
        }
        clear_carried_for_test();
    }

    /// Past [`CARRY_OVER_CAP`] the oldest carried rows are dropped, and the
    /// report says how many.
    #[tokio::test]
    async fn carried_rows_past_the_cap_are_dropped_and_counted() {
        clear_carried_for_test();
        let written = Written::default();
        let spent = InvocationDb::spent(1, &written);
        let rows = (0..CARRY_OVER_CAP + 3)
            .map(|i| row(&format!("r{i}")))
            .collect();
        let report = persist(&spent, rows).await;
        assert_eq!(report.dropped, 3, "{report:?}");
        clear_carried_for_test();
    }
}
