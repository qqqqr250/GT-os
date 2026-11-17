//! Hand-written synchronisation primitives for the post-12 task module.
//!
//! `blog_os` post-12 takes its bounded queue, once-cell and waker from the
//! `crossbeam-queue`, `conquer-once` and `futures-util` crates. This kernel keeps
//! the repository's habit of writing its own infrastructure: [`queue`] is a
//! bounded lock-free multi-producer/multi-consumer queue and [`once_cell`] is a
//! one-shot initialiser, both with unit tests. Only
//! [`futures_util::task::AtomicWaker`] and the [`futures_util::stream::Stream`]
//! trait are taken from a dependency (see [`crate::task::keyboard`]).

pub mod once_cell;
pub mod queue;

pub use once_cell::OnceCell;
pub use queue::ArrayQueue;
