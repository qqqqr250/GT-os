//! The bootable binary: the bootloader's entry point into the
//! `rustos-heap-allocation` library.
//!
//! All of the logic lives in the library (`src/lib.rs`); this file only owns the
//! bootloader configuration, the `_start` entry point that [`entry_point`]
//! generates, and the panic/alloc handlers Rust requires in the final binary.

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

use bootloader_api::config::Mapping;
use bootloader_api::{BootInfo, BootloaderConfig, entry_point};

/// Boot-time configuration handed to the bootloader.
///
/// `physical_memory` is enabled because both the paging layer and the heap need
/// it: the heap is mapped through the mapper, and the mapper reads physical
/// frames through the window. The tests use the same configuration, so they
/// exercise the same boot path.
pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.kernel_stack_size = 64 * 1024;
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    rustos_heap_allocation::kernel_main(boot_info)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    rustos_heap_allocation::panic_handler(info)
}

#[alloc_error_handler]
fn alloc_error(layout: core::alloc::Layout) -> ! {
    rustos_heap_allocation::alloc_error_handler(layout)
}
