//! Verifies that the in-QEMU test framework actually notices a panicking test.
//!
//! This file deliberately has no `#[test_case]`: it calls a failing assertion
//! directly and expects its own panic handler to run. `harness = false` in
//! `Cargo.toml` keeps cargo from wrapping it in the normal harness.
//!
//! The entry point still goes through [`entry_point!`] so the ELF carries the
//! `.bootloader-config` section the bootloader needs in order to load it.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use bootloader_api::{BootInfo, entry_point};
use kernel::{QemuExitCode, exit_qemu, serial_print, serial_println};

entry_point!(test_kernel_main);

fn test_kernel_main(_boot_info: &'static mut BootInfo) -> ! {
    kernel::testing::init_serial();
    should_fail();
    serial_println!("[test did not panic]");
    exit_qemu(QemuExitCode::Failed);
}

fn should_fail() {
    serial_print!("should_panic::should_fail...\t");
    assert_eq!(0, 1);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    serial_println!("[ok]");
    exit_qemu(QemuExitCode::Success);
}
