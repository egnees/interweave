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
//! Two accounts hold a fixed total of 100. A `transfer` moves 10 from `a` to `b`
//! in separate, unlocked steps; an `audit` reads both and checks the total. With
//! no lock held across the two accounts, Optimal DPOR finds the interleaving
//! where the auditor catches the money mid-transfer:
//!
//! ```
//! use interweave::{Strategy, World, explore};
//!
//! fn bank(world: &mut World) {
//!     let a = world.atomic("a", 100);
//!     let b = world.atomic("b", 0);
//!
//!     let (from, to) = (a.clone(), b.clone());
//!     world.spawn("transfer", async move {
//!         let av = from.load().await;
//!         from.store(av - 10).await;
//!         let bv = to.load().await;
//!         to.store(bv + 10).await;
//!         Ok(())
//!     });
//!
//!     world.spawn("audit", async move {
//!         let av = a.load().await;
//!         let bv = b.load().await;
//!         if av + bv != 100 {
//!             return Err(format!("invariant violated: a={av} + b={bv}").into());
//!         }
//!         Ok(())
//!     });
//! }
//!
//! // `()` is the no-op observer; Optimal DPOR finds a schedule that breaks the
//! // `a + b == 100` invariant.
//! explore(&bank, &mut (), Strategy::Optimal).expect_err("the transfer has a race");
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
