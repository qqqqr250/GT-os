# RustOS cpu-exceptions — `blog_os` post-05, done properly

This is a tiny, standalone crate that starts from
[`blog_os` post-05](https://github.com/phil-opp/blog_os/tree/post-05) — the
*CPU Exceptions* post, which installs an interrupt descriptor table (IDT) and a
breakpoint handler — and rebuilds it as something you would actually ship. It is
deliberately *not* part of the main RustOS workspace, so it can be built and
tested on its own.

It is the sibling of [`examples/minimal`](../minimal/README.md) (post-02),
[`examples/vga-text`](../vga-text/README.md) (post-03) and
[`examples/testing`](../testing/README.md) (post-04). It reuses post-04's in-QEMU
test framework so that the exception handlers can be *asserted on*, not just
printed.

## What post-05 does

Before post-05 the CPU had nowhere to go on an exception, so any fault was a
triple fault (an immediate reset). Post-05 installs an IDT with a single
breakpoint handler:

```rust
lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt
    };
}

pub fn init_idt() {
    IDT.load();
}

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    println!("EXCEPTION: BREAKPOINT\n{:#?}", stack_frame);
}

#[test_case]
fn test_breakpoint_exception() {
    // invoke a breakpoint exception
    x86_64::instructions::interrupts::int3();
}
```

The handler is `extern "x86-interrupt"` (hence the `abi_x86_interrupt` feature),
which is what makes the compiler emit the `iretq`-style epilogue the CPU expects.
The `int3` in the test raises `#BP`, the handler prints the stack frame, and
because a breakpoint is recoverable, execution continues.

## What this crate does instead

The IDT and the handler shape are post-05's, but rebuilt around this
repository's conventions:

| post-05 | here |
| --- | --- |
| a `lazy_static` IDT | the IDT is a [`spin::Mutex`] in a `static`, built in place (`InterruptDescriptorTable::new()` is a `const fn`), and loaded with `load_unsafe` |
| one breakpoint handler | all six self-generated exceptions are handled: divide error, invalid opcode, breakpoint, double fault, general protection fault, page fault |
| handlers only `println!` | handlers record the vector in an atomic (`interrupts::last_vector`) and bump a counter (`interrupts::exception_count`), so a test can assert the handler ran |
| fatal faults spin silently | the fatal handlers report the faulting `rip`/`rsp` (and error code / fault address) before halting |
| `test_breakpoint_exception` just calls `int3` | the in-QEMU test calls `int3` and then asserts the count went up by one and the last vector is `Breakpoint` |

The public names are post-05's, so the diff against the tutorial reads as "the
same code, wired into this repository".

[`spin::Mutex`]: https://docs.rs/spin

## Layout

```
Cargo.toml              standalone package (its own workspace)
.cargo/config.toml      cargo `runner` (boot the test ELF in QEMU) + the `qtest` alias
src/lib.rs              print! macros, the post-04 test framework, kernel_main, unit tests
src/main.rs             the bootable binary: bootloader config + entry_point!
src/interrupts.rs       the IDT, the exception handlers, the post-07 timer hook, last_vector/exception_count/ticks
src/pic.rs              the hand-rolled 8259 PIC: remap, mask, EOI (post-07)
src/pit.rs              the 8254 PIT: the tick rate and its divisor (post-07)
src/memory.rs           the post-08 paging hook: CR3, the table walk, the mapper and the frame allocators
src/allocator.rs        the post-10/11 heap hook: the heap region, Dummy, Locked and the installed fixed-size block allocator
src/allocator/bump.rs   the bump allocator design
src/allocator/linked_list.rs  the linked-list (free-list) allocator design
src/allocator/fixed_size_block.rs  the fixed-size block allocator design
src/serial.rs           a minimal COM1 UART driver (the console)
src/port.rs             x86 port I/O, used by the UART, the PIC/PIT and exit_qemu
tests/breakpoint.rs     in-QEMU integration test for the IDT, through the public API
tests/timer.rs          in-QEMU integration test for the post-07 timer hook
tests/paging.rs         in-QEMU integration test for the post-08 paging hook
tests/heap_allocation.rs  in-QEMU integration test for the post-10/11 heap hook
```

## Test, build and run

The crate is `no_std` even under `cfg(test)`, so `cargo test` links bare-metal
test binaries and hands each one to the shared `runner`, which wraps it in a
bootable BIOS disk image, boots it in QEMU with `-device isa-debug-exit` and COM1
on stdout, and maps QEMU's exit code back onto `cargo test`. Because the runner is
configured by `.cargo/config.toml`, the tests must be run **from this directory**:

```powershell
cd examples\cpu-exceptions
cargo qtest        # == cargo test --target x86_64-unknown-none
```

or, from the repository root:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\cpu-exceptions.ps1 -Test
```

Five test binaries run — the library's unit tests (including one that raises a
breakpoint, plus the allocator designs' own tests), `tests/breakpoint.rs` (the IDT
through the public API), `tests/timer.rs` (the post-07 timer hook, which asserts
`interrupts::ticks` advances while the CPU idles in `hlt`), `tests/paging.rs` (the
post-08 paging hook, which maps a fresh page and reads the value back) and
`tests/heap_allocation.rs` (the post-10/11 heap hook, which allocates a `Box`,
grows a `Vec` and runs `many_boxes_long_lived`) — and QEMU reports success
as exit code 33 (the framework writes `0x10`, which QEMU reports as
`(0x10 << 1) | 1`). A passing run shows the handler's own line, for example:

```
EXCEPTION: BREAKPOINT at rip 0x00000100000039ef, rsp 0x0000018000014e48
```

Build the bootable kernel and boot it (BIOS, via the shared `runner`):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\cpu-exceptions.ps1
```

Pass `-NoBoot` to only build the image, `-Release` for an optimised build, or
extra QEMU arguments after `--`.

## Scope

Post-05 is only the IDT. The double-fault handler installed here is what the post
asks for, but a *recoverable* double fault needs a task state segment (TSS) with a
dedicated interrupt stack, which is post-06's work — so in this crate a double
fault is still fatal, exactly as in the tutorial at this point.

As a forward-looking hook, `interrupts::init` also brings forward post-07's
hardware interrupts: it remaps the 8259 PIC, programs the PIT and unmasks the
timer, so the crate keeps running against the latest code in the series and a
test can watch `interrupts::ticks` advance. The full post-07 treatment — the
print-lock deadlock fix, the keyboard and the `hlt` idle loop — is
[`examples/hardware-interrupts`](../hardware-interrupts/README.md).

A second forward-looking hook, [`memory`](src/memory.rs), brings forward post-08's
paging: the `CR3` read, a hand-written four-level table walk, the
`OffsetPageTable` mapper and a frame allocator over the bootloader's memory map. It
is exercised through the crate's public API by `tests/paging.rs`, whose bootloader
config turns on the physical-memory window. The full post-08 treatment is
[`examples/paging`](../paging/README.md).

A third forward-looking hook, [`allocator`](src/allocator.rs), brings forward
post-10's heap *interface* and post-11's allocator *designs*: it maps a heap through
the paging layer and installs a `#[global_allocator]`, so `Box`, `Vec` and `Rc`
work, and it implements the allocator with the three designs post-11 builds — a
bump, a linked-list and a fixed-size block allocator — installing the last. It is
exercised through the crate's public API by `tests/heap_allocation.rs`. The full
treatment is [`examples/allocator-designs`](../allocator-designs/README.md).

> **Why there is no VGA writer here.** Post-05's kernel prints to the VGA text
> buffer; this crate's console is COM1, because that is the only console a
> headless `cargo test` can read. The display is
> [`examples/vga-text`](../vga-text/README.md).
