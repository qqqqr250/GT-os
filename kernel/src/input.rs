//! A lock-free single-producer/single-consumer byte queue shared between the
//! input sources and the shell or desktop.
//!
//! The producers are the post-12 keyboard decoding task (which turns raw
//! scancodes into bytes) and the COM1 serial interrupt. Both run with interrupts
//! disabled — the serial handler because it is an interrupt gate, the keyboard
//! task because it wraps its decode in `without_interrupts` — so pushes never
//! race with each other; the atomics only order the producers against the
//! consumer running on the main thread.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

const CAPACITY: usize = 256;

static BUFFER: [AtomicU8; CAPACITY] = [const { AtomicU8::new(0) }; CAPACITY];
static HEAD: AtomicUsize = AtomicUsize::new(0);
static TAIL: AtomicUsize = AtomicUsize::new(0);

/// Control codes for the non-ASCII keys the desktop cares about (the arrow and
/// paging keys, plus the Windows key). They sit above the printable range, so
/// the shell's line reader ignores them and only the desktop reacts.
pub const KEY_UP: u8 = 0x80;
pub const KEY_DOWN: u8 = 0x81;
pub const KEY_LEFT: u8 = 0x82;
pub const KEY_RIGHT: u8 = 0x83;
pub const KEY_PAGE_UP: u8 = 0x84;
pub const KEY_PAGE_DOWN: u8 = 0x85;
pub const KEY_HOME: u8 = 0x86;
pub const KEY_END: u8 = 0x87;
pub const KEY_WINDOWS: u8 = 0x88;

/// Set by the COM1 interrupt the first time a serial byte arrives.
///
/// The desktop environment uses this to hand the console over to the shell:
/// the queue above merges keyboard and serial input, so a dedicated flag is
/// the only way to tell the two apart.
static SERIAL_SEEN: AtomicBool = AtomicBool::new(false);

/// Pushes a byte, dropping it if the queue is full.
pub fn push(byte: u8) {
    let head = HEAD.load(Ordering::Relaxed);
    let next = (head + 1) % CAPACITY;
    if next == TAIL.load(Ordering::Acquire) {
        return; // full: drop input rather than overwrite history
    }
    BUFFER[head].store(byte, Ordering::Relaxed);
    HEAD.store(next, Ordering::Release);
}

pub fn pop() -> Option<u8> {
    let tail = TAIL.load(Ordering::Relaxed);
    if tail == HEAD.load(Ordering::Acquire) {
        return None;
    }
    let byte = BUFFER[tail].load(Ordering::Relaxed);
    TAIL.store((tail + 1) % CAPACITY, Ordering::Release);
    Some(byte)
}

pub fn peek() -> Option<u8> {
    let tail = TAIL.load(Ordering::Relaxed);
    if tail == HEAD.load(Ordering::Acquire) {
        None
    } else {
        Some(BUFFER[tail].load(Ordering::Relaxed))
    }
}

/// Records that COM1 delivered a byte. Called from the serial interrupt.
pub fn note_serial() {
    SERIAL_SEEN.store(true, Ordering::Relaxed);
}

/// Whether COM1 has delivered any byte since boot.
pub fn serial_seen() -> bool {
    SERIAL_SEEN.load(Ordering::Relaxed)
}
