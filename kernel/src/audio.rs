//! AC'97 audio output.
//!
//! Drives QEMU's emulated Intel 82801AA AC'97 controller (PCI `8086:2415`).
//! The mixer registers live in BAR0 (NAM) and the bus-master registers in BAR1
//! (NABM). Playback goes through the PCM-out DMA engine with a small buffer
//! descriptor list; the transfer is polled rather than interrupt-driven so the
//! whole driver stays single-threaded, matching the rest of the kernel.
//!
//! `-device AC97,audiodev=snd0` with `-audiodev wav,id=snd0` in QEMU writes
//! everything the guest plays to a `.wav` file, which is how the driver is
//! verified.

use alloc::vec::Vec;

use spin::Mutex;
use x86_64::instructions::port::Port;

use crate::pci;

const VENDOR: u16 = 0x8086;
const DEVICE: u16 = 0x2415;

// NAM (mixer) registers.
const NAM_RESET: u16 = 0x00;
const NAM_MASTER_VOLUME: u16 = 0x02;
const NAM_PCM_OUT_VOLUME: u16 = 0x18;
const NAM_PCM_FRONT_RATE: u16 = 0x2c;

// NABM (bus master) registers.
const NABM_PO_BDBAR: u16 = 0x10;
const NABM_PO_LVI: u16 = 0x15;
const NABM_PO_SR: u16 = 0x16;
const NABM_PO_CR: u16 = 0x1b;
const NABM_GLOB_CNT: u16 = 0x2c;
const NABM_GLOB_STA: u16 = 0x30;

const PO_CR_RPBM: u8 = 0x01; // run / pause bus master
const PO_CR_RR: u8 = 0x02; // reset registers
const PO_SR_DCH: u16 = 0x0001; // DMA controller halted

/// Buffer descriptor list: 32 entries of 16 bytes.
const DESCRIPTORS: usize = 32;
/// Largest single descriptor, in bytes. Must be a multiple of the 4-byte frame
/// size and below 0xFFFE.
const DESCRIPTOR_BYTES: usize = 0x3ff8;

/// Native sample rate used for generated tones.
pub const SAMPLE_RATE: u16 = 48_000;

/// The hard cap on one playback, in bytes (~5 s of 16-bit stereo at 48 kHz).
const MAX_PLAYBACK: usize = DESCRIPTORS * DESCRIPTOR_BYTES;

struct Ac97 {
    mixer: u16,
    bus: u16,
    /// `physical_memory_offset`; the DMA buffers are heap allocations, so their
    /// physical address is `virtual - offset`.
    offset: u64,
    /// The descriptor list, as 64-bit words so it is 8-byte aligned.
    bdl: Vec<u64>,
    /// The sample staging buffer, reused across plays and grown as needed.
    buffer: Vec<u64>,
}

static AUDIO: Mutex<Option<Ac97>> = Mutex::new(None);

unsafe fn outb(port: u16, value: u8) {
    unsafe { Port::<u8>::new(port).write(value) };
}

unsafe fn outw(port: u16, value: u16) {
    unsafe { Port::<u16>::new(port).write(value) };
}

unsafe fn outl(port: u16, value: u32) {
    unsafe { Port::<u32>::new(port).write(value) };
}

unsafe fn inb(port: u16) -> u8 {
    unsafe { Port::<u8>::new(port).read() }
}

unsafe fn inw(port: u16) -> u16 {
    unsafe { Port::<u16>::new(port).read() }
}

unsafe fn inl(port: u16) -> u32 {
    unsafe { Port::<u32>::new(port).read() }
}

