# User mode (ring 3)

Programs run with real hardware isolation. `kernel/src/usermode.rs` installs its
own GDT (null, kernel code/data, user code/data and a TSS). The TSS
`privilege_stack_table[0]` points at a 128 KiB ring-0 stack private to the running
process, and `interrupt_stack_table[0]` at a separate 16 KiB double-fault stack
(post-06). Each process maps a private program window plus stack at PML4 entry 6
(`0x0000_0300_0000_0000`), far from the kernel's identity map and the
physical-memory window. Every process owns its own level-4 page table; the
kernel mappings are copied and the user slot is empty until the program is
loaded.

The only way in is `int 0x80`, installed as a DPL-3 gate. The handler is written
in `global_asm!`: it saves the user registers, calls the Rust dispatcher and
`iretq`s back. Fifty-six system calls (0 through 55) exist today:

| Number | Call | Purpose |
|--------|------|---------|
| 0 | `exit(status)` | terminate the program |
| 1 | `write(fd, buffer, length)` | write to console fd 1/2 or a writable scheme descriptor |
| 2 | `open(url, length)` | open a scheme resource, returning fd >= 3 |
| 3 | `read(fd, buffer, length)` | read bytes from a scheme descriptor; one `Siginfo` record from a signal descriptor |
| 4 | `close(fd)` | release a scheme descriptor |
| 5 | `getpid()` | return the current process identifier |
| 6 | `brk(address)` | grow or query the process heap |
| 7 | `yield()` | run the async executor, then switch to another process if ready |
| 8 | `fork()` | share pages copy-on-write and return 0 in the child |
| 9 | `exec(image, length, args)` | replace the current image from a user buffer; `args` is zero or a length-prefixed packed command line |
| 10 | `waitpid(pid, status, options)` | wait for a child's exit status and reap it; parks until it exits, or returns zero with `WNOHANG` |
| 11 | `sleep(milliseconds)` | block until the PIT deadline, letting peers run |
| 12 | `uptime()` | return milliseconds since the PIT was initialized |
| 13 | `clock_gettime()` | return monotonic nanoseconds interpolated with the TSC |
| 14 | `realtime()` | return Unix seconds from the CMOS RTC |
| 15 | `lseek(fd, offset, whence)` | move a descriptor's read/write cursor |
| 16 | `signal(signum, handler)` | install a handler, returning the previous one |
| 17 | `kill(pid, signum, info)` | queue a signal; a null `info` is a plain kill, otherwise `info` is a `Siginfo*` carrying a queued payload |
| 18 | `sigreturn(frame)` | restore the interrupted context and leave a handler |
| 19 | `fbinfo(info)` | copy framebuffer geometry into a user buffer |
| 20 | `fbcommit(buffer, length)` | blit a user shadow framebuffer to the display |
| 21 | `input(event)` | poll one decoded input event, polling scancodes first |
| 22 | `wincreate(description)` | reserve a clipped kernel-composited window |
| 23 | `wincommit(id, buffer, length)` | copy a user image into a window |
| 24 | `winpresent()` | blit the composited shadow frame to the display |
| 25 | `windestroy(id)` | release a window owned by the caller |
| 26 | `wincommitrect(id, buffer, damage)` | update a window subrectangle from a strided buffer |
| 27 | `winsetlayer(id, layer)` | change a window's stacking layer |
| 28 | `winpost(id, buffer, length)` | queue a message for a window, even across processes |
| 29 | `winread(id, message, capacity)` | read one message from an owned window |
| 30 | `winsettitle(id, buffer, length)` | set the title shown in a window's title bar |
| 31 | `shmcreate(length)` | reserve a shared-memory region |
| 32 | `shmattach(id)` | map a shared region and return its base address |
| 33 | `shmdetach(id)` | unmap a shared region from the caller |
| 34 | `shmdestroy(id)` | retire a shared region owned by the caller |
| 35 | `winbindshm(id, region)` | use a shared region as a window's pixel store |
| 36 | `pipe(fds)` | write a read/write stream descriptor pair into a user array |
| 37 | `schemeregister(name, length, notify_fd)` | serve a scheme from a ring-3 process |
| 38 | `schemeunregister(name, length)` | stop serving a scheme owned by the caller |
| 39 | `stat(path, length, info)` | write VFS metadata into a fixed-size struct |
| 40 | `readdir(path, length, buffer)` | write NUL-separated directory names into a fixed buffer |
| 41 | `chdir(path, length)` | set the caller's current VFS directory |
| 42 | `getcwd(buffer, length)` | write the caller's current VFS directory |
| 43 | `shmbind(fd, region)` | use a shared region as a scheme stream's byte transport |
| 44 | `ioctl(fd, request, arg)` | query or adjust a descriptor (FIONREAD, FIONWRITE, file size/truncate/sync, socket peer PID) |
| 45 | `sigprocmask(how, set, oldset)` | block, unblock or replace the signal mask and return the old mask |
| 46 | `sigpending(set)` | write the pending signal set into a user buffer |
| 47 | `socketpair(domain, type, fds)` | create two connected `AF_UNIX` endpoints (`SOCK_SEQPACKET` or `SOCK_STREAM`); `SOCK_NONBLOCK` and `SOCK_CLOEXEC` may be ORed into `type` |
| 48 | `dup(fd)` | duplicate a descriptor, sharing underlying handles |
| 49 | `dup2(oldfd, newfd)` | duplicate a descriptor to a specific number, closing `newfd` first if open |
| 50 | `shutdown(fd, how)` | half-close a socket/stream (`SHUT_RD`, `SHUT_WR`, `SHUT_RDWR`) |
| 51 | `fcntl(fd, cmd, arg)` | get/set fd flags (`F_GETFD`/`F_SETFD`, `FD_CLOEXEC`) and file status flags (`F_GETFL`/`F_SETFL`, `O_NONBLOCK`) |
| 52 | `sigtimedwait(set, info, timeout_ms)` | wait for a blocked signal, write its `Siginfo`, and return its number; `u64::MAX` is an infinite timeout |
| 53 | `signalfd(set, flags)` | create a descriptor whose reads consume pending signals from `set`; `SFD_NONBLOCK` and `SFD_CLOEXEC` may be ORed into `flags` |
| 54 | `poll(fds, length, timeout_ms)` | wait for readiness on an array of 16-byte `PollFd` records; `u64::MAX` is an infinite timeout |
| 55 | `select(args)` | classic fd-bitmask multiplexing through a pointer to a 40-byte argument block; `u64::MAX` is an infinite timeout |
| 56 | `pipe2(fds, flags)` | create a pipe with `O_NONBLOCK` and/or `O_CLOEXEC` applied to both ends |

