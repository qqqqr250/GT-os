//! Page-table management: the bootloader's physical-memory window, an
//! [`OffsetPageTable`] mapper, a frame allocator over the bootloader's memory map
//! and a hand-written address translation to compare it against.
//!
//! This is `blog_os` post-08 (*Introduction to Paging*) plus the post-09 paging
//! *implementation*, done properly. Post-08's only code change is to read `CR3`
//! and print the active level-4 table's address ([`active_level_4_frame`]); the
//! real work — walking the four-level table, mapping a page through an
//! [`OffsetPageTable`] and handing out frames from the bootloader's memory map —
//! is post-09's. Both live here.
//!
//! The tutorial uses `bootloader` 0.9's `MemoryMap`; this repository targets
//! `bootloader_api` 0.11, whose memory map is a [`MemoryRegions`] slice of
//! [`MemoryRegion`]s. The *shape* is unchanged: keep the `Usable` regions and step
//! through them one frame at a time.
//!
//! Everything is exposed twice on purpose:
//!
//! - the pure functions ([`active_level_4_table`], [`translate_addr`],
//!   [`create_example_mapping`], [`EmptyFrameAllocator`],
//!   [`BootInfoFrameAllocator`]) mirror the post, so the diff against the
//!   tutorial reads as "the same code, wired into this repository",
//! - the global helpers ([`init_paging`], [`with_mapper`], [`with_frames`],
//!   [`mapper_translate_addr`], [`map_page`]) keep the single mapper and frame
//!   allocator the kernel actually uses, so [`crate::kernel_main`] and the tests
//!   share one address space.
//!
//! # Aliasing
//!
//! An [`OffsetPageTable`] borrows the active level-4 table *mutably*. Two live
//! mappers over the same table would alias `&mut` references, which is undefined
//! behaviour, so the mapper is created **once** — [`init_paging`] or [`init`],
//! never both — and reached through [`with_mapper`] afterwards.

use core::sync::atomic::{AtomicU64, Ordering};

use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
use spin::Mutex;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::page_table::FrameError;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
    Translate,
};
use x86_64::{PhysAddr, VirtAddr};

/// The frame size the bootloader maps with: 4 KiB.
pub const FRAME_SIZE: u64 = 4096;

/// The physical address of the legacy VGA text buffer.
///
/// Post-08's example mapping points a page at this frame. It is a convenient,
/// harmless target: mapping it is safe, and writing through the mapping proves
/// the page really reaches the frame.
pub const VGA_BUFFER: u64 = 0xb8000;

/// The active mapper, once [`init_paging`] has run.
static MAPPER: Mutex<Option<OffsetPageTable<'static>>> = Mutex::new(None);

/// The frame allocator over the bootloader's memory map, once [`init_paging`] has
/// run.
static FRAMES: Mutex<Option<BootInfoFrameAllocator>> = Mutex::new(None);

/// The bootloader's physical-memory window offset, or `0` before [`init_paging`].
static PHYSICAL_MEMORY_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Returns a mutable reference to the active level-4 table.
///
/// # Safety
///
/// The complete physical memory must be mapped at `physical_memory_offset`, and
/// this must be called at most once (and not together with [`init`]): the returned
/// reference aliases the table the CPU is walking.
#[must_use]
pub unsafe fn active_level_4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    let (level_4_table_frame, _) = Cr3::read();
    let phys = level_4_table_frame.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();

    // SAFETY: the caller guarantees the physical-memory window maps every frame
    // and that no other `&mut` to this table is live.
    unsafe { &mut *page_table_ptr }
}

/// Creates an [`OffsetPageTable`] for the active address space.
///
/// # Safety
///
/// As [`active_level_4_table`]: the complete physical memory must be mapped at
/// `physical_memory_offset`, and this must be called at most once — two live
/// mappers over the same table would alias `&mut` references.
#[must_use]
pub unsafe fn init(physical_memory_offset: VirtAddr) -> OffsetPageTable<'static> {
    // SAFETY: forwarded to the caller.
    unsafe {
        let level_4_table = active_level_4_table(physical_memory_offset);
        OffsetPageTable::new(level_4_table, physical_memory_offset)
    }
}

/// The physical address of the active level-4 table, read from `CR3`.
///
/// This is post-08's whole code change: read `CR3` and report where the root page
/// table lives.
#[must_use]
pub fn active_level_4_frame() -> PhysAddr {
    let (frame, _) = Cr3::read();
    frame.start_address()
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

/// Maps `page` to the VGA text buffer (`0xb8000`) with the given mapper and frame
/// allocator.
///
/// Post-08's example mapping. The tutorial marks it `FIXME: this is not safe`; it
/// is a *teaching* mapping, so this crate keeps the same shape and documents the
/// caveat instead of hiding it.
///
/// # Panics
///
/// Panics if `map_to` fails, for example because `page` is already mapped.
pub fn create_example_mapping(
    page: Page,
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) {
    let frame = PhysFrame::containing_address(PhysAddr::new(VGA_BUFFER));
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;

    // SAFETY: the caller owns the mapping for `page`; the target frame is the VGA
    // buffer, so the mapping cannot break the kernel's own address space.
    let result = unsafe { mapper.map_to(page, frame, flags, frame_allocator) };
    result
        .expect("create_example_mapping: map_to failed")
        .flush();
}

/// A frame allocator that never hands out a frame.
///
/// Post-08 uses it to show that mapping a page into a fresh part of the address
/// space *needs* an allocator: the walk has to create new page tables, and without
/// frames it fails instead of silently corrupting the address space.
pub struct EmptyFrameAllocator;

unsafe impl FrameAllocator<Size4KiB> for EmptyFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        None
    }
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
    pub unsafe fn init(memory_regions: &'static MemoryRegions) -> Self {
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
            .flat_map(|region| (region.start..region.end).step_by(FRAME_SIZE as usize))
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

/// Initializes the global mapper and frame allocator and remembers the
/// physical-memory window.
///
/// # Safety
///
/// As [`init`]: the complete physical memory must be mapped at
/// `physical_memory_offset`, and this must be called at most once. The `Usable`
/// regions of `memory_regions` must really be unused.
pub unsafe fn init_paging(physical_memory_offset: u64, memory_regions: &'static MemoryRegions) {
    PHYSICAL_MEMORY_OFFSET.store(physical_memory_offset, Ordering::Relaxed);
    let offset = VirtAddr::new(physical_memory_offset);
    // SAFETY: forwarded to the caller.
    let mapper = unsafe { init(offset) };
    *MAPPER.lock() = Some(mapper);
    // SAFETY: forwarded to the caller.
    let frames = unsafe { BootInfoFrameAllocator::init(memory_regions) };
    *FRAMES.lock() = Some(frames);
}

/// The physical-memory window offset, or `0` before [`init_paging`].
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

/// Runs `f` with the frame allocator, or returns `None` if paging is not
/// initialized.
#[must_use]
pub fn with_frames<R>(f: impl FnOnce(&mut BootInfoFrameAllocator) -> R) -> Option<R> {
    FRAMES.lock().as_mut().map(f)
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

/// Allocates one 4 KiB frame from the bootloader's memory map.
#[must_use]
pub fn allocate_frame() -> Option<PhysFrame> {
    with_frames(<BootInfoFrameAllocator as FrameAllocator<Size4KiB>>::allocate_frame).flatten()
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
