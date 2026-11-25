# Unix-like kernel rewrite (KDE enablement)

This is the implementation plan for rewriting GTos into a Unix-like OS that can
eventually host KDE Plasma. It supersedes the shell-only design. The feasibility
background is in [the Plasma port assessment](15-plasma-port.md).

Guiding constraints: keep the kernel `no_std`, bare-metal, and **100% Rust**.
No Qt/KDE/C source may be vendored. Each phase must leave the kernel bootable
and the existing checks (`cargo build`, clippy, fmt) passing.

## Phase 1 — Process foundation (current)

| Step | Deliverable | Status |
| --- | --- | --- |
| 1.1 | `address_space.rs`: allocate a fresh PML4, copy kernel mappings, map/unmap user pages, load/restore CR3 | done |
| 1.2 | `process.rs`: `Pid`, `Process` (PCB), process table, process states | done |
| 1.3 | `elf.rs`: static/PIE `PT_LOAD` loader with load bias; flat-binary loader retained | done |
| 1.4 | Syscalls `getpid`, `brk`, `yield`, `fork`, `exec` and `waitpid`; per-process fd tables | done |
| 1.5 | `context.rs` + `scheduler.rs`: round-robin switching on `yield`/`exit` and PIT ticks | done |
| 1.6 | `fork` shares user pages copy-on-write; `exec` replaces the image in place; `waitpid` polls and reaps | done |
| 1.7 | reference-counted user and page-table frames released on exit, reap and `exec` | done |
| 1.8 | `sleep(milliseconds)` and `uptime()` syscalls backed by the PIT tick | done |
| 1.9 | `guest/`: `no_std` Rust ELF binary built by `rustc` and seeded on the data disk | done |
| 1.10 | `guest/gtos-sys`: syscall bindings and a `gtos_main!` entry-point macro | done |
| 1.11 | physical frame pool from the bootloader memory map, with recycling | done |
| 1.12 | `clock_gettime()` syscall: monotonic nanoseconds from PIT ticks + TSC | done |
| 1.13 | `realtime()` syscall: Unix seconds from the CMOS RTC | done |
| 1.14 | `write` and `lseek` on file scheme descriptors; FAT16 flush on write | done |
| 1.15 | apply `R_X86_64_RELATIVE` relocations when loading PIE ELF guests | done |
| 1.16 | `signal`, `kill` and `sigreturn` with synthetic user-stack handler frames | done |
| 1.17 | `gtos_heap!`: lock-free `brk`-backed global allocator for `no_std` guests | done |
| 1.18 | `input`: mirrored nonblocking ring-3 events with key press/release, modifiers, PS/2 mouse and serial bytes | done |
| 1.19 | Damage-aware, layer-ordered window compositor with `wincreate`/`wincommit`/`wincommitrect`/`winpresent`/`winsetlayer`/`winsettitle`/`winpost`/`winread`/`windestroy` | done |
| 1.20 | Reference-counted shared memory with `shmcreate`/`shmattach`/`shmdetach`/`shmdestroy` | done |
| 1.21 | Shared-memory-backed compositor pixels with `winbindshm` and retained frame references | done |
| 1.22 | Bounded `pipe()` streams inherited across `fork`/`exec`, with EOF on last writer close | done |
| 1.23 | `schemeregister`/`schemeunregister`: pipe-backed user-space scheme servers with duplex connections | done |
| 1.24 | VFS mount table, path resolution, `stat`/`readdir`, `chdir`/`getcwd`, and dynamic `/srv` scheme mounts | done |
| 1.25 | `shmbind`: retained shared-memory byte rings for scheme stream descriptors | done |
| 1.26 | `ioctl`: pipe/stream occupancy plus FAT file size, truncate and sync | done |
| 1.27 | Rust-only C ABI shim: `_start`, `main(argc, argv)`, raw syscalls and C-style descriptor wrappers | done |
| 1.28 | Incremental FAT16 file descriptors: cluster-at-a-time `read`/`write`, `lseek`, `truncate` and metadata | done |
| 1.29 | Bounded FAT cluster cache and descriptor-based directory streaming | done |
| 1.30 | Fuller `crt0`: `errno`, recycling `brk` heap, string/stdlib routines, `environ`, `atexit` | done |
| 1.31 | Clean-room widget toolkit foundation: clipped canvas, bitmap text, layout, label/button widgets and RAII compositor surface | done |
| 1.32 | Richer widgets: reusable layouts, clipping scroll container, text input and keyboard focus routing | done |
| 1.33 | Themed widgets, multi-line text view, declarative views and accessibility introspection | done |
| 1.34 | Nested declarative containers and a multi-line text editor with cursor navigation | done |
| 1.35 | Clean-room desktop shell panel with launcher grid, clock and system tray | done |
| 1.36 | Shell applet slots, VFS-backed launch metadata and keyboard focus cycling | done |
| 1.37 | Shell actions, real VFS launch handlers, applet/session actions and focus memory | done |
| 1.38 | Ring-3 session manager with VFS autostart, state file and input-driven shutdown | done |
| 1.39 | Registered session service bus with VFS-backed runner and KCM methods | done |
| 1.40 | Managed child services, bounded restart policy, service state file and status bus | done |
| 1.41 | Signal masks, default actions, queued delivery after `sigreturn`, and cross-process tests | done |
| 1.42 | Dirty FAT cluster write-back with LRU eviction, invalidation, sync-on-close and cross-cache streaming tests | done |
| 1.43 | `AF_UNIX`/`SOCK_SEQPACKET` `socketpair()` datagrams with boundary-preserving reads, nonblocking EOF, and fork/exec inheritance tests | done |
| 1.44 | Ring-3 CPU fault delivery as signals (`SIGSEGV`/`SIGBUS`/`SIGILL`/`SIGFPE`) with handler/resume-or-terminate tests | done |
| 1.45 | Real-time signal numbers (`SIGRTMIN..SIGRTMAX`) with 64-bit masks/pending sets and delivery tests | done |
| 1.46 | `dup(fd)`/`dup2(oldfd, newfd)` descriptor duplication with shared handles and independent cursors | done |
| 1.47 | `AF_UNIX`/`SOCK_STREAM` socketpair byte-stream endpoints | done |
| 1.48 | `shutdown(fd, how)` for socket/stream read/write half-close | done |
| 1.49 | `fcntl(F_GETFL/F_SETFL,O_NONBLOCK)` for socket/stream nonblocking I/O | done |
| 1.50 | `socketpair` `SOCK_NONBLOCK` type flag for nonblocking endpoints | done |
| 1.51 | `FD_CLOEXEC`/`F_GETFD`/`F_SETFD` with close-on-exec across `exec` | done |
| 1.52 | `SOCK_CLOEXEC` type flag for close-on-exec socket pairs | done |
| 1.53 | Socket peer credentials through `IOCTL_SOCKET_PEER_PID` | done |
| 1.54 | FIFO-queued real-time signals with `sigqueue` `siginfo` payloads and a three-argument handler ABI | done |
| 1.55 | Blocking `sigtimedwait`/`sigwaitinfo` with `siginfo` payloads, timeouts and cross-process wait tests | done |
| 1.56 | `signalfd`-style descriptor waits with blocking/nonblocking reads and payload verification | done |
| 1.57 | `poll` multiplexing across pipes, sockets and signal descriptors with timeout and `POLLNVAL` tests | done |
| 1.58 | `select` fd-bitmask multiplexing with read/write/exception sets and timeout tests | done |
| 1.59 | event-driven readiness wakeups for blocking `poll` and `select` waits | done |
| 1.60 | event-driven blocking `read` on pipes and sockets with nonblocking and EOF behavior retained | done |
| 1.61 | event-driven blocking `write` on full pipes and sockets with nonblocking and partial-write behavior retained | done |
| 1.62 | event-driven blocking `waitpid` with atomic child-exit registration, `WNOHANG` probes and status verification | done |
| 1.63 | `pipe2` with atomic `O_NONBLOCK`/`O_CLOEXEC` flags on both pipe ends | done |
| 1.64 | C ABI descriptor wrappers for `pipe`, `pipe2`, `dup`, `dup2` and `fcntl` with demo verification | done |
| 1.65 | C ABI `socketpair` and `shutdown` wrappers with datagram and half-close verification | done |
| 1.66 | Kernel-seeded `argc`/`argv` for shell `run` and syscall 9 `exec`, verified through `CABI.ELF` | done |