Every pointer argument is checked against the process's user region before it
is dereferenced, so a program cannot name kernel memory even with a wrapping
address. A fault taken in ring 3 is contained: the exception handler notices the
faulting `CS` has RPL 3, reports it and terminates the program, handing control
back to the shell instead of halting the machine.

`run` with no argument executes a hand-assembled demo program built in memory;
`run HELLO.BIN` loads a flat binary off the data disk. The seeded image also
contains `HELLO.ELF`, a hand-assembled static ELF64 program, and
`RUSTDEMO.ELF`, a real `no_std` Rust program compiled by the host toolchain.
Its `guest/gtos-sys` library exposes the syscall ABI, a `gtos_main!` entry-point
macro and an optional `gtos_heap!` global allocator, so a guest needs only a few
lines of Rust before it can use `Vec`, `String` and `format!`. Files beginning with
the ELF magic are loaded by `kernel/src/elf.rs`, which maps `PT_LOAD` segments
into the process's private address space. PIE binaries are relocated by a load
bias, and `R_X86_64_RELATIVE` entries in the dynamic table patch pointer-sized
GOT and `.data.rel.ro` values so calls through the GOT reach the right code. A
flat binary is still linked against the user base, so it is position-dependent by
design. Flat binaries are limited to the legacy 64 KiB code window; ELF images
are mapped segment by segment and may be up to 1 MiB. A second seeded image,
`CABI.ELF`, is compiled from Rust against the `gtos-cabi` runtime: it enters through
`main(argc, argv)`, calls the raw `int 0x80` entry and uses C-style `open`,
`read`, `write`, `close`, `pipe`, `pipe2`, `dup`, `dup2`, `fcntl`, `socketpair`,
`shutdown` and PID wrappers, all without importing libc. The shell and `exec`
syscall seed the initial stack with the real command line and enter `_start`
with `argc` in `rdi` and `argv` in `rsi`; with no explicit arguments the image
receives a synthetic `argv[0]` of `"gtos"`. The shim
also supplies `errno`/`__errno_location`, a `brk`-backed `malloc`/`calloc`/
`realloc`/`free` heap with recycling, clean-room `string.h`/`stdlib.h` routines,
`puts`, an empty `environ` and `atexit` handlers that `exit` runs on the way out.
`RUSTDEMO.ELF` also links `gtos-widgets`, a clean-room Rust toolkit with a
clipped pixel canvas, the same public-domain 8x8 bitmap font as the kernel
console, widget layout, `Label`/`Button` controls, an RAII compositor `Surface`
that owns the window and its shadow image, reusable vertical/horizontal/grid
layouts, a clipped scrolling container with focus routing, and a caret-equipped
`TextInput`. It also has theme tokens with per-widget style overrides, a
multi-line `TextView`, declarative `View` descriptions compiled by
`ViewBuilder`, nested row/column/grid containers with recursive accessibility
introspection, and a multi-line `TextArea` editor with cursor navigation. Its
`shell` module adds a clean-room panel, launcher grid, UTC clock, system tray,
applet slots and Tab/Shift+Tab focus routing; launcher labels and executable
targets come from the seeded `APPS.TSV` file through the mounted VFS. Shell
controls emit owned `ShellAction` values for launch, applet, tray and session
requests; the guest host handles launch actions by forking and `exec`ing the
metadata target, and remembers panel focus while the launcher is open. The
seeded metadata format is clean-room `id<TAB>label<TAB>target`; a missing target
falls back to the id. `AUTO.TSV` uses `#` comments and one launcher id per line.
`RUNNERS.TSV` and `KCMS.TSV` use the same three-column format for D-Bus-like
runner and configuration-module registries; their `.KCM` targets are ordinary
VFS files. `SERVICES.TSV` adds a fourth column for a managed service's restart
limit. `SESSION.ELF` is a separate ring-3 process: it reads that metadata
through the VFS, starts autostart targets as children, publishes a registered
`session:` service bus at `/srv/session`, writes `starting`/`running`/`stopping`
to `SESSION.ST`, and dispatches keyboard shell actions to launch, applet, tray
and session handlers. The bus answers `runner/query`, `runner/launch`,
`kcm/open`, `service/list`, `service/status` and `service/restart` calls over
kernel pipes, so service clients are real ring-3 processes rather than
in-process callbacks. Managed services run as children with a bounded restart
policy; their fixed-width lifecycle table is published as `SERVICES.ST`. The
seeded `SERVICE.ELF` announces readiness, exits nonzero, and is restarted until
its limit is reached, proving the manager observes real process deaths. No
Plasma or Qt code is involved.

