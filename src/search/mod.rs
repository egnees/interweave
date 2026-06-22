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

pub use explore::{FailedState, explore};

// The step types are the public hook for [`Observer::step`]: a consumer reads each
// `Step<'_>` through the borrowed `StepCx` / `WakeupNode` views. They live in the
// `observer` module alongside the trait and are surfaced through this re-export.
pub use observer::{Observer, RaceOutcome, Step, StepCx, WakeupNode};
