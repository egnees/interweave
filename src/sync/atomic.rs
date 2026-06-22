//! An atomic cell whose load / store / compare-exchange operations are `.await` yield points.
//!
//! [`Handle`] (re-exported as [`Atomic`](crate::Atomic)) is a cloneable handle to a shared
//! atomic cell. Every operation registers itself with the cell on its first poll and then
//! yields control back to the executor, so the search strategy can decide *when* it commits
//! relative to other processes' operations on the same cell. Each commit is therefore a distinct
//! scheduling point with a [`Transition`] the model checker can reorder.
//!
//! Operations are split into registration and commit: registration records the intended op and
//! its waker; commit (driven by the strategy via [`Object::apply`]) reads the value present at
//! that moment, applies any write, and wakes the process so it can read the observed value back.
//! This is what makes a compare-exchange evaluate its comparison at *commit* time rather than at
//! registration time.

use std::{
    cell::{Cell, RefCell},
    fmt::Debug,
    future::poll_fn,
    rc::Rc,
    task::{Poll, Waker},
};

use crate::model::{Object, ObjectID, Transition};

#[derive(Clone, Copy)]
enum Op<T> {
    Store(T),
    Load,
    CompareExchange { current: T, new: T },
}

impl<T> Op<T> {
    fn is_load(&self) -> bool {
        matches!(self, Op::Load)
    }
}

struct Request<T> {
    transition: Transition,
    waker: Waker,
    op: Op<T>,
    // The commit writes the observed (previous) value here; the future reads it back to resolve
    // its `.await`.
    result: Rc<Cell<Option<T>>>,
}

// `prev` is the value observed at commit time; for a store it is also what was overwritten.
struct Record<T> {
    transition: Transition,
    op: Op<T>,
    prev: T,
}

struct Atomic<T> {
    value: T,
    id: ObjectID,
    requests: Vec<Request<T>>,
    history: Vec<Record<T>>,
    seq: usize,
}

impl<T: Copy + PartialEq + Debug> Atomic<T> {
    fn new(id: ObjectID, value: T) -> Self {
        Self {
            value,
            id,
            requests: Vec::new(),
            history: Vec::new(),
            seq: 0,
        }
    }

    fn register(&mut self, op: Op<T>, waker: Waker, result: Rc<Cell<Option<T>>>) {
        let transition = Transition::new(self.id, self.seq);
        self.seq += 1;
        self.requests.push(Request {
            transition,
            waker,
            op,
            result,
        });
    }

    // Commits one pending op: a store (or a matching compare-exchange) writes the new value, every
    // op observes the value present just before it.
    fn apply(&mut self, t: Transition) {
        let Some(i) = self.requests.iter().position(|r| r.transition == t) else {
            panic!("transition must be enabled");
        };
        let req = self.requests.remove(i);
        let prev = self.value;
        match req.op {
            Op::Store(new) => self.value = new,
            Op::CompareExchange { current, new } if prev == current => self.value = new,
            Op::Load | Op::CompareExchange { .. } => {}
        }
        self.history.push(Record {
            transition: t,
            op: req.op,
            prev,
        });
        req.result.set(Some(prev));
        req.waker.wake();
    }

    fn enabled(&self) -> Vec<Transition> {
        self.requests.iter().map(|r| r.transition).collect()
    }

    // Resolves a transition's op whether it is still pending or already committed. DPOR asks about
    // a past transition (in `history`) against a process's next op (still in `requests`), so both
    // vectors must be searched.
    fn op_of(&self, t: Transition) -> Op<T> {
        let pending = self.requests.iter().map(|r| (r.transition, r.op));
        let committed = self.history.iter().map(|r| (r.transition, r.op));
        pending
            .chain(committed)
            .find(|(transition, _)| *transition == t)
            .map(|(_, op)| op)
            .expect("transition not registered on this atomic")
    }

