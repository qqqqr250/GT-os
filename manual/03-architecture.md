# Architecture

## Project layout

```
Cargo.toml               workspace + host launcher package (the kernel is excluded)
.cargo\config.toml       cargo `runner` (in-QEMU tests) + the `xtest` alias
build.rs                 turns the kernel artifact into BIOS/UEFI disk images and the ISO
build\iso.rs             dependency-free ISO9660 + El Torito disc image writer
src\main.rs              host launcher: locates QEMU/OVMF, boots an image or the ISO
kernel\
  Cargo.toml             freestanding kernel (its own workspace): library + thin binary (x86_64-unknown-none)
  src\main.rs            binary: bootloader entry point, panic/alloc handlers
  src\lib.rs             kernel library: kernel_main, boot log, test framework
  src\console.rs         framebuffer text console + print!/println! macros
  src\vga.rs             legacy VGA text-mode console fallback (0xb8000)
  src\volatile.rs        Volatile<T>: the one place raw device pointers are read/written
  src\font.rs            generated 8x8 bitmap font
  src\memory.rs          heap selection, the global allocator and the paging layer (post-08, post-10/11)
  src\allocator.rs       the Locked wrapper and the allocator designs module (post-11)
  src\allocator\bump.rs   the bump allocator design
  src\allocator\linked_list.rs  the linked-list (free-list) allocator design
  src\allocator\fixed_size_block.rs  the fixed-size block allocator design (the one installed)
  src\interrupts.rs      IDT, PIC remap, PIT, IRQ handlers, exception reports
  src\input.rs           lock-free input ring buffer
  src\keyboard.rs        PS/2 scancode set 1 → ASCII
  src\sync\queue.rs      the bounded lock-free MPMC queue the async keyboard feeds (post-12)
  src\sync\once_cell.rs  the one-shot initialiser for the scancode queue
  src\task\mod.rs        Task, TaskId and the kernel's async runtime entry points
  src\task\executor.rs   the waker-based executor: ready queue, waker cache, sleep_if_idle
  src\task\simple_executor.rs  the naive executor and its dummy waker
  src\task\keyboard.rs   ScancodeStream, add_scancode and the async decoder task
  src\mouse.rs           PS/2 mouse driver (8042 aux port, 3-byte packets)
  src\pci.rs             PCI bus-0 configuration-space scan (0xCF8/0xCFC)
  src\net.rs             RTL8139 driver + IPv4/ARP/ICMP/UDP/DNS/TCP stack + DHCP
  src\audio.rs           AC'97 codec driver (DMA buffer descriptor list)
  src\ata.rs             ATA PIO driver for the primary IDE channel (LBA28)
  src\fat.rs             read/write FAT16 filesystem
  src\browser.rs         HTML-to-text renderer for the desktop Browser window
  src\usermode.rs        ring 3: GDT/TSS/paging, int 0x80 syscalls, program loader
  src\serial.rs          16550 UART driver for COM1
  src\wm.rs              Windows XP-style desktop: taskbar, windows, cursor
  src\shell.rs           banner, boot log helpers, shell and commands
  tests\basic_boot.rs    in-QEMU integration test (boot, serial, VGA fallback)
  tests\should_panic.rs  in-QEMU test that the harness notices a panic
  tests\stack_overflow.rs in-QEMU test that a stack overflow double-faults onto the IST stack
runner\
  Cargo.toml             host-side cargo test runner package
  src\main.rs            wraps a test ELF in a disk image, boots QEMU, maps exit code
                         (`--image <elf> [<out.img>]` only builds the image)
examples\
  minimal\               standalone `blog_os` post-02+ crate: safe VGA console,
                         its own README, host `cargo test` + in-QEMU test (not in the workspace)
  vga-text\              standalone `blog_os` post-03 crate: safe VGA text
                         writer, its own README, host `cargo test` + in-QEMU test (not in the workspace)
  testing\               standalone `blog_os` post-04 crate: the in-QEMU test
                         framework itself, always `no_std` (not in the workspace)
  cpu-exceptions\        standalone `blog_os` post-05 crate: the IDT and the CPU
                         exception handlers, always `no_std` (not in the workspace)
  double-faults\         standalone `blog_os` post-06 crate: the GDT/TSS/IST that
                         catches a stack-overflow double fault, always `no_std`
                         (not in the workspace)
  hardware-interrupts\   standalone `blog_os` post-07 crate: the 8259 PIC, the
                         PIT timer, the keyboard and the `without_interrupts`
                         print-lock fix, always `no_std` (not in the workspace)
  paging\                standalone `blog_os` post-08 crate: `CR3`, the four-level
                         table walk, the `OffsetPageTable` mapper and a frame
                         allocator, always `no_std` (not in the workspace)
  heap-allocation\       standalone `blog_os` post-10 crate: the heap region, the
                         `#[global_allocator]` and `Box`/`Vec`/`Rc`, always
                         `no_std` (not in the workspace)
  allocator-designs\     standalone `blog_os` post-11 crate: the bump, linked-list
                         and fixed-size block allocators, always `no_std`
                         (not in the workspace)
  async-await\           standalone `blog_os` post-12 crate: `Task`, the two
                         executors, the hand-written queue/once-cell and the async
                         keyboard, always `no_std` (not in the workspace)
