# Testing

GTos has two layers of tests: an **in-QEMU `cargo test`** suite that runs
inside the kernel, and an **end-to-end smoke test** that boots a real image and
drives the shell.

## In-QEMU `cargo test`

A bare-metal binary cannot run under the host's test harness, so `cargo test`
links the kernel's test binaries for `x86_64-unknown-none` and hands each one to
a runner (`runner/`, wired up as the `runner` key in `.cargo\config.toml`). The
runner wraps the test ELF in a bootable BIOS disk image with the `bootloader`
crate, boots it in QEMU with `-device isa-debug-exit` and COM1 on stdout, and
maps QEMU's exit code back onto its own (the framework writes `0x10` on success,
which QEMU reports as exit code 33).

```powershell
cargo xtest        # == cargo test --manifest-path kernel\Cargo.toml --target x86_64-unknown-none
```

The kernel is its own workspace (it only builds for the bare-metal target), so it
is addressed by manifest path; the host workspace at the root excludes it.

Four test binaries run:

- **unit tests** in `kernel/src/lib.rs` and its modules (`cargo test --lib`);
  the `vga` module's tests drive the console writer against a RAM buffer,
- **`tests/basic_boot.rs`** — an integration test with its own `entry_point!`,
  checking the serial path, the physical-memory window, the VGA fallback path
  and a breakpoint exception (post-05: an `int3` must be handled and return),
  the timer (post-07: the tick count advances, and printing through the same
  lock while the timer fires does not deadlock), the paging API (post-08:
  the hand-written walk and the mapper agree, and a freshly mapped page reads
  back), and the kernel heap (post-10/11: a `Box` and a growing `Vec` round-trip
  through the global allocator with the pointer landing in the heap window, the
  installed design is the fixed-size block allocator, and a long-lived allocation
  does not starve the heap), and the async runtime (post-12: a scancode pushed the
  way the IRQ handler pushes one is decoded by the keyboard task into the input
  queue after the executor is polled),
- **`tests/should_panic.rs`** (`harness = false`) — asserts that the harness
  notices a panicking test,
- **`tests/stack_overflow.rs`** (`harness = false`) — post-06: overflows the
  kernel stack, and asserts the resulting double fault is handled on the
  dedicated interrupt stack instead of triple-faulting.

Test output goes to COM1; a failure panics and exits QEMU with the failure code.
The framework lives in `kernel/src/lib.rs` (`Testable`, `test_runner`,
`exit_qemu`, `serial_print!`/`serial_println!`), and every test binary reaches
the entry point through `entry_point!` so its ELF carries the
`.bootloader-config` section the bootloader needs in order to load it.

This is `blog_os` post-04's test framework, hardened. The public surface is the
same (`Testable`, `test_runner`, `test_main`, `QemuExitCode`, `exit_qemu`,
`serial_print!`/`serial_println!`), but the test entry point goes through
`entry_point!` instead of post-04's bare `#[unsafe(no_mangle)] _start` (so the
test ELF carries `.bootloader-config` without `bootimage`), `exit_qemu` returns
`!`, and the `runner` adds a 60-second timeout on top of post-04's exit-code
mapping. [`examples/testing`](../examples/testing/README.md) is that same framework
as a standalone post-04 crate.

The runner also has an `--image <elf> [<out.img>]` mode that only builds the
bootable disk image and prints its path; `scripts\minimal.ps1`,
`scripts\vga-text.ps1`, `scripts\testing.ps1`, `scripts\cpu-exceptions.ps1`,
`scripts\double-faults.ps1`, `scripts\hardware-interrupts.ps1`,
`scripts\paging.ps1`, `scripts\heap-allocation.ps1`,
`scripts\allocator-designs.ps1` and `scripts\async-await.ps1` use it to boot their
standalone `examples/` kernels.

