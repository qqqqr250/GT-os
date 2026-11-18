//! PS/2 keyboard handling (scancode set 1, the firmware default).

use core::sync::atomic::{AtomicBool, Ordering};

use crate::input;

/// True while either shift key is held down.
static SHIFT: AtomicBool = AtomicBool::new(false);
/// True after the caps-lock key has been toggled an odd number of times.
static CAPS_LOCK: AtomicBool = AtomicBool::new(false);
/// Set when the previous scancode was the 0xE0 "extended" prefix.
static EXTENDED: AtomicBool = AtomicBool::new(false);

const LEFT_SHIFT: u8 = 0x2a;
const RIGHT_SHIFT: u8 = 0x36;
const CAPS_LOCK_KEY: u8 = 0x3a;

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

/// Feeds one raw scancode from port 0x60 into the input queue.
pub fn handle_scancode(scancode: u8) {
    if scancode == 0xe0 {
        EXTENDED.store(true, Ordering::Relaxed);
        return;
    }
    if EXTENDED.swap(false, Ordering::Relaxed) {
        // Extended keys. The arrow and paging keys are delivered to the desktop
        // as high control codes; everything else (keypad enter, right
        // ctrl/alt, ...) is still ignored.
        if scancode & 0x80 != 0 {
            return; // key release
        }
        let key = match scancode {
            0x48 => Some(input::KEY_UP),
            0x50 => Some(input::KEY_DOWN),
            0x4b => Some(input::KEY_LEFT),
            0x4d => Some(input::KEY_RIGHT),
            0x49 => Some(input::KEY_PAGE_UP),
            0x51 => Some(input::KEY_PAGE_DOWN),
            0x47 => Some(input::KEY_HOME),
            0x4f => Some(input::KEY_END),
            // Left/right Windows (GUI) keys. The desktop uses this to toggle
            // the Start menu; the shell ignores the control code.
            0x5b | 0x5c => Some(input::KEY_WINDOWS),
            _ => None,
        };
        if let Some(key) = key {
            input::push(key);
        }
        return;
    }

    let released = scancode & 0x80 != 0;
    let code = scancode & 0x7f;

    match code {
        LEFT_SHIFT | RIGHT_SHIFT => SHIFT.store(!released, Ordering::Relaxed),
        CAPS_LOCK_KEY => {
            if !released {
                CAPS_LOCK.fetch_xor(true, Ordering::Relaxed);
            }
        }
        // F1 is a desktop shortcut for opening the Browser window.
        0x3b if !released => input::push(input::KEY_BROWSER),
        _ if !released => {
            let Some(&normal) = NORMAL.get(code as usize) else {
                return;
            };
            if normal == 0 {
                return;
            }
            let mut ascii = if SHIFT.load(Ordering::Relaxed) {
                SHIFTED[code as usize]
            } else {
                normal
            };
            if CAPS_LOCK.load(Ordering::Relaxed) && ascii.is_ascii_alphabetic() {
                ascii ^= 0x20; // flip the case bit
            }
            input::push(ascii);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::handle_scancode;
    use crate::input;

    fn drain_input() {
        while input::pop().is_some() {}
    }

    #[test_case]
    fn left_windows_key_toggles_the_desktop_start_menu() {
        drain_input();
        handle_scancode(0xe0);
        handle_scancode(0x5b); // left Windows make
        assert_eq!(input::pop(), Some(input::KEY_WINDOWS));
        assert_eq!(input::pop(), None);

        // Releasing the key must not queue a second toggle.
        handle_scancode(0xe0);
        handle_scancode(0xdb); // left Windows break
        assert_eq!(input::pop(), None);
    }

    #[test_case]
    fn right_windows_key_toggles_the_desktop_start_menu() {
        drain_input();
        handle_scancode(0xe0);
        handle_scancode(0x5c); // right Windows make
        assert_eq!(input::pop(), Some(input::KEY_WINDOWS));
        assert_eq!(input::pop(), None);

        handle_scancode(0xe0);
        handle_scancode(0xdc); // right Windows break
        assert_eq!(input::pop(), None);
    }

    #[test_case]
    fn f1_opens_the_browser_and_release_does_not_queue_again() {
        drain_input();
        handle_scancode(0x3b); // F1 make
        assert_eq!(input::pop(), Some(input::KEY_BROWSER));
        assert_eq!(input::pop(), None);

        handle_scancode(0xbb); // F1 break
        assert_eq!(input::pop(), None);
    }
}
