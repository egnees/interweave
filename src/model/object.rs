//! The [`Object`] trait every synchronization primitive implements, and the
//! [`Transition`] a strategy picks. This is the crate's extension point: implement
//! [`Object`] for your own primitive and register it with
//! [`World::register`](crate::World::register).

use super::{pid, process};

/// Index of an object in the [`World`](crate::World)'s object table, assigned in
/// registration order; doubles as the object's identity. A custom [`Object`]
/// receives its `ObjectID` from [`World::register`](crate::World::register) and stamps
/// it into every [`Transition`] it builds.
pub type ObjectID = usize;

/// One schedulable step: a process performing one observable operation on one
/// synchronization object — the unit a [`Strategy`](crate::Strategy) picks at each
/// scheduling point.
///
/// It carries the operating process, the target object, and a per-object `seq`
/// that tells the object's several concurrent operations apart — unique per
/// registration on the object, across all processes. A custom [`Object`] builds one
/// with [`Transition::new`] when an awaited
/// operation registers itself, stores it, and hands the same value back from
/// [`Object::enabled`]; the model later returns it to [`Object::apply`]. Identity
/// is by value, so an object matches a transition simply with `==`.
#[derive(Eq, PartialEq, Hash, Clone, Copy, Debug)]
pub struct Transition {
    pub(crate) pid: process::ProcessID,
    pub(crate) oid: ObjectID,
    pub(crate) seq: usize,
}

impl Transition {
    /// Builds a transition for the operation the *current* process is registering
    /// on object `oid`, tagged with `seq`.
    ///
    /// The operating process is stamped automatically from the executor, so this
    /// must be called while a process is being polled — i.e. from inside an awaited
    /// operation of a synchronization primitive.
    ///
    /// `seq` is part of the transition's identity and is compared by value across
    /// the whole search, so use a per-object counter that bumps on every
    /// registration and never reuses a value for the object's lifetime (what
    /// [`Atomic`](crate::Atomic) does). Reusing one after its transition was retired
    /// would alias two distinct operations and break deterministic replay.
    pub fn new(oid: ObjectID, seq: usize) -> Self {
        Self {
            pid: pid(),
            oid,
            seq,
        }
    }

    /// The process performing this transition.
    pub fn pid(&self) -> process::ProcessID {
        self.pid
    }

    /// The object this transition operates on.
    pub fn oid(&self) -> ObjectID {
        self.oid
    }

    /// The per-object sequence number distinguishing the object's concurrent
    /// operations; unique per registration on the object, across all processes.
    pub fn seq(&self) -> usize {
        self.seq
    }
}

/// A synchronization primitive as the model sees it: a small state machine whose
/// every observable operation is a schedulable [`Transition`].
///
/// Implement this to add your own primitive — a lock, a channel, a barrier — next
/// to the built-in [`Atomic`](crate::Atomic), then register it with
/// [`World::register`](crate::World::register). These four methods are the *entire*
/// contract between a primitive and the search layer: everything the strategy
/// knows about your primitive it learns through them.
///
/// # The operation lifecycle
///
/// The model never drives your futures directly. Instead, an awaited operation
/// should *register* itself — record its intent together with the process's
/// [`Waker`](std::task::Waker), build a [`Transition`] with [`Transition::new`],
/// and return [`Poll::Pending`](std::task::Poll::Pending) — and then report that
/// pending transition from [`enabled`](Object::enabled). The strategy drives
/// execution by choosing one enabled transition and calling
/// [`apply`](Object::apply), at which point the operation *commits*: it mutates the
/// object's state, records what happened so [`label`](Object::label) can describe
/// it, and wakes the process so its `.await` resolves. Splitting registration from
/// commit is what lets the strategy decide *when* each operation takes effect
/// relative to the others; [`Atomic`](crate::Atomic)'s source is the worked
/// reference for this pattern.
///
/// # Shared state
///
/// [`World::register`](crate::World::register) keeps one clone of your handle to drive
/// and hands the others to the processes, so a handle's clones **must** share one
/// underlying state — wrap it in an `Rc<RefCell<…>>`. A commit applied through the
/// model's clone has to be visible to the process holding another clone.
///
/// # Determinism
///
/// Replay rebuilds states by re-running the program and re-applying a trace, so
/// every method must be a deterministic function of the operations applied so far:
/// [`enabled`](Object::enabled) must list transitions in a fixed order, and `seq`
/// identities must be assigned the same way on every run.
pub trait Object {
    /// Commits the pending operation identified by `t`: apply its effect to the
    /// object's state, record it so [`label`](Object::label) can describe it
    /// afterwards, and wake the operation's process. `t` is always one this object
    /// previously returned from [`enabled`](Object::enabled).
    fn apply(&mut self, t: Transition);

    /// The operations that can commit right now, one [`Transition`] each — the
    /// scheduling points the strategy chooses among.
    ///
    /// An operation that is registered but not yet runnable (a `lock` while the
    /// mutex is held, a `recv` on an empty channel) must be withheld here until it
    /// becomes runnable; that is how a primitive blocks a process.
    fn enabled(&self) -> Vec<Transition>;

    /// A human-readable label for an already-[`apply`](Object::apply)ied
    /// transition, e.g. `"load -> 123"`. Only ever called after the commit, so it
    /// may rely on recorded history.
    fn label(&self, t: &Transition) -> String;

    /// Whether two operations on this object *conflict* — fail to commute, so the
    /// order in which they commit can change the outcome.
    ///
    /// This is the dependency relation that drives the partial-order reduction: the
    /// fewer pairs reported dependent, the more interleavings are pruned, so report
    /// `true` only when the operations genuinely interfere (two reads commute; a
    /// write conflicts with reads and writes). Reporting too few dependencies is
    /// *unsound* — it can hide reachable states — so when in doubt, return `true`.
    ///
    /// The relation may depend on committed state, not only on the two operations'
    /// kinds — a channel `recv`, say, conflicts with a `send` only when it consumed
    /// that send — so the search only ever asks about a pair drawn from one committed
    /// (maximal or replayed) trace, and the object resolves each operation from its
    /// own recorded history.
    ///
    /// Only ever called for two transitions on this same object; the model treats
    /// operations on different objects as independent.
    fn depends(&self, t1: Transition, t2: Transition) -> bool;
}