The `examples/` crates are a small `blog_os`-post-by-post series, each with its
own README: post-02 (`minimal`), post-03 (`vga-text`), post-04 (`testing`),
post-05 (`cpu-exceptions`), post-06 (`double-faults`), post-07
(`hardware-interrupts`), post-08 (`paging`), post-10 (`heap-allocation`), post-11
(`allocator-designs`) and post-12 (`async-await`).
Each builds standalone and is deliberately *not* part of the host workspace.
`minimal` and `vga-text` are dual-mode — `no_std` for the kernel, `std` under
`cfg(test)` — so their unit tests (14 and 15 respectively) run on the **host**
with no QEMU:

```powershell
cargo test --manifest-path examples\minimal\Cargo.toml --lib
cargo test --manifest-path examples\vga-text\Cargo.toml --lib
```

`minimal` and `vga-text` also carry in-QEMU integration tests that boot the real
kernel, gated behind the `qemu-test` feature and their own `[[test]]` targets so a
plain host `cargo test` never tries to build a bare-metal test binary. Besides
the console checks, `tests/qemu.rs` raises a breakpoint and asserts that the
post-05 IDT handled it and returned, and `tests/stack_overflow.rs` (post-06)
overflows the kernel stack and asserts the double fault is caught on the IST
stack. Run them with the script's `-Test` switch, or from the crate directory
with `cargo qtest` (the `qtest` alias passes
`--features qemu-test --test qemu --test stack_overflow`):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\minimal.ps1 -Test
powershell -ExecutionPolicy Bypass -File scripts\vga-text.ps1 -Test
```

`examples/testing` is the post-04 kernel *and* the framework itself: it is always
`no_std`, so every one of its tests — three `#[test_case]` unit tests,
`tests/basic_boot.rs` and `tests/should_panic.rs` — runs inside QEMU through the
custom `test_runner`:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\testing.ps1 -Test
```

`examples/cpu-exceptions` is the post-05 kernel; it is always `no_std` too, so
its twelve `#[test_case]` unit tests (one of which raises a breakpoint),
`tests/breakpoint.rs` and its post-07/post-08/post-10/post-11 hook tests
(`tests/timer.rs`, `tests/paging.rs`, `tests/heap_allocation.rs`) run inside QEMU:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\cpu-exceptions.ps1 -Test
```

`examples/double-faults` is the post-06 kernel; it is always `no_std` too, so its
fourteen `#[test_case]` unit tests, `tests/stack_overflow.rs` and its
post-07/post-08/post-10/post-11 hook tests (`tests/timer.rs`, `tests/paging.rs`,
`tests/heap_allocation.rs`) run inside QEMU. The
stack-overflow test runs without the normal harness (a double fault cannot
return) and passes by exiting QEMU from the handler on the interrupt stack:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\double-faults.ps1 -Test
```

`examples/hardware-interrupts` is the post-07 kernel; it is always `no_std` too,
so its fifteen `#[test_case]` unit tests, `tests/hardware_interrupts.rs` and its
post-08/post-10/post-11 hook tests (`tests/paging.rs`, `tests/heap_allocation.rs`)
run inside QEMU. The integration test drives
the live timer through `hlt`, checks the keyboard decoder, and turns on the
timer's dot printing while the main thread prints through the same lock, which is
what proves the post-07 `without_interrupts` fix holds:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\hardware-interrupts.ps1 -Test
```

`examples/paging` is the post-08 kernel — the *Introduction to Paging* post plus
the paging *implementation* — and the first example whose bootloader config turns
on the physical-memory window for its own boot path (post-08 is *about* that
window). It is always `no_std` too, so its twenty-one `#[test_case]` unit tests,
`tests/paging.rs` and its post-10/post-11 hook test (`tests/heap_allocation.rs`)
run inside QEMU; they check the window, the `CR3` frame, the hand-written walk
against the mapper, the `EmptyFrameAllocator` failure and a real mapping read
back, plus the carried-forward post-07 timer:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\paging.ps1 -Test
```

`examples/heap-allocation` is the post-10 kernel — the *Heap Allocation* post —
which maps a heap at a fixed virtual address through the paging layer, installs a
`#[global_allocator]` and uses `Box`, `Vec` and `Rc`. It is always `no_std` too,
so its twenty-four `#[test_case]` unit tests and `tests/heap_allocation.rs` run
inside QEMU; they check a `Box` round-trip, a `Vec` that grows by reallocating,
`many_boxes_long_lived` (one allocation held live across the whole loop), a heap
pointer landing inside the mapped window, and the carried-forward post-07 timer.
As a forward hook it also carries post-11's three allocator designs and installs
the fixed-size block one:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\heap-allocation.ps1 -Test
```

`examples/allocator-designs` is the post-11 kernel — the *Allocator Designs*
post — which replaces the `linked_list_allocator` implementation with three
designs written from scratch (a bump allocator, a linked-list allocator and a
fixed-size block allocator, the last of which it installs). It is always `no_std`
too, so its twenty-four `#[test_case]` unit tests (the designs' own, over a small
scratch region) and `tests/allocator_designs.rs` run inside QEMU; they check the
bump allocator's consecutive blocks and its starving under a long-lived
allocation, the linked-list allocator's free-list reuse, the fixed-size block
allocator's size-class reuse and its large-allocation fallback, and the
`many_boxes_long_lived` test that separates the three:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\allocator-designs.ps1 -Test
```

`examples/async-await` is the post-12 kernel — the *Async/Await* post — which adds
a cooperative multitasking runtime on top of post-11's heap: a `Task` wrapping a
`Pin<Box<dyn Future>>`, the naive `SimpleExecutor`, and a waker-based `Executor`
with a ready queue, a per-task waker cache and `sleep_if_idle`, plus the async
keyboard whose interrupt handler queues raw scancodes for a decoding task. It is
always `no_std` too, so its `#[test_case]` unit tests (the heap and allocator
tests carried forward, plus the queue, the once-cell and both executors) and
`tests/async_await.rs` run inside QEMU; they check a `Box` round-trip, a growing
`Vec`, `many_boxes_long_lived`, both executors running a task, and the async
keyboard stream yielding a scancode pushed the way the interrupt handler pushes
one:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\async-await.ps1 -Test
```

The in-QEMU tests must be run from their crate directory: Cargo reads
`.cargo/config.toml` from the current directory upwards, and each example's file
points the `runner` at the shared `../../runner`.

## End-to-end smoke test

`scripts\smoke-test.ps1` is the integration test. It exposes COM1 as a TCP
socket (`-serial tcp:...,server=on,wait=off`) so it can wait for the kernel to
come up before typing, then runs the command list, asserts on the output, checks
for panics and exceptions, issues `shutdown` and verifies that QEMU reports the
`isa-debug-exit` success code (`0x21`). Because the desktop owns the screen when
a framebuffer is present, the test first sends a bare Enter on COM1 to hand the
console to the shell (see [Desktop environment](04-desktop.md)).
Transcripts land in `target\smoke-test-<firmware>-<profile>.log` (for example
`target\smoke-test-bios-debug.log`, or
`target\smoke-test-uefi-iso-debug.log` for the ISO). Pass `-Release` to test the
optimised image, `-Firmware uefi` for the OVMF path, `-Iso` to boot the disc
image from a virtual CD-ROM, and `-Network` to also exercise DNS and HTTP
against the real internet. By default it also checks the ATA/FAT16 disk
(`ls`/`cat`/`write`/`rm`), AC'97 playback and the ring-3 `run` commands; pass
`-Audio` to additionally capture QEMU's playback and assert the samples are
non-silent. It fails fast if the kernel panics.

The project is warning-free and clippy-clean under the lint configuration in
`Cargo.toml` (`clippy::all` and `clippy::pedantic` at `warn`); `scripts\setup.ps1`
installs the `clippy` and `rustfmt` components for that. Because the kernel is
its own workspace, the host workspace at the root is clippy-clean end to end:

```powershell
cargo clippy --workspace --all-targets                                   # host launcher
cargo clippy --manifest-path kernel\Cargo.toml --target x86_64-unknown-none   # kernel
```
