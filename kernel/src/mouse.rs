//! PS/2 mouse driver for the auxiliary port of the 8042 controller.
//!
//! Initialisation puts the mouse into its standard 3-byte streaming packet mode
//! and enables IRQ12. Each complete packet updates an absolute pointer position
//! and pushes move/button events into the ring-3 input mirror. The shell does
//! not consume mouse bytes, so a guest can use the pointer without disturbing
//! command-line editing.

use core::sync::atomic::{AtomicI32, AtomicU8, Ordering};

use spin::Mutex;
use x86_64::instructions::port::Port;

use crate::console;
use crate::input::{self, Event};

const DATA_PORT: u16 = 0x60;
const STATUS_PORT: u16 = 0x64;
const COMMAND_PORT: u16 = 0x64;

static MOUSE_X: AtomicI32 = AtomicI32::new(0);
static MOUSE_Y: AtomicI32 = AtomicI32::new(0);
static MOUSE_BUTTONS: AtomicU8 = AtomicU8::new(0);
static PACKET: Mutex<[u8; 3]> = Mutex::new([0; 3]);
static PACKET_INDEX: AtomicU8 = AtomicU8::new(0);

fn wait_input_clear() {
    for _ in 0..100_000 {
        // SAFETY: reading the controller status port has no side effects.
        let status = unsafe { Port::<u8>::new(STATUS_PORT).read() };
        if status & 0x02 == 0 {
            return;
        }
    }
}

fn wait_output_full() {
    for _ in 0..100_000 {
        // SAFETY: reading the controller status port has no side effects.
        let status = unsafe { Port::<u8>::new(STATUS_PORT).read() };
        if status & 0x01 != 0 {
            return;
        }
    }
}

fn send_command(command: u8) {
    wait_input_clear();
    // SAFETY: the 8042 command port is a standard x86 I/O port.
    unsafe { Port::<u8>::new(COMMAND_PORT).write(command) };
}

fn write_data(byte: u8) {
    wait_input_clear();
    // SAFETY: the 8042 data port is a standard x86 I/O port.
    unsafe { Port::<u8>::new(DATA_PORT).write(byte) };
}

fn read_data() -> u8 {
    wait_output_full();
    // SAFETY: the caller waits for the controller's output buffer to fill.
    unsafe { Port::<u8>::new(DATA_PORT).read() }
}

fn flush_output() {
    for _ in 0..32 {
        // SAFETY: reading the controller status port has no side effects.
        let status = unsafe { Port::<u8>::new(STATUS_PORT).read() };
        if status & 0x01 == 0 {
            break;
        }
        let _ = read_data();
    }
}

fn mouse_write(byte: u8) {
    send_command(0xd4);
    write_data(byte);
}

fn read_config() -> u8 {
    send_command(0x20);
    read_data()
}

fn write_config(config: u8) {
    send_command(0x60);
    write_data(config);
}

fn pointer_event(kind: u32, code: u32, x: i32, y: i32, buttons: u8) {
    input::push_event(Event {
        kind,
        code,
        modifiers: 0,
        x,
        y,
        value: u32::from(buttons),
    });
}

/// Initialises the mouse and centres the pointer in the VBE framebuffer.
///
/// IRQ12 stays masked in the 8042 configuration until the command handshake is
/// complete, so its ACK bytes cannot be stolen by the interrupt handler. The PIC
/// line itself is unmasked later in `interrupts::init`.
pub fn init() {
    let info = console::framebuffer_info();
    MOUSE_X.store(
        i32::try_from(info.width / 2).unwrap_or(0),
        Ordering::Relaxed,
    );
    MOUSE_Y.store(
        i32::try_from(info.height / 2).unwrap_or(0),
        Ordering::Relaxed,
    );

    send_command(0xad); // disable keyboard port
    send_command(0xa7); // disable auxiliary port
    flush_output();

    let mut config = read_config();
    config |= 0x01; // keyboard interrupt
    config &= !0x02; // mouse interrupt stays masked during setup
    config &= !0x20; // mouse clock enabled
    write_config(config);

    send_command(0xa8); // enable auxiliary port
    send_command(0xae); // enable keyboard port

    mouse_write(0xf6); // load default settings
    let _ = read_data(); // ACK
    mouse_write(0xf4); // enable data reporting
    let _ = read_data(); // ACK

    let mut config = read_config();
    config |= 0x02; // enable mouse interrupt now that streaming started
    write_config(config);
}

/// Consumes one byte from the auxiliary port, emitting events for full packets.
pub fn handle_byte(byte: u8) {
    let index = PACKET_INDEX.load(Ordering::Relaxed);
    if index == 0 && byte & 0x08 == 0 {
        return; // resynchronise after stray command responses
    }

    let mut packet = PACKET.lock();
    packet[index as usize] = byte;
    let next = index + 1;
    if next < 3 {
        PACKET_INDEX.store(next, Ordering::Relaxed);
        return;
    }
    PACKET_INDEX.store(0, Ordering::Relaxed);

    let flags = packet[0];
    let dx = i32::from(packet[1] as i8);
    let dy = i32::from(packet[2] as i8);
    drop(packet);

    let info = console::framebuffer_info();
    let max_x = i32::try_from(info.width.saturating_sub(1)).unwrap_or(0);
    let max_y = i32::try_from(info.height.saturating_sub(1)).unwrap_or(0);
    let x = (MOUSE_X.load(Ordering::Relaxed) + dx).clamp(0, max_x);
    // PS/2 Y grows upward; framebuffer Y grows downward.
    let y = (MOUSE_Y.load(Ordering::Relaxed) - dy).clamp(0, max_y);
    MOUSE_X.store(x, Ordering::Relaxed);
    MOUSE_Y.store(y, Ordering::Relaxed);

    let old_buttons = MOUSE_BUTTONS.load(Ordering::Relaxed);
    let buttons = flags & 0x07;
    for bit in 0..3u8 {
        let mask = 1u8 << bit;
        if old_buttons & mask == buttons & mask {
            continue;
        }
        pointer_event(input::EVENT_MOUSE_BUTTON, u32::from(bit), x, y, buttons);
    }
    MOUSE_BUTTONS.store(buttons, Ordering::Relaxed);

    if dx != 0 || dy != 0 {
        pointer_event(input::EVENT_MOUSE_MOVE, 0, x, y, buttons);
    }
}

/// Current pointer position, used by diagnostics.
#[must_use]
pub fn position() -> (i32, i32) {
    (
        MOUSE_X.load(Ordering::Relaxed),
        MOUSE_Y.load(Ordering::Relaxed),
    )
}

/// Currently held button mask, used by diagnostics.
#[must_use]
pub fn buttons() -> u8 {
    MOUSE_BUTTONS.load(Ordering::Relaxed)
}
