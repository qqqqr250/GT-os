//! `GTos` kernel library.
//!
//! The crate is split into this library plus a thin binary (`src/main.rs`).
//! The binary owns the bootloader entry point and the panic/alloc handlers; this
//! library owns everything else.

#![no_std]
#![feature(abi_x86_interrupt)]
// These helpers were exercised by the former test framework and are kept for
// on-device diagnostics. With that framework gone they are intentionally unused
// in a normal build.
#![allow(dead_code)]
// A freestanding kernel cannot avoid `unsafe`: port I/O, descriptor tables,
// paging and raw pointer access all need it. Every `unsafe` block carries a
// SAFETY comment explaining why it is sound. The workspace `unsafe_code` lint
// stays active for the host launcher, which must remain entirely safe.
#![allow(unsafe_code)]
// Screen coordinates, framebuffer strides and packed 0xRRGGBB colour channels
// are converted with `as`, where the truncation or wrap-around is the intended,
// checked-by-construction behaviour. The pedantic cast lints would only add
// noise, or force error handling that can never actually fail.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

extern crate alloc;

mod allocator;
mod ata;
mod audio;
mod browser;
mod console;
mod fat;
mod fm;
mod font;
mod input;
mod interrupts;
mod keyboard;
mod memory;
mod mouse;
mod net;
mod pci;
mod scheme;
mod serial;
mod shell;
mod sync;
mod task;
mod usermode;
mod vga;
mod volatile;
mod wm;

use bootloader_api::BootInfo;

use crate::console::println;

/// The kernel's entry point, called by the binary once the bootloader has handed
/// over the machine.
pub fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    // Serial first: from here on every console write is also logged.
    serial::init();
    let framebuffer = console::init(boot_info.framebuffer.take());
    let fb_info = framebuffer;

    let physical_memory_offset = boot_info.physical_memory_offset.into_option();

    // Without a linear framebuffer, fall back to the firmware's VGA text mode so
    // the kernel still has an on-screen console (colours are limited to the
    // default light-gray-on-black, but text, scrolling and clearing all work).
    if fb_info.is_none() {
        if let Some(offset) = physical_memory_offset {
            vga::init(offset);
        }
    }

    // Everything up to the heap initialization is allocation-free: the banner
    // and the boot log use `format_args!`, never `format!`.
    shell::banner();
    println!();
    println!("kernel 0.1.0  |  ring 0  |  no_std  |  built with rustc nightly");
    println!();

    match fb_info {
        Some(info) => {
            let (columns, rows) = console::dimensions().unwrap_or((0, 0));
            shell::status(
                true,
                format_args!(
                    "framebuffer {}x{} px, {:?}, {} bpp, stride {} ({columns}x{rows} cells)",
                    info.width, info.height, info.pixel_format, info.bytes_per_pixel, info.stride
                ),
            );
        }
        None if vga::is_initialized() => shell::status(
            true,
            format_args!("no linear framebuffer, using the VGA text console"),
        ),
        None => shell::status(
            false,
            format_args!("no framebuffer found, serial output only"),
        ),
    }

    shell::status(
        true,
        format_args!(
            "kernel image at 0x{:012x} ({} KiB), stack {} KiB",
            boot_info.kernel_addr,
            boot_info.kernel_len / 1024,
            boot_info.kernel_stack_len / 1024
        ),
    );

    let Some(physical_memory_offset) = physical_memory_offset else {
        shell::status(false, format_args!("no physical memory mapping"));
        println!("         check BootloaderConfig.mappings.physical_memory");
        halt_forever();
    };

    match memory::init(
        &boot_info.memory_regions,
        (boot_info.kernel_addr, boot_info.kernel_len),
        physical_memory_offset,
    ) {
        Ok(()) => {
            let (start, size) = memory::info();
            shell::status(
                true,
                format_args!(
                    "heap: 0x{start:012x} .. 0x{:012x} ({} MiB)",
                    start + size,
                    size / (1024 * 1024)
                ),
            );
        }
        Err(error) => {
            shell::status(false, format_args!("heap init failed"));
            println!("         {error}");
            halt_forever();
        }
    }

    // The GDT and TSS must be loaded before the IDT names the double-fault IST
    // stack, so a stack-overflow double fault has somewhere to go.
    usermode::init_gdt();

    // Start the async runtime before the keyboard IRQ is unmasked, so the handler
    // always finds an initialised scancode queue.
    task::init();

    interrupts::init();
    mouse::init();
    shell::status(
        true,
        format_args!(
            "IDT, PIC and PIT loaded; timer at {} Hz, keyboard + COM1 IRQs live",
            interrupts::TICK_HZ
        ),
    );

    init_usermode(physical_memory_offset);
    init_network(physical_memory_offset);
    init_audio(physical_memory_offset);
    init_disks();

    // Expose the boot state through the Redox-style `sys:` scheme. `file:` and
    // `net:` need no setup beyond the mounted disk and initialised NIC above.
    scheme::init(boot_info);
    shell::status(
        true,
        format_args!("scheme namespace ready (file:/ net:/ sys:/)"),
    );

    // If a framebuffer is available, launch the Windows XP-style desktop. It
    // runs until a byte arrives on COM1, which hands the console to the serial
    // shell (so a headless test can still drive the shell); without a
    // framebuffer the shell starts straight away.
    if let Some(info) = fb_info {
        wm::init(info.width, info.height, boot_info);
        wm::run();
    }
    shell::run(boot_info)
}

