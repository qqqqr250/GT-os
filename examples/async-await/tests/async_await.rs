//! In-QEMU tests for the `rustos-async-await` kernel.
//!
//! These drive the crate's *public* API from a separate test binary: `cargo test`
//! links this file for `x86_64-unknown-none`, the runner in `runner/` wraps it in
//! a bootable disk image, and QEMU reports pass/fail through the `isa-debug-exit`
//! device.
//!
//! Two suites live here. The first is post-11's heap suite — `simple_allocation`,
//! `large_vec`, `many_boxes` and `many_boxes_long_lived` — which proves the
//! allocator designs are still in place. The second is post-12's: it spawns tasks
//! on both executors and drives the async keyboard stream by pushing a scancode
//! the way the interrupt handler does.
//!
//! Run it with `scripts\async-await.ps1 -Test` (or `cargo qtest` from the crate
//! directory). The entry point goes through [`entry_point!`] so the test ELF
//! carries the `.bootloader-config` section the bootloader needs; its config turns
//! on the physical-memory window, which is what the paging layer — and therefore
//! the heap and the task queue — needs.

#![no_std]
#![no_main]
// Building the mapper, mapping pages and installing a global allocator are all
// inherently `unsafe`.
#![allow(unsafe_code)]
// Test functions fail by panicking, so the pedantic `missing_panics_doc` lint
// does not apply to them.
#![allow(clippy::missing_panics_doc)]
#![feature(custom_test_frameworks)]
#![feature(alloc_error_handler)]
#![test_runner(rustos_async_await::test_runner)]
#![reexport_test_harness_main = "test_main"]

// The tests allocate through the global allocator, so they need the `alloc`
// crate's `Box` and `Vec`.
extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use bootloader_api::config::Mapping;
use bootloader_api::{BootInfo, BootloaderConfig, entry_point};
use futures_util::StreamExt;
use rustos_async_await::task::Task;
use rustos_async_await::task::executor::Executor;
use rustos_async_await::task::keyboard::{self, ScancodeStream};
use rustos_async_await::task::simple_executor::SimpleExecutor;
use rustos_async_await::{allocator, halt_forever, memory, serial};

static TEST_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(test_kernel_main, config = &TEST_CONFIG);

fn test_kernel_main(boot_info: &'static mut BootInfo) -> ! {
    serial::init();
    // The GDT/TSS must be loaded before the IDT names the double-fault IST slot,
    // and `interrupts::init` remaps the PIC, starts the PIT and enables
    // interrupts. `init` does both in the right order.
    rustos_async_await::init();

    let offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("the test config must map physical memory");
    // SAFETY: the bootloader mapped all physical memory at `offset`, and this is
    // the only mapper created in this binary.
    unsafe { memory::init_paging(offset, &boot_info.memory_regions) };
    allocator::init().expect("heap initialization failed");

    test_main();
    halt_forever()
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    rustos_async_await::test_panic_handler(info)
}

#[alloc_error_handler]
fn alloc_error(layout: core::alloc::Layout) -> ! {
    rustos_async_await::alloc_error_handler(layout)
}

// ---------------------------------------------------------------------------
// post-11: the allocator designs
// ---------------------------------------------------------------------------

#[test_case]
fn simple_allocation() {
    // Two boxes: the values must survive their trip through the heap.
    let heap_value_1 = Box::new(41);
    let heap_value_2 = Box::new(13);
    assert_eq!(*heap_value_1, 41);
    assert_eq!(*heap_value_2, 13);
}

#[test_case]
fn large_vec() {
    // A `Vec` grows by reallocating, so this exercises both `alloc` and `dealloc`.
    let n = 1000;
    let mut vec = Vec::new();
    for i in 0..n {
        vec.push(i);
    }
    assert_eq!(vec.iter().sum::<u64>(), (n - 1) * n / 2);
}

#[test_case]
fn many_boxes() {
    // Allocate and free many values in turn. Each `Box` is dropped at the end of
    // its iteration, so the allocator reuses the memory it just freed; a heap
    // smaller than the total number of allocations is enough.
    for i in 0..allocator::HEAP_SIZE {
        let x = Box::new(i);
        assert_eq!(*x, i);
    }
}

#[test_case]
fn many_boxes_long_lived() {
    // The test that separates the designs. One allocation stays live for the
    // whole loop, so a bump allocator's counter never returns to zero and it
    // exhausts the heap; the linked-list and fixed-size block allocators reuse
    // every freed block and serve the whole loop from a 100 KiB heap.
    let long_lived = Box::new(1);
    for i in 0..allocator::HEAP_SIZE {
        let x = Box::new(i);
        assert_eq!(*x, i);
    }
    assert_eq!(*long_lived, 1);
}

// ---------------------------------------------------------------------------
// post-12: tasks, executors and the async keyboard
// ---------------------------------------------------------------------------

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
