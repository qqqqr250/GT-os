//! `blog_os` post-05, done properly: a minimal bare-metal kernel that installs an
//! interrupt descriptor table and handles CPU exceptions.
//!
//! Post-05 is the *CPU Exceptions* post. Until now a CPU exception was a triple
//! fault: the IDT was never set up, so the CPU had nowhere to go. The tutorial
//! builds a `lazy_static` IDT with a single breakpoint handler that prints the
//! stack frame, plus a `test_breakpoint_exception` that fires `int3`. This crate
//! keeps that shape but rebuilds it as something you would ship:
//!
//! - the IDT lives in a [`spin::Mutex`] built in place
//!   (`InterruptDescriptorTable::new()` is a `const fn`) instead of a
//!   `lazy_static`, so there is no hidden allocation and the load path is
//!   explicit,
//! - all six self-generated exceptions post-05's IDT leaves out — divide error,
//!   invalid opcode, breakpoint, double fault, general protection fault and page
//!   fault — get handlers, and the fatal ones halt the CPU after reporting,
//! - the handlers *record* what they saw ([`interrupts::last_vector`] and
//!   [`interrupts::exception_count`]) instead of only printing, so the test can
//!   assert that the handler really ran,
//! - the test framework from post-04 is reused verbatim, so `cargo test` here
//!   means "boot this kernel inside QEMU and run the tests",
//! - as a forward-looking hook, [`interrupts::init`] also brings forward post-07:
//!   it remaps the 8259 PIC, starts the PIT and unmasks the timer, so a test can
//!   watch [`interrupts::ticks`] advance. The full post-07 treatment lives in
//!   `examples/hardware-interrupts`,
//! - as a further hook, [`memory`] brings forward post-08: the `CR3` read, the
//!   hand-written table walk, the `OffsetPageTable` mapper and a frame allocator
//!   over the bootloader's memory map. The full post-08 treatment lives in
//!   `examples/paging`,
//! - as a final hook, [`allocator`] brings forward post-10 and post-11: it maps a
//!   heap through the paging layer and installs a `#[global_allocator]`, so `Box`,
//!   `Vec` and `Rc` work, and it implements the allocator with the three designs
//!   post-11 builds ([`allocator::bump`], [`allocator::linked_list`],
//!   [`allocator::fixed_size_block`]), installing the last. The full treatment
//!   lives in `examples/allocator-designs`.
//!
//! Everything runs on COM1: post-05's own output goes to the VGA text buffer,
//! which a headless `cargo test` cannot read. See the crate `README.md`.

#![no_std]
// A test binary has no `main`; the harness provides `test_main` instead.
#![cfg_attr(test, no_main)]
// The library's own test binary installs an `#[alloc_error_handler]` so a failed
// allocation fails the test run instead of aborting silently.
#![cfg_attr(test, feature(alloc_error_handler))]
#![feature(custom_test_frameworks)]
// The exception handlers are `extern "x86-interrupt"`, which is what lets the
// compiler emit the `iretq`-style epilogue the CPU expects.
#![feature(abi_x86_interrupt)]
#![test_runner(crate::test_runner)]
#![reexport_test_harness_main = "test_main"]
// A freestanding kernel cannot avoid `unsafe`: port I/O and loading the IDT need
// it. Every `unsafe` block carries a SAFETY comment, and the port accesses are
// confined to `port` and `serial`.
#![allow(unsafe_code)]
// Vector numbers and packed exit codes are converted with `as`, where the
// truncation or wrap-around is intended and checked by construction.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

// The heap is what the post-10/11 hook is about: `alloc` provides `Box`, `Vec`,
// `Rc` and the `GlobalAlloc` trait the [`allocator`] module's designs implement.
extern crate alloc;

pub mod allocator;
pub mod interrupts;
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
/// Post-05 keeps this separate from the kernel's `print!` because its kernel
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

/// The kernel entry point, called by the bootloader (see `src/main.rs`).
pub fn kernel_main(_boot_info: &'static mut BootInfo) -> ! {
    serial::init();

    crate::println!("installing the interrupt descriptor table");
    interrupts::init();

    // post-05's `_start` greets the world; this kernel instead demonstrates the
    // one thing an IDT buys you: a CPU exception that is handled, not a triple
    // fault. A breakpoint is recoverable, so execution continues afterwards.
    crate::println!("raising a breakpoint to prove the handler runs");
    interrupts::trigger_breakpoint();

    match interrupts::last_vector() {
        Some(vector) => crate::println!(
            "the {} handler ran; {} exception(s) handled so far",
            vector.name(),
            interrupts::exception_count(),
        ),
        None => crate::println!("no exception was recorded - the handler did not run"),
    }

    crate::println!("done. halting the CPU.");
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
// In-QEMU test framework (post-04, reused by post-05)
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
// this crate, because the crate is `no_std` even under `cfg(test)`. The IDT is
// loaded by the test entry point below, so the breakpoint test has a handler.
#[cfg(test)]
mod tests {
    use super::QemuExitCode;
    use super::interrupts::{self, Vector};

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
    fn every_handled_vector_round_trips_through_its_number() {
        let handled = [
            Vector::DivideError,
            Vector::Breakpoint,
            Vector::InvalidOpcode,
            Vector::DoubleFault,
            Vector::GeneralProtectionFault,
            Vector::PageFault,
        ];
        for vector in handled {
            assert_eq!(Vector::from_u8(vector as u8), Some(vector));
            assert_ne!(vector.name(), "");
        }
        // Vectors this kernel does not handle are reported as such.
        assert_eq!(Vector::from_u8(0x20), None);
        assert_eq!(Vector::from_u8(0xff), None);
    }

    #[test_case]
    fn the_breakpoint_handler_records_the_exception() {
        let before = interrupts::exception_count();
        interrupts::trigger_breakpoint();
        assert_eq!(
            interrupts::exception_count(),
            before + 1,
            "the breakpoint handler should have run exactly once"
        );
        assert_eq!(interrupts::last_vector(), Some(Vector::Breakpoint));
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
        // Install the handlers before the harness runs: the breakpoint test needs
        // them, and a fault without them would triple-fault instead of failing
        // the run.
        crate::interrupts::init();
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
