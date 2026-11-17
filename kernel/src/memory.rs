//! Heap allocator backed by the largest usable region reported by the
//! bootloader, plus a paging layer over the active page table.
//!
//! The heap ([`init`], [`info`]) is what `kernel_main` uses. Its global allocator
//! is post-11's [`FixedSizeBlockAllocator`] (see [`crate::allocator`]): a free list
//! per size class with a `linked_list_allocator` fallback, so a long-lived
//! allocation never strands the rest of the heap the way a bump allocator would.
//! The paging layer
//! ([`init_mapper`], [`with_mapper`], [`translate_addr`], [`init_frame_allocator`],
//! [`map_page`]) is the post-08 surface: it reads `CR3`, walks the four-level
//! table, and maps pages through an
//! [`OffsetPageTable`](x86_64::structures::paging::OffsetPageTable) with a frame
//! allocator over the bootloader's memory map.
//!
//! # One mapper at a time
//!
//! An [`OffsetPageTable`](x86_64::structures::paging::OffsetPageTable) borrows the
//! active level-4 table *mutably*, and `usermode` builds its own mapper for the
//! ring-3 window ([`crate::usermode`]). Two live mappers over the same table would
//! alias `&mut` references, which is undefined behaviour. So the mapper here is
//! created only by [`init_mapper`], which is called from the in-QEMU tests through
//! [`crate::testing::init_memory`] — never from `kernel_main`, where `usermode`
//! owns the mapping work. The tests run the boot path without `usermode`, so no
//! second mapper is ever live.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::allocator::Locked;
use crate::allocator::fixed_size_block::FixedSizeBlockAllocator;
use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
use spin::Mutex;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::page_table::FrameError;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
    Translate,
};
use x86_64::{PhysAddr, VirtAddr};

/// Upper bound for the kernel heap, regardless of how much RAM the machine has.
const MAX_HEAP_BYTES: u64 = 32 * 1024 * 1024;
const MIN_REGION_BYTES: u64 = 2 * 1024 * 1024;
const PAGE_SIZE: u64 = 4096;

#[global_allocator]
static ALLOCATOR: Locked<FixedSizeBlockAllocator> = Locked::new(FixedSizeBlockAllocator::new());

/// The name of the allocator design [`ALLOCATOR`] implements, exposed to the
/// in-QEMU tests through [`crate::testing::allocator_design`].
pub const ALLOCATOR_DESIGN: &str = "fixed-size block";

static HEAP_START: AtomicU64 = AtomicU64::new(0);
static HEAP_SIZE: AtomicU64 = AtomicU64::new(0);

fn overlaps(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start < b_end && b_start < a_end
}

/// Initializes the kernel heap.
///
/// `kernel` is the `(physical address, length)` of the loaded kernel image; the
/// heap is taken from the *top* of the largest usable region so that it cannot
/// collide with the kernel or the bootloader's data structures.
///
/// `physical_memory_offset` is the bootloader's physical-memory mapping offset
/// (`BootInfo::physical_memory_offset`). The bootloader does not identity-map
/// RAM in the kernel's page table, so usable regions must be reached through
/// `offset + physical_address`.
pub fn init(
    regions: &MemoryRegions,
    kernel: (u64, u64),
    physical_memory_offset: u64,
) -> Result<(), &'static str> {
    let (kernel_start, kernel_len) = kernel;
    let kernel_end = kernel_start.saturating_add(kernel_len);

    let mut best: Option<(u64, u64)> = None;
    for region in regions.iter() {
        if region.kind != MemoryRegionKind::Usable {
            continue;
        }
        let size = region.end.saturating_sub(region.start);
        if size < MIN_REGION_BYTES {
            continue;
        }
        if overlaps(region.start, region.end, kernel_start, kernel_end) {
            continue;
        }
        if best.is_none_or(|(start, end)| size > end - start) {
            best = Some((region.start, region.end));
        }
    }

    let (_, end) = best.ok_or("no usable memory region for the heap")?;
    let start = end.saturating_sub(MAX_HEAP_BYTES) & !(PAGE_SIZE - 1);
    let size = (end - start) & !(PAGE_SIZE - 1);
    if size < PAGE_SIZE {
        return Err("usable memory region is too small");
    }

    let heap_start = physical_memory_offset
        .checked_add(start)
        .ok_or("heap address overflows the virtual address space")?;

    let heap_bytes = usize::try_from(size).expect("the heap fits in the address space");
    unsafe { ALLOCATOR.lock().init(heap_start as usize, heap_bytes) };
    HEAP_START.store(heap_start, Ordering::Relaxed);
    HEAP_SIZE.store(size, Ordering::Relaxed);
    Ok(())
}

