//! `blog_os` post-08, done properly: a minimal bare-metal kernel that turns on
//! paging and maps memory through an [`OffsetPageTable`](x86_64::structures::paging::OffsetPageTable).
//!
//! Post-08 is the *Introduction to Paging* post: the CPU has been using page
//! tables all along (the bootloader set them up), and the post's only code change
//! is to read `CR3` and print where the active level-4 table lives. The paging
//! *implementation* — walking the four-level table, translating an address and
//! mapping a fresh page through an offset page table with a frame allocator over
//! the bootloader's memory map — is the next post. This crate carries both, so it
//! is the full paging treatment rather than only the `CR3` peek.
//!
//! It is cumulative: everything post-07 built is still here, and [`memory`] is
//! added on top.
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
//! - [`memory`] adds the paging API: [`memory::active_level_4_frame`] (post-08),
//!   a hand-written [`memory::translate_addr`] walk, and the real
//!   [`memory::init_paging`] / [`memory::map_page`] path (post-09),
//! - as a forward-looking hook, [`allocator`] brings forward post-10 and post-11:
//!   it maps a heap through the paging layer and installs a `#[global_allocator]`,
//!   so `Box`, `Vec` and `Rc` work, and it implements the allocator with the three
//!   designs post-11 builds ([`allocator::bump`], [`allocator::linked_list`],
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
//! Everything runs on COM1: post-08's own output goes to the VGA text buffer,
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

/// A free virtual page the bootable demo maps to the VGA buffer.
///
/// `PML4` index 144, far above the bootloader's dynamic window (which starts at
/// index 1), so the walk has to create new page tables and the frame allocator is
/// really exercised.
const DEMO_PAGE: u64 = 0x0000_4800_0000_0000;

