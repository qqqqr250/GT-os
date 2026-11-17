//! ATA PIO disk driver.
//!
//! Talks to the drives on the primary IDE channel (I/O base `0x1F0`) using
//! 28-bit LBA and programmed I/O: no DMA, no interrupts. QEMU attaches the
//! boot disk as the primary master and the `RustOS` data disk as the primary
//! slave. Both [`read_sectors`] and [`write_sectors`] take a drive index of
//! zero (master) or one (slave). The transfer is polled, which matches the
//! rest of the kernel and keeps the driver usable before (and without) the
//! interrupt controller.

use alloc::string::String;

use spin::Mutex;
use x86_64::instructions::port::Port;

const PRIMARY_IO: u16 = 0x1F0;
const PRIMARY_CTRL: u16 = 0x3F6;

const REG_DATA: u16 = 0;
const REG_SECTOR_COUNT: u16 = 2;
const REG_LBA_LOW: u16 = 3;
const REG_LBA_MID: u16 = 4;
const REG_LBA_HIGH: u16 = 5;
const REG_DRIVE: u16 = 6;
const REG_STATUS: u16 = 7;
const REG_COMMAND: u16 = 7;

const STATUS_BSY: u8 = 0x80;
const STATUS_DF: u8 = 0x20;
const STATUS_DRQ: u8 = 0x08;
const STATUS_ERR: u8 = 0x01;

const CMD_READ: u8 = 0x20;
const CMD_WRITE: u8 = 0x30;
const CMD_IDENTIFY: u8 = 0xEC;
const CMD_FLUSH: u8 = 0xE7;

/// One sector is always 512 bytes on the ATA interface.
pub const SECTOR_BYTES: usize = 512;

/// How many drives the primary channel can hold (master and slave).
pub const DRIVES: usize = 2;

struct Drive {
    sectors: u32,
    model: String,
}

static DRIVES_TABLE: Mutex<[Option<Drive>; DRIVES]> = Mutex::new([None, None]);

/// Reads the alternate status register four times, the architectural 400 ns
/// settle delay after a command or register write.
fn delay() {
    // SAFETY: 0x3F6 is the primary channel's control/alternate-status port.
    unsafe {
        let mut alt = Port::<u8>::new(PRIMARY_CTRL);
        for _ in 0..4 {
            alt.read();
        }
    }
}

/// Probes both drives, returning how many responded.
pub fn init() -> usize {
    let mut table = DRIVES_TABLE.lock();
    let mut found = 0;
    for (index, slot) in table.iter_mut().enumerate() {
        if let Some(drive) = identify(index as u8) {
            *slot = Some(drive);
            found += 1;
        }
    }
    found
}

/// Whether drive `index` was detected.
pub fn present(index: usize) -> bool {
    DRIVES_TABLE.lock()[index].is_some()
}

/// The capacity of drive `index` in 512-byte sectors, or 0 when absent.
pub fn sectors(index: usize) -> u32 {
    DRIVES_TABLE.lock()[index]
        .as_ref()
        .map_or(0, |drive| drive.sectors)
}

/// The model string of drive `index`, or `None` when absent.
pub fn model(index: usize) -> Option<String> {
    DRIVES_TABLE.lock()[index]
        .as_ref()
        .map(|drive| drive.model.clone())
}

/// Runs ATA IDENTIFY DEVICE and decodes the interesting fields.
fn identify(drive: u8) -> Option<Drive> {
    // SAFETY: all accesses are to the primary channel's register window, and
    // the drive number is 0 or 1.
    unsafe {
        let mut status = Port::<u8>::new(PRIMARY_IO + REG_STATUS);
        Port::<u8>::new(PRIMARY_IO + REG_DRIVE).write(0xA0 | (drive << 4));
        delay();
        Port::<u8>::new(PRIMARY_IO + REG_SECTOR_COUNT).write(0);
        Port::<u8>::new(PRIMARY_IO + REG_LBA_LOW).write(0);
        Port::<u8>::new(PRIMARY_IO + REG_LBA_MID).write(0);
        Port::<u8>::new(PRIMARY_IO + REG_LBA_HIGH).write(0);
        Port::<u8>::new(PRIMARY_IO + REG_COMMAND).write(CMD_IDENTIFY);
        delay();

        // A floating bus reads as zero: no drive on this position.
        if status.read() == 0 {
            return None;
        }
        if !wait_not_busy(&mut status) {
            return None;
        }
        // Non-zero LBA mid/high means the device is not a plain ATA disk.
        if Port::<u8>::new(PRIMARY_IO + REG_LBA_MID).read() != 0
            || Port::<u8>::new(PRIMARY_IO + REG_LBA_HIGH).read() != 0
        {
            return None;
        }
        if !wait_drq(&mut status) {
            return None;
        }

        let mut words = [0u16; 256];
        let mut data = Port::<u16>::new(PRIMARY_IO + REG_DATA);
        for word in &mut words {
            *word = data.read();
        }

        // Words 27..47 hold the model, big-endian within each word.
        let mut model_bytes = [0u8; 40];
        for i in 0..20 {
            let word = words[27 + i];
            model_bytes[i * 2] = (word >> 8) as u8;
            model_bytes[i * 2 + 1] = (word & 0xFF) as u8;
        }
        let model: String = String::from_utf8_lossy(&model_bytes).trim().into();
        // Words 60/61 hold the 28-bit sector count.
        let sectors = u32::from(words[60]) | (u32::from(words[61]) << 16);

        Some(Drive { sectors, model })
    }
}

