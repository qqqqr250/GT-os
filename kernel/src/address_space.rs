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

use alloc::vec::Vec;
use core::cell::UnsafeCell;
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

/// Looks up a page-keyed entry in one of the address space's linear tables.
fn page_entry<T: Copy>(entries: &[(u64, T)], page: u64) -> Option<T> {
    entries
        .iter()
        .find(|(key, _)| *key == page)
        .map(|(_, value)| *value)
}

/// Inserts or replaces a page-keyed entry in one of the linear tables.
fn set_page_entry<T: Copy>(entries: &mut Vec<(u64, T)>, page: u64, value: T) {
    if let Some(entry) = entries.iter_mut().find(|(key, _)| *key == page) {
        entry.1 = value;
    } else {
        entries.push((page, value));
    }
}

/// Removes a page-keyed entry from one of the linear tables.
fn take_page_entry<T>(entries: &mut Vec<(u64, T)>, page: u64) -> Option<T> {
    let index = entries.iter().position(|(key, _)| *key == page)?;
    Some(entries.remove(index).1)
}

/// The bootloader's physical-memory window offset, published by [`init`].
static OFFSET: AtomicU64 = AtomicU64::new(0);

/// Maximum number of simultaneously referenced physical frames.
///
/// This only bounds *tracked* frames, not the physical pool. A kernel with a few
/// processes and some shared mappings stays far below it; running out is a bug
/// and panics instead of silently losing a reference.
const MAX_FRAME_REFS: usize = 16_384;

/// Static storage backing [`FRAME_REFS`].
///
/// Keeping the arrays in static storage, rather than inside a heap object, makes
/// frame bookkeeping allocation-free. Frame release runs while address spaces
/// are torn down, often from syscall paths that also hold scheduler locks; a
/// `BTreeMap` there would allocate and free nodes on a path where an allocator
/// bug could corrupt unrelated kernel state.
struct FrameStore {
    keys: UnsafeCell<[u64; MAX_FRAME_REFS]>,
    counts: UnsafeCell<[usize; MAX_FRAME_REFS]>,
}

// SAFETY: every access is made through the `FRAME_REFS` mutex, so only one
// context can hold a mutable view of either array at a time.
unsafe impl Sync for FrameStore {}

static FRAME_STORE: FrameStore = FrameStore {
    keys: UnsafeCell::new([0; MAX_FRAME_REFS]),
    counts: UnsafeCell::new([0; MAX_FRAME_REFS]),
};

struct FrameRefs {
    len: usize,
}

impl FrameRefs {
    const fn new() -> Self {
        Self { len: 0 }
    }

    /// Returns mutable views of both arrays. The caller must hold `FRAME_REFS`.
    fn arrays() -> (
        &'static mut [u64; MAX_FRAME_REFS],
        &'static mut [usize; MAX_FRAME_REFS],
    ) {
        // SAFETY: `FrameRefs` is only reachable through the `FRAME_REFS` mutex,
        // which excludes concurrent access to the static cells.
        unsafe { (&mut *FRAME_STORE.keys.get(), &mut *FRAME_STORE.counts.get()) }
    }

    fn retain(&mut self, address: u64) {
        let len = self.len;
        let (keys, counts) = Self::arrays();
        if let Some(index) = keys[..len].iter().position(|&key| key == address) {
            counts[index] += 1;
            return;
        }
        assert!(len < MAX_FRAME_REFS, "frame reference table is full");
        keys[len] = address;
        counts[len] = 1;
        self.len = len + 1;
    }

    /// Decrements one reference, returning `true` when the frame became free.
    fn release(&mut self, address: u64) -> bool {
        let len = self.len;
        let (keys, counts) = Self::arrays();
        let Some(index) = keys[..len].iter().position(|&key| key == address) else {
            return false;
        };
        if counts[index] > 1 {
            counts[index] -= 1;
            return false;
        }
        let last = len - 1;
        keys[index] = keys[last];
        counts[index] = counts[last];
        self.len = last;
        true
    }
}

static FRAME_REFS: Mutex<FrameRefs> = Mutex::new(FrameRefs::new());

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
    FRAME_REFS.lock().retain(frame.start_address().as_u64());
    Some(frame)
}

/// Adds another owner reference to a shared frame.
fn retain_frame(frame: PhysFrame) {
    FRAME_REFS.lock().retain(frame.start_address().as_u64());
}

/// Releases one owner reference, returning the frame to the pool at zero.
fn release_frame(frame: PhysFrame) {
    let free = FRAME_REFS.lock().release(frame.start_address().as_u64());
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
    /// Linear tables keep page bookkeeping allocation-light and deterministic.
    mapped: Vec<(u64, PhysFrame)>,
    /// The flags used for each mapped page, kept so `fork` can reproduce them.
    page_flags: Vec<(u64, PageTableFlags)>,
    /// Pages shared after `fork`: virtual page -> original frame and the flags
    /// it had before the page was made read-only for copy-on-write.
    cow_pages: Vec<(u64, (PhysFrame, PageTableFlags))>,
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
            mapped: Vec::new(),
            page_flags: Vec::new(),
            cow_pages: Vec::new(),
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
        if let Some(frame) = page_entry(&self.mapped, page_start) {
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
        set_page_entry(&mut self.mapped, page_start, frame);
        set_page_entry(&mut self.page_flags, page_start, flags);
        Ok((offset_virt() + frame.start_address().as_u64()).as_u64())
    }

    /// Returns the physical-memory-window address backing a mapped user page.
    ///
    /// This is used by the ELF loader to patch relocations after segments are
    /// copied; it never creates a mapping for an address the ELF did not load.
    #[must_use]
    pub fn mapped_frame_address(&self, virtual_address: u64) -> Option<u64> {
        let page_start = virtual_address & !(PAGE_BYTES - 1);
        page_entry(&self.mapped, page_start)
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
            if page_entry(&self.mapped, page).is_some()
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
        let pages = self.mapped.clone();
        for (page_start, frame) in pages {
            let flags = page_entry(&self.page_flags, page_start).unwrap_or_else(|| {
                PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE | PageTableFlags::WRITABLE
            });
            let original_flags =
                page_entry(&self.cow_pages, page_start).map_or(flags, |(_, original)| original);

            let child_flags = if original_flags.contains(PageTableFlags::WRITABLE) {
                if flags.contains(PageTableFlags::WRITABLE) {
                    let shared_flags = original_flags & !PageTableFlags::WRITABLE;
                    self.set_page_flags(page_start, shared_flags)?;
                    set_page_entry(&mut self.page_flags, page_start, shared_flags);
                }
                set_page_entry(&mut self.cow_pages, page_start, (frame, original_flags));
                set_page_entry(&mut child.cow_pages, page_start, (frame, original_flags));
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
            set_page_entry(&mut child.mapped, page_start, frame);
            set_page_entry(&mut child.page_flags, page_start, child_flags);
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
        let Some((old_frame, original_flags)) = page_entry(&self.cow_pages, page_start) else {
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
        set_page_entry(&mut self.mapped, page_start, new_frame);
        set_page_entry(&mut self.page_flags, page_start, original_flags);
        let _ = take_page_entry(&mut self.cow_pages, page_start);
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
