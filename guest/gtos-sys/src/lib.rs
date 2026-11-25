//! Minimal `no_std` bindings for the `GTos` ring-3 syscall ABI.
//!
//! The kernel uses `int 0x80` with arguments in `rax`, `rdi`, `rsi` and `rdx`,
//! and reports errors as `u64::MAX`. This crate is intentionally tiny: it has
//! no dependency on `std`. A small `brk`-backed allocator is available through
//! [`gtos_heap!`] for guests that need `alloc`.

#![no_std]
// The syscall ABI is implemented with inline assembly; that is the crate's
// entire purpose.
#![allow(unsafe_code)]

use core::alloc::{GlobalAlloc, Layout};
use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// `exit(status)`.
const SYS_EXIT: u64 = 0;
/// `write(fd, buffer, length)`.
const SYS_WRITE: u64 = 1;
/// `open(url, length)`.
const SYS_OPEN: u64 = 2;
/// `read(fd, buffer, length)`.
const SYS_READ: u64 = 3;
/// `close(fd)`.
const SYS_CLOSE: u64 = 4;
/// `getpid()`.
const SYS_GETPID: u64 = 5;
/// `brk(address)`.
const SYS_BRK: u64 = 6;
/// `yield()`.
const SYS_YIELD: u64 = 7;
/// `fork()`.
const SYS_FORK: u64 = 8;
/// `exec(image, length, args)`, where `args` is a length-prefixed argument block.
const SYS_EXEC: u64 = 9;
/// `waitpid(pid, status, options)`.
const SYS_WAITPID: u64 = 10;
/// `sleep(milliseconds)`.
const SYS_SLEEP: u64 = 11;
/// `uptime()`.
const SYS_UPTIME: u64 = 12;
/// `clock_gettime()`.
const SYS_CLOCK_GETTIME: u64 = 13;
/// `realtime()`.
const SYS_REALTIME: u64 = 14;
/// `lseek(fd, offset, whence)`.
const SYS_LSEEK: u64 = 15;
/// `signal(signum, handler)`.
const SYS_SIGNAL: u64 = 16;
/// `kill(pid, signum, info)`.
const SYS_KILL: u64 = 17;
/// `sigreturn(frame)`.
const SYS_SIGRETURN: u64 = 18;
/// `fbinfo(info)`.
const SYS_FBINFO: u64 = 19;
/// `fbcommit(buffer, length)`.
const SYS_FBCOMMIT: u64 = 20;
/// `input(event)`.
const SYS_INPUT: u64 = 21;
/// `wincreate(description)`.
const SYS_WINCREATE: u64 = 22;
/// `wincommit(id, buffer, length)`.
const SYS_WINCOMMIT: u64 = 23;
/// `winpresent()`.
const SYS_WINPRESENT: u64 = 24;
/// `windestroy(id)`.
const SYS_WINDESTROY: u64 = 25;
/// `wincommitrect(id, buffer, damage)`.
const SYS_WINCOMMIT_RECT: u64 = 26;
/// `winsetlayer(id, layer)`.
const SYS_WINSETLAYER: u64 = 27;
/// `winpost(id, buffer, length)`.
const SYS_WINPOST: u64 = 28;
/// `winread(id, message, capacity)`.
const SYS_WINREAD: u64 = 29;
/// `winsettitle(id, buffer, length)`.
const SYS_WINSETTITLE: u64 = 30;
/// `shmcreate(length)`.
const SYS_SHMCREATE: u64 = 31;
/// `shmattach(id)`.
const SYS_SHMATTACH: u64 = 32;
/// `shmdetach(id)`.
const SYS_SHMDETACH: u64 = 33;
/// `shmdestroy(id)`.
const SYS_SHMDESTROY: u64 = 34;
/// `winbindshm(id, region)`.
const SYS_WINBINDSHM: u64 = 35;
/// `pipe(fds)`.
const SYS_PIPE: u64 = 36;
/// `schemeregister(name, length, notify_fd)`.
const SYS_SCHEMEREGISTER: u64 = 37;
/// `schemeunregister(name, length)`.
const SYS_SCHEMEUNREGISTER: u64 = 38;
/// `stat(path, length, info)`.
const SYS_STAT: u64 = 39;
/// `readdir(path, length, buffer)`.
const SYS_READDIR: u64 = 40;
/// `chdir(path, length)`.
const SYS_CHDIR: u64 = 41;
/// `getcwd(buffer, length)`.
const SYS_GETCWD: u64 = 42;
/// `shmbind(fd, region)`.
const SYS_SHMBIND: u64 = 43;
/// `ioctl(fd, request, arg)`.
const SYS_IOCTL: u64 = 44;
/// `sigprocmask(how, set, oldset)` with 64-bit masks.
const SYS_SIGPROCMASK: u64 = 45;
/// `sigpending(set)` with a 64-bit mask.
const SYS_SIGPENDING: u64 = 46;
/// `socketpair(domain, type, fds)`.
const SYS_SOCKETPAIR: u64 = 47;
/// `dup(fd)`.
const SYS_DUP: u64 = 48;
/// `dup2(oldfd, newfd)`.
const SYS_DUP2: u64 = 49;
/// `shutdown(fd, how)`.
const SYS_SHUTDOWN: u64 = 50;
/// `fcntl(fd, cmd, arg)`.
const SYS_FCNTL: u64 = 51;
/// `sigtimedwait(set, info, timeout_ms)`.
const SYS_SIGTIMEDWAIT: u64 = 52;
/// `signalfd(mask, flags)`.
const SYS_SIGNALFD: u64 = 53;
/// `poll(fds, length, timeout_ms)`.
const SYS_POLL: u64 = 54;
/// `select(args)`.
const SYS_SELECT: u64 = 55;
/// `pipe2(fds, flags)`.
const SYS_PIPE2: u64 = 56;

/// Unix-domain socket family.
pub const AF_UNIX: u64 = 1;
/// Connection-oriented byte stream socket.
pub const SOCK_STREAM: u64 = 1;
/// Connection-oriented datagram socket that preserves message boundaries.
pub const SOCK_SEQPACKET: u64 = 5;
/// Create the socket pair with nonblocking endpoints.
pub const SOCK_NONBLOCK: u64 = 0x0800;
/// Create the socket pair with close-on-exec endpoints.
pub const SOCK_CLOEXEC: u64 = 0x8_0000;

/// `shutdown`: disable further receives.
pub const SHUT_RD: u64 = 0;
/// `shutdown`: disable further sends.
pub const SHUT_WR: u64 = 1;
/// `shutdown`: disable further sends and receives.
pub const SHUT_RDWR: u64 = 2;

