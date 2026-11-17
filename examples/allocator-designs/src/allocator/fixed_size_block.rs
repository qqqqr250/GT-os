//! A fixed-size block allocator: one free list per size class, with a
//! linked-list fallback for the rest.
//!
//! The linked-list allocator's scan cost and fragmentation both come from having
//! to fit *different* sizes into one list. This design sidesteps that by
//! rounding every allocation up to one of a fixed set of power-of-two sizes and
//! keeping a separate free list per size. A node in a size class's list no longer
//! needs to store its own size — the list it is on already implies it — and every
//! node in a list fits every request for that class, so `alloc` and `dealloc` are
//! a single list pop and push.
//!
//! Requests larger than the biggest class (2 KiB here) cannot be served this way,
//! so they go to an embedded [`linked_list_allocator::Heap`]. That fallback is
//! also where new blocks come from: the lists start empty and fill lazily the
//! first time each class is used. Because blocks are only ever moved *between*
//! the fallback and a list, never back to the fallback, a class's memory stays
//! reserved for it — which trades some internal fragmentation for speed.
//!
//! The post concludes that this is the right design for a kernel, so it is the
//! one installed as the global allocator in [`crate::allocator`].

use super::Locked;
use alloc::alloc::{GlobalAlloc, Layout};
use core::{
    mem,
    ptr::{self, NonNull},
};

/// The block sizes to use.
///
/// Each must be a power of two because the size is also used as the block's
/// alignment (alignments must always be powers of two).
const BLOCK_SIZES: &[usize] = &[8, 16, 32, 64, 128, 256, 512, 1024, 2048];

/// Chooses the block size class for the given layout.
///
/// Returns an index into [`BLOCK_SIZES`], or `None` if the layout is larger than
/// the biggest class and must go to the fallback allocator.
fn list_index(layout: &Layout) -> Option<usize> {
    let required_block_size = layout.size().max(layout.align());
    BLOCK_SIZES.iter().position(|&s| s >= required_block_size)
}

/// A free block in a size class's list. Its size is implied by the list it is on.
struct ListNode {
    next: Option<&'static mut ListNode>,
}

/// A size-classed block allocator with a linked-list fallback.
pub struct FixedSizeBlockAllocator {
    list_heads: [Option<&'static mut ListNode>; BLOCK_SIZES.len()],
    fallback_allocator: linked_list_allocator::Heap,
}

impl FixedSizeBlockAllocator {
    /// Creates an empty `FixedSizeBlockAllocator`.
    ///
    /// It must be [`init`](FixedSizeBlockAllocator::init)ialized before it can
    /// allocate.
    #[must_use]
    pub const fn new() -> Self {
        // `Option<&mut T>` is not `Copy`, so `[None; N]` is not available in a
        // `const fn`; a named constant makes the repeat expression work.
        const EMPTY: Option<&'static mut ListNode> = None;
        FixedSizeBlockAllocator {
            list_heads: [EMPTY; BLOCK_SIZES.len()],
            fallback_allocator: linked_list_allocator::Heap::empty(),
        }
    }

    /// Initializes the allocator with the given heap bounds.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the given heap bounds are valid and that
    /// the heap is unused, and must call this method only once.
    pub unsafe fn init(&mut self, heap_start: usize, heap_size: usize) {
        // SAFETY: forwarded to the caller, who guarantees the region is unused.
        unsafe {
            self.fallback_allocator
                .init(heap_start as *mut u8, heap_size);
        }
    }