    // Two ops on the same atomic conflict unless both are loads: loads commute (each observes the
    // same value, neither writes). A store always writes; a CAS may write, and the op kind alone
    // cannot rule that out, so it counts as a write unconditionally — a sound (never
    // under-approximating) choice.
    fn depends(&self, t1: Transition, t2: Transition) -> bool {
        !(self.op_of(t1).is_load() && self.op_of(t2).is_load())
    }

    fn label(&self, t: &Transition) -> String {
        let Some(rec) = self.history.iter().find(|r| r.transition == *t) else {
            panic!("label called on an unapplied transition");
        };
        match rec.op {
            Op::Store(new) => format!("store {new:?} (was {:?})", rec.prev),
            Op::Load => format!("load -> {:?}", rec.prev),
            Op::CompareExchange { current, new } if rec.prev == current => {
                format!("cas({current:?}->{new:?}) ok")
            }
            Op::CompareExchange { current, new } => {
                format!("cas({current:?}->{new:?}) fail: {:?}", rec.prev)
            }
        }
    }
}

/// A cloneable handle to a shared atomic cell.
///
/// Clones share the same underlying cell, so handing a clone to each process gives them a common
/// atomic to operate on. Re-exported from the crate root as [`Atomic`](crate::Atomic).
///
/// Every operation ([`store`](Handle::store), [`load`](Handle::load),
/// [`compare_exchange`](Handle::compare_exchange)) is an `async` method: awaiting it registers the
/// operation and yields, turning the commit into a [`Transition`] the search strategy schedules
/// against other processes' operations on the same cell.
#[derive(Clone)]
pub struct Handle<T: Copy + PartialEq + Debug> {
    atomic: Rc<RefCell<Atomic<T>>>,
}

impl<T: Copy + PartialEq + Debug> Handle<T> {
    pub(crate) fn new(id: ObjectID, value: T) -> Self {
        Self {
            atomic: Rc::new(RefCell::new(Atomic::new(id, value))),
        }
    }

    /// Writes `value` into the cell, returning the value it overwrote.
    ///
    /// Awaiting this is a scheduling point: the write commits when the strategy selects this
    /// operation's [`Transition`], and the returned value is whatever the cell held just before
    /// the commit.
    pub async fn store(&self, value: T) -> T {
        self.request(Op::Store(value)).await
    }

    /// Reads the cell's current value without modifying it.
    ///
    /// Awaiting this is a scheduling point: the read commits when the strategy selects this
    /// operation's [`Transition`], and the returned value is whatever the cell holds at that
    /// moment. Two loads on the same cell are independent (they commute), so the
    /// search need not explore both of their orderings.
    pub async fn load(&self) -> T {
        self.request(Op::Load).await
    }

    /// Stores `new` if, at commit time, the cell still equals `current`.
    ///
    /// The comparison happens at *commit* time, not when the operation is awaited, so an
    /// intervening store from another process is observed. Returns the value seen at commit:
    /// `Ok(current)` if the swap happened, `Err(actual)` otherwise.
    ///
    /// The operation is treated as a potential write — dependent with other writes and
    /// loads — even when the swap ultimately fails.
    pub async fn compare_exchange(&self, current: T, new: T) -> Result<T, T> {
        let prev = self.request(Op::CompareExchange { current, new }).await;
        if prev == current { Ok(prev) } else { Err(prev) }
    }

    // First poll registers the op and yields so the strategy can pick it; the commit fills `result`
    // and wakes us, and the next poll reads it back. A spurious poll before the commit finds
    // `result` empty and stays pending. `op.take()` registers at most once, so re-polling while
    // pending does not register a second op.
    async fn request(&self, op: Op<T>) -> T {
        let result = Rc::new(Cell::new(None));
        let mut op = Some(op);
        poll_fn(move |cx| {
            if let Some(value) = result.get() {
                return Poll::Ready(value);
            }
            if let Some(op) = op.take() {
                self.atomic
                    .borrow_mut()
                    .register(op, cx.waker().clone(), Rc::clone(&result));
            }
            Poll::Pending
        })
        .await
    }
}

