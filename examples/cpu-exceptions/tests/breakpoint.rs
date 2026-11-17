//! In-QEMU test for the `rustos-cpu-exceptions` IDT.
//!
//! The unit tests in `src/lib.rs` exercise the vector table from inside the
//! library. This test does the same through the crate's *public* API, from a
//! separate test binary, which is what proves the surface is usable from outside:
//! `cargo test` links this file for `x86_64-unknown-none`, the runner in
//! `runner/` wraps it in a bootable disk image, and QEMU reports pass/fail
//! through the `isa-debug-exit` device.
//!
//! Run it with `scripts\cpu-exceptions.ps1 -Test` (or `cargo qtest` from the
//! crate directory). The entry point goes through [`entry_point!`] so the test
//! ELF carries the `.bootloader-config` section the bootloader needs.

#![no_std]
#![no_main]
// Test functions fail by panicking, so the pedantic `missing_panics_doc` lint
// does not apply to them.
#![allow(clippy::missing_panics_doc)]
#![feature(custom_test_frameworks)]
#![test_runner(rustos_cpu_exceptions::test_runner)]
#![reexport_test_harness_main = "test_main"]

use core::panic::PanicInfo;

use bootloader_api::{BootInfo, BootloaderConfig, entry_point};
use rustos_cpu_exceptions::interrupts::{
    Vector, exception_count, init, last_vector, trigger_breakpoint,
};
use rustos_cpu_exceptions::{halt_forever, serial, serial_println};

static TEST_CONFIG: BootloaderConfig = BootloaderConfig::new_default();

entry_point!(test_kernel_main, config = &TEST_CONFIG);

fn test_kernel_main(_boot_info: &'static mut BootInfo) -> ! {
    serial::init();
    // The handlers must be installed before the harness runs: the tests below
    // raise a breakpoint, and without a handler that would triple-fault.
    init();
    test_main();
    halt_forever()
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    rustos_cpu_exceptions::test_panic_handler(info)
}

#[test_case]
fn the_breakpoint_handler_records_the_exception() {
    let before = exception_count();
    trigger_breakpoint();
    assert_eq!(
        exception_count(),
        before + 1,
        "the breakpoint handler should have run exactly once"
    );
    assert_eq!(last_vector(), Some(Vector::Breakpoint));
}

#[test_case]
fn a_breakpoint_does_not_stop_the_cpu() {
    // The handler returns with `iretq`, so reaching this line proves execution
    // continued past the `int3`.
    trigger_breakpoint();
    serial_println!("breakpoint.rs: execution continued after `int3`");
}
