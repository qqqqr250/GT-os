//! In-QEMU boot tests for the `RustOS` kernel.
//!
//! Run them with `cargo xtest` (or
//! `cargo test --manifest-path kernel\Cargo.toml --target x86_64-unknown-none`).
//! Cargo links this file
//! into a bare-metal test binary, the runner in `runner/` wraps it in a bootable
//! disk image, and QEMU reports pass/fail through the `isa-debug-exit` device.
//!
//! These tests boot the real kernel entry path. The VGA *text-buffer* logic is
//! covered by the unit tests in `kernel/src/vga.rs`, which drive a RAM buffer:
//! under QEMU the bootloader always switches the display into linear-framebuffer
//! graphics mode, so the legacy text aperture at `0xb8000` is not backed and
//! cannot be asserted on here.
#![no_std]
#![no_main]
// The physical-memory probe reads device memory through a raw pointer.
#![allow(unsafe_code)]
// Test functions fail by panicking, so the pedantic `missing_panics_doc` lint
// does not apply to them.
#![allow(clippy::missing_panics_doc)]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner)]
#![reexport_test_harness_main = "test_main"]

// post-10: the heap tests use `Box` and `Vec`.
extern crate alloc;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, Ordering};

use bootloader_api::config::Mapping;
use bootloader_api::{BootInfo, BootloaderConfig, entry_point};

/// Test kernels need the physical-memory window, exactly like the real kernel,
/// so that the VGA text buffer at `0xb8000` is reachable.
pub static TEST_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(test_kernel_main, config = &TEST_CONFIG);

/// The physical-memory window the bootloader handed us, for the VGA test.
static PHYSICAL_MEMORY_OFFSET: AtomicU64 = AtomicU64::new(0);

/// A free virtual page (`PML4` index 160) the paging tests map. Its `PML4` entry
/// is unused, so mapping it has to allocate new page tables.
const PAGING_TEST_PAGE: u64 = 0x0000_5000_0000_0000;

/// A second free page (`PML4` index 192) that no test maps, for the
/// no-translation assertion. It is distinct from [`PAGING_TEST_PAGE`] so the
/// result does not depend on the order the harness runs the tests in.
const UNMAPPED_PAGE: u64 = 0x0000_6000_0000_0000;

fn test_kernel_main(boot_info: &'static mut BootInfo) -> ! {
    kernel::testing::init_serial();
    if let Some(offset) = boot_info.physical_memory_offset.into_option() {
        PHYSICAL_MEMORY_OFFSET.store(offset, Ordering::Relaxed);
    }
    // post-05: install the exception handlers before the harness runs, so the
    // breakpoint test has an IDT to trap into.
    kernel::testing::init_idt();
    // post-08: build the paging mapper and frame allocator from the bootloader's
    // memory map. This is the only mapper the test boot path creates — the real
    // kernel leaves the mapping work to `usermode`, which the tests never enter.
    if let Some(offset) = boot_info.physical_memory_offset.into_option() {
        kernel::testing::init_memory(offset, &boot_info.memory_regions);
    }
    // post-10: build the kernel heap so the tests can allocate. `init_memory`
    // above only creates the mapper and frame allocator; the heap is a separate
    // region taken from the top of the largest usable one.
    if let Some(offset) = boot_info.physical_memory_offset.into_option() {
        kernel::testing::init_heap(
            &boot_info.memory_regions,
            (boot_info.kernel_addr, boot_info.kernel_len),
            offset,
        )
        .expect("heap initialization failed");
    }
    test_main();
    kernel::testing::halt()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kernel::test_panic_handler(info)
}

#[test_case]
fn test_serial_is_writable() {
    // The test runner already prints over COM1; this just proves it from a test.
    kernel::serial_println!("basic_boot: serial output is live");
}

