//! In-QEMU test for the post-07 hardware-interrupt hook in `gtos-double-faults`.
//!
//! The crate is post-06, so its focus is the double-fault handler and its IST
//! stack, but [`interrupts::init`] also brings forward post-07's PIC/PIT setup.
//! This test proves the timer really interrupts the CPU through the crate's
//! *public* API: `cargo test` links this file for `x86_64-unknown-none`, the
//! runner in `runner/` wraps it in a bootable disk image, and QEMU reports
//! pass/fail through the `isa-debug-exit` device.
//!
//! Run it with `scripts\double-faults.ps1 -Test` (or `cargo qtest` from the crate
//! directory). The entry point goes through [`entry_point!`] so the test ELF
//! carries the `.bootloader-config` section the bootloader needs.

#![no_std]
#![no_main]
// Test functions fail by panicking, so the pedantic `missing_panics_doc` lint
// does not apply to them.
#![allow(clippy::missing_panics_doc)]
#![feature(custom_test_frameworks)]
#![test_runner(gtos_double_faults::test_runner)]
#![reexport_test_harness_main = "test_main"]

use core::panic::PanicInfo;

use bootloader_api::{BootInfo, BootloaderConfig, entry_point};
use gtos_double_faults::{gdt, halt_forever, interrupts, pic, pit, serial};

static TEST_CONFIG: BootloaderConfig = BootloaderConfig::new_default();

entry_point!(test_kernel_main, config = &TEST_CONFIG);

fn test_kernel_main(_boot_info: &'static mut BootInfo) -> ! {
    serial::init();
    // The GDT/TSS must be loaded before the IDT names the double-fault IST slot,
    // and `interrupts::init` is what remaps the PIC, starts the PIT and enables
    // interrupts, so the timer is already ticking when the harness runs.
    let _ = gdt::init();
    interrupts::init();
    test_main();
    halt_forever()
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    gtos_double_faults::test_panic_handler(info)
}

#[test_case]
fn the_timer_interrupt_advances_the_tick_count() {
    // `hlt` is only left by an interrupt, so this both waits for ticks and
    // proves the timer really is interrupting the CPU.
    let start = interrupts::ticks();
    let deadline = start + 20; // ~200 ms at 100 Hz
    while interrupts::ticks() < deadline {
        x86_64::instructions::hlt();
    }
    assert!(
        interrupts::ticks() >= deadline,
        "the PIT should have produced at least {deadline} ticks"
    );
}

#[test_case]
fn the_pic_is_remapped_above_the_exception_vectors() {
    // Out of reset IRQ0 is vector 8, which the IDT uses for the double fault.
    const { assert!(pic::PIC1_OFFSET > 0x1f) };
    assert_eq!(interrupts::TIMER_VECTOR, 0x20);
}

#[test_case]
fn the_pit_runs_at_the_configured_frequency() {
    assert_eq!(pit::divisor(pit::TICK_HZ), 11_931);
}
