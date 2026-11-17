//! In-QEMU stack-overflow test for `rustos-double-faults`.
//!
//! This is post-06's `tests/stack_overflow.rs`: the test recurses until the
//! guard page below the kernel stack is hit and asserts that the *double fault*
//! handler runs on the dedicated IST stack, instead of the machine triple
//! faulting. It runs without the normal harness, because a double fault cannot
//! return: the test passes or fails by exiting QEMU.
//!
//! `cargo test` links this file for `x86_64-unknown-none`, the runner in
//! `runner/` wraps it in a bootable disk image, and QEMU reports pass/fail
//! through the `isa-debug-exit` device. Run it with
//! `scripts\double-faults.ps1 -Test` (or `cargo qtest` from the crate
//! directory). The entry point goes through [`entry_point!`] so the test ELF
//! carries the `.bootloader-config` section the bootloader needs.

#![no_std]
#![no_main]
// This test fails by exiting QEMU, and the pedantic `missing_panics_doc` lint
// does not apply to a test entry point.
#![allow(clippy::missing_panics_doc)]
// The test installs its own double-fault handler with `extern "x86-interrupt"`.
#![feature(abi_x86_interrupt)]
// Installing an IST-pinned handler and loading a descriptor table is inherently
// unsafe; both call sites carry a SAFETY comment.
#![allow(unsafe_code)]

use core::panic::PanicInfo;

use bootloader_api::{BootInfo, BootloaderConfig, entry_point};
use spin::Mutex;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};

use rustos_double_faults::gdt::{self, DOUBLE_FAULT_IST_INDEX};
use rustos_double_faults::{QemuExitCode, exit_qemu, overflow_stack, serial, serial_println};

/// The tests need the physical-memory window, exactly like the real kernel, so
/// that the boot path is the same.
static TEST_CONFIG: BootloaderConfig = BootloaderConfig::new_default();

entry_point!(test_kernel_main, config = &TEST_CONFIG);

fn test_kernel_main(_boot_info: &'static mut BootInfo) -> ! {
    serial::init();
    serial_println!("stack_overflow::stack_overflow...\t");

    // The GDT/TSS/IST must be installed before the test IDT, because the
    // double-fault entry points at an IST slot that only exists once the TSS is
    // loaded.
    let _ = gdt::init();
    init_test_idt();

    // This never returns: the recursion hits the guard page below the kernel
    // stack, the page fault cannot be delivered there, and the CPU raises a
    // double fault, which switches to the IST stack and reaches our handler.
    overflow_stack();

    // Reached only if the guard page was never hit, which means the test failed.
    serial_println!("[failed]");
    serial_println!("Error: execution continued after the stack overflow");
    exit_qemu(QemuExitCode::Failed);
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    rustos_double_faults::test_panic_handler(info)
}

/// The test's own IDT: a double fault handler that passes the test instead of
/// halting the CPU.
static TEST_IDT: Mutex<InterruptDescriptorTable> = Mutex::new(InterruptDescriptorTable::new());

/// Points the double fault at the IST stack and loads the table.
fn init_test_idt() {
    let mut idt = TEST_IDT.lock();
    // SAFETY: `gdt::init` has loaded the TSS, so the IST slot is valid and no
    // other handler uses it.
    unsafe {
        idt.double_fault
            .set_handler_fn(test_double_fault_handler)
            .set_stack_index(DOUBLE_FAULT_IST_INDEX);
    }
    // SAFETY: `TEST_IDT` lives in a `static` and is never moved, so the address
    // the CPU caches in the IDTR stays valid. `load` would need a `&'static`
    // reference the mutex guard cannot hand out.
    unsafe { idt.load_unsafe() };
}

/// Runs on the IST stack, so it is safe to call even though the kernel stack is
/// exhausted. Reaching it at all is the test passing.
extern "x86-interrupt" fn test_double_fault_handler(
    _frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    serial_println!("[ok]");
    exit_qemu(QemuExitCode::Success);
}
