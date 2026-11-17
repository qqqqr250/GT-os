//! The 8254 programmable interval timer.
//!
//! Channel 0 is wired to IRQ0. [`init`] programs it to divide its 1.193182 MHz
//! input clock down to [`TICK_HZ`], so the CPU takes a timer interrupt at a
//! steady rate; [`divisor`] is the pure part of that calculation and is unit
//! tested.

use crate::port::outb;

/// Frequency of the timer interrupt.
pub const TICK_HZ: u32 = 100;

/// The PIT's fixed input clock: 1.193182 MHz.
const PIT_FREQUENCY: u32 = 1_193_182;

const PIT_COMMAND: u16 = 0x43;
const PIT_CHANNEL0: u16 = 0x40;
/// Channel 0, access lo/hi byte, mode 3 (square wave), binary.
const PIT_MODE: u8 = 0x36;

/// The divisor that turns the 1.193182 MHz input clock into `hz`.
#[must_use]
pub const fn divisor(hz: u32) -> u16 {
    (PIT_FREQUENCY / hz) as u16
}

/// Programs channel 0 to raise IRQ0 at [`TICK_HZ`].
pub fn init() {
    let divisor = divisor(TICK_HZ);
    // SAFETY: the PIT command and channel-0 ports belong to this driver, and the
    // divisor is written low byte first as the mode byte requires.
    unsafe {
        outb(PIT_COMMAND, PIT_MODE);
        outb(PIT_CHANNEL0, (divisor & 0xff) as u8);
        outb(PIT_CHANNEL0, ((divisor >> 8) & 0xff) as u8);
    }
}