/// `waitpid`: return zero immediately instead of parking on a live child.
pub const WNOHANG: u64 = 1;

/// `fcntl`: get file descriptor flags.
pub const F_GETFD: u64 = 1;
/// `fcntl`: set file descriptor flags.
pub const F_SETFD: u64 = 2;
/// `fcntl`: get file status flags.
pub const F_GETFL: u64 = 3;
/// `fcntl`: set file status flags.
pub const F_SETFL: u64 = 4;
/// Close-on-exec file descriptor flag.
pub const FD_CLOEXEC: u64 = 1;
/// Nonblocking file status flag.
pub const O_NONBLOCK: u64 = 1;
/// Close-on-exec flag accepted by `pipe2`.
pub const O_CLOEXEC: u64 = 0x8_0000;
/// Create a signal descriptor whose empty reads fail instead of blocking.
pub const SFD_NONBLOCK: u64 = 0x0800;
/// Close a signal descriptor on `exec`.
pub const SFD_CLOEXEC: u64 = 0x0008_0000;

/// `poll`: the descriptor is readable.
pub const POLLIN: u16 = 0x0001;
/// `poll`: the descriptor is writable.
pub const POLLOUT: u16 = 0x0004;
/// `poll`: the descriptor is in an error state.
pub const POLLERR: u16 = 0x0008;
/// `poll`: the peer closed the connection.
pub const POLLHUP: u16 = 0x0010;
/// `poll`: the descriptor number is not open.
pub const POLLNVAL: u16 = 0x0020;

/// One readiness request for [`poll`].
///
/// The kernel writes `revents` in place; callers should initialise it to zero
/// with [`PollFd::new`]. The layout is deliberately identical in ring 0 and
/// ring 3.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PollFd {
    /// Descriptor number to query.
    pub fd: u64,
    /// Requested events, an OR of [`POLLIN`] and [`POLLOUT`].
    pub events: u16,
    /// Events reported by the kernel.
    pub revents: u16,
}

const _: () = assert!(core::mem::size_of::<PollFd>() == 16);

impl PollFd {
    /// Creates a request with no reported events yet.
    #[must_use]
    pub const fn new(fd: u64, events: u16) -> Self {
        Self {
            fd,
            events,
            revents: 0,
        }
    }
}

/// Number of `u64` words in one [`FdSet`].
const FD_SET_WORDS: usize = 16;
/// Number of descriptor bits in one [`FdSet`].
pub const FD_SETSIZE: usize = FD_SET_WORDS * 64;

/// One descriptor bitmask for [`select`].
///
/// The kernel rewrites the set with the ready subset on return, so callers that
/// retry with a shorter timeout should restore their original bits first.
/// Sixteen `u64` words let late descriptor numbers participate instead of
/// forcing guests to renumber their tables.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FdSet {
    words: [u64; FD_SET_WORDS],
}

const _: () = assert!(core::mem::size_of::<FdSet>() == 128);

impl FdSet {
    /// Creates an empty descriptor set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            words: [0; FD_SET_WORDS],
        }
    }

    /// Adds `fd`, returning `false` when it is outside [`FD_SETSIZE`].
    pub fn insert(&mut self, fd: u64) -> bool {
        let Some(word) = usize::try_from(fd / 64).ok().filter(|word| *word < FD_SET_WORDS) else {
            return false;
        };
        self.words[word] |= 1u64 << (fd % 64);
        true
    }

    /// Removes `fd`, returning `false` when it is outside [`FD_SETSIZE`].
    pub fn remove(&mut self, fd: u64) -> bool {
        let Some(word) = usize::try_from(fd / 64).ok().filter(|word| *word < FD_SET_WORDS) else {
            return false;
        };
        self.words[word] &= !(1u64 << (fd % 64));
        true
    }

    /// Reports whether `fd` is present.
    #[must_use]
    pub fn contains(&self, fd: u64) -> bool {
        let Ok(word) = usize::try_from(fd / 64) else {
            return false;
        };
        word < FD_SET_WORDS && self.words[word] & (1u64 << (fd % 64)) != 0
    }

    /// Removes every descriptor from the set.
    pub fn clear(&mut self) {
        self.words = [0; FD_SET_WORDS];
    }

    /// Reports whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|word| *word == 0)
    }

    /// Returns the raw words shared with the kernel ABI.
    #[must_use]
    pub const fn words(&self) -> [u64; FD_SET_WORDS] {
        self.words
    }
}

/// Private argument block shared with the kernel for [`select`].
#[repr(C)]
#[derive(Clone, Copy)]
struct SelectArgs {
    nfds: u64,
    readfds: u64,
    writefds: u64,
    exceptfds: u64,
    timeout_ms: u64,
}

const _: () = assert!(core::mem::size_of::<SelectArgs>() == 40);

/// Largest shared-memory region the kernel will create.
pub const SHM_MAX_BYTES: u64 = 0x0008_0000;

/// Terminal interrupt; terminates by default.
pub const SIGINT: u8 = 2;
/// Illegal instruction; terminates by default.
pub const SIGILL: u8 = 4;
/// Bus error / bad memory access; terminates by default.
pub const SIGBUS: u8 = 7;
/// Arithmetic exception; terminates by default.
pub const SIGFPE: u8 = 8;
/// Kill; cannot be caught, ignored or blocked.
pub const SIGKILL: u8 = 9;
/// User-defined signal 1; ignored by default.
pub const SIGUSR1: u8 = 10;
/// Segmentation fault / invalid memory access; terminates by default.
pub const SIGSEGV: u8 = 11;
/// User-defined signal 2; ignored by default.
pub const SIGUSR2: u8 = 12;
/// Termination request; terminates by default.
pub const SIGTERM: u8 = 15;
/// First real-time signal. These signals queue instead of coalescing.
pub const SIGRTMIN: u8 = 34;
/// Last real-time signal.
pub const SIGRTMAX: u8 = 64;

/// Reset a signal to its default disposition.
pub const SIG_DFL: u64 = 0;
/// Ignore a signal explicitly.
pub const SIG_IGN: u64 = 1;

/// `siginfo`: sent by `kill` from a user process.
pub const SI_USER: u64 = 1;
/// `siginfo`: sent with an explicit value through the `sigqueue` helper.
pub const SI_QUEUE: u64 = 2;
/// `siginfo`: generated by the kernel.
pub const SI_KERNEL: u64 = 3;
/// `siginfo`: generated by a CPU fault.
pub const SI_FAULT: u64 = 4;

