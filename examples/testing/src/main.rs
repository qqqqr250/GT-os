//! The bootable binary: the bootloader's entry point into the `gtos-testing`
//! library.
//!
//! All of the logic lives in the library (`src/lib.rs`); this file only owns the
//! bootloader configuration, the `_start` entry point that [`entry_point`]
//! generates, and the panic handler Rust requires in the final binary.

#![no_std]
#![no_main]

use bootloader_api::config::Mapping;
use bootloader_api::{BootInfo, BootloaderConfig, entry_point};

/// Boot-time configuration handed to the bootloader.
///
/// `physical_memory` is enabled so the kernel has a window onto RAM; the test
/// framework does not strictly need it, but keeping the binary's configuration
/// and the tests' configuration aligned means the tests exercise the same boot
/// path the real kernel takes.
pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.kernel_stack_size = 64 * 1024;
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    gtos_testing::kernel_main(boot_info)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    gtos_testing::panic_handler(info)
}
