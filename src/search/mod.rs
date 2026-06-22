//! The strategy / exploration layer — the model checker proper.
//!
//! This module enumerates interleavings of a concurrent program and reports the first failure
//! it finds. The single entry point is [`explore`], which runs Optimal DPOR; pass an
//! [`Observer`] to record the explored tree; the no-op `()` observer ignores everything.
//!
//! The strategy reaches the modeled system only through the public `model` surface (never the
//! synchronization primitives directly), and reports what it explores purely through typed
//! [`Observer`] callbacks.

mod explore;
mod observer;
mod optimal;
mod step;

pub use explore::{FailedState, explore};
pub use observer::Observer;

// The step-instrumentation hook: a public, `viz`-gated API a visualizer builds on
// top of (the renderer lives in the external `unweave` crate). The `step`/`explore`
// modules stay private; the items are `pub` and surfaced only through these
// re-exports. The
// always-compiled internals (`optimal.rs`, `explore.rs`, the `step.rs` golden test)
// reach them via direct module paths, so a no-`viz` build still compiles and tests.
#[cfg(feature = "viz")]
pub use {
    explore::explore_stepped,
    step::{RaceOutcome, Step, StepCx, StepObserver, WakeupNode},
};
