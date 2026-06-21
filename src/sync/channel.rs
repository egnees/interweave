//! An unbounded MPSC channel whose send / recv operations are `.await` yield points.
//!
//! [`Sender`] (cloneable, multi-producer) and [`Receiver`] (`!Clone`, single-consumer) are handles
//! to one shared [`Channel`]. Every operation registers itself on its first poll and yields, so the
//! search strategy decides *when* each send or recv commits relative to operations on the same
//! channel. Each commit is a distinct scheduling point with a [`Transition`] the model checker can
//! reorder.
//!
//! Operations are split into registration and commit exactly like [`Atomic`](crate::Atomic):
//! registration records the intended op and its waker; commit (via [`Object::apply`]) mutates the
//! queue and wakes the committing process so its `.await` resolves. A `recv` registered against an
//! empty queue is withheld from [`enabled`](Object::enabled) — that is how the consumer blocks, and
//! `State::settle` turns a live consumer with nothing enabled into a deadlock.

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    fmt::Debug,
    future::poll_fn,
    rc::Rc,
    task::{Poll, Waker},
};

use crate::model::{Object, ObjectID, Transition};

// A send carries its value (moved out by the commit) and a flag the commit sets to release the
// sender; a recv carries the slot the commit writes the popped value into.
enum Op<T> {
    Send { value: T, done: Rc<Cell<bool>> },
    Recv { slot: Rc<Cell<Option<T>>> },
}

// The op kind resolved for dependency tests, carrying the recv's consumed send-seq (`None` for a
// still-pending recv, which has not consumed anything yet).
enum Kind {
    Send,
    Recv { consumed: Option<usize> },
}

struct Request<T> {
    transition: Transition,
    waker: Waker,
    op: Op<T>,
}

// `value` is rendered at commit time (purely for `label`; the value itself moves through the
// queue and is otherwise not kept).
enum Record {
    Send { value: String },
    Recv { consumed: usize, value: String },
}

struct Channel<T> {
    id: ObjectID,
    seq: usize,
    // Committed-but-unreceived messages: (producing send's seq, value).
    queue: VecDeque<(usize, T)>,
    requests: Vec<Request<T>>,
    history: Vec<(Transition, Record)>,
}

impl<T: Debug> Channel<T> {
    fn new(id: ObjectID) -> Self {
        Self {
            id,
            seq: 0,
            queue: VecDeque::new(),
            requests: Vec::new(),
            history: Vec::new(),
        }
    }

    fn register(&mut self, op: Op<T>, waker: Waker) {
        let transition = Transition::new(self.id, self.seq);
        self.seq += 1;
        self.requests.push(Request {
            transition,
            waker,
            op,
        });
    }

    // Commits one pending op: a send enqueues its value; a recv pops the head. Only the committing
    // op's process is woken — a blocked recv becomes selectable through `enabled` reading the
    // queue, not by being re-queued in the executor (symmetric with `Atomic::apply`).
    fn apply(&mut self, t: Transition) {
        let Some(i) = self.requests.iter().position(|r| r.transition == t) else {
            panic!("transition must be enabled");
        };
        let req = self.requests.remove(i);
        match req.op {
            Op::Send { value, done } => {
                self.history.push((
                    t,
                    Record::Send {
                        value: format!("{value:?}"),
                    },
                ));
                self.queue.push_back((t.seq, value));
                done.set(true);
            }
            Op::Recv { slot } => {
                let (send_seq, value) = self.queue.pop_front().expect("recv must be enabled");
                self.history.push((
                    t,
                    Record::Recv {
                        consumed: send_seq,
                        value: format!("{value:?}"),
                    },
                ));
                slot.set(Some(value));
            }
        }
        req.waker.wake();
    }

    // Sends never block; a recv blocks while the queue is empty. Insertion order is fixed for
    // replay determinism.
    fn enabled(&self) -> Vec<Transition> {
        let queue_nonempty = !self.queue.is_empty();
        self.requests
            .iter()
            .filter(|r| match r.op {
                Op::Send { .. } => true,
                Op::Recv { .. } => queue_nonempty,
            })
            .map(|r| r.transition)
            .collect()
    }

    // Resolves a transition's kind whether still pending or already committed; DPOR asks about a
    // past transition (in `history`) against a process's next op (still in `requests`).
    fn kind_of(&self, t: Transition) -> Kind {
        if let Some(req) = self.requests.iter().find(|r| r.transition == t) {
            return match req.op {
                Op::Send { .. } => Kind::Send,
                Op::Recv { .. } => Kind::Recv { consumed: None },
            };
        }
        let (_, rec) = self
            .history
            .iter()
            .find(|(tt, _)| *tt == t)
            .expect("transition not registered on this channel");
        match rec {
            Record::Send { .. } => Kind::Send,
            Record::Recv { consumed, .. } => Kind::Recv {
                consumed: Some(*consumed),
            },
        }
    }

