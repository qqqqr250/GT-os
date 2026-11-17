//! The kernel heap and the three allocator designs that hand it out.
//!
//! This is a forward hook into a hardware-interrupts crate (post-07): it carries
//! post-10's heap *interface* and post-11's allocator *designs* forward, so the
//! later examples' code runs here too. Post-10 built the heap *interface* — a
//! `#[global_allocator]` and a mapped region — and left the implementation to the
//! `linked_list_allocator` crate. Post-11 replaces that implementation with three
//! designs written from scratch, each a [`GlobalAlloc`] in its own module:
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
//!   installed below.
//!
//! [`init_heap`] is post-10's function, kept with its exact signature: it maps
//! [`HEAP_SIZE`] bytes at [`HEAP_START`] through the paging layer and then hands
//! the region to whichever design is installed. [`init`] is the wrapper that
//! supplies the paging layer's stored mapper and frame allocator, and [`Dummy`] is
//! post-10's first step, kept here so the diff against the tutorial reads as "the
//! same code, wired into this repository".

pub mod bump;
pub mod fixed_size_block;
pub mod linked_list;

use alloc::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

use fixed_size_block::FixedSizeBlockAllocator;
use x86_64::VirtAddr;
use x86_64::structures::paging::mapper::MapToError;
use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB};

use crate::memory;

/// The first virtual address of the kernel heap.
///
/// `PML4` index 136, far above the bootloader's dynamic window (which starts at
/// index 1), so building the heap really has to create new page tables and the
/// frame allocator is exercised.
pub const HEAP_START: usize = 0x_4444_4444_0000;

/// The size of the kernel heap: 100 KiB.
pub const HEAP_SIZE: usize = 100 * 1024;

/// The kernel's global allocator: post-11's fixed-size block design.
///
/// [`Locked`] puts the allocator behind a spin lock, so it is usable from any
/// context that can take the lock — including an interrupt handler. The design is
/// `const`-constructible, which is what lets it initialize a `static`.
#[global_allocator]
static ALLOCATOR: Locked<FixedSizeBlockAllocator> = Locked::new(FixedSizeBlockAllocator::new());

/// A `GlobalAlloc` that always fails.
///
/// This is post-10's first step, kept here so the heap *interface* is visible
/// before any real bookkeeping exists: it shows what a global allocator has to
/// provide (an `alloc` and a `dealloc`) and nothing more. It is never installed.
pub struct Dummy;

// SAFETY: the trait requires `alloc` to return either a valid pointer or null,
// and `dealloc` to accept only pointers this allocator handed out. `alloc` always
// returns null, so `dealloc` can never be called with a valid pointer; panicking
// on it is the loudest possible way to say so.
unsafe impl GlobalAlloc for Dummy {
    unsafe fn alloc(&self, _layout: Layout) -> *mut u8 {
        null_mut()
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        panic!("dealloc should never be called")
    }
}

/// Maps [`HEAP_SIZE`] bytes at [`HEAP_START`] and hands the region to the global
/// allocator.
///
/// This is post-10's function, kept with its exact signature: it takes the mapper
/// and frame allocator explicitly so the two are visible in the call. [`init`] is
/// the wrapper that supplies the paging layer's stored ones.
///
/// Every allocator design provides the same `init(heap_start, heap_size)`
/// interface, so this function does not change when the installed design does.
///
/// # Errors
///
/// Returns [`MapToError::FrameAllocationFailed`] if the frame allocator runs out
/// of frames, or whatever `map_to` reports — for example
/// [`MapToError::PageAlreadyMapped`] if the heap window is already in use.
pub fn init_heap(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), MapToError<Size4KiB>> {
    let page_range = {
        let heap_start = VirtAddr::new(HEAP_START as u64);
        let heap_end = heap_start + HEAP_SIZE as u64 - 1u64;
        let heap_start_page = Page::containing_address(heap_start);
        let heap_end_page = Page::containing_address(heap_end);
        Page::range_inclusive(heap_start_page, heap_end_page)
    };

    for page in page_range {
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(MapToError::FrameAllocationFailed)?;
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        // SAFETY: `HEAP_START` is not used by anything else, so every page in the
        // range is mapped exactly once, and `frame` was just handed out by the
        // allocator, so nothing aliases it.
        unsafe { mapper.map_to(page, frame, flags, frame_allocator)?.flush() };
    }

    // SAFETY: the whole heap region is now mapped and writable, and this is the
    // only place the global allocator is initialized.
    unsafe {
        ALLOCATOR.lock().init(HEAP_START, HEAP_SIZE);
    }

    Ok(())
}

/// Initializes the heap through the paging layer's stored mapper and frame
/// allocator.
///
/// [`crate::memory::init_paging`] must have run first: it is what creates the
/// single mapper and frame allocator this wrapper borrows.
///
/// # Errors
///
/// Returns an error if paging is not initialized, if the frame allocator ran out
/// of frames, or if the heap window is already mapped.
pub fn init() -> Result<(), &'static str> {
    // The mapper lock is taken before the frame-allocator lock, the same order
    // `memory::map_page` uses, so this cannot deadlock.
    let result =
        memory::with_mapper(|mapper| memory::with_frames(|frames| init_heap(mapper, frames)));
    match result {
        None => Err("paging is not initialized"),
        Some(None) => Err("the frame allocator is not initialized"),
        Some(Some(Ok(()))) => Ok(()),
        Some(Some(Err(MapToError::FrameAllocationFailed))) => {
            Err("the frame allocator ran out of frames")
        }
        Some(Some(Err(MapToError::ParentEntryHugePage))) => {
            Err("a page-table entry is a huge page")
        }
        Some(Some(Err(MapToError::PageAlreadyMapped(_)))) => Err("a heap page is already mapped"),
    }
}

/// A wrapper around [`spin::Mutex`] that lets a [`GlobalAlloc`] be implemented on
/// a `static`.
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
/// `align` must be a power of two, which the [`GlobalAlloc`] trait guarantees:
/// the [`Layout`] it passes always carries a power-of-two alignment. That makes
/// the rounding a single add, mask and negate.
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
