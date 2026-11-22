//! PS/2 keyboard handling (scancode set 1, the firmware default).
//!
//! The shell only wants translated bytes, so it consumes the byte queue filled
//! by `input::push`. Ring 3 also gets press/release events with modifier state
//! through `input::push_event`, which is what a window system needs.

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::input::{self, Event};

/// The two shift keys are tracked separately: releasing one must not clear the
/// other when both are held down.
static LEFT_SHIFT_HELD: AtomicBool = AtomicBool::new(false);
static RIGHT_SHIFT_HELD: AtomicBool = AtomicBool::new(false);
/// The two Ctrl keys, likewise tracked separately (the right one is extended).
static LEFT_CTRL: AtomicBool = AtomicBool::new(false);
static RIGHT_CTRL: AtomicBool = AtomicBool::new(false);
/// Alt keys are tracked separately too; neither is an extended left/right pair.
static LEFT_ALT: AtomicBool = AtomicBool::new(false);
static RIGHT_ALT: AtomicBool = AtomicBool::new(false);
/// True after the caps-lock key has been toggled an odd number of times.
static CAPS_LOCK: AtomicBool = AtomicBool::new(false);
/// Guards against auto-repeat toggling caps lock on every repeated make code.
static CAPS_HELD: AtomicBool = AtomicBool::new(false);
/// Set when the previous scancode was the 0xE0 "extended" prefix.
static EXTENDED: AtomicBool = AtomicBool::new(false);

/// Translation captured on each key press, so a release reports the same code
/// even if the user let go of Shift first.
static LAST_CODE: [AtomicU8; 128] = [const { AtomicU8::new(0) }; 128];
static LAST_EXTENDED_CODE: [AtomicU8; 128] = [const { AtomicU8::new(0) }; 128];

const LEFT_SHIFT: u8 = 0x2a;
const RIGHT_SHIFT: u8 = 0x36;
const CAPS_LOCK_KEY: u8 = 0x3a;
const LEFT_CTRL_KEY: u8 = 0x1d;
const LEFT_ALT_KEY: u8 = 0x38;
const RIGHT_CTRL_KEY: u8 = 0x1d;
const RIGHT_ALT_KEY: u8 = 0x38;

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

/// Current modifier bitmask for a decoded event.
fn modifier_bits() -> u32 {
    let mut bits = 0;
    if LEFT_SHIFT_HELD.load(Ordering::Relaxed) || RIGHT_SHIFT_HELD.load(Ordering::Relaxed) {
        bits |= input::MOD_SHIFT;
    }
    if LEFT_CTRL.load(Ordering::Relaxed) || RIGHT_CTRL.load(Ordering::Relaxed) {
        bits |= input::MOD_CTRL;
    }
    if LEFT_ALT.load(Ordering::Relaxed) || RIGHT_ALT.load(Ordering::Relaxed) {
        bits |= input::MOD_ALT;
    }
    if CAPS_LOCK.load(Ordering::Relaxed) {
        bits |= input::MOD_CAPS_LOCK;
    }
    bits
}

/// Sends a key event to the ring-3 mirror.
fn key_event(pressed: bool, code: u32) {
    input::push_event(Event {
        kind: if pressed {
            input::EVENT_KEY_DOWN
        } else {
            input::EVENT_KEY_UP
        },
        code,
        modifiers: modifier_bits(),
        ..Event::default()
    });
}

/// Applies Shift, Caps Lock and Ctrl to a scancode's unshifted translation.
fn translate(code: u8, normal: u8) -> u8 {
    let shift = LEFT_SHIFT_HELD.load(Ordering::Relaxed) || RIGHT_SHIFT_HELD.load(Ordering::Relaxed);
    let mut ascii = if shift {
        SHIFTED[code as usize]
    } else {
        normal
    };
    if CAPS_LOCK.load(Ordering::Relaxed) && ascii.is_ascii_alphabetic() {
        ascii ^= 0x20;
    }
    if (LEFT_CTRL.load(Ordering::Relaxed) || RIGHT_CTRL.load(Ordering::Relaxed))
        && (ascii.is_ascii_alphabetic() || matches!(ascii, b'[' | b'\\' | b']' | b'^' | b'_'))
    {
        ascii &= 0x1f;
    }
    ascii
}