build\
  iso.rs                 ISO9660 + El Torito writer
  fat16.rs               host-side FAT16 image builder (seeds the data disk)
scripts\
  setup.ps1              one-shot toolchain installer
  env.ps1                shared PATH/toolchain detection, dot-sourced
  build.ps1, run.ps1     thin wrappers around cargo
  minimal.ps1            build + boot (or -Test) the `examples/minimal` kernel
  vga-text.ps1           build + boot (or -Test) the `examples/vga-text` kernel
  testing.ps1            build + boot (or -Test) the `examples/testing` kernel
  cpu-exceptions.ps1     build + boot (or -Test) the `examples/cpu-exceptions` kernel
  double-faults.ps1      build + boot (or -Test) the `examples/double-faults` kernel
  hardware-interrupts.ps1 build + boot (or -Test) the `examples/hardware-interrupts` kernel
  paging.ps1             build + boot (or -Test) the `examples/paging` kernel
  heap-allocation.ps1    build + boot (or -Test) the `examples/heap-allocation` kernel
  allocator-designs.ps1  build + boot (or -Test) the `examples/allocator-designs` kernel
  async-await.ps1        build + boot (or -Test) the `examples/async-await` kernel
  smoke-test.ps1         end-to-end boot test (BIOS and UEFI; -Iso for the disc image)
  screenshot.ps1         capture the desktop framebuffer to PNG
tools\
  gen-font.ps1           regenerates kernel\src\font.rs from font8x8_basic.h
  font8x8_basic.h        public-domain 8x8 font data
