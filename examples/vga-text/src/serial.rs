//! A minimal 16550 UART driver for COM1.
//!
//! The VGA text buffer is invisible to a headless QEMU run, so every console
//! write is mirrored here: `-serial stdio` then shows the same text the guest
//! would draw on a real text-mode monitor.

use core::fmt;

use crate::port::{inb, outb};

/// Base I/O port of COM1.
pub const COM1: u16 = 0x3f8;

/// Initializes COM1 to 115200 baud, 8N1, with a 14-byte FIFO.
pub fn init() {
    // SAFETY: the standard COM1 register block; each write is part of the
    // documented 16550 initialization sequence.
    unsafe {
        outb(COM1 + 1, 0x00); // disable interrupts
        outb(COM1 + 3, 0x80); // enable the divisor latch
        outb(COM1, 0x03); // divisor low  (115200 baud)
        outb(COM1 + 1, 0x00); // divisor high
        outb(COM1 + 3, 0x03); // 8 bits, no parity, one stop bit
        outb(COM1 + 2, 0xc7); // enable and clear the FIFO
        outb(COM1 + 4, 0x0b); // IRQs on, RTS/DSR set
    }
}

/// Whether the transmitter is ready for another byte.
fn transmit_empty() -> bool {
    // SAFETY: COM1+5 is the line-status register.
    unsafe { inb(COM1 + 5) & 0x20 != 0 }
}

/// Writes one byte, spinning until the transmitter drains.
pub fn write_byte(byte: u8) {
    while !transmit_empty() {}
    // SAFETY: COM1 is the transmit register, and the transmitter is empty.
    unsafe { outb(COM1, byte) };
}

/// Writes `s`, translating `\n` to the `\r\n` a terminal expects.
pub fn write_str(s: &str) {
    for byte in s.bytes() {
        if byte == b'\n' {
            write_byte(b'\r');
        }
        write_byte(byte);
    }
}

/// Writes formatted output to COM1.
pub fn print_args(args: fmt::Arguments<'_>) {
    let _ = fmt::Write::write_fmt(&mut Writer, args);
}

/// A [`fmt::Write`] adapter over the UART.
pub struct Writer;

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str(s);
        Ok(())
    }
}
