//! The bootable binary: the bootloader's entry point into the `gtos-minimal`
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
/// `physical_memory` is enabled because the bootloader does not identity-map RAM
/// for the kernel: the VGA text buffer at `0xb8000` is only reachable through the
/// window it reports in `BootInfo::physical_memory_offset`.
pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.kernel_stack_size = 64 * 1024;
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    gtos_minimal::kernel_main(boot_info)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    gtos_minimal::panic_handler(info)
}
