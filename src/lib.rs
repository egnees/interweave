//! Stateless model checking for small concurrent programs.
//!
//! `interweave` explores the interleavings of concurrent processes and checks
//! that every one of them is correct. Processes are written as ordinary Rust
//! [`Future`]s and driven by a custom single-threaded, deterministic executor.
//! Synchronization primitives ([`Atomic`] and an MPSC channel: [`Sender`] /
//! [`Receiver`]) are implemented
//! from scratch so that every operation that can interact with another process
//! becomes an explicit scheduling point — an `.await` that hands control back to
//! the checker. The executor stays deliberately dumb; all interleaving control
//! lives in the primitives and in the exploration strategy on top.
//!
//! That strategy is [**Optimal DPOR**](https://doi.org/10.1145/2535838.2535845)
//! (Abdulla et al., POPL'14): it explores exactly one interleaving per
//! Mazurkiewicz equivalence class, with no redundant work and no sleep-set
//! blocking. A naive exhaustive [`Strategy::Dfs`] is also available for
//! cross-checking.
//!
//! [`Future`]: std::future::Future
//!
//! # Example
//!
//! A `producer` hands a value to a `consumer` through a one-shot `ready` flag,
//! but raises the flag *before* it writes the value. Optimal DPOR finds the
//! interleaving where the consumer sees `ready` set yet reads the stale,
//! not-yet-published value — the unsafe-publication race behind broken
//! double-checked locking:
//!
//! ```
//! use interweave::{Strategy, World, explore};
//!
//! fn publish(world: &mut World) {
//!     let data = world.atomic("data", 0);
//!     let ready = world.atomic("ready", 0);
//!
//!     let (data_w, ready_w) = (data.clone(), ready.clone());
//!     world.spawn("producer", async move {
//!         ready_w.store(1).await; // announce the value...
//!         data_w.store(42).await; // ...before it has been written
//!         Ok(())
//!     });
//!
//!     world.spawn("consumer", async move {
//!         // No wait loop: the checker explores the schedule where the flag is
//!         // already set, so a single guarded read stays a finite safety check.
//!         if ready.load().await == 1 {
//!             let v = data.load().await;
//!             if v != 42 {
//!                 return Err(format!("read the value before it was published: {v}").into());
//!             }
//!         }
//!         Ok(())
//!     });
//! }
//!
//! // `()` is the no-op observer; Optimal DPOR finds the schedule where the
//! // consumer sees `ready == 1` but still reads the stale `data`.
//! explore(&publish, &mut (), Strategy::Optimal).expect_err("publishes the flag before the value");
//! ```
//!
//! # Architecture
//!
//! Three module layers, dependencies pointing downward (`search → model`, with
//! `model ↔ sync` a deliberate mutual pair):
//!
//! - **`model`** — the modeled system and its execution: the deterministic executor, the [`World`]
//!   / [`State`] a program builds, the [`Transition`] the strategy picks, and the [`ProcessError`]
//!   / [`FailureReason`] verdicts.
//! - **`sync`** — synchronization primitives whose every observable operation is an `.await` yield
//!   point: [`Atomic`] and an unbounded MPSC channel ([`Sender`] / [`Receiver`]).
//! - **`search`** — the exploration algorithms: [`explore`] dispatches on a [`Strategy`], reports
//!   the first [`FailedState`], and calls an [`Observer`] at every visited state.
//!
//! # Custom synchronization objects
//!
//! [`Atomic`] is built on the same public surface you can use yourself: implement the [`Object`]
//! trait for a from-scratch primitive (a lock, a channel, a barrier) so that each of its
//! observable operations becomes a [`Transition`] the strategy schedules, then register it with
//! [`World::register`]. See [`Object`] for the operation lifecycle and the dependency relation that
//! drives the reduction, and `examples/custom_object.rs` for a tiny worked primitive.

mod model;
mod search;
mod sync;

pub use model::{
    FailureReason, Object, ObjectID, ProcessError, ProcessID, ProcessResult, State, Transition,
    World,
};
pub use search::{FailedState, Observer, Strategy, explore};
pub use sync::{Atomic, Receiver, Sender};

// The step-instrumentation hook for Optimal DPOR: the only public surface the `viz`
// feature adds, and only when it is on. A consumer drives `explore_stepped(&setup,
// &mut observer)` with its own [`StepObserver`] and reads each `Step<'_>` through the
// borrowed `StepCx`/`WakeupNode` views — the external `unweave` crate's console
// renderer is exactly such a consumer, built entirely on this public API.
#[cfg(feature = "viz")]
pub use search::{RaceOutcome, Step, StepCx, StepObserver, WakeupNode, explore_stepped};
