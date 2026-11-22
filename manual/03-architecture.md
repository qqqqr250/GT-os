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
  src\logo_data.rs       generated 128x128 2-bpp GT logo bitmap
  src\logo.rs            framebuffer logo renderer (boot strip + `logo` command)
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
  src\pci.rs             PCI bus-0 configuration-space scan (0xCF8/0xCFC)
  src\net.rs             RTL8139/E1000 drivers + IPv4/ARP/ICMP/UDP/DNS/TCP stack + DHCP + HTTP(S)
  src\scheme.rs          Redox-style `file:/`, `net:/`, `sys:/` namespace and descriptor table
  src\pkg.rs             Redox-inspired package dependency graph (`pkg` state)
  src\pkgar.rs           clean-room `.pkga` archive reader/writer (pkgar-inspired)
  src\audio.rs           AC'97 codec driver (DMA buffer descriptor list)
  src\ata.rs             ATA PIO driver for the primary IDE channel (LBA28)
  src\fat.rs             read/write FAT16 filesystem
  src\address_space.rs   per-process PML4 tables, user page mapping, COW fork and CR3 switching
  src\process.rs         PIDs, process control blocks, kernel stacks and the process table
  src\context.rs         saved ring-3 register frames and low-level context switching
  src\scheduler.rs       round-robin process scheduling on yield/exit and PIT ticks
  src\rtc.rs             CMOS real-time clock and Unix-second conversion
  src\elf.rs             static ELF64 loader with load-bias and `R_X86_64_RELATIVE` relocation
  src\usermode.rs        ring 3: GDT/TSS, int 0x80 syscalls, program loader
  src\serial.rs          16550 UART driver for COM1
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

**Physical frames.** `memory::init_frame_allocator` collects the `Usable` frames
outside the heap and below 1 MiB into a free stack. `address_space` allocates
zeroed frames from that stack for PML4s, page tables and user pages, and returns
them through `memory::deallocate_frame` when their last owner disappears.
`fork` shares writable frames copy-on-write; a ring-3 write fault copies the
frame into a new physical frame rather than a heap block.

**Interrupts.** Before the IDT is loaded, `usermode::init_gdt` installs the GDT
and TSS, including the interrupt stack the double-fault handler runs on (post-06);
the IDT entry names that stack, so it must already be loaded. Then
`interrupts::init` remaps the 8259 PIC (IRQ0–7 → 0x20–0x27,
IRQ8–15 → 0x28–0x2F), loads the IDT, programs the PIT for 100 Hz and unmasks
IRQ0, IRQ1, IRQ2, IRQ4, IRQ12 (remember: a set bit in the PIC's IMR *masks* a
line). `kernel_main` starts the async runtime (`task::init`) and completes the
PS/2 mouse handshake just before `interrupts::init`, so the keyboard IRQ always
finds an initialised scancode queue and IRQ12 cannot consume handshake ACKs. The
keyboard handler only queues the raw scancode and wakes the async decoder, which
pushes the translated byte into `input`, a 256-byte SPSC ring buffer that the
shell drains while halted with `hlt`. The decoder also records key releases and
modifier transitions in a separate 256-entry event mirror; the mouse handler
adds absolute pointer and button events to that mirror, and the serial handler
writes bytes directly.

## Schemes and the shell (Redox-inspired)

Redox's most recognisable idea is that "everything is a URL": a file, a network
resource and a kernel status item are all opened through a scheme namespace with
the same small set of operations. `kernel/src/scheme.rs` brings that pattern into
the kernel with three built-in schemes:

| Scheme | Example | What it provides |
|--------|---------|------------------|
| `file` | `file:/README.TXT` | read/write snapshots on the FAT16 data disk |
| `net`  | `net:/http://example.com` | HTTP(S) client responses (a bare host defaults to HTTPS) |
| `sys`  | `sys:/mem`, `sys:/net`, `sys:/version` | live kernel state |
| registered | `demo:/hello` | pipe-backed ring-3 scheme server |

Use `schemes` to list them and `open <url>` to read one, for example
`open file:/README.TXT` or `open sys:/uptime`. An open resource is snapshotted
into memory and placed in a small per-process descriptor table starting at fd 3
(0–2 remain the console streams). Ring 3 reaches its own table through the
`open`, `read`, `write`, `lseek` and `close` syscalls. Disk files are the writable
scheme: a write updates the snapshot and flushes it through the FAT16 driver;
`net` and `sys` descriptors stay read-only. `run scheme` is a hand-assembled user
program that opens `sys:/version`, reads it, prints it and exits, so the whole
path can be tested without a disk. `fork` copies the parent's table and `exec`
preserves it.

A ring-3 process can also register a scheme name with `schemeregister(name, fd)`
and serve it through a pipe. When any process opens a URL using that scheme, the
kernel creates a duplex stream, inserts its server end into the registering
process's descriptor table, and writes a `CONNECT <fd> <caller> <url>` line to
the server's listen pipe. Client `write` calls become server `read` calls and
vice versa, so scheme servers are ordinary cooperative processes. Registrations
are owned by a PID and removed when that process exits or calls
`schemeunregister`. A server can call `shmbind(fd, region)` on the connection
descriptor to replace the pipe buffer with a retained shared-memory byte ring.
The kernel keeps its own frame references, so the region can be destroyed after
binding and bulk messages are no longer limited to 4 KiB. `ioctl` exposes the
same buffer occupancy through `FIONREAD`/`FIONWRITE`, while file descriptors
support size, truncate and sync requests against the FAT16 backing.

