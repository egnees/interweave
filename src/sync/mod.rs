//! Synchronization primitives whose every observable operation is a scheduling point.
//!
//! Operations that may interact across processes become explicit `.await` yield points, so the
//! checker can interleave them. This module provides [`Atomic`] and an unbounded MPSC channel
//! ([`Sender`] / [`Receiver`]).

mod atomic;
mod channel;

pub use atomic::Handle as Atomic;

pub use channel::{Receiver, Sender};

pub(crate) use channel::ChannelHandle;
