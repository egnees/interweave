use std::{error::Error as StdError, fmt};

use super::observer::Observer;
use super::step::StepObserver;
use crate::model::{FailureReason, State, StateView, World};

/// Enumerates interleavings of the program built by `setup` under Optimal DPOR.
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
) -> Result<(), FailedState<'a>> {
    let root = StateView::new(setup).state();
    super::optimal::run(root, observer, &mut ())
}

/// Like [`explore`] (Optimal DPOR), but with a [`StepObserver`] wired
/// into the driver so a consumer can watch the algorithm's discrete decisions
/// (descend, seed, race-reversal, pop, …) as they happen. The state-level
/// [`Observer`](super::Observer) is the no-op `()`; this entry point exists for the
/// step instrumentation a visualizer is built on. Re-exported as
/// `interweave::explore_stepped` only under the `viz` feature.
///
/// [`StepObserver`]: super::step::StepObserver
// Without `viz` it is unreachable from outside the crate (not re-exported); only the
// golden test calls it. Re-exported and exercised by the renderer under `viz`.
#[cfg_attr(not(feature = "viz"), allow(dead_code))]
pub fn explore_stepped<'a>(
    setup: &'a dyn Fn(&mut World<'a>),
    steps: &mut impl StepObserver,
) -> Result<(), FailedState<'a>> {
    let root = StateView::new(setup).state();
    super::optimal::run(root, &mut (), steps)
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
    use super::explore;
    use crate::model::World;

    // A writer racing a reader on one atomic: the reader errors unless the store commits first, so
    // some interleaving fails. Used to check that a discovered failure replays identically.
    fn racy(world: &mut World) {
        let x = world.atomic("x", 0u32);
        let writer = x.clone();
        let reader = x.clone();
        world.spawn("writer", async move {
            writer.store(1).await;
            Ok(())
        });
        world.spawn("reader", async move {
            match reader.load().await {
                1 => Ok(()),
                _ => Err("unexpected value".into()),
            }
        });
    }

    #[test]
    fn play_reproduces_failure() {
        let failed = explore(&racy, &mut ()).unwrap_err();
        let again = failed.play();
        assert_eq!(failed.to_string(), again.to_string());
    }

    // Two independent stores on one atomic: nothing reads the value back, so every interleaving
    // terminates cleanly.
    fn two_writers(world: &mut World) {
        let x = world.atomic("x", 0u32);
        let first = x.clone();
        let second = x.clone();
        world.spawn("writer-1", async move {
            first.store(1).await;
            Ok(())
        });
        world.spawn("writer-2", async move {
            second.store(2).await;
            Ok(())
        });
    }

    #[test]
    fn explores_clean_program() {
        assert!(explore(&two_writers, &mut ()).is_ok());
    }

    // A lone process blocked forever: nothing is enabled yet it never completes, which the search
    // must report as a deadlock.
    fn never_finishes(world: &mut World) {
        world.spawn("stuck", async {
            std::future::pending::<()>().await;
            Ok(())
        });
    }

    #[test]
    fn deadlock_is_detected() {
        let failed = explore(&never_finishes, &mut ()).unwrap_err();
        assert_eq!(failed.to_string(), "deadlock");
        failed.play();
    }
}
