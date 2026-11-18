//! Minimal PCI configuration-space access.
//!
//! Just enough to find and enable the emulated network adapters used by the
//! test environments: QEMU's RTL8139 (I/O BAR) and `VirtualBox`'s Intel E1000
//! (MMIO BAR). Only bus 0 is scanned: both hypervisors place these devices
//! there, which keeps the scan short.

use x86_64::instructions::port::Port;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// Reads a 32-bit word from PCI configuration space.
fn read_u32(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    let address = 0x8000_0000
        | (u32::from(bus) << 16)
        | (u32::from(slot) << 11)
        | (u32::from(func) << 8)
        | u32::from(offset & 0xFC);
    // SAFETY: 0xCF8/0xCFC are the architectural PCI configuration ports.
    unsafe {
        Port::<u32>::new(CONFIG_ADDRESS).write(address);
        Port::<u32>::new(CONFIG_DATA).read()
    }
}

/// Writes a 32-bit word to PCI configuration space.
fn write_u32(bus: u8, slot: u8, func: u8, offset: u8, value: u32) {
    let address = 0x8000_0000
        | (u32::from(bus) << 16)
        | (u32::from(slot) << 11)
        | (u32::from(func) << 8)
        | u32::from(offset & 0xFC);
    // SAFETY: as above.
    unsafe {
        Port::<u32>::new(CONFIG_ADDRESS).write(address);
        Port::<u32>::new(CONFIG_DATA).write(value);
    }
}

/// A discovered PCI function.
///
/// `vendor`, `id`, `class` and `subclass` are kept for diagnostics even though
/// the driver only needs the BAR and the IRQ line.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub struct Device {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    pub vendor: u16,
    pub id: u16,
    pub class: u8,
    pub subclass: u8,
    pub bar0: u32,
    pub bar1: u32,
    pub irq: u8,
}

/// Scans every function on bus 0 and calls `f` for each present device.
pub fn scan<F: FnMut(Device)>(mut f: F) {
    for slot in 0..32u8 {
        let header = read_u32(0, slot, 0, 0x0C);
        let functions = if header & 0x0080_0000 != 0 { 8 } else { 1 };
        for func in 0..functions {
            let ids = read_u32(0, slot, func, 0x00);
            let found_vendor = (ids & 0xFFFF) as u16;
            if found_vendor == 0xFFFF || found_vendor == 0 {
                continue;
            }
            let class_revision = read_u32(0, slot, func, 0x08);
            f(Device {
                bus: 0,
                slot,
                func,
                vendor: found_vendor,
                id: (ids >> 16) as u16,
                class: (class_revision >> 24) as u8,
                subclass: (class_revision >> 16) as u8,
                bar0: read_u32(0, slot, func, 0x10),
                bar1: read_u32(0, slot, func, 0x14),
                irq: read_u32(0, slot, func, 0x3C) as u8,
            });
        }
    }
}

/// Finds the first function on bus 0 with the given vendor and device IDs.
pub fn find(vendor: u16, id: u16) -> Option<Device> {
    for slot in 0..32u8 {
        let header = read_u32(0, slot, 0, 0x0C);
        let functions = if header & 0x0080_0000 != 0 { 8 } else { 1 };
        for func in 0..functions {
            let ids = read_u32(0, slot, func, 0x00);
            let found_vendor = (ids & 0xFFFF) as u16;
            if found_vendor != vendor {
                continue;
            }
            let found_id = (ids >> 16) as u16;
            if found_id != id {
                continue;
            }
            let class_revision = read_u32(0, slot, func, 0x08);
            return Some(Device {
                bus: 0,
                slot,
                func,
                vendor: found_vendor,
                id: found_id,
                class: (class_revision >> 24) as u8,
                subclass: (class_revision >> 16) as u8,
                bar0: read_u32(0, slot, func, 0x10),
                bar1: read_u32(0, slot, func, 0x14),
                irq: read_u32(0, slot, func, 0x3C) as u8,
            });
        }
    }
    None
}

/// Enables I/O space, memory space and bus mastering for `device`.
pub fn enable(device: &Device) {
    let command = read_u32(device.bus, device.slot, device.func, 0x04);
    // bit 0 = I/O space, bit 1 = memory space, bit 2 = bus master.
    // The E1000 needs memory space for its MMIO BAR; the RTL8139 needs I/O.
    write_u32(device.bus, device.slot, device.func, 0x04, command | 0x0007);
}
