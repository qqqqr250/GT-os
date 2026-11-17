//! The 8259 programmable interrupt controller.
//!
//! The PICs deliver hardware interrupts on vectors that collide with the CPU's
//! own exceptions: out of reset, IRQ0 is vector 8 — the double fault. [`init`]
//! remaps them, so the master PIC's IRQs land on [`PIC1_OFFSET`] (`0x20`..`0x27`)
//! and the slave's on [`PIC2_OFFSET`] (`0x28`..`0x2F`), well clear of the
//! exception vectors.
//!
//! Everything starts masked. [`unmask`] enables the lines the kernel uses, and
//! [`end_of_interrupt`] acknowledges an interrupt once its handler is done.

use crate::port::outb;

const PIC1_COMMAND: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_COMMAND: u16 = 0xa0;
const PIC2_DATA: u16 = 0xa1;

/// The vector the master PIC's IRQ0 is remapped to.
pub const PIC1_OFFSET: u8 = 0x20;
/// The vector the slave PIC's IRQ8 is remapped to.
pub const PIC2_OFFSET: u8 = 0x28;

/// ICW1: start initialization, expect ICW4, cascade, edge-triggered.
const ICW1_INIT: u8 = 0x11;
/// ICW3: the slave PIC is wired to the master's IRQ2.
const ICW3_CASCADE: u8 = 0x04;
/// ICW3: the slave PIC's identity.
const ICW3_SLAVE_ID: u8 = 0x02;
/// ICW4: 8086/88 mode.
const ICW4_8086: u8 = 0x01;
/// The end-of-interrupt command.
const PIC_EOI: u8 = 0x20;

/// A tiny delay between the ICW writes; port `0x80` is unused.
fn io_wait() {
    // SAFETY: port 0x80 is the traditional POST/delay port and accepts a write.
    unsafe { outb(0x80, 0x00) };
}

/// The master-PIC lines that must stay unmasked: the timer (IRQ0), the keyboard
/// (IRQ1) and the cascade to the slave PIC (IRQ2).
#[must_use]
pub const fn master_lines() -> u8 {
    (1 << 0) | (1 << 1) | (1 << 2)
}

/// Remaps both PICs and masks every line.
///
/// The remap runs before any interrupt is enabled, so the window in which an
/// IRQ0 would arrive on vector 8 (the double fault) never opens.
pub fn init() {
    // SAFETY: the PIC command/data ports belong to this driver; the whole ICW
    // sequence runs with interrupts still disabled.
    unsafe {
        outb(PIC1_COMMAND, ICW1_INIT);
        io_wait();
        outb(PIC2_COMMAND, ICW1_INIT);
        io_wait();

        outb(PIC1_DATA, PIC1_OFFSET);
        io_wait();
        outb(PIC2_DATA, PIC2_OFFSET);
        io_wait();

        outb(PIC1_DATA, ICW3_CASCADE);
        io_wait();
        outb(PIC2_DATA, ICW3_SLAVE_ID);
        io_wait();

        outb(PIC1_DATA, ICW4_8086);
        io_wait();
        outb(PIC2_DATA, ICW4_8086);
        io_wait();

        // Everything masked until `unmask` says otherwise.
        outb(PIC1_DATA, 0xff);
        outb(PIC2_DATA, 0xff);
    }
}

/// Unmasks `master` and `slave`: a set bit is an *enabled* line.
///
/// The 8259's interrupt mask register is active-low, so the value written is the
/// complement of the lines to enable.
pub fn unmask(master: u8, slave: u8) {
    // SAFETY: the data ports are the PIC's mask registers.
    unsafe {
        outb(PIC1_DATA, !master);
        outb(PIC2_DATA, !slave);
    }
}

/// Signals the end of the interrupt on `irq`.
///
/// An IRQ on the slave PIC needs its EOI sent to *both* PICs, slave first.
pub fn end_of_interrupt(irq: u8) {
    // SAFETY: the command ports are the PIC's EOI registers.
    unsafe {
        if irq >= 8 {
            outb(PIC2_COMMAND, PIC_EOI);
        }
        outb(PIC1_COMMAND, PIC_EOI);
    }
}