```
gtos> run
running demo program (71 bytes) in ring 3 ...
Hello from ring 3! (via int 0x80)
demo program exited with status 0
gtos> run HELLO.BIN
running HELLO.BIN (77 bytes) in ring 3 ...
Hello from HELLO.BIN running in ring 3!
HELLO.BIN exited with status 0
gtos> run scheme
running scheme demo (416 bytes) in ring 3 ...
GTos 0.1.0
x86_64 bare-metal kernel
written in Rust, no std, no libc
scheme demo exited with status 0
gtos> run fork
running fork demo (170 bytes) in ring 3 ...
child: yielded and exited
parent: child reaped
fork demo exited with status 0
gtos> run preempt
running preemption demo (116 bytes) in ring 3 ...
PPP...CCC...PPP...
preemption demo exited with status 0
gtos> run sleep
running sleep demo (159 bytes) in ring 3 ...
parent: woke after sleep
child: woke after sleep
sleep demo exited with status 0
gtos> run exec
running exec demo (102 bytes) in ring 3 ...
Hello from ring 3! (via int 0x80)
exec demo exited with status 0
gtos> run hello.elf
running hello.elf (197 bytes) in ring 3 ...
Hello from HELLO.ELF running in ring 3!
hello.elf exited with status 0
gtos> run rustdemo.elf
running rustdemo.elf (size varies) in ring 3 ...
Hello from a Rust guest in ring 3!
monotonic clock advanced
wall clock available
file write/read ok
heap vector: [1, 2, 3, 4]
guest allocator ok
framebuffer device ok
window damage ok
window stacking ok
window decorations ok
window shm ok
window ipc ok
window compositor ok
widget toolkit ok
widget layout ok
widget view ok
widget nested ok
widget editor ok
shell panel ok
shell apps ok
shell focus ok
shell actions ok
shell launch ok
shared memory ok
streaming ipc ok
ioctl ok
stream file ok
fat writeback ok
directory stream ok
vfs ok
scheme server ok
scheme shm ok
waiting for input event
input press: A shift
input release: A
input device ok
handler entered
signal delivered
masked handler entered
signal mask ok
signal ignore ok
queued first handler
queued handler entered
signal queue ok
child handler entered
signal cross process ok
signal default action ok
signal kill ok
rustdemo.elf exited with status 0
hello
cabi ok
crt ok
cabi.elf exited with status 0
gtos> session
running SESSION.ELF (size varies) in ring 3 ...
session manager starting
Hello from HELLO.BIN running in ring 3!
session autostart ok
session service demo start <pid>
managed service ready
managed service exiting
session service demo restart 1 status 7
session service demo start <pid>
managed service ready
managed service exiting
session service demo restart 2 status 7
session service demo start <pid>
managed service ready
managed service exiting
session service demo stopped status 7 restarts 2
session services ok
Hello from HELLO.BIN running in ring 3!
session bus ok
session kcm ok
session kcm theme: theme=dark
session waiting for input
session request: shutdown
session manager shutdown ok
SESSION.ELF exited with status 0
```

