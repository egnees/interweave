use std::{
    cell::{Cell, RefCell},
    future::poll_fn,
    rc::Rc,
    task::{Poll, Waker},
};

use crate::{
    executor::pid,
    object::{Object, ObjectID, Transition},
};

enum Op<T> {
    Store(T),
    Load,
    CompareExchange { current: T, new: T },
}

struct Request<T> {
    transition: Transition,
    waker: Waker,
    op: Op<T>,
    // The commit writes the observed (previous) value here for the future to
    // read back on its next poll.
    result: Rc<Cell<Option<T>>>,
}

struct Atomic<T> {
    value: T,
    id: ObjectID,
    requests: Vec<Request<T>>,
    // Registration counter; stamps each request's `Transition::seq`.
    seq: usize,
}

impl<T: Copy + PartialEq> Atomic<T> {
    fn new(id: ObjectID, value: T) -> Self {
        Self {
            value,
            id,
            requests: Vec::new(),
            seq: 0,
        }
    }

    fn register(&mut self, op: Op<T>, waker: Waker, result: Rc<Cell<Option<T>>>) {
        let transition = Transition {
            pid: pid(),
            oid: self.id,
            seq: self.seq,
        };
        self.seq += 1;
        self.requests.push(Request {
            transition,
            waker,
            op,
            result,
        });
    }

    // Commits one pending op: a store (or a matching compare-exchange) writes
    // the new value, every op observes the value present just before it.
    fn apply(&mut self, t: Transition) {
        let i = self
            .requests
            .iter()
            .position(|r| r.transition == t)
            .expect("transition must be enabled");
        let req = self.requests.remove(i);
        let prev = self.value;
        match req.op {
            Op::Store(new) => self.value = new,
            Op::Load => {}
            Op::CompareExchange { current, new } if prev == current => self.value = new,
            Op::CompareExchange { .. } => {}
        }
        req.result.set(Some(prev));
        req.waker.wake();
    }

    fn enabled(&self) -> Vec<Transition> {
        self.requests.iter().map(|r| r.transition).collect()
    }
}

#[derive(Clone)]
pub struct Handle<T: Copy + PartialEq> {
    atomic: Rc<RefCell<Atomic<T>>>,
}

impl<T: Copy + PartialEq> Handle<T> {
    pub(crate) fn new(id: ObjectID, value: T) -> Self {
        Self {
            atomic: Rc::new(RefCell::new(Atomic::new(id, value))),
        }
    }

    /// Returns the previous value.
    pub async fn store(&self, value: T) -> T {
        self.request(Op::Store(value)).await
    }

    pub async fn load(&self) -> T {
        self.request(Op::Load).await
    }

    /// Stores `new` if the value still equals `current`. Returns the value seen
    /// at commit: `Ok(current)` if the swap happened, `Err(actual)` otherwise.
    pub async fn compare_exchange(&self, current: T, new: T) -> Result<T, T> {
        let prev = self.request(Op::CompareExchange { current, new }).await;
        if prev == current { Ok(prev) } else { Err(prev) }
    }

    // First poll registers the op and yields so the strategy can pick it; the
    // commit fills `result` and wakes us, and the next poll reads it back. A
    // spurious poll before the commit finds `result` empty and stays pending.
    async fn request(&self, op: Op<T>) -> T {
        let result = Rc::new(Cell::new(None));
        let mut op = Some(op);
        poll_fn(move |cx| match result.get() {
            Some(value) => Poll::Ready(value),
            None => {
                if let Some(op) = op.take() {
                    self.atomic
                        .borrow_mut()
                        .register(op, cx.waker().clone(), Rc::clone(&result));
                }
                Poll::Pending
            }
        })
        .await
    }
}

impl<T: Copy + PartialEq + 'static> Object for Handle<T> {
    fn apply(&mut self, t: Transition) {
        self.atomic.borrow_mut().apply(t);
    }

    fn enabled(&self) -> Vec<Transition> {
        self.atomic.borrow().enabled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::Executor;
    use std::cell::Cell;

    // Drives the strategy by hand: keep committing the first enabled transition
    // and resuming the executor until the process finishes. `apply` needs
    // `&mut Handle`, but the state lives behind an `Rc`, so any clone works.
    fn drive(exec: &mut Executor, obj: &mut impl Object) {
        exec.execute().unwrap();
        while let Some(&t) = obj.enabled().first() {
            obj.apply(t);
            exec.execute().unwrap();
        }
    }

    #[test]
    fn load_observes_initial_value() {
        let a = Handle::new(0, 42u32);
        let out = Rc::new(Cell::new(0));

        let mut exec = Executor::default();
        let h = a.clone();
        let o = out.clone();
        exec.schedule(async move {
            o.set(h.load().await);
            Ok(())
        });

        drive(&mut exec, &mut a.clone());
        assert_eq!(out.get(), 42);
    }

    #[test]
    fn store_writes_and_returns_previous() {
        let a = Handle::new(0, 1u32);
        let prev = Rc::new(Cell::new(0));
        let after = Rc::new(Cell::new(0));

        let mut exec = Executor::default();
        let h = a.clone();
        let p = prev.clone();
        let af = after.clone();
        exec.schedule(async move {
            p.set(h.store(9).await);
            af.set(h.load().await);
            Ok(())
        });

        drive(&mut exec, &mut a.clone());
        assert_eq!(prev.get(), 1);
        assert_eq!(after.get(), 9);
    }

    #[test]
    fn compare_exchange_swaps_on_match() {
        let a = Handle::new(0, 1u32);
        let res = Rc::new(Cell::new(Err(0)));
        let after = Rc::new(Cell::new(0));

        let mut exec = Executor::default();
        let h = a.clone();
        let r = res.clone();
        let af = after.clone();
        exec.schedule(async move {
            r.set(h.compare_exchange(1, 9).await);
            af.set(h.load().await);
            Ok(())
        });

        drive(&mut exec, &mut a.clone());
        assert_eq!(res.get(), Ok(1));
        assert_eq!(after.get(), 9);
    }

    // The defining RMW property: the compare happens at commit, not at
    // registration. Another process stores between this CAS's registration and
    // its commit, so the CAS sees the fresh value and reports the mismatch.
    #[test]
    fn compare_exchange_observes_commit_time_value() {
        let a = Handle::new(0, 1u32);
        let res = Rc::new(Cell::new(Ok(0)));

        let mut exec = Executor::default();
        let h = a.clone();
        let r = res.clone();
        exec.schedule(async move {
            r.set(h.compare_exchange(1, 9).await);
            Ok(())
        });
        let h2 = a.clone();
        exec.schedule(async move {
            h2.store(5).await;
            Ok(())
        });
        exec.execute().unwrap();

        // Both ops are registered and pending; commit the store, then the CAS.
        let mut obj = a.clone();
        let store = *obj.enabled().iter().find(|t| t.pid == 1).unwrap();
        obj.apply(store);
        exec.execute().unwrap();
        let cas = *obj.enabled().iter().find(|t| t.pid == 0).unwrap();
        obj.apply(cas);
        exec.execute().unwrap();

        assert_eq!(res.get(), Err(5));
    }
}
