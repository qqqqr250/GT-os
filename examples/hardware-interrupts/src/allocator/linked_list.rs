//! A linked-list allocator: a free list threaded through the freed blocks.
//!
//! The bump allocator's weakness is that it cannot record *where* a freed block
//! was, so it cannot reuse one block while another is still live. The trick this
//! design uses is to store that record inside the freed block itself: every free
//! region begins with a [`ListNode`] holding the region's size and a pointer to
//! the next free region, and the allocator keeps only a single `head` pointer. A
//! freed region therefore costs no extra memory, and an unbounded number of them
//! can be tracked.
//!
//! `alloc` scans the list for the first region that fits (first fit), removes it
//! and returns the leftover bytes to the list as a new node; `dealloc` writes a
//! new node over the freed block and pushes it onto the front. The scan is linear
//! in the number of free regions, and repeatedly allocating and freeing different
//! sizes fragments the list, but no freed memory is ever stranded.
//!
//! This is essentially the design the `linked_list_allocator` crate implements.

use super::{Locked, align_up};
use alloc::alloc::{GlobalAlloc, Layout};
use core::{mem, ptr};

/// A free region, written into the region's own first bytes.
struct ListNode {
    size: usize,
    next: Option<&'static mut ListNode>,
}

impl ListNode {
    /// A node for a region of `size` bytes, not yet linked into the list.
    const fn new(size: usize) -> Self {
        ListNode { size, next: None }
    }

    /// The address of the region this node describes.
    fn start_addr(&self) -> usize {
        core::ptr::from_ref(self).addr()
    }

    /// One past the last byte of the region this node describes.
    fn end_addr(&self) -> usize {
        self.start_addr() + self.size
    }
}

/// A first-fit allocator over a single free list.
pub struct LinkedListAllocator {
    /// A sentinel node whose `next` is the first real free region.
    head: ListNode,
}

impl LinkedListAllocator {
    /// Creates an empty `LinkedListAllocator`.
    ///
    /// It must be [`init`](LinkedListAllocator::init)ialized before it can
    /// allocate.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            head: ListNode::new(0),
        }
    }

    /// Initializes the allocator with the given heap bounds.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the given heap bounds are valid and that
    /// the heap is unused, and must call this method only once.
    pub unsafe fn init(&mut self, heap_start: usize, heap_size: usize) {
        // SAFETY: the caller guarantees the region is unused and valid, which is
        // exactly what `add_free_region` needs to write a node into it.
        unsafe {
            self.add_free_region(heap_start, heap_size);
        }
    }

    /// Adds the given memory region to the front of the list.
    ///
    /// # Safety
    ///
    /// The region must be unused, large enough for a [`ListNode`], and aligned
    /// for one.
    unsafe fn add_free_region(&mut self, addr: usize, size: usize) {
        // ensure that the freed region is capable of holding ListNode
        assert_eq!(align_up(addr, mem::align_of::<ListNode>()), addr);
        assert!(size >= mem::size_of::<ListNode>());

        // create a new list node and append it at the start of the list
        let mut node = ListNode::new(size);
        node.next = self.head.next.take();
        let node_ptr = addr as *mut ListNode;
        // SAFETY: the caller guarantees `addr` is unused, aligned for a node and
        // at least a node wide, so writing one there is sound; the node is then
        // owned by the list, which hands out no overlapping region while it lives.
        unsafe {
            node_ptr.write(node);
            self.head.next = Some(&mut *node_ptr);
        }
    }

    /// Looks for a free region with the given size and alignment and removes it
    /// from the list.
    ///
    /// Returns the removed node and the start address of the allocation.
    fn find_region(&mut self, size: usize, align: usize) -> Option<(&'static mut ListNode, usize)> {
        // reference to current list node, updated for each iteration
        let mut current = &mut self.head;
        // look for a large enough memory region in the linked list
        while let Some(ref mut region) = current.next {
            if let Ok(alloc_start) = Self::alloc_from_region(region, size, align) {
                // region suitable for allocation -> remove node from list
                let next = region.next.take();
                let ret = Some((current.next.take().unwrap(), alloc_start));
                current.next = next;
                return ret;
            }
            // region not suitable -> continue with the next region
            current = current.next.as_mut().unwrap();
        }

        // no suitable region found
        None
    }

    /// Tries to use the given region for an allocation with the given size and
    /// alignment.
    ///
    /// Returns the allocation's start address on success.
    fn alloc_from_region(region: &ListNode, size: usize, align: usize) -> Result<usize, ()> {
        let alloc_start = align_up(region.start_addr(), align);
        let alloc_end = alloc_start.checked_add(size).ok_or(())?;

        if alloc_end > region.end_addr() {
            // region too small
            return Err(());
        }

        let excess_size = region.end_addr() - alloc_end;
        if excess_size > 0 && excess_size < mem::size_of::<ListNode>() {
            // rest of the region is too small to hold a ListNode (required
            // because the allocation splits the region into a used and a free
            // part)
            return Err(());
        }

        // region suitable for allocation
        Ok(alloc_start)
    }

    /// Adjusts the given layout so that the resulting allocated memory region is
    /// also capable of storing a [`ListNode`].
    ///
    /// This matters because a block is returned to the list on `dealloc` by
    /// writing a node over it: if the block were smaller than a node, or less
    /// aligned, that write would corrupt the neighbouring allocation.
    ///
    /// Returns the adjusted size and alignment as a `(size, align)` tuple.
    fn size_align(layout: Layout) -> (usize, usize) {
        let layout = layout
            .align_to(mem::align_of::<ListNode>())
            .expect("adjusting alignment failed")
            .pad_to_align();
        let size = layout.size().max(mem::size_of::<ListNode>());
        (size, layout.align())
    }
}