/// Add the signals in a mask to the blocked set.
pub const SIG_BLOCK: u8 = 0;
/// Remove the signals in a mask from the blocked set.
pub const SIG_UNBLOCK: u8 = 1;
/// Replace the blocked set with a mask.
pub const SIG_SETMASK: u8 = 2;

/// Internal syscall result used by blocking wait paths.
///
/// The kernel returns this when it had to suspend the caller; [`sigtimedwait`],
/// [`read`] on a signal descriptor, [`poll`] and [`select`] yield and retry so
/// guests never observe it directly.
pub const SIGWAIT_RETRY: u64 = u64::MAX - 1;

/// Signal information passed to a handler as its third argument.
///
/// The `si_` names mirror the kernel's stable clean-room C ABI.
#[allow(clippy::struct_field_names)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Siginfo {
    /// The signal number, filled in by the kernel.
    pub si_signo: u64,
    /// One of the `SI_*` codes above.
    pub si_code: u64,
    /// The sending process, or 0 when the kernel sent it.
    pub si_pid: u64,
    /// The value supplied to `sigqueue`.
    pub si_value: u64,
    /// The fault address for synchronous CPU signals.
    pub si_addr: u64,
}

const _: () = assert!(core::mem::size_of::<Siginfo>() == 40);

/// The mask bit for one signal number.
#[must_use]
pub const fn signal_bit(signum: u8) -> u64 {
    1_u64 << (signum - 1)
}

/// A signal handler entered with `(signum, saved_frame, siginfo)`.
///
/// Handlers must call [`sigreturn`]; returning normally jumps to address 0.
pub type SignalHandler = extern "C" fn(u64, *mut u64, *const Siginfo) -> !;

/// Unknown or driver-specific pixel channel order.
pub const PIXEL_FORMAT_UNKNOWN: u32 = 0;
/// Red, green, blue byte order.
pub const PIXEL_FORMAT_RGB: u32 = 1;
/// Blue, green, red byte order.
pub const PIXEL_FORMAT_BGR: u32 = 2;

/// A regular file returned by [`stat`].
pub const VFS_FILE: u8 = 1;
/// A directory returned by [`stat`].
pub const VFS_DIR: u8 = 2;
/// Capacity of the fixed [`read_dir`] buffer.
pub const VFS_READDIR_BYTES: usize = 1024;

/// VFS metadata matching the kernel's stable `stat` ABI.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VfsStat {
    /// A stable identifier derived from the absolute path.
    pub inode: u64,
    /// Size in bytes, or zero for directories and streams.
    pub size: u64,
    /// Either [`VFS_FILE`] or [`VFS_DIR`].
    pub kind: u8,
    /// One when ring 3 may write through the node.
    pub writable: u8,
    /// Reserved padding.
    pub reserved: [u8; 6],
}

/// A key went down. [`InputEvent::code`] holds the translated key code.
pub const INPUT_KEY_DOWN: u32 = 1;
/// A key went up. [`InputEvent::code`] holds the code captured on key down.
pub const INPUT_KEY_UP: u32 = 2;
/// The pointer moved; `x`, `y` and the button mask are valid.
pub const INPUT_MOUSE_MOVE: u32 = 3;
/// A pointer button changed; `code` is 0 for left, 1 for right, 2 for middle.
pub const INPUT_MOUSE_BUTTON: u32 = 4;
/// A byte arrived on the serial port; [`InputEvent::code`] holds that byte.
pub const INPUT_SERIAL_BYTE: u32 = 5;

/// Either Shift key is held.
pub const MOD_SHIFT: u32 = 1 << 0;
/// Either Ctrl key is held.
pub const MOD_CTRL: u32 = 1 << 1;
/// Either Alt key is held.
pub const MOD_ALT: u32 = 1 << 2;
/// Caps Lock is active.
pub const MOD_CAPS_LOCK: u32 = 1 << 3;

/// Up arrow, used in [`InputEvent::code`].
pub const KEY_UP: u32 = 0x80;
/// Down arrow, used in [`InputEvent::code`].
pub const KEY_DOWN: u32 = 0x81;
/// Left arrow, used in [`InputEvent::code`].
pub const KEY_LEFT: u32 = 0x82;
/// Right arrow, used in [`InputEvent::code`].
pub const KEY_RIGHT: u32 = 0x83;
/// Page Up, used in [`InputEvent::code`].
pub const KEY_PAGE_UP: u32 = 0x84;
/// Page Down, used in [`InputEvent::code`].
pub const KEY_PAGE_DOWN: u32 = 0x85;
/// Home, used in [`InputEvent::code`].
pub const KEY_HOME: u32 = 0x86;
/// End, used in [`InputEvent::code`].
pub const KEY_END: u32 = 0x87;
/// Left Shift, used in [`InputEvent::code`].
pub const KEY_LEFT_SHIFT: u32 = 0x90;
/// Right Shift, used in [`InputEvent::code`].
pub const KEY_RIGHT_SHIFT: u32 = 0x91;
/// Left Ctrl, used in [`InputEvent::code`].
pub const KEY_LEFT_CTRL: u32 = 0x92;
/// Right Ctrl, used in [`InputEvent::code`].
pub const KEY_RIGHT_CTRL: u32 = 0x93;
/// Left Alt, used in [`InputEvent::code`].
pub const KEY_LEFT_ALT: u32 = 0x94;
/// Right Alt, used in [`InputEvent::code`].
pub const KEY_RIGHT_ALT: u32 = 0x95;
/// Caps Lock, used in [`InputEvent::code`].
pub const KEY_CAPS_LOCK: u32 = 0x96;

/// One decoded input event, matching the kernel's stable ABI.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputEvent {
    /// One of the `INPUT_*` event type constants above.
    pub kind: u32,
    /// Translated key code, mouse button index or serial byte.
    pub code: u32,
    /// Bitmask of the `MOD_*` constants above for keyboard events.
    pub modifiers: u32,
    /// Absolute X coordinate for pointer events, otherwise zero.
    pub x: i32,
    /// Absolute Y coordinate for pointer events, otherwise zero.
    pub y: i32,
    /// Current button mask for pointer events, otherwise zero.
    pub value: u32,
}

impl InputEvent {
    /// Whether this is a key press or release.
    #[must_use]
    pub fn is_key(self) -> bool {
        matches!(self.kind, INPUT_KEY_DOWN | INPUT_KEY_UP)
    }

    /// Whether this is a key press.
    #[must_use]
    pub fn is_press(self) -> bool {
        self.kind == INPUT_KEY_DOWN
    }

    /// Whether this is a key release.
    #[must_use]
    pub fn is_release(self) -> bool {
        self.kind == INPUT_KEY_UP
    }

