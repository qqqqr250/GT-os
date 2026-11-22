//! Per-process x86_64 address spaces.
//!
//! Each process owns a level-4 page table (PML4). The kernel's mappings are
//! copied from the currently active table so kernel code, the heap and the
//! physical-memory window stay reachable; the user slot is left empty for the
//! process's own code and stack pages. Switching a process therefore only needs
//! a `CR3` reload, and no program can see another program's memory.
//!
//! # Lifetime
//!
//! Frames come from the physical frame pool built from the bootloader memory
//! map and are reference-counted globally. Dropping an address space releases
//! its references; the last reference returns the frame to the pool. `fork`
//! shares frames copy-on-write, so each process holds a reference until it
//! writes and makes the frame private.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

use x86_64::VirtAddr;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::mapper::MapperFlush;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
};

const PAGE_BYTES: u64 = 4096;
const PML4_ENTRIES: usize = 512;

/// The bootloader's physical-memory window offset, published by [`init`].
static OFFSET: AtomicU64 = AtomicU64::new(0);

/// Reference counts for every frame handed out by the physical frame pool.
///
/// The key is the frame's physical address. The count tells us when the last
/// owner is gone and the frame can be returned to the pool.
static FRAME_REFS: Mutex<BTreeMap<u64, usize>> = Mutex::new(BTreeMap::new());

/// Remembers the physical-memory window offset.
pub fn init(offset: u64) {
    OFFSET.store(offset, Ordering::Relaxed);
}

/// The physical-memory window offset as a virtual address.
fn offset_virt() -> VirtAddr {
    VirtAddr::new(OFFSET.load(Ordering::Relaxed))
}

/// Pointer to a physical frame through the physical-memory window.
fn at<T>(frame: PhysFrame) -> *mut T {
    (offset_virt() + frame.start_address().as_u64()).as_mut_ptr()
}

/// Allocates one zeroed 4 KiB physical frame from the bootloader's frame pool.
///
/// Returns `None` if the pool is not initialized or exhausted.
pub fn allocate_frame() -> Option<PhysFrame> {
    let frame = crate::memory::allocate_frame()?;
    // SAFETY: the pool owns this frame exclusively and the physical-memory
    // window maps all RAM, so the frame can be zeroed through the window.
    unsafe {
        core::ptr::write_bytes(at::<u8>(frame), 0, PAGE_BYTES as usize);
    }
    FRAME_REFS
        .lock()
        .entry(frame.start_address().as_u64())
        .and_modify(|count| *count += 1)
        .or_insert(1);
    Some(frame)
}

/// Adds another owner reference to a shared frame.
fn retain_frame(frame: PhysFrame) {
    FRAME_REFS
        .lock()
        .entry(frame.start_address().as_u64())
        .and_modify(|count| *count += 1)
        .or_insert(1);
}

/// Releases one owner reference, returning the frame to the pool at zero.
fn release_frame(frame: PhysFrame) {
    let address = frame.start_address().as_u64();
    let free = {
        let mut refs = FRAME_REFS.lock();
        match refs.get_mut(&address) {
            Some(count) if *count > 1 => {
                *count -= 1;
                false
            }
            Some(_) => {
                refs.remove(&address);
                true
            }
            None => false,
        }
    };
    if free {
        // SAFETY: the reference count proved this layer owns the frame
        // exclusively, so no page table or live allocation still refers to it.
        unsafe { crate::memory::deallocate_frame(frame) };
    }
}

/// Adds one region-owned shared-memory frame reference.
pub(crate) fn retain_shared_frame(frame: PhysFrame) {
    retain_frame(frame);
}

/// Releases one region-owned shared-memory frame reference.
pub(crate) fn release_shared_owner(frame: PhysFrame) {
    release_frame(frame);
}

/// Kernel virtual address backing a shared-memory frame.
pub(crate) fn frame_window_address(frame: PhysFrame) -> u64 {
    (offset_virt() + frame.start_address().as_u64()).as_u64()
}