Managed-service pids vary by boot. The bus verification forks a client that
queries a runner, opens a KCM, launches `term`, and checks `service/list`,
`service/status` and the exhausted `service/restart` policy; a second
`HELLO.BIN` banner appears before `session bus ok`. The session manager focuses
its `PWR` tray slot at startup, so pressing Enter dispatches a shutdown request;
the shell also sees that Enter as a blank line after the session exits. Its
state files can be inspected with `cat SESSION.ST` and `cat SERVICES.ST`.

`run scheme` is the built-in demonstration of the scheme syscalls: it opens the
`sys:/version` resource, reads it into its own buffer, writes that buffer to the
console and closes the descriptor. The same `open`/`read`/`close` path is
available to any program loaded from the data disk. Disk files are writable
incremental streams: `open file:/NAME.TXT` creates an empty file if needed, and
`read`/`write` walk only the FAT16 clusters they touch instead of snapshotting
the whole file into memory. `lseek` supports `SEEK_SET`, `SEEK_CUR` and
`SEEK_END`, so a program can reread what it just wrote without reopening the
file, and the `ioctl` size/truncate requests act on the on-disk directory entry.
A bounded LRU cluster cache avoids rereading hot clusters; writes mark clusters
dirty and are written back on cache eviction, `IOCTL_FILE_SYNC`, or descriptor
close, while freeing or reusing a cluster invalidates its cached bytes.
`open file:/` returns a directory descriptor whose `read` yields NUL-separated
names incrementally instead of building a full listing in ring 3. Each process
owns its descriptor table: `fork` gives the child a copy and `exec` leaves the table untouched, so
one program cannot close another program's descriptors. `dup` and `dup2` create
new descriptors for the same underlying stream and always start with a clear
close-on-exec flag. `fcntl(F_GETFD/F_SETFD, FD_CLOEXEC)` marks a descriptor so
the next successful `exec` closes it; the flag is copied by `fork` but cleared
on duplicated descriptors. `fcntl(F_GETFL/F_SETFL, O_NONBLOCK)` controls
nonblocking sockets and streams.

`signal` installs a handler for signals 1–64; disposition `0` restores the
default action and `1` explicitly ignores the signal. `kill(pid, signum, 0)`
queues a signal to any live process; signalling your own PID delivers it before
the next return to ring 3, and the 100 Hz timer also delivers pending signals.
Passing a non-null `Siginfo*` instead is the clean-room `sigqueue` operation: the
guest SDK exposes it as `sigqueue(pid, signum, value)`, which deliberately reuses
syscall 17 with `rdx` set to the payload pointer. The kernel copies and validates
the whole 40-byte structure before queueing it.

