//! Work a handler hands off to run after its response has gone out.
//!
//! A handler that must answer every caller alike — the public auth endpoints
//! that may not reveal whether an address has an account — cannot also do
//! slow work on only some of its paths: the reply would be the same bytes,
//! but it would arrive later for a registered address than for an unknown
//! one. [`defer`] takes that work off the response path. The handler returns
//! at once, on every path, and the work runs afterwards.
//!
//! Where "afterwards" runs depends on the host, so the host picks the mode:
//!
//! - [`DeferMode::Spawn`] (the default; native and the browser runtime): the
//!   task is spawned on the current executor with
//!   [`wafer_block::spawn_producer`] — `tokio::spawn` on native, where the
//!   process outlives any one request, and `spawn_local` in the browser.
//! - [`DeferMode::Queued`] (Cloudflare): the task is queued on this isolate
//!   and the platform entry takes it with [`drain`] after dispatch and hands
//!   it to `ctx.wait_until`. A task spawned any other way on Workers is not
//!   tied to an event and is cancelled once the response is sent, which is
//!   why this mode exists. The entry also re-installs the request's service
//!   bindings around each task, because on Workers those are request-scoped.
//!
//! Tests select [`DeferMode::Queued`] too and run what [`drain`] hands back,
//! which makes "this path deferred a send" observable without a clock.
//!
//! A deferred task has no response to report into, so it logs its own
//! failures. Anything whose failure the caller must hear about does not
//! belong here.
//!
//! A task can also be dropped before it finishes: a native process shutting
//! down drops every task its runtime still holds, a browser tab closing drops
//! its `spawn_local` tasks, and a queue nothing drains is dropped with the
//! isolate. Each task carries a guard that logs a warning when that happens
//! ([`Unfinished`]), so a mail lost to a restart leaves a line. A Cloudflare
//! hard stop is the exception: it runs no destructors at all.

use std::{cell::Cell, future::Future, pin::Pin};

use wafer_block::MaybeSend;

/// A task queued by [`defer`] under [`DeferMode::Queued`].
pub type DeferredTask = Pin<Box<dyn Future<Output = ()>>>;

/// How [`defer`] runs a task on this thread (isolate). See the module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferMode {
    Spawn,
    Queued,
}

thread_local! {
    static MODE: Cell<DeferMode> = const { Cell::new(DeferMode::Spawn) };
    /// [`IsolateCell`](crate::IsolateCell), not `RefCell`, for the reason
    /// `pipeline`'s request-log queue gives: a Cloudflare hard stop inside a
    /// push must not strand a borrow flag for the life of the isolate.
    static QUEUE: crate::IsolateCell<Vec<DeferredTask>> = const { crate::IsolateCell::new() };
}

/// Select how [`defer`] runs tasks on this thread. The Cloudflare entry sets
/// [`DeferMode::Queued`] once per isolate; native never calls it.
pub fn set_mode(mode: DeferMode) {
    MODE.with(|m| m.set(mode));
}

/// Run `task` after the current response, as this thread's [`DeferMode`]
/// says.
pub fn defer<F>(task: F)
where
    F: Future<Output = ()> + MaybeSend + 'static,
{
    let unfinished = Unfinished;
    let task = async move {
        task.await;
        std::mem::forget(unfinished);
    };
    match MODE.with(Cell::get) {
        DeferMode::Spawn => wafer_block::spawn_producer(task),
        DeferMode::Queued => QUEUE.with(|queue| {
            let mut tasks = queue.take().unwrap_or_default();
            tasks.push(Box::pin(task));
            queue.set(tasks);
        }),
    }
}

/// Take every queued task, clearing the queue. The Cloudflare entry calls
/// this after each dispatch.
pub fn drain() -> Vec<DeferredTask> {
    QUEUE.with(|queue| queue.take().unwrap_or_default())
}

/// How many tasks are queued and not yet drained. For an entry point that
/// does not drain, to say so when something is waiting.
pub fn pending() -> usize {
    QUEUE.with(|queue| {
        let tasks = queue.take();
        let n = tasks.as_ref().map_or(0, Vec::len);
        if let Some(tasks) = tasks {
            queue.set(tasks);
        }
        n
    })
}

/// Travels inside every deferred task and is forgotten when the task
/// completes, so its `Drop` runs only for a task dropped unfinished.
struct Unfinished;

impl Drop for Unfinished {
    fn drop(&mut self) {
        tracing::warn!(
            "a deferred task was dropped before it finished (runtime shutdown, closed page or \
             an undrained queue); work it carried, such as an auth mail, did not happen"
        );
        #[cfg(test)]
        DROPPED_UNFINISHED.with(|n| n.set(n.get() + 1));
    }
}

#[cfg(test)]
thread_local! {
    static DROPPED_UNFINISHED: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use super::*;

    #[tokio::test]
    async fn queued_tasks_wait_for_drain_and_drain_empties_the_queue() {
        set_mode(DeferMode::Queued);
        let ran = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let ran = Arc::clone(&ran);
            defer(async move {
                ran.fetch_add(1, Ordering::SeqCst);
            });
        }
        assert_eq!(ran.load(Ordering::SeqCst), 0, "queued, not run");

        let tasks = drain();
        assert_eq!(tasks.len(), 2);
        assert!(drain().is_empty(), "drain must clear the queue");
        for task in tasks {
            task.await;
        }
        assert_eq!(ran.load(Ordering::SeqCst), 2);
        set_mode(DeferMode::Spawn);
    }

    #[tokio::test]
    async fn spawned_tasks_run_without_a_drain() {
        set_mode(DeferMode::Spawn);
        let (tx, rx) = tokio::sync::oneshot::channel();
        defer(async move {
            tx.send(()).expect("receiver alive");
        });
        assert!(drain().is_empty(), "a spawned task is never queued");
        rx.await.expect("the spawned task ran");
    }

    #[tokio::test]
    async fn a_task_dropped_unfinished_is_reported_and_a_finished_one_is_not() {
        set_mode(DeferMode::Queued);
        let before = DROPPED_UNFINISHED.with(Cell::get);

        defer(async {});
        for task in drain() {
            task.await;
        }
        assert_eq!(
            DROPPED_UNFINISHED.with(Cell::get),
            before,
            "a finished task reports nothing"
        );

        defer(std::future::pending());
        assert_eq!(pending(), 1);
        drop(drain());
        assert_eq!(
            DROPPED_UNFINISHED.with(Cell::get),
            before + 1,
            "a task dropped before it finished is reported"
        );
        set_mode(DeferMode::Spawn);
    }
}