    /// Allocates using the fallback allocator.
    fn fallback_alloc(&mut self, layout: Layout) -> *mut u8 {
        match self.fallback_allocator.allocate_first_fit(layout) {
            Ok(ptr) => ptr.as_ptr(),
            Err(()) => ptr::null_mut(),
        }
    }
}

// A fixed-size block allocator starts with empty lists and an empty fallback,
// which is what `new` builds.
impl Default for FixedSizeBlockAllocator {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: `alloc` returns either a uniquely owned, correctly aligned block or
// null, and `dealloc` accepts exactly the pointers `alloc` handed out for the
// same layout. A block is always on exactly one free list (or live) and is only
// moved between the fallback and a list under the lock, so it is never handed out
// twice at once.
unsafe impl GlobalAlloc for Locked<FixedSizeBlockAllocator> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut allocator = self.lock();
        if let Some(index) = list_index(&layout) {
            if let Some(node) = allocator.list_heads[index].take() {
                allocator.list_heads[index] = node.next.take();
                core::ptr::from_mut(node).cast::<u8>()
            } else {
                // no block exists in the list => allocate a new one
                let block_size = BLOCK_SIZES[index];
                // only works if all block sizes are a power of two
                let block_align = block_size;
                let layout = Layout::from_size_align(block_size, block_align)
                    .expect("the block size and alignment are valid");
                allocator.fallback_alloc(layout)
            }
        } else {
            allocator.fallback_alloc(layout)
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let mut allocator = self.lock();
        if let Some(index) = list_index(&layout) {
            let new_node = ListNode {
                next: allocator.list_heads[index].take(),
            };
            // verify that the block is large and aligned enough for a node
            assert!(mem::size_of::<ListNode>() <= BLOCK_SIZES[index]);
            assert!(mem::align_of::<ListNode>() <= BLOCK_SIZES[index]);
            // The block was handed out for `BLOCK_SIZES[index]` — either popped
            // from that list or carved from the fallback with that size and
            // alignment — so it really is aligned for a `ListNode`; the raw `u8`
            // pointer hides that from clippy.
            #[allow(clippy::cast_ptr_alignment)]
            let new_node_ptr = ptr.cast::<ListNode>();
            // SAFETY: the caller hands back a block this allocator returned for
            // `layout`, and the assertions above prove it is large and aligned
            // enough to hold the node we write over it.
            unsafe {
                new_node_ptr.write(new_node);
                allocator.list_heads[index] = Some(&mut *new_node_ptr);
            }
        } else {
            let ptr = NonNull::new(ptr).expect("the freed pointer must not be null");
            // SAFETY: the block came from the fallback allocator (it is too large
            // for any size class) and is no longer used.
            unsafe {
                allocator.fallback_allocator.deallocate(ptr, layout);
            }
        }
    }
}

// A small scratch region covers the class selection, the lazy fill from the
// fallback, and the reuse the classed lists buy — all without a mapped heap.
#[cfg(test)]
mod tests {
    use super::{FixedSizeBlockAllocator, Locked, list_index};
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
    fn block_index_picks_the_smallest_fitting_class() {
        assert_eq!(list_index(&Layout::from_size_align(1, 1).unwrap()), Some(0));
        assert_eq!(list_index(&Layout::from_size_align(8, 8).unwrap()), Some(0));
        assert_eq!(list_index(&Layout::from_size_align(9, 8).unwrap()), Some(1));
        assert_eq!(
            list_index(&Layout::from_size_align(2048, 1).unwrap()),
            Some(8)
        );
        // Above the biggest class there is no list; the fallback takes it.
        assert_eq!(list_index(&Layout::from_size_align(2049, 1).unwrap()), None);
    }

    #[test_case]
    fn fixed_size_blocks_reuse_their_size_class() {
        let allocator = Locked::new(FixedSizeBlockAllocator::new());
        let (start, size) = scratch();
        // SAFETY: the scratch region is unused, and this is the only allocator
        // initialized over it in this test.
        unsafe { allocator.lock().init(start, size) };

        let layout = Layout::new::<u64>();
        // SAFETY: the allocator was just initialized over the scratch region.
        let long_lived = unsafe { allocator.alloc(layout) };
        assert!(!long_lived.is_null());

        // The first small block comes from the fallback; each freed one then goes
        // onto its size class's list, so the same block is reused over and over.
        for _ in 0..(size * 2) {
            // SAFETY: as above.
            let block = unsafe { allocator.alloc(layout) };
            assert!(
                !block.is_null(),
                "the size class should have reused its block"
            );
            // SAFETY: `block` came from this allocator and is no longer used.
            unsafe { allocator.dealloc(block, layout) };
        }

        assert!(!long_lived.is_null());
    }

    #[test_case]
    fn large_allocations_fall_back_to_the_linked_list() {
        let allocator = Locked::new(FixedSizeBlockAllocator::new());
        let (start, size) = scratch();
        // SAFETY: the scratch region is unused, and this is the only allocator
        // initialized over it in this test.
        unsafe { allocator.lock().init(start, size) };

        // Larger than every class, so this is served by the fallback allocator.
        let layout = Layout::from_size_align(3000, 8).unwrap();
        // SAFETY: the allocator was just initialized over the scratch region.
        let block = unsafe { allocator.alloc(layout) };
        assert!(
            !block.is_null(),
            "the fallback should serve large allocations"
        );
        // SAFETY: `block` came from this allocator and is no longer used.
        unsafe { allocator.dealloc(block, layout) };
    }
}