Delivery builds a synthetic call frame on the current user stack. A handler is
`extern "C" fn(u64, *mut u64, *const Siginfo) -> !`: it receives the signal
number, a pointer to the saved 160-byte register frame, and a pointer to the
`Siginfo` written immediately after that frame. `Siginfo` carries `si_signo`,
`si_code`, `si_pid`, `si_value` and `si_addr`; codes are `SI_USER` for plain
sends, `SI_QUEUE` for explicit values, `SI_KERNEL` for kernel-generated signals
and `SI_FAULT` for CPU faults. The current CPU-fault path leaves `si_addr` at
zero. The handler must finish with `sigreturn(frame)`, which restores only the
20 register-frame slots. Only one handler runs per process at a time.

Standard signals coalesce in a 64-bit pending mask and report the sender through
an `SI_USER` payload. Real-time signals in `SIGRTMIN..=SIGRTMAX` (`34..=64`) instead
keep one queued `Siginfo` per send and are delivered in strict send order, even
when the same number is sent repeatedly. Blocked real-time signals stay queued
and are skipped until unblocked; a bounded queue of 64 entries rejects further
queued sends with `ERROR`. `SIGINT`, `SIGILL`, `SIGBUS`, `SIGFPE`, `SIGSEGV` and
`SIGTERM` terminate by default; `SIGKILL` is always fatal and cannot be caught,
ignored or blocked. Other unhandled signals are ignored. CPU faults taken in
ring 3 are delivered through the same path with an `SI_FAULT` payload; a
synchronous fault left ignored is still terminated to avoid re-executing the
faulting instruction forever.

`sigprocmask` blocks, unblocks or replaces a per-process bit mask, and
`sigpending` reports signals that arrived while blocked. A blocked signal stays
pending until it is unblocked, then is delivered on the next return to ring 3.
A handler may also queue another signal: because delivery resumes at
`sigreturn`, the newly pending handler runs immediately after the interrupted
context is restored. `fork` copies dispositions and the mask but not pending
signals, and `exec` preserves them.

`sigtimedwait` consumes a blocked signal from a caller-supplied 64-bit set,
writes its `Siginfo` into a user buffer and returns the signal number. The guest
SDK exposes indefinite `sigwaitinfo` and payload-discarding `sigwait` wrappers;
`Some(0)` polls once and `None` waits forever. The kernel requires every waited
signal to be blocked so a handler cannot race the waiter, and `SIGKILL` can never
be waited for. A blocked signal whose disposition is ignore is still queued for
a waiter. Internally the kernel returns a private retry value when it suspends
the caller; the SDK wrapper yields and retries so the blocking detail stays
inside `gtos-sys`. The timeout uses the same 100 Hz clock as `sleep`, and a
finite wait returns `ERROR` when it expires. The Rust guest has a child send a
queued real-time payload while the parent waits, then checks the payload and a
separate timeout.

`signalfd` creates a descriptor that turns the same pending set into readable
40-byte `Siginfo` records. Every signal in its set must already be blocked, and
`SIGKILL` is excluded as usual. A blocking read consumes the next matching
signal in the same order as `sigtimedwait` and waits on the signal wake path if
none is pending; `SFD_NONBLOCK` makes an empty read return `ERROR` instead, and
`SFD_CLOEXEC` closes the descriptor on `exec`. As with `sigtimedwait`, the kernel
returns a private value when it suspends the caller, and the guest `read` wrapper
yields and retries. The handle stores only the mask and flags, so a child that
inherits the descriptor reads its own signals after `fork`. `fcntl` can change
`O_NONBLOCK` on the descriptor. The Rust guest has a child send a queued payload
while the parent performs a blocking `read`, then verifies the payload, pending
cleanup, and a nonblocking empty read.

Ordinary pipes and sockets now honour blocking `read` as well. An empty blocking
descriptor arms the readiness generation together with the sleep state, and a
peer write or half-close wakes the reader directly through the same path used by
`poll` and `select`. `O_NONBLOCK` preserves the `ERROR` response for an empty
queue, EOF remains `0`, and a successful read advances the generation so writers
waiting for capacity can retry. The Rust guest tests both a byte pipe and a
sequenced socket with a delayed child producer.