impl<T: Copy + PartialEq + Debug + 'static> Object for Handle<T> {
    fn apply(&mut self, t: Transition) {
        self.atomic.borrow_mut().apply(t);
    }

    fn enabled(&self) -> Vec<Transition> {
        self.atomic.borrow().enabled()
    }

    fn label(&self, t: &Transition) -> String {
        self.atomic.borrow().label(t)
    }

    fn depends(&self, t1: Transition, t2: Transition) -> bool {
        self.atomic.borrow().depends(t1, t2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Executor, ProcessResult};
    use std::cell::Cell;
    use std::future::Future;

    // Drives the strategy by hand: keep committing the first enabled transition and resuming the
    // executor until every process finishes. `apply` needs `&mut Handle`, but the cell lives behind
    // an `Rc`, so any clone serves as the `&mut` object.
    fn drive(exec: &mut Executor, obj: &mut impl Object) {
        exec.execute().unwrap();
        while let Some(&t) = obj.enabled().first() {
            obj.apply(t);
            exec.execute().unwrap();
        }
    }

    // Runs `body` as the only process against `atomic`, committing each op in turn.
    fn run_single(atomic: &Handle<u32>, body: impl Future<Output = ProcessResult> + 'static) {
        let mut exec = Executor::default();
        exec.schedule(body);
        drive(&mut exec, &mut atomic.clone());
    }

    // A shared slot a process moves a clone of and writes its observed value into.
    fn slot<T: Copy>(init: T) -> Rc<Cell<T>> {
        Rc::new(Cell::new(init))
    }

    // The first enabled op of process `pid` on `atomic`.
    fn enabled_of(atomic: &Handle<u32>, pid: usize) -> Transition {
        *atomic.enabled().iter().find(|t| t.pid == pid).unwrap()
    }

    #[test]
    fn load_observes_initial_value() {
        let a = Handle::new(0, 42u32);
        let seen = slot(0);

        let (h, dst) = (a.clone(), seen.clone());
        run_single(&a, async move {
            dst.set(h.load().await);
            Ok(())
        });

        assert_eq!(seen.get(), 42);
    }

    #[test]
    fn store_writes_and_returns_previous() {
        let a = Handle::new(0, 1u32);
        let (prev, after) = (slot(0), slot(0));

        let (h, p, af) = (a.clone(), prev.clone(), after.clone());
        run_single(&a, async move {
            p.set(h.store(9).await);
            af.set(h.load().await);
            Ok(())
        });

        assert_eq!(prev.get(), 1);
        assert_eq!(after.get(), 9);
    }

    #[test]
    fn compare_exchange_swaps_on_match() {
        let a = Handle::new(0, 1u32);
        let (res, after) = (slot(Err(0)), slot(0));

        let (h, r, af) = (a.clone(), res.clone(), after.clone());
        run_single(&a, async move {
            r.set(h.compare_exchange(1, 9).await);
            af.set(h.load().await);
            Ok(())
        });

        assert_eq!(res.get(), Ok(1));
        assert_eq!(after.get(), 9);
    }

    // The defining RMW property: the compare happens at commit, not at registration. Both ops are
    // registered first; committing the store *before* the CAS makes the CAS see the fresh value
    // (5 != 1) and report the mismatch.
    #[test]
    fn compare_exchange_observes_commit_time_value() {
        let a = Handle::new(0, 1u32);
        let res = slot(Ok(0));

        let mut exec = Executor::default();
        let (cas_h, r) = (a.clone(), res.clone());
        exec.schedule(async move {
            r.set(cas_h.compare_exchange(1, 9).await);
            Ok(())
        });
        let store_h = a.clone();
        exec.schedule(async move {
            store_h.store(5).await;
            Ok(())
        });
        exec.execute().unwrap();

        let mut obj = a.clone();
        obj.apply(enabled_of(&obj, 1));
        exec.execute().unwrap();
        obj.apply(enabled_of(&obj, 0));
        exec.execute().unwrap();

        assert_eq!(res.get(), Err(5));
    }
}