    /// Whether this is a pointer move or button event.
    #[must_use]
    pub fn is_mouse(self) -> bool {
        matches!(self.kind, INPUT_MOUSE_MOVE | INPUT_MOUSE_BUTTON)
    }

    /// Returns the translated ASCII/control byte for a keyboard event.
    #[must_use]
    pub fn byte(self) -> Option<u8> {
        if self.is_key() {
            u8::try_from(self.code).ok()
        } else {
            None
        }
    }
}

/// Result of a nonblocking input poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputPoll {
    /// No event was queued when the syscall ran.
    Idle,
    /// One decoded event was removed from the kernel's mirror queue.
    Event(InputEvent),
}

/// Geometry passed to [`window_create`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowCreate {
    /// Left edge in framebuffer pixels.
    pub x: u32,
    /// Top edge in framebuffer pixels.
    pub y: u32,
    /// Visible width in framebuffer pixels.
    pub width: u32,
    /// Visible height in framebuffer pixels.
    pub height: u32,
    /// Stacking layer; larger values paint on top.
    pub layer: u32,
}

/// A subrectangle of a window and the stride of the source image.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowDamage {
    /// Left edge relative to the window.
    pub x: u32,
    /// Top edge relative to the window.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes between source rows; must cover `width * bytes_per_pixel`.
    pub stride: u32,
}

/// One message read from a window's kernel-mediated mailbox.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct WindowMessage {
    /// PID of the posting process.
    pub sender: u64,
    /// Number of valid bytes in `data`.
    pub length: u64,
    /// Fixed-size payload buffer.
    pub data: [u8; 64],
}

impl WindowMessage {
    /// Returns the valid portion of the payload.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        let length = usize::try_from(self.length).unwrap_or(self.data.len());
        &self.data[..length.min(self.data.len())]
    }
}

/// Geometry returned by [`framebuffer_info`].
///
/// `stride` is measured in pixels, and `byte_len` is the number of bytes the
/// kernel expects for one full commit.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FramebufferInfo {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub bytes_per_pixel: u32,
    pub pixel_format: u32,
    pub _reserved: u32,
    pub byte_len: u64,
}

/// Seek from the start of a resource.
pub const SEEK_SET: u8 = 0;
/// Seek relative to the current cursor.
pub const SEEK_CUR: u8 = 1;
/// Seek relative to the end of a resource.
pub const SEEK_END: u8 = 2;

/// `ioctl`: bytes available to read from a descriptor.
pub const IOCTL_FIONREAD: u64 = 1;
/// `ioctl`: free bytes in a descriptor's outgoing buffer.
pub const IOCTL_FIONWRITE: u64 = 2;
/// `ioctl`: current size of a file descriptor.
pub const IOCTL_FILE_SIZE: u64 = 3;
/// `ioctl`: resize a writable file descriptor to `arg` bytes.
pub const IOCTL_FILE_TRUNCATE: u64 = 4;
/// `ioctl`: flush a writable file descriptor to disk.
pub const IOCTL_FILE_SYNC: u64 = 5;
/// `ioctl`: peer process identifier of a socket/stream connection.
pub const IOCTL_SOCKET_PEER_PID: u64 = 6;

/// The value the kernel returns for a failed syscall.
pub const ERROR: u64 = u64::MAX;

/// Page size used when growing the guest heap with `brk`.
const PAGE_BYTES: u64 = 4096;
/// First byte after the most recently handed-out allocation.
static HEAP_NEXT: AtomicU64 = AtomicU64::new(0);
/// Highest break value the kernel has granted to this process.
static HEAP_LIMIT: AtomicU64 = AtomicU64::new(0);

/// A bump allocator backed by the `brk` syscall.
///
/// It has no locks: reservations are published with one compare-and-exchange,
/// so a signal handler can allocate while `main` is between syscalls. Freed
/// blocks are not recycled; `dealloc` is a no-op. That is deliberate for a
/// pre-alpha guest runtime, where a small monotonic heap is preferable to a
/// complex free list in ring 3.
pub struct GtosAllocator;

/// Installs [`GtosAllocator`] as the binary's global allocator.
///
/// Use this once in the guest crate root, alongside [`gtos_main!`]:
///
/// ```ignore
/// extern crate alloc;
/// gtos_sys::gtos_heap!();
/// gtos_sys::gtos_main!(main);
/// ```
#[macro_export]
macro_rules! gtos_heap {
    () => {
        #[global_allocator]
        static GTOS_ALLOCATOR: $crate::GtosAllocator = $crate::GtosAllocator;
    };
}

/// Aligns `value` up to `alignment`, which must be a power of two.
const fn align_up(value: u64, alignment: u64) -> Option<u64> {
    match value.checked_add(alignment - 1) {
        Some(sum) => Some(sum & !(alignment - 1)),
        None => None,
    }
}

/// Reads the current break once and caches it.
fn heap_limit() -> u64 {
    let limit = HEAP_LIMIT.load(Ordering::SeqCst);
    if limit != 0 {
        return limit;
    }
    let current = brk(0);
    if current == ERROR || current == 0 {
        return 0;
    }
    let _ = HEAP_LIMIT.compare_exchange(0, current, Ordering::SeqCst, Ordering::SeqCst);
    HEAP_LIMIT.load(Ordering::SeqCst)
}

/// Returns the bump cursor, starting it at the image's initial break.
fn heap_cursor(limit: u64) -> u64 {
    let next = HEAP_NEXT.load(Ordering::SeqCst);
    if next != 0 {
        return next;
    }
    let _ = HEAP_NEXT.compare_exchange(0, limit, Ordering::SeqCst, Ordering::SeqCst);
    HEAP_NEXT.load(Ordering::SeqCst)
}

unsafe impl GlobalAlloc for GtosAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size() as u64;
        if size == 0 {
            return core::ptr::null_mut();
        }
        let align = layout.align() as u64;
        loop {
            let limit = heap_limit();
            if limit == 0 {
                return core::ptr::null_mut();
            }
            let next = heap_cursor(limit);
            let Some(start) = align_up(next, align) else {
                return core::ptr::null_mut();
            };
            let Some(end) = start.checked_add(size) else {
                return core::ptr::null_mut();
            };
            if end <= limit {
                if HEAP_NEXT
                    .compare_exchange(next, end, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    return start as *mut u8;
                }
                // Another allocation moved the cursor; retry with its value.
                continue;
            }
            let Some(new_limit) = align_up(end, PAGE_BYTES) else {
                return core::ptr::null_mut();
            };
            let actual = brk(new_limit);
            if actual == ERROR || actual < new_limit {
                return core::ptr::null_mut();
            }
            let _ = HEAP_LIMIT.fetch_max(actual, Ordering::SeqCst);
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_layout = match Layout::from_size_align(new_size, layout.align()) {
            Ok(value) => value,
            Err(_) => return core::ptr::null_mut(),
        };
        let new_ptr = unsafe { self.alloc(new_layout) };
        if !new_ptr.is_null() {
            // SAFETY: both blocks are valid for `layout.size()` bytes, and the
            // allocator never reuses a block, so the regions cannot overlap.
            unsafe {
                core::ptr::copy_nonoverlapping(ptr, new_ptr, layout.size().min(new_size));
            }
        }
        new_ptr
    }
}

