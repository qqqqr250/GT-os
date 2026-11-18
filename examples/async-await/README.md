# GTos async-await — `blog_os` post-12, done properly

This is a tiny, standalone crate that starts from
[`blog_os` post-12](https://github.com/phil-opp/blog_os/tree/post-12) — the
*Async/Await* post, which adds a cooperative multitasking runtime to the kernel —
and rebuilds it as something you would actually ship. It is deliberately *not* part
of the main GTos workspace, so it can be built and tested on its own.

It is the sibling of [`examples/minimal`](../minimal/README.md) (post-02),
[`examples/vga-text`](../vga-text/README.md) (post-03),
[`examples/testing`](../testing/README.md) (post-04),
[`examples/cpu-exceptions`](../cpu-exceptions/README.md) (post-05),
[`examples/double-faults`](../double-faults/README.md) (post-06),
[`examples/hardware-interrupts`](../hardware-interrupts/README.md) (post-07),
[`examples/paging`](../paging/README.md) (post-08 + post-09),
[`examples/heap-allocation`](../heap-allocation/README.md) (post-10) and
[`examples/allocator-designs`](../allocator-designs/README.md) (post-11). It reuses
post-04's in-QEMU test framework, post-05's exception handling, post-06's
GDT/TSS/IST, post-07's PIC/PIT/keyboard, post-08/09's paging layer and post-11's
allocator designs, and adds the task runtime on top.

## What post-12 does

Post-11 finished the heap; post-12 builds cooperative multitasking on it:

- a **`Task`** wraps a future in a `Pin<Box<dyn Future<Output = ()>>>` and gives it
  a unique id,
- a **`SimpleExecutor`** polls every task in a loop, handing each a *dummy* waker
  that does nothing — the naive first step, which busy-spins on a task that is
  waiting,
- an **`Executor`** keeps a ready queue of task ids and a cached waker per task.
  A task's waker pushes its id back onto the queue, so the executor polls only the
  tasks that were woken, and `sleep_if_idle` parks the CPU with `hlt` when none
  are ready,
- an **async keyboard**: the interrupt handler pushes raw scancodes into a
  lock-free queue and wakes the task that decodes them, so the decoder is an
  ordinary `async` function that suspends until the next key.

The subtle part is `sleep_if_idle`: it disables interrupts, checks the ready queue
and only then executes `hlt` (`enable_and_hlt`). Without that, an interrupt could
queue a task *after* the check but *before* the halt, and the task would sleep
until the next unrelated interrupt.

## What this crate does instead

The runtime is the tutorial's, but rebuilt around this repository's conventions:

| the tutorial | here |
| --- | --- |
| the bounded queue comes from `crossbeam-queue`, the once-cell from `conquer-once` | both are hand-written in [`src/sync/`](src/sync): `ArrayQueue` is Vyukov's bounded lock-free MPMC queue and `OnceCell` is a one-shot initialiser, each with unit tests |
| the keyboard is decoded with the `pc-keyboard` crate | the hand-written scancode-set-1 decoder from post-07 is kept; the async task feeds it and prints what it produces |
| `Executor::run` is the kernel's main loop | `run_ready_tasks` is public, so a demo (or a shell loop) can poll the executor without giving up its own loop |
| the post's keyboard prints decoded characters | the same, but non-ASCII control codes (the arrow keys, backspace, escape) are spelled out (`<up>`, `<bs>`, …) so the serial log stays readable |

The public names are the tutorial's, so the diff against it reads as "the same
code, wired into this repository".

## Layout

```
Cargo.toml                       standalone package (its own workspace)
.cargo/config.toml               cargo `runner` (boot the test ELF in QEMU) + the `qtest` alias
src/lib.rs                       print! macros, the post-04 test framework, kernel_main, heap_demo, async_demo, unit tests
src/main.rs                      the bootable binary: bootloader config + entry_point!
src/task/mod.rs                  Task and TaskId
src/task/simple_executor.rs      the naive executor and its dummy RawWaker
src/task/executor.rs             the waker-based executor: ready queue, waker cache, sleep_if_idle
src/task/keyboard.rs             ScancodeStream, add_scancode and the async print_keypresses task
src/sync/mod.rs                  the hand-written primitives, re-exported
src/sync/queue.rs                ArrayQueue: a bounded lock-free MPMC queue
src/sync/once_cell.rs            OnceCell: a one-shot initialiser
src/allocator.rs                 the heap region, Locked, align_up and the installed fixed-size block allocator (post-11)
src/allocator/bump.rs            the bump allocator and its unit tests
src/allocator/linked_list.rs     the linked-list (free-list) allocator and its unit tests
src/allocator/fixed_size_block.rs  the fixed-size block allocator and its unit tests
src/memory.rs                    the paging layer the heap is built on (post-08 + post-09)
src/gdt.rs                       the GDT, the TSS and the interrupt stack table (post-06)
src/interrupts.rs                the IDT, the PIC/PIT setup, the timer/keyboard/exception handlers
src/pic.rs                       the hand-rolled 8259 PIC: remap, mask, EOI
src/pit.rs                       the 8254 PIT: the tick rate and its divisor
src/keyboard.rs                  the scancode-set-1 decoder the async task feeds
src/serial.rs                    a minimal COM1 UART driver, with the `without_interrupts` fix
src/port.rs                      x86 port I/O, used by the UART, the PIC/PIT and exit_qemu
tests/async_await.rs             in-QEMU tests: post-11's heap suite plus post-12's task and stream tests
```

## Test, build and run

The crate is `no_std` even under `cfg(test)`, so `cargo test` links bare-metal
test binaries and hands each one to the shared `runner`, which wraps it in a
bootable BIOS disk image, boots it in QEMU with `-device isa-debug-exit` and COM1
on stdout, and maps QEMU's exit code back onto `cargo test`. Because the runner is
configured by `.cargo/config.toml`, the tests must be run **from this directory**:

```powershell
cd examples\async-await
cargo qtest        # == cargo test --target x86_64-unknown-none
```

or, from the repository root:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\async-await.ps1 -Test
```

Two test binaries run — the library's unit tests (the heap tests, the three
allocator designs, the queue, the once-cell and both executors) and
`tests/async_await.rs` — and QEMU reports success as exit code 33 (the framework
writes `0x10`, which QEMU reports as `(0x10 << 1) | 1`). The tests cover a `Box`
round-trip, a `Vec` that grows by reallocating, `many_boxes_long_lived`, both
executors running a task, and the async keyboard stream yielding a scancode that
was pushed the way the interrupt handler pushes one.

Build the bootable kernel and boot it (BIOS, via the shared `runner`):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\async-await.ps1
```

The kernel installs the GDT/IDT/PIC/PIT, prints the physical-memory offset and the
active level-4 frame, builds the heap, prints where a `Box`, a `Vec` and an `Rc`
land, proves a long-lived `Box` survives many short-lived allocations, reports how
many timer interrupts arrived in about two seconds, then starts the async executor:
a demo task awaits a future and prints its result, and the async keyboard echoes
whatever is typed on the PS/2 keyboard. Pass `-NoBoot` to only build the image,
`-Release` for an optimised build, or extra QEMU arguments after `--`.

## Scope

Post-12 is the task runtime: `Task`, `SimpleExecutor`, `Executor`, the ready queue,
the waker cache and the async keyboard stream, plus the two hand-written
synchronisation primitives they rest on. The heap and the paging layer underneath
are post-10 and post-11's; the real kernel in [`kernel/`](../../kernel) carries
both forward and routes its keyboard interrupt through the async stream.

> **Why the console is COM1.** Post-12's kernel prints to the VGA text buffer;
> this crate's console is COM1, because that is the only console a headless
> `cargo test` can read. The display is
> [`examples/vga-text`](../vga-text/README.md).