```

## How it works

**Boot.** `build.rs` asks the `bootloader` crate to wrap the compiled kernel ELF
into a FAT disk image (BIOS: MBR + stages 2–4; UEFI: an `.efi` application).
Firmware loads the image, the bootloader sets up paging, maps the kernel, a
stack, the framebuffer, BootInfo and a window onto physical memory, then jumps to
`kernel_main` in long mode.

**Configuration.** `BOOTLOADER_CONFIG` in `kernel/src/main.rs` sets a 512 KiB
kernel stack and enables `mappings.physical_memory = Dynamic`, which is what
gives the kernel a virtual window onto RAM (`BootInfo::physical_memory_offset`).
The default configuration maps no physical memory at all, so a kernel that wants
a heap must ask for it.

**Console.** `console::init` takes the framebuffer, picks a glyph scale of
`min(width / (8*80), height / (9*25))` clamped to 1–3 (so a 1280x720 BIOS screen
is 80x40 cells) and paints the screen. Every write goes to COM1 first, then to
the framebuffer, so a crash in the renderer still leaves a serial log.

**Heap.** `memory::init` picks the largest usable region that does not overlap
the kernel image, takes its top 32 MiB, and hands
`physical_memory_offset + address` to the global allocator. That allocator is
post-11's fixed-size block design (`kernel/src/allocator/`): one free list per
power-of-two size class, with a `linked_list_allocator` fallback for large
requests, so a freed block is reused immediately instead of waiting for the whole
heap to empty. Nothing may allocate before that point: the banner and boot log
deliberately use `format_args!` rather than `format!`.

**Interrupts.** Before the IDT is loaded, `usermode::init_gdt` installs the GDT
and TSS, including the interrupt stack the double-fault handler runs on (post-06);
the IDT entry names that stack, so it must already be loaded. Then
`interrupts::init` remaps the 8259 PIC (IRQ0–7 → 0x20–0x27,
IRQ8–15 → 0x28–0x2F), loads the IDT, programs the PIT for 100 Hz and unmasks
IRQ0, IRQ1, IRQ2, IRQ4, IRQ12 (remember: a set bit in the PIC's IMR *masks* a
line). `kernel_main` starts the async runtime (`task::init`) just before
`interrupts::init`, so the keyboard IRQ always finds an initialised scancode
queue. The keyboard handler then only queues the raw scancode and wakes the
post-12 async decoder, which pushes the decoded byte into `input`, a 256-byte
SPSC ring buffer that the shell drains while halted with `hlt`; the serial
handler writes bytes directly.

## Console fallback

When the bootloader reports a linear framebuffer, `console::init` renders into
it. When it reports none — a firmware that could not set a video mode, or a
deliberate `-vga none` run — `kernel/src/vga.rs` takes over the 80x25 text
buffer the firmware has already set up at physical `0xb8000`. The buffer is
device memory, so it is reached through the bootloader's physical-memory window
(`physical_memory_offset + 0xb8000`) and only touched through the `Volatile<T>`
wrapper in `kernel/src/volatile.rs` (`read_volatile`/`write_volatile`); the
module owns the cursor, the colour attribute and scrolling, and never hands out a
raw pointer. All sixteen text colours are available (`set_fg`/`set_bg`/
`set_color`), and the hardware cursor is programmed through the CRTC at ports
`0x3D4`/`0x3D5`.

The console's named colours keep their meaning here: `console::Color::to_vga_text`
maps them to their intended text colour (the muted success green becomes the
bright text green, not grey), and any other 24-bit colour is snapped to the
nearest palette entry by `vga::Color::from_rgb`. If there is neither a
framebuffer nor a physical-memory window, the kernel falls back to serial-only
output and says so in the boot log.

The writer's grid is abstracted behind a small `Buffer` trait, so the unit tests
at the bottom of `kernel/src/vga.rs` drive the same scrolling, wrapping,
backspace, colour and cursor logic against a RAM buffer instead of a live device.
That matters because QEMU plus the bootloader *always* end up in
linear-framebuffer graphics mode, which leaves the legacy text aperture at
`0xb8000` unbacked: the RAM-backed tests exercise the logic deterministically,
while the `basic_boot` integration test only checks that the fallback path
initializes, tracks colours and does not fault.

[`examples/minimal/`](../examples/minimal) is a standalone teaching crate that
starts from `blog_os` post-02 (a raw `*mut u8` write to `0xb8000`) and rebuilds
it with the same safe writer;
[`examples/vga-text/`](../examples/vga-text/README.md) does the same for post-03,
keeping that post's `vga_buffer` names. A byte the text mode cannot render shows
the CP437 `0xfe` box (post-03's choice) rather than disappearing.
[`examples/testing/`](../examples/testing/README.md) is post-04 — the *Testing*
post — rebuilt around this repository's conventions; it is the crate that carries
the in-QEMU test framework the kernel's own `cargo xtest` uses.
[`examples/cpu-exceptions/`](../examples/cpu-exceptions/README.md) is post-05 —
the *CPU Exceptions* post — an IDT with divide-error, invalid-opcode, breakpoint,
double-fault, general-protection and page-fault handlers, each recording what it
saw so a test can assert the handler ran.
[`examples/double-faults/`](../examples/double-faults/README.md) is post-06 — the
*Double Faults* post — which gives the double-fault handler a real interrupt
stack (a GDT, a TSS and the interrupt stack table) so a kernel stack overflow is
caught instead of triple-faulting.
[`examples/hardware-interrupts/`](../examples/hardware-interrupts/README.md) is
post-07 — the *Hardware Interrupts* post — which remaps the 8259 PIC, programs
the PIT, decodes the keyboard and takes the print lock inside
`without_interrupts`, so a handler that prints cannot deadlock the main thread.
[`examples/paging/`](../examples/paging/README.md) is post-08 — the *Introduction
to Paging* post together with the paging *implementation* — which reads `CR3`,
walks the four-level table, maps a page through an `OffsetPageTable` and hands out
frames from the bootloader's memory map.
[`examples/heap-allocation/`](../examples/heap-allocation/README.md) is post-10 —
the *Heap Allocation* post — which maps a heap through the paging layer, installs
a `#[global_allocator]`, and uses `Box`, `Vec` and `Rc`.
[`examples/allocator-designs/`](../examples/allocator-designs/README.md) is
post-11 — the *Allocator Designs* post — which replaces that implementation with
three designs written from scratch (a bump allocator, a linked-list allocator and
a fixed-size block allocator) and installs the last.
