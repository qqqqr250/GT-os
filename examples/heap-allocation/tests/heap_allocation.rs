//! In-QEMU tests for the `rustos-heap-allocation` kernel.
//!
//! These drive the crate's *public* API from a separate test binary: `cargo test`
//! links this file for `x86_64-unknown-none`, the runner in `runner/` wraps it in
//! a bootable disk image, and QEMU reports pass/fail through the `isa-debug-exit`
//! device.
//!
//! Run it with `scripts\heap-allocation.ps1 -Test` (or `cargo qtest` from the
//! crate directory). The entry point goes through [`entry_point!`] so the test ELF
//! carries the `.bootloader-config` section the bootloader needs; its config turns
//! on the physical-memory window, which is what the paging layer — and therefore
//! the heap — needs.

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
#![test_runner(rustos_heap_allocation::test_runner)]
#![reexport_test_harness_main = "test_main"]

// The tests allocate through the global allocator, so they need the `alloc`
// crate's `Box` and `Vec`.
extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::panic::PanicInfo;

use bootloader_api::config::Mapping;
use bootloader_api::{BootInfo, BootloaderConfig, entry_point};
use rustos_heap_allocation::{allocator, halt_forever, memory, serial};

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
    rustos_heap_allocation::init();

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
    rustos_heap_allocation::test_panic_handler(info)
}

#[alloc_error_handler]
fn alloc_error(layout: core::alloc::Layout) -> ! {
    rustos_heap_allocation::alloc_error_handler(layout)
}

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
    // The test that separates the allocator designs. One allocation stays live
    // for the whole loop, so a bump allocator's counter never returns to zero and
    // it exhausts the heap; the linked-list and fixed-size block allocators reuse
    // every freed block and serve the whole loop from a 100 KiB heap.
    let long_lived = Box::new(1);
    for i in 0..allocator::HEAP_SIZE {
        let x = Box::new(i);
        assert_eq!(*x, i);
    }
    assert_eq!(*long_lived, 1);
}
