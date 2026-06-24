use std::{error::Error as StdError, fmt};

use super::observer::Observer;
use crate::model::{FailureReason, State, StateView, World};

/// Enumerates interleavings of the program built by `setup` under Optimal DPOR.
///
/// `setup` builds the program once into a fresh [`World`](crate::World) (spawning
/// processes, creating the shared objects); it is invoked repeatedly and must be
/// deterministic.
/// `observer`'s [`step`](Observer::step) is notified at each discrete decision, including a
/// [`Step::Visit`](crate::Step::Visit) for every explored state — runnable, terminal, or failed;
/// pass `&mut ()` to observe nothing.
///
/// Returns `Ok(())` if every reachable interleaving terminates cleanly, or the first
/// [`FailedState`] encountered (a process error or a deadlock), at which point the search stops.
pub fn explore<'a>(
    setup: &'a dyn Fn(&mut World<'a>),
    observer: &mut impl Observer,
) -> Result<(), FailedState<'a>> {
    let root = StateView::new(setup).state();
    super::optimal::run(root, observer)
}

/// A reproducible failure returned by [`explore`].
///
/// Holds why the interleaving failed plus a replayable view of the prefix that led there, so the
/// offending schedule can be reproduced deterministically with [`play`](FailedState::play).
/// Implements [`std::error::Error`], [`std::fmt::Display`] (the reason) and a `Debug` that also
/// shows the failing trace.
///
/// It borrows the `setup` closure for replay, so it is neither `Send` nor `Sync` (nor, when the
/// closure captures borrowed locals, `'static`): it cannot be `?`-propagated into a boxed
/// `'static` error such as `anyhow::Error` or a `Box<dyn Error + Send + Sync>` test return.
/// Inspect it in place — [`reason`](FailedState::reason), its [`Display`](std::fmt::Display), or
/// [`play`](FailedState::play) — or copy out what you need (e.g. the `Display` string) before the
/// `setup` scope ends.
pub struct FailedState<'a> {
    reason: FailureReason,
    view: StateView<'a>,
}

impl<'a> FailedState<'a> {
    pub(super) fn new(reason: FailureReason, view: StateView<'a>) -> Self {
        Self { reason, view }
    }

    fn from_state(state: State<'a>) -> Self {
        let (reason, view) = state.into_failure();
        Self::new(reason, view)
    }

    /// The reason this interleaving failed — a process error or a deadlock.
    pub fn reason(&self) -> &FailureReason {
        &self.reason
    }

    /// Replays the failing prefix from scratch, reproducing the same failure as an
    /// independent [`FailedState`].
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

    // A consumer recvs more than the single producer sends: after the one message the
    // second recv blocks forever with no producer left, so the channel's withhold →
    // `settle` path must surface a deadlock through the search.
    fn starved_consumer(world: &mut World) {
        let (tx, rx) = world.channel::<i32>("ch");
        world.spawn("producer", async move {
            tx.send(1).await;
            Ok(())
        });
        world.spawn("consumer", async move {
            rx.recv().await;
            rx.recv().await; // nothing left to receive
            Ok(())
        });
    }

    #[test]
    fn channel_starvation_is_deadlock() {
        let failed = explore(&starved_consumer, &mut ()).unwrap_err();
        assert_eq!(failed.to_string(), "deadlock");
    }
}