## Phase 2 — Kernel infrastructure
The physical frame pool, cluster-at-a-time FAT16 file `read`/`write`/`lseek`,
basic signal delivery, bounded `pipe()` streams, pipe-backed user-space scheme
servers, a VFS with mount points and inode-like metadata, shared-memory scheme
streams, descriptor `ioctl`, a bounded LRU cluster cache with dirty write-back
and descriptor-based directory streaming are already in place. Cached clusters
are flushed on eviction, explicit sync or close, and are invalidated when a
chain is freed or reused. Signals now also support explicit ignore/default
dispositions, fatal `SIGINT`/`SIGTERM`/`SIGKILL` actions, per-process masks with
pending sets, and immediate delivery of signals queued by a handler after
`sigreturn`. Local `AF_UNIX` `socketpair()` now supports both `SOCK_SEQPACKET` datagrams and
`SOCK_STREAM` byte streams, with fork/exec inheritance, nonblocking reads and EOF on
peer close. Ring-3 CPU faults now enter the same signal path: `#DE` maps to `SIGFPE`,
`#UD` to `SIGILL`, `#GP` to `SIGBUS`, and page faults map to `SIGSEGV` or
`SIGBUS` depending on the error code. A handler can resume through `sigreturn`;
ignored synchronous faults still terminate to avoid fault loops. Real-time
signal numbers in `SIGRTMIN..SIGRTMAX` are accepted with 64-bit signal
masks/pending sets; unlike standard signals, they keep one queued `Siginfo`
per send and arrive in strict send order. Socket pairs also accept
`SOCK_NONBLOCK` and `SOCK_CLOEXEC` in their type flag, and
`IOCTL_SOCKET_PEER_PID` exposes the peer process across socketpair and scheme
connections. Signal wait APIs now consume blocked signals synchronously with
`sigtimedwait`, `sigwaitinfo` and `sigwait`, including `Siginfo` payloads and
finite timeouts. `signalfd` exposes the same pending set as a descriptor whose
blocking `read` yields one `Siginfo` record at a time. `poll` now multiplexes
readiness across pipes, sockets, signal descriptors and file-like resources with
a stable 16-byte ABI and finite or infinite timeouts. `select` maps those answers
onto 1024-bit fd sets with read, write and exception bitmasks. Blocking waits
now arm a readiness registration under the process-table lock and wake directly
when a descriptor or signal changes readiness, instead of sleeping in one-tick
PIT slices. Ordinary blocking `read` uses the same registration, so an empty pipe
or socket parks until a peer writes or closes; `O_NONBLOCK` still reports `ERROR`
and end-of-file still reports `0`. A full blocking pipe or datagram socket parks
until a reader frees capacity, while `O_NONBLOCK` and partial writes keep their
existing behavior. `waitpid` checks the child's state and arms the caller's
readiness registration in one critical section, so a parent parks directly on
the child's exit instead of polling with `yield`; `WNOHANG` keeps an explicit
nonblocking status probe for service managers. `pipe2` applies `O_NONBLOCK` and
`O_CLOEXEC` to both ends before either descriptor is returned, matching the
flag support already available for `socketpair`.

