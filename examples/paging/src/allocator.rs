//! The kernel heap: the [`GlobalAlloc`] interface, a dummy allocator that shows
//! its shape, and the `linked_list_allocator`-backed heap the kernel actually
//! uses.
//!
//! This is `blog_os` post-10 (*Heap Allocation*), done properly. The post builds
//! a heap in three steps, and all three live here:
//!
//! 1. **The interface.** Rust's `alloc` crate needs a `#[global_allocator]` that
//!    implements [`GlobalAlloc`]. [`Dummy`] is the post's first step: it compiles
//!    and returns null for every allocation, which is enough to make the "no
//!    global allocator" error go away while the real one is built.
//! 2. **The region.** A heap is a run of virtual pages backed by physical frames.
//!    [`init_heap`] maps [`HEAP_SIZE`] bytes at [`HEAP_START`] with the mapper and
//!    frame allocator the paging layer provides, then hands the region to the
//!    allocator.
//! 3. **The allocator.** [`ALLOCATOR`] is a
//!    [`LockedHeap`](linked_list_allocator::LockedHeap) — a linked-list allocator
//!    behind a spin lock — the same crate the real kernel in
//!    [`kernel/`](../../kernel) uses.
//!
//! The tutorial passes its mapper and frame allocator around by hand; here the
//! paging layer already stores them behind locks ([`crate::memory`]), so
//! [`init`] is the convenience wrapper `kernel_main` and the tests call, while
//! [`init_heap`] keeps the post's exact signature for contrast.

use alloc::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

use linked_list_allocator::LockedHeap;
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

/// The kernel's global allocator, empty until [`init_heap`] (or [`init`]) runs.
///
/// `LockedHeap` is a linked-list allocator behind a spin lock, so it is usable
/// from any context that can take the lock — including an interrupt handler.
#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// A `GlobalAlloc` that always fails.
///
/// This is the post's first step, kept here so the diff against the tutorial
/// reads as "the same code, wired into this repository": it shows what a global
/// allocator has to provide (an `alloc` and a `dealloc`) before any real
/// bookkeeping exists. It is never installed.
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
/// This is the post's function, kept with its exact signature: it takes the
/// mapper and frame allocator explicitly so the two are visible in the call.
/// [`init`] is the wrapper that supplies the paging layer's stored ones.
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
        ALLOCATOR.lock().init(HEAP_START as *mut u8, HEAP_SIZE);
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
