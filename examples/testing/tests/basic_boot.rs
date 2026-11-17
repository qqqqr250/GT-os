//! In-QEMU boot tests for the `rustos-testing` crate.
//!
//! Cargo links this file into a bare-metal test binary, the runner in `runner/`
//! wraps it in a bootable disk image, and QEMU reports pass/fail through the
//! `isa-debug-exit` device. It boots through the same [`entry_point!`] path as the
//! real binary so the test ELF carries the `.bootloader-config` section the
//! bootloader needs.

#![no_std]
#![no_main]
// Test functions fail by panicking, so the pedantic `missing_panics_doc` lint
// does not apply to them.
#![allow(clippy::missing_panics_doc)]
#![feature(custom_test_frameworks)]
#![test_runner(rustos_testing::test_runner)]
#![reexport_test_harness_main = "test_main"]

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, Ordering};

use bootloader_api::config::Mapping;
use bootloader_api::{BootInfo, BootloaderConfig, entry_point};
use rustos_testing::serial_println;

/// The physical-memory window the bootloader reported, read by the tests.
static PHYSICAL_MEMORY_OFFSET: AtomicU64 = AtomicU64::new(0);

/// The tests need the physical-memory window so they can check that the
/// bootloader reported one, exactly as the binary does.
static TEST_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(test_kernel_main, config = &TEST_CONFIG);

fn test_kernel_main(boot_info: &'static mut BootInfo) -> ! {
    rustos_testing::serial::init();
    if let Some(offset) = boot_info.physical_memory_offset.into_option() {
        PHYSICAL_MEMORY_OFFSET.store(offset, Ordering::Relaxed);
    }
    test_main();
    rustos_testing::halt_forever()
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    rustos_testing::test_panic_handler(info)
}

#[test_case]
fn test_serial_output() {
    // The runner puts COM1 on stdout, so this line is what a passing run shows.
    serial_println!("basic_boot: serial output is live");
}

#[test_case]
fn test_bootloader_mapped_physical_memory() {
    let offset = PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed);
    assert_ne!(offset, 0, "the bootloader should map physical memory");
}
