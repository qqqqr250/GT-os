//! In-QEMU tests for the post-12 async hook in `rustos-cpu-exceptions`.
//!
//! The crate is post-05, so its focus is the IDT, but it also carries post-07's
//! PIC/PIT/keyboard, post-08's paging, post-10's heap and post-12's task runtime
//! forward. This test drives the
//! [`task`](rustos_cpu_exceptions::task) module's *public* API: `cargo test`
//! links this file for `x86_64-unknown-none`, the runner in `runner/` wraps it in
//! a bootable disk image, and QEMU reports pass/fail through the `isa-debug-exit`
//! device.
//!
//! Run it with `scripts\cpu-exceptions.ps1 -Test` (or `cargo qtest` from the
//! crate directory). The entry point goes through [`entry_point!`] so the test
//! ELF carries the `.bootloader-config` section the bootloader needs; its config
//! turns on the physical-memory window, which is what the paging layer — and
//! therefore the heap and the task queues — needs.

#![no_std]
#![no_main]
// Building the mapper, mapping the heap and installing a global allocator are all
// inherently `unsafe`.
#![allow(unsafe_code)]
// Test functions fail by panicking, so the pedantic `missing_panics_doc` lint
// does not apply to them.
#![allow(clippy::missing_panics_doc)]
#![feature(custom_test_frameworks)]
#![feature(alloc_error_handler)]
#![test_runner(rustos_cpu_exceptions::test_runner)]
#![reexport_test_harness_main = "test_main"]

// The tasks allocate through the global allocator, so the test needs the `alloc`
// crate linked.
extern crate alloc;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use bootloader_api::config::Mapping;
use bootloader_api::{BootInfo, BootloaderConfig, entry_point};
use futures_util::StreamExt;
use rustos_cpu_exceptions::task::Task;
use rustos_cpu_exceptions::task::executor::Executor;
use rustos_cpu_exceptions::task::keyboard::{self, ScancodeStream};
use rustos_cpu_exceptions::task::simple_executor::SimpleExecutor;
use rustos_cpu_exceptions::{allocator, halt_forever, interrupts, memory, serial};

static TEST_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(test_kernel_main, config = &TEST_CONFIG);

fn test_kernel_main(boot_info: &'static mut BootInfo) -> ! {
    serial::init();
    // Install the IDT, remap the PIC, start the PIT and enable interrupts, so the
    // timer is already ticking when the harness runs.
    interrupts::init();

    let offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("the test config must map physical memory");
    // SAFETY: the bootloader mapped all physical memory at `offset`, and this is
    // the only mapper created in this binary.
    unsafe { memory::init_paging(offset, &boot_info.memory_regions) };
    // The task runtime allocates, so build the heap before the harness runs.
    allocator::init().expect("heap initialization failed");

    test_main();
    halt_forever()
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    rustos_cpu_exceptions::test_panic_handler(info)
}

#[alloc_error_handler]
fn alloc_error(layout: core::alloc::Layout) -> ! {
    rustos_cpu_exceptions::alloc_error_handler(layout)
}

#[test_case]
fn the_simple_executor_runs_a_task() {
    static RAN: AtomicBool = AtomicBool::new(false);

    let mut executor = SimpleExecutor::new();
    executor.spawn(Task::new(async {
        RAN.store(true, Ordering::Relaxed);
    }));
    executor.run();
    assert!(RAN.load(Ordering::Relaxed));
}

#[test_case]
fn the_executor_runs_a_ready_task() {
    static RAN: AtomicBool = AtomicBool::new(false);

    let mut executor = Executor::new();
    executor.spawn(Task::new(async {
        RAN.store(true, Ordering::Relaxed);
    }));
    executor.run_ready_tasks();
    assert!(RAN.load(Ordering::Relaxed));
}

#[test_case]
fn the_scancode_stream_yields_pushed_scancodes() {
    static SEEN: AtomicU8 = AtomicU8::new(0);

    let mut executor = Executor::new();
    executor.spawn(Task::new(async {
        // This is the only `ScancodeStream` in the binary, as its constructor
        // requires: it reads one scancode and records it.
        let mut scancodes = ScancodeStream::new();
        if let Some(scancode) = scancodes.next().await {
            SEEN.store(scancode, Ordering::Relaxed);
        }
    }));

    // First poll: the queue is empty, so the task registers its waker and parks.
    executor.run_ready_tasks();
    assert_eq!(SEEN.load(Ordering::Relaxed), 0, "nothing pushed yet");

    // The interrupt path pushes a raw scancode and wakes the task, which then
    // reads it out of the queue.
    keyboard::add_scancode(0x1e);
    executor.run_ready_tasks();
    assert_eq!(SEEN.load(Ordering::Relaxed), 0x1e);
}
