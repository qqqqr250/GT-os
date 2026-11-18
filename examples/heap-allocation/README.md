# GTos heap-allocation — `blog_os` post-10, done properly

This is a tiny, standalone crate that starts from
[`blog_os` post-10](https://github.com/phil-opp/blog_os/tree/post-10) — the *Heap
Allocation* post, which adds a virtual heap region, a `#[global_allocator]` and
the `alloc` crate's `Box`, `Vec` and `Rc` — and rebuilds it as something you would
actually ship. It is deliberately *not* part of the main GTos workspace, so it
can be built and tested on its own.

It is the sibling of [`examples/minimal`](../minimal/README.md) (post-02),
[`examples/vga-text`](../vga-text/README.md) (post-03),
[`examples/testing`](../testing/README.md) (post-04),
[`examples/cpu-exceptions`](../cpu-exceptions/README.md) (post-05),
[`examples/double-faults`](../double-faults/README.md) (post-06),
[`examples/hardware-interrupts`](../hardware-interrupts/README.md) (post-07) and
[`examples/paging`](../paging/README.md) (post-08 + post-09),
[`examples/allocator-designs`](../allocator-designs/README.md) (post-11) and
[`examples/async-await`](../async-await/README.md) (post-12). It reuses
post-04's in-QEMU test framework, post-05's exception handling, post-06's
GDT/TSS/IST, post-07's PIC/PIT/keyboard and post-08/09's paging layer, and adds the
heap. As a forward hook it also carries post-11's allocator designs.

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

Post-11 replaces the `linked_list_allocator` implementation with three designs
written from scratch — a bump allocator, a linked-list allocator and a fixed-size
block allocator — and this crate carries them forward, installing the last. See
[`allocator`](src/allocator.rs) and
[`examples/allocator-designs`](../allocator-designs/README.md).

## What this crate does instead

The heap shape is the tutorial's, but rebuilt around this repository's
conventions:

| the tutorial | here |
| --- | --- |
| the mapper and frame allocator are passed to `init_heap` by hand | [`allocator::init_heap`](src/allocator.rs) keeps that exact signature for contrast, and [`allocator::init`](src/allocator.rs) is the wrapper that supplies the paging layer's stored ones ([`memory::with_mapper`](src/memory.rs) / [`memory::with_frames`](src/memory.rs)) |
| `linked_list_allocator` 0.9 | `linked_list_allocator` 0.10, the same crate and version the real kernel in [`kernel/`](../../kernel) uses — kept as the fixed-size design's fallback |
| the post ends at post-10 | post-11's three allocator designs are carried forward and the fixed-size block design is installed, so a long-lived allocation no longer starves the heap |
| the post ends at post-10 | post-12's async runtime is carried forward, so a keyboard interrupt wakes a decoder task instead of decoding in the handler |
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
src/allocator.rs            the heap: Dummy, init_heap, Locked and the installed fixed-size block allocator
src/allocator/bump.rs       the bump allocator design
src/allocator/linked_list.rs  the linked-list (free-list) allocator design
src/allocator/fixed_size_block.rs  the fixed-size block allocator design
src/sync/mod.rs             the post-12 sync hook: the bounded lock-free queue and the once-cell
src/task/mod.rs             the post-12 task hook: Task, the executors and the async keyboard
src/memory.rs               the paging layer the heap is built on (post-08 + post-09)
src/gdt.rs                  the GDT, the TSS and the interrupt stack table (post-06)
src/interrupts.rs           the IDT, the PIC/PIT setup, the timer/keyboard/exception handlers
src/pic.rs                  the hand-rolled 8259 PIC: remap, mask, EOI
src/pit.rs                  the 8254 PIT: the tick rate and its divisor
src/keyboard.rs             a scancode-set-1 decoder and the input queue
src/serial.rs               a minimal COM1 UART driver, with the `without_interrupts` fix
src/port.rs                 x86 port I/O, used by the UART, the PIC/PIT and exit_qemu
tests/heap_allocation.rs    in-QEMU tests for Box, a growing Vec and many boxes
tests/task.rs               in-QEMU tests for the post-12 async hook
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

Three test binaries run — the library's unit tests (the heap tests, the three
designs' own tests and the post-12 sync/task tests), `tests/heap_allocation.rs` and
`tests/task.rs` (the post-12 async hook, which runs both executors and drives the
async scancode stream) — and QEMU reports success as exit code 33 (the framework
writes `0x10`, which QEMU reports as `(0x10 << 1) | 1`). The tests cover a `Box`
round-trip, a `Vec` that grows by reallocating, many boxes, `many_boxes_long_lived`,
a heap pointer landing inside the mapped window, and the post-07 timer.

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

Post-10 is the heap interface, the heap region and the allocator that hands it out;
post-11 is the allocator's own design — a bump allocator, a linked-list allocator
and a fixed-size block allocator, the last of which this crate installs as a
forward hook. The full post-11 treatment is
[`examples/allocator-designs`](../allocator-designs/README.md), and the real kernel
in [`kernel/`](../../kernel) already carries both forward.

As a further forward-looking hook, [`sync`](src/sync) and [`task`](src/task) bring
forward post-12's `async`/`await` runtime: a hand-written bounded lock-free queue
(`sync::ArrayQueue`) and one-shot initialiser (`sync::OnceCell`), and on top of
them `task::Task`, the naive `task::simple_executor::SimpleExecutor`, the
waker-based `task::executor::Executor` and the async keyboard
(`task::keyboard::ScancodeStream`). The keyboard IRQ now only pushes the raw
scancode and wakes the decoder task. It is exercised through the crate's public API
by `tests/task.rs`. The full post-12 treatment is
[`examples/async-await`](../async-await/README.md).

> **Why the console is COM1.** Post-10's kernel prints to the VGA text buffer;
> this crate's console is COM1, because that is the only console a headless
> `cargo test` can read. The display is
> [`examples/vga-text`](../vga-text/README.md).