/// Brings up the AC'97 controller. Returns `false` when no such device exists.
pub fn init(physical_memory_offset: u64) -> bool {
    let Some(device) = pci::find(VENDOR, DEVICE) else {
        return false;
    };
    pci::enable(&device);

    let mixer = (device.bar0 & 0xFFFC) as u16;
    let bus = (device.bar1 & 0xFFFC) as u16;
    if mixer == 0 || bus == 0 {
        return false;
    }

    // SAFETY: BAR0/BAR1 are the device's own I/O windows, mapped by the
    // firmware for this function; the accesses below only touch them.
    unsafe {
        // Cold reset the AC'97 link, then wait for the codec to report ready.
        outl(bus + NABM_GLOB_CNT, 0x0000_0002);
        for _ in 0..500_000 {
            if inl(bus + NABM_GLOB_STA) & 0x0000_0100 != 0 {
                break;
            }
            core::hint::spin_loop();
        }

        // Reset the mixer and wait for it to come back.
        outw(mixer + NAM_RESET, 0x0000);
        for _ in 0..500_000 {
            if inw(mixer + NAM_RESET) & 0x0001 == 0 {
                break;
            }
            core::hint::spin_loop();
        }

        // Unmute the master and PCM-out paths at 0 dB, and set the native rate.
        outw(mixer + NAM_MASTER_VOLUME, 0x0000);
        outw(mixer + NAM_PCM_OUT_VOLUME, 0x0808);
        outw(mixer + NAM_PCM_FRONT_RATE, SAMPLE_RATE);

        // Reset the PCM-out DMA engine so it starts from a clean state.
        outb(bus + NABM_PO_CR, PO_CR_RR);
        for _ in 0..100_000 {
            if inb(bus + NABM_PO_CR) & PO_CR_RR == 0 {
                break;
            }
            core::hint::spin_loop();
        }
    }

    *AUDIO.lock() = Some(Ac97 {
        mixer,
        bus,
        offset: physical_memory_offset,
        bdl: alloc::vec![0u64; DESCRIPTORS * 2],
        buffer: Vec::new(),
    });
    true
}

/// Whether an AC'97 controller is present and initialized.
pub fn is_up() -> bool {
    AUDIO.lock().is_some()
}

/// A short attention beep.
pub fn beep() -> bool {
    tone(880, 160)
}

/// Plays a square-ish tone of `frequency` Hz for `milliseconds`.
pub fn tone(frequency: u32, milliseconds: u32) -> bool {
    let rate = u32::from(SAMPLE_RATE);
    let frames = (rate * milliseconds / 1000).max(1);
    let period = (rate / frequency.max(1)).max(2);
    let half = (period / 2).max(1);

    let mut samples = Vec::with_capacity(frames as usize * 2);
    for frame in 0..frames {
        // Triangle wave: a softer edge than a square wave, and cheap to
        // generate without a sine table.
        let phase = frame % period;
        let value = if phase < half {
            let t = phase as i32 * 2 * 7000 / half as i32;
            t - 7000
        } else {
            let t = (phase - half) as i32 * 2 * 7000 / half as i32;
            7000 - t
        };
        samples.push(value as i16);
        samples.push(value as i16);
    }
    play(&samples, SAMPLE_RATE)
}

/// Plays 16-bit signed little-endian stereo samples at `rate` Hz.
pub fn play(samples: &[i16], rate: u16) -> bool {
    let mut guard = AUDIO.lock();
    let Some(audio) = guard.as_mut() else {
        return false;
    };
    audio.play(samples, rate)
}

