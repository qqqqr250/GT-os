//! PS/2 mouse driver: initialises the auxiliary (mouse) port on the 8042
//! controller, puts the mouse into streaming mode with 3-byte packets, and
//! exposes the latest pointer state (x, y, buttons).

use core::sync::atomic::{AtomicI32, AtomicU8, Ordering};

use spin::Mutex;
use x86_64::instructions::port::Port;

/// Mouse state shared between the IRQ handler and the main thread.
static MOUSE_X: AtomicI32 = AtomicI32::new(400);
static MOUSE_Y: AtomicI32 = AtomicI32::new(300);
static MOUSE_BUTTONS: AtomicU8 = AtomicU8::new(0);

/// Accumulator for the current 3-byte packet.
static PACKET: Mutex<[u8; 3]> = Mutex::new([0; 3]);
static PACKET_IDX: AtomicU8 = AtomicU8::new(0);

const DATA_PORT: u16 = 0x60;
const STATUS_PORT: u16 = 0x64;
const COMMAND_PORT: u16 = 0x64;

/// Wait until the controller's input buffer is empty (safe to write a command).
fn wait_input_clear() {
    for _ in 0..100_000 {
        let status: u8 = unsafe { Port::<u8>::new(STATUS_PORT).read() };
        if status & 0x02 == 0 {
            return;
        }
    }
}

/// Wait until the controller has a byte ready for us to read.
fn wait_output_full() {
    for _ in 0..100_000 {
        let status: u8 = unsafe { Port::<u8>::new(STATUS_PORT).read() };
        if status & 0x01 != 0 {
            return;
        }
    }
}

/// Write a byte to the 8042 command register (0x64).
fn send_command(cmd: u8) {
    wait_input_clear();
    unsafe { Port::<u8>::new(COMMAND_PORT).write(cmd) };
}

/// Write a byte to the 8042 data register (0x60).
fn write_data(byte: u8) {
    wait_input_clear();
    unsafe { Port::<u8>::new(DATA_PORT).write(byte) };
}

/// Read a byte from the 8042 data register (0x60).
fn read_data() -> u8 {
    wait_output_full();
    unsafe { Port::<u8>::new(DATA_PORT).read() }
}

/// Drain any bytes already sitting in the controller's output buffer.
fn flush_output() {
    for _ in 0..32 {
        let status: u8 = unsafe { Port::<u8>::new(STATUS_PORT).read() };
        if status & 0x01 == 0 {
            break;
        }
        let _ = unsafe { Port::<u8>::new(DATA_PORT).read() };
    }
}

/// Send a byte to the *mouse* (prefixing it with the 0xD4 "next byte is for
/// the auxiliary device" command).
fn mouse_write(byte: u8) {
    send_command(0xD4);
    write_data(byte);
}

/// Read the controller configuration byte (command 0x20).
fn read_config() -> u8 {
    send_command(0x20);
    read_data()
}

/// Write the controller configuration byte (command 0x60).
fn write_config(config: u8) {
    send_command(0x60);
    write_data(config);
}

/// Initialise the PS/2 mouse.
///
/// The ordering matters. The mouse interrupt (IRQ12) is deliberately left
/// masked in the controller configuration while the mouse command/response
/// handshake runs, so the interrupt handler cannot consume the ACK bytes the
/// handshake is waiting for. IRQ12 is enabled only once the mouse is streaming.
pub fn init() {
    // Disable both ports while we reconfigure the controller.
    send_command(0xAD); // disable keyboard port
    send_command(0xA7); // disable auxiliary (mouse) port
    flush_output();

    // Configuration: keep the keyboard interrupt, keep the mouse interrupt
    // masked for now, and enable the mouse clock (clear bit 5).
    let mut config = read_config();
    config |= 0x01; // keyboard interrupt (IRQ1)
    config &= !0x02; // mouse interrupt (IRQ12) stays off during setup
    config &= !0x20; // mouse clock enabled
    write_config(config);

    // Bring both ports back up.
    send_command(0xA8); // enable auxiliary port
    send_command(0xAE); // enable keyboard port

    // Mouse: load defaults, then enable data reporting.
    mouse_write(0xF6); // set defaults
    let _ = read_data(); // ACK
    mouse_write(0xF4); // enable data reporting
    let _ = read_data(); // ACK

    // Now that the handshake is finished, enable the mouse interrupt. The
    // mouse is stationary at boot, so this exchange is not disturbed.
    let mut config = read_config();
    config |= 0x02; // mouse interrupt (IRQ12)
    write_config(config);
}

/// Called from the IRQ12 handler with a single byte.
pub fn handle_byte(byte: u8) {
    let idx = PACKET_IDX.load(Ordering::Relaxed);

    // Resynchronise: the first byte of a packet always has bit 3 set. If we are
    // waiting for the first byte and this one does not, drop it (this is how we
    // ignore stray ACK bytes the mouse sends in response to commands).
    if idx == 0 && byte & 0x08 == 0 {
        return;
    }

    let mut pkt = PACKET.lock();
    pkt[idx as usize] = byte;
    let next = idx + 1;
    if next < 3 {
        PACKET_IDX.store(next, Ordering::Relaxed);
        return;
    }
    PACKET_IDX.store(0, Ordering::Relaxed);

    let b0 = pkt[0];
    let dx = pkt[1] as i8 as i32;
    let dy = pkt[2] as i8 as i32;
    drop(pkt);

    let x = (MOUSE_X.load(Ordering::Relaxed) + dx).max(0);
    // The PS/2 Y axis points up; screen coordinates point down.
    let y = (MOUSE_Y.load(Ordering::Relaxed) - dy).max(0);

    MOUSE_X.store(x, Ordering::Relaxed);
    MOUSE_Y.store(y, Ordering::Relaxed);
    MOUSE_BUTTONS.store(b0 & 0x07, Ordering::Relaxed);
}

pub fn x() -> i32 {
    MOUSE_X.load(Ordering::Relaxed)
}

pub fn y() -> i32 {
    MOUSE_Y.load(Ordering::Relaxed)
}

pub fn buttons() -> u8 {
    MOUSE_BUTTONS.load(Ordering::Relaxed)
}

pub fn set_bounds(width: i32, height: i32) {
    let x = MOUSE_X.load(Ordering::Relaxed).clamp(0, width - 1);
    let y = MOUSE_Y.load(Ordering::Relaxed).clamp(0, height - 1);
    MOUSE_X.store(x, Ordering::Relaxed);
    MOUSE_Y.store(y, Ordering::Relaxed);
}
