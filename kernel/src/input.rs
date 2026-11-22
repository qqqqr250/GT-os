//! Lock-free input queues shared between the hardware, the shell and ring 3.
//!
//! The shell consumes a byte queue: printable keys become their ASCII value, and
//! the few non-ASCII keys it understands use the `KEY_*` codes below. Ring 3
//! consumes a separate queue of rich [`Event`] values that also carries key
//! releases, modifier state and pointer motion. The two queues are independent,
//! so a guest polling events cannot steal shell history.
//!
//! Producers run with interrupts disabled — the keyboard decoder task wraps its
//! work in `without_interrupts`, and the keyboard, mouse and COM1 hardware all
//! arrive through interrupt gates — so each queue has a single producer at a
//! time. The atomics only order those producers against the consumer.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering};

const CAPACITY: usize = 256;
const EVENT_FIELDS: usize = 6;

static BUFFER: [AtomicU8; CAPACITY] = [const { AtomicU8::new(0) }; CAPACITY];
static HEAD: AtomicUsize = AtomicUsize::new(0);
static TAIL: AtomicUsize = AtomicUsize::new(0);

/// Ring-3 event mirror. Each slot stores the six fields of one [`Event`].
static USER_EVENTS: [AtomicU32; CAPACITY * EVENT_FIELDS] =
    [const { AtomicU32::new(0) }; CAPACITY * EVENT_FIELDS];
static USER_HEAD: AtomicUsize = AtomicUsize::new(0);
static USER_TAIL: AtomicUsize = AtomicUsize::new(0);

/// A key was pressed. `code` is the translated ASCII/control byte or a `KEY_*`
/// value; `modifiers` describes the state after the key went down.
pub const EVENT_KEY_DOWN: u32 = 1;
/// A key was released. `code` is the translation captured when it went down.
pub const EVENT_KEY_UP: u32 = 2;
/// The pointer moved; `x`/`y` are absolute framebuffer pixels and `value` is
/// the current button mask.
pub const EVENT_MOUSE_MOVE: u32 = 3;
/// A mouse button changed; `code` is the button index (0 left, 1 right, 2
/// middle), `value` is the new button mask and `x`/`y` are the pointer position.
pub const EVENT_MOUSE_BUTTON: u32 = 4;
/// A byte arrived on COM1; `code` is that byte.
pub const EVENT_SERIAL_BYTE: u32 = 5;

/// Either Shift key is down.
pub const MOD_SHIFT: u32 = 1 << 0;
/// Either Ctrl key is down.
pub const MOD_CTRL: u32 = 1 << 1;
/// Either Alt key is down.
pub const MOD_ALT: u32 = 1 << 2;
/// Caps Lock is active.
pub const MOD_CAPS_LOCK: u32 = 1 << 3;

/// Control codes for non-ASCII keys in the shell byte queue. They sit above the
/// printable range, so the shell's line reader only acts on the ones it
/// understands (Up/Down for command history) and ignores the rest.
pub const KEY_UP: u8 = 0x80;
pub const KEY_DOWN: u8 = 0x81;
pub const KEY_LEFT: u8 = 0x82;
pub const KEY_RIGHT: u8 = 0x83;
pub const KEY_PAGE_UP: u8 = 0x84;
pub const KEY_PAGE_DOWN: u8 = 0x85;
pub const KEY_HOME: u8 = 0x86;
pub const KEY_END: u8 = 0x87;

/// Codes for modifier keys themselves in [`Event::code`].
pub const KEY_LEFT_SHIFT: u32 = 0x90;
pub const KEY_RIGHT_SHIFT: u32 = 0x91;
pub const KEY_LEFT_CTRL: u32 = 0x92;
pub const KEY_RIGHT_CTRL: u32 = 0x93;
pub const KEY_LEFT_ALT: u32 = 0x94;
pub const KEY_RIGHT_ALT: u32 = 0x95;
pub const KEY_CAPS_LOCK: u32 = 0x96;

/// One decoded input event, shared with ring 3 through the stable syscall ABI.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Event {
    /// One of the `EVENT_*` values above.
    pub kind: u32,
    /// Translated key code, button index or serial byte.
    pub code: u32,
    /// Bitmask of the `MOD_*` values above.
    pub modifiers: u32,
    /// Absolute X coordinate for pointer events, otherwise zero.
    pub x: i32,
    /// Absolute Y coordinate for pointer events, otherwise zero.
    pub y: i32,
    /// Current button mask for pointer events, otherwise zero.
    pub value: u32,
}

/// Set by the COM1 interrupt the first time a serial byte arrives.
///
/// The byte queue above merges keyboard and serial input, so a dedicated flag
/// is the only way to tell the two apart.
static SERIAL_SEEN: AtomicBool = AtomicBool::new(false);

/// Pushes one byte into the shell's queue, dropping it if the queue is full.
pub fn push(byte: u8) {
    let head = HEAD.load(Ordering::Relaxed);
    let next = (head + 1) % CAPACITY;
    if next == TAIL.load(Ordering::Acquire) {
        return;
    }
    BUFFER[head].store(byte, Ordering::Relaxed);
    HEAD.store(next, Ordering::Release);
}

/// Pushes a rich event into the ring-3 mirror, dropping it if the queue is full.
pub fn push_event(event: Event) {
    let head = USER_HEAD.load(Ordering::Relaxed);
    let next = (head + 1) % CAPACITY;
    if next == USER_TAIL.load(Ordering::Acquire) {
        return;
    }
    let base = head * EVENT_FIELDS;
    USER_EVENTS[base].store(event.kind, Ordering::Relaxed);
    USER_EVENTS[base + 1].store(event.code, Ordering::Relaxed);
    USER_EVENTS[base + 2].store(event.modifiers, Ordering::Relaxed);
    USER_EVENTS[base + 3].store(event.x as u32, Ordering::Relaxed);
    USER_EVENTS[base + 4].store(event.y as u32, Ordering::Relaxed);
    USER_EVENTS[base + 5].store(event.value, Ordering::Relaxed);
    USER_HEAD.store(next, Ordering::Release);
}

/// Delivers a serial byte to both the shell and the ring-3 event mirror.
pub fn push_serial(byte: u8) {
    push(byte);
    push_event(Event {
        kind: EVENT_SERIAL_BYTE,
        code: u32::from(byte),
        ..Event::default()
    });
}

/// Removes the next event from the ring-3 mirror.
pub fn pop_user_event() -> Option<Event> {
    let tail = USER_TAIL.load(Ordering::Relaxed);
    if tail == USER_HEAD.load(Ordering::Acquire) {
        return None;
    }
    let base = tail * EVENT_FIELDS;
    let event = Event {
        kind: USER_EVENTS[base].load(Ordering::Relaxed),
        code: USER_EVENTS[base + 1].load(Ordering::Relaxed),
        modifiers: USER_EVENTS[base + 2].load(Ordering::Relaxed),
        x: USER_EVENTS[base + 3].load(Ordering::Relaxed) as i32,
        y: USER_EVENTS[base + 4].load(Ordering::Relaxed) as i32,
        value: USER_EVENTS[base + 5].load(Ordering::Relaxed),
    };
    USER_TAIL.store((tail + 1) % CAPACITY, Ordering::Release);
    Some(event)
}

/// Discards queued ring-3 events, used before each program launch so a guest
/// starts with a clean input stream.
pub fn clear_user() {
    USER_HEAD.store(0, Ordering::Relaxed);
    USER_TAIL.store(0, Ordering::Relaxed);
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