impl Ac97 {
    #[allow(clippy::cast_possible_truncation)]
    fn play(&mut self, samples: &[i16], rate: u16) -> bool {
        let frames = samples.len().min(MAX_PLAYBACK / 2);
        if frames == 0 {
            return false;
        }
        let bytes = frames * 2;

        // Copy the samples into the staging buffer (little-endian).
        let words = bytes.div_ceil(8);
        if self.buffer.len() < words {
            self.buffer.resize(words, 0);
        }
        // SAFETY: `buffer` holds `len() * 8` valid bytes and the loop below
        // stays inside that range.
        let staging = unsafe {
            core::slice::from_raw_parts_mut(
                self.buffer.as_mut_ptr().cast::<u8>(),
                self.buffer.len() * 8,
            )
        };
        for (i, sample) in samples.iter().take(frames).enumerate() {
            let pair = sample.to_le_bytes();
            staging[i * 2] = pair[0];
            staging[i * 2 + 1] = pair[1];
        }

        // Build the descriptor list. Each entry points at part of the buffer.
        let buffer_physical = (self.buffer.as_ptr() as u64).wrapping_sub(self.offset);
        // SAFETY: `bdl` is 16 * DESCRIPTORS bytes long (as 64-bit words).
        let bdl = unsafe {
            core::slice::from_raw_parts_mut(self.bdl.as_mut_ptr().cast::<u8>(), self.bdl.len() * 8)
        };
        let mut offset = 0usize;
        let mut count = 0usize;
        while offset < bytes && count < DESCRIPTORS {
            let length = (bytes - offset).min(DESCRIPTOR_BYTES) & !3;
            if length == 0 {
                break;
            }
            let entry = &mut bdl[count * 16..count * 16 + 16];
            entry[0..4].copy_from_slice(&((buffer_physical + offset as u64) as u32).to_le_bytes());
            entry[4..6].copy_from_slice(&(length as u16).to_le_bytes());
            entry[6..8].copy_from_slice(&0u16.to_le_bytes());
            offset += length;
            count += 1;
        }
        if count == 0 {
            return false;
        }

        let bdl_physical = (self.bdl.as_ptr() as u64).wrapping_sub(self.offset) as u32;
        let bus = self.bus;
        let mixer = self.mixer;

        // SAFETY: NABM/NAM are the controller's I/O windows.
        unsafe {
            // Stop and reset the engine before reprogramming it.
            outb(bus + NABM_PO_CR, PO_CR_RR);
            for _ in 0..100_000 {
                if inb(bus + NABM_PO_CR) & PO_CR_RR == 0 {
                    break;
                }
                core::hint::spin_loop();
            }

            outl(bus + NABM_PO_BDBAR, bdl_physical);
            outb(bus + NABM_PO_LVI, (count - 1) as u8);
            outw(mixer + NAM_PCM_FRONT_RATE, rate);
            outw(bus + NABM_PO_SR, 0x001c); // clear any pending status
            outb(bus + NABM_PO_CR, PO_CR_RPBM);
        }

        // Poll until the engine reports that it has halted.
        for _ in 0..20_000_000 {
            // SAFETY: as above.
            if unsafe { inw(bus + NABM_PO_SR) } & PO_SR_DCH != 0 {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }
}

/// Plays a RIFF/WAVE file of 16-bit PCM (mono or stereo).
///
/// Returns `false` for anything that is not a supported PCM WAV.
#[allow(dead_code)] // used by the disk `play` command
pub fn play_wav(data: &[u8]) -> bool {
    if data.len() < 44 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return false;
    }

    let mut position = 12usize;
    let mut channels = 0u16;
    let mut rate = SAMPLE_RATE;
    let mut bits = 0u16;
    while position + 8 <= data.len() {
        let id = &data[position..position + 4];
        let size = u32::from_le_bytes([
            data[position + 4],
            data[position + 5],
            data[position + 6],
            data[position + 7],
        ]) as usize;
        let body = position + 8;
        if body + size > data.len() {
            break;
        }
        if id == b"fmt " && size >= 16 {
            channels = u16::from_le_bytes([data[body + 2], data[body + 3]]);
            rate = u16::from_le_bytes([data[body + 4], data[body + 5]]);
            bits = u16::from_le_bytes([data[body + 14], data[body + 15]]);
        } else if id == b"data" {
            if bits != 16 || channels == 0 || channels > 2 {
                return false;
            }
            let mut samples = Vec::with_capacity(size / 2);
            for chunk in data[body..body + size].chunks_exact(2) {
                samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
            }
            if channels == 1 {
                // Duplicate the mono channel to both speakers.
                let mut stereo = Vec::with_capacity(samples.len() * 2);
                for sample in samples {
                    stereo.push(sample);
                    stereo.push(sample);
                }
                samples = stereo;
            }
            return play(&samples, rate);
        }
        // WAV chunks are word-aligned.
        position = body + size + (size & 1);
    }
    false
}
