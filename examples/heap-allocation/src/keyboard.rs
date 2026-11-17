//! PS/2 keyboard handling (scancode set 1, the firmware default).
//!
//! [`decode`] is the pure state machine: it tracks Shift, Caps Lock and the
//! `0xE0` extended prefix and turns a raw scancode into a byte. [`handle_scancode`]
//! pushes the result into a small queue that the keyboard IRQ handler fills and
//! the main thread drains through [`pop`].

use core::sync::atomic::{AtomicBool, Ordering};

use spin::Mutex;
use x86_64::instructions::interrupts;

/// True while either shift key is held down.
static SHIFT: AtomicBool = AtomicBool::new(false);
/// True after the caps-lock key has been toggled an odd number of times.
static CAPS_LOCK: AtomicBool = AtomicBool::new(false);
/// Set when the previous scancode was the 0xE0 "extended" prefix.
static EXTENDED: AtomicBool = AtomicBool::new(false);

const EXTENDED_PREFIX: u8 = 0xe0;
const LEFT_SHIFT: u8 = 0x2a;
const RIGHT_SHIFT: u8 = 0x36;
const CAPS_LOCK_KEY: u8 = 0x3a;

/// Control codes for the non-ASCII keys this crate tracks. They sit above the
/// printable range so a line reader can ignore them.
pub const KEY_UP: u8 = 0x80;
pub const KEY_DOWN: u8 = 0x81;
pub const KEY_LEFT: u8 = 0x82;
pub const KEY_RIGHT: u8 = 0x83;

/// Unshifted scancode -> ASCII. Index 0 is "no character".
static NORMAL: [u8; 58] = [
    0, 0x1b, b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9', b'0', b'-', b'=', 0x08, b'\t',
    b'q', b'w', b'e', b'r', b't', b'y', b'u', b'i', b'o', b'p', b'[', b']', b'\n', 0, b'a', b's',
    b'd', b'f', b'g', b'h', b'j', b'k', b'l', b';', b'\'', b'`', 0, b'\\', b'z', b'x', b'c', b'v',
    b'b', b'n', b'm', b',', b'.', b'/', 0, b'*', 0, b' ',
];

/// Shifted scancode -> ASCII, same layout as [`NORMAL`].
static SHIFTED: [u8; 58] = [
    0, 0x1b, b'!', b'@', b'#', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'_', b'+', 0x08, b'\t',
    b'Q', b'W', b'E', b'R', b'T', b'Y', b'U', b'I', b'O', b'P', b'{', b'}', b'\n', 0, b'A', b'S',
    b'D', b'F', b'G', b'H', b'J', b'K', b'L', b':', b'"', b'~', 0, b'|', b'Z', b'X', b'C', b'V',
    b'B', b'N', b'M', b'<', b'>', b'?', 0, b'*', 0, b' ',
];

/// How many decoded bytes the queue holds.
const CAPACITY: usize = 32;

/// A tiny byte ring. The keyboard IRQ is the only producer and the main thread
/// the only consumer, so the lock is the whole synchronisation story.
struct Queue {
    bytes: [u8; CAPACITY],
    head: usize,
    tail: usize,
}

impl Queue {
    const fn new() -> Self {
        Self {
            bytes: [0; CAPACITY],
            head: 0,
            tail: 0,
        }
    }

    fn push(&mut self, byte: u8) {
        let next = (self.head + 1) % CAPACITY;
        if next == self.tail {
            return; // full: drop the byte rather than overwrite history
        }
        self.bytes[self.head] = byte;
        self.head = next;
    }

    fn pop(&mut self) -> Option<u8> {
        if self.head == self.tail {
            return None;
        }
        let byte = self.bytes[self.tail];
        self.tail = (self.tail + 1) % CAPACITY;
        Some(byte)
    }

    fn clear(&mut self) {
        self.head = 0;
        self.tail = 0;
    }
}

/// Decoded bytes waiting for the main thread.
static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());

/// Decodes one raw scancode into a byte, updating the modifier state.
///
/// Returns `None` for key releases and for keys this crate does not map.
#[must_use]
pub fn decode(scancode: u8) -> Option<u8> {
    if scancode == EXTENDED_PREFIX {
        EXTENDED.store(true, Ordering::Relaxed);
        return None;
    }
    if EXTENDED.swap(false, Ordering::Relaxed) {
        return decode_extended(scancode);
    }

    let released = scancode & 0x80 != 0;
    let code = scancode & 0x7f;

    match code {
        LEFT_SHIFT | RIGHT_SHIFT => {
            SHIFT.store(!released, Ordering::Relaxed);
            None
        }
        CAPS_LOCK_KEY => {
            if !released {
                CAPS_LOCK.fetch_xor(true, Ordering::Relaxed);
            }
            None
        }
        _ if released => None,
        _ => {
            let normal = *NORMAL.get(usize::from(code))?;
            if normal == 0 {
                return None;
            }
            let mut ascii = if SHIFT.load(Ordering::Relaxed) {
                *SHIFTED.get(usize::from(code))?
            } else {
                normal
            };
            if CAPS_LOCK.load(Ordering::Relaxed) && ascii.is_ascii_alphabetic() {
                ascii ^= 0x20; // flip the case bit
            }
            Some(ascii)
        }
    }
}

/// Decodes a scancode that followed the `0xE0` prefix.
fn decode_extended(scancode: u8) -> Option<u8> {
    if scancode & 0x80 != 0 {
        return None; // key release
    }
    match scancode {
        0x48 => Some(KEY_UP),
        0x50 => Some(KEY_DOWN),
        0x4b => Some(KEY_LEFT),
        0x4d => Some(KEY_RIGHT),
        _ => None,
    }
}

/// Feeds one raw scancode from port 0x60 into the input queue.
pub fn handle_scancode(scancode: u8) {
    if let Some(byte) = decode(scancode) {
        // The IRQ handler runs with interrupts already off; a caller on the main
        // thread must not race it, so the push disables them too.
        interrupts::without_interrupts(|| QUEUE.lock().push(byte));
    }
}

/// Pops the next decoded byte, or `None` if the queue is empty.
#[must_use]
pub fn pop() -> Option<u8> {
    interrupts::without_interrupts(|| QUEUE.lock().pop())
}

/// Clears the queue and the modifier state. Used by the tests.
pub fn reset() {
    interrupts::without_interrupts(|| QUEUE.lock().clear());
    SHIFT.store(false, Ordering::Relaxed);
    CAPS_LOCK.store(false, Ordering::Relaxed);
    EXTENDED.store(false, Ordering::Relaxed);
}
