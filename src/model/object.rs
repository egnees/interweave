//! The [`Object`] trait every synchronization primitive implements, and the
//! [`Transition`] a strategy picks.

use super::process;

/// Index of an object in the [`World`](crate::World)'s object table; doubles as
/// its identity.
pub(crate) type ObjectID = usize;

/// One schedulable step: process `pid` operating on object `oid`, with a
/// per-object `seq` to disambiguate a process's several pending ops on the same
/// object. Opaque — produced by the model and handed back to it, never built by
/// callers.
#[derive(Eq, PartialEq, Hash, Clone, Copy, Debug)]
pub struct Transition {
    pub(crate) pid: process::ProcessID,
    pub(crate) oid: ObjectID,
    pub(crate) seq: usize,
}

/// A synchronization primitive seen by the model: a state machine whose every
/// observable operation is a schedulable [`Transition`]. `depends` is only ever
/// called for same-object pairs; the caller treats different objects as
/// independent.
pub(crate) trait Object {
    fn apply(&mut self, t: Transition);

    // Transitions runnable now.
    fn enabled(&self) -> Vec<Transition>;

    // Human-readable label for a committed transition, e.g. "load -> 123".
    fn label(&self, t: &Transition) -> String;

    // Whether two transitions conflict (do not commute).
    fn depends(&self, t1: Transition, t2: Transition) -> bool;
}
