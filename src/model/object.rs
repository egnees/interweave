//! The shared vocabulary the model and search layers pick from: the [`Object`]
//! trait every synchronization primitive implements, and the [`Transition`] the
//! strategy picks.

use super::process;

/// Index of an object in the [`World`](crate::World)'s object table; doubles as
/// its identity.
pub(crate) type ObjectID = usize;

/// One schedulable step: the unit a strategy picks when advancing a
/// [`State`](crate::State).
///
/// A transition names *which* process (`pid`) operates on *which* object (`oid`),
/// disambiguated by a per-object sequence number (`seq`) when a process has more
/// than one pending op on the same object. The fields are private: a transition
/// is an opaque token produced by the model and handed back to it, never built by
/// callers.
#[derive(Eq, PartialEq, Hash, Clone, Copy, Debug)]
pub struct Transition {
    pub(crate) pid: process::ProcessID,
    pub(crate) oid: ObjectID,
    pub(crate) seq: usize,
}

/// A synchronization primitive seen by the model: a state machine whose every
/// observable operation is a schedulable [`Transition`].
///
/// `enabled` reports the ops runnable *now*, `pending` each process's next
/// registered op (runnable or not), and `depends` / `co_enabled` expose the
/// commutativity and concurrency relations the DPOR strategy reasons about.
pub(crate) trait Object {
    fn apply(&mut self, t: Transition);
    // Transitions runnable *now*. A blocking primitive registers ops it cannot
    // yet run (e.g. a receive on an empty channel); those are excluded here.
    fn enabled(&self) -> Vec<Transition>;
    fn label(&self, t: &Transition) -> String;
    // Whether two transitions on this object conflict (do not commute). Only
    // called for transitions that target this object (same oid); the caller
    // handles different-object pairs as independent.
    fn depends(&self, t1: Transition, t2: Transition) -> bool;
    // Each process's next registered op, runnable or not. DPOR's race detection
    // needs to see a blocked op (e.g. a receive on an empty channel) to know it
    // races with whatever will enable it; for non-blocking objects every
    // registered op is enabled, so the default coincides with `enabled`.
    fn pending(&self) -> Vec<Transition> {
        self.enabled()
    }
    // Whether two transitions on this object can be simultaneously enabled in
    // some reachable state. A blocking primitive may rule out two ops that are
    // never simultaneously enabled. Non-blocking ops always are.
    fn co_enabled(&self, _t1: Transition, _t2: Transition) -> bool {
        true
    }
}