#[test_case]
fn test_breakpoint_exception() {
    // post-05: an `int3` must reach the breakpoint handler and return to us.
    // The handler counts, so the count going up by exactly one proves both that
    // the IDT caught the trap and that execution resumed.
    let before = kernel::testing::breakpoint_count();
    kernel::testing::trigger_breakpoint();
    assert_eq!(
        kernel::testing::breakpoint_count(),
        before + 1,
        "the breakpoint handler should have run exactly once"
    );
}

#[test_case]
fn test_timer_interrupts_advance() {
    // post-07: `init_idt` remapped the PIC and unmasked the timer. `hlt` only
    // returns on an interrupt, so waiting for the count to move proves the PIT
    // really is interrupting the CPU (and that the handler's EOI re-arms IRQ0).
    let start = kernel::testing::ticks();
    let deadline = start + 20; // ~200 ms at 100 Hz
    while kernel::testing::ticks() < deadline {
        x86_64::instructions::hlt();
    }
    assert!(
        kernel::testing::ticks() >= deadline,
        "the PIT should have produced at least 20 ticks"
    );
}

#[test_case]
fn test_printing_during_timer_interrupts_does_not_deadlock() {
    // post-07's deadlock fix. Turn the timer handler's dot printing on, then
    // print through the same lock from the main thread. Before the fix the timer
    // could fire while the main thread held the serial lock; the handler would
    // spin on it forever, because the CPU cannot return to release it. The
    // `without_interrupts` wrappers make the critical section uninterruptible,
    // so this loop must finish.
    kernel::testing::set_timer_dots(true);
    let start = kernel::testing::ticks();
    let deadline = start + 30; // ~300 ms at 100 Hz, i.e. at least 3 dots
    while kernel::testing::ticks() < deadline {
        kernel::testing::console_println("RustOS: printing under timer interrupts");
    }
    kernel::testing::set_timer_dots(false);

    // Reaching here at all is the proof; this only confirms the timer ran.
    assert!(
        kernel::testing::ticks() >= deadline,
        "the timer should have kept running while the main thread printed"
    );
}

#[test_case]
fn test_physical_memory_window_maps_ram() {
    let offset = PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed);
    assert_ne!(offset, 0, "the bootloader should map physical memory");

    // Conventional RAM is reachable through the window. Reading it must not
    // fault; the exact bytes are firmware-dependent, so only the access is
    // checked here.
    // SAFETY: `offset` is the bootloader's physical-memory window and 0x1000 is
    // inside the low, usable RAM the bootloader maps.
    let probe = unsafe { ((offset + 0x1000) as *const u32).read_volatile() };
    let _ = probe;
}

#[test_case]
fn test_vga_fallback_path_is_usable() {
    let offset = PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed);
    assert_ne!(offset, 0, "the bootloader should map physical memory");

    kernel::testing::init_vga(offset);
    assert!(kernel::testing::vga_is_initialized());

    // Exercise the volatile device writes. Their effect is only observable when
    // the firmware left the machine in text mode, which the bootloader does not
    // do under QEMU, so this only proves the path does not fault.
    kernel::testing::vga_write("RustOS");
    kernel::testing::vga_clear();

    // Out-of-range reads are always blank.
    assert_eq!(kernel::testing::vga_screen_char(25, 0).ascii, b' ');
}

#[test_case]
fn test_vga_fallback_colour_is_tracked() {
    let offset = PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed);
    assert_ne!(offset, 0, "the bootloader should map physical memory");

    kernel::testing::init_vga(offset);
    kernel::testing::vga_set_fg(kernel::testing::Color::LightGreen);
    assert_eq!(
        kernel::testing::vga_color().foreground(),
        kernel::testing::Color::LightGreen
    );

    // A 24-bit colour snaps to the nearest entry of the text palette.
    assert_eq!(
        kernel::testing::Color::from_rgb(0x55, 0xff, 0x55),
        kernel::testing::Color::LightGreen
    );

    // The console's named colours keep their meaning on the text fallback: its
    // muted success green is matched to the bright text green, not to grey.
    assert_eq!(
        kernel::testing::vga_color_for_rgb(0x00_7e_c6_99),
        kernel::testing::Color::LightGreen
    );
}