## Phase 3 — Graphics and input
A first virtual framebuffer device is exposed through `fbinfo` and `fbcommit`:
ring 3 owns a shadow buffer and the kernel blits a validated copy to the VBE
display. `kernel/src/compositor.rs` adds a clipped window model and an explicit
present syscall, and `input` mirrors decoded keyboard, mouse and serial events
into a separate ring-3 queue with press/release transitions, modifiers and
absolute pointer coordinates. Each window also has a private backing image, so
`wincommitrect` can redraw a strided subrectangle and `winsetlayer` can restack
windows without tearing down the scene. Each window also has a bounded mailbox
so any process can post a short message that only the owner can read, plus a
kernel-drawn border and title bar with `winsettitle`. `shm.rs` adds reference-counted
shared-memory regions, and `winbindshm` can use them as window backing stores so
`winpresent` repaints without another pixel copy. `pipe()` gives processes a
bounded byte stream through the same descriptor syscalls; `pipe2()` creates one
with `O_NONBLOCK` and/or `O_CLOEXEC` already applied to both ends, and
`schemeregister`/`schemeunregister` let a ring-3 process serve a URL scheme over
duplex stream connections, and `shmbind` can attach a retained SHM byte ring to
one of those connections for payloads larger than the pipe buffer. Built-in
scheme descriptors still snapshot resources. Still needed: zero-copy guest
mapped scheme buffers and DRM/KMS-style abstractions.

