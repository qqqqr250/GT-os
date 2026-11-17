# RustOS heap-allocation — `blog_os` post-10, done properly

This is a tiny, standalone crate that starts from
[`blog_os` post-10](https://github.com/phil-opp/blog_os/tree/post-10) — the *Heap
Allocation* post, which adds a virtual heap region, a `#[global_allocator]` and
the `alloc` crate's `Box`, `Vec` and `Rc` — and rebuilds it as something you would
actually ship. It is deliberately *not* part of the main RustOS workspace, so it
can be built and tested on its own.

It is the sibling of [`examples/minimal`](../minimal/README.md) (post-02),
[`examples/vga-text`](../vga-text/README.md) (post-03),
[`examples/testing`](../testing/README.md) (post-04),
[`examples/cpu-exceptions`](../cpu-exceptions/README.md) (post-05),
[`examples/double-faults`](../double-faults/README.md) (post-06),
[`examples/hardware-interrupts`](../hardware-interrupts/README.md) (post-07) and
[`examples/paging`](../paging/README.md) (post-08 + post-09). It reuses post-04's
in-QEMU test framework, post-05's exception handling, post-06's GDT/TSS/IST,
post-07's PIC/PIT/keyboard and post-08/09's paging layer, and adds the heap.

## What post-10 does

Until now the kernel only ever used the stack and `static`s. Post-10 adds dynamic
memory in three steps:

1. **The interface.** Rust's `alloc` crate needs a `#[global_allocator]` that
   implements `GlobalAlloc`. The post first writes a `Dummy` allocator that
   returns null from `alloc` and panics in `dealloc` — enough to make the "no
   global allocator" error go away while the real one is built.
2. **The region.** A heap is a run of virtual pages backed by physical frames.
   `init_heap` maps `HEAP_SIZE` bytes at `HEAP_START` with the mapper and frame
   allocator the paging layer provides, then hands the region to the allocator.
3. **The allocator.** A `linked_list_allocator::LockedHeap` — a linked-list
   allocator behind a spin lock — is installed as the global allocator. It is
   usable from any context that can take the lock, including an interrupt handler.

With that in place, `Box`, `Vec`, `Rc` and every other `alloc` type work.

## What this crate does instead

The heap shape is the tutorial's, but rebuilt around this repository's
conventions:

| the tutorial | here |
| --- | --- |
| the mapper and frame allocator are passed to `init_heap` by hand | [`allocator::init_heap`](src/allocator.rs) keeps that exact signature for contrast, and [`allocator::init`](src/allocator.rs) is the wrapper that supplies the paging layer's stored ones ([`memory::with_mapper`](src/memory.rs) / [`memory::with_frames`](src/memory.rs)) |
| `linked_list_allocator` 0.9 | `linked_list_allocator` 0.10, the same crate and version the real kernel in [`kernel/`](../../kernel) uses |
| the post's `main` allocates a `Box`, a `Vec` and an `Rc` inline | [`heap_demo`](src/lib.rs) does the same, but as its own function, and the tests assert the pointers land inside the heap window |
| the post ends at post-10 | the GDT/TSS/IST, the PIC/PIT/keyboard and the paging layer are carried in unchanged, and a test still asserts the timer interrupts |

The public names are the tutorial's, so the diff against it reads as "the same
code, wired into this repository".

## Layout

```
Cargo.toml                  standalone package (its own workspace)
.cargo/config.toml          cargo `runner` (boot the test ELF in QEMU) + the `qtest` alias
src/lib.rs                  print! macros, the post-04 test framework, kernel_main, heap_demo, unit tests
src/main.rs                 the bootable binary: bootloader config + entry_point!
src/allocator.rs            the heap: Dummy, init_heap, the LockedHeap global allocator
src/memory.rs               the paging layer the heap is built on (post-08 + post-09)
src/gdt.rs                  the GDT, the TSS and the interrupt stack table (post-06)
src/interrupts.rs           the IDT, the PIC/PIT setup, the timer/keyboard/exception handlers
src/pic.rs                  the hand-rolled 8259 PIC: remap, mask, EOI
src/pit.rs                  the 8254 PIT: the tick rate and its divisor
src/keyboard.rs             a scancode-set-1 decoder and the input queue
src/serial.rs               a minimal COM1 UART driver, with the `without_interrupts` fix
src/port.rs                 x86 port I/O, used by the UART, the PIC/PIT and exit_qemu
tests/heap_allocation.rs    in-QEMU tests for Box, a growing Vec and many boxes
```

## Test, build and run

The crate is `no_std` even under `cfg(test)`, so `cargo test` links bare-metal
test binaries and hands each one to the shared `runner`, which wraps it in a
bootable BIOS disk image, boots it in QEMU with `-device isa-debug-exit` and COM1
on stdout, and maps QEMU's exit code back onto `cargo test`. Because the runner is
configured by `.cargo/config.toml`, the tests must be run **from this directory**:

```powershell
cd examples\heap-allocation
cargo qtest        # == cargo test --target x86_64-unknown-none
```

or, from the repository root:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\heap-allocation.ps1 -Test
```

Two test binaries run — the library's unit tests (including the three heap tests)
and `tests/heap_allocation.rs` — and QEMU reports success as exit code 33 (the
framework writes `0x10`, which QEMU reports as `(0x10 << 1) | 1`). The tests cover
a `Box` round-trip, a `Vec` that grows by reallocating, a heap pointer landing
inside the mapped window, and the post-07 timer.

Build the bootable kernel and boot it (BIOS, via the shared `runner`):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\heap-allocation.ps1
```

The kernel installs the GDT/IDT/PIC/PIT, prints the physical-memory offset and the
active level-4 frame, builds the heap, prints where a `Box`, a `Vec` and an `Rc`
land and the `Rc`'s strong count, then reports how many timer interrupts arrived
in about two seconds and halts. Pass `-NoBoot` to only build the image, `-Release`
for an optimised build, or extra QEMU arguments after `--`.

## Scope

Post-10 is the heap interface, the heap region and the `linked_list_allocator`
that hands it out; the allocator's own design (bump, linked-list, fixed-size
blocks) is the next post. The real kernel in [`kernel/`](../../kernel) already has
a heap, and post-11's allocator designs build on the same `GlobalAlloc` interface.

> **Why the console is COM1.** Post-10's kernel prints to the VGA text buffer;
> this crate's console is COM1, because that is the only console a headless
> `cargo test` can read. The display is
> [`examples/vga-text`](../vga-text/README.md).
