//! The modeled concurrent system and its execution.
//!
//! A [`World`] is the builder that owns the processes and synchronization objects
//! of a program under test. A [`State`] is a node in the search tree: a `World`
//! advanced along a [`Transition`] trace, reporting its enabled transitions and,
//! at a leaf, why it stopped ([`FailureReason`]). The search layer reaches the
//! model only through this public surface; states are reconstructed by *replaying*
//! a trace, since process futures cannot be cloned.

mod executor;
mod object;
mod process;

use std::{error::Error, fmt::Debug};

pub use object::Transition;
pub use process::ProcessResult;

pub(crate) use executor::{Executor, pid};
pub(crate) use object::{Object, ObjectID};

use crate::sync::Atomic;
use executor::RawProcessError;

/// The program under test: the builder that owns its processes and
/// synchronization objects.
///
/// A setup closure populates a `World` by [`spawn`](World::spawn)ing process
/// futures and creating shared objects with [`atomic`](World::atomic). The same
/// closure is re-run on every replay, so it must build the same processes and
/// objects in the same order — object and transition ids are assigned in insertion
/// order, and that fixed order is what makes replay deterministic.
#[derive(Default)]
pub struct World<'a> {
    objects: Vec<Box<dyn Object>>,
    exec: Executor<'a>,
    process_names: Vec<String>,
    object_names: Vec<String>,
}

impl<'a> World<'a> {
    /// Registers a named process from a future. Every `.await` inside `code` on a
    /// synchronization primitive is a scheduling point the model checker can pivot
    /// on. The name appears in traces and reports.
    pub fn spawn(
        &mut self,
        name: impl Into<String>,
        code: impl Future<Output = ProcessResult> + 'a,
    ) {
        let id = self.exec.schedule(code);
        debug_assert_eq!(id, self.process_names.len(), "pid must match name index");
        self.process_names.push(name.into());
    }

    /// Creates a named shared [`Atomic`] cell with an initial value, returning a
    /// cloneable handle whose `load` / `store` / `compare_exchange` operations are
    /// scheduling points. Clone the handle into the processes that share the cell.
    pub fn atomic<T: Copy + PartialEq + Debug + 'static>(
        &mut self,
        name: impl Into<String>,
        value: T,
    ) -> Atomic<T> {
        let id = self.objects.len();
        let handle = Atomic::new(id, value);
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

    /// A human-readable label for a *committed* [`Transition`] (e.g.
    /// `"load -> 123"`). Only meaningful after the transition has been applied.
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

    // Promotes a raw executor failure (which only knows the pid) into the
    // public, name-bearing error. The pid indexes the name table.
    fn named_error(&self, raw: RawProcessError) -> ProcessError {
        ProcessError {
            process: self.process_names[raw.pid].clone(),
            source: raw.source,
        }
    }

    // Drives the executor and reports a process error as a settled failure.
    fn run(&mut self) -> Option<FailureReason> {
        self.exec
            .execute()
            .err()
            .map(|raw| FailureReason::Process(self.named_error(raw)))
    }
}

/// The error reported when a process future returns `Err`, naming the offending
/// process.
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