`kernel/src/vfs.rs` supplies the Unix-facing view of the same resources. Its
mount table maps `/` to the FAT16 data disk, `/sys` and `/net` to the built-in
schemes, and `/srv/<name>` to each registered ring-3 scheme. Paths are
normalized component by component, so `.` and `..` work while an absolute path
can never climb above `/`. `open` resolves a path to a scheme URL before the
normal descriptor path; `stat` and `readdir` provide inode-like metadata and
directory listings without duplicating file data; `chdir` and `getcwd` keep a
per-process working directory in the PCB, inherited by `fork` and preserved by
`exec`. The shell has matching `cd`, `pwd`, `stat`, `mounts` and `ls [path]`
commands.

Signals are the first step beyond the polling model: `signal`, `kill` and
`sigreturn` are handled in `kernel/src/signal.rs` and `kernel/src/usermode.rs`.
Delivery rewrites the saved `int 0x80` or timer frame so the next `iretq` enters
a user-space handler with the interrupted register state copied to the user
stack. This keeps the kernel free of handler code and needs no second scheduler
path; the handler resumes its caller with `sigreturn`.

The framebuffer device keeps the same discipline: `fbinfo` returns geometry as
plain integers, and `fbcommit` validates a user-owned shadow buffer before
copying it into the kernel's VBE framebuffer. Ring 3 never receives the kernel's
framebuffer pointer, so the display device needs no user-accessible alias into
kernel memory. On top of that pixel backend, `kernel/src/compositor.rs` owns a
single display shadow plus a private backing image for each clipped window.
Commits update only the backing image; `wincommitrect` updates a strided damage
rectangle, `winsetlayer` restacks windows, `winsettitle` updates kernel-drawn
chrome, `winbindshm` swaps in retained shared-memory frames as a backing image,
and `winpresent` repaints intersecting windows in layer order before blitting the
shadow. Every window has a border and title bar drawn by the kernel
after its content, so a guest cannot spoof or omit them. `kernel/src/input.rs`
mirrors decoded keyboard, mouse and serial events into a separate ring-3 queue,
so guests get press/release transitions, modifiers and pointer coordinates
without consuming shell history. Each window also owns a bounded mailbox: any
process can post a short message, while only the owner can read it.
`kernel/src/shm.rs` also provides bounded, reference-counted shared-memory
regions: a process creates a region, other processes map the same frames into
their own address spaces, and the frames survive until the owner destroys the
region and every mapping is gone. Shared mappings are tracked outside normal
user pages, so `fork` never turns them into copy-on-write pages. A window bound
with `winbindshm` retains its own frame references, so its pixels survive region
destruction and process exit until the window is released. `pipe()` adds a
bounded byte stream to the descriptor table. Its endpoints are reference-counted
handles cloned by `fork`, so a parent and child can stream messages through the
same `read`/`write` syscalls used for scheme resources; EOF appears when the last
write end closes.

The shell also borrows from Redox's Ion shell: **Up**/**Down** recall command
history (32 entries, with duplicate suppression), **Tab** completes command
names and, for `cat`, `open`, `play`, `rm` and `run`, FAT16 file names. Completion
only rewrites the line when there is exactly one candidate.

`kernel/src/pkg.rs` adopts the dependency-graph model from Redox's
[pkgutils](https://gitlab.redox-os.org/redox-os/pkgutils) rather than its old
flat "installed = true" flags: validated `[host:|target:]recipe[.suffix]` names,
manual vs. automatic install flags, protected packages, dependency and reverse
dependency sets, and install/uninstall resolution that refuses to break a live
dependent. The graph logic is kept separate from the shell so a real backend can
replace the compiled-in catalog later.

The catalog entries carry recipes shaped like Redox
[cookbook](https://gitlab.redox-os.org/redox-os/cookbook)'s `recipe.toml`: a
`source` (`builtin`, `archive:<file>` or `meta`), a build kind, and — crucially —
**separate build-time and runtime dependency lists**, just as cookbook splits
`[build] dependencies` from `[package] dependencies`. Only the runtime list
drives the install graph; the build list is metadata for a future build backend.
`desktop` is a meta package (cookbook's `groups` idea): no source, no build, only
a dependency group. `pkg recipe <name>` exposes the whole record. Cookbook's git
sources, tarball checksums, build templates and cross-compilation scripts are
deliberately omitted — there is no in-guest compiler or package fetcher here.

`kernel/src/pkgar.rs` takes the archive idea from Redox's
[pkgar](https://gitlab.redox-os.org/redox-os/pkgar): a reproducible, hashed
header and fixed-size file entries that can be integrity-checked independently
of the data. GTos uses its own `.pkga` layout — a 136-byte header, 308-byte
entries and SHA-256 instead of Blake3/Ed25519 — so it is **not** binary-compatible
with Redox archives. This is deliberate: the kernel has no signature scheme or
remote repository, so it keeps only the auditable container shape (entry-table
hash, per-file hash, checked ranges, validated paths) and leaves signing to a
future backend. `pkg pack` builds such an archive from FAT files and `pkg
archive` verifies one; the host `cargo run --bin pkgar` produces the same bytes.

## Boot logo

The GT logo (a yellow disc with a blue ring and red "GT" letters) is embedded as
a 128x128, 2-bpp bitmap in `kernel/src/logo_data.rs`. `tools/gen-logo.ps1`
regenerates it from `tools/logo-source.jpg`; the near-white picture background
becomes palette index 0, which `kernel/src/logo.rs` treats as transparent. At
boot, `logo::draw_boot` asks `console::reserve_bitmap` to clear and reserve a
strip at the top of the framebuffer, then paints the logo there magnified to
about a quarter of the screen height. In VGA text mode there is no bitmap to
draw, so the text banner stands alone. The `logo` command repaints the bitmap at
its boot position, which also restores it after `clear`.

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