## Phase 4 — Userspace runtime
The Rust syscall crate and `gtos-cabi` runtime are in place: `_start` with
`main(argc, argv)`, `errno`, a recycling `brk`-backed heap, clean-room string and
`stdlib` routines, `puts`, an empty `environ` and LIFO `atexit` handlers. The C
ABI also exposes `pipe`, `pipe2`, `dup`, `dup2`, `fcntl`, `socketpair` and
`shutdown`, verified by `CABI.ELF` through a pipe round trip, flag checks, a
datagram exchange and half-close EOF. The kernel now seeds a conventional C
argument frame from shell tokens or a length-prefixed `exec` block, so
`main(argc, argv)` sees the real command line instead of a synthetic one. The
`gtos-widgets` adds a clipped canvas, shared 8x8 bitmap text, reusable
vertical/horizontal/grid layouts, themed label/button/text-input widgets, a
multi-line text view and editor, nested declarative views, scrolling with focus
routing, recursive accessibility introspection, event translation and an RAII
compositor surface. Still needed: a dynamic linker / C++ ABI, a full style
engine, assistive-technology integration and a clean-room Qt-compatible toolkit
in Rust.

## Phase 5 — Desktop shell
A clean-room panel, launcher grid, clock, system tray and applet slots now run
as guest widgets. Launch targets are parsed from `APPS.TSV` through the mounted
VFS; selecting one emits a shell action that the guest host handles by forking
and `exec`ing the target program. Applet and tray slots emit their own actions,
session tray slots dispatch session requests to the host, and the launcher
remembers the panel focus to restore when it closes. Tab/Shift+Tab cycles focus
across panel controls and the launcher; `gtos-widgets` supplies its canvas,
theme, nested view, layout, widget, scrolling, text-editing, accessibility and
event layer. A separate `SESSION.ELF` ring-3 process reads VFS autostart
metadata, starts target programs as children, publishes `SESSION.ST` lifecycle
state, and dispatches real shell input events to launch, applet, tray and
session handlers. It also registers a `session:` scheme under `/srv/session` and
answers `runner/query`, `runner/launch`, `kcm/open`, `service/list`,
`service/status` and `service/restart` from VFS-backed metadata, verified by a
forked ring-3 client over kernel pipes. Named child services run with bounded
restart limits and publish fixed-width state in `SERVICES.ST`; the seeded
`SERVICE.ELF` exits nonzero twice, proving real death/restart tracking.
Plasma-compatible applets, KCMs, runners and a full service manager remain
future work.

## Phase 6 — Hardware/services
GPU, audio server, network manager and full power management remain future work;
the clean-room session manager now covers ring-3 autostart, state publication,
shell-driven session requests, a small D-Bus-like session bus and restart-limited
child services.

## Original baseline (before this rewrite)
The kernel originally ran one hard-wired flat binary at `USER_BASE` inside its
**own** page tables, with five custom syscalls and three global atomics
(`KERNEL_RSP`, `EXIT_CODE`, `MAPPED`) as its entire "process state". The reusable
pieces were the GDT/TSS, the `int 0x80` gate and the register-saving assembly
stubs in `usermode.rs`.

Phase 1 has now replaced that with per-process PML4 tables, PCBs, an ELF64
loader and extra syscalls. The PML4 user slot moved from entry 1 to entry 6
because the bootloader identity-maps entries 0–5; using entry 1 previously
made the kernel lose its own mappings on `CR3` reload.