/// Invokes a syscall with up to three arguments.
#[inline]
fn syscall(number: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    // SAFETY: the kernel validates every user pointer before dereferencing it.
    // This function only moves register values and does not touch memory.
    unsafe {
        let result: u64;
        asm!(
            "int 0x80",
            inlateout("rax") number => result,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            options(nostack),
        );
        result
    }
}

/// Writes `bytes` to a file descriptor, returning the number of bytes written
/// or [`ERROR`].
///
/// A blocking descriptor with no free space can suspend the caller and resume
/// it with [`SIGWAIT_RETRY`]. This wrapper hides that detail by yielding and
/// retrying the original write.
#[inline]
pub fn write(fd: u64, bytes: &[u8]) -> u64 {
    loop {
        let result = syscall(SYS_WRITE, fd, bytes.as_ptr() as u64, bytes.len() as u64);
        if result != SIGWAIT_RETRY {
            return result;
        }
        yield_now();
    }
}

/// Reads up to `bytes.len()` bytes from a descriptor.
///
/// A blocking signal descriptor can suspend the caller and resume it with the
/// private [`SIGWAIT_RETRY`] value. This wrapper hides that detail by yielding
/// and retrying; ordinary descriptors never produce the sentinel.
#[inline]
pub fn read(fd: u64, bytes: &mut [u8]) -> u64 {
    loop {
        let result = syscall(SYS_READ, fd, bytes.as_mut_ptr() as u64, bytes.len() as u64);
        if result != SIGWAIT_RETRY {
            return result;
        }
        yield_now();
    }
}

/// Waits for readiness on several descriptors.
///
/// Returns the number of entries with a nonzero `revents`, `0` on timeout, or
/// [`ERROR`] for bad arguments. `None` means wait indefinitely. When the kernel
/// suspends the caller it returns [`SIGWAIT_RETRY`]; this wrapper hides that and
/// recomputes the remaining time before retrying.
#[inline]
pub fn poll(fds: &mut [PollFd], timeout_ms: Option<u64>) -> u64 {
    if fds.len() > 64 {
        return ERROR;
    }
    let timeout = timeout_ms.unwrap_or(u64::MAX);
    let started = uptime();
    loop {
        let elapsed = uptime().saturating_sub(started);
        let remaining = if timeout == u64::MAX {
            u64::MAX
        } else {
            timeout.saturating_sub(elapsed)
        };
        let result = syscall(
            SYS_POLL,
            fds.as_mut_ptr() as u64,
            fds.len() as u64,
            remaining,
        );
        if result != SIGWAIT_RETRY {
            return result;
        }
        yield_now();
    }
}

/// Waits for readiness using classic fd bitmasks.
///
/// Each supplied [`FdSet`] starts as the requested descriptors and becomes the
/// ready subset on return. Read sets report `POLLIN`, write sets report
/// `POLLOUT`, and exception sets report `POLLERR`, `POLLHUP` or `POLLNVAL`.
/// Returns the number of ready bits, `0` on timeout, or [`ERROR`] for bad
/// arguments. `None` means wait indefinitely. A wake is hidden behind
/// [`SIGWAIT_RETRY`] just like [`poll`], with the remaining timeout recomputed.
#[inline]
pub fn select(
    readfds: Option<&mut FdSet>,
    writefds: Option<&mut FdSet>,
    exceptfds: Option<&mut FdSet>,
    timeout_ms: Option<u64>,
) -> u64 {
    let read_pointer = match readfds {
        Some(set) => set as *mut FdSet as u64,
        None => 0,
    };
    let write_pointer = match writefds {
        Some(set) => set as *mut FdSet as u64,
        None => 0,
    };
    let except_pointer = match exceptfds {
        Some(set) => set as *mut FdSet as u64,
        None => 0,
    };
    let timeout = timeout_ms.unwrap_or(u64::MAX);
    let started = uptime();
    let mut args = SelectArgs {
        nfds: FD_SETSIZE as u64,
        readfds: read_pointer,
        writefds: write_pointer,
        exceptfds: except_pointer,
        timeout_ms: timeout,
    };
    loop {
        let elapsed = uptime().saturating_sub(started);
        args.timeout_ms = if timeout == u64::MAX {
            u64::MAX
        } else {
            timeout.saturating_sub(elapsed)
        };
        let result = syscall(SYS_SELECT, &raw mut args as u64, 0, 0);
        if result != SIGWAIT_RETRY {
            return result;
        }
        yield_now();
    }
}

/// Opens a scheme URL or a VFS path such as `/sys/version`.
#[inline]
pub fn open(url: &[u8]) -> u64 {
    syscall(SYS_OPEN, url.as_ptr() as u64, url.len() as u64, 0)
}

/// Returns VFS metadata for a path or URL.
#[inline]
pub fn stat(path: &[u8]) -> Option<VfsStat> {
    let mut info = VfsStat::default();
    let result = syscall(
        SYS_STAT,
        path.as_ptr() as u64,
        path.len() as u64,
        &raw mut info as u64,
    );
    (result == 0).then_some(info)
}

/// Writes NUL-separated directory names into `buffer`, returning the byte count.
#[inline]
pub fn read_dir(path: &[u8], buffer: &mut [u8; VFS_READDIR_BYTES]) -> Option<usize> {
    let result = syscall(
        SYS_READDIR,
        path.as_ptr() as u64,
        path.len() as u64,
        buffer.as_mut_ptr() as u64,
    );
    usize::try_from(result)
        .ok()
        .filter(|count| *count > 0 && *count <= buffer.len())
}

/// Changes the caller's current VFS directory.
#[inline]
pub fn chdir(path: &[u8]) -> bool {
    syscall(SYS_CHDIR, path.as_ptr() as u64, path.len() as u64, 0) == 0
}

/// Writes the caller's current VFS directory into `buffer`, NUL-terminated.
#[inline]
pub fn getcwd(buffer: &mut [u8]) -> Option<usize> {
    let result = syscall(
        SYS_GETCWD,
        buffer.as_mut_ptr() as u64,
        buffer.len() as u64,
        0,
    );
    usize::try_from(result)
        .ok()
        .filter(|count| *count > 0 && *count <= buffer.len())
}

