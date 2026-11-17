//! `RustOS` kernel library.
//!
//! The crate is split into this library plus a thin binary (`src/main.rs`) so
//! that the kernel logic can be exercised by an in-QEMU test framework: the
//! integration tests under `kernel/tests` link against this library and drive it
//! from their own entry point (see the README's *Testing* section).
//!
//! The binary owns the bootloader entry point and the panic/alloc handlers; this
//! library owns everything else.

#![no_std]
// A test binary needs no `main`; the harness provides `test_main` instead.
#![cfg_attr(test, no_main)]
#![feature(abi_x86_interrupt)]
#![cfg_attr(test, feature(alloc_error_handler))]
#![feature(custom_test_frameworks)]
#![test_runner(crate::test_runner)]
#![reexport_test_harness_main = "test_main"]
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
mod font;
mod input;
mod interrupts;
mod keyboard;
mod memory;
mod mouse;
mod net;
mod pci;
mod serial;
mod shell;
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
/// QEMU attaches the NIC for `-nic user,model=rtl8139` and runs a DHCP server
/// on its user-mode network; without either the boot simply continues on the
/// static fallback addresses.
fn init_network(physical_memory_offset: u64) {
    if !net::init(physical_memory_offset) {
        shell::status(
            false,
            format_args!("no RTL8139 NIC found (boot with -nic user,model=rtl8139)"),
        );
        return;
    }

    let (io, irq) = net::adapter().unwrap_or((0, 0));
    shell::status(
        true,
        format_args!("RTL8139 NIC up: I/O 0x{io:04x}, IRQ {irq}"),
    );

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
/// QEMU attaches the controller for `-device AC97,audiodev=snd0`; without one
/// the playback commands report that no device is present.
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
/// QEMU attaches the boot disk as the primary master (drive 0) and the `RustOS`
/// data disk as the primary slave (drive 1). The kernel only mounts a volume it
/// finds on the slave, so a plain boot disk is left untouched.
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
                "ATA drive {index}: {} ({} MiB) {model}",
                ata::sectors(index) / 2048,
                if index == 0 { "master" } else { "slave" }
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

// ---------------------------------------------------------------------------
// In-QEMU test framework
// ---------------------------------------------------------------------------
//
// `cargo test` cannot run a bare-metal binary natively, so the tests run inside
// QEMU: `cargo test --manifest-path kernel\Cargo.toml --target x86_64-unknown-none`
// links a test binary,
// the runner in `runner/` wraps it in a bootable disk image, and QEMU reports
// the result through the `isa-debug-exit` device. A test binary panics on
// failure (which exits with the failure code) and calls `exit_qemu` on success.

/// A single test case, as collected by the test harness.
pub trait Testable {
    /// Runs the test.
    fn run(&self);
}

impl<T: Fn()> Testable for T {
    fn run(&self) {
        serial_print!("{}...\t", core::any::type_name::<T>());
        self();
        serial_println!("[ok]");
    }
}

/// Runs every collected `#[test_case]`, then exits QEMU with a success code.
pub fn test_runner(tests: &[&dyn Testable]) {
    serial_println!("running {} test(s)", tests.len());
    for test in tests {
        test.run();
    }
    exit_qemu(QemuExitCode::Success);
}

/// Panic handler for test binaries: report the panic, then fail the QEMU run.
pub fn test_panic_handler(info: &core::panic::PanicInfo<'_>) -> ! {
    serial_println!("[failed]");
    serial_println!("Error: {}", info);
    exit_qemu(QemuExitCode::Failed);
}

/// Exit codes understood by the QEMU `isa-debug-exit` device.
///
/// QEMU reports `(value << 1) | 1` to the host, so `Success` (0x10) becomes the
/// process exit code 33 and `Failed` (0x11) becomes 35.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum QemuExitCode {
    /// Every test passed.
    Success = 0x10,
    /// A test failed.
    Failed = 0x11,
}

/// Powers the machine off through the `isa-debug-exit` device.
///
/// The runner always passes `-device isa-debug-exit,iobase=0xf4,iosize=0x04`.
pub fn exit_qemu(exit_code: QemuExitCode) -> ! {
    // SAFETY: port 0xf4 belongs to the `isa-debug-exit` device the test runner
    // attaches; writing it terminates the emulator with `(code << 1) | 1`.
    unsafe {
        x86_64::instructions::port::Port::<u32>::new(0xf4).write(exit_code as u32);
    }
    halt_forever()
}

/// Entry point for this library's own unit-test binary (`cargo test --lib`).
///
/// The [`entry_point!`](bootloader_api::entry_point) macro is used rather than a
/// hand-written `_start` because it also emits the `.bootloader-config` section
/// the bootloader reads when loading the kernel: without that section the test
/// disk image does not boot. The configuration mirrors the binary's so the tests
/// can reach the physical memory window. The integration tests under
/// `kernel/tests` provide their own entry point.
#[cfg(test)]
mod test_entry {
    use bootloader_api::config::Mapping;
    use bootloader_api::{BootInfo, BootloaderConfig, entry_point};

