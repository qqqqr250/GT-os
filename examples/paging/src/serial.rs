//! A minimal 16550 UART driver for COM1.
//!
//! The deadlock fix from post-07 lives here. The timer handler prints through
//! [`print_args`] as well, so if the main thread held the driver lock while the
//! timer fired, the handler would spin on that lock forever. [`print_args`]
//! takes the lock inside
//! [`without_interrupts`](x86_64::instructions::interrupts::without_interrupts),
//! so the handler can never run while the lock is held.

use core::fmt;

use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::port::{inb, outb};

/// Base I/O port of COM1.
pub const COM1: u16 = 0x3f8;

/// The UART, behind a lock shared by the main thread and the interrupt handlers.
static SERIAL: Mutex<Option<Serial>> = Mutex::new(None);

/// A [`fmt::Write`] adapter over the UART. The lock around it is the point.
struct Serial;

impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str(s);
        Ok(())
    }
}

/// Programs COM1 for 115200 baud, 8N1, with a 14-byte FIFO.
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
    *SERIAL.lock() = Some(Serial);
}

/// Whether the transmitter is ready for another byte.
fn transmit_empty() -> bool {
    // SAFETY: COM1+5 is the line-status register.
    unsafe { inb(COM1 + 5) & 0x20 != 0 }
}

/// Writes one byte, spinning until the transmitter drains.
fn write_byte(byte: u8) {
    while !transmit_empty() {}
    // SAFETY: COM1 is the transmit register, and the transmitter is empty.
    unsafe { outb(COM1, byte) };
}

/// Writes `s`, translating `\n` to the `\r\n` a terminal expects.
fn write_str(s: &str) {
    for byte in s.bytes() {
        if byte == b'\n' {
            write_byte(b'\r');
        }
        write_byte(byte);
    }
}

/// Formats `args` to COM1 with interrupts disabled around the lock.
///
/// This is post-07's deadlock fix. The timer handler prints through the same
/// lock, so the lock has to be held with interrupts off: otherwise the handler
/// could fire while the main thread is mid-print and spin on the lock forever.
pub fn print_args(args: fmt::Arguments<'_>) {
    interrupts::without_interrupts(|| {
        use core::fmt::Write;
        if let Some(serial) = SERIAL.lock().as_mut() {
            let _ = serial.write_fmt(args);
        }
    });
}