/// Sets up ring-3 user mode: a GDT with user segments, a TSS for the ring-0
/// stack, and the `int 0x80` system-call gate.
fn init_usermode(physical_memory_offset: u64) {
    usermode::init(physical_memory_offset);
    shell::status(
        true,
        format_args!(
            "ring 3 ready: user window at {:#018x}, int 0x80 syscalls enabled",
            usermode::USER_BASE
        ),
    );
}

/// Brings up the RTL8139 adapter and its IPv4 stack, then asks for a DHCP lease.
///
/// Supported adapters are the RTL8139 and the Intel 82540EM (`VirtualBox`).
/// Both run a DHCP server on their NAT network, but the boot continues on the
/// static fallback addresses if none answers.
fn init_network(physical_memory_offset: u64) {
    if !net::init(physical_memory_offset) {
        shell::status(
            false,
            format_args!("no supported NIC found (need an RTL8139 or an E1000)"),
        );
        return;
    }

    if let Some(info) = net::adapter() {
        shell::status(
            true,
            format_args!("{} NIC up: {}, IRQ {}", info.name, info.bus, info.irq),
        );
    }

    net::dhcp();
    let config = net::address();
    shell::status(
        true,
        format_args!(
            "IPv4 {}.{}.{}.{} mask {}.{}.{}.{}",
            config.ip[0],
            config.ip[1],
            config.ip[2],
            config.ip[3],
            config.netmask[0],
            config.netmask[1],
            config.netmask[2],
            config.netmask[3],
        ),
    );
    shell::status(
        true,
        format_args!(
            "gateway {}.{}.{}.{} dns {}.{}.{}.{} ({})",
            config.gateway[0],
            config.gateway[1],
            config.gateway[2],
            config.gateway[3],
            config.dns[0],
            config.dns[1],
            config.dns[2],
            config.dns[3],
            if config.dhcp {
                "DHCP lease"
            } else {
                "static fallback"
            },
        ),
    );
}

/// Brings up AC'97 audio.
///
/// Without an AC'97 controller the playback commands report that no device is
/// present.
fn init_audio(physical_memory_offset: u64) {
    if audio::init(physical_memory_offset) {
        shell::status(
            true,
            format_args!("AC'97 audio up: {} Hz, 16-bit stereo", audio::SAMPLE_RATE),
        );
    } else {
        shell::status(
            false,
            format_args!("no AC'97 audio device found (boot with -device AC97,audiodev=snd0)"),
        );
    }
}

/// Detects the ATA drives and mounts the FAT16 data disk.
///
/// The boot disk is the primary master (drive 0) and the `GTos` data disk is the
/// primary slave (drive 1). The kernel only mounts a volume it finds on the
/// slave, so a plain boot disk is left untouched.
fn init_disks() {
    let found = ata::init();
    if found == 0 {
        shell::status(false, format_args!("no ATA drives detected"));
        return;
    }
    shell::status(
        true,
        format_args!("{found} ATA drive(s) on the primary IDE channel"),
    );
    for index in 0..ata::DRIVES {
        if !ata::present(index) {
            continue;
        }
        let model = ata::model(index).unwrap_or_default();
        shell::status(
            true,
            format_args!(
                "ATA drive {index} ({}): {} MiB {model}",
                if index == 0 { "master" } else { "slave" },
                ata::sectors(index) / 2048
            ),
        );
    }

    let data = 1;
    if ata::present(data) && fat::mount_data_disk(data) {
        let summary = fat::with_volume(fat::Volume::summary).unwrap_or_default();
        shell::status(
            true,
            format_args!("FAT16 volume on drive {data}: {summary}"),
        );
    } else {
        shell::status(
            false,
            format_args!("no FAT16 volume on drive {data} (data disk not attached)"),
        );
    }
}

/// Disables interrupts and halts the CPU forever.
pub fn halt_forever() -> ! {
    loop {
        x86_64::instructions::interrupts::disable();
        x86_64::instructions::hlt();
    }
}

/// The kernel panic handler. Referenced by the binary's `#[panic_handler]`.
pub fn panic_handler(info: &core::panic::PanicInfo<'_>) -> ! {
    console::panic_print(format_args!("\n*** KERNEL PANIC ***\n{info}\n"));
    halt_forever()
}

/// The kernel allocation-error handler. Referenced by the binary's
/// `#[alloc_error_handler]`.
pub fn alloc_error_handler(layout: core::alloc::Layout) -> ! {
    console::panic_print(format_args!(
        "\n*** KERNEL PANIC ***\nheap allocation failed: {layout:?}\n"
    ));
    halt_forever()
}

/// Forwards formatted output to COM1.
///
/// The [`serial_print!`] macros call this crate-root function so that the serial
/// module itself can stay private.
#[doc(hidden)]
pub fn serial_print_args(args: core::fmt::Arguments<'_>) {
    serial::print_args(args);
}