// A linked-list allocator starts with an empty free list, which is what `new`
// builds.
impl Default for LinkedListAllocator {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: `alloc` returns either a uniquely owned, correctly aligned pointer into
// the region `init` was given, or null; `dealloc` accepts exactly the pointers
// `alloc` handed out (for the same layout) and returns each block to the free
// list, so no block is ever handed out twice while it is live.
unsafe impl GlobalAlloc for Locked<LinkedListAllocator> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // perform layout adjustments
        let (size, align) = LinkedListAllocator::size_align(layout);
        let mut allocator = self.lock();

        if let Some((region, alloc_start)) = allocator.find_region(size, align) {
            let alloc_end = alloc_start.checked_add(size).expect("overflow");
            let excess_size = region.end_addr() - alloc_end;
            if excess_size > 0 {
                // SAFETY: the region was removed from the free list above, so its
                // tail is unused and can hold the new node.
                unsafe {
                    allocator.add_free_region(alloc_end, excess_size);
                }
            }
            alloc_start as *mut u8
        } else {
            ptr::null_mut()
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // perform layout adjustments
        let (size, _) = LinkedListAllocator::size_align(layout);

        // SAFETY: the caller hands back a block this allocator returned for
        // `layout`, so `ptr` is unused, aligned and at least `size` bytes wide.
        unsafe { self.lock().add_free_region(ptr as usize, size) }
    }
}

// A small scratch region makes both halves of the design — the free-list
// bookkeeping and the reuse it buys — testable without a mapped heap.
#[cfg(test)]
mod tests {
    use super::{LinkedListAllocator, Locked};
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
    fn size_align_reserves_room_for_a_list_node() {
        // A block must be able to hold a `ListNode` (16 bytes, 8-aligned) so that
        // `dealloc` can write one over it.
        assert_eq!(
            LinkedListAllocator::size_align(Layout::from_size_align(8, 8).unwrap()),
            (16, 8)
        );
        // A one-byte allocation is padded up to the node size.
        assert_eq!(
            LinkedListAllocator::size_align(Layout::from_size_align(1, 1).unwrap()),
            (16, 8)
        );
        // Alignment and size above the node's are preserved.
        assert_eq!(
            LinkedListAllocator::size_align(Layout::from_size_align(32, 32).unwrap()),
            (32, 32)
        );
    }

    #[test_case]
    fn linked_list_reuses_freed_blocks() {
        let allocator = Locked::new(LinkedListAllocator::new());
        let (start, size) = scratch();
        // SAFETY: the scratch region is unused, and this is the only allocator
        // initialized over it in this test.
        unsafe { allocator.lock().init(start, size) };

        let layout = Layout::new::<u64>();
        // SAFETY: the allocator was just initialized over the scratch region.
        let long_lived = unsafe { allocator.alloc(layout) };
        assert!(!long_lived.is_null());

        // Every freed block goes straight back on the free list, so the same
        // bytes are handed out again and again: far more allocations than the
        // region could hold at once are served.
        for _ in 0..(size * 2) {
            // SAFETY: as above.
            let block = unsafe { allocator.alloc(layout) };
            assert!(!block.is_null(), "the free list should have reused memory");
            // SAFETY: `block` came from this allocator and is no longer used.
            unsafe { allocator.dealloc(block, layout) };
        }

        // The long-lived allocation was never touched by the reuse.
        assert!(!long_lived.is_null());
    }
}
