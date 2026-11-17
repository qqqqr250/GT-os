# RustOS paging — `blog_os` post-08, done properly

This is a tiny, standalone crate that starts from
[`blog_os` post-08](https://github.com/phil-opp/blog_os/tree/post-08) — the
*Introduction to Paging* post, whose only code change is to read `CR3` and print
the active level-4 table's address — and rebuilds it, together with the paging
*implementation* from the following post, as something you would actually ship. It
is deliberately *not* part of the main RustOS workspace, so it can be built and
tested on its own.

It is the sibling of [`examples/minimal`](../minimal/README.md) (post-02),
[`examples/vga-text`](../vga-text/README.md) (post-03),
[`examples/testing`](../testing/README.md) (post-04),
[`examples/cpu-exceptions`](../cpu-exceptions/README.md) (post-05),
[`examples/double-faults`](../double-faults/README.md) (post-06),
[`examples/hardware-interrupts`](../hardware-interrupts/README.md) (post-07) and
[`examples/heap-allocation`](../heap-allocation/README.md) (post-10) and
[`examples/allocator-designs`](../allocator-designs/README.md) (post-11). It reuses
post-04's in-QEMU test framework, post-05's exception handling, post-06's
GDT/TSS/IST and post-07's PIC/PIT/keyboard, and adds the paging layer; as a
forward-looking hook it also carries post-10's heap interface and post-11's
allocator designs.

## What post-08 does

The CPU has been using page tables since the first instruction the bootloader
ran; post-08 does not *enable* paging, it makes the kernel *aware* of it:

1. **Read `CR3`.** The `CR3` control register points at the active level-4 (PML4)
   table. Post-08's whole code change is to read it and print the frame address.
2. **Find the physical-memory window.** The bootloader maps *all* physical memory
   at a fixed virtual offset, so a physical frame `p` is also visible at
   `offset + p`. That is what makes it possible to read page tables the CPU
   created.
3. **Walk the table.** Translate a virtual address by following the
   `p4`/`p3`/`p2`/`p1` indices and dereferencing each table through the window.
4. **Map a page.** Given a mapper and a frame allocator, point a virtual page at a
   physical frame. This is where paging stops being read-only.

The follow-up post ("Paging Implementation") turns steps 3 and 4 into the
[`OffsetPageTable`](https://docs.rs/x86_64/latest/x86_64/structures/paging/struct.OffsetPageTable.html)
mapper and the frame allocator this crate ships, so `examples/paging` covers both
rather than only the `CR3` peek.

## What this crate does instead

The paging shape is the tutorial's, but rebuilt around this repository's
conventions:

| the tutorial | here |
| --- | --- |
| `bootloader` 0.9's `MemoryMap` / `MemoryRegionType::Usable` | `bootloader_api` 0.11's [`MemoryRegions`](src/memory.rs) slice of `MemoryRegion`s with `MemoryRegionKind::Usable`; the *shape* is unchanged, only the names differ |
| a bare `translate_addr` that panics on huge frames | [`memory::translate_addr`](src/memory.rs) keeps that walk for contrast, next to the mapper-based [`memory::mapper_translate_addr`](src/memory.rs) that a test compares it against |
| a mapper passed around by hand | [`memory::init_paging`](src/memory.rs) stores the one mapper and frame allocator behind locks, so `kernel_main` and the tests share one address space without aliasing `&mut` page tables |
| `EmptyFrameAllocator` shown in prose | a test maps a page into a fresh `PML4` entry with `EmptyFrameAllocator` and asserts `MapToError::FrameAllocationFailed`, then does the same with the `BootInfoFrameAllocator` and reads the value back |
| post-07 is left behind | the GDT/TSS/IST, the PIC/PIT and the keyboard are carried in unchanged, and a test still asserts the timer interrupts |

The public names are the tutorial's, so the diff against it reads as "the same
code, wired into this repository".

## Layout

```
Cargo.toml                  standalone package (its own workspace)
.cargo/config.toml          cargo `runner` (boot the test ELF in QEMU) + the `qtest` alias
src/lib.rs                  print! macros, the post-04 test framework, kernel_main, unit tests
src/main.rs                 the bootable binary: bootloader config + entry_point!
src/memory.rs               the paging API: CR3, the table walk, the mapper and the frame allocators
src/allocator.rs            the post-10/11 heap hook: the heap region, Dummy, Locked and the installed fixed-size block allocator
src/allocator/bump.rs       the bump allocator design
src/allocator/linked_list.rs  the linked-list (free-list) allocator design
src/allocator/fixed_size_block.rs  the fixed-size block allocator design
src/gdt.rs                  the GDT, the TSS and the interrupt stack table (post-06)
src/interrupts.rs           the IDT, the PIC/PIT setup, the timer/keyboard/exception handlers
src/pic.rs                  the hand-rolled 8259 PIC: remap, mask, EOI
src/pit.rs                  the 8254 PIT: the tick rate and its divisor
src/keyboard.rs             a scancode-set-1 decoder and the input queue
src/serial.rs               a minimal COM1 UART driver, with the `without_interrupts` fix
src/port.rs                 x86 port I/O, used by the UART, the PIC/PIT and exit_qemu
tests/paging.rs             in-QEMU tests for the window, the walk and the mapping
tests/heap_allocation.rs    in-QEMU tests for the post-10/11 heap hook
```

## Test, build and run

The crate is `no_std` even under `cfg(test)`, so `cargo test` links bare-metal
test binaries and hands each one to the shared `runner`, which wraps it in a
bootable BIOS disk image, boots it in QEMU with `-device isa-debug-exit` and COM1
on stdout, and maps QEMU's exit code back onto `cargo test`. Because the runner is
configured by `.cargo/config.toml`, the tests must be run **from this directory**:

```powershell
cd examples\paging
cargo qtest        # == cargo test --target x86_64-unknown-none
```

or, from the repository root:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\paging.ps1 -Test
```

Three test binaries run — the library's unit tests, `tests/paging.rs` and
`tests/heap_allocation.rs` (the post-10/11 heap hook, which allocates a `Box`,
grows a `Vec` and runs `many_boxes_long_lived`) — and QEMU reports success as exit
code 33 (the framework writes `0x10`, which QEMU reports as `(0x10 << 1) | 1`). The
tests cover the physical-memory window, the `CR3` frame, the hand-written walk
against the mapper, an unmapped page, the `EmptyFrameAllocator` failure, a real
mapping read back, the post-07 timer, and the allocator designs' own unit tests.

Build the bootable kernel and boot it (BIOS, via the shared `runner`):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\paging.ps1
```

The kernel installs the GDT/IDT/PIC/PIT, prints the physical-memory offset and the
active level-4 frame, compares the hand-written walk with the mapper, maps a fresh
page to the VGA buffer and reads the value back, then reports how many timer
interrupts arrived in about two seconds and halts. Pass `-NoBoot` to only build
the image, `-Release` for an optimised build, or extra QEMU arguments after `--`.

## Scope

Post-08 is `CR3`, the physical-memory window, the table walk and mapping a page;
the frame allocator comes from the bootloader's memory map. Userspace is a later
post; the real kernel in [`kernel/`](../../kernel) already has it.

As a forward-looking hook, [`allocator`](src/allocator.rs) also brings forward
post-10's heap *interface* and post-11's allocator *designs*: it maps a heap
through the paging layer and installs a `#[global_allocator]`, so `Box`, `Vec` and
`Rc` work, and it implements the allocator with the three designs post-11 builds —
a bump, a linked-list and a fixed-size block allocator — installing the last. It
is exercised through the crate's public API by `tests/heap_allocation.rs`. The full
treatment is [`examples/allocator-designs`](../allocator-designs/README.md).

> **Why the console is COM1.** Post-08's kernel prints to the VGA text buffer;
> this crate's console is COM1, because that is the only console a headless
> `cargo test` can read. The display is
> [`examples/vga-text`](../vga-text/README.md).