    /// Test-kernel bootloader configuration; the physical-memory window is
    /// needed to reach the VGA text buffer at `0xb8000`.
    pub static TEST_CONFIG: BootloaderConfig = {
        let mut config = BootloaderConfig::new_default();
        config.mappings.physical_memory = Some(Mapping::Dynamic);
        config
    };

    entry_point!(test_kernel_main, config = &TEST_CONFIG);

    fn test_kernel_main(_boot_info: &'static mut BootInfo) -> ! {
        crate::serial::init();
        crate::test_main();
        crate::halt_forever()
    }
}

#[cfg(test)]
#[panic_handler]
fn test_panic(info: &core::panic::PanicInfo<'_>) -> ! {
    test_panic_handler(info)
}

#[cfg(test)]
#[alloc_error_handler]
fn test_alloc_error(layout: core::alloc::Layout) -> ! {
    serial_println!("allocation failed: {:?}", layout);
    exit_qemu(QemuExitCode::Failed);
}

/// Helpers exposed to the in-QEMU integration tests under `kernel/tests`.
///
/// The kernel proper keeps its modules private; this small surface is the only
/// thing a test binary needs from the library.
pub mod testing {
    use bootloader_api::info::MemoryRegions;

    pub use crate::usermode::DOUBLE_FAULT_IST_INDEX;
    pub use crate::vga::{Color, ColorCode, ScreenChar};

    /// Initializes COM1 so test output reaches the host.
    pub fn init_serial() {
        crate::serial::init();
    }

    /// Installs the GDT and TSS, exactly as `kernel_main` does early on.
    ///
    /// The double-fault IDT entry names [`DOUBLE_FAULT_IST_INDEX`], which only
    /// resolves once the TSS is loaded, so this must run before [`init_idt`].
    /// It is idempotent, so calling it alongside [`init_idt`] is harmless.
    pub fn init_gdt() {
        crate::usermode::init_gdt();
    }

    /// The top of the double-fault stack the TSS names, or 0 before
    /// [`init_gdt`].
    #[must_use]
    pub fn double_fault_stack_top() -> u64 {
        crate::usermode::double_fault_stack_top()
    }

    /// Installs the IDT and starts the PIC/PIT, exactly as the kernel does.
    ///
    /// The breakpoint test needs the exception handlers to be in place first.
    /// The timer, keyboard and serial IRQs this also unmasks are harmless during
    /// a test run: no key or serial byte arrives, and the timer handler only
    /// counts.
    ///
    /// [`init_gdt`] runs first so the double-fault handler has a valid IST
    /// stack.
    pub fn init_idt() {
        crate::usermode::init_gdt();
        crate::interrupts::init();
    }

    /// Builds the paging mapper and frame allocator the post-08 tests use.
    ///
    /// The real kernel never calls this: `kernel_main` hands the mapping work to
    /// [`crate::usermode`], and two live mappers over the active table would alias
    /// `&mut` references (see `memory`'s module docs). The test boot path does not
    /// enter user mode, so creating the mapper here is sound.
    ///
    /// `physical_memory_offset` is `BootInfo::physical_memory_offset`, and
    /// `memory_regions` is `BootInfo::memory_regions`.
    pub fn init_memory(physical_memory_offset: u64, memory_regions: &'static MemoryRegions) {
        // SAFETY: the bootloader mapped all physical memory at
        // `physical_memory_offset`, and this is the only mapper the test binary
        // creates.
        unsafe {
            crate::memory::init_mapper(x86_64::VirtAddr::new(physical_memory_offset));
        }
        // SAFETY: the `Usable` regions of the bootloader's memory map are unused:
        // it marks its own allocations, the kernel image and the kernel stack as
        // `Bootloader`.
        unsafe {
            crate::memory::init_frame_allocator(memory_regions);
        }
    }