/// Closes a descriptor.
#[inline]
pub fn close(fd: u64) -> u64 {
    syscall(SYS_CLOSE, fd, 0, 0)
}

/// Duplicates a descriptor.
#[inline]
pub fn dup(fd: u64) -> u64 {
    syscall(SYS_DUP, fd, 0, 0)
}

/// Duplicates a descriptor to a specific descriptor number.
#[inline]
pub fn dup2(oldfd: u64, newfd: u64) -> u64 {
    syscall(SYS_DUP2, oldfd, newfd, 0)
}

/// Shuts down read and/or write sides of a socket/stream.
#[inline]
pub fn shutdown(fd: u64, how: u64) -> u64 {
    syscall(SYS_SHUTDOWN, fd, how, 0)
}

/// Gets or sets file status flags.
#[inline]
pub fn fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    syscall(SYS_FCNTL, fd, cmd, arg)
}

/// Creates a bounded pipe, returning its `(read_fd, write_fd)` pair.
///
/// An empty blocking pipe parks the caller until a writer sends data or closes;
/// [`O_NONBLOCK`] instead reports [`ERROR`]. EOF is reported as zero once every
/// write end has closed. The descriptors survive `fork` and `exec`.
#[inline]
pub fn pipe() -> Option<(u64, u64)> {
    let mut fds = [u64::MAX; 2];
    if syscall(SYS_PIPE, fds.as_mut_ptr() as u64, 0, 0) != 0 {
        return None;
    }
    Some((fds[0], fds[1]))
}

/// Creates a pipe with [`O_NONBLOCK`] and/or [`O_CLOEXEC`] set on both ends.
#[inline]
pub fn pipe2(flags: u64) -> Option<(u64, u64)> {
    let mut fds = [u64::MAX; 2];
    if syscall(SYS_PIPE2, fds.as_mut_ptr() as u64, flags, 0) != 0 {
        return None;
    }
    Some((fds[0], fds[1]))
}

/// Creates a connected `AF_UNIX`/`SOCK_SEQPACKET` pair.
///
/// Each `write` sends one whole datagram and each `read` receives exactly one.
/// Empty sockets return [`ERROR`] until data arrives or the peer closes; once
/// the peer closes, reads return zero. Descriptors survive `fork` and, unless
/// [`SOCK_CLOEXEC`] is used, `exec`.
#[inline]
pub fn socketpair() -> Option<(u64, u64)> {
    socketpair_with(AF_UNIX, SOCK_SEQPACKET)
}

/// Creates a connected socket pair with explicit domain/type.
///
/// `SOCK_SEQPACKET` or `SOCK_STREAM` selects the transport, and
/// [`SOCK_NONBLOCK`]/[`SOCK_CLOEXEC`] may be OR-ed into `socket_type`.
#[inline]
pub fn socketpair_with(domain: u64, socket_type: u64) -> Option<(u64, u64)> {
    let mut fds = [u64::MAX; 2];
    if syscall(SYS_SOCKETPAIR, domain, socket_type, fds.as_mut_ptr() as u64) != 0 {
        return None;
    }
    Some((fds[0], fds[1]))
}

/// Registers `name` as a scheme served by this process.
///
/// `notify_fd` must be the write end of a pipe from [`pipe`]. The kernel clones
/// that end and writes `CONNECT <fd> <caller_pid> <url>` notifications for every
/// [`open`] of a URL using this scheme.
#[inline]
pub fn scheme_register(name: &[u8], notify_fd: u64) -> bool {
    syscall(
        SYS_SCHEMEREGISTER,
        name.as_ptr() as u64,
        name.len() as u64,
        notify_fd,
    ) == 0
}

/// Unregisters a scheme owned by this process.
#[inline]
pub fn scheme_unregister(name: &[u8]) -> bool {
    syscall(
        SYS_SCHEMEUNREGISTER,
        name.as_ptr() as u64,
        name.len() as u64,
        0,
    ) == 0
}

/// Returns the current process identifier.
#[inline]
pub fn getpid() -> u64 {
    syscall(SYS_GETPID, 0, 0, 0)
}

/// Grows the process heap or queries the current break.
#[inline]
pub fn brk(address: u64) -> u64 {
    syscall(SYS_BRK, address, 0, 0)
}

/// Yields the CPU to another runnable process.
#[inline]
pub fn yield_now() {
    let _ = syscall(SYS_YIELD, 0, 0, 0);
}

/// Forks the process; returns 0 in the child and the child pid in the parent.
#[inline]
pub fn fork() -> u64 {
    syscall(SYS_FORK, 0, 0, 0)
}

/// Replaces the current image with an ELF or flat binary and a packed command line.
///
/// `args` is a sequence of NUL-terminated strings followed by one extra NUL
/// byte. The kernel copies it before abandoning the old address space.
#[inline]
pub fn exec_with_args(image: &[u8], args: &[u8]) -> u64 {
    const HEADER_BYTES: usize = 8;
    const MAX_BLOCK_BYTES: usize = 4096;
    if args.len() > MAX_BLOCK_BYTES - HEADER_BYTES {
        return ERROR;
    }
    let mut block = [0u8; MAX_BLOCK_BYTES];
    block[..HEADER_BYTES].copy_from_slice(&(args.len() as u64).to_ne_bytes());
    block[HEADER_BYTES..HEADER_BYTES + args.len()].copy_from_slice(args);
    syscall(
        SYS_EXEC,
        image.as_ptr() as u64,
        image.len() as u64,
        block.as_ptr() as u64,
    )
}

/// Replaces the current image with an ELF or flat binary.
///
/// The new image receives a synthetic `argv[0]` of `"gtos"`.
#[inline]
pub fn exec(image: &[u8]) -> u64 {
    exec_with_args(image, b"gtos\0\0")
}

/// Waits for a child's exit status; `status` receives the status on success.
///
/// A live child parks the caller and resumes it with [`SIGWAIT_RETRY`]. This
/// wrapper hides that detail by retrying until the child exits or the target is
/// gone.
#[inline]
pub fn waitpid(pid: u64, status: &mut u64) -> u64 {
    loop {
        let result = syscall(SYS_WAITPID, pid, status as *mut u64 as u64, 0);
        if result != SIGWAIT_RETRY {
            return result;
        }
        yield_now();
    }
}

/// Checks a child's exit status without blocking; returns zero while it lives.
#[inline]
pub fn try_waitpid(pid: u64, status: &mut u64) -> u64 {
    syscall(
        SYS_WAITPID,
        pid,
        status as *mut u64 as u64,
        WNOHANG,
    )
}