#[test_case]
fn test_the_mapper_agrees_with_the_hand_written_walk() {
    // post-08: a function pointer is in the kernel image, which the bootloader
    // maps with 4 KiB pages, so the hand-written walk and the `OffsetPageTable`
    // mapper must find the same frame.
    let address = kernel::testing::mapper_translate_addr as *const () as u64;
    let by_hand = kernel::testing::translate_addr(address);
    assert!(by_hand.is_some(), "the kernel image must be mapped");
    assert_eq!(
        by_hand,
        kernel::testing::mapper_translate_addr(address),
        "the walk and the mapper should agree"
    );
}

#[test_case]
fn test_the_physical_memory_window_translates_to_frame_zero() {
    // Virtual `offset + 0` is physical frame 0, so the window is present and the
    // mapper is initialized.
    let offset = PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed);
    assert_ne!(offset, 0, "the bootloader should map physical memory");
    assert_eq!(kernel::testing::mapper_translate_addr(offset), Some(0));
}

#[test_case]
fn test_an_unmapped_high_page_has_no_translation() {
    assert_eq!(kernel::testing::mapper_translate_addr(UNMAPPED_PAGE), None);
}

#[test_case]
fn test_mapping_a_fresh_page_reads_back() {
    assert_eq!(
        kernel::testing::mapper_translate_addr(PAGING_TEST_PAGE),
        None,
        "the page must start unmapped"
    );

    // Take a free frame from the bootloader's memory map, then map it into a
    // fresh page. (The VGA buffer is device memory and does not read back, so a
    // usable frame is the honest target for a round-trip.)
    let phys = kernel::testing::allocate_frame().expect("the memory map has usable frames");
    // SAFETY: `PAGING_TEST_PAGE` is unused and `phys` was just handed out, so
    // nothing else aliases it.
    unsafe { kernel::testing::map_page(PAGING_TEST_PAGE, phys, true) }.expect("map_page failed");
    assert_eq!(
        kernel::testing::mapper_translate_addr(PAGING_TEST_PAGE),
        Some(phys)
    );

    // The mapping is real: writing through the virtual page reaches the physical
    // frame it was mapped to.
    // SAFETY: `PAGING_TEST_PAGE` was just mapped writable.
    unsafe {
        let cell = PAGING_TEST_PAGE as *mut u16;
        cell.write_volatile(0x1234);
        assert_eq!(cell.read_volatile(), 0x1234);
    }
}

#[test_case]
fn test_the_kernel_heap_is_initialized() {
    // post-10: `memory::init` picks the heap from the top of the largest usable
    // region. The boot log prints the same window.
    let (start, size) = kernel::testing::heap_info();
    assert_ne!(start, 0, "the heap should be initialized");
    assert!(size > 0, "the heap should have a non-zero size");
    assert_eq!(size % 4096, 0, "the heap is page aligned");
}

#[test_case]
fn test_heap_allocations_round_trip() {
    // post-10: a `Box` and a growing `Vec` must survive the global allocator. The
    // kernel's heap is a linked-list allocator over the physical-memory window, so
    // a heap pointer lands inside the window `heap_info` reports.
    let (start, size) = kernel::testing::heap_info();
    let value = alloc::boxed::Box::new(0x1234_5678_u32);
    assert_eq!(*value, 0x1234_5678);
    let address = alloc::boxed::Box::into_raw(value) as u64;
    assert!(address >= start, "below the heap: {address:#x}");
    assert!(address < start + size, "above the heap: {address:#x}");

    let mut vec = alloc::vec::Vec::new();
    for i in 0..1000_u64 {
        vec.push(i);
    }
    assert_eq!(vec.len(), 1000);
    assert_eq!(vec.iter().sum::<u64>(), 999 * 1000 / 2);
}