Phase 1 also adds private ring-0 stacks, a saved register `Context` and a
preemptive round-robin scheduler. `yield`, `sleep`, `exit` and the 100 Hz PIT call
into `scheduler.rs`, which saves the live register frame into the outgoing process
and resumes another with `iretq`. Ticks that interrupt ring 0 keep time and wake
sleepers. `fork`
shares the caller's user pages copy-on-write and resolves write faults into
private frames, `exec` installs a new image and jumps to it with
`resume_context`, and `waitpid` returns a child's status and reaps it. The
built-in `run fork`, `run preempt`, `run sleep` and `run exec` demos exercise these paths;
`run`, `run HELLO.BIN` and `run scheme` still work.

The seeded FAT image now includes `HELLO.ELF`, a hand-assembled static ELF64
program, so `run HELLO.ELF` exercises the loader from disk as well as `exec`.
It also includes `RUSTDEMO.ELF`, a real `no_std` Rust program built by the host
toolchain: `guest/` is compiled for `x86_64-unknown-none` with `rustc` as the
linker and seeded by the root build script. This proves the syscall ABI and ELF
loader work for compiler-produced binaries, not just hand-assembled test images.
The guest uses `gtos-sys`, a small `no_std` library with typed syscall wrappers
and a `gtos_main!` macro that supplies `_start` and the panic handler. The disk
also seeds `CABI.ELF`, built from `guest/cabi-demo` against `gtos-cabi`, a
Rust-only C runtime. It supplies the raw `int 0x80` entry, a C-style
`main(argc, argv)` trampoline, descriptor/process wrappers that map kernel
errors to `-1` and set `errno`, a recycling `malloc` heap, classic string and
`stdlib` routines, `puts` and `atexit`, proving the ABI without importing libc
or any C source. `SESSION.ELF`, built from `guest/session`, is another `no_std`
Rust guest that acts as the session host: it reads VFS metadata, spawns autostart
children, publishes lifecycle state, serves a registered session bus, manages
restart-limited child services and handles real shell input. `SERVICE.ELF`, built
from `guest/service`, is the tiny no_std managed service that announces
readiness and exits nonzero so the restart policy is exercised end to end.
`RUSTDEMO.ELF`
additionally links `gtos-widgets`, which paints a
real compositor window using the same public-domain font as the kernel console
and dispatches translated synthetic pointer/key events to a hit-tested button.
Descriptors belong to the process that opened them: `fork` copies the table and
`exec` preserves it. Frames come from a physical frame pool built from the bootloader's `Usable`
regions, excluding the heap and the first MiB, and are returned to the pool when
the last owner exits, is reaped, or replaces its image. `sleep` blocks a process until a
PIT deadline while peers keep running, and `uptime` exposes the same tick as
milliseconds. `clock_gettime()` interpolates between PIT ticks with the TSC to
give monotonic nanoseconds; the first PIT period calibrates the TSC, so no CPU
frequency has to be configured. `realtime()` reads the battery-backed CMOS RTC
and converts BCD or binary date registers into Unix seconds. Signal delivery is
the first polling-free wakeup mechanism: a queued signal rewrites the next
syscall or timer frame so `iretq` enters a registered ring-3 handler instead of
the interrupted instruction. The handler receives the signal number and a copy
of the saved context on its own stack, and `sigreturn` restores that context.
The Rust guest exercises the full path by signalling itself and printing
`handler entered` before returning to `main`. It also blocks a signal, observes
it in the pending set, unblocks it, installs `SIG_IGN`, queues a signal from a
handler for immediate delivery after `sigreturn`, and forks children that prove
cross-process handler delivery and fatal default actions through pipes and
`waitpid`. `gtos_heap!` adds a lock-free
`brk`-backed allocator, so the same guest can build a `Vec` and use `format!`
without `std`; the guest verifies that the break moved and prints
`guest allocator ok`. The guest build strips debug sections so the loadable ELF
stays well inside the kernel's program-size limit.
