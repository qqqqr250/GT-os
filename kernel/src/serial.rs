//! Driver for the first 16550-compatible UART (COM1).
//!
//! The serial port is the kernel's out-of-band console: every line printed to
//! the screen is also written here, which is what makes the OS testable in CI
//! (QEMU can pipe COM1 to a socket or to stdout).
//!
//! Every lock on [`SERIAL`] is taken inside `without_interrupts` (post-07). The
//! timer handler prints through this same lock, so if the main thread held it
//! with interrupts enabled, the handler could fire, spin on the lock and never
//! let the main thread release it — a deadlock the CPU cannot recover from.

use spin::Mutex;
use x86_64::instructions::interrupts;

const COM1: u16 = 0x3F8;

unsafe fn outb(base: u16, offset: u16, value: u8) {
    unsafe {
        x86_64::instructions::port::Port::<u8>::new(base + offset).write(value);
    }
}

unsafe fn inb(base: u16, offset: u16) -> u8 {
    unsafe { x86_64::instructions::port::Port::<u8>::new(base + offset).read() }
}

/// A minimal, polled 16550 UART.
pub struct Serial {
    base: u16,
}

impl Serial {
    pub const fn new(base: u16) -> Self {
        Self { base }
    }

    /// Programs the UART for 115200 baud, 8N1, with the RX interrupt enabled.
    ///
    /// # Safety
    ///
    /// The caller must own the I/O ports of `base`, i.e. no other driver may
    /// talk to this UART at the same time.
    pub unsafe fn init(&mut self) {
        let base = self.base;
        unsafe {
            outb(base, 1, 0x00); // disable all interrupts while configuring
            outb(base, 3, 0x80); // enable DLAB to set the baud divisor
            outb(base, 0, 0x01); // divisor low byte: 1 -> 115200 baud
            outb(base, 1, 0x00); // divisor high byte
            outb(base, 3, 0x03); // 8 bits, no parity, one stop bit
            outb(base, 2, 0xC7); // enable + clear the FIFOs, 14 byte threshold
            outb(base, 4, 0x0B); // IRQs enabled, RTS/DSR set
            outb(base, 1, 0x01); // interrupt when a byte is received
        }
    }

    pub fn write_byte(&mut self, byte: u8) {
        // Wait for the transmit holding register to become empty. The bound
        // keeps a broken/missing UART from hanging the kernel forever.
        for _ in 0..100_000 {
            let status = unsafe { inb(self.base, 5) };
            if status & 0x20 != 0 {
                unsafe { outb(self.base, 0, byte) };
                return;
            }
        }
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_byte(byte);
        }
    }
}

static SERIAL: Mutex<Option<Serial>> = Mutex::new(None);

/// Initializes COM1. Safe to call once; later calls are no-ops.
pub fn init() {
    let mut serial = Serial::new(COM1);
    unsafe { serial.init() };
    *SERIAL.lock() = Some(serial);
}

pub fn write_str(s: &str) {
    write_bytes(s.as_bytes());
}

pub fn write_bytes(bytes: &[u8]) {
    // Interrupts off for the whole critical section, so the timer handler cannot
    // fire between taking the lock and releasing it.
    interrupts::without_interrupts(|| {
        if let Some(serial) = SERIAL.lock().as_mut() {
            serial.write_bytes(bytes);
        }
    });
}

/// Reads one byte from the UART without taking the driver lock.
///
/// Only the IRQ4 handler may call this: the main thread must never read the
/// receive register, otherwise the two contexts would race for bytes.
pub fn read_byte_irq() -> Option<u8> {
    let status = unsafe { inb(COM1, 5) };
    if status & 0x01 != 0 {
        Some(unsafe { inb(COM1, 0) })
    } else {
        None
    }
}

/// `core::fmt::Write` adapter used when the console lock is unavailable.
pub struct Writer;

impl core::fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        write_str(s);
        Ok(())
    }
}

/// Best-effort output for the panic path, where taking a lock could deadlock.
pub fn try_write_str(s: &str) {
    let mut lock = SERIAL.try_lock();
    if let Some(serial) = lock.as_mut().and_then(|guard| guard.as_mut()) {
        serial.write_bytes(s.as_bytes());
    }
}

impl core::fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write_bytes(s.as_bytes());
        Ok(())
    }
}

/// Formats `args` to COM1. Backing function for the [`serial_print!`] macros.
///
/// It is a no-op before [`init`], so early prints (and the test harness) cannot
/// fault on an unconfigured UART.
///
/// The lock is taken inside `without_interrupts`; see the module docs.
pub fn print_args(args: core::fmt::Arguments<'_>) {
    use core::fmt::Write;
    interrupts::without_interrupts(|| {
        if let Some(serial) = SERIAL.lock().as_mut() {
            let _ = serial.write_fmt(args);
        }
    });
}

/// Prints to the host through the serial interface (COM1).
///
/// This is the machine-readable mirror of the on-screen console, which is what
/// makes the OS testable: QEMU pipes COM1 to stdout or a socket.
#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => {
        $crate::serial_print_args(format_args!($($arg)*))
    };
}

/// Prints to the host through the serial interface, appending a newline.
#[macro_export]
macro_rules! serial_println {
    () => {
        $crate::serial_print!("\n")
    };
    ($fmt:expr) => {
        $crate::serial_print!(concat!($fmt, "\n"))
    };
    ($fmt:expr, $($arg:tt)*) => {
        $crate::serial_print!(concat!($fmt, "\n"), $($arg)*)
    };
}