/// The kernel entry point, called by the bootloader (see `src/main.rs`).
pub fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
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

    // Post-08: the bootloader already enabled paging and mapped all physical
    // memory at an offset. Read `CR3` to see where the active level-4 table is,
    // then build the mapper the rest of the kernel uses.
    crate::println!();
    match boot_info.physical_memory_offset.into_option() {
        Some(offset) => {
            crate::println!("physical memory is mapped at {offset:#018x}");
            crate::println!(
                "the active level-4 table lives at {:#018x}",
                memory::active_level_4_frame()
            );
            // SAFETY: the bootloader mapped all physical memory at `offset`, and
            // this is the only mapper the kernel creates.
            unsafe { memory::init_paging(offset, &boot_info.memory_regions) };

            // The hand-written walk and the mapper must agree. A function pointer
            // is in the kernel image, which the bootloader maps with 4 KiB pages.
            let probe = memory::active_level_4_frame as *const () as u64;
            let by_hand =
                memory::translate_addr(x86_64::VirtAddr::new(probe), x86_64::VirtAddr::new(offset));
            crate::println!(
                "translate_addr({probe:#018x}) = {by_hand:?}; the mapper says {:#?}",
                memory::mapper_translate_addr(probe)
            );

            // Map a fresh page to a frame from the bootloader's memory map and
            // prove the round-trip: write through the page, read the same value
            // back. (The VGA buffer is device memory and does not read back.)
            match memory::allocate_frame() {
                Some(frame) => {
                    let phys = frame.start_address().as_u64();
                    // SAFETY: `DEMO_PAGE` is unused and `frame` was just handed
                    // out, so nothing else aliases it.
                    match unsafe { memory::map_page(DEMO_PAGE, phys, true) } {
                        Ok(()) => {
                            // SAFETY: `DEMO_PAGE` was just mapped writable.
                            unsafe {
                                let cell = DEMO_PAGE as *mut u16;
                                cell.write_volatile(0x1234);
                                crate::println!(
                                    "mapped {DEMO_PAGE:#018x} -> {phys:#018x} and read back {:#06x}",
                                    cell.read_volatile()
                                );
                            }
                        }
                        Err(error) => crate::println!("map_page failed: {error}"),
                    }
                }
                None => crate::println!("the memory map has no usable frames"),
            }
        }
        None => crate::println!("the bootloader did not map physical memory"),
    }

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
    use super::memory;
    use super::{pic, pit};

    use x86_64::structures::paging::mapper::MapToError;
    use x86_64::structures::paging::{Mapper, Page, PageTableFlags, PhysFrame, Size4KiB};
    use x86_64::{PhysAddr, VirtAddr};

    /// A free virtual page whose `PML4` entry is unused, so mapping it has to
    /// allocate new page tables.
    const EMPTY_PAGE: u64 = 0x0000_4000_0000_0000;
    /// Another free page, in a different `PML4` entry from [`EMPTY_PAGE`], so the
    /// two tests cannot share page tables.
    const MAPPED_PAGE: u64 = 0x0000_5000_0000_0000;
    /// A third free page that no test maps.
    const UNMAPPED_PAGE: u64 = 0x0000_6000_0000_0000;

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

    #[test_case]
    fn the_bootloader_mapped_the_physical_memory_window() {
        // Virtual `offset + 0` is physical frame 0, so the window is present and
        // the mapper is initialized.
        let offset = memory::physical_memory_offset();
        assert_ne!(offset, 0, "the test config must map physical memory");
        assert_eq!(memory::mapper_translate_addr(offset), Some(0));
    }

    #[test_case]
    fn the_active_level_4_table_is_where_cr3_points() {
        let frame = memory::active_level_4_frame();
        assert_ne!(frame.as_u64(), 0);
        assert_eq!(
            frame.as_u64() % memory::FRAME_SIZE,
            0,
            "frames are 4 KiB aligned"
        );
        // The table is reachable through the window, and translates to itself.
        let offset = memory::physical_memory_offset();
        assert_eq!(
            memory::mapper_translate_addr(offset + frame.as_u64()),
            Some(frame.as_u64())
        );
    }

    #[test_case]
    fn the_hand_written_walk_agrees_with_the_mapper() {
        // A function pointer is in the kernel image, which the bootloader maps
        // with 4 KiB pages, so both walks must find the same frame.
        let address = memory::active_level_4_frame as *const () as u64;
        let offset = VirtAddr::new(memory::physical_memory_offset());
        let by_hand = memory::translate_addr(VirtAddr::new(address), offset);
        assert!(by_hand.is_some(), "the kernel image must be mapped");
        assert_eq!(
            by_hand.map(PhysAddr::as_u64),
            memory::mapper_translate_addr(address)
        );
    }

    #[test_case]
    fn an_unmapped_high_page_has_no_translation() {
        assert_eq!(memory::mapper_translate_addr(UNMAPPED_PAGE), None);
    }

    #[test_case]
    fn mapping_a_new_page_needs_a_frame_allocator() {
        // A fresh `PML4` entry means the walk must allocate new page tables. The
        // empty allocator cannot, so `map_to` fails instead of silently
        // corrupting the address space.
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(EMPTY_PAGE));
        let frame = PhysFrame::containing_address(PhysAddr::new(memory::VGA_BUFFER));
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        let result = memory::with_mapper(|mapper| {
            // SAFETY: `EMPTY_PAGE` is unused, so nothing is being remapped.
            match unsafe { mapper.map_to(page, frame, flags, &mut memory::EmptyFrameAllocator) } {
                Ok(flush) => {
                    flush.flush();
                    Ok(())
                }
                Err(error) => Err(error),
            }
        });
        assert!(matches!(
            result,
            Some(Err(MapToError::FrameAllocationFailed))
        ));
    }

    #[test_case]
    fn mapping_a_page_with_the_boot_info_allocator_succeeds() {
        // Take a free frame from the bootloader's memory map, then map it into a
        // fresh page. (The VGA buffer is device memory and does not read back, so
        // a usable frame is the honest target for a round-trip.)
        let frame = memory::allocate_frame().expect("the memory map has usable frames");
        let phys = frame.start_address().as_u64();
        // SAFETY: `MAPPED_PAGE` is unused and `frame` was just handed out, so
        // nothing else aliases it.
        let result = unsafe { memory::map_page(MAPPED_PAGE, phys, true) };
        assert!(result.is_ok(), "map_page failed: {result:?}");
        assert_eq!(memory::mapper_translate_addr(MAPPED_PAGE), Some(phys));

        // The mapping is real: writing through the virtual page changes the
        // physical frame it was mapped to.
        // SAFETY: `MAPPED_PAGE` was just mapped writable.
        unsafe {
            let cell = MAPPED_PAGE as *mut u16;
            cell.write_volatile(0x1234);
            assert_eq!(cell.read_volatile(), 0x1234);
        }
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

    /// The paging tests need the physical-memory window, so the test config turns
    /// it on — exactly like the bootable binary's configuration in `src/main.rs`.
    static TEST_CONFIG: BootloaderConfig = {
        let mut config = BootloaderConfig::new_default();
        config.mappings.physical_memory = Some(Mapping::Dynamic);
        config
    };

    entry_point!(test_kernel_main, config = &TEST_CONFIG);

    fn test_kernel_main(boot_info: &'static mut BootInfo) -> ! {
        crate::serial::init();
        // Install the GDT/TSS and the IDT/PIC/PIT before the harness runs: the
        // timer test needs the tick to be live and the GDT test needs the TSS.
        crate::init();
        // Then build the single mapper the paging tests share.
        let offset = boot_info
            .physical_memory_offset
            .into_option()
            .expect("the test config must map physical memory");
        // SAFETY: the bootloader mapped all physical memory at `offset`, and this
        // is the only mapper created in this binary.
        unsafe { crate::memory::init_paging(offset, &boot_info.memory_regions) };
        // The post-10 hook: build the heap so any test that allocates works.
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