/// A frame allocator that records intermediate page-table frames for reclamation.
struct RecordingFrames {
    frames: Vec<PhysFrame>,
}

impl RecordingFrames {
    fn new() -> Self {
        Self { frames: Vec::new() }
    }
}

// SAFETY: every call allocates a fresh, zeroed frame from the heap and records
// it so its owner can release it later.
unsafe impl FrameAllocator<Size4KiB> for RecordingFrames {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        let frame = allocate_frame()?;
        self.frames.push(frame);
        Some(frame)
    }
}

/// An owned level-4 page table for one process.
pub struct AddressSpace {
    pml4: PhysFrame,
    /// Data frames mapped for this process, one reference per live mapping.
    user_frames: Vec<PhysFrame>,
    /// Intermediate page-table frames allocated for the user slot. Kernel
    /// page-table frames are copied by reference and must never be released.
    page_table_frames: Vec<PhysFrame>,
    /// Page-aligned user virtual addresses already mapped in this space. This
    /// makes mapping an ELF segment that shares a page with another segment safe.
    mapped: BTreeMap<u64, PhysFrame>,
    /// The flags used for each mapped page, kept so `fork` can reproduce them.
    page_flags: BTreeMap<u64, PageTableFlags>,
    /// Pages shared after `fork`: virtual page -> original frame and the flags
    /// it had before the page was made read-only for copy-on-write.
    cow_pages: BTreeMap<u64, (PhysFrame, PageTableFlags)>,
    /// Explicit shared-memory mappings: region, attaching PID, virtual page and
    /// frame. These are deliberately excluded from `mapped` so `fork` does not
    /// turn a genuinely shared region into a copy-on-write page.
    shared_mappings: Vec<(u64, u64, u64, PhysFrame)>,
}

impl AddressSpace {
    /// Creates a new address space with the current kernel mappings and an empty
    /// user slot.
    pub fn new() -> Result<Self, &'static str> {
        if OFFSET.load(Ordering::Relaxed) == 0 {
            return Err("address spaces are not initialized");
        }
        let (current_pml4, _) = Cr3::read();
        // SAFETY: the physical-memory window maps every frame, and the active
        // PML4 lives for the whole kernel lifetime.
        let current = unsafe { &*at::<PageTable>(current_pml4) };
        let new_frame = allocate_frame().ok_or("cannot allocate PML4 frame")?;
        // SAFETY: `allocate_frame` returned an owned, zeroed frame reachable
        // through the physical-memory window.
        let new_table = unsafe { &mut *at::<PageTable>(new_frame) };
        new_table.zero();

        // Copy every kernel mapping except the user slot. The bootloader leaves
        // that slot unused, so the new process starts with no user pages while
        // all kernel identity and physical-memory windows stay intact.
        let user_index = usize::from(VirtAddr::new(crate::usermode::USER_BASE).p4_index());
        for index in 0..PML4_ENTRIES {
            if index != user_index {
                new_table[index] = current[index].clone();
            }
        }