    /// Initializes the kernel heap, exactly as `kernel_main` does.
    ///
    /// `kernel` is `(BootInfo::kernel_addr, BootInfo::kernel_len)`: the heap is
    /// taken from the top of the largest usable region that does not overlap the
    /// kernel image, so it cannot collide with the kernel or the bootloader's
    /// data structures. Call this before any test allocates.
    ///
    /// # Errors
    ///
    /// Returns an error if no usable region is large enough.
    pub fn init_heap(
        memory_regions: &'static MemoryRegions,
        kernel: (u64, u64),
        physical_memory_offset: u64,
    ) -> Result<(), &'static str> {
        crate::memory::init(memory_regions, kernel, physical_memory_offset)
    }

    /// `(start, size)` of the kernel heap, or `(0, 0)` before [`init_heap`].
    #[must_use]
    pub fn heap_info() -> (u64, u64) {
        crate::memory::info()
    }

    /// The allocator design the kernel installs as its global allocator.
    ///
    /// This is post-11's fixed-size block design (see [`crate::allocator`]), the
    /// one the post settles on.
    #[must_use]
    pub fn allocator_design() -> &'static str {
        crate::memory::ALLOCATOR_DESIGN
    }

    /// Translates `address` by walking the four-level table by hand.
    ///
    /// [`init_memory`] must have run first. Compare the result with
    /// [`mapper_translate_addr`] to prove the mapper and the walk agree.
    #[must_use]
    pub fn translate_addr(address: u64) -> Option<u64> {
        let offset = x86_64::VirtAddr::new(crate::memory::physical_memory_offset());
        crate::memory::translate_addr(x86_64::VirtAddr::new(address), offset)
            .map(x86_64::PhysAddr::as_u64)
    }

    /// Translates `address` through the active mapper, or `None` if it is not
    /// mapped.
    ///
    /// [`init_memory`] must have run first.
    #[must_use]
    pub fn mapper_translate_addr(address: u64) -> Option<u64> {
        crate::memory::mapper_translate_addr(address)
    }

    /// Allocates one 4 KiB frame from the bootloader's memory map and returns its
    /// physical start address.
    ///
    /// [`init_memory`] must have run first.
    #[must_use]
    pub fn allocate_frame() -> Option<u64> {
        crate::memory::allocate_frame().map(|frame| frame.start_address().as_u64())
    }

    /// Maps the page containing `virt` to the frame containing `phys`.
    ///
    /// # Safety
    ///
    /// `virt` must not already be mapped to a different frame, and the caller
    /// must treat the physical frame as owned by the mapping.
    ///
    /// # Errors
    ///
    /// Returns an error if paging is not initialized or if `map_to` fails.
    pub unsafe fn map_page(virt: u64, phys: u64, writable: bool) -> Result<(), &'static str> {
        // SAFETY: forwarded to the caller.
        unsafe { crate::memory::map_page(virt, phys, writable) }
    }

    /// Raises a breakpoint exception (`int3`).
    ///
    /// [`init_idt`] must have run first, otherwise this triple-faults.
    pub fn trigger_breakpoint() {
        x86_64::instructions::interrupts::int3();
    }

    /// Number of breakpoint (`#BP`) exceptions handled since boot.
    #[must_use]
    pub fn breakpoint_count() -> u64 {
        crate::interrupts::breakpoints()
    }

    /// Number of timer interrupts since boot.
    #[must_use]
    pub fn ticks() -> u64 {
        crate::interrupts::ticks()
    }

    /// Turns the timer handler's dot printing on or off.
    ///
    /// The handler prints through the same serial lock the console does, so a
    /// test that turns this on and prints at the same time exercises the post-07
    /// deadlock fix.
    pub fn set_timer_dots(enabled: bool) {
        crate::interrupts::set_timer_dots(enabled);
    }

    /// Prints one line through the kernel's normal print path.
    ///
    /// The console is not initialized in these tests, so the line reaches the
    /// host through the serial mirror — the same lock the timer handler takes.
    pub fn console_println(text: &str) {
        crate::console::print_args(format_args!("{text}\n"));
    }

    /// Recurses until the kernel stack hits the guard page the bootloader placed
    /// below it.
    ///
    /// The page fault cannot be delivered on the exhausted stack, so the CPU
    /// raises a double fault and switches to the IST stack named by
    /// [`DOUBLE_FAULT_IST_INDEX`] — the post-06 behaviour. Returns only if the
    /// guard page was somehow never hit.
    pub fn overflow_stack() {
        #[allow(unconditional_recursion)]
        fn recurse() {
            // A local whose address is taken, so every call really reserves a
            // frame.
            let marker = 0u8;
            recurse();
            // A volatile read the optimizer may not remove: it keeps the
            // recursive call above from being rewritten as a loop (tail-call
            // elimination), so the stack keeps growing instead of staying flat.
            // SAFETY: reading a live local through a raw pointer is always sound.
            unsafe { core::ptr::read_volatile(&raw const marker) };
        }

        recurse();
    }

    /// Points the VGA text fallback at `physical_memory_offset`.
    pub fn init_vga(physical_memory_offset: u64) {
        crate::vga::init(physical_memory_offset);
    }

    /// Whether the VGA text fallback has been initialized.
    #[must_use]
    pub fn vga_is_initialized() -> bool {
        crate::vga::is_initialized()
    }

    /// Writes `text` to the VGA text buffer.
    pub fn vga_write(text: &str) {
        crate::vga::write_str(text);
    }

    /// Clears the VGA text buffer.
    pub fn vga_clear() {
        crate::vga::clear();
    }

    /// Sets the VGA text foreground colour.
    pub fn vga_set_fg(color: Color) {
        crate::vga::set_fg(color);
    }

    /// The VGA text colour the console renders `rgb` (`0xRRGGBB`) as.
    ///
    /// Named console colours keep their meaning (the success green stays green);
    /// any other colour is snapped to the nearest palette entry.
    #[must_use]
    pub fn vga_color_for_rgb(rgb: u32) -> Color {
        crate::console::Color(rgb).to_vga_text()
    }

    /// The VGA text colour applied to new characters.
    #[must_use]
    pub fn vga_color() -> ColorCode {
        crate::vga::color()
    }

    /// Reads one VGA cell back, for assertions.
    #[must_use]
    pub fn vga_screen_char(row: usize, column: usize) -> ScreenChar {
        crate::vga::read_screen_char(row, column)
    }

    /// Halts the CPU forever.
    pub fn halt() -> ! {
        crate::halt_forever()
    }
}