/// Feeds one raw scancode from port 0x60 into the input queues.
pub fn handle_scancode(scancode: u8) {
    if scancode == 0xe0 {
        EXTENDED.store(true, Ordering::Relaxed);
        return;
    }
    if EXTENDED.swap(false, Ordering::Relaxed) {
        let pressed = scancode & 0x80 == 0;
        let code = scancode & 0x7f;
        match code {
            RIGHT_CTRL_KEY => {
                RIGHT_CTRL.store(pressed, Ordering::Relaxed);
                key_event(pressed, input::KEY_RIGHT_CTRL);
            }
            RIGHT_ALT_KEY => {
                RIGHT_ALT.store(pressed, Ordering::Relaxed);
                key_event(pressed, input::KEY_RIGHT_ALT);
            }
            0x48 | 0x50 | 0x4b | 0x4d | 0x49 | 0x51 | 0x47 | 0x4f => {
                let key = match code {
                    0x48 => input::KEY_UP,
                    0x50 => input::KEY_DOWN,
                    0x4b => input::KEY_LEFT,
                    0x4d => input::KEY_RIGHT,
                    0x49 => input::KEY_PAGE_UP,
                    0x51 => input::KEY_PAGE_DOWN,
                    0x47 => input::KEY_HOME,
                    0x4f => input::KEY_END,
                    _ => unreachable!(),
                };
                if pressed {
                    LAST_EXTENDED_CODE[code as usize].store(key, Ordering::Relaxed);
                    input::push(key);
                    key_event(true, u32::from(key));
                } else {
                    let translated = LAST_EXTENDED_CODE[code as usize].load(Ordering::Relaxed);
                    if translated != 0 {
                        key_event(false, u32::from(translated));
                    }
                }
            }
            _ => {}
        }
        return;
    }

    let pressed = scancode & 0x80 == 0;
    let code = scancode & 0x7f;

    match code {
        LEFT_SHIFT | RIGHT_SHIFT => {
            let held = pressed;
            if code == LEFT_SHIFT {
                LEFT_SHIFT_HELD.store(held, Ordering::Relaxed);
                key_event(pressed, input::KEY_LEFT_SHIFT);
            } else {
                RIGHT_SHIFT_HELD.store(held, Ordering::Relaxed);
                key_event(pressed, input::KEY_RIGHT_SHIFT);
            }
        }
        LEFT_CTRL_KEY => {
            LEFT_CTRL.store(pressed, Ordering::Relaxed);
            key_event(pressed, input::KEY_LEFT_CTRL);
        }
        LEFT_ALT_KEY => {
            LEFT_ALT.store(pressed, Ordering::Relaxed);
            key_event(pressed, input::KEY_LEFT_ALT);
        }
        CAPS_LOCK_KEY => {
            if pressed {
                if !CAPS_HELD.swap(true, Ordering::Relaxed) {
                    CAPS_LOCK.fetch_xor(true, Ordering::Relaxed);
                    key_event(true, input::KEY_CAPS_LOCK);
                }
            } else {
                CAPS_HELD.store(false, Ordering::Relaxed);
                key_event(false, input::KEY_CAPS_LOCK);
            }
        }
        _ => {
            let Some(&normal) = NORMAL.get(code as usize) else {
                return;
            };
            if normal == 0 {
                return;
            }
            if pressed {
                let ascii = translate(code, normal);
                LAST_CODE[code as usize].store(ascii, Ordering::Relaxed);
                input::push(ascii);
                key_event(true, u32::from(ascii));
            } else {
                let ascii = LAST_CODE[code as usize].load(Ordering::Relaxed);
                if ascii != 0 {
                    key_event(false, u32::from(ascii));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::Ordering;

    use super::handle_scancode;
    use crate::input;

    fn drain_input() {
        while input::pop().is_some() {}
        while input::pop_user_event().is_some() {}
        super::LEFT_SHIFT_HELD.store(false, Ordering::Relaxed);
        super::RIGHT_SHIFT_HELD.store(false, Ordering::Relaxed);
        super::LEFT_CTRL.store(false, Ordering::Relaxed);
        super::RIGHT_CTRL.store(false, Ordering::Relaxed);
        super::LEFT_ALT.store(false, Ordering::Relaxed);
        super::RIGHT_ALT.store(false, Ordering::Relaxed);
        super::CAPS_LOCK.store(false, Ordering::Relaxed);
        super::CAPS_HELD.store(false, Ordering::Relaxed);
    }

    #[test_case]
    fn releasing_one_shift_does_not_clear_the_other() {
        drain_input();
        handle_scancode(0x2a);
        handle_scancode(0x36);
        handle_scancode(0xaa);
        handle_scancode(0x1e); // 'a'
        assert_eq!(input::pop(), Some(b'A'));
    }

    #[test_case]
    fn caps_lock_only_toggles_on_the_first_make() {
        drain_input();
        handle_scancode(0x3a);
        handle_scancode(0x3a);
        handle_scancode(0x1e); // 'a'
        assert_eq!(input::pop(), Some(b'A'));
        handle_scancode(0xba);
        handle_scancode(0x1e); // 'a'
        assert_eq!(input::pop(), Some(b'A'));
    }

    #[test_case]
    fn ctrl_produces_a_control_code() {
        drain_input();
        handle_scancode(0x1d);
        handle_scancode(0x2e); // 'c'
        assert_eq!(input::pop(), Some(0x03));
        handle_scancode(0x9d);
        handle_scancode(0x2e); // 'c'
        assert_eq!(input::pop(), Some(b'c'));
    }

    #[test_case]
    fn function_and_gui_keys_are_ignored() {
        drain_input();
        handle_scancode(0x3b);
        handle_scancode(0x3c);
        handle_scancode(0xe0);
        handle_scancode(0x5b);
        assert_eq!(input::pop(), None);
    }
}