    // Two sends into one FIFO are dependent (the single consumer reads them in enqueue order, so
    // the order is observable). A send and a recv are dependent only when the recv consumed *this*
    // send (the causal send→recv edge); a concurrent send appended behind the popped element
    // commutes with the recv. Two recvs share the one consumer (program order handles them), and a
    // pending recv has consumed nothing yet, so it is independent.
    fn depends(&self, t1: Transition, t2: Transition) -> bool {
        match (self.kind_of(t1), self.kind_of(t2)) {
            (Kind::Send, Kind::Send) => true,
            (Kind::Recv { .. }, Kind::Recv { .. }) => false,
            (Kind::Send, Kind::Recv { consumed }) => consumed == Some(t1.seq),
            (Kind::Recv { consumed }, Kind::Send) => consumed == Some(t2.seq),
        }
    }

    fn label(&self, t: &Transition) -> String {
        let (_, rec) = self
            .history
            .iter()
            .find(|(tt, _)| tt == t)
            .expect("label called on an unapplied transition");
        match rec {
            Record::Send { value } => format!("send {value}"),
            Record::Recv { consumed, value } => format!("recv -> {value} (#{consumed})"),
        }
    }
}

// The internal `Object` the world drives. `Sender`/`Receiver` share the same `Rc`, but they are not
// `Clone`-symmetric (a `Receiver` must not be cloneable), and `World::register` needs a
// `Clone + Object` handle — this is that handle.
pub(crate) struct ChannelHandle<T> {
    chan: Rc<RefCell<Channel<T>>>,
}

impl<T> Clone for ChannelHandle<T> {
    fn clone(&self) -> Self {
        Self {
            chan: Rc::clone(&self.chan),
        }
    }
}

impl<T: Debug> ChannelHandle<T> {
    pub(crate) fn new(id: ObjectID) -> Self {
        Self {
            chan: Rc::new(RefCell::new(Channel::new(id))),
        }
    }

    // The shared state, for splitting the driver into producer/consumer halves.
    pub(crate) fn split(&self) -> (Sender<T>, Receiver<T>) {
        let chan = Rc::clone(&self.chan);
        (
            Sender {
                chan: Rc::clone(&chan),
            },
            Receiver { chan },
        )
    }
}

impl<T: Debug + 'static> Object for ChannelHandle<T> {
    fn apply(&mut self, t: Transition) {
        self.chan.borrow_mut().apply(t);
    }

    fn enabled(&self) -> Vec<Transition> {
        self.chan.borrow().enabled()
    }

    fn label(&self, t: &Transition) -> String {
        self.chan.borrow().label(t)
    }

    fn depends(&self, t1: Transition, t2: Transition) -> bool {
        self.chan.borrow().depends(t1, t2)
    }
}

/// The sending half of an MPSC channel; cloneable, so several producers can share it.
///
/// [`send`](Sender::send) is an `async` method: awaiting it registers the send and yields, so the
/// commit becomes a [`Transition`] the strategy schedules against other operations on the channel.
pub struct Sender<T> {
    chan: Rc<RefCell<Channel<T>>>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            chan: Rc::clone(&self.chan),
        }
    }
}

impl<T: Debug> Sender<T> {
    /// Enqueues `value` at the back of the channel.
    ///
    /// Awaiting this is a scheduling point: the value is enqueued when the strategy selects this
    /// send's [`Transition`]. Always succeeds — the channel is unbounded.
    pub async fn send(&self, value: T) {
        // First poll registers the send and yields; the commit sets `done` and wakes us, and the
        // next poll sees the flag and resolves. `value` is moved into the request at registration.
        let done = Rc::new(Cell::new(false));
        let mut pending = Some(value);
        poll_fn(move |cx| {
            if done.get() {
                return Poll::Ready(());
            }
            if let Some(value) = pending.take() {
                self.chan.borrow_mut().register(
                    Op::Send {
                        value,
                        done: Rc::clone(&done),
                    },
                    cx.waker().clone(),
                );
            }
            Poll::Pending
        })
        .await
    }
}

/// The receiving half of an MPSC channel. Deliberately **not** `Clone`: the dependency relation
/// relies on there being a single consumer, and `!Clone` enforces that at the type level.
///
/// [`recv`](Receiver::recv) is an `async` method: awaiting it registers the recv and yields. A recv
/// on an empty channel stays blocked (withheld from the enabled set) until a send makes the queue
/// non-empty and the strategy selects this recv.
pub struct Receiver<T> {
    chan: Rc<RefCell<Channel<T>>>,
}

