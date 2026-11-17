//! The bootable binary: the bootloader's entry point into the `rustos-paging`
//! library.
//!
//! All of the logic lives in the library (`src/lib.rs`); this file only owns the
//! bootloader configuration, the `_start` entry point that [`entry_point`]
//! generates, and the panic handler Rust requires in the final binary.

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

use bootloader_api::config::Mapping;
use bootloader_api::{BootInfo, BootloaderConfig, entry_point};

/// Boot-time configuration handed to the bootloader.
///
/// `physical_memory` is enabled because post-08 is *about* the physical-memory
/// window: the kernel reads `CR3` through it and builds its mapper from it. The
/// tests use the same configuration, so they exercise the same boot path.
pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.kernel_stack_size = 64 * 1024;
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    rustos_paging::kernel_main(boot_info)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    rustos_paging::panic_handler(info)
}

#[alloc_error_handler]
fn alloc_error(layout: core::alloc::Layout) -> ! {
    rustos_paging::alloc_error_handler(layout)
}
