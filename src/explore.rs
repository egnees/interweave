use std::{error::Error as StdError, fmt};

use crate::{
    executor::ProcessError,
    state::{State, StateView, World},
};

// Enumerates every interleaving of the program built by `setup`, returning the
// first failure if one is found.
pub fn explore<'a>(setup: &'a dyn Fn(&mut World<'a>)) -> Result<(), FailedState<'a>> {
    let root = StateView::new(setup);
    match root.state() {
        Ok(state) => dfs(&state),
        Err(f) => Err(FailedState::process(root, f)),
    }
}

// A state is deadlocked when no transition is enabled yet some process is
// unfinished. This judgement is the strategy's; the state only reports the facts.
fn deadlocked(state: &State) -> bool {
    state.enabled().is_empty() && state.pending() > 0
}

fn dfs<'a>(state: &State<'a>) -> Result<(), FailedState<'a>> {
    if deadlocked(state) {
        return Err(FailedState::deadlock(state.view()));
    }
    // An empty `enabled` here means a terminal state: every process finished.
    for t in state.enabled() {
        let mut next = state.fork();
        if let Err(f) = next.apply(t) {
            return Err(FailedState::process(next.view(), f));
        }
        dfs(&next)?;
    }
    Ok(())
}

// A reproducible failure: a view of the failing state plus what went wrong, so
// `play` can replay the offending interleaving deterministically.
pub struct FailedState<'a> {
    reason: FailureReason,
    view: StateView<'a>,
}

#[derive(Debug, thiserror::Error)]
enum FailureReason {
    #[error("{0}")]
    Process(#[source] ProcessError),
    #[error("deadlock")]
    Deadlock,
}

impl<'a> FailedState<'a> {
    fn process(view: StateView<'a>, failure: ProcessError) -> Self {
        Self {
            reason: FailureReason::Process(failure),
            view,
        }
    }

    fn deadlock(view: StateView<'a>) -> Self {
        Self {
            reason: FailureReason::Deadlock,
            view,
        }
    }

    // Replays the recorded view from scratch and checks the same failure recurs.
    // A mismatch means the model is non-deterministic, which is a bug worth a
    // panic — with the divergence in the message.
    pub fn play(&self) -> FailedState<'a> {
        let replayed = self.view.state();
        let reproduced = match (&self.reason, &replayed) {
            (FailureReason::Process(orig), Err(f)) => f.to_string() == orig.to_string(),
            (FailureReason::Deadlock, Ok(state)) => deadlocked(state),
            _ => false,
        };
        assert!(
            reproduced,
            "replay did not reproduce {self}: model is non-deterministic"
        );
        // `reproduced` guarantees the pairing: a process failure replays to
        // `Err`, a deadlock to an `Ok` state with nothing enabled.
        match replayed {
            Err(f) => FailedState::process(self.view.clone(), f),
            Ok(_) => FailedState::deadlock(self.view.clone()),
        }
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
    use crate::state::World;

    fn racy(world: &mut World) {
        let atomic = world.atomic(0u32);
        let r1 = atomic.clone();
        let r2 = atomic.clone();
        world.spawn(async move {
            r1.store(1).await;
            Ok(())
        });
        world.spawn(async move {
            if r2.load().await == 1 {
                Ok(())
            } else {
                Err("unexpected value".into())
            }
        });
    }

    #[test]
    fn play_reproduces_failure() {
        let failed = explore(&racy).unwrap_err();
        let again = failed.play();
        assert_eq!(failed.to_string(), again.to_string());
    }

    // Two independent stores on one atomic: every interleaving runs cleanly.
    fn two_writers(world: &mut World) {
        let atomic = world.atomic(0u32);
        let r1 = atomic.clone();
        let r2 = atomic.clone();
        world.spawn(async move {
            r1.store(1).await;
            Ok(())
        });
        world.spawn(async move {
            r2.store(2).await;
            Ok(())
        });
    }

    #[test]
    fn explores_clean_program() {
        assert!(explore(&two_writers).is_ok());
    }

    // A process that never makes progress: blocked with nothing enabled.
    fn never_finishes(world: &mut World) {
        world.spawn(async {
            std::future::pending::<()>().await;
            Ok(())
        });
    }

    #[test]
    fn deadlock_is_detected() {
        let failed = explore(&never_finishes).unwrap_err();
        assert_eq!(failed.to_string(), "deadlock");
        failed.play();
    }
}