/// Sleeps for at least `milliseconds`, letting peer processes run.
#[inline]
pub fn sleep(milliseconds: u64) -> u64 {
    syscall(SYS_SLEEP, milliseconds, 0, 0)
}

/// Returns milliseconds since the PIT was initialized.
#[inline]
pub fn uptime() -> u64 {
    syscall(SYS_UPTIME, 0, 0, 0)
}

/// Returns a monotonic nanosecond count from the PIT/TSC clock.
#[inline]
pub fn monotonic_ns() -> u64 {
    syscall(SYS_CLOCK_GETTIME, 0, 0, 0)
}

/// Returns Unix seconds from the battery-backed CMOS clock.
#[inline]
pub fn realtime_seconds() -> u64 {
    syscall(SYS_REALTIME, 0, 0, 0)
}

/// Moves a descriptor's cursor, returning its new absolute offset.
#[inline]
pub fn lseek(fd: u64, offset: i64, whence: u8) -> u64 {
    syscall(SYS_LSEEK, fd, offset as u64, u64::from(whence))
}

/// Performs a descriptor control request, returning `None` on failure.
#[inline]
pub fn ioctl(fd: u64, request: u64, arg: u64) -> Option<u64> {
    let result = syscall(SYS_IOCTL, fd, request, arg);
    (result != ERROR).then_some(result)
}

/// Installs a signal handler, returning the previous disposition.
#[inline]
pub fn signal(signum: u8, handler: SignalHandler) -> u64 {
    signal_disposition(signum, handler as *const () as u64)
}

/// Installs a raw disposition: [`SIG_DFL`], [`SIG_IGN`] or a handler address.
#[inline]
pub fn signal_disposition(signum: u8, disposition: u64) -> u64 {
    syscall(SYS_SIGNAL, u64::from(signum), disposition, 0)
}

/// Sends a signal to a process; use `getpid()` to signal yourself.
#[inline]
pub fn kill(pid: u64, signum: u8) -> u64 {
    syscall(SYS_KILL, pid, u64::from(signum), 0)
}

/// Sends a real-time signal with a 64-bit payload.
///
/// This is the `sigqueue` operation expressed through the existing `kill`
/// syscall: the third argument is a pointer to a kernel-validated `Siginfo`.
#[inline]
pub fn sigqueue(pid: u64, signum: u8, value: u64) -> u64 {
    let info = Siginfo {
        si_signo: u64::from(signum),
        si_code: SI_QUEUE,
        si_pid: getpid(),
        si_value: value,
        si_addr: 0,
    };
    syscall(
        SYS_KILL,
        pid,
        u64::from(signum),
        &raw const info as *const Siginfo as u64,
    )
}

/// Adjusts or queries the caller's 64-bit blocked signal mask.
///
/// A `None` set leaves the mask unchanged while still returning the old mask in
/// `oldset`, which makes it a convenient query operation.
#[inline]
pub fn sigprocmask(how: u8, set: Option<&u64>, oldset: Option<&mut u64>) -> u64 {
    let set_pointer = set.map_or(0, |value| value as *const u64 as u64);
    let old_pointer = oldset.map_or(0, |value| value as *mut u64 as u64);
    syscall(SYS_SIGPROCMASK, u64::from(how), set_pointer, old_pointer)
}

/// Writes the caller's 64-bit pending signal set into `set`.
#[inline]
pub fn sigpending(set: &mut u64) -> u64 {
    syscall(SYS_SIGPENDING, set as *mut u64 as u64, 0, 0)
}

/// Waits for a blocked signal from `set`, consuming it and returning its number.
///
/// `timeout_ms` is `None` for an indefinite wait. `Some(0)` polls once. The raw
/// kernel syscall may return [`SIGWAIT_RETRY`] after a wakeup; this wrapper
/// hides that detail by yielding and retrying until a result is available.
#[inline]
pub fn sigtimedwait(set: &u64, info: &mut Siginfo, timeout_ms: Option<u64>) -> u64 {
    let set_pointer = set as *const u64 as u64;
    let info_pointer = info as *mut Siginfo as u64;
    let timeout = timeout_ms.unwrap_or(u64::MAX);
    loop {
        let result = syscall(SYS_SIGTIMEDWAIT, set_pointer, info_pointer, timeout);
        if result != SIGWAIT_RETRY {
            return result;
        }
        yield_now();
    }
}

/// Indefinite form of [`sigtimedwait`].
#[inline]
pub fn sigwaitinfo(set: &u64, info: &mut Siginfo) -> u64 {
    sigtimedwait(set, info, None)
}

/// Waits for a blocked signal and discards its payload.
#[inline]
pub fn sigwait(set: &u64) -> u64 {
    let mut info = Siginfo::default();
    sigwaitinfo(set, &mut info)
}

/// Creates a descriptor that reads pending signals from `set`.
///
/// Every signal in `set` must already be blocked. `flags` may contain
/// [`SFD_NONBLOCK`] and/or [`SFD_CLOEXEC`]. Each successful [`read`] of the
/// descriptor returns one 40-byte `Siginfo`; an empty blocking descriptor waits
/// for a matching signal, while a nonblocking one returns [`ERROR`].
#[inline]
pub fn signalfd(set: &u64, flags: u64) -> Option<u64> {
    let fd = syscall(
        SYS_SIGNALFD,
        set as *const u64 as u64,
        flags,
        0,
    );
    (fd != ERROR).then_some(fd)
}

/// Reads one signal record from a descriptor created by [`signalfd`].
#[inline]
pub fn signalfd_read(fd: u64, info: &mut Siginfo) -> u64 {
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(
            (info as *mut Siginfo).cast::<u8>(),
            core::mem::size_of::<Siginfo>(),
        )
    };
    read(fd, bytes)
}

/// Restores the register frame passed to a signal handler.
///
/// This never returns when the kernel accepts the frame. A rejected frame exits
/// the process rather than running with a corrupted context.
#[inline]
pub fn sigreturn(frame: *mut u64) -> ! {
    let _ = syscall(SYS_SIGRETURN, frame as u64, 0, 0);
    exit(1)
}

/// Returns the framebuffer geometry, or `None` when no display is available.
#[inline]
pub fn framebuffer_info() -> Option<FramebufferInfo> {
    let mut info = FramebufferInfo::default();
    if syscall(
        SYS_FBINFO,
        &raw mut info as *mut FramebufferInfo as u64,
        0,
        0,
    ) == 0
    {
        Some(info)
    } else {
        None
    }
}

