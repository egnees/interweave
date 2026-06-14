use std::{
    cell::Cell,
    collections::VecDeque,
    error::Error,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Wake, Waker},
};

use crate::process::{self, ProcessID};

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

pub(crate) struct Executor<'a> {
    processes: Vec<Option<Process<'a>>>,
    queue: VecDeque<ProcessID>,
    inbox: Arc<Inbox>,
}

impl<'a> Executor<'a> {
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
        self.queue.push_back(id);
        id
    }

    // Live processes: scheduled but not yet completed. Completion drops the slot.
    pub(crate) fn pending(&self) -> usize {
        self.processes.iter().filter(|p| p.is_some()).count()
    }

    /// Polls runnable processes until the queue drains, promoting woken
    /// processes before each poll. The leading flush picks up wakes that
    /// arrived between calls (e.g. the strategy committing a transition).
    pub(crate) fn execute(&mut self) -> Result<(), ProcessError> {
        loop {
            self.flush_wakes();
            let Some(id) = self.queue.pop_front() else {
                break;
            };
            let poll = {
                let _guard = Guard::enter(id);
                // Disjoint field borrows: the waker for the Context, the future
                // to poll — so no clone is needed.
                let process = self.processes[id].as_mut().unwrap();
                process
                    .future
                    .as_mut()
                    .poll(&mut Context::from_waker(&process.waker))
            };
            match poll {
                Poll::Ready(Ok(())) => self.processes[id] = None,
                Poll::Ready(Err(e)) => return Err(ProcessError { pid: id, source: e }),
                Poll::Pending => {}
            }
        }
        Ok(())
    }

    // Moves woken processes back onto the queue. Promotes in ascending-pid order
    // so the FIFO tie-break stays replay-stable however the wakes happened to
    // fire, and skips any process already queued (self-wake / double-wake).
    fn flush_wakes(&mut self) {
        let mut woken: Vec<ProcessID> = self.inbox.woken.lock().unwrap().drain(..).collect();
        woken.sort_unstable();
        woken.dedup();
        for id in woken {
            let live = matches!(self.processes.get(id), Some(Some(_)));
            if live && !self.queue.contains(&id) {
                self.queue.push_back(id);
            }
        }
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

pub(crate) fn pid() -> ProcessID {
    CURRENT
        .with(Cell::get)
        .expect("pid() called outside an executor poll")
}

#[derive(Debug, thiserror::Error)]
#[error("process {pid} failed: {source}")]
pub(crate) struct ProcessError {
    pub(crate) pid: ProcessID,
    source: Box<dyn Error>,
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
        assert!(matches!(err, ProcessError { pid: 0, .. }));
    }

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