        Ok(Self {
            pml4: new_frame,
            user_frames: Vec::new(),
            page_table_frames: Vec::new(),
            mapped: BTreeMap::new(),
            page_flags: BTreeMap::new(),
            cow_pages: BTreeMap::new(),
            shared_mappings: Vec::new(),
        })
    }

    /// The physical frame holding this address space's PML4.
    pub fn pml4_frame(&self) -> PhysFrame {
        self.pml4
    }

    /// Maps an owned frame at `page_start` with `flags`.
    ///
    /// The caller is responsible for recording the frame in `user_frames` and
    /// `mapped` so it stays alive and is not mapped twice.
    fn map_frame(
        &mut self,
        page_start: u64,
        frame: PhysFrame,
        flags: PageTableFlags,
    ) -> Result<(), &'static str> {
        // SAFETY: the PML4 frame is owned by this address space and mapped in the
        // physical-memory window; no other mapper aliases it while this call
        // runs.
        let table = unsafe { &mut *at::<PageTable>(self.pml4) };
        // SAFETY: the offset maps all physical memory and this table is the only
        // mutable reference to this PML4.
        let mut mapper = unsafe { OffsetPageTable::new(table, offset_virt()) };
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(page_start));
        let mut frames = RecordingFrames::new();
        // SAFETY: `frame` is freshly allocated or explicitly retained, and
        // `page_start` was not mapped in this table yet, as recorded by
        // `self.mapped`.
        let result = unsafe {
            mapper
                .map_to(page, frame, flags, &mut frames)
                .map_err(|_| "map_to failed")
                .map(MapperFlush::flush)
        };
        // Keep even the frames from a failed mapping: the paging API does not
        // return them, and this address space is their only owner.
        self.page_table_frames.extend(frames.frames);
        result
    }

    /// Changes the flags of an already-mapped page and flushes the TLB entry.
    fn set_page_flags(
        &mut self,
        page_start: u64,
        flags: PageTableFlags,
    ) -> Result<(), &'static str> {
        // SAFETY: the PML4 frame is owned by this address space and reachable
        // through the physical-memory window.
        let table = unsafe { &mut *at::<PageTable>(self.pml4) };
        // SAFETY: this is the only mutable mapper for this address space.
        let mut mapper = unsafe { OffsetPageTable::new(table, offset_virt()) };
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(page_start));
        // SAFETY: `page_start` is recorded in `self.mapped`, so the mapping
        // exists. Making a writable page read-only only removes permission.
        unsafe {
            mapper
                .update_flags(page, flags)
                .map_err(|_| "update_flags failed")?
                .flush();
        }
        Ok(())
    }

    /// Replaces the frame mapped at `page_start` with `frame`.
    fn remap_frame(
        &mut self,
        page_start: u64,
        frame: PhysFrame,
        flags: PageTableFlags,
    ) -> Result<(), &'static str> {
        // SAFETY: the PML4 frame is owned by this address space and reachable
        // through the physical-memory window.
        let table = unsafe { &mut *at::<PageTable>(self.pml4) };
        // SAFETY: this is the only mutable mapper for this address space.
        let mut mapper = unsafe { OffsetPageTable::new(table, offset_virt()) };
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(page_start));
        let (_, unmap_flush) = mapper.unmap(page).map_err(|_| "unmap failed")?;
        unmap_flush.flush();
        let mut frames = RecordingFrames::new();
        // SAFETY: the page was just unmapped and `frame` is freshly allocated.
        let result = unsafe {
            mapper
                .map_to(page, frame, flags, &mut frames)
                .map_err(|_| "map_to failed")
                .map(MapperFlush::flush)
        };
        self.page_table_frames.extend(frames.frames);
        result
    }

    /// Removes one user page mapping without releasing its data frame.
    fn unmap_page(&mut self, page_start: u64) -> Result<(), &'static str> {
        let table = unsafe { &mut *at::<PageTable>(self.pml4) };
        let mut mapper = unsafe { OffsetPageTable::new(table, offset_virt()) };
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(page_start));
        let (_, flush) = mapper.unmap(page).map_err(|_| "unmap failed")?;
        flush.flush();
        Ok(())
    }

    /// Maps one user page to a fresh frame and returns the frame's virtual
    /// address so the caller can initialise its contents.
    pub fn map_user_page(
        &mut self,
        virtual_address: u64,
        writable: bool,
        executable: bool,
    ) -> Result<u64, &'static str> {
        let page_start = virtual_address & !(PAGE_BYTES - 1);
        if let Some(frame) = self.mapped.get(&page_start) {
            return Ok((offset_virt() + frame.start_address().as_u64()).as_u64());
        }

        let frame = allocate_frame().ok_or("cannot allocate user frame")?;
        let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        if writable {
            flags |= PageTableFlags::WRITABLE;
        }
        if !executable {
            flags |= PageTableFlags::NO_EXECUTE;
        }
        if let Err(error) = self.map_frame(page_start, frame, flags) {
            release_frame(frame);
            return Err(error);
        }
        self.user_frames.push(frame);
        self.mapped.insert(page_start, frame);
        self.page_flags.insert(page_start, flags);
        Ok((offset_virt() + frame.start_address().as_u64()).as_u64())
    }

    /// Returns the physical-memory-window address backing a mapped user page.
    ///
    /// This is used by the ELF loader to patch relocations after segments are
    /// copied; it never creates a mapping for an address the ELF did not load.
    #[must_use]
    pub fn mapped_frame_address(&self, virtual_address: u64) -> Option<u64> {
        let page_start = virtual_address & !(PAGE_BYTES - 1);
        self.mapped
            .get(&page_start)
            .map(|frame| (offset_virt() + frame.start_address().as_u64()).as_u64())
    }

    /// Maps a shared-memory region into this address space.
    ///
    /// The caller owns one reference per frame in the region; every mapping adds
    /// another. Mappings are tracked separately from [`Self::mapped`] so `fork`
    /// never copy-on-writes them.
    pub fn map_shared(
        &mut self,
        pid: u64,
        region_id: u64,
        base: u64,
        frames: &[PhysFrame],
    ) -> Result<(), &'static str> {
        for (index, _) in frames.iter().enumerate() {
            let page = base
                .checked_add(index as u64 * PAGE_BYTES)
                .ok_or("shared region overflow")?;
            if self.mapped.contains_key(&page)
                || self
                    .shared_mappings
                    .iter()
                    .any(|&(_, _, mapped_page, _)| mapped_page == page)
            {
                return Err("shared region overlaps an existing mapping");
            }
        }

        let flags = PageTableFlags::PRESENT
            | PageTableFlags::USER_ACCESSIBLE
            | PageTableFlags::WRITABLE
            | PageTableFlags::NO_EXECUTE;
        let mut pending = Vec::new();
        for (index, &frame) in frames.iter().enumerate() {
            let page = base + index as u64 * PAGE_BYTES;
            retain_frame(frame);
            match self.map_frame(page, frame, flags) {
                Ok(()) => pending.push((region_id, pid, page, frame)),
                Err(error) => {
                    for (_, _, mapped_page, mapped_frame) in pending.drain(..) {
                        let _ = self.unmap_page(mapped_page);
                        release_frame(mapped_frame);
                    }
                    release_frame(frame);
                    return Err(error);
                }
            }
        }
        self.shared_mappings.extend(pending);
        Ok(())
    }

    /// Removes every mapping of one shared-memory region.
    ///
    /// Returns the number of pages unmapped. Each released mapping decrements
    /// the region's mapping count through [`crate::shm::on_mapping_released`].
    pub fn unmap_shared(&mut self, region_id: u64) -> usize {
        let targets: Vec<_> = self
            .shared_mappings
            .iter()
            .copied()
            .filter(|&(id, _, _, _)| id == region_id)
            .collect();
        for &(id, pid, page, frame) in &targets {
            let _ = self.unmap_page(page);
            release_frame(frame);
            crate::shm::on_mapping_released(id, pid);
        }
        self.shared_mappings
            .retain(|&(id, _, _, _)| id != region_id);
        targets.len()
    }

    /// Clones this address space for `fork` using copy-on-write.
    ///
    /// Both processes map the same data frames with `WRITABLE` cleared. A later
    /// write fault allocates a private copy for the writing process. Read-only
    /// pages are shared without a COW entry: writing to code stays illegal.
    pub fn fork(&mut self) -> Result<(Self, u64), &'static str> {
        let mut child = Self::new()?;
        // Collect first: marking the parent's pages read-only mutates `self`
        // while the old loop would still be borrowing `self.mapped`.
        let pages: alloc::vec::Vec<_> = self
            .mapped
            .iter()
            .map(|(&page_start, &frame)| (page_start, frame))
            .collect();
        for (page_start, frame) in pages {
            let flags = self
                .page_flags
                .get(&page_start)
                .copied()
                .unwrap_or_else(|| {
                    PageTableFlags::PRESENT
                        | PageTableFlags::USER_ACCESSIBLE
                        | PageTableFlags::WRITABLE
                });
            let original_flags = self
                .cow_pages
                .get(&page_start)
                .map_or(flags, |&(_, original)| original);

            let child_flags = if original_flags.contains(PageTableFlags::WRITABLE) {
                if flags.contains(PageTableFlags::WRITABLE) {
                    let shared_flags = original_flags & !PageTableFlags::WRITABLE;
                    self.set_page_flags(page_start, shared_flags)?;
                    self.page_flags.insert(page_start, shared_flags);
                }
                self.cow_pages.insert(page_start, (frame, original_flags));
                child.cow_pages.insert(page_start, (frame, original_flags));
                original_flags & !PageTableFlags::WRITABLE
            } else {
                flags
            };

            // The child now owns a second reference to the shared frame. Push it
            // before mapping so a failed map leaves the child owning it and its
            // `Drop` releases the reference again.
            retain_frame(frame);
            child.user_frames.push(frame);
            child.map_frame(page_start, frame, child_flags)?;
            child.mapped.insert(page_start, frame);
            child.page_flags.insert(page_start, child_flags);
        }
        let cr3 = child.pml4.start_address().as_u64();
        Ok((child, cr3))
    }

    /// Resolves a write fault on a copy-on-write page.
    ///
    /// Returns `true` when the fault was a COW page in this address space. The
    /// original frame stays mapped read-only for the other process until it
    /// writes too; this process drops its reference to the original.
    pub fn handle_cow_write(&mut self, virtual_address: u64) -> bool {
        let page_start = virtual_address & !(PAGE_BYTES - 1);
        let Some(&(old_frame, original_flags)) = self.cow_pages.get(&page_start) else {
            return false;
        };
        let Some(new_frame) = allocate_frame() else {
            return false;
        };

        // SAFETY: both frames are owned by live address spaces and mapped
        // through the physical-memory window; they are distinct allocations.
        unsafe {
            core::ptr::copy_nonoverlapping(
                (offset_virt() + old_frame.start_address().as_u64()).as_ptr::<u8>(),
                (offset_virt() + new_frame.start_address().as_u64()).as_mut_ptr::<u8>(),
                PAGE_BYTES as usize,
            );
        }
        if self
            .remap_frame(page_start, new_frame, original_flags)
            .is_err()
        {
            release_frame(new_frame);
            return false;
        }
        self.user_frames.push(new_frame);
        self.mapped.insert(page_start, new_frame);
        self.page_flags.insert(page_start, original_flags);
        self.cow_pages.remove(&page_start);
        // This space no longer maps the original frame. Drop exactly one of its
        // references; the other process (if any) keeps the frame alive.
        if let Some(index) = self
            .user_frames
            .iter()
            .position(|frame| frame.start_address() == old_frame.start_address())
        {
            self.user_frames.remove(index);
        }
        release_frame(old_frame);
        true
    }

    /// Loads this address space into `CR3`.
    ///
    /// # Safety
    ///
    /// The caller must ensure the kernel mappings are present in this table and
    /// that no code relying on the previous address space runs until it is
    /// restored.
    pub unsafe fn switch_to(&self) {
        // SAFETY: forwarded to the caller. Reloading CR3 flushes all non-global
        // TLB entries, so the new mappings take effect immediately.
        unsafe { Cr3::write(self.pml4, Cr3Flags::empty()) };
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        for (region_id, pid, _, frame) in self.shared_mappings.drain(..) {
            release_frame(frame);
            crate::shm::on_mapping_released(region_id, pid);
        }
        for frame in self.user_frames.drain(..) {
            release_frame(frame);
        }
        for frame in self.page_table_frames.drain(..) {
            release_frame(frame);
        }
        release_frame(self.pml4);
    }
}

/// Reloads a saved `CR3`, restoring the caller's address space.
///
/// # Safety
///
/// `frame` must be a valid PML4 with the current kernel mappings.
pub unsafe fn restore(frame: PhysFrame, flags: Cr3Flags) {
    // SAFETY: forwarded to the caller.
    unsafe { Cr3::write(frame, flags) };
}