/// Why a settled [`State`] can make no further useful progress.
///
/// A `State` reports this about itself via [`State::failure_reason`], so the search
/// layer sees failed states through the same interface as healthy ones.
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
/// point; the search layer steps it forward one transition at a time. A state
/// exposes its enabled transitions and, once settled, its
/// [`failure_reason`](State::failure_reason). States cannot be cloned directly
/// (process futures cannot be cloned) — the search layer rebuilds them by replaying
/// a trace.
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

    // Replays this view's setup against a fresh trace prefix. Lets the Optimal
    // driver rebuild the live state at any surviving prefix from the root in one
    // replay, keeping `setup` private (the view is the only setup carrier).
    pub(crate) fn replay(&self, trace: Vec<Transition>) -> State<'a> {
        Self {
            setup: self.setup,
            trace,
        }
        .state()
    }

    // Rebuilds the full state by replaying the recorded schedule. A view is a
    // valid prefix by construction, so only its final state may carry a failure;
    // an earlier one means the model is non-deterministic.
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

    /// The underlying [`World`], for resolving process/object names and labels of
    /// the transitions in this state's trace.
    pub fn world(&self) -> &World<'a> {
        &self.world
    }

    /// The sequence of [`Transition`]s applied to reach this state from the root.
    pub fn trace(&self) -> &[Transition] {
        &self.trace
    }

    /// The reason this state ends its branch, or `None` if it is still runnable or
    /// completed cleanly.
    ///
    /// The strategy reads it to stop exploring a branch; an observer reads it to
    /// distinguish a failing leaf from a clean one.
    pub fn failure_reason(&self) -> Option<&FailureReason> {
        self.failure.as_ref()
    }

    /// Whether this state is a leaf of the search tree: a failure, or a clean
    /// completion with no process left to advance.
    ///
    /// Each leaf corresponds to one maximal interleaving, so an [`Observer`] can
    /// count maximal traces by counting the states for which this is `true`.
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

    // Every process's next registered op, including ones not yet runnable (a
    // receive blocked on an empty channel). DPOR's race detection needs these;
    // deadlock detection and replay keep using `enabled` (runnable now).
    pub(crate) fn pending_transitions(&self) -> Vec<Transition> {
        self.world
            .objects
            .iter()
            .flat_map(|o| o.pending())
            .collect()
    }

    // Transitions on different objects are independent; same-object pairs are the
    // object's call.
    pub(crate) fn depends(&self, t1: Transition, t2: Transition) -> bool {
        if t1.oid != t2.oid {
            return false;
        }
        self.world.objects[t1.oid].depends(t1, t2)
    }

    // Transitions on different objects can always coexist; a blocking primitive may
    // rule out a same-object pair that is never simultaneously enabled.
    pub(crate) fn co_enabled(&self, t1: Transition, t2: Transition) -> bool {
        if t1.oid != t2.oid {
            return true;
        }
        self.world.objects[t1.oid].co_enabled(t1, t2)
    }

    pub(crate) fn apply(&mut self, t: Transition) {
        debug_assert!(self.failure.is_none(), "apply on an already-failed state");
        // Record before executing so a failing transition is part of the trace:
        // the view of a failed state must replay the failure (see FailedState).
        self.trace.push(t);
        self.world.objects[t.oid].apply(t);
        self.failure = self.world.run();
        self.settle();
    }

    // After the executor has run: if no process erred yet nothing is enabled and
    // a process is still live, the state is deadlocked. A process error (already
    // set) takes precedence.
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

    // Consumes a failed state into its reason and a replayable view. Panics if
    // the state has not failed.
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
    use super::{FailureReason, State, World};

    fn program(world: &mut World) {
        let atomic = world.atomic("x", 123);
        let r1 = atomic.clone();
        let r2 = atomic.clone();
        world.spawn("writer", async move {
            r1.store(222).await;
            Ok(())
        });
        world.spawn("reader", async move {
            if r2.load().await == 222 {
                Ok(())
            } else {
                Err("unexpected value".into())
            }
        });
    }

    #[test]
    fn store_then_load() {
        let mut state = State::new(&program);
        let enabled = state.enabled();
        let p1 = *enabled.iter().find(|t| t.pid == 0).unwrap();
        let p2 = *enabled.iter().find(|t| t.pid == 1).unwrap();
        state.apply(p1);
        state.apply(p2);
        assert!(state.enabled().is_empty());
        assert!(state.failure_reason().is_none());
    }

    // Loading before the store leaves the reader observing the initial value and
    // erroring; the state reports the failure, and the failing op's label is
    // still resolvable because `apply` records history before the process polls.
    #[test]
    fn load_then_store_records_failure() {
        let mut state = State::new(&program);
        let load = *state.enabled().iter().find(|t| t.pid == 1).unwrap();
        state.apply(load);
        assert!(matches!(
            state.failure_reason(),
            Some(FailureReason::Process(_))
        ));
        assert_eq!(state.world().label(&load), "load -> 123");
    }
}
