//! In-QEMU tests for the `rustos-hardware-interrupts` kernel.
//!
//! These drive the crate's *public* API from a separate test binary: `cargo test`
//! links this file for `x86_64-unknown-none`, the runner in `runner/` wraps it in
//! a bootable disk image, and QEMU reports pass/fail through the
//! `isa-debug-exit` device.
//!
//! Run it with `scripts\hardware-interrupts.ps1 -Test` (or `cargo qtest` from the
//! crate directory). The entry point goes through [`entry_point!`] so the test
//! ELF carries the `.bootloader-config` section the bootloader needs.

#![no_std]
#![no_main]
// Test functions fail by panicking, so the pedantic `missing_panics_doc` lint
// does not apply to them.
#![allow(clippy::missing_panics_doc)]
#![feature(custom_test_frameworks)]
#![test_runner(rustos_hardware_interrupts::test_runner)]
#![reexport_test_harness_main = "test_main"]

use core::panic::PanicInfo;

use bootloader_api::{BootInfo, BootloaderConfig, entry_point};
use rustos_hardware_interrupts::{
    gdt, halt_forever, interrupts, keyboard, pic, pit, serial, serial_println,
};

static TEST_CONFIG: BootloaderConfig = BootloaderConfig::new_default();

entry_point!(test_kernel_main, config = &TEST_CONFIG);

fn test_kernel_main(_boot_info: &'static mut BootInfo) -> ! {
    serial::init();
    // The GDT/TSS must be loaded before the IDT names the double-fault IST slot,
    // and `interrupts::init` is what remaps the PIC, starts the PIT and enables
    // interrupts, so the timer is already ticking when the harness runs.
    let _ = gdt::init();
    interrupts::init();
    test_main();
    halt_forever()
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    rustos_hardware_interrupts::test_panic_handler(info)
}

#[test_case]
fn the_timer_interrupt_advances_the_tick_count() {
    // `hlt` is only left by an interrupt, so this both waits for ticks and
    // proves the timer really is interrupting the CPU.
    let start = interrupts::ticks();
    let deadline = start + 20; // ~200 ms at 100 Hz
    while interrupts::ticks() < deadline {
        x86_64::instructions::hlt();
    }
    assert!(
        interrupts::ticks() >= deadline,
        "the PIT should have produced at least {deadline} ticks"
    );
}

#[test_case]
fn the_pic_is_remapped_above_the_exception_vectors() {
    // Out of reset IRQ0 is vector 8, which the IDT uses for the double fault.
    const { assert!(pic::PIC1_OFFSET > 0x1f) };
    assert_eq!(interrupts::TIMER_VECTOR, 0x20);
    assert_eq!(interrupts::KEYBOARD_VECTOR, 0x21);
}

#[test_case]
fn the_pit_runs_at_the_configured_frequency() {
    assert_eq!(pit::divisor(pit::TICK_HZ), 11_931);
}

#[test_case]
fn the_keyboard_decoder_handles_modifiers() {
    keyboard::reset();

    // 'a' (0x1e) press and release: only the press yields a byte.
    keyboard::handle_scancode(0x1e);
    keyboard::handle_scancode(0x9e);
    assert_eq!(keyboard::pop(), Some(b'a'));
    assert_eq!(keyboard::pop(), None);

    // Shift (0x2a) held: 'a' becomes 'A'; releasing shift restores 'a'.
    keyboard::handle_scancode(0x2a);
    keyboard::handle_scancode(0x1e);
    keyboard::handle_scancode(0xaa); // left-shift release
    keyboard::handle_scancode(0x1e);
    assert_eq!(keyboard::pop(), Some(b'A'));
    assert_eq!(keyboard::pop(), Some(b'a'));
    assert_eq!(keyboard::pop(), None);

    // An extended key is delivered as a control code, not ASCII.
    keyboard::handle_scancode(0xe0);
    keyboard::handle_scancode(0x48); // up arrow
    assert_eq!(keyboard::pop(), Some(keyboard::KEY_UP));
}

#[test_case]
fn printing_while_the_timer_interrupts_does_not_deadlock() {
    // This is post-07's deadlock, made real: with dots on, the timer handler
    // prints through the same serial lock this loop holds. Without
    // `without_interrupts` around the lock the handler would spin on it forever
    // and the runner would time out; with it, the main thread holds the lock
    // with interrupts off, so the handler simply cannot run mid-print.
    interrupts::set_timer_dots(true);
    for line in 0..100 {
        serial_println!("line {}", line);
    }
    interrupts::set_timer_dots(false);

    // Reaching here is the pass condition; a deadlock would never get this far.
    assert!(interrupts::ticks() > 0, "the timer should still be ticking");
}
