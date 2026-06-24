//! Stateless model checking for small concurrent programs.
//!
//! `interweave` explores the interleavings of concurrent processes and checks
//! that every one of them is correct. Processes are written as ordinary Rust
//! [`Future`]s and driven by a custom single-threaded, deterministic executor.
//! Synchronization primitives — the built-in [`Atomic`] and an MPSC channel ([`Sender`] /
//! [`Receiver`]) — are implemented from scratch so that every operation that can interact
//! with another process becomes an explicit scheduling point: an `.await` that hands
//! control back to the checker.
//!
//! That strategy is [**Optimal DPOR**](https://doi.org/10.1145/2535838.2535845)
//! (Abdulla et al., POPL'14): it explores exactly one interleaving per
//! Mazurkiewicz equivalence class.
//!
//! [`Future`]: std::future::Future
//!
//! # Example
//!
//! A `writer` stores a value and a `reader` expects to see it — but nothing
//! orders the two, so on the interleaving where the read beats the store the
//! reader observes the initial value and fails. Optimal DPOR finds exactly that
//! schedule:
//!
//! ```
//! use interweave::{World, explore};
//!
//! fn racy(world: &mut World) {
//!     let x = world.atomic("x", 0);
//!     let writer = x.clone();
//!     world.spawn("writer", async move {
//!         writer.store(1).await;
//!         Ok(())
//!     });
//!     world.spawn("reader", async move {
//!         match x.load().await {
//!             1 => Ok(()),
//!             v => Err(format!("read {v} before the store landed").into()),
//!         }
//!     });
//! }
//!
//! // `()` is the no-op observer. Optimal DPOR finds the schedule where the
//! // reader runs first and sees the initial `0`.
//! explore(&racy, &mut ()).expect_err("the reader can run before the writer");
//! ```
//!
//! # How it fits together
//!
//! - **Build** a program on a [`World`]: [`spawn`](World::spawn) the processes and create the
//!   shared objects they communicate through.
//! - **Communicate** through synchronization primitives whose every observable operation is a
//!   scheduling point — the built-in [`Atomic`] and unbounded MPSC channel ([`Sender`] /
//!   [`Receiver`]), or your own via [`Object`] and [`World::register`].
//! - **Explore** with [`explore`]: it runs Optimal DPOR over the program and returns the first
//!   [`FailedState`], or `Ok(())` if no interleaving fails. An [`Observer`] watches the search
//!   through one [`Observer::step`] callback fired at each decision the algorithm makes — a
//!   [`Step::Visit`] for every state it reaches and a [`Step::Maximal`] for every complete
//!   interleaving, among other [`Step`] cases — delivered with a [`StepCx`] view.
//!
//! # Custom synchronization objects
//!
//! [`Atomic`] is built on the same public surface you can use yourself: implement the [`Object`]
//! trait for a from-scratch primitive (a lock, a channel, a barrier) so that each of its
//! observable operations becomes a [`Transition`] the strategy schedules, then register it with
//! [`World::register`]. See [`Object`] for the operation lifecycle and the dependency relation that
//! drives the reduction, and `examples/custom_object.rs` for a tiny worked primitive.

#![warn(missing_docs)]

mod model;
mod search;
mod sync;

pub use model::{
    FailureReason, Object, ObjectID, ProcessError, ProcessID, ProcessResult, State, Transition,
    World,
};
pub use search::{FailedState, Observer, RaceOutcome, Step, StepCx, WakeupNode, explore};
pub use sync::{Atomic, Receiver, Sender};

// README code blocks are compiled as doctests so they cannot drift from the API.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
