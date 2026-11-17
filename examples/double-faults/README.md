# RustOS double-faults — `blog_os` post-06, done properly

This is a tiny, standalone crate that starts from
[`blog_os` post-06](https://github.com/phil-opp/blog_os/tree/post-06) — the
*Double Faults* post, which catches a stack-overflow double fault on a dedicated
interrupt stack — and rebuilds it as something you would actually ship. It is
deliberately *not* part of the main RustOS workspace, so it can be built and
tested on its own.

It is the sibling of [`examples/minimal`](../minimal/README.md) (post-02),
[`examples/vga-text`](../vga-text/README.md) (post-03),
[`examples/testing`](../testing/README.md) (post-04) and
[`examples/cpu-exceptions`](../cpu-exceptions/README.md) (post-05). It reuses
post-04's in-QEMU test framework and post-05's exception handling, and adds the
hardware stack switch that makes the worst double fault survivable.

## What post-06 does

A *double fault* is what the CPU raises when it cannot invoke the handler for
another exception. If that is left unhandled too, the machine triple-faults and
resets. Post-05 installs a plain double-fault handler, but there is one case it
cannot catch: a **kernel stack overflow**. By then the stack pointer already
points at the guard page, so the CPU cannot push the exception frame and the
double fault never reaches a handler.

Post-06 fixes that with hardware stack switching. The CPU is told to switch to a
known-good stack *before* pushing the double-fault frame. That stack is named in
the **task state segment** (TSS), whose **interrupt stack table** (IST) holds up
to seven of them, and the TSS is reached through a descriptor in the **global
descriptor table** (GDT):

```rust
static mut TSS: TaskStateSegment = TaskStateSegment::new();
static mut DOUBLE_FAULT_STACK: Stack = Stack([0; DOUBLE_FAULT_STACK_SIZE]);

pub fn init() {
    unsafe {
        (*core::ptr::addr_of_mut!(TSS)).interrupt_stack_table[DOUBLE_FAULT_IST_INDEX] =
            VirtAddr::new(double_fault_stack_top());
    }

    let mut gdt = GDT.lock();
    let code = gdt.append(Descriptor::kernel_code_segment());
    let tss = gdt.append(Descriptor::tss_segment(unsafe { &*core::ptr::addr_of!(TSS) }));
    unsafe { gdt.load_unsafe() };
    unsafe {
        CS::set_reg(code);
        load_tss(tss);
    }
}

// in the IDT:
unsafe {
    idt.double_fault
        .set_handler_fn(double_fault)
        .set_stack_index(DOUBLE_FAULT_IST_INDEX);
}
```

The test then recurses until the guard page below the kernel stack is hit; the
page fault cannot be delivered there, so the CPU raises a double fault and
switches to the IST stack, where the handler runs.

## What this crate does instead

The GDT/TSS/IST shape is post-06's, but rebuilt around this repository's
conventions:

| post-06 | here |
| --- | --- |
| a `lazy_static` GDT/TSS | the GDT is a [`spin::Mutex`] in a `static`, and the TSS and the double-fault stack are `static`s, all built in place (`GlobalDescriptorTable::new()` and `TaskStateSegment::new()` are `const fn`s) |
| only a code segment; reloads `cs` | a code *and* a data segment, and `cs`, `ss`, `ds`, `es`, `fs`, `gs` are all reloaded, so no register keeps pointing at a stale descriptor from the bootloader's GDT |
| the double-fault stack is a plain array | the stack is `#[repr(align(16))]`, and its top is exposed as `gdt::double_fault_stack_top()` so a test can assert the TSS really names it |
| the handler just prints | the handler bumps an atomic (`interrupts::double_fault_count`) and records the vector (`interrupts::last_vector`) |
| `test_stack_overflow` in `tests/` with no explanation | `tests/stack_overflow.rs` runs `harness = false` (a double fault cannot return) and passes by exiting QEMU from the handler |

The public names are post-06's, so the diff against the tutorial reads as "the
same code, wired into this repository".

[`spin::Mutex`]: https://docs.rs/spin

## Layout

```
Cargo.toml                  standalone package (its own workspace)
.cargo/config.toml          cargo `runner` (boot the test ELF in QEMU) + the `qtest` alias
src/lib.rs                  print! macros, the post-04 test framework, kernel_main, overflow_stack, unit tests
src/main.rs                 the bootable binary: bootloader config + entry_point!
src/gdt.rs                  the GDT, the TSS and the interrupt stack table
src/interrupts.rs           the IDT, the exception handlers, the post-07 timer hook, double_fault_count/ticks
src/pic.rs                  the hand-rolled 8259 PIC: remap, mask, EOI (post-07)
src/pit.rs                  the 8254 PIT: the tick rate and its divisor (post-07)
src/memory.rs               the post-08 paging hook: CR3, the table walk, the mapper and the frame allocators
src/allocator.rs            the post-10 heap hook: the heap region, Dummy and the LockedHeap global allocator
src/serial.rs               a minimal COM1 UART driver (the console)
src/port.rs                 x86 port I/O, used by the UART, the PIC/PIT and exit_qemu
tests/stack_overflow.rs     in-QEMU stack-overflow test (harness = false)
tests/timer.rs              in-QEMU test for the post-07 timer hook
tests/paging.rs             in-QEMU test for the post-08 paging hook
tests/heap_allocation.rs    in-QEMU test for the post-10 heap hook
```

## Test, build and run

The crate is `no_std` even under `cfg(test)`, so `cargo test` links bare-metal
test binaries and hands each one to the shared `runner`, which wraps it in a
bootable BIOS disk image, boots it in QEMU with `-device isa-debug-exit` and COM1
on stdout, and maps QEMU's exit code back onto `cargo test`. Because the runner is
configured by `.cargo/config.toml`, the tests must be run **from this directory**:

```powershell
cd examples\double-faults
cargo qtest        # == cargo test --target x86_64-unknown-none
```

or, from the repository root:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\double-faults.ps1 -Test
```

Five test binaries run — the library's unit tests (including one that raises a
breakpoint and one that asserts the TSS names a double-fault stack),
`tests/stack_overflow.rs` (the stack-overflow double fault), `tests/timer.rs`
(the post-07 timer hook, which asserts `interrupts::ticks` advances while the CPU
idles in `hlt`), `tests/paging.rs` (the post-08 paging hook, which maps a fresh
page and reads the value back) and `tests/heap_allocation.rs` (the post-10 heap
hook, which allocates a `Box` and grows a `Vec`) — and QEMU reports success as exit code 33 (the
framework writes `0x10`, which QEMU reports as `(0x10 << 1) | 1`). A passing
stack-overflow test shows:

```
stack_overflow::stack_overflow...	[ok]
```

Build the bootable kernel and boot it (BIOS, via the shared `runner`):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\double-faults.ps1
```

Pass `-NoBoot` to only build the image, `-Release` for an optimised build, or
extra QEMU arguments after `--`.

## Scope

Post-06 is only the double-fault handler and the stack switch that supports it. A
plain double fault is still fatal — the handler halts the CPU — but it now
*runs*, which is what turns a silent reset into a diagnosable fault.

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
post-10's heap: it maps a `linked_list_allocator` heap through the paging layer and
installs a `#[global_allocator]`, so `Box`, `Vec` and `Rc` work. It is exercised
through the crate's public API by `tests/heap_allocation.rs`. The full post-10
treatment is [`examples/heap-allocation`](../heap-allocation/README.md).

> **Why there is no VGA writer here.** Post-06's kernel prints to the VGA text
> buffer; this crate's console is COM1, because that is the only console a
> headless `cargo test` can read. The display is
> [`examples/vga-text`](../vga-text/README.md).
