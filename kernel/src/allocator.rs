//! The three heap allocator designs from `blog_os` post-11 (*Allocator
//! Designs*), and the [`Locked`] wrapper that lets one be a `#[global_allocator]`.
//!
//! The kernel's heap *region* and its `#[global_allocator]` live in
//! [`crate::memory`]: the region is taken from the top of the largest usable
//! physical region and reached through the bootloader's physical-memory window.
//! What lives here is the allocator's own *design*, written from scratch rather
//! than delegated to the `linked_list_allocator` crate:
//!
//! - [`bump`]: a linear "bump" pointer. The fastest design, but it can only reuse
//!   memory once *every* allocation has been freed, so a single long-lived
//!   allocation starves it.
//! - [`linked_list`]: a free list threaded through the freed blocks themselves, so
//!   any freed region can be reused immediately. Allocation is a linear scan and
//!   the list can fragment, but no memory is ever stranded.
//! - [`fixed_size_block`]: one free list per size class, with a
//!   [`linked_list_allocator`] fallback for large allocations. Fast allocation and
//!   deallocation for the common sizes; the design the post settles on, and the one
//!   [`crate::memory`] installs.
//!
//! The designs' unit tests live beside them (they run in the library's in-QEMU
//! test binary against a small scratch region), and the installed design is
//! exercised end to end by `kernel/tests/basic_boot.rs`.

pub mod bump;
pub mod fixed_size_block;
pub mod linked_list;

/// A wrapper around [`spin::Mutex`] that lets a `GlobalAlloc` be implemented on a
/// `static`.
///
/// Rust's orphan rules forbid implementing a foreign trait (`GlobalAlloc`) for a
/// foreign type (`spin::Mutex`), so each design is implemented for
/// `Locked<Design>` instead: the wrapper is local, and the lock is what makes the
/// allocator usable from an interrupt handler.
pub struct Locked<A> {
    inner: spin::Mutex<A>,
}

impl<A> Locked<A> {
    /// Wraps `inner` in a spin lock, without locking it.
    pub const fn new(inner: A) -> Self {
        Locked {
            inner: spin::Mutex::new(inner),
        }
    }

    /// Locks the wrapper and returns a guard to the allocator inside.
    pub fn lock(&self) -> spin::MutexGuard<'_, A> {
        self.inner.lock()
    }
}

/// Rounds the address `addr` up to the next multiple of `align`.
///
/// `align` must be a power of two, which the `GlobalAlloc` trait guarantees: the
/// `Layout` it passes always carries a power-of-two alignment. That makes the
/// rounding a single add, mask and negate.
fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

// The pure helper above is the one piece of the allocator that does not need a
// heap to test.
#[cfg(test)]
mod tests {
    use super::align_up;

    #[test_case]
    fn align_up_rounds_up_to_the_alignment() {
        assert_eq!(align_up(0, 8), 0);
        assert_eq!(align_up(1, 8), 8);
        assert_eq!(align_up(8, 8), 8, "an aligned address must not move");
        assert_eq!(align_up(9, 8), 16);
        assert_eq!(align_up(0x1234, 0x1000), 0x2000);
    }
}