Ordinary writes now use the same symmetry: a full blocking pipe or datagram
socket arms the readiness generation and sleeps until a reader frees room, then
the SDK wrapper hides the retry value. `O_NONBLOCK` still returns `ERROR` and a
partially accepted write still returns its byte count. The Rust guest fills both
queue types before forking a delayed reader that drains one item, so a
timeout-only or short-return implementation cannot pass.

`poll` waits on several descriptors at once. Each `PollFd` is a stable 16-byte
record (`u64 fd`, `u16 events`, `u16 revents`); the kernel fills `revents` in
place and returns the number of ready entries. Requestable events are `POLLIN`
and `POLLOUT`; `POLLERR`, `POLLHUP` and `POLLNVAL` are reported regardless of the
request. Pipes and sockets report buffered bytes and free capacity, signal
descriptors report pending signals, and regular files and console output are
always ready. A blocking call arms a readiness registration under the process
table lock and wakes directly when a peer change advances the readiness
generation; the PIT only enforces the deadline. The kernel returns the same
private retry value used by `sigtimedwait` when it suspends the caller, and the
guest `poll` wrapper recomputes the remaining timeout before retrying. The Rust
guest waits on a pipe, a socket and a signal descriptor together, validates one
payload from each, then checks write readiness, a finite timeout and `POLLNVAL`.
A second guest case blocks `poll` with an infinite timeout until a child writes,
so a timeout-only implementation cannot pass by waking on a nearby tick.

`select` offers the same readiness model with classic fd bitmasks. Because the
syscall ABI has three argument registers, the caller passes a pointer to a
40-byte block holding `nfds` and the read, write, exception and timeout fields.
Each set is sixteen `u64` words, so descriptors `0..1024` are selectable; a null
pointer means no set. Read and write sets report `POLLIN` and `POLLOUT`, while the
exception set reports `POLLERR`, `POLLHUP` and `POLLNVAL`. The kernel rewrites
each set with the ready subset only on the final return, leaving the original
request intact across retries. The Rust guest waits on the same pipe, socket and
signal descriptor used by `poll`, then checks write readiness, a finite timeout
and an exception report for a closed descriptor.

`gtos_heap!` installs a small lock-free bump allocator that grows through `brk`.
It reserves memory with one atomic compare-and-exchange, so a signal handler can
allocate without locking against interrupted code. Freed blocks are not
recycled yet: `dealloc` is a no-op, which keeps ring 3 simple while the guest
runtime is still pre-alpha. The Rust guest builds a `Vec`, formats it with
`format!`, and checks that `brk(0)` moved before reporting `guest allocator ok`.

`fbinfo` returns a stable geometry struct: width, height, pixel stride,
bytes per pixel, a numeric pixel format, and the exact byte length of one frame.
The kernel does not hand ring 3 its own framebuffer pointer. Instead a guest
allocates a shadow buffer with the normal heap, paints it, and calls
`fbcommit`; the kernel copies only the reported frame length after validating
the whole source range. The Rust guest paints an 8x8 checkerboard in the
bottom-right corner, commits it, and reports `framebuffer device ok`.

`wincreate`, `wincommit`, `wincommitrect`, `winpresent`, `winsetlayer`,
`winsettitle`, `winpost`, `winread`, `winbindshm` and `windestroy` layer a small
compositor on that pixel backend.
`kernel/src/compositor.rs` keeps one shadow frame plus a backing image for each
of sixteen clipped windows. A guest normally supplies tightly packed pixels in
its own heap; the kernel copies them into the window's private backing image and
does not touch the display until `winpresent`. Each `WindowCreate` carries a
`layer`: larger values paint on top and creation order breaks ties.
`wincommitrect` updates one subrectangle from a strided source buffer, so a
toolkit can redraw a small tile without resending a whole frame. `winsetlayer`
changes the order and triggers a repaint, while `winsettitle` updates the text in
a kernel-drawn border and title bar that sits above every window's content.
Windows belong to the process that created them and are cleared automatically
when it exits, so a guest cannot paint another process's window or leave stale
pixels behind. Every window also has an eight-message, 64-byte-per-message
mailbox: any process may `winpost` to a live window, but only the owner can
`winread` it. The Rust guest forks a child that posts `ping` to a parent-owned
window, verifies the sender PID and payload, then reports `window ipc ok`. It
also sets both window titles and reports `window decorations ok`.

