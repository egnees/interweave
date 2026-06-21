//! The modeled concurrent system and its execution: a [`World`] builds the
//! processes and synchronization objects of a program under test, and a [`State`]
//! is that world advanced along a [`Transition`] trace. The search layer reaches
//! the model only through this surface and rebuilds states by *replaying* a trace,
//! since process futures cannot be cloned.

mod executor;
mod object;
mod process;

use std::{error::Error, fmt::Debug};

pub use object::{Object, ObjectID, Transition};
pub use process::{ProcessID, ProcessResult};

pub(crate) use executor::{Executor, pid};

use crate::sync::{Atomic, ChannelHandle, Receiver, Sender};

/// The program under test: a builder owning its processes and synchronization
/// objects. A setup closure populates it via [`spawn`](World::spawn) and
/// [`atomic`](World::atomic); the same closure is re-run on every replay, so it
/// must build the same objects in the same order — ids are assigned by insertion
/// order, and that fixed order is what makes replay deterministic.
#[derive(Default)]
pub struct World<'a> {
    objects: Vec<Box<dyn Object>>,
    exec: Executor<'a>,
    process_names: Vec<String>,
    object_names: Vec<String>,
}

impl<'a> World<'a> {
    /// Registers a named process. Every `.await` on a synchronization primitive
    /// inside `code` is a scheduling point. The name appears in traces and reports.
    pub fn spawn(
        &mut self,
        name: impl Into<String>,
        code: impl Future<Output = ProcessResult> + 'a,
    ) {
        let id = self.exec.schedule(code);
        debug_assert_eq!(id, self.process_names.len(), "pid must match name index");
        self.process_names.push(name.into());
    }

    /// Creates a named shared [`Atomic`] cell, returning a cloneable handle whose
    /// `load` / `store` / `compare_exchange` operations are scheduling points.
    pub fn atomic<T: Copy + PartialEq + Debug + 'static>(
        &mut self,
        name: impl Into<String>,
        value: T,
    ) -> Atomic<T> {
        self.register(name, |id| Atomic::new(id, value))
    }

    /// Creates a named unbounded MPSC channel, returning a cloneable [`Sender`]
    /// (multi-producer) and a single [`Receiver`] (not cloneable). Every `send` and
    /// `recv` is a scheduling point; a `recv` on an empty channel blocks.
    pub fn channel<T: Debug + 'static>(
        &mut self,
        name: impl Into<String>,
    ) -> (Sender<T>, Receiver<T>) {
        let driver = self.register(name, ChannelHandle::new);
        driver.split()
    }

    /// Registers a custom synchronization object, returning the cloneable handle
    /// the program shares among its processes.
    ///
    /// `build` receives the [`ObjectID`] the world assigns and returns a handle
    /// implementing [`Object`]; the world keeps one clone to drive and gives this
    /// one back. The handle's clones must share state (see [`Object`]). This is the
    /// open extension point behind [`atomic`](World::atomic) — implement [`Object`]
    /// for your own lock, channel, or barrier and register it here.
    pub fn register<O>(&mut self, name: impl Into<String>, build: impl FnOnce(ObjectID) -> O) -> O
    where
        O: Object + Clone + 'static,
    {
        let id = self.objects.len();
        let handle = build(id);
        self.objects.push(Box::new(handle.clone()));
        self.object_names.push(name.into());
        handle
    }

    /// The name of the process that performs the given [`Transition`].
    pub fn process(&self, t: &Transition) -> &str {
        &self.process_names[t.pid]
    }

    /// The name of the object the given [`Transition`] operates on.
    pub fn object(&self, t: &Transition) -> &str {
        &self.object_names[t.oid]
    }

    /// A human-readable label for a *committed* [`Transition`] (e.g. `"load -> 123"`).
    pub fn label(&self, t: &Transition) -> String {
        self.objects[t.oid].label(t)
    }

    /// The process names, indexed by process id.
    pub fn processes(&self) -> &[String] {
        &self.process_names
    }

    /// The object names, indexed by object id.
    pub fn objects(&self) -> &[String] {
        &self.object_names
    }

    // Drives the executor, promoting a raw failure (pid only) into the public,
    // name-bearing FailureReason.
    fn run(&mut self) -> Option<FailureReason> {
        self.exec.execute().err().map(|raw| {
            FailureReason::Process(ProcessError {
                process: self.process_names[raw.pid].clone(),
                source: raw.source,
            })
        })
    }
}

