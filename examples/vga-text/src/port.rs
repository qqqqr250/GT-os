//! Raw x86 port I/O, used to move the hardware text cursor and to exit QEMU.
//!
//! The VGA text buffer is memory-mapped, but the cursor is programmed through the
//! CRTC's index/data register pair at ports `0x3D4`/`0x3D5`. The in-QEMU test
//! additionally writes a 32-bit code to the `isa-debug-exit` port (`0xF4`) so the
//! runner can map QEMU's exit status back onto `cargo test`; that exit path is
//! gated behind the `qemu-test` feature, so a normal build carries only what the
//! cursor needs.
//!
//! Those ports exist only on x86, so the module is compiled only there; on any
//! other host the functions would not be callable anyway.

/// CRTC index register.
pub const CRTC_INDEX: u16 = 0x3d4;
/// CRTC data register.
pub const CRTC_DATA: u16 = 0x3d5;
/// CRTC index of the cursor-location high byte.
pub const CURSOR_HIGH: u8 = 0x0e;
/// CRTC index of the cursor-location low byte.
pub const CURSOR_LOW: u8 = 0x0f;
/// I/O port of the `isa-debug-exit` device the in-QEMU test runner configures.
#[cfg(feature = "qemu-test")]
pub const DEBUG_EXIT: u16 = 0xf4;

/// Writes `value` to the I/O port `port`.
///
/// # Safety
///
/// `port` must be an I/O port the CPU is allowed to write, and the write must not
/// race a device that is mid-handshake.
pub unsafe fn outb(port: u16, value: u8) {
    // SAFETY: the caller guarantees the port is writable. `out` reads the flags
    // and the `dx`/`al` registers and clobbers nothing else.
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Reads one byte from the I/O port `port`.
///
/// # Safety
///
/// `port` must be an I/O port the CPU is allowed to read.
#[must_use]
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: the caller guarantees the port is readable.
    unsafe {
        core::arch::asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

/// Writes the 32-bit `value` to the I/O port `port`.
///
/// # Safety
///
/// As [`outb`], but the device must accept a 32-bit write (`isa-debug-exit` is
/// configured with `iosize=0x04`).
#[cfg(feature = "qemu-test")]
pub unsafe fn outl(port: u16, value: u32) {
    // SAFETY: the caller guarantees the port accepts a 32-bit write.
    unsafe {
        core::arch::asm!(
            "out dx, eax",
            in("dx") port,
            in("eax") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Moves the hardware text cursor to the linear cell `index`.
///
/// The cursor is cosmetic: the writer tracks its own position, so a failure to
/// program the CRTC would only leave the blinking underline behind.
pub fn set_text_cursor(index: usize) {
    let position = index as u16;
    // SAFETY: 0x3D4/0x3D5 are the CRTC ports and only the VGA device listens on
    // them; the sequence is the documented way to set the cursor location.
    unsafe {
        outb(CRTC_INDEX, CURSOR_HIGH);
        outb(CRTC_DATA, (position >> 8) as u8);
        outb(CRTC_INDEX, CURSOR_LOW);
        outb(CRTC_DATA, position as u8);
    }
}
