//! Define your own synchronization primitive by implementing [`Object`].
//!
//! A deliberately tiny `Counter` with two operations — `inc` and `get` — built on
//! nothing but the crate's public surface ([`Object`], [`Transition`],
//! [`World::register`]). It mirrors the structure of the built-in `Atomic` at a
//! fraction of the size; see `Atomic`'s source for the full-featured version.
//!
//! Two processes increment the counter and a third reads it; `inc`/`inc` and
//! `inc`/`get` conflict while two `get`s commute, so Optimal DPOR explores exactly
//! the orderings that the reader can tell apart.
//!
//! ```sh
//! cargo run --example custom_object
//! ```

use std::cell::{Cell, RefCell};
use std::future::poll_fn;
use std::rc::Rc;
use std::task::{Poll, Waker};

use interweave::{Object, ObjectID, Observer, Step, StepCx, Transition, World, explore};

#[derive(Clone, Copy)]
enum Op {
    Inc,
    Get,
}

// A registered-but-not-yet-committed operation: its transition, the waker to
// resume the process, and the slot the commit writes the observed count into.
struct Request {
    transition: Transition,
    waker: Waker,
    op: Op,
    result: Rc<Cell<Option<usize>>>,
}

#[derive(Default)]
struct Inner {
    count: usize,
    seq: usize,
    requests: Vec<Request>,
    history: Vec<(Transition, Op, usize)>,
}

/// A cloneable handle to a shared counter. Clones share one `Inner` through the
/// `Rc<RefCell<…>>`, so the clone the world drives and the clones the processes
/// hold all see the same cell.
#[derive(Clone)]
struct Counter {
    id: ObjectID,
    state: Rc<RefCell<Inner>>,
}

impl Counter {
    fn new(id: ObjectID) -> Self {
        Self {
            id,
            state: Rc::new(RefCell::new(Inner::default())),
        }
    }

    async fn inc(&self) {
        self.request(Op::Inc).await;
    }

    async fn get(&self) -> usize {
        self.request(Op::Get).await
    }

    // First poll registers the op and yields; the commit fills `result` and wakes
    // us, and the next poll reads the observed count back.
    async fn request(&self, op: Op) -> usize {
        let result = Rc::new(Cell::new(None));
        let mut op = Some(op);
        poll_fn(move |cx| {
            if let Some(value) = result.get() {
                return Poll::Ready(value);
            }
            if let Some(op) = op.take() {
                self.register(op, cx.waker().clone(), Rc::clone(&result));
            }
            Poll::Pending
        })
        .await
    }

    fn register(&self, op: Op, waker: Waker, result: Rc<Cell<Option<usize>>>) {
        let mut st = self.state.borrow_mut();
        let transition = Transition::new(self.id, st.seq);
        st.seq += 1;
        st.requests.push(Request {
            transition,
            waker,
            op,
            result,
        });
    }

    fn op_of(&self, t: Transition) -> Op {
        let st = self.state.borrow();
        st.requests
            .iter()
            .map(|r| (r.transition, r.op))
            .chain(st.history.iter().map(|(tt, op, _)| (*tt, *op)))
            .find(|(tt, _)| *tt == t)
            .map(|(_, op)| op)
            .expect("transition not registered on this counter")
    }
}

impl Object for Counter {
    fn apply(&mut self, t: Transition) {
        let mut st = self.state.borrow_mut();
        let i = st
            .requests
            .iter()
            .position(|r| r.transition == t)
            .expect("transition must be enabled");
        let req = st.requests.remove(i);
        let observed = st.count;
        if let Op::Inc = req.op {
            st.count += 1;
        }
        st.history.push((t, req.op, observed));
        req.result.set(Some(observed));
        req.waker.wake();
    }

    fn enabled(&self) -> Vec<Transition> {
        self.state
            .borrow()
            .requests
            .iter()
            .map(|r| r.transition)
            .collect()
    }

    fn label(&self, t: Transition) -> String {
        let st = self.state.borrow();
        let (_, op, observed) = st
            .history
            .iter()
            .find(|(tt, _, _)| *tt == t)
            .expect("label called on an unapplied transition");
        match op {
            Op::Inc => format!("inc ({observed} -> {})", observed + 1),
            Op::Get => format!("get -> {observed}"),
        }
    }

    // Two gets commute (each observes a value, neither writes); anything touching
    // an inc conflicts. Sound and simple.
    fn depends(&self, t1: Transition, t2: Transition) -> bool {
        !matches!((self.op_of(t1), self.op_of(t2)), (Op::Get, Op::Get))
    }
}

fn program(world: &mut World) {
    let counter = world.register("counter", Counter::new);
    let (a, b, reader) = (counter.clone(), counter.clone(), counter);
    world.spawn("inc-a", async move {
        a.inc().await;
        Ok(())
    });
    world.spawn("inc-b", async move {
        b.inc().await;
        Ok(())
    });
    world.spawn("reader", async move {
        reader.get().await;
        Ok(())
    });
}

// Tallies maximal interleavings by counting terminal leaves of the search tree.
#[derive(Default)]
struct Traces(usize);

impl Observer for Traces {
    fn step(&mut self, step: Step<'_>, cx: StepCx<'_, '_>) {
        if matches!(step, Step::Visit) && cx.state().is_terminal() {
            self.0 += 1;
        }
    }
}

fn main() {
    let mut traces = Traces::default();
    explore(&program, &mut traces).expect("the counter never fails");
    // With a single reader every pair conflicts (inc/inc and inc/get), so each of
    // the 3! orderings is its own Mazurkiewicz class: 6 maximal interleavings. A
    // second reader would commute with the first, collapsing some classes.
    println!("custom Counter: {} maximal interleavings", traces.0);
}