/// The error reported when a process future returns `Err`, naming the process.
#[derive(Debug, thiserror::Error)]
#[error("process {process} failed: {source}")]
pub struct ProcessError {
    process: String,
    source: Box<dyn Error>,
}

impl ProcessError {
    /// The name of the process that erred.
    pub fn process(&self) -> &str {
        &self.process
    }
}

/// Why a settled [`State`] makes no further progress. A `State` reports this via
/// [`failure_reason`](State::failure_reason), so the search layer sees failed and
/// healthy states through one interface.
#[derive(Debug, thiserror::Error)]
pub enum FailureReason {
    /// A process future returned an error.
    #[error("{0}")]
    Process(#[source] ProcessError),
    /// No process is enabled yet a process is still live: the program is stuck.
    #[error("deadlock")]
    Deadlock,
}

/// A node in the search tree: a [`World`] advanced along a [`Transition`] trace.
///
/// Construction runs the setup closure and the executor to the first scheduling
/// point; the search layer steps it forward one transition at a time. States
/// cannot be cloned (process futures cannot) — the search layer rebuilds them by
/// replaying a trace.
pub struct State<'a> {
    world: World<'a>,
    setup: &'a dyn Fn(&mut World<'a>),
    trace: Vec<Transition>,
    failure: Option<FailureReason>,
}

#[derive(Clone)]
pub(crate) struct StateView<'a> {
    setup: &'a dyn Fn(&mut World<'a>),
    trace: Vec<Transition>,
}

impl<'a> StateView<'a> {
    pub(crate) fn new(setup: &'a dyn Fn(&mut World<'a>)) -> Self {
        Self {
            setup,
            trace: Vec::new(),
        }
    }

    pub(crate) fn trace(&self) -> &[Transition] {
        &self.trace
    }

    // Rebuilds the live state at an arbitrary prefix in one replay from the root,
    // keeping `setup` private to the view.
    pub(crate) fn replay(&self, trace: Vec<Transition>) -> State<'a> {
        Self {
            setup: self.setup,
            trace,
        }
        .state()
    }

    // Rebuilds the full state by replaying the recorded schedule. A view is a valid
    // prefix by construction, so only its final state may carry a failure.
    pub(crate) fn state(&self) -> State<'a> {
        let mut state = State::new(self.setup);
        for &t in &self.trace {
            debug_assert!(state.failure.is_none(), "replay diverged: early failure");
            debug_assert!(
                state.enabled().contains(&t),
                "replay diverged at {t:?}: model is non-deterministic"
            );
            state.apply(t);
        }
        state
    }
}

