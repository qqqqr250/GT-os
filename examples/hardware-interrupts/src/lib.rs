//! `blog_os` post-07, done properly: a minimal bare-metal kernel driven by
//! hardware interrupts.
//!
//! Post-07 is the *Hardware Interrupts* post. Until now the CPU only took the
//! exceptions it raised itself; the devices were silent. Post-07 wires the 8259
//! PIC up so the timer and the keyboard can interrupt the CPU, programs the PIT,
//! and — the part that is easy to get wrong — fixes the *deadlock* that appears
//! the moment an interrupt handler prints through the same lock as the main
//! thread.
//!
//! This crate keeps the shape of the tutorial but rebuilds it as something you
//! would ship:
//!
//! - the PIC is remapped and masked by hand ([`pic`]) rather than through the
//!   `pic8259` crate, and the PIT divisor is a pure, unit-tested function
//!   ([`pit::divisor`]),
//! - [`serial::print_args`] takes the UART lock inside `without_interrupts`,
//!   which is post-07's deadlock fix: the timer handler prints through the same
//!   lock, so the lock has to be held with interrupts off,
//! - the keyboard decoder ([`keyboard`]) is a pure state machine, so Shift, Caps
//!   Lock and the `0xE0` prefix are unit tested without a keyboard,
//! - the timer handler counts ticks ([`interrupts::ticks`]) and, when a test
//!   enables it, prints a dot, which is what makes the deadlock test real,
//! - the GDT/TSS/IST from post-06 is kept, so the double-fault handler still runs
//!   on its own stack,
//! - as a forward-looking hook, [`memory`] brings forward post-08: the `CR3` read,
//!   the hand-written table walk, the `OffsetPageTable` mapper and a frame
//!   allocator over the bootloader's memory map. The full post-08 treatment lives
//!   in `examples/paging`,
//! - as a final hook, [`allocator`] brings forward post-10: it maps a
//!   `linked_list_allocator` heap through the paging layer and installs a
//!   `#[global_allocator]`, so `Box`, `Vec` and `Rc` work. The full post-10
//!   treatment lives in `examples/heap-allocation`.
//!
//! Everything runs on COM1: post-07's own output goes to the VGA text buffer,
//! which a headless `cargo test` cannot read. See the crate `README.md`.

#![no_std]
// A test binary has no `main`; the harness provides `test_main` instead.
#![cfg_attr(test, no_main)]
// The library's own test binary installs an `#[alloc_error_handler]` so a failed
// allocation fails the test run instead of aborting silently.
#![cfg_attr(test, feature(alloc_error_handler))]
#![feature(custom_test_frameworks)]
// The handlers are `extern "x86-interrupt"`, which is what lets the compiler
// emit the `iretq`-style epilogue the CPU expects.
#![feature(abi_x86_interrupt)]
#![test_runner(crate::test_runner)]
#![reexport_test_harness_main = "test_main"]
// A freestanding kernel cannot avoid `unsafe`: port I/O, loading descriptor
// tables and reading a `static mut` TSS need it. Every `unsafe` block carries a
// SAFETY comment, and the port accesses are confined to `port`, `serial`, `pic`
// and `pit`.
#![allow(unsafe_code)]
// Vector numbers, packed exit codes and PIT divisors are converted with `as`,
// where the truncation or wrap-around is intended and checked by construction.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

// The heap is what the post-10 hook is about: `alloc` provides `Box`, `Vec`, `Rc`
// and the `GlobalAlloc` trait the [`allocator`] module implements.
extern crate alloc;

pub mod allocator;
pub mod gdt;
pub mod interrupts;
pub mod keyboard;
pub mod memory;
pub mod pic;
pub mod pit;
pub mod serial;

mod port;

use core::panic::PanicInfo;

pub use bootloader_api::BootInfo;

/// Like the standard library's `print!`, but writes to the kernel's console.
///
/// The console here is COM1 (see [`serial`]): it is the only one a headless
/// `cargo test` can read.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::serial::print_args(format_args!($($arg)*))
    };
}

/// Like the standard library's `println!`, but writes to the kernel's console.
#[macro_export]
macro_rules! println {
    () => {
        $crate::print!("\n")
    };
    ($($arg:tt)*) => {
        $crate::print!("{}\n", format_args!($($arg)*))
    };
}