`winbindshm` replaces a window's backing image with retained shared-memory
frames. The guest fills its own `shmattach` mapping once, binds the region, and
can then let `winpresent` repaint from the same frames without another pixel
copy. The compositor holds its own frame references, so the guest may detach and
destroy the region after binding and the window keeps its pixels until it is
destroyed or replaced. The Rust guest verifies this path with `window shm ok`.

`input` exposes a nonblocking mirror of decoded keyboard, mouse and serial
input. The shell keeps its own byte queue, so a guest polling events cannot steal
command history; the mirror is cleared when `run` launches a program. Because the
shell's async executor stops while a guest runs, `sys_input` polls the executor
itself before checking the queue, which makes scancodes injected during a guest
visible immediately.

Each event is a 24-byte struct: a kind (`INPUT_KEY_DOWN`, `INPUT_KEY_UP`,
`INPUT_MOUSE_MOVE`, `INPUT_MOUSE_BUTTON` or `INPUT_SERIAL_BYTE`), a code
(translated key byte, `KEY_*` value or mouse button index), a modifier bitmask
(`MOD_SHIFT`, `MOD_CTRL`, `MOD_ALT`, `MOD_CAPS_LOCK`), absolute pointer `x`/`y`,
and a button mask. Key releases reuse the translation captured on key down, so a
guest sees the release of `A` even if Shift was released first. The PS/2 mouse
driver initialises IRQ12 in streaming packet mode and emits absolute move and
button events; the shell ignores those, so pointer input belongs entirely to
ring 3. The Rust guest prints `waiting for input event`, waits up to ten seconds
for a shifted `A` press and release, reports both, then prints `input device ok`.

`shmcreate`, `shmattach`, `shmdetach` and `shmdestroy` expose bounded
shared-memory regions from `kernel/src/shm.rs`. A region owns a small set of
physical frames; attaching maps those same frames into a process's reserved user
slot and adds a reference, so pages are freed only after the owner destroys the
region and every attached process detaches or exits. Shared mappings are kept
outside the ordinary page map, so `fork` does not turn them into copy-on-write
pages. The Rust guest creates a 4 KiB region, attaches it, forks a child that
attaches the same region and writes a marker, then reads the marker through its
own mapping and reports `shared memory ok`. The same regions now back windows
through `winbindshm`.

`pipe(fds)` creates a bounded 4 KiB byte stream and writes a read/write
descriptor pair into a user array. Both survive `fork` and `exec`, and closing
the last write end reports EOF to readers. An empty blocking pipe parks the
caller until a writer sends data or closes; `O_NONBLOCK` instead returns
`ERROR`. `pipe2(fds, flags)` applies `O_NONBLOCK` and/or `O_CLOEXEC` to both
ends before publishing the pair. The Rust guest forks a child that writes a
phrase in three chunks, then reads it in the parent and reports
`streaming ipc ok`; a separate check reports `pipe2 flags ok` after verifying
both creation flags on each end.

