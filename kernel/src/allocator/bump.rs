//! A bump allocator: the simplest possible heap design.
//!
//! The allocator keeps three pointers: the start and end of the heap and a `next`
//! pointer that starts at the beginning. Every allocation rounds `next` up to the
//! requested alignment, hands out the bytes from there and moves `next` past
//! them. Nothing is ever written into the free memory, so allocation is a handful
//! of instructions.
//!
//! The catch is deallocation. A block cannot be freed on its own — the design
//! keeps no record of where it was — so `dealloc` only counts allocations down.
//! When the count reaches zero, *every* allocation is gone and the whole heap is
//! free again, so `next` resets to the start. A single long-lived allocation keeps
//! the count above zero forever and the heap is never reused. Bump allocation is
//! therefore useful for arenas and scratch memory, not as a general-purpose heap;
//! the kernel installs the fixed-size block design instead, and the test below
//! pins the starving behaviour down on a small scratch region.
// The kernel installs the fixed-size block design, so this one is kept as a
// reference implementation and exercised only by its own tests. `memory` never
// names it, so without this allow rustc would report its items as dead code.
#![allow(dead_code)]

use super::{Locked, align_up};
use alloc::alloc::{GlobalAlloc, Layout};
use core::ptr;

/// A linear allocator over a fixed region.
pub struct BumpAllocator {
    heap_start: usize,
    heap_end: usize,
    next: usize,
    allocations: usize,
}

impl BumpAllocator {
    /// Creates an empty bump allocator.
    ///
    /// It must be [`init`](BumpAllocator::init)ialized before it can allocate.
    #[must_use]
    pub const fn new() -> Self {
        BumpAllocator {
            heap_start: 0,
            heap_end: 0,
            next: 0,
            allocations: 0,
        }
    }

    /// Initializes the bump allocator with the given heap bounds.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the given memory range is unused, and that
    /// this method is called only once.
    pub unsafe fn init(&mut self, heap_start: usize, heap_size: usize) {
        self.heap_start = heap_start;
        self.heap_end = heap_start.saturating_add(heap_size);
        self.next = heap_start;
    }
}

// A bump allocator starts empty, which is what `new` builds.
impl Default for BumpAllocator {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: `alloc` returns either a valid, uniquely owned pointer into the region
// `init` was given, or null; `dealloc` only ever counts, so it never invalidates
// memory another allocation still holds. Every pointer handed out comes from the
// region and is aligned to the requested power-of-two alignment by `align_up`.
unsafe impl GlobalAlloc for Locked<BumpAllocator> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut bump = self.lock(); // get a mutable reference

        let alloc_start = align_up(bump.next, layout.align());
        let Some(alloc_end) = alloc_start.checked_add(layout.size()) else {
            return ptr::null_mut();
        };

        if alloc_end > bump.heap_end {
            ptr::null_mut() // out of memory
        } else {
            bump.next = alloc_end;
            bump.allocations += 1;
            alloc_start as *mut u8
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        let mut bump = self.lock(); // get a mutable reference

        bump.allocations -= 1;
        if bump.allocations == 0 {
            bump.next = bump.heap_start;
        }
    }
}

// The design's defining property — that it only reuses memory once every
// allocation is gone — is testable on a small scratch region, without a mapped
// heap.
#[cfg(test)]
mod tests {
    use super::{BumpAllocator, Locked};
    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::UnsafeCell;

    /// A small scratch region the design tests allocate from.
    #[repr(align(16))]
    struct Scratch(UnsafeCell<[u8; 4096]>);

    // SAFETY: the tests in this crate run sequentially on one CPU, and every test
    // re-initializes its allocator over the whole region before allocating, so no
    // two live allocators ever alias it.
    unsafe impl Sync for Scratch {}

    static SCRATCH: Scratch = Scratch(UnsafeCell::new([0; 4096]));

    /// The `(start, size)` of the shared scratch region.
    fn scratch() -> (usize, usize) {
        (SCRATCH.0.get() as usize, 4096)
    }

    #[test_case]
    fn bump_hands_out_consecutive_blocks() {
        let allocator = Locked::new(BumpAllocator::new());
        let (start, size) = scratch();
        // SAFETY: the scratch region is unused, and this is the only allocator
        // initialized over it in this test.
        unsafe { allocator.lock().init(start, size) };

        // SAFETY: the allocator was just initialized over the scratch region.
        let first = unsafe { allocator.alloc(Layout::new::<u64>()) };
        // SAFETY: as above.
        let second = unsafe { allocator.alloc(Layout::new::<u64>()) };
        assert_eq!(first as usize, start);
        assert_eq!(
            second as usize,
            start + 8,
            "the second block bumps past the first"
        );
    }

    #[test_case]
    fn bump_starves_when_a_long_lived_allocation_is_live() {
        let allocator = Locked::new(BumpAllocator::new());
        let (start, size) = scratch();
        // SAFETY: the scratch region is unused, and this is the only allocator
        // initialized over it in this test.
        unsafe { allocator.lock().init(start, size) };

        // SAFETY: the allocator was just initialized over the scratch region.
        let long_lived = unsafe { allocator.alloc(Layout::new::<u64>()) };
        assert!(!long_lived.is_null());

        // With one allocation still live, the counter never returns to zero, so
        // `next` never resets: each freed block is lost and the region is
        // exhausted after `size / 8 - 1` more allocations.
        let mut served = 0usize;
        loop {
            // SAFETY: as above.
            let block = unsafe { allocator.alloc(Layout::new::<u64>()) };
            if block.is_null() {
                break;
            }
            // SAFETY: `block` came from this allocator and is no longer used.
            unsafe { allocator.dealloc(block, Layout::new::<u64>()) };
            served += 1;
            assert!(
                served <= size / 8,
                "the bump allocator must run out of memory"
            );
        }
        assert_eq!(
            served,
            size / 8 - 1,
            "only the first block is kept; the rest are never reused"
        );
        assert!(!long_lived.is_null());
    }
}
