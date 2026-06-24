//! The deterministic, single-threaded driver underneath the model.
//!
//! The executor is deliberately *dumb*: it owns the process futures and their
//! wakers and runs whichever process is runnable, in a replay-stable FIFO order.
//! It makes no scheduling *decisions* — all interleaving control lives in the
//! synchronization primitives (via wakers) and the strategy layer above. Given
//! the same processes and the same wake pattern it produces an identical
//! execution, which is what lets the strategy replay prefixes.

use std::{
    cell::Cell,
    collections::VecDeque,
    error::Error,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Wake, Waker},
};

use super::process::{self, ProcessID};

struct Process<'a> {
    future: Pin<Box<dyn Future<Output = process::ProcessResult> + 'a>>,
    waker: Waker,
}

// The Mutex only exists to satisfy `Wake: Send + Sync`; execution is
// single-threaded, so there is no real contention.
struct Inbox {
    woken: Mutex<VecDeque<ProcessID>>,
}

struct ProcessWaker {
    id: ProcessID,
    inbox: Arc<Inbox>,
}

impl Wake for ProcessWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.inbox.woken.lock().unwrap().push_back(self.id);
    }
}

/// The single-threaded process driver: a process table indexed by [`ProcessID`],
/// a FIFO run queue, and a shared inbox of woken process ids.
pub(crate) struct Executor<'a> {
    processes: Vec<Option<Process<'a>>>,
    queue: VecDeque<ProcessID>,
    inbox: Arc<Inbox>,
    // Count of live (scheduled but not completed) processes, maintained incrementally
    // so `pending` is O(1) on the hot path instead of scanning the process table.
    live: usize,
    // Scratch buffer reused by `flush_wakes` so the per-poll drain does not allocate.
    flush_buf: Vec<ProcessID>,
}

impl<'a> Executor<'a> {
    /// Registers a process future, returning its [`ProcessID`] (== its push index),
    /// and enqueues it as runnable.
    pub(crate) fn schedule(
        &mut self,
        code: impl Future<Output = process::ProcessResult> + 'a,
    ) -> ProcessID {
        let id = self.processes.len();
        let waker = Waker::from(Arc::new(ProcessWaker {
            id,
            inbox: Arc::clone(&self.inbox),
        }));
        self.processes.push(Some(Process {
            future: Box::pin(code),
            waker,
        }));
        self.live += 1;
        self.queue.push_back(id);
        id
    }

    /// Number of live processes: scheduled but not yet completed (completion drops
    /// the slot). Used to distinguish a clean finish from a deadlock.
    pub(crate) fn pending(&self) -> usize {
        self.live
    }

    /// Runs the poll loop until the run queue drains, returning the first process
    /// error as a [`RawProcessError`]. Does not detect deadlock — the strategy
    /// does, by comparing [`pending`](Self::pending) against the enabled set.
    pub(crate) fn execute(&mut self) -> Result<(), RawProcessError> {
        loop {
            // The leading flush also picks up wakes that arrived between calls
            // (e.g. the strategy committing a transition).
            self.flush_wakes();
            let Some(id) = self.queue.pop_front() else {
                return Ok(());
            };
            match self.poll(id) {
                Poll::Ready(Ok(())) => {
                    self.processes[id] = None;
                    self.live -= 1;
                }
                Poll::Ready(Err(e)) => return Err(RawProcessError { pid: id, source: e }),
                Poll::Pending => {}
            }
        }
    }

    // Polls one process once, with `pid()` set to it for the duration of the poll.
    fn poll(&mut self, id: ProcessID) -> Poll<process::ProcessResult> {
        let _guard = Guard::enter(id);
        // Disjoint field borrows: the waker for the Context, the future to poll —
        // so no clone is needed.
        let process = self.processes[id].as_mut().unwrap();
        process
            .future
            .as_mut()
            .poll(&mut Context::from_waker(&process.waker))
    }

    // Moves woken processes back onto the queue. Promotes in ascending-pid order
    // so the FIFO tie-break stays replay-stable however the wakes happened to
    // fire, and skips any process already queued (self-wake / double-wake). Drains
    // into a reused scratch buffer and skips the sort/dedup when at most one process
    // woke (the common single-wake case), so the hot path allocates nothing.
    fn flush_wakes(&mut self) {
        {
            let mut woken = self.inbox.woken.lock().unwrap();
            if woken.is_empty() {
                return;
            }
            self.flush_buf.extend(woken.drain(..));
        }
        if self.flush_buf.len() > 1 {
            self.flush_buf.sort_unstable();
            self.flush_buf.dedup();
        }
        for i in 0..self.flush_buf.len() {
            let id = self.flush_buf[i];
            let live = matches!(self.processes.get(id), Some(Some(_)));
            if live && !self.queue.contains(&id) {
                self.queue.push_back(id);
            }
        }
        self.flush_buf.clear();
    }
}

impl Default for Executor<'_> {
    fn default() -> Self {
        Self {
            processes: Vec::new(),
            queue: VecDeque::new(),
            inbox: Arc::new(Inbox {
                woken: Mutex::new(VecDeque::new()),
            }),
            live: 0,
            flush_buf: Vec::new(),
        }
    }
}

thread_local! {
    static CURRENT: Cell<Option<ProcessID>> = const { Cell::new(None) };
}

struct Guard;

impl Guard {
    fn enter(id: ProcessID) -> Self {
        CURRENT.with(|c| c.set(Some(id)));
        Guard
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(None));
    }
}

/// The [`ProcessID`] of the process currently being polled.
///
/// This is the hook synchronization primitives use to stamp [`Transition`](super::Transition)s
/// with their operating process. Panics if called outside an executor poll.
pub(crate) fn pid() -> ProcessID {
    CURRENT
        .with(Cell::get)
        .expect("pid() called outside an executor poll")
}

/// A process error carrying only the raw [`ProcessID`], as known to the executor.
///
/// `World::run` promotes this into the public, name-bearing
/// [`ProcessError`](super::ProcessError).
#[derive(Debug)]
pub(crate) struct RawProcessError {
    pub(crate) pid: ProcessID,
    pub(crate) source: Box<dyn Error + Send + Sync>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::poll_fn;

    #[test]
    fn completes_ok() {
        let mut exec = Executor::default();
        exec.schedule(async { Ok(()) });
        assert!(exec.execute().is_ok());
    }

    #[test]
    fn process_error() {
        let mut exec = Executor::default();
        exec.schedule(async { Err("boom".into()) });
        let err = exec.execute().unwrap_err();
        assert!(matches!(err, RawProcessError { pid: 0, .. }));
    }

    // A process that wakes itself while returning Pending must be re-queued and
    // polled again rather than dropped: the run must still terminate cleanly.
    #[test]
    fn self_yield() {
        let mut exec = Executor::default();
        let mut yielded = false;
        exec.schedule(poll_fn(move |cx| {
            if yielded {
                Poll::Ready(Ok(()))
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }));
        assert!(exec.execute().is_ok());
    }
}
