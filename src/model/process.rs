//! Shared process vocabulary: the process identifier and the output type every
//! process future yields.

use std::error::Error;

/// Index of a process in the executor's process table; doubles as its identity.
/// Exposed through [`Transition::pid`](crate::Transition::pid).
pub type ProcessID = usize;

/// Output of a process future: `Ok(())` on clean completion, or an error that the
/// model surfaces as a [`crate::ProcessError`].
///
/// The error is `Send + Sync` so it composes with the standard error ecosystem — a
/// process body can `?` into it from `anyhow`/`eyre` and the canonical
/// `Box<dyn Error + Send + Sync>` test-return type.
pub type ProcessResult = Result<(), Box<dyn Error + Send + Sync>>;
