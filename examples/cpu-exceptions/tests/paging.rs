//! In-QEMU test for the post-08 paging hook in `rustos-cpu-exceptions`.
//!
//! The crate is post-05, so its focus is the IDT, but it also carries post-07's
//! PIC/PIT and post-08's [`memory`] module forward. This test proves the paging
//! API works through the crate's *public* API: `cargo test` links this file for
//! `x86_64-unknown-none`, the runner in `runner/` wraps it in a bootable disk
//! image, and QEMU reports pass/fail through the `isa-debug-exit` device.
//!
//! Run it with `scripts\cpu-exceptions.ps1 -Test` (or `cargo qtest` from the crate
//! directory). The entry point goes through [`entry_point!`] so the test ELF
//! carries the `.bootloader-config` section the bootloader needs; its config turns
//! on the physical-memory window, which is what the paging API needs.

#![no_std]
#![no_main]
// Building the mapper and mapping a page are inherently `unsafe`.
#![allow(unsafe_code)]
// Test functions fail by panicking, so the pedantic `missing_panics_doc` lint
// does not apply to them.
#![allow(clippy::missing_panics_doc)]
#![feature(custom_test_frameworks)]
#![test_runner(rustos_cpu_exceptions::test_runner)]
#![reexport_test_harness_main = "test_main"]

use core::panic::PanicInfo;

use bootloader_api::config::Mapping;
use bootloader_api::{BootInfo, BootloaderConfig, entry_point};
use rustos_cpu_exceptions::{halt_forever, interrupts, memory, serial};

/// A free virtual page (`PML4` index 144) the mapping test uses.
const TEST_PAGE: u64 = 0x0000_4800_0000_0000;

static TEST_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(test_kernel_main, config = &TEST_CONFIG);

fn test_kernel_main(boot_info: &'static mut BootInfo) -> ! {
    serial::init();
    // Install the IDT, remap the PIC, start the PIT and enable interrupts, so
    // the timer is already ticking when the harness runs.
    interrupts::init();

    let offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("the test config must map physical memory");
    // SAFETY: the bootloader mapped all physical memory at `offset`, and this is
    // the only mapper created in this binary.
    unsafe { memory::init_paging(offset, &boot_info.memory_regions) };

    test_main();
    halt_forever()
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    rustos_cpu_exceptions::test_panic_handler(info)
}

#[test_case]
fn the_timer_still_interrupts_the_cpu() {
    // Adding paging did not break the PIC/PIT: `hlt` is only left by an interrupt.
    let start = interrupts::ticks();
    let deadline = start + 20; // ~200 ms at 100 Hz
    while interrupts::ticks() < deadline {
        x86_64::instructions::hlt();
    }
    assert!(interrupts::ticks() >= deadline);
}

#[test_case]
fn the_physical_memory_window_maps_frame_zero() {
    // Virtual `offset + 0` is physical frame 0.
    let offset = memory::physical_memory_offset();
    assert_eq!(memory::mapper_translate_addr(offset), Some(0));
}

#[test_case]
fn a_fresh_page_can_be_mapped_and_read_back() {
    assert_eq!(memory::mapper_translate_addr(TEST_PAGE), None);

    // Map a frame handed out by the bootloader's memory map, so the write/read
    // round-trip is real.
    let frame = memory::allocate_frame().expect("the memory map has usable frames");
    let phys = frame.start_address().as_u64();
    // SAFETY: `TEST_PAGE` is unused and `frame` was just handed out.
    unsafe { memory::map_page(TEST_PAGE, phys, true) }.expect("map_page failed");
    assert_eq!(memory::mapper_translate_addr(TEST_PAGE), Some(phys));

    // SAFETY: `TEST_PAGE` was just mapped writable.
    unsafe {
        let cell = TEST_PAGE as *mut u16;
        cell.write_volatile(0xbeef);
        assert_eq!(cell.read_volatile(), 0xbeef);
    }
}
