//! In-QEMU boot test for the `examples/minimal` kernel.
//!
//! The host unit tests in `src/vga.rs` drive the writer against a RAM buffer and
//! cover the scrolling, wrapping, colour and cursor logic. This test covers the
//! other half: that the *real* kernel boots, reaches physical memory and drives
//! the device path without faulting. Run it with
//! `scripts\minimal.ps1 -Test` (or `cargo qtest` from the crate directory).
//!
//! Under QEMU the bootloader always switches the display into linear-framebuffer
//! graphics mode, so the legacy text aperture at `0xb8000` is not backed: a
//! write is accepted but a read-back is meaningless. The test therefore asserts
//! only what is observable there — initialization, out-of-range reads and the
//! tracked colour — and leaves the cell contents to the host unit tests.
//!
//! It also covers post-05's IDT: a breakpoint raised with `int3` must be handled
//! and must return (see [`rustos_minimal::testing::breakpoint_round_trip`]).

#![no_std]
#![no_main]
// This test fails by calling `exit_qemu` rather than by panicking, and the
// pedantic `missing_panics_doc` lint does not apply to a test entry point.
#![allow(clippy::missing_panics_doc)]

use core::panic::PanicInfo;

use bootloader_api::config::Mapping;
use bootloader_api::{BootInfo, BootloaderConfig, entry_point};
use rustos_minimal::testing::{QemuExitCode, exit_qemu, init_serial};
use rustos_minimal::{console, println, vga};

/// The test needs the physical-memory window to reach `0xb8000`.
static TEST_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(test_kernel_main, config = &TEST_CONFIG);

fn test_kernel_main(boot_info: &'static mut BootInfo) -> ! {
    init_serial();
    println!("running the minimal in-QEMU test");

    let Some(offset) = boot_info.physical_memory_offset.into_option() else {
        println!("[failed] the bootloader did not map physical memory");
        exit_qemu(QemuExitCode::Failed);
    };

    vga::init(offset);
    check("the writer initializes", vga::is_initialized());

    // Exercising the volatile device writes must not fault.
    vga::write_str("RustOS");
    vga::set_fg(vga::Color::LightGreen);
    check(
        "the colour is tracked",
        vga::color().foreground() == vga::Color::LightGreen,
    );

    // Out-of-range reads short-circuit to a blank cell.
    check(
        "out-of-range reads are blank",
        vga::read_screen_char(vga::HEIGHT, 0).ascii == b' ',
    );

    // The console path (COM1 + VGA) must stay usable, and filling more than
    // HEIGHT lines must scroll without faulting.
    console::clear_screen();
    console::set_fg(vga::Color::LightGray);
    for line in 0..(vga::HEIGHT + 2) {
        println!("  scroll line {line:02}");
    }

    // post-05: the IDT path. This installs a breakpoint handler and raises
    // `int3`; the handler must run exactly once and execution must continue.
    check(
        "a breakpoint is handled and returns",
        rustos_minimal::testing::breakpoint_round_trip() == 1,
    );

    println!("[ok]");
    exit_qemu(QemuExitCode::Success);
}

/// Reports one named check, failing the run if it did not hold.
fn check(name: &str, ok: bool) {
    if ok {
        println!("  {name} ... [ok]");
    } else {
        println!("  {name} ... [failed]");
        exit_qemu(QemuExitCode::Failed);
    }
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    println!("[failed]\nError: {info}");
    exit_qemu(QemuExitCode::Failed)
}