/// `(start address, size in bytes)` of the kernel heap, or `(0, 0)`.
///
/// The start address is a virtual address in the bootloader's physical-memory
/// window, which is the address the allocator hands out.
pub fn info() -> (u64, u64) {
    (
        HEAP_START.load(Ordering::Relaxed),
        HEAP_SIZE.load(Ordering::Relaxed),
    )
}

// ---------------------------------------------------------------------------
// Paging (post-08)
// ---------------------------------------------------------------------------

/// The active mapper, once [`init_mapper`] has run.
static MAPPER: Mutex<Option<OffsetPageTable<'static>>> = Mutex::new(None);

/// The frame allocator over the bootloader's memory map, once
/// [`init_frame_allocator`] has run.
static FRAMES: Mutex<Option<BootInfoFrameAllocator>> = Mutex::new(None);

/// The bootloader's physical-memory window offset, or `0` before
/// [`init_mapper`].
static PHYSICAL_MEMORY_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Returns a mutable reference to the active level-4 table.
///
/// # Safety
///
/// The complete physical memory must be mapped at `physical_memory_offset`, and
/// this must be called at most once while no other `&mut` to the table is live
/// (see the module docs): the returned reference aliases the table the CPU walks.
pub unsafe fn active_level_4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    let (level_4_table_frame, _) = Cr3::read();
    let phys = level_4_table_frame.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();

    // SAFETY: the caller guarantees the physical-memory window maps every frame
    // and that no other `&mut` to this table is live.
    unsafe { &mut *page_table_ptr }
}

/// Creates an [`OffsetPageTable`] for the active address space, stores it for
/// [`with_mapper`] and remembers the window offset.
///
/// # Safety
///
/// As [`active_level_4_table`]: the complete physical memory must be mapped at
/// `physical_memory_offset`, and this must be called at most once — two live
/// mappers over the same table would alias `&mut` references.
pub unsafe fn init_mapper(physical_memory_offset: VirtAddr) {
    PHYSICAL_MEMORY_OFFSET.store(physical_memory_offset.as_u64(), Ordering::Relaxed);
    // SAFETY: forwarded to the caller.
    let mapper = unsafe {
        let level_4_table = active_level_4_table(physical_memory_offset);
        OffsetPageTable::new(level_4_table, physical_memory_offset)
    };
    *MAPPER.lock() = Some(mapper);
}

/// The physical-memory window offset, or `0` before [`init_mapper`].
#[must_use]
pub fn physical_memory_offset() -> u64 {
    PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed)
}

