//! Raw x86 port I/O: the 16550 UART's registers, the PIC/PIT control ports and
//! the QEMU exit device.
//!
//! [`serial`](crate::serial), [`pic`](crate::pic) and [`pit`](crate::pit)
//! program their devices through these helpers, and the in-QEMU test framework
//! writes a 32-bit code to the `isa-debug-exit` port (`0xF4`) so the runner can
//! map QEMU's exit status back onto `cargo test`.

/// I/O port of the `isa-debug-exit` device the test runner configures.
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

/// Writes the 32-bit `value` to the I/O port `port`.
///
/// # Safety
///
/// As [`outb`], but the device must accept a 32-bit write (`isa-debug-exit` is
/// configured with `iosize=0x04`).
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
