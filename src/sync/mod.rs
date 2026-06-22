//! Synchronization primitives whose every observable operation is a scheduling point.
//!
//! Each primitive is implemented from scratch so that operations that may interact across
//! processes become explicit `.await` yield points carrying enough metadata (process id, object,
//! operation kind, target value) for the search layer to compute happens-before and dependency
//! relations. This module provides [`Atomic`] and an unbounded MPSC channel.

mod atomic;
mod channel;

pub use atomic::Handle as Atomic;

pub use channel::{Receiver, Sender};

pub(crate) use channel::ChannelHandle;