/// Like the standard library's `print!`, but for test-framework output on COM1.
///
/// Post-07 keeps this separate from the kernel's `print!` because its kernel
/// prints to the VGA text buffer while the framework prints to serial. This crate
/// has only the serial console, so the two coincide — the macros stay distinct so
/// the framework's public API matches the tutorial.
#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => {
        $crate::serial::print_args(format_args!($($arg)*))
    };
}

/// Like the standard library's `println!`, but for test-framework output on COM1.
#[macro_export]
macro_rules! serial_println {
    () => {
        $crate::serial_print!("\n")
    };
    ($fmt:expr) => {
        $crate::serial_print!(concat!($fmt, "\n"))
    };
    ($fmt:expr, $($arg:tt)*) => {
        $crate::serial_print!(concat!($fmt, "\n"), $($arg)*)
    };
}

/// Stops the CPU until the next interrupt, forever.
pub fn halt_forever() -> ! {
    loop {
        // SAFETY: `hlt` is always legal in ring 0 and is only left by an
        // interrupt.
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Installs the GDT/TSS/IST, then the IDT, the PIC and the PIT.
///
/// The order matters: the double-fault IDT entry names an IST slot, and the CPU
/// can only resolve it once the TSS is loaded. [`interrupts::init`] enables
/// interrupts as its last step.
pub fn init() {
    let _ = gdt::init();
    interrupts::init();
}

/// The kernel entry point, called by the bootloader (see `src/main.rs`).
pub fn kernel_main(_boot_info: &'static mut BootInfo) -> ! {
    serial::init();

    crate::println!("installing the GDT, the TSS and the interrupt stack table");
    let _ = gdt::init();
    crate::println!(
        "installing the IDT (double fault -> IST slot {})",
        gdt::DOUBLE_FAULT_IST_INDEX
    );
    crate::println!(
        "remapping the PIC to vectors {:#04x} and {:#04x}",
        pic::PIC1_OFFSET,
        pic::PIC2_OFFSET
    );
    crate::println!(
        "programming the PIT to {} Hz (divisor {}) and enabling interrupts",
        pit::TICK_HZ,
        pit::divisor(pit::TICK_HZ)
    );
    interrupts::init();

    crate::println!();
    crate::println!(
        "the timer handler prints a dot every {} ticks:",
        interrupts::DOT_INTERVAL
    );
    interrupts::set_timer_dots(true);

    // Sleep in `hlt` until about two seconds of ticks have passed. `hlt` is only
    // left by an interrupt, so this is the post-07 idle loop.
    let start = interrupts::ticks();
    let deadline = start + 2 * u64::from(pit::TICK_HZ);
    while interrupts::ticks() < deadline {
        x86_64::instructions::hlt();
    }
    interrupts::set_timer_dots(false);

    crate::println!();
    crate::println!(
        "{} timer interrupts in about two seconds; halting.",
        interrupts::ticks() - start
    );
    halt_forever()
}

/// The kernel panic handler, referenced by the binary's `#[panic_handler]`.
pub fn panic_handler(info: &PanicInfo<'_>) -> ! {
    crate::println!("\n*** PANIC ***\n{info}");
    halt_forever()
}

/// The kernel allocation-error handler, referenced by the binary's
/// `#[alloc_error_handler]`.
pub fn alloc_error_handler(layout: core::alloc::Layout) -> ! {
    crate::println!("\n*** PANIC ***\nheap allocation failed: {layout:?}");
    halt_forever()
}

// ---------------------------------------------------------------------------
// In-QEMU test framework (post-04, reused by post-05, post-06 and post-07)
// ---------------------------------------------------------------------------
//
// `cargo test` cannot run a bare-metal binary natively, so the tests run inside
// QEMU: cargo links a test binary for `x86_64-unknown-none`, the runner in
// `runner/` wraps it in a bootable disk image, and QEMU reports the result
// through the `isa-debug-exit` device. A test binary panics on failure (which
// exits with the failure code) and calls `exit_qemu` on success.

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
pub fn test_panic_handler(info: &PanicInfo<'_>) -> ! {
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
        port::outl(port::DEBUG_EXIT, exit_code as u32);
    }
    halt_forever()
}

// The library's own unit tests. They run inside QEMU like every other test in
// this crate, because the crate is `no_std` even under `cfg(test)`. The GDT, the
// IDT, the PIC and the PIT are installed by the test entry point below, so the
// timer is already ticking when these run.
#[cfg(test)]
mod tests {
    use super::QemuExitCode;
    use super::interrupts;
    use super::keyboard;
    use super::{pic, pit};

    #[test_case]
    #[allow(clippy::eq_op)] // post-04's deliberately trivial assertion
    fn trivial_assertion() {
        assert_eq!(1, 1);
    }

    #[test_case]
    fn qemu_exit_codes_match_isa_debug_exit() {
        // QEMU reports `(value << 1) | 1`, so `Success` must be 0x10 for the
        // runner to see exit code 33.
        assert_eq!(QemuExitCode::Success as u32, 0x10);
        assert_eq!(QemuExitCode::Failed as u32, 0x11);
        assert_eq!(((QemuExitCode::Success as u32) << 1) | 1, 33);
    }

    #[test_case]
    fn the_pic_offsets_clear_the_cpu_exception_vectors() {
        // The CPU's own exceptions are vectors 0..=0x1f, and out of reset the PIC
        // would deliver IRQ0 on vector 8 (the double fault). The remap must move
        // every hardware IRQ above them.
        assert_eq!(pic::PIC1_OFFSET, 0x20);
        assert_eq!(pic::PIC2_OFFSET, 0x28);
        // A compile-time check, so the property cannot be lost by an edit.
        const { assert!(pic::PIC1_OFFSET > 0x1f) };
        assert_eq!(interrupts::TIMER_VECTOR, pic::PIC1_OFFSET);
        assert_eq!(interrupts::KEYBOARD_VECTOR, pic::PIC1_OFFSET + 1);
    }

    #[test_case]
    fn the_timer_and_keyboard_lines_are_unmasked() {
        // A set bit in `master_lines` is a line the kernel enables: IRQ0 (timer)
        // and IRQ1 (keyboard) must both be there.
        let lines = pic::master_lines();
        assert_eq!(lines & (1 << 0), 1 << 0, "IRQ0 (timer) must be unmasked");
        assert_eq!(lines & (1 << 1), 1 << 1, "IRQ1 (keyboard) must be unmasked");
    }

    #[test_case]
    fn the_pit_divisor_matches_the_tick_rate() {
        // 1.193182 MHz / 100 Hz = 11931.82 -> 11931.
        assert_eq!(pit::divisor(pit::TICK_HZ), 11_931);
        assert_eq!(pit::divisor(1000), 1_193);
    }

    #[test_case]
    fn the_timer_handler_counts_ticks() {
        let start = interrupts::ticks();
        let deadline = start + 10;
        while interrupts::ticks() < deadline {
            x86_64::instructions::hlt();
        }
        assert!(interrupts::ticks() >= deadline);
    }

    #[test_case]
    fn the_keyboard_decoder_maps_scancodes() {
        keyboard::reset();
        keyboard::handle_scancode(0x1e); // 'a' press
        keyboard::handle_scancode(0x9e); // 'a' release
        assert_eq!(keyboard::pop(), Some(b'a'));
        assert_eq!(keyboard::pop(), None, "a release must not produce a byte");
    }
}

/// Entry point for this library's own unit-test binary.
///
/// The [`entry_point!`](bootloader_api::entry_point) macro is used rather than a
/// hand-written `_start` because it also emits the `.bootloader-config` section
/// the bootloader reads when loading the kernel: without that section the test
/// disk image does not boot. The integration test under `tests` provides its own
/// entry point.
#[cfg(test)]
mod test_entry {
    use bootloader_api::{BootInfo, BootloaderConfig, entry_point};

    static TEST_CONFIG: BootloaderConfig = BootloaderConfig::new_default();

    entry_point!(test_kernel_main, config = &TEST_CONFIG);

    fn test_kernel_main(_boot_info: &'static mut BootInfo) -> ! {
        crate::serial::init();
        // Install the GDT/TSS and the IDT/PIC/PIT before the harness runs: the
        // timer test needs the tick to be live and the GDT test needs the TSS.
        crate::init();
        crate::test_main();
        crate::halt_forever()
    }
}

#[cfg(test)]
#[panic_handler]
fn test_panic(info: &PanicInfo<'_>) -> ! {
    test_panic_handler(info)
}

#[cfg(test)]
#[alloc_error_handler]
fn test_alloc_error(layout: core::alloc::Layout) -> ! {
    serial_println!("allocation failed: {:?}", layout);
    exit_qemu(QemuExitCode::Failed);
}
