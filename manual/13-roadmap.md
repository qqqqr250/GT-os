# Roadmap

- A frame pool that tracks bootloader-owned regions more precisely than `Usable`.
- Richer signals with default actions, masks, pending sets, fatal dispositions,
  delivery after `sigreturn`, hardware-fault delivery for ring-3 CPU exceptions,
  64-bit signal masks with RT signal-number support, and cross-process
  pipe-synchronised tests are now in place. FIFO-queued RT payloads (`siginfo`)
  remain future work.
- A richer guest SDK (`std`-compatible facade, allocator reuse, Rust `main` support).
- Extend the clean-room widget toolkit (`gtos-widgets`) beyond its themes,
  nested declarative views, multi-line editing and accessibility hooks: add a
  full style engine, a screen-reader backend and reusable view composition.
- The clean-room desktop shell now has session requests, real fork/exec launch
  handlers for VFS metadata entries, applet interaction and persistent launcher
  focus memory. A dedicated ring-3 session manager reads VFS autostart metadata,
  publishes session state, dispatches shell actions and serves a registered
  `session:` bus with runner, KCM and managed-service methods backed by VFS
  registries. It tracks named child services, enforces bounded restart policies
  and publishes their state in `SERVICES.ST`; the bus is verified by real
  cross-process calls. Plasma-compatible applets, KCMs, runners and a full
  service manager are still future work; none of this imports Plasma or Qt code.
- A full C/C++ runtime for guest programs: `crt0` with `errno`, `malloc`, string
  routines and `atexit` is already available; a dynamic linker, C++ ABI and
  `std`-compatible facade are still needed.
- Move the remaining in-kernel `file`/`sys` resources behind VFS-backed registered schemes.
- Extend shared-memory scheme transport with zero-copy guest mapping and explicit
  flow control (the kernel-retained byte ring is now available through `shmbind`).
- Add local-domain stream sockets and socketpair options beyond the initial
  `AF_UNIX`/`SOCK_SEQPACKET` datagram pair.
- AHCI and USB mass storage alongside ATA PIO.
- Verified TLS (a root store and clock; today's HTTPS is encrypted but unverified).
- IPv6 for the network stack, plus retry/congestion handling in the TCP client.
- More exception context (error codes, stack traces against a symbol map).

The process foundation is now in place: per-process PML4 tables, PCBs, private
ring-0 stacks and fd tables, a preemptive round-robin scheduler,
reference-counted frame reclamation, `fork`/`exec`/`waitpid`, syscalls
`brk`/`getpid`/`yield`, a static ELF64 program on the data disk, a
shadow-framebuffer device with `fbinfo`/`fbcommit`, a damage-aware layer-ordered
window compositor
(`wincreate`/`wincommit`/`wincommitrect`/`winpresent`/`winsetlayer`/`winsettitle`/`winpost`/`winread`/`winbindshm`/`windestroy`)
and a nonblocking input event
mirror (`input`) with key press/release, modifiers, PS/2 mouse and serial events,
plus reference-counted shared memory (`shmcreate`/`shmattach`/`shmdetach`/`shmdestroy`)
that can back compositor windows directly, bounded `pipe()` streams for
incremental cross-process I/O, `schemeregister`/`schemeunregister` for
pipe-backed user-space scheme servers, `shmbind` for shared-memory scheme
streams, descriptor `ioctl` queries, a Rust-only C ABI shim, incremental FAT16
file descriptors with a bounded LRU cluster cache, dirty write-back and
descriptor-based directory streaming, and a VFS with mount points, path
resolution, `stat`/`readdir` and per-process working directories.
See [the Unix-like kernel rewrite](16-unix-rewrite.md) for the phases that lead
toward KDE enablement.
