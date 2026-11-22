# Porting KDE Plasma Desktop to GTos: feasibility plan

This document records what would be required to use
[`KDE/plasma-desktop`](https://github.com/KDE/plasma-desktop) as the desktop
environment of GTos. It is written as the first step of a real port assessment,
not as a claim that Plasma can currently run.

## Verdict

**Plasma cannot be built or run on GTos in its present form.** This is not a
matter of missing glue code: Plasma is a C++/QML application suite that assumes
a general-purpose Unix-like operating system. GTos is a `no_std`, single-address-
space Rust kernel with a framebuffer text console.

## Dependency gap

| Plasma stack layer | What it needs | What GTos has today |
| --- | --- | --- |
| `plasma-desktop` itself | C++17/20, Qt 6 (Core/Gui/Qml/Quick), KDE Frameworks 6, QML/JS engine | a Rust-only C runtime (`errno`, heap, string routines, `atexit`); no C++ ABI or dynamic linker |
| `plasma-workspace`, `libplasma`, KWin | window manager, compositor, session management | a basic clipped window compositor and input device; a ring-3 session manager starts autostart targets, dispatches shell session requests, serves a small registered session bus and tracks managed child services with restart limits, but there is no full session manager or window protocol |
| Display server | X11 or Wayland; KWin uses DRM/KMS, EGL, OpenGL, libinput | a linear framebuffer with a ring-3 commit ABI, clipped windows and decoded keyboard/mouse events; no DRM/KMS or Wayland |
| IPC | D-Bus, session bus, `kded` daemon, polkit | a small `int 0x80` syscall gate plus registered schemes, pipe-backed streams and a `session:` bus with verified runner/KCM/managed-service methods; no full service manager or policy layer |
| Process model | POSIX processes, threads, shared memory, per-process page tables | per-process page tables, preemptive ring-3 processes and reference-counted shared memory; no threads yet |
| C runtime | glibc, pthreads, dynamic loader, C++ standard library | a Rust C runtime with `errno`, heap, string routines and `atexit`; no libc, threads or loader |
| Widget/toolkit layer | Qt Widgets/QML, style engine, accessibility | a clean-room Rust canvas, bitmap text, reusable layouts, themed label/button/text-input widgets, nested declarative views, multi-line text view/editor, scrolling, focus routing, recursive accessibility introspection and a clean-room panel/launcher/clock/tray shell with VFS-backed launch targets, applet actions, session requests and persistent focus memory; no full style engine or assistive-technology backend |
| System services | udev, NetworkManager, PipeWire/PulseAudio, systemd-style units | ATA/FAT16, E1000/RTL8139 drivers, AC'97 driver |
| Filesystem | POSIX VFS, `~/.config`, XDG directories | FAT16 root directory only |

Building Plasma also means building `plasma-workspace`, `libplasma`, KWin,
Qt and KDE Frameworks first. The repository's own README states that it
contains only desktop-form-factor components and points to several other
repositories for the generic parts.

## Non-negotiable project constraints

- **Clean room.** No Plasma, Qt or KDE source may be vendored or copied into
  this repository. Existing documentation already commits to clean-room
  implementations for other projects.
- **100% Rust.** The GitHub language bar must stay 100% Rust. Importing a
  C++/QML codebase would violate this.
- **`no_std` bare metal.** No Linux, no libc, no host process model.

## Phases required for a genuine port

1. **Userspace foundation** — virtual memory with per-process page tables,
   preemptive scheduling, ELF loader, POSIX-like syscalls and a C ABI shim are
   in place; a dynamic linker (or a Rust-only equivalent with an entirely new
   shell) is still missing.
2. **System infrastructure** — a VFS with namespaces, shared memory and an IPC
   bus with D-Bus-like semantics are in place; a ring-3 session manager now
   handles autostart, state transitions, shell session requests, a small
   registered service bus and named child services with restart limits, but a
   full service manager and policy layer are still missing.
3. **Graphics stack** — a compositor with DRM/KMS or a virtual framebuffer
   backend, a 2D/OpenGL-compatible drawing API, input abstraction and a
   window protocol equivalent to Wayland.
4. **Toolkit layer** — a clean-room reimplementation of Qt/Quick and the
   KDE Frameworks APIs Plasma is written against, in Rust.
5. **Plasma-compatible shell** — reimplement the panel,applets, KCMs,
   `plasmashell`, runners and layout templates against that Rust toolkit.
6. **Hardware enablement** — GPU drivers, audio server, network manager.
7. **Integration and testing** — session startup, power management, and
   the full widget/theme ecosystem.

Each phase is a substantial project on its own; the whole is a multi-year,
multi-engineer effort and would turn GTos into a different operating system.

## Practical alternatives

- **Real KDE Plasma today:** run a Linux distribution (or another Unix-like
  OS) in the VM and use GTos as a separate research kernel. This is the only
  way to have actual Plasma on this machine.
- **Plasma-like experience in GTos:** build a clean-room Rust panel shell —
  bottom panel, launcher grid, system tray and widgets — drawn directly on
  the framebuffer. It can borrow Plasma's *visual language* without copying
  any code and keeps the 100% Rust and clean-room constraints intact. This
  is achievable in this codebase; it is not Plasma and cannot run Plasma
  applications.
- **Stay shell-only:** keep the current text console and focus on kernel
  features.

## Status

No Plasma code has been imported. Since the assessment, the kernel has gained
per-process memory, ring-3 ELF programs, signals, a guest heap, a
shadow-framebuffer commit ABI, a damage-aware layer-ordered window compositor
with per-window mailboxes and kernel-drawn title bars, reference-counted shared
memory usable as zero-copy window backing, incremental FAT16 file descriptors
with a bounded cluster cache and streaming directory descriptors, bounded
`pipe()` streams for incremental IPC, pipe-backed user-space scheme servers with shared-memory
stream transport, a mounted VFS with path resolution and metadata, a rich
input device with mouse and key press/release events, a Rust-only C runtime
with `errno`, a recycling heap, string routines and `atexit`, and the
`gtos-widgets` toolkit (clipped canvas, bitmap text, vertical/horizontal/grid
layouts, themed label/button/text-input widgets, nested declarative views,
multi-line text view and editor, scrolling, focus routing, recursive
accessibility introspection, an RAII compositor surface, and a clean-room desktop
shell with a panel, VFS-backed launcher grid and fork/exec launch handlers,
applet and session actions, UTC clock, system tray and keyboard focus cycling
with focus memory, plus a dedicated ring-3 session manager with VFS autostart,
state-file transitions, a registered runner/KCM/service bus with restart-limited
child services, and real input-driven shutdown dispatch). There is still no full
C++ ABI or dynamic linker, window protocol or complete toolkit,
and no DRM/KMS backend. Further work requires an
explicit decision to build one of the alternatives above.
