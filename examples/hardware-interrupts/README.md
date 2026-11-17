# RustOS hardware-interrupts — `blog_os` post-07, done properly

This is a tiny, standalone crate that starts from
[`blog_os` post-07](https://github.com/phil-opp/blog_os/tree/post-07) — the
*Hardware Interrupts* post, which wires the 8259 PIC up so the timer and the
keyboard can interrupt the CPU — and rebuilds it as something you would actually
ship. It is deliberately *not* part of the main RustOS workspace, so it can be
built and tested on its own.

It is the sibling of [`examples/minimal`](../minimal/README.md) (post-02),
[`examples/vga-text`](../vga-text/README.md) (post-03),
[`examples/testing`](../testing/README.md) (post-04),
[`examples/cpu-exceptions`](../cpu-exceptions/README.md) (post-05),
[`examples/double-faults`](../double-faults/README.md) (post-06) and
[`examples/paging`](../paging/README.md) (post-08). It reuses post-04's in-QEMU
test framework, post-05's exception handling and post-06's GDT/TSS/IST, and adds
the first *device*-driven interrupts.

## What post-07 does

Until now the CPU only took the exceptions it raised itself; the devices were
silent. Post-07 turns that on:

1. **Remap the PIC.** Out of reset the 8259 delivers IRQ0 on vector 8 — the
   double fault. The remap moves the master PIC's IRQs to `0x20..0x27` and the
   slave's to `0x28..0x2F`, clear of the CPU's exception vectors.
2. **Enable interrupts** with `sti`, once an IDT and handlers exist.
3. **Handle the timer.** The PIT is programmed to raise IRQ0 at a steady rate;
   the handler counts ticks and sends an end-of-interrupt (EOI) so the PIC
   unmasks the line again.
4. **Fix the deadlock.** The moment a handler prints through the same lock as the
   main thread, that lock can deadlock. Post-07's fix is to take it inside
   `without_interrupts`.
5. **Handle the keyboard.** IRQ1 reads a scancode from port `0x60` and decodes it.
6. **Idle in `hlt`.** The main loop sleeps until the next interrupt instead of
   spinning.

The deadlock is the interesting part. If `print!` takes the serial lock and the
timer fires while the main thread holds it, the handler spins on that lock
forever — the main thread can never release it, because the CPU is stuck in the
handler:

```rust
// broken: the handler can fire between the lock and the unlock
let mut serial = SERIAL.lock();
write!(serial, ...);
```

```rust
// fixed: interrupts are off for the whole critical section
interrupts::without_interrupts(|| {
    let mut serial = SERIAL.lock();
    write!(serial, ...);
});
```

## What this crate does instead

The PIC/PIT/keyboard shape is post-07's, but rebuilt around this repository's
conventions:

| post-07 | here |
| --- | --- |
| the `pic8259` crate (`ChainedPics`) | a hand-rolled [`pic`](src/pic.rs) that runs the ICW sequence itself, so the crate depends only on `bootloader_api`, `spin` and `x86_64` |
| the `pc-keyboard` crate (`ScancodeSet1`) | a hand-rolled [`keyboard`](src/keyboard.rs): a pure `decode` state machine (Shift, Caps Lock, the `0xE0` prefix) plus a small ring the IRQ handler fills |
| the PIT divisor is a magic constant | [`pit::divisor`](src/pit.rs) is a pure, unit-tested function of the tick rate |
| the deadlock is described in prose | [`serial::print_args`](src/serial.rs) takes the lock inside `without_interrupts`, and a test turns on timer dots and prints through the same lock to prove it does not deadlock |
| the timer handler only prints | it counts ticks ([`interrupts::ticks`](src/interrupts.rs)) and prints a dot only when a test enables it, so a normal boot stays quiet |
| post-06's GDT/TSS/IST is left behind | it is carried in unchanged, so the double-fault handler still runs on its own stack |
| later posts are not present | as forward-looking hooks, [`memory`](src/memory.rs) brings forward post-08's paging, [`allocator`](src/allocator.rs) brings forward post-10's heap interface and post-11's allocator designs, and [`sync`](src/sync) / [`task`](src/task) bring forward post-12's async runtime, so `tests/paging.rs`, `tests/heap_allocation.rs` and `tests/task.rs` can drive them through the crate's public API |

The public names are post-07's, so the diff against the tutorial reads as "the
same code, wired into this repository".

## Layout

```
Cargo.toml                  standalone package (its own workspace)
.cargo/config.toml          cargo `runner` (boot the test ELF in QEMU) + the `qtest` alias
src/lib.rs                  print! macros, the post-04 test framework, kernel_main, unit tests
src/main.rs                 the bootable binary: bootloader config + entry_point!
src/gdt.rs                  the GDT, the TSS and the interrupt stack table (post-06)
src/interrupts.rs           the IDT, the PIC/PIT setup, the timer/keyboard/exception handlers
src/pic.rs                  the hand-rolled 8259 PIC: remap, mask, EOI
src/pit.rs                  the 8254 PIT: the tick rate and its divisor
src/keyboard.rs             a scancode-set-1 decoder and the input queue
src/memory.rs               the post-08 paging hook: CR3, the table walk, the mapper and the frame allocators
src/allocator.rs            the post-10/11 heap hook: the heap region, Dummy, Locked and the installed fixed-size block allocator
src/allocator/bump.rs       the bump allocator design
src/allocator/linked_list.rs  the linked-list (free-list) allocator design
src/allocator/fixed_size_block.rs  the fixed-size block allocator design
src/sync/mod.rs             the post-12 sync hook: the bounded lock-free queue and the once-cell
src/task/mod.rs             the post-12 task hook: Task, the executors and the async keyboard
src/serial.rs               a minimal COM1 UART driver, with the `without_interrupts` fix
src/port.rs                 x86 port I/O, used by the UART, the PIC/PIT and exit_qemu
tests/hardware_interrupts.rs  in-QEMU tests for the timer, the PIC, the PIT and the keyboard
tests/paging.rs             in-QEMU test for the post-08 paging hook
tests/heap_allocation.rs    in-QEMU test for the post-10/11 heap hook
tests/task.rs               in-QEMU test for the post-12 async hook
```

## Test, build and run

The crate is `no_std` even under `cfg(test)`, so `cargo test` links bare-metal
test binaries and hands each one to the shared `runner`, which wraps it in a
bootable BIOS disk image, boots it in QEMU with `-device isa-debug-exit` and COM1
on stdout, and maps QEMU's exit code back onto `cargo test`. Because the runner is
configured by `.cargo/config.toml`, the tests must be run **from this directory**:

```powershell
cd examples\hardware-interrupts
cargo qtest        # == cargo test --target x86_64-unknown-none
```

or, from the repository root:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\hardware-interrupts.ps1 -Test
```

Five test binaries run — the library's unit tests (including the allocator
designs' own tests and the post-12 sync/task tests), `tests/hardware_interrupts.rs`,
`tests/paging.rs` (the post-08 paging hook, which maps a fresh page and reads the
value back), `tests/heap_allocation.rs` (the post-10/11 heap hook, which allocates a
`Box`, grows a `Vec` and runs `many_boxes_long_lived`) and `tests/task.rs` (the
post-12 async hook, which runs both executors and drives the async scancode stream)
— and QEMU reports success as exit code 33 (the framework writes `0x10`, which QEMU
reports as `(0x10 << 1) | 1`). The tests cover the remapped PIC offsets, the PIT
divisor, the live tick count, the keyboard decoder and the `without_interrupts`
deadlock fix.

Build the bootable kernel and boot it (BIOS, via the shared `runner`):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\hardware-interrupts.ps1
```

The kernel enables the timer, prints a dot every ten ticks for about two seconds,
reports how many interrupts arrived, and halts. Pass `-NoBoot` to only build the
image, `-Release` for an optimised build, or extra QEMU arguments after `--`.

## Scope

Post-07 is the PIC, the PIT, the timer, the deadlock fix, the `hlt` loop and the
keyboard. Paging and a heap come later; the mouse (IRQ12) and the serial IRQ are
not exercised here — the real kernel in [`kernel/`](../../kernel) handles both.

As a forward-looking hook, [`memory`](src/memory.rs) brings forward post-08's
paging: the `CR3` read, a hand-written four-level table walk, the
`OffsetPageTable` mapper and a frame allocator over the bootloader's memory map. It
is exercised through the crate's public API by `tests/paging.rs`, whose bootloader
config turns on the physical-memory window. The full post-08 treatment is
[`examples/paging`](../paging/README.md).

A second forward-looking hook, [`allocator`](src/allocator.rs), brings forward
post-10's heap *interface* and post-11's allocator *designs*: it maps a heap through
the paging layer and installs a `#[global_allocator]`, so `Box`, `Vec` and `Rc`
work, and it implements the allocator with the three designs post-11 builds — a
bump, a linked-list and a fixed-size block allocator — installing the last. It is
exercised through the crate's public API by `tests/heap_allocation.rs`. The full
treatment is [`examples/allocator-designs`](../allocator-designs/README.md).

A third forward-looking hook, [`sync`](src/sync) and [`task`](src/task), brings
forward post-12's `async`/`await` runtime: a hand-written bounded lock-free queue
(`sync::ArrayQueue`) and one-shot initialiser (`sync::OnceCell`), and on top of
them `task::Task`, the naive `task::simple_executor::SimpleExecutor`, the
waker-based `task::executor::Executor` and the async keyboard
(`task::keyboard::ScancodeStream`). The keyboard IRQ now only pushes the raw
scancode and wakes the decoder task. It is exercised through the crate's public API
by `tests/task.rs`. The full post-12 treatment is
[`examples/async-await`](../async-await/README.md).

> **Why the console is COM1.** Post-07's kernel prints to the VGA text buffer;
> this crate's console is COM1, because that is the only console a headless
> `cargo test` can read. The display is
> [`examples/vga-text`](../vga-text/README.md).
