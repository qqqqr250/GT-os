//! `blog_os` post-04, done properly: a minimal bare-metal kernel that carries its
//! own in-QEMU test framework.
//!
//! Post-04 is the *Testing* post. A bare-metal binary cannot run under the host's
//! test harness, so the tutorial builds a tiny one of its own: the
//! `custom_test_frameworks` feature, a `test_runner` that collects `#[test_case]`
//! functions, a `Testable` trait, `serial_print!`/`serial_println!`, a
//! `QemuExitCode` and an `exit_qemu` that powers the emulator off through the
//! `isa-debug-exit` device. This crate keeps that whole surface but rebuilds it as
//! something you would ship:
//!
//! - the test entry point goes through [`entry_point!`](bootloader_api::entry_point),
//!   so the test ELF carries the `.bootloader-config` section the bootloader needs
//!   in order to load it (post-04 relied on `bootimage` to inject it),
//! - `exit_qemu` goes through the [`port`] abstraction instead of a bare port
//!   write, and `QemuExitCode`'s encoding is asserted by a test,
//! - the `serial_print!` macros reach a private [`serial`] module through one
//!   `#[doc(hidden)]` function, so no UART details leak into the macros,
//! - the runner (`runner/`) adds a timeout and maps QEMU's exit code back onto
//!   `cargo test`, which is what makes `cargo test` here mean "run inside QEMU".
//!
//! Everything runs on COM1. The crate has no VGA writer on purpose: the point of
//! post-04 is the framework, not the display, and serial is the only console a
//! headless `cargo test` can read. See the crate `README.md`.

#![no_std]
// A test binary has no `main`; the harness provides `test_main` instead.
#![cfg_attr(test, no_main)]
#![feature(custom_test_frameworks)]
#![test_runner(crate::test_runner)]
#![reexport_test_harness_main = "test_main"]
// A freestanding kernel cannot avoid `unsafe`: port I/O needs it. Every `unsafe`
// block carries a SAFETY comment, and the port accesses are confined to `port`
// and `serial`.
#![allow(unsafe_code)]
// Packed colour channels and exit codes are converted with `as`, where the
// truncation or wrap-around is intended and checked by construction.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

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
/// Post-04 keeps this separate from the kernel's `print!` because its kernel
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

    // post-04's `_start`, verbatim: greet the world.
    crate::println!("Hello World{}", "!");

    crate::println!("this kernel has no VGA writer; COM1 is the console.");
    crate::println!("run its tests with: cargo test (from examples\\testing)");

    halt_forever()
}

/// The kernel panic handler, referenced by the binary's `#[panic_handler]`.
pub fn panic_handler(info: &PanicInfo<'_>) -> ! {
    crate::println!("\n*** PANIC ***\n{info}");
    halt_forever()
}

// ---------------------------------------------------------------------------
// In-QEMU test framework (post-04)
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

// The library's own unit tests (`cargo test --lib`). They run inside QEMU like
// every other test in this crate, because the crate is `no_std` even under
// `cfg(test)`.
#[cfg(test)]
mod tests {
    use super::QemuExitCode;

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
    fn serial_output_is_live() {
        serial_println!("this line is written from inside the kernel's test binary");
    }
}

/// Entry point for this library's own unit-test binary.
///
/// The [`entry_point!`](bootloader_api::entry_point) macro is used rather than a
/// hand-written `_start` because it also emits the `.bootloader-config` section
/// the bootloader reads when loading the kernel: without that section the test
/// disk image does not boot. The integration tests under `tests` provide their
/// own entry point.
#[cfg(test)]
mod test_entry {
    use bootloader_api::{BootInfo, BootloaderConfig, entry_point};

    static TEST_CONFIG: BootloaderConfig = BootloaderConfig::new_default();

    entry_point!(test_kernel_main, config = &TEST_CONFIG);

    fn test_kernel_main(_boot_info: &'static mut BootInfo) -> ! {
        crate::serial::init();
        crate::test_main();
        crate::halt_forever()
    }
}

#[cfg(test)]
#[panic_handler]
fn test_panic(info: &PanicInfo<'_>) -> ! {
    test_panic_handler(info)
}