`socketpair(AF_UNIX, type, fds)` creates two connected endpoints and writes
their descriptors into a two-`u64` user array. `SOCK_SEQPACKET` preserves
message boundaries: each `write` sends one whole message and each `read`
receives one message, with 4 KiB per direction in 1 KiB datagrams. `SOCK_STREAM`
is a byte stream: writes may be coalesced and reads may return partial data.
`SOCK_NONBLOCK` may be ORed into `type` to create both endpoints nonblocking,
and `SOCK_CLOEXEC` marks both endpoints close-on-exec; the two flags combine.
`fcntl(F_SETFL/F_GETFL, O_NONBLOCK)` can also change status flags later, while
`fcntl(F_SETFD/F_GETFD, FD_CLOEXEC)` adjusts descriptor flags.
`shutdown(fd, how)` supports read/write half-close on sockets and streams.
Descriptors are inherited across `fork` and, by default, `exec`; the peer sees
EOF only when the last copy of the sending end closes or is shut down for
writing. The Rust guest verifies seqpacket and stream behavior, both type flags,
inheritance/EOF, and reports `socketpair ok` and `sock cloexec ok`.
`ioctl(fd, request, arg)` adds descriptor queries without moving payload bytes.
`FIONREAD` reports readable bytes on pipes, scheme streams, SHM rings and file
snapshots; `FIONWRITE` reports free bytes in the outgoing pipe or SHM ring. File
descriptors also support `FILE_SIZE`, `FILE_TRUNCATE` and `FILE_SYNC`, which
resize or flush the FAT16 backing through the same validated path used by
`write`. Socket and scheme-connection descriptors answer
`IOCTL_SOCKET_PEER_PID` with the process identifier on the other end; plain
pipes have no peer process and reject that request. The Rust guest checks pipe
occupancy, socket credentials across `fork`, truncates and resyncs a test file,
and reports `ioctl ok` and `socket cred ok`.

`schemeregister(name, length, notify_fd)` lets a process serve a URL scheme from
ring 3. `notify_fd` is the write end of a pipe; the kernel keeps its own clone
and sends `CONNECT <fd> <caller_pid> <url>\n` whenever another process calls
`open` on that scheme. The notification names a new duplex descriptor already
installed in the server's table: what the client writes, the server reads, and
vice versa. `schemeunregister` releases the registration, and a server that
exits is unregistered automatically. The Rust guest registers `demo`, forks a
client that opens `/srv/demo/hello`, exchanges `PING`/`PONG` over the connection,
and reports `scheme server ok`. Built-in `file`, `net` and `sys` schemes still
work through their original snapshot path.

`shmbind(fd, region)` upgrades a duplex scheme connection to shared-memory
transport. The kernel retains its own view of the region's frames and uses them
as a bounded byte ring for that descriptor; the pipe underneath still supplies
end-of-file and broken-connection state. The owner may destroy the region
immediately after a successful bind because the stream keeps its own frame
references. The Rust guest registers `shm`, binds an 8 KiB region (twice the
pipe capacity), sends a patterned payload through `/srv/shm/data`, and reports
`scheme shm ok`.

The VFS syscalls add Unix-style paths on top of that namespace. `/` maps to the
FAT16 data disk, `/sys` and `/net` map to the corresponding built-in schemes,
and every registered scheme gets a `/srv/<name>` mount. `open` accepts either a
URL or a path, resolves `.`/`..` without escaping the root, and uses the calling
process's current directory for relative paths. `stat` exposes an inode-like
identifier, node kind, size and write permission; `readdir` writes NUL-separated
child names into a 1 KiB buffer. `chdir` and `getcwd` maintain a per-process
working directory that is inherited by `fork` and preserved by `exec`. The Rust
guest checks `/` and `/sys/version` metadata, lists the root, changes into
`/sys`, reads `version` relatively, verifies a failed `chdir`, and reports
`vfs ok`.

`run fork` demonstrates cooperative switching: the parent forks, yields while
polling `waitpid`, and the child yields before exiting. Ordinary guest callers
can also use plain blocking `waitpid`: the kernel checks the child's state and
arms the caller's readiness registration together, then parks until the child's
exit wakes it and hides the retry value in the SDK wrapper. `try_waitpid` passes
`WNOHANG` for the session manager's service polls. `fork` shares the user pages
copy-on-write; a ring-3 write fault gives the writer a private frame.
`run preempt` demonstrates PIT preemption: two processes print `P` and `C`
without yielding, and the interleaved output shows the 100 Hz timer switching
between them. `run sleep` demonstrates timer-driven blocking: the child sleeps
200 ms, the parent sleeps 100 ms, and the parent's message still arrives first.
Context switches happen through `kernel/src/context.rs`, from both the `int 0x80`
and timer handlers.
`run exec` embeds the ordinary demo program in a user buffer and replaces itself
with it; a successful `exec` never returns to the old image.

Because `exit_user` returns to the kernel with `ret` rather than `iretq`, it
restores the kernel's saved `RFLAGS` by hand — without that, the `int 0x80`
interrupt gate would leave interrupts disabled and the shell's `hlt` would never
wake again.
