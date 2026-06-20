use std::{error::Error as StdError, fmt};

use super::observer::Observer;
use crate::model::{FailureReason, State, StateView, World};

/// The model-checking strategy [`explore`] runs.
///
/// Both strategies cover the same state space; they differ only in how much redundant work they
/// do. [`Strategy::Optimal`] is the intended driver for non-trivial programs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Exhaustive depth-first search over every interleaving.
    ///
    /// Visits each distinct scheduling once, including interleavings that are equivalent under
    /// happens-before. Simple and useful as a reference oracle, but its cost grows with the raw
    /// number of schedules rather than the number of equivalence classes.
    Dfs,
    /// Optimal Dynamic Partial Order Reduction.
    ///
    /// Explores exactly one interleaving per Mazurkiewicz (happens-before) equivalence class and
    /// never reaches a sleep-set-blocked state (Abdulla et al., POPL'14).
    Optimal,
}

/// Enumerates interleavings of the program built by `setup` under the chosen [`Strategy`].
///
/// `setup` builds the program once into a fresh [`World`](crate::model::World) (spawning
/// processes, declaring atomics); it is re-run on every replay, so it must be deterministic.
/// `observer` is notified at every explored state — runnable, terminal, or failed; pass
/// `&mut ()` to observe nothing.
///
/// Returns `Ok(())` if every reachable interleaving terminates cleanly, or the first
/// [`FailedState`] encountered (a process error or a deadlock), at which point the search stops.
pub fn explore<'a>(
    setup: &'a dyn Fn(&mut World<'a>),
    observer: &mut impl Observer,
    strategy: Strategy,
) -> Result<(), FailedState<'a>> {
    let root = StateView::new(setup).state();
    match strategy {
        Strategy::Dfs => dfs(root, observer),
        Strategy::Optimal => super::optimal::run(root, observer),
    }
}

// The observer sees every state first — failed ones included — so it can read the failure reason
// and resolve the failing transition before a failed branch aborts the search.
fn dfs<'a>(state: State<'a>, observer: &mut impl Observer) -> Result<(), FailedState<'a>> {
    observer.observe(&state);
    if state.is_failed() {
        return Err(FailedState::from_state(state));
    }
    // An empty `enabled` with no failure is a clean terminal: every process is done.
    for t in state.enabled() {
        let mut next = state.fork();
        next.apply(t);
        dfs(next, observer)?;
    }
    Ok(())
}

/// A reproducible failure returned by [`explore`].
///
/// Holds why the interleaving failed plus a replayable view of the prefix that led there, so the
/// offending schedule can be reproduced deterministically with [`play`](FailedState::play).
/// Implements [`std::error::Error`], [`std::fmt::Display`] (the reason) and a `Debug` that also
/// shows the failing trace.
pub struct FailedState<'a> {
    reason: FailureReason,
    view: StateView<'a>,
}

impl<'a> FailedState<'a> {
    pub(super) fn new(reason: FailureReason, view: StateView<'a>) -> Self {
        Self { reason, view }
    }

    // Splits a failed state into its reason and a replayable view of the prefix that reached it.
    fn from_state(state: State<'a>) -> Self {
        let (reason, view) = state.into_failure();
        Self::new(reason, view)
    }

    /// The reason this interleaving failed — a process error or a deadlock.
    pub fn reason(&self) -> &FailureReason {
        &self.reason
    }

    /// Replays the failing prefix from scratch, reproducing the same failure.
    ///
    /// Re-runs the `setup` closure and re-applies the recorded trace, yielding an equivalent
    /// [`FailedState`]. This demonstrates that the discovered schedule is deterministically
    /// reproducible.
    pub fn play(&self) -> Self {
        Self::from_state(self.view.state())
    }
}

impl fmt::Debug for FailedState<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FailedState")
            .field("reason", &self.reason)
            .field("trace", &self.view.trace())
            .finish()
    }
}

impl fmt::Display for FailedState<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl StdError for FailedState<'_> {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.reason.source()
    }
}

#[cfg(test)]
mod tests {
    use super::{Strategy, explore};
    use crate::model::World;

    fn racy(world: &mut World) {
        let atomic = world.atomic("x", 0u32);
        let r1 = atomic.clone();
        let r2 = atomic.clone();
        world.spawn("writer", async move {
            r1.store(1).await;
            Ok(())
        });
        world.spawn("reader", async move {
            if r2.load().await == 1 {
                Ok(())
            } else {
                Err("unexpected value".into())
            }
        });
    }

    #[test]
    fn play_reproduces_failure() {
        let failed = explore(&racy, &mut (), Strategy::Dfs).unwrap_err();
        let again = failed.play();
        assert_eq!(failed.to_string(), again.to_string());
    }

    // Two independent stores on one atomic: every interleaving runs cleanly.
    fn two_writers(world: &mut World) {
        let atomic = world.atomic("x", 0u32);
        let r1 = atomic.clone();
        let r2 = atomic.clone();
        world.spawn("writer-1", async move {
            r1.store(1).await;
            Ok(())
        });
        world.spawn("writer-2", async move {
            r2.store(2).await;
            Ok(())
        });
    }

    #[test]
    fn explores_clean_program() {
        assert!(explore(&two_writers, &mut (), Strategy::Dfs).is_ok());
    }

    // A process that never makes progress: blocked with nothing enabled.
    fn never_finishes(world: &mut World) {
        world.spawn("stuck", async {
            std::future::pending::<()>().await;
            Ok(())
        });
    }

    #[test]
    fn deadlock_is_detected() {
        let failed = explore(&never_finishes, &mut (), Strategy::Dfs).unwrap_err();
        assert_eq!(failed.to_string(), "deadlock");
        failed.play();
    }
}
