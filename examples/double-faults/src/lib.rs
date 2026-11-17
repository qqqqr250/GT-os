//! `blog_os` post-06, done properly: a minimal bare-metal kernel that catches
//! double faults on a dedicated stack.
//!
//! Post-06 is the *Double Faults* post. A double fault is what the CPU raises
//! when it cannot invoke the handler for another exception; if that is left
//! unhandled, the machine triple-faults and resets. Post-05's plain double-fault
//! handler catches *most* of them, but not the one that matters most: a kernel
//! stack overflow. There the stack pointer already points at the guard page, so
//! the CPU cannot push the exception frame, and the double fault never reaches a
//! handler.
//!
//! Post-06 fixes that with hardware stack switching. This crate keeps the shape
//! of the tutorial — a double-fault handler plus a GDT, a TSS and an interrupt
//! stack table — but rebuilds it as something you would ship:
//!
//! - the GDT lives in a [`spin::Mutex`] built in place
//!   (`GlobalDescriptorTable::new()` is a `const fn`) instead of a `lazy_static`,
//! - the TSS, the double-fault stack and the GDT are `static`s, so the load path
//!   is explicit and there is no hidden allocation,
//! - the double-fault handler is pinned to its own IST stack
//!   ([`gdt::DOUBLE_FAULT_IST_INDEX`]), which is what makes a stack-overflow
//!   double fault survivable,
//! - the data-segment registers are reloaded from the new GDT as well as `cs`
//!   (post-06 only reloads `cs`), so no register is left naming a stale
//!   descriptor,
//! - [`overflow_stack`] recurses until the guard page is hit, and
//!   `tests/stack_overflow.rs` asserts that the double fault is caught,
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
//!   lives in `examples/allocator-designs`,
//! - as a final hook, [`sync`] and [`task`] bring forward post-12: a hand-written
//!   bounded lock-free queue ([`sync::ArrayQueue`]) and one-shot initialiser
//!   ([`sync::OnceCell`]), and on top of them [`task::Task`], the naive
//!   [`task::simple_executor::SimpleExecutor`], the waker-based
//!   [`task::executor::Executor`] and the async keyboard
//!   ([`task::keyboard::ScancodeStream`]). The keyboard IRQ now only pushes the
//!   raw scancode and wakes the decoder task. The full post-12 treatment lives in
//!   `examples/async-await`.
//!
//! Everything runs on COM1: post-06's own output goes to the VGA text buffer,
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
// A freestanding kernel cannot avoid `unsafe`: port I/O, loading descriptor
// tables and reading a `static mut` TSS need it. Every `unsafe` block carries a
// SAFETY comment, and the port accesses are confined to `port` and `serial`.
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
pub mod gdt;
pub mod interrupts;
pub mod keyboard;
pub mod memory;
pub mod pic;
pub mod pit;
pub mod serial;
pub mod sync;
pub mod task;

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
/// Post-06 keeps this separate from the kernel's `print!` because its kernel
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

/// Installs the GDT, the TSS and the interrupt stack table, then the IDT.
///
/// The order matters: the double-fault IDT entry names an IST slot, and the CPU
/// can only resolve it once the TSS is loaded.
pub fn init() {
    let _ = gdt::init();
    interrupts::init();
}

/// Recurses until the kernel stack hits the guard page the bootloader placed
/// below it.
///
/// The page fault cannot be delivered on the exhausted stack, so the CPU raises
/// a double fault and switches to the IST stack — which is the whole point of
/// post-06. Returns only if the guard page was somehow never hit.
pub fn overflow_stack() {
    #[allow(unconditional_recursion)]
    fn recurse() {
        // A local whose address is taken, so every call really reserves a frame.
        let marker = 0u8;
        recurse();
        // A volatile read the optimizer may not remove: it keeps the recursive
        // call above from being rewritten as a loop (tail-call elimination), so
        // the stack keeps growing instead of staying flat.
        // SAFETY: reading a live local through a raw pointer is always sound.
        unsafe { core::ptr::read_volatile(&raw const marker) };
    }

    recurse();
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
    interrupts::init();

    crate::println!();
    crate::println!("overflowing the kernel stack to prove the double fault is caught:");
    crate::println!("the guard page below the stack faults, the page fault cannot be");
    crate::println!("delivered there, and the CPU switches to the IST stack to handle the");
    crate::println!("double fault instead of resetting the machine.");
    crate::println!();
    overflow_stack();

    // Unreachable: the double-fault handler halts the CPU.
    crate::println!("the guard page was never hit - the stack did not overflow");
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
// In-QEMU test framework (post-04, reused by post-05 and post-06)
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
// this crate, because the crate is `no_std` even under `cfg(test)`. The GDT and
// the IDT are installed by the test entry point below, so the tests can assert
// on them; the heap is built there too, so the post-12 sync and task tests can
// allocate.
#[cfg(test)]
mod tests {
    use super::QemuExitCode;
    use super::gdt;
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

    #[test_case]
    fn the_gdt_installs_the_expected_selectors() {
        // The null descriptor is index 0, so the three we append land at 0x08,
        // 0x10 and 0x18.
        let selectors = gdt::selectors().expect("gdt::init should have run");
        assert_eq!(selectors.code.0, 0x08);
        assert_eq!(selectors.data.0, 0x10);
        assert_eq!(selectors.tss.0, 0x18);
    }

    #[test_case]
    fn the_double_fault_handler_has_its_own_stack() {
        // A zero top would mean the TSS names no IST stack, and a stack-overflow
        // double fault would triple-fault.
        assert_ne!(
            gdt::double_fault_stack_top(),
            0,
            "the TSS should name a dedicated double-fault stack"
        );
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
    use bootloader_api::config::Mapping;
    use bootloader_api::{BootInfo, BootloaderConfig, entry_point};

    /// The post-12 sync/task tests allocate through the global allocator, so the
    /// test config turns on the physical-memory window the heap is built on —
    /// exactly like the bootable binary's configuration in `src/main.rs`.
    static TEST_CONFIG: BootloaderConfig = {
        let mut config = BootloaderConfig::new_default();
        config.mappings.physical_memory = Some(Mapping::Dynamic);
        config
    };

    entry_point!(test_kernel_main, config = &TEST_CONFIG);

    fn test_kernel_main(boot_info: &'static mut BootInfo) -> ! {
        crate::serial::init();
        // Install the GDT/TSS and the IDT before the harness runs: the breakpoint
        // test needs the handlers, and the GDT test needs the TSS.
        crate::init();
        // Then build the single mapper the paging tests share.
        let offset = boot_info
            .physical_memory_offset
            .into_option()
            .expect("the test config must map physical memory");
        // SAFETY: the bootloader mapped all physical memory at `offset`, and this
        // is the only mapper created in this binary.
        unsafe { crate::memory::init_paging(offset, &boot_info.memory_regions) };
        // The post-12 queues allocate, so build the heap before the harness runs.
        crate::allocator::init().expect("heap initialization failed");
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
