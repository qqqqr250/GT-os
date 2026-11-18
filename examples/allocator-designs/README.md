# GTos allocator-designs — `blog_os` post-11, done properly

This is a tiny, standalone crate that starts from
[`blog_os` post-11](https://github.com/phil-opp/blog_os/tree/post-11) — the
*Allocator Designs* post, which replaces the `linked_list_allocator` heap of
post-10 with three allocators written from scratch — and rebuilds it as something
you would actually ship. It is deliberately *not* part of the main GTos
workspace, so it can be built and tested on its own.

It is the sibling of [`examples/minimal`](../minimal/README.md) (post-02),
[`examples/vga-text`](../vga-text/README.md) (post-03),
[`examples/testing`](../testing/README.md) (post-04),
[`examples/cpu-exceptions`](../cpu-exceptions/README.md) (post-05),
[`examples/double-faults`](../double-faults/README.md) (post-06),
[`examples/hardware-interrupts`](../hardware-interrupts/README.md) (post-07),
[`examples/paging`](../paging/README.md) (post-08 + post-09),
[`examples/heap-allocation`](../heap-allocation/README.md) (post-10) and
[`examples/async-await`](../async-await/README.md) (post-12). It reuses
post-04's in-QEMU test framework, post-05's exception handling, post-06's
GDT/TSS/IST, post-07's PIC/PIT/keyboard and post-08/09's paging layer, and adds
the allocator designs on top of post-10's heap interface.

## What post-11 does

Post-10 gave the kernel a heap *interface*: a mapped region and a
`#[global_allocator]`, implemented by the `linked_list_allocator` crate. Post-11
implements that interface from scratch, three times, and compares the designs:

| design | idea | trade-off |
| --- | --- | --- |
| `BumpAllocator` | a linear "bump" pointer; `dealloc` only counts | fastest, but a single long-lived allocation stops all reuse |
| `LinkedListAllocator` | a free list threaded through the freed blocks themselves | reuses every freed block, but `alloc` is a linear scan and the list fragments |
| `FixedSizeBlockAllocator` | one free list per power-of-two size class, plus a `linked_list_allocator` fallback for large allocations | fast `alloc`/`dealloc` and low fragmentation for common sizes; some internal fragmentation |

The post's decisive test is `many_boxes_long_lived`: it holds one allocation live
while cycling through many short-lived ones. The bump allocator fails it (its
counter never returns to zero, so it exhausts the heap); the linked-list and
fixed-size block allocators pass it. The post concludes the fixed-size block
design is the right one for a kernel, and that is the design this crate installs.

## What this crate does instead

The designs are the tutorial's, but rebuilt around this repository's conventions:

| the tutorial | here |
| --- | --- |
| `src/allocator.rs` becomes a module of `bump`, `linked_list` and `fixed_size_block` submodules | the same layout: [`src/allocator.rs`](src/allocator.rs) holds `Locked`, `align_up` and `init_heap`, and [`src/allocator/`](src/allocator) holds the three designs |
| each design is only ever exercised by installing it as the global allocator in turn | each design also has unit tests that drive it over a small static scratch region, so all three are covered in one binary — including the bump allocator's *failure* to reuse memory |
| `linked_list_allocator` 0.9's `allocate_first_fit`/`deallocate` | `linked_list_allocator` 0.10, the same crate and version the real kernel in [`kernel/`](../../kernel) uses, as the fixed-size design's fallback |
| the post ends at post-11 | the GDT/TSS/IST, the PIC/PIT/keyboard and the paging layer are carried in unchanged, and a test still asserts the timer interrupts |
| the post ends at post-11 | post-12's async runtime is carried forward, so a keyboard interrupt wakes a decoder task instead of decoding in the handler |

The public names are the tutorial's, so the diff against it reads as "the same
code, wired into this repository".

## Layout

```
Cargo.toml                       standalone package (its own workspace)
.cargo/config.toml               cargo `runner` (boot the test ELF in QEMU) + the `qtest` alias
src/lib.rs                       print! macros, the post-04 test framework, kernel_main, heap_demo, unit tests
src/main.rs                      the bootable binary: bootloader config + entry_point!
src/allocator.rs                 the heap region, Locked, align_up, Dummy and the installed fixed-size block allocator
src/allocator/bump.rs            the bump allocator and its unit tests
src/allocator/linked_list.rs     the linked-list (free-list) allocator and its unit tests
src/allocator/fixed_size_block.rs  the fixed-size block allocator and its unit tests
src/sync/mod.rs                  the post-12 sync hook: the bounded lock-free queue and the once-cell
src/task/mod.rs                  the post-12 task hook: Task, the executors and the async keyboard
src/memory.rs                    the paging layer the heap is built on (post-08 + post-09)
src/gdt.rs                       the GDT, the TSS and the interrupt stack table (post-06)
src/interrupts.rs                the IDT, the PIC/PIT setup, the timer/keyboard/exception handlers
src/pic.rs                       the hand-rolled 8259 PIC: remap, mask, EOI
src/pit.rs                       the 8254 PIT: the tick rate and its divisor
src/keyboard.rs                  a scancode-set-1 decoder and the input queue
src/serial.rs                    a minimal COM1 UART driver, with the `without_interrupts` fix
src/port.rs                      x86 port I/O, used by the UART, the PIC/PIT and exit_qemu
tests/allocator_designs.rs       in-QEMU tests: the post's heap suite plus `many_boxes_long_lived`
tests/task.rs                    in-QEMU tests for the post-12 async hook
```

## Test, build and run

The crate is `no_std` even under `cfg(test)`, so `cargo test` links bare-metal
test binaries and hands each one to the shared `runner`, which wraps it in a
bootable BIOS disk image, boots it in QEMU with `-device isa-debug-exit` and COM1
on stdout, and maps QEMU's exit code back onto `cargo test`. Because the runner is
configured by `.cargo/config.toml`, the tests must be run **from this directory**:

```powershell
cd examples\allocator-designs
cargo qtest        # == cargo test --target x86_64-unknown-none
```

or, from the repository root:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\allocator-designs.ps1 -Test
```

Three test binaries run — the library's unit tests (the heap tests, the three
designs' own tests and the post-12 sync/task tests), `tests/allocator_designs.rs`
and `tests/task.rs` (the post-12 async hook, which runs both executors and drives
the async scancode stream) — and QEMU reports success as exit code 33 (the framework
writes `0x10`, which QEMU reports as `(0x10 << 1) | 1`). The tests cover a `Box`
round-trip, a `Vec` that grows by reallocating, many boxes, and
`many_boxes_long_lived`.

Build the bootable kernel and boot it (BIOS, via the shared `runner`):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\allocator-designs.ps1
```

The kernel installs the GDT/IDT/PIC/PIT, prints the physical-memory offset and the
active level-4 frame, builds the heap with the fixed-size block allocator, prints
where a `Box`, a `Vec` and an `Rc` land and the `Rc`'s strong count, proves a
long-lived `Box` survives many short-lived allocations, then reports how many
timer interrupts arrived in about two seconds and halts. Pass `-NoBoot` to only
build the image, `-Release` for an optimised build, or extra QEMU arguments after
`--`.

## Scope

Post-11 is the three allocator designs and the `Locked<A>` wrapper and `align_up`
helper that let them implement `GlobalAlloc`. The heap *interface* and the region
they hand out are post-10's; the real kernel in [`kernel/`](../../kernel) carries
both forward.

As a further forward-looking hook, [`sync`](src/sync) and [`task`](src/task) bring
forward post-12's `async`/`await` runtime: a hand-written bounded lock-free queue
(`sync::ArrayQueue`) and one-shot initialiser (`sync::OnceCell`), and on top of
them `task::Task`, the naive `task::simple_executor::SimpleExecutor`, the
waker-based `task::executor::Executor` and the async keyboard
(`task::keyboard::ScancodeStream`). The keyboard IRQ now only pushes the raw
scancode and wakes the decoder task. It is exercised through the crate's public API
by `tests/task.rs`. The full post-12 treatment is
[`examples/async-await`](../async-await/README.md).

> **Why the console is COM1.** Post-11's kernel prints to the VGA text buffer;
> this crate's console is COM1, because that is the only console a headless
> `cargo test` can read. The display is
> [`examples/vga-text`](../vga-text/README.md).
