//! The modeled concurrent system and its execution: a [`World`] builds the
//! processes and synchronization objects of a program under test, and a [`State`]
//! is that world advanced along a [`Transition`] trace. The search layer reaches
//! the model only through this surface and rebuilds states by *replaying* a trace,
//! since process futures cannot be cloned.

mod executor;
mod object;
mod process;

use std::{
    error::Error,
    fmt::{self, Debug},
};

pub use object::{Object, ObjectID, Transition};
pub use process::{ProcessID, ProcessResult};

pub(crate) use executor::{Executor, pid};

use crate::sync::{Atomic, ChannelHandle, Receiver, Sender};

/// The program under test: a builder that owns the processes and synchronization
/// objects created on it. A setup closure populates it via [`spawn`](World::spawn),
/// [`atomic`](World::atomic), and [`channel`](World::channel). That closure must be
/// deterministic — building the same objects in the same order every time — because
/// the search runs it repeatedly to explore different schedules.
pub struct World<'a> {
    objects: Vec<Box<dyn Object>>,
    exec: Executor<'a>,
    process_names: Vec<String>,
    object_names: Vec<String>,
}

impl<'a> World<'a> {
    // Built only by the search (via `State::new`); a user receives a `&mut World` in the
    // `setup` closure and never constructs one, so construction stays crate-internal.
    pub(crate) fn new() -> Self {
        Self {
            objects: Vec::new(),
            exec: Executor::default(),
            process_names: Vec::new(),
            object_names: Vec::new(),
        }
    }

    /// Registers a named process. Every `.await` on a synchronization primitive
    /// inside `code` is a scheduling point. The name identifies this process wherever
    /// its [`Transition`]s are shown.
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
    #[must_use = "the returned handle must be given to a process; an object no process holds is never operated on"]
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
    #[must_use = "the returned sender/receiver must be given to processes; an unused channel is never operated on"]
    pub fn channel<T: Debug + 'static>(
        &mut self,
        name: impl Into<String>,
    ) -> (Sender<T>, Receiver<T>) {
        let driver = self.register(name, ChannelHandle::new);
        driver.split()
    }

    /// Registers a custom synchronization object and returns the cloneable handle
    /// your processes share.
    ///
    /// `build` receives the [`ObjectID`] assigned to the object and returns a handle
    /// implementing [`Object`]; the handle's clones must all share one underlying state
    /// (e.g. an `Rc<RefCell<…>>`), so every process operating on the object sees the same
    /// cell. This is the extension point behind [`atomic`](World::atomic) and
    /// [`channel`](World::channel) — implement [`Object`] for your own lock, channel, or
    /// barrier and register it here.
    #[must_use = "the returned handle must be given to a process; an object no process holds is never operated on"]
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
    pub fn process(&self, t: Transition) -> &str {
        &self.process_names[t.pid]
    }

    /// The name of the object the given [`Transition`] operates on.
    pub fn object(&self, t: Transition) -> &str {
        &self.object_names[t.oid]
    }

    /// A human-readable label for a *committed* [`Transition`] (e.g. `"load -> 123"`).
    /// Panics on a transition that has not committed.
    pub fn label(&self, t: Transition) -> String {
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

impl Debug for World<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("World")
            .field("processes", &self.process_names)
            .field("objects", &self.object_names)
            .finish_non_exhaustive()
    }
}

/// The error reported when a process future returns `Err`, naming the process.
#[derive(Debug, thiserror::Error)]
#[error("process {process} failed: {source}")]
pub struct ProcessError {
    process: String,
    source: Box<dyn Error + Send + Sync>,
}

impl ProcessError {
    /// The name of the process that erred.
    pub fn process(&self) -> &str {
        &self.process
    }
}

/// Why a [`State`] makes no further progress, as reported by
/// [`State::failure_reason`] and carried by a [`FailedState`](crate::FailedState).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FailureReason {
    /// A process future returned an error.
    #[error("{0}")]
    Process(#[source] ProcessError),
    /// No process is enabled yet a process is still live: the program is stuck.
    #[error("deadlock")]
    Deadlock,
}