/// Copies a user-owned shadow buffer to the display, returning bytes committed.
///
/// The buffer must contain at least [`FramebufferInfo::byte_len`] bytes in the
/// same linear layout described by that struct.
#[inline]
pub fn framebuffer_commit(bytes: &[u8]) -> u64 {
    syscall(SYS_FBCOMMIT, bytes.as_ptr() as u64, bytes.len() as u64, 0)
}

/// Polls the kernel's ring-3 input mirror once.
///
/// The syscall decodes any pending scancodes before checking the queue, so a
/// guest does not need the shell to be running its async executor.
#[inline]
pub fn input_event() -> Result<InputPoll, u64> {
    let mut event = InputEvent::default();
    match syscall(SYS_INPUT, &raw mut event as *mut InputEvent as u64, 0, 0) {
        0 => Ok(InputPoll::Idle),
        1 => Ok(InputPoll::Event(event)),
        error => Err(error),
    }
}

/// Creates a kernel-composited window, returning its one-based identifier.
#[inline]
pub fn window_create(description: WindowCreate) -> Option<u64> {
    let id = syscall(
        SYS_WINCREATE,
        &raw const description as *const WindowCreate as u64,
        0,
        0,
    );
    (id != ERROR).then_some(id)
}

/// Copies a tightly packed window image into the compositor's shadow frame.
///
/// `bytes` must contain `width * height * bytes_per_pixel` bytes for the window.
#[inline]
pub fn window_commit(id: u64, bytes: &[u8]) -> Option<u64> {
    let count = syscall(SYS_WINCOMMIT, id, bytes.as_ptr() as u64, bytes.len() as u64);
    (count != ERROR).then_some(count)
}

/// Blits all committed windows to the display.
#[inline]
pub fn window_present() -> bool {
    syscall(SYS_WINPRESENT, 0, 0, 0) == 0
}

/// Destroys a window; its owner exits automatically release their windows.
#[inline]
pub fn window_destroy(id: u64) -> bool {
    syscall(SYS_WINDESTROY, id, 0, 0) == 0
}

/// Updates one subrectangle of a window from a larger strided image.
///
/// Returns the number of source bytes consumed. `damage.stride` is in bytes.
#[inline]
pub fn window_commit_rect(id: u64, bytes: &[u8], damage: WindowDamage) -> Option<u64> {
    let count = syscall(
        SYS_WINCOMMIT_RECT,
        id,
        bytes.as_ptr() as u64,
        &raw const damage as *const WindowDamage as u64,
    );
    (count != ERROR).then_some(count)
}

/// Changes a window's stacking layer; larger values paint on top.
#[inline]
pub fn window_set_layer(id: u64, layer: u32) -> bool {
    syscall(SYS_WINSETLAYER, id, u64::from(layer), 0) == 0
}

/// Sets the printable ASCII title shown in a window's kernel-drawn title bar.
#[inline]
pub fn window_set_title(id: u64, title: &[u8]) -> bool {
    syscall(
        SYS_WINSETTITLE,
        id,
        title.as_ptr() as u64,
        title.len() as u64,
    ) == 0
}

/// Reserves a page-sized shared-memory region owned by the caller.
///
/// Use [`shm_attach`] to map it into the current process.
#[inline]
pub fn shm_create(length: u64) -> Option<u64> {
    let id = syscall(SYS_SHMCREATE, length, 0, 0);
    (id != ERROR).then_some(id)
}

/// Maps a shared region into the caller and returns its base address.
#[inline]
pub fn shm_attach(id: u64) -> Option<u64> {
    let base = syscall(SYS_SHMATTACH, id, 0, 0);
    (base != ERROR).then_some(base)
}

/// Unmaps a shared region from the caller.
#[inline]
pub fn shm_detach(id: u64) -> bool {
    syscall(SYS_SHMDETACH, id, 0, 0) == 0
}

/// Retires a shared region; its frames survive until every mapping is gone.
#[inline]
pub fn shm_destroy(id: u64) -> bool {
    syscall(SYS_SHMDESTROY, id, 0, 0) == 0
}

/// Uses a shared region as a duplex scheme stream's byte transport.
///
/// The kernel retains its own view of the frames, so the region can be destroyed
/// after a successful bind. Reads and writes on `fd` then use the shared ring;
/// the underlying pipe still provides end-of-file and broken-connection state.
#[inline]
pub fn shm_bind(fd: u64, region: u64) -> bool {
    syscall(SYS_SHMBIND, fd, region, 0) == 0
}

/// Uses a shared region as a window's backing store.
///
/// The region must be large enough for the whole window. After binding, writes
/// through [`shm_attach`] become visible on the next
/// [`window_present`](fn.window_present.html) without another commit.
#[inline]
pub fn window_bind_shm(id: u64, region: u64) -> bool {
    syscall(SYS_WINBINDSHM, id, region, 0) == 0
}

/// Queues a message for any live window, including one owned by another process.
#[inline]
pub fn window_post(id: u64, bytes: &[u8]) -> bool {
    syscall(SYS_WINPOST, id, bytes.as_ptr() as u64, bytes.len() as u64) == 0
}

/// Reads one message from a window owned by the caller.
///
/// `Ok(None)` means the mailbox is empty; `Err` reports an invalid window or
/// buffer.
#[inline]
pub fn window_read(id: u64) -> Result<Option<WindowMessage>, u64> {
    let mut message = WindowMessage {
        sender: 0,
        length: 0,
        data: [0; 64],
    };
    let length = syscall(
        SYS_WINREAD,
        id,
        &raw mut message as *mut WindowMessage as u64,
        core::mem::size_of::<WindowMessage>() as u64,
    );
    match length {
        ERROR => Err(ERROR),
        0 => Ok(None),
        _ => Ok(Some(message)),
    }
}

/// Terminates the process; this function never returns.
#[inline]
pub fn exit(status: u64) -> ! {
    // SAFETY: `exit` never returns to the caller.
    unsafe {
        asm!(
            "int 0x80",
            in("rax") SYS_EXIT,
            in("rdi") status,
            options(noreturn, nostack),
        );
    }
}

/// Defines the guest entry point and panic handler for a `main` function.
///
/// ```ignore
/// #![no_std]
/// #![no_main]
/// gtos_sys::gtos_main!(main);
///
/// fn main() {
///     let _ = gtos_sys::write(1, b"hello\r\n");
/// }
/// ```
#[macro_export]
macro_rules! gtos_main {
    ($main:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn _start() -> ! {
            $main();
            $crate::exit(0)
        }

        #[panic_handler]
        fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
            $crate::exit(1)
        }
    };
}