/// Waits for the busy flag to clear.
fn wait_not_busy(status: &mut Port<u8>) -> bool {
    for _ in 0..1_000_000 {
        // SAFETY: the caller passes the status port of the primary channel.
        if unsafe { status.read() } & STATUS_BSY == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Waits until a data transfer can start.
fn wait_drq(status: &mut Port<u8>) -> bool {
    for _ in 0..1_000_000 {
        // SAFETY: as above.
        let value = unsafe { status.read() };
        if value & STATUS_BSY == 0 && value & STATUS_DRQ != 0 {
            return true;
        }
        if value & (STATUS_DF | STATUS_ERR) != 0 {
            return false;
        }
        core::hint::spin_loop();
    }
    false
}

/// Programs the drive/head and LBA registers for an LBA28 transfer.
fn select(index: usize, lba: u32, count: u8) {
    // SAFETY: all ports belong to the primary channel.
    unsafe {
        let mut status = Port::<u8>::new(PRIMARY_IO + REG_STATUS);
        wait_not_busy(&mut status);
        Port::<u8>::new(PRIMARY_IO + REG_DRIVE)
            .write(0xE0 | ((index as u8) << 4) | ((lba >> 24) & 0x0F) as u8);
        delay();
        Port::<u8>::new(PRIMARY_IO + REG_SECTOR_COUNT).write(count);
        Port::<u8>::new(PRIMARY_IO + REG_LBA_LOW).write((lba & 0xFF) as u8);
        Port::<u8>::new(PRIMARY_IO + REG_LBA_MID).write(((lba >> 8) & 0xFF) as u8);
        Port::<u8>::new(PRIMARY_IO + REG_LBA_HIGH).write(((lba >> 16) & 0xFF) as u8);
    }
}

/// Reads `count` sectors starting at `lba` into `out`.
///
/// `out` must hold at least `count * 512` bytes.
pub fn read_sectors(index: usize, lba: u32, count: u8, out: &mut [u8]) -> bool {
    if count == 0 || out.len() < usize::from(count) * SECTOR_BYTES || !present(index) {
        return false;
    }
    if u64::from(lba) + u64::from(count) > u64::from(sectors(index)) {
        return false;
    }

    select(index, lba, count);
    // SAFETY: the ports belong to the primary channel and `out` is large enough.
    unsafe {
        let mut status = Port::<u8>::new(PRIMARY_IO + REG_STATUS);
        let mut data = Port::<u16>::new(PRIMARY_IO + REG_DATA);
        Port::<u8>::new(PRIMARY_IO + REG_COMMAND).write(CMD_READ);
        for sector in 0..usize::from(count) {
            if !wait_drq(&mut status) {
                return false;
            }
            for word in 0..256 {
                let value = data.read();
                out[sector * SECTOR_BYTES + word * 2] = (value & 0xFF) as u8;
                out[sector * SECTOR_BYTES + word * 2 + 1] = (value >> 8) as u8;
            }
        }
    }
    true
}

/// Writes `count` sectors starting at `lba` from `data`, then flushes the cache.
///
/// `data` must hold at least `count * 512` bytes.
pub fn write_sectors(index: usize, lba: u32, count: u8, data: &[u8]) -> bool {
    if count == 0 || data.len() < usize::from(count) * SECTOR_BYTES || !present(index) {
        return false;
    }
    if u64::from(lba) + u64::from(count) > u64::from(sectors(index)) {
        return false;
    }

    select(index, lba, count);
    // SAFETY: the ports belong to the primary channel and `data` is large enough.
    unsafe {
        let mut status = Port::<u8>::new(PRIMARY_IO + REG_STATUS);
        let mut port = Port::<u16>::new(PRIMARY_IO + REG_DATA);
        Port::<u8>::new(PRIMARY_IO + REG_COMMAND).write(CMD_WRITE);
        for sector in 0..usize::from(count) {
            if !wait_drq(&mut status) {
                return false;
            }
            for word in 0..256 {
                let low = u16::from(data[sector * SECTOR_BYTES + word * 2]);
                let high = u16::from(data[sector * SECTOR_BYTES + word * 2 + 1]);
                port.write(low | (high << 8));
            }
        }
        Port::<u8>::new(PRIMARY_IO + REG_COMMAND).write(CMD_FLUSH);
        wait_not_busy(&mut status);
    }
    true
}

/// Flushes the drive's write cache.
pub fn flush(index: usize) -> bool {
    if !present(index) {
        return false;
    }
    // SAFETY: the ports belong to the primary channel.
    unsafe {
        let mut status = Port::<u8>::new(PRIMARY_IO + REG_STATUS);
        Port::<u8>::new(PRIMARY_IO + REG_COMMAND).write(CMD_FLUSH);
        wait_not_busy(&mut status);
    }
    true
}