/// A node in the search tree: the program after a sequence of [`Transition`]s has been
/// applied. An [`Observer`](crate::Observer) receives one at every state the search
/// reaches and inspects it — its [`trace`](State::trace), its [`world`](State::world)
/// for resolving names and labels, whether it [`is_terminal`](State::is_terminal), and
/// any [`failure_reason`](State::failure_reason).
pub struct State<'a> {
    world: World<'a>,
    setup: &'a dyn Fn(&mut World<'a>),
    trace: Vec<Transition>,
    failure: Option<FailureReason>,
    // The enabled set, cached and recomputed once per committed step (after `run`,
    // before `settle`). Every read goes through this rather than re-querying the
    // objects, which on the replay-heavy hot path saves a per-object allocation per
    // access.
    enabled: Vec<Transition>,
    // Whether no registered object can block a process (every object reports
    // `!may_block`). Fixed by the object set, so computed once at construction. Lets
    // the strategy skip the non-disabling replay check (see `runnable_after`).
    non_blocking: bool,
}

impl Debug for State<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("State")
            .field("trace", &self.trace)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
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

    // Rebuilds the live state at an arbitrary prefix in one replay from the root. A
    // view's `trace` is a valid prefix by construction, so only the final state may
    // carry a failure; `setup` stays private to the view.
    pub(crate) fn replay(&self, trace: &[Transition]) -> State<'a> {
        let mut state = State::new(self.setup);
        for &t in trace {
            debug_assert!(state.failure.is_none(), "replay diverged: early failure");
            debug_assert!(
                state.enabled().contains(&t),
                "replay diverged at {t:?}: model is non-deterministic"
            );
            state.apply(t);
        }
        state
    }

    // Rebuilds the full state by replaying the recorded schedule.
    pub(crate) fn state(&self) -> State<'a> {
        self.replay(&self.trace)
    }
}

impl<'a> State<'a> {
    fn new(setup: &'a dyn Fn(&mut World<'a>)) -> Self {
        let mut world = World::new();
        setup(&mut world);
        let failure = world.run();
        let non_blocking = world.objects.iter().all(|o| !o.may_block());
        let mut state = Self {
            world,
            setup,
            trace: Vec::new(),
            failure,
            enabled: Vec::new(),
            non_blocking,
        };
        state.recompute_enabled();
        state.settle();
        state
    }

    // Whether no registered object can block a process. When true, reversing a race
    // always leaves the later op runnable, so the strategy skips the replay-based
    // non-disabling check (`runnable_after`).
    pub(crate) fn non_blocking(&self) -> bool {
        self.non_blocking
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
        self.failure.is_some() || self.enabled.is_empty()
    }

    pub(crate) fn is_failed(&self) -> bool {
        self.failure.is_some()
    }

    pub(crate) fn pending(&self) -> usize {
        self.world.exec.pending()
    }

    pub(crate) fn enabled(&self) -> &[Transition] {
        &self.enabled
    }

    // Refreshes the cached enabled set from the objects. Fixed iteration order
    // (objects, then each object's requests, both insertion-ordered) is what lets
    // replay rebuild identical states. Called once per committed step, after `run`
    // has registered the woken processes' next ops and before `settle` reads it.
    fn recompute_enabled(&mut self) {
        let mut buf = std::mem::take(&mut self.enabled);
        buf.clear();
        for o in &self.world.objects {
            o.enabled_into(&mut buf);
        }
        self.enabled = buf;
    }

    /// Whether two transitions *conflict* — fail to commute, so the order in which they
    /// commit can change the outcome. This is the dependency relation that drives
    /// partial-order reduction: independent transitions may be reordered freely, while
    /// dependent ones must be explored in both orders. Use it to reconstruct
    /// happens-before over a trace (e.g. for a visualization or a custom analysis).
    ///
    /// Evaluate it on a state where both transitions have occurred (e.g. a maximal trace):
    /// for history-sensitive primitives (channels) the answer depends on the committed
    /// history, so calling it elsewhere may silently mislead. Transitions on different
    /// objects are always independent — the model guarantees one transition touches
    /// exactly one object, so no single step couples two — while same-object pairs defer
    /// to the object's own [`Object::depends`].
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
        self.recompute_enabled();
        self.settle();
    }

    // No process erred but nothing is enabled while a process is still live:
    // deadlock. An already-set process error takes precedence. Reads the freshly
    // recomputed enabled cache.
    fn settle(&mut self) {
        if self.failure.is_none() && self.enabled.is_empty() && self.pending() > 0 {
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

    // An independent copy, rebuilt by replaying its view from scratch. Only the
    // test-only exhaustive-DFS oracle needs it; Optimal replays views directly.
    #[cfg(test)]
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
        assert_eq!(state.world().label(load), "load -> 123");
    }
}
