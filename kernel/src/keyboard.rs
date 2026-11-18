//! PS/2 keyboard handling (scancode set 1, the firmware default).

use core::sync::atomic::{AtomicBool, Ordering};

use crate::input;

/// The two shift keys are tracked separately: releasing one must not clear the
/// other when both are held down.
static LEFT_SHIFT_HELD: AtomicBool = AtomicBool::new(false);
static RIGHT_SHIFT_HELD: AtomicBool = AtomicBool::new(false);
/// The two Ctrl keys, likewise tracked separately (the right one is extended).
static LEFT_CTRL: AtomicBool = AtomicBool::new(false);
static RIGHT_CTRL: AtomicBool = AtomicBool::new(false);
/// True after the caps-lock key has been toggled an odd number of times.
static CAPS_LOCK: AtomicBool = AtomicBool::new(false);
/// Guards against auto-repeat toggling caps lock on every repeated make code.
static CAPS_HELD: AtomicBool = AtomicBool::new(false);
/// Guards against auto-repeat re-triggering the special one-shot keys.
static WINDOWS_HELD: AtomicBool = AtomicBool::new(false);
static BROWSER_HELD: AtomicBool = AtomicBool::new(false);
static FILES_HELD: AtomicBool = AtomicBool::new(false);
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
        // as high control codes; right Ctrl is a modifier and everything else
        // (keypad enter, right Alt, ...) is still ignored.
        let released = scancode & 0x80 != 0;
        let code = scancode & 0x7f;
        if code == 0x1d {
            RIGHT_CTRL.store(!released, Ordering::Relaxed);
            return;
        }
        if released {
            if code == 0x5b || code == 0x5c {
                WINDOWS_HELD.store(false, Ordering::Relaxed);
            }
            return; // key release
        }
        let key = match code {
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
        if key == Some(input::KEY_WINDOWS) && WINDOWS_HELD.swap(true, Ordering::Relaxed) {
            return; // auto-repeat must not toggle the Start menu again
        }
        if let Some(key) = key {
            input::push(key);
        }
        return;
    }

    let released = scancode & 0x80 != 0;
    let code = scancode & 0x7f;

    match code {
        LEFT_SHIFT | RIGHT_SHIFT => {
            let held = !released;
            if code == LEFT_SHIFT {
                LEFT_SHIFT_HELD.store(held, Ordering::Relaxed);
            } else {
                RIGHT_SHIFT_HELD.store(held, Ordering::Relaxed);
            }
        }
        0x1d => LEFT_CTRL.store(!released, Ordering::Relaxed),
        CAPS_LOCK_KEY => {
            if released {
                CAPS_HELD.store(false, Ordering::Relaxed);
            } else if !CAPS_HELD.swap(true, Ordering::Relaxed) {
                CAPS_LOCK.fetch_xor(true, Ordering::Relaxed);
            }
        }
        // F1 opens the Browser, F2 opens the file manager.
        0x3b if !released => {
            if !BROWSER_HELD.swap(true, Ordering::Relaxed) {
                input::push(input::KEY_BROWSER);
            }
        }
        0x3b => BROWSER_HELD.store(false, Ordering::Relaxed),
        0x3c if !released => {
            if !FILES_HELD.swap(true, Ordering::Relaxed) {
                input::push(input::KEY_FILES);
            }
        }
        0x3c => FILES_HELD.store(false, Ordering::Relaxed),
        _ if !released => {
            let Some(&normal) = NORMAL.get(code as usize) else {
                return;
            };
            if normal == 0 {
                return;
            }
            let shift =
                LEFT_SHIFT_HELD.load(Ordering::Relaxed) || RIGHT_SHIFT_HELD.load(Ordering::Relaxed);
            let mut ascii = if shift {
                SHIFTED[code as usize]
            } else {
                normal
            };
            if CAPS_LOCK.load(Ordering::Relaxed) && ascii.is_ascii_alphabetic() {
                ascii ^= 0x20; // flip the case bit
            }
            if LEFT_CTRL.load(Ordering::Relaxed) || RIGHT_CTRL.load(Ordering::Relaxed) {
                // Ctrl+letter and Ctrl+[ \ ] ^ _ map to control codes 1..31.
                if ascii.is_ascii_alphabetic() || matches!(ascii, b'[' | b'\\' | b']' | b'^' | b'_')
                {
                    ascii &= 0x1f;
                }
            }
            input::push(ascii);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::Ordering;

    use super::handle_scancode;
    use crate::input;

    fn drain_input() {
        while input::pop().is_some() {}
        // Start every test from a known modifier state.
        super::LEFT_SHIFT_HELD.store(false, Ordering::Relaxed);
        super::RIGHT_SHIFT_HELD.store(false, Ordering::Relaxed);
        super::LEFT_CTRL.store(false, Ordering::Relaxed);
        super::RIGHT_CTRL.store(false, Ordering::Relaxed);
        super::CAPS_LOCK.store(false, Ordering::Relaxed);
        super::CAPS_HELD.store(false, Ordering::Relaxed);
        super::WINDOWS_HELD.store(false, Ordering::Relaxed);
        super::BROWSER_HELD.store(false, Ordering::Relaxed);
        super::FILES_HELD.store(false, Ordering::Relaxed);
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

    #[test_case]
    fn f2_opens_the_file_manager_and_release_does_not_queue_again() {
        drain_input();
        handle_scancode(0x3c); // F2 make
        assert_eq!(input::pop(), Some(input::KEY_FILES));
        assert_eq!(input::pop(), None);

        handle_scancode(0xbc); // F2 break
        assert_eq!(input::pop(), None);
    }

    #[test_case]
    fn releasing_one_shift_does_not_clear_the_other() {
        drain_input();
        handle_scancode(0x2a); // left shift make
        handle_scancode(0x36); // right shift make
        handle_scancode(0xaa); // left shift break
        handle_scancode(0x41); // 'a'
        assert_eq!(input::pop(), Some(b'A'));
    }

    #[test_case]
    fn caps_lock_only_toggles_on_the_first_make() {
        drain_input();
        handle_scancode(0x3a); // caps make
        handle_scancode(0x3a); // auto-repeat make
        handle_scancode(0x41); // 'a'
        assert_eq!(input::pop(), Some(b'A'));
        handle_scancode(0xba); // caps break
        handle_scancode(0x41); // 'a'
        assert_eq!(input::pop(), Some(b'A'));
    }

    #[test_case]
    fn ctrl_produces_a_control_code() {
        drain_input();
        handle_scancode(0x1d); // left ctrl make
        handle_scancode(0x2e); // 'c'
        assert_eq!(input::pop(), Some(0x03));
        handle_scancode(0x9d); // left ctrl break
        handle_scancode(0x2e); // 'c'
        assert_eq!(input::pop(), Some(b'c'));
    }

    #[test_case]
    fn holding_f1_does_not_reopen_the_browser() {
        drain_input();
        handle_scancode(0x3b); // F1 make
        handle_scancode(0x3b); // auto-repeat
        assert_eq!(input::pop(), Some(input::KEY_BROWSER));
        assert_eq!(input::pop(), None);
    }
}
