//! `GTos` kernel binary: the bootloader's entry point into the `kernel` crate.
//!
//! All of the kernel lives in the `kernel` library (see `src/lib.rs`); this file
//! only owns the bootloader configuration, the `_start` entry point that the
//! [`entry_point`] macro generates, and the panic/alloc handlers that Rust
//! requires in the final binary.

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

use bootloader_api::config::Mapping;
use bootloader_api::{BootInfo, BootloaderConfig, entry_point};

/// Boot-time configuration handed to the bootloader.
///
/// The framebuffer request is a *minimum*: firmware without a suitable mode
/// falls back to a smaller one and the console adapts its glyph scale.
///
/// `physical_memory` is enabled because the bootloader does not identity-map
/// RAM for the kernel: usable regions are only reachable through the offset it
/// reports in `BootInfo::physical_memory_offset`. The heap needs that window.
pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.kernel_stack_size = 512 * 1024;
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    kernel::kernel_main(boot_info)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    kernel::panic_handler(info)
}

#[alloc_error_handler]
fn alloc_error(layout: core::alloc::Layout) -> ! {
    kernel::alloc_error_handler(layout)
}
