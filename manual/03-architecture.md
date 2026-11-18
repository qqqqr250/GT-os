# Architecture

## Project layout

```
Cargo.toml               host workspace + build-only package (the kernel is excluded)
.cargo\config.toml       nightly artifact-dependency flag and bare-metal rustflags
build.rs                 turns the kernel artifact into BIOS/UEFI disk images and the ISO
build\iso.rs             dependency-free ISO9660 + El Torito disc image writer
build\fat16.rs           host-side FAT16 image builder (seeds the data disk)
src\main.rs              build-only helper: prints the paths of the generated images
kernel\
  Cargo.toml             freestanding kernel (its own workspace): library + thin binary (x86_64-unknown-none)
  src\main.rs            binary: bootloader entry point, panic/alloc handlers
  src\lib.rs             kernel library: kernel_main and the boot log
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
  src\net.rs             RTL8139/E1000 drivers + IPv4/ARP/ICMP/UDP/DNS/TCP stack + DHCP + HTTP(S)
  src\scheme.rs          Redox-style `file:/`, `net:/`, `sys:/` namespace and descriptor table
  src\audio.rs           AC'97 codec driver (DMA buffer descriptor list)
  src\ata.rs             ATA PIO driver for the primary IDE channel (LBA28)
  src\fat.rs             read/write FAT16 filesystem
  src\fm.rs              file-manager sorting, filtering and formatting helpers
  src\browser.rs         clean-room HTML/CSS engine for the desktop Browser window
  src\usermode.rs        ring 3: GDT/TSS/paging, int 0x80 syscalls, program loader
  src\serial.rs          16550 UART driver for COM1
  src\wm.rs              Windows XP-style desktop: taskbar, windows, cursor
  src\shell.rs           banner, boot log helpers, shell and commands
scripts\
  virtualbox.ps1         create/start a GTos VM on VirtualBox with a NAT E1000
  setup.ps1              one-shot toolchain installer
  env.ps1                shared PATH/toolchain detection, dot-sourced
  build.ps1              thin wrapper around `cargo build`
tools\
  gen-font.ps1           regenerates kernel\src\font.rs from font8x8_basic.h
  font8x8_basic.h        public-domain 8x8 font data
```

## How it works

**Boot.** `build.rs` asks the `bootloader` crate to wrap the compiled kernel ELF
into a FAT disk image (BIOS: MBR + stages 2–4; UEFI: an `.efi` application).
Firmware loads the image, the bootloader sets up paging, maps the kernel, a
stack, the framebuffer, BootInfo and a window onto physical memory, then jumps to
`kernel_main` in long mode. `scripts\virtualbox.ps1` converts the BIOS image into
a VDI and attaches it to the VM's primary IDE channel.

**Configuration.** `BOOTLOADER_CONFIG` in `kernel/src/main.rs` sets a 512 KiB
kernel stack and enables `mappings.physical_memory = Dynamic`, which is what
gives the kernel a virtual window onto RAM (`BootInfo::physical_memory_offset`).
The default configuration maps no physical memory at all, so a kernel that wants
a heap must ask for it.

**Console.** `console::init` takes the framebuffer, picks a glyph scale of
`min(width / (8*80), height / (9*25))` clamped to 1–3 (so a 1280x720 screen is
80x40 cells) and paints the screen. Every write goes to COM1 first, then to the
framebuffer, so a crash in the renderer still leaves a serial log.

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

## Schemes and the shell (Redox-inspired)

Redox's most recognisable idea is that "everything is a URL": a file, a network
resource and a kernel status item are all opened through a scheme namespace with
the same small set of operations. `kernel/src/scheme.rs` brings that pattern into
the kernel with three read-only schemes:

| Scheme | Example | What it reads |
|--------|---------|---------------|
| `file` | `file:/README.TXT` | the mounted FAT16 data disk |
| `net`  | `net:/http://example.com` | the HTTP(S) client (a bare host defaults to HTTPS) |
| `sys`  | `sys:/mem`, `sys:/net`, `sys:/version` | live kernel state |

Use `schemes` to list them and `open <url>` to read one, for example
`open file:/README.TXT` or `open sys:/uptime`. An open resource is snapshotted
into memory and placed in a small descriptor table starting at fd 3 (0–2 remain
the console streams). Ring 3 reaches the same table through the `open`, `read`
and `close` syscalls; `run scheme` is a hand-assembled user program that opens
`sys:/version`, reads it, prints it and exits, so the whole path can be tested
without a disk. This is a deliberate simplification of Redox's streaming,
user-space scheme servers — it keeps the syscall surface small while making all
resources uniform.

The shell also borrows from Redox's Ion shell: **Up**/**Down** recall command
history (32 entries, with duplicate suppression), **Tab** completes command
names and, for `cat`, `open`, `play`, `rm` and `run`, FAT16 file names. Completion
only rewrites the line when there is exactly one candidate. The GUI Command
Prompt shares the same history and completion code, so both front ends behave
identically.

## Console fallback

When the bootloader reports a linear framebuffer, `console::init` renders into
it. When it reports none — a firmware that could not set a video mode, or a
guest with no graphics adapter — `kernel/src/vga.rs` takes over the 80x25 text
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

The writer's grid is abstracted behind a small `Buffer` trait, so the same
scrolling, wrapping, backspace, colour and cursor logic can be exercised against
a RAM buffer rather than a live device. At boot the path is chosen purely from
what the bootloader reports: framebuffer first, then the legacy text aperture,
then serial-only output.
