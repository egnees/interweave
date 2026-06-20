//! Synchronization primitives whose every observable operation is a scheduling point.
//!
//! Each primitive is implemented from scratch so that operations that may interact across
//! processes become explicit `.await` yield points carrying enough metadata (process id, object,
//! operation kind, target value) for the search layer to compute happens-before and dependency
//! relations. Currently this module provides [`Atomic`].

mod atomic;

/// A cloneable handle to a shared atomic cell whose operations are DPOR scheduling points.
///
/// See [`atomic::Handle`] for the operation semantics.
pub use atomic::Handle as Atomic;
