//! The strategy / exploration layer — the model checker proper.
//!
//! This module enumerates interleavings of a concurrent program and reports the first failure
//! it finds. The single entry point is [`explore`]; pick between exhaustive DFS and Optimal
//! DPOR with [`Strategy`]. Pass an [`Observer`] to record the explored tree; the no-op `()`
//! observer ignores everything.
//!
//! The strategy reaches the modeled system only through the public `model` surface (it touches
//! neither the synchronization primitives nor the output layer), and communicates back to a
//! recorder purely through typed [`Observer`] callbacks.

mod explore;
mod observer;
mod optimal;

pub use explore::{FailedState, Strategy, explore};
pub use observer::Observer;