impl<'a> State<'a> {
    fn new(setup: &'a dyn Fn(&mut World<'a>)) -> Self {
        let mut world = World::default();
        setup(&mut world);
        let failure = world.run();
        let mut state = Self {
            world,
            setup,
            trace: Vec::new(),
            failure,
        };
        state.settle();
        state
    }

    /// The underlying [`World`], for resolving names and labels of this state's
    /// transitions.
    pub fn world(&self) -> &World<'a> {
        &self.world
    }

    /// The sequence of [`Transition`]s applied to reach this state from the root.
    pub fn trace(&self) -> &[Transition] {
        &self.trace
    }

    /// Why this state ends its branch, or `None` if it is still runnable or
    /// completed cleanly.
    pub fn failure_reason(&self) -> Option<&FailureReason> {
        self.failure.as_ref()
    }

    /// Whether this state is a leaf: a failure, or a clean completion with nothing
    /// left to advance. Each leaf is one maximal interleaving, so an [`Observer`]
    /// can count maximal traces by counting terminal states.
    ///
    /// [`Observer`]: crate::Observer
    pub fn is_terminal(&self) -> bool {
        self.failure.is_some() || self.enabled().is_empty()
    }

    pub(crate) fn is_failed(&self) -> bool {
        self.failure.is_some()
    }

    pub(crate) fn pending(&self) -> usize {
        self.world.exec.pending()
    }

    pub(crate) fn enabled(&self) -> Vec<Transition> {
        // Fixed iteration order (objects, then each object's requests, both
        // insertion-ordered) is what lets replay rebuild identical states.
        self.world
            .objects
            .iter()
            .flat_map(|o| o.enabled())
            .collect()
    }

    /// Whether two transitions *conflict* — fail to commute, so the order in which they
    /// commit can change the outcome. This is the dependency relation that drives
    /// partial-order reduction: independent transitions may be reordered freely, while
    /// dependent ones must be explored in both orders. Use it to reconstruct
    /// happens-before over a trace (e.g. for a visualization or a custom analysis).
    ///
    /// Transitions on different objects are always independent — sound because the model
    /// is "one transition touches exactly one object", so no single step can couple two;
    /// same-object pairs defer to the object's own [`Object::depends`]. Evaluate it on a
    /// state where both transitions occur (e.g. a maximal trace): for some primitives
    /// (channels) the relation depends on the committed history.
    pub fn depends(&self, t1: Transition, t2: Transition) -> bool {
        t1.oid == t2.oid && self.world.objects[t1.oid].depends(t1, t2)
    }

    pub(crate) fn apply(&mut self, t: Transition) {
        debug_assert!(self.failure.is_none(), "apply on an already-failed state");
        // Record before executing so a failing transition is part of the trace
        // (its view must replay the failure — see FailedState).
        self.trace.push(t);
        self.world.objects[t.oid].apply(t);
        self.failure = self.world.run();
        self.settle();
    }

    // No process erred but nothing is enabled while a process is still live:
    // deadlock. An already-set process error takes precedence.
    fn settle(&mut self) {
        if self.failure.is_none() && self.enabled().is_empty() && self.pending() > 0 {
            self.failure = Some(FailureReason::Deadlock);
        }
    }

    // A view of this state, ready to be replayed back into a full `State`.
    pub(crate) fn view(&self) -> StateView<'a> {
        StateView {
            setup: self.setup,
            trace: self.trace.clone(),
        }
    }

    // An independent copy, rebuilt by replaying its view from scratch.
    pub(crate) fn fork(&self) -> Self {
        self.view().state()
    }

    // Consumes a failed state into its reason and a replayable view; panics if healthy.
    pub(crate) fn into_failure(self) -> (FailureReason, StateView<'a>) {
        let reason = self.failure.expect("into_failure on a healthy state");
        let view = StateView {
            setup: self.setup,
            trace: self.trace,
        };
        (reason, view)
    }
}

#[cfg(test)]
mod tests {
    use super::{FailureReason, State, Transition, World};

    const WRITER: usize = 0;
    const READER: usize = 1;

    // A writer storing 222 and a reader that only succeeds if it observes that
    // store, so the two scheduling orders give a clean run vs. a process failure.
    fn program(world: &mut World) {
        let atomic = world.atomic("x", 123);
        let writer = atomic.clone();
        let reader = atomic;
        world.spawn("writer", async move {
            writer.store(222).await;
            Ok(())
        });
        world.spawn("reader", async move {
            if reader.load().await == 222 {
                Ok(())
            } else {
                Err("unexpected value".into())
            }
        });
    }

    fn enabled_op(state: &State, pid: usize) -> Transition {
        *state.enabled().iter().find(|t| t.pid == pid).unwrap()
    }

    #[test]
    fn store_then_load() {
        let mut state = State::new(&program);
        state.apply(enabled_op(&state, WRITER));
        state.apply(enabled_op(&state, READER));
        assert!(state.enabled().is_empty());
        assert!(state.failure_reason().is_none());
    }

    // Loading before the store leaves the reader observing the initial value and
    // erroring; the failing op's label is still resolvable because `apply` records
    // history before the process polls.
    #[test]
    fn load_then_store_records_failure() {
        let mut state = State::new(&program);
        let load = enabled_op(&state, READER);
        state.apply(load);
        assert!(matches!(
            state.failure_reason(),
            Some(FailureReason::Process(_))
        ));
        assert_eq!(state.world().label(&load), "load -> 123");
    }
}
