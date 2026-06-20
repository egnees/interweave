//! Stateless model checking for small concurrent programs.
//!
//! `interweave` explores the interleavings of concurrent processes and checks
//! that every one of them is correct. Processes are written as ordinary Rust
//! [`Future`]s and driven by a custom single-threaded, deterministic executor.
//! Synchronization primitives ([`Atomic`] today; channels later) are implemented
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
//! Two processes race to store into one shared atomic. The two orderings are not
//! equivalent, so Optimal DPOR explores both and confirms neither fails:
//!
//! ```
//! use interweave::{Strategy, World, explore};
//!
//! fn program(world: &mut World) {
//!     let x = world.atomic("x", 0u32);
//!     let a = x.clone();
//!     world.spawn("writer-1", async move {
//!         a.store(1).await;
//!         Ok(())
//!     });
//!     world.spawn("writer-2", async move {
//!         x.store(2).await;
//!         Ok(())
//!     });
//! }
//!
//! // `()` is the no-op observer; the result is `Ok` iff every interleaving passes.
//! explore(&program, &mut (), Strategy::Optimal).expect("no interleaving fails");
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
//!   point. Currently [`Atomic`].
//! - **`search`** — the exploration algorithms: [`explore`] dispatches on a [`Strategy`], reports
//!   the first [`FailedState`], and calls an [`Observer`] at every visited state.

mod model;
mod search;
mod sync;

pub use model::{FailureReason, ProcessError, ProcessResult, State, Transition, World};
pub use search::{FailedState, Observer, Strategy, explore};
pub use sync::Atomic;