/// Runs `f` with the active mapper, or returns `None` if paging is not
/// initialized.
#[must_use]
pub fn with_mapper<R>(f: impl FnOnce(&mut OffsetPageTable<'static>) -> R) -> Option<R> {
    MAPPER.lock().as_mut().map(f)
}

/// Translates `address` through the active mapper, or returns `None` if the
/// address is not mapped (or paging is not initialized).
#[must_use]
pub fn mapper_translate_addr(address: u64) -> Option<u64> {
    with_mapper(|mapper| {
        mapper
            .translate_addr(VirtAddr::new(address))
            .map(PhysAddr::as_u64)
    })
    .flatten()
}

/// Translates `address` by walking the four-level table by hand.
///
/// Read `CR3`, then follow the `p4`/`p3`/`p2`/`p1` indices, dereferencing each
/// table through the physical-memory window. This exists next to
/// [`mapper_translate_addr`] so the two can be compared; the mapper performs
/// exactly this walk.
///
/// # Panics
///
/// Panics if the walk meets a huge (2 MiB or 1 GiB) page, which this simple walk
/// cannot represent as a 4 KiB frame.
#[must_use]
pub fn translate_addr(address: VirtAddr, physical_memory_offset: VirtAddr) -> Option<PhysAddr> {
    let (level_4_table_frame, _) = Cr3::read();
    let indices = [
        address.p4_index(),
        address.p3_index(),
        address.p2_index(),
        address.p1_index(),
    ];

    let mut frame = level_4_table_frame;
    for index in indices {
        let virt = physical_memory_offset + frame.start_address().as_u64();
        let table_ptr: *const PageTable = virt.as_ptr();
        // SAFETY: the physical-memory window maps every frame, so `frame` names a
        // valid, present page table.
        let table = unsafe { &*table_ptr };
        frame = match table[index].frame() {
            Ok(frame) => frame,
            Err(FrameError::FrameNotPresent) => return None,
            Err(FrameError::HugeFrame) => panic!("huge pages are not supported by this walk"),
        };
    }

    Some(frame.start_address() + u64::from(address.page_offset()))
}

/// A frame allocator that hands out the `Usable` frames of the bootloader's
/// memory map, one 4 KiB frame at a time.
pub struct BootInfoFrameAllocator {
    memory_regions: &'static MemoryRegions,
    next: usize,
}

impl BootInfoFrameAllocator {
    /// Creates a frame allocator over `memory_regions`.
    ///
    /// # Safety
    ///
    /// Every frame marked [`MemoryRegionKind::Usable`] must really be unused. The
    /// bootloader marks its own allocations, the kernel image and the kernel stack
    /// as [`MemoryRegionKind::Bootloader`], so the `Usable` regions are free.
    #[must_use]
    pub unsafe fn new(memory_regions: &'static MemoryRegions) -> Self {
        Self {
            memory_regions,
            next: 0,
        }
    }

    /// Iterates over every frame in the `Usable` regions, in address order.
    fn usable_frames(&self) -> impl Iterator<Item = PhysFrame> {
        self.memory_regions
            .iter()
            .filter(|region| region.kind == MemoryRegionKind::Usable)
            .flat_map(|region| (region.start..region.end).step_by(PAGE_SIZE as usize))
            .map(|address| PhysFrame::containing_address(PhysAddr::new(address)))
    }
}

unsafe impl FrameAllocator<Size4KiB> for BootInfoFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        let frame = self.usable_frames().nth(self.next);
        self.next += 1;
        frame
    }
}

/// Stores a frame allocator over `memory_regions` for [`allocate_frame`] and
/// [`map_page`] to use.
///
/// # Safety
///
/// Every frame marked [`MemoryRegionKind::Usable`] must really be unused, and
/// `memory_regions` must outlive the kernel.
pub unsafe fn init_frame_allocator(memory_regions: &'static MemoryRegions) {
    // SAFETY: forwarded to the caller.
    let frames = unsafe { BootInfoFrameAllocator::new(memory_regions) };
    *FRAMES.lock() = Some(frames);
}

/// Runs `f` with the frame allocator, or returns `None` if it is not
/// initialized.
#[must_use]
pub fn with_frame_allocator<R>(f: impl FnOnce(&mut BootInfoFrameAllocator) -> R) -> Option<R> {
    FRAMES.lock().as_mut().map(f)
}

/// Allocates one 4 KiB frame from the bootloader's memory map.
#[must_use]
pub fn allocate_frame() -> Option<PhysFrame> {
    with_frame_allocator(<BootInfoFrameAllocator as FrameAllocator<Size4KiB>>::allocate_frame)
        .flatten()
}

/// Maps the page containing `virt` to the frame containing `phys`.
///
/// # Safety
///
/// `virt` must not already be mapped to a different frame, and the caller must
/// treat the physical frame as owned by the mapping: mapping a frame that is
/// already in use aliases memory.
///
/// # Errors
///
/// Returns an error if paging is not initialized, or if `map_to` fails — for
/// example because the page is already mapped or the frame allocator ran out.
pub unsafe fn map_page(virt: u64, phys: u64, writable: bool) -> Result<(), &'static str> {
    let mut flags = PageTableFlags::PRESENT;
    if writable {
        flags |= PageTableFlags::WRITABLE;
    }
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt));
    let frame = PhysFrame::containing_address(PhysAddr::new(phys));

    let mut mapper_guard = MAPPER.lock();
    let mapper = mapper_guard.as_mut().ok_or("paging is not initialized")?;
    let mut frames_guard = FRAMES.lock();
    let frames = frames_guard
        .as_mut()
        .ok_or("the frame allocator is not initialized")?;

    // SAFETY: forwarded to the caller. The mapper and allocator are the active
    // ones, and the two locks are always taken in the same order (mapper, then
    // frames), so this cannot deadlock.
    let flush =
        unsafe { mapper.map_to(page, frame, flags, frames) }.map_err(|_| "map_to failed")?;
    flush.flush();
    Ok(())
}