impl<T: Debug> Receiver<T> {
    /// Removes and returns the message at the front of the channel, blocking while it is empty.
    ///
    /// Awaiting this is a scheduling point: the recv commits (and the head is popped) when the
    /// strategy selects this recv's [`Transition`], which only happens once the queue is non-empty.
    pub async fn recv(&self) -> T {
        // First poll registers the recv and yields; the commit fills `slot` and wakes us, and the
        // next poll reads the popped value back.
        let slot = Rc::new(Cell::new(None));
        let mut registered = false;
        poll_fn(move |cx| {
            if let Some(value) = slot.take() {
                return Poll::Ready(value);
            }
            if !registered {
                registered = true;
                self.chan.borrow_mut().register(
                    Op::Recv {
                        slot: Rc::clone(&slot),
                    },
                    cx.waker().clone(),
                );
            }
            Poll::Pending
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Executor, ProcessResult};
    use std::future::Future;

    // A fresh channel with id 0, driver plus its two halves.
    fn make() -> (ChannelHandle<i32>, Sender<i32>, Receiver<i32>) {
        let driver = ChannelHandle::new(0);
        let (tx, rx) = driver.split();
        (driver, tx, rx)
    }

    // Drives the strategy by hand: keep committing the first enabled transition and resuming the
    // executor until every process finishes. The driver handle is any clone of the shared channel.
    fn drive(exec: &mut Executor, obj: &mut impl Object) {
        exec.execute().unwrap();
        while let Some(&t) = obj.enabled().first() {
            obj.apply(t);
            exec.execute().unwrap();
        }
    }

    // Runs `body` as the only process against a fresh channel.
    fn run_single(
        body: impl FnOnce(Sender<i32>, Receiver<i32>) -> Box<dyn Future<Output = ProcessResult>>,
    ) {
        let (mut driver, tx, rx) = make();
        let mut exec = Executor::default();
        exec.schedule(Box::into_pin(body(tx, rx)));
        drive(&mut exec, &mut driver);
    }

    // The first enabled op of process `pid` on the channel.
    fn enabled_of(handle: &ChannelHandle<i32>, pid: usize) -> Transition {
        *handle.enabled().iter().find(|t| t.pid == pid).unwrap()
    }

    #[test]
    fn send_then_recv_returns_value() {
        let seen = Rc::new(Cell::new(0));
        let dst = seen.clone();
        run_single(move |tx, rx| {
            Box::new(async move {
                tx.send(7).await;
                dst.set(rx.recv().await);
                Ok(())
            })
        });
        assert_eq!(seen.get(), 7);
    }

    #[test]
    fn fifo_order_for_one_producer() {
        let first = Rc::new(Cell::new(0));
        let second = Rc::new(Cell::new(0));
        let (a, b) = (first.clone(), second.clone());
        run_single(move |tx, rx| {
            Box::new(async move {
                tx.send(1).await;
                tx.send(2).await;
                a.set(rx.recv().await);
                b.set(rx.recv().await);
                Ok(())
            })
        });
        assert_eq!(first.get(), 1);
        assert_eq!(second.get(), 2);
    }

    // A recv against an empty channel is withheld from `enabled` — that is the blocking mechanism.
    #[test]
    fn recv_on_empty_blocks() {
        let (driver, _tx, rx) = make();
        let mut exec = Executor::default();
        exec.schedule(async move {
            rx.recv().await;
            Ok(())
        });
        exec.execute().unwrap();
        assert!(
            driver.enabled().is_empty(),
            "recv must block on an empty queue"
        );
    }

    // The dependency truth table on a hand-driven executor: two sends from distinct producers and
    // one consumer. send/send is dependent; the recv depends only on the send it actually consumes.
    #[test]
    fn dependency_truth_table() {
        let (mut driver, tx, rx) = make();
        let tx2 = tx.clone();
        let mut exec = Executor::default();
        exec.schedule(async move {
            tx.send(10).await;
            Ok(())
        });
        exec.schedule(async move {
            tx2.send(20).await;
            Ok(())
        });
        exec.schedule(async move {
            rx.recv().await;
            Ok(())
        });
        exec.execute().unwrap();

        // Two registered sends and a blocked recv: only the sends are enabled.
        let send0 = enabled_of(&driver, 0);
        let send1 = enabled_of(&driver, 1);
        assert_eq!(driver.enabled().len(), 2);
        assert!(driver.depends(send0, send1), "send/send is dependent");

        // Commit send0, then the recv consuming it; the recv depends on send0 but not on send1.
        driver.apply(send0);
        exec.execute().unwrap();
        let recv = enabled_of(&driver, 2);
        driver.apply(recv);
        exec.execute().unwrap();

        assert!(
            driver.depends(send0, recv),
            "recv depends on the send it consumed"
        );
        assert!(
            !driver.depends(send1, recv),
            "recv is independent of the unconsumed send"
        );
        assert!(
            !driver.depends(recv, send1),
            "symmetric: unconsumed send is independent"
        );
    }
}
