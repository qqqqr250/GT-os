//! Writes the small FAT16 data disk image that the kernel mounts at boot.
//!
//! The geometry mirrors what the kernel's `fat::format` produces, so a volume
//! formatted from inside `GTos` and one built here are indistinguishable. The
//! image is seeded with a readme and a WAV file so `ls`, `cat` and `play` have
//! something to work with before the first write.
//!
//! On-disk fields have fixed widths (a boot sector stores `u16`s, a directory
//! entry stores `u32`s), so each width gets its own named constant instead of a
//! cast, exactly like `build/iso.rs`.

/// 16 MiB, the size of the data disk.
const TOTAL_SECTORS: u32 = 32 * 1024;
const TOTAL_SECTORS_U16: u16 = 32 * 1024;
const SECTOR: usize = 512;
const SECTOR_U16: u16 = 512;
const SECTOR_U32: u32 = 512;
const SECTORS_PER_CLUSTER: u32 = 4;
const SECTORS_PER_CLUSTER_U8: u8 = 4;
const RESERVED: u32 = 1;
const RESERVED_U16: u16 = 1;
const NUM_FATS: u32 = 2;
const NUM_FATS_U8: u8 = 2;
const ROOT_ENTRIES: u32 = 512;
const ROOT_ENTRIES_U16: u16 = 512;
const FAT_SIZE: u32 = 32;
const FAT_SIZE_U16: u16 = 32;
const FIRST_CLUSTER: u16 = 2;

const ROOT_SECTORS: u32 = (ROOT_ENTRIES * 32).div_ceil(SECTOR_U32);
const ROOT_START: u32 = RESERVED + NUM_FATS * FAT_SIZE;
const DATA_START: u32 = ROOT_START + ROOT_SECTORS;
const CLUSTER_BYTES: usize = SECTORS_PER_CLUSTER as usize * SECTOR;

/// Builds a complete FAT16 image holding `files`.
pub fn build(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut image = vec![0u8; TOTAL_SECTORS as usize * SECTOR];
    write_boot_sector(&mut image);

    let mut fat = vec![0u16; 256 * FAT_SIZE as usize];
    fat[0] = 0xFFF8;
    fat[1] = 0xFFFF;

    let mut directory = vec![0u8; ROOT_ENTRIES as usize * 32];
    let mut next_free = FIRST_CLUSTER;

    for (slot, (name, data)) in files.iter().enumerate() {
        let clusters = data.len().div_ceil(CLUSTER_BYTES);
        let first = if clusters == 0 { 0 } else { next_free };
        for index in 0..clusters {
            let cluster = next_free + u16::try_from(index).expect("cluster index fits in u16");
            fat[usize::from(cluster)] = if index + 1 < clusters {
                cluster + 1
            } else {
                0xFFFF
            };
            let lba = DATA_START + u32::from(cluster - FIRST_CLUSTER) * SECTORS_PER_CLUSTER;
            let offset = lba as usize * SECTOR;
            let start = index * CLUSTER_BYTES;
            let end = (start + CLUSTER_BYTES).min(data.len());
            image[offset..offset + (end - start)].copy_from_slice(&data[start..end]);
        }
        next_free += u16::try_from(clusters).expect("cluster count fits in u16");

        let entry = &mut directory[slot * 32..slot * 32 + 32];
        write_directory_entry(
            entry,
            name,
            first,
            u32::try_from(data.len()).expect("file size fits in u32"),
        );
    }

    let root_offset = ROOT_START as usize * SECTOR;
    image[root_offset..root_offset + directory.len()].copy_from_slice(&directory);

    // Mirror the FAT to both copies.
    for (index, value) in fat.iter().enumerate() {
        let bytes = value.to_le_bytes();
        for copy in 0..NUM_FATS {
            let offset =
                (RESERVED as usize + copy as usize * FAT_SIZE as usize) * SECTOR + index * 2;
            image[offset..offset + 2].copy_from_slice(&bytes);
        }
    }

    image
}

fn write_boot_sector(image: &mut [u8]) {
    let boot = &mut image[0..SECTOR];
    boot[0] = 0xEB;
    boot[1] = 0x3C;
    boot[2] = 0x90;
    boot[3..11].copy_from_slice(b"GTOS    ");
    boot[11..13].copy_from_slice(&SECTOR_U16.to_le_bytes());
    boot[13] = SECTORS_PER_CLUSTER_U8;
    boot[14..16].copy_from_slice(&RESERVED_U16.to_le_bytes());
    boot[16] = NUM_FATS_U8;
    boot[17..19].copy_from_slice(&ROOT_ENTRIES_U16.to_le_bytes());
    boot[19..21].copy_from_slice(&TOTAL_SECTORS_U16.to_le_bytes());
    boot[21] = 0xF8;
    boot[22..24].copy_from_slice(&FAT_SIZE_U16.to_le_bytes());
    boot[24..26].copy_from_slice(&32u16.to_le_bytes());
    boot[26..28].copy_from_slice(&64u16.to_le_bytes());
    boot[36] = 0x80;
    boot[38] = 0x29;
    boot[39..43].copy_from_slice(&0x5255_5354u32.to_le_bytes());
    boot[43..54].copy_from_slice(b"GTOS       ");
    boot[54..62].copy_from_slice(b"FAT16   ");
    boot[510] = 0x55;
    boot[511] = 0xAA;
}

fn write_directory_entry(entry: &mut [u8], name: &str, cluster: u16, size: u32) {
    entry.fill(0);
    entry[0..11].copy_from_slice(&short_name(name));
    entry[11] = 0x20; // archive
    entry[26..28].copy_from_slice(&cluster.to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
}

fn short_name(name: &str) -> [u8; 11] {
    let mut out = [b' '; 11];
    let upper = name.to_ascii_uppercase();
    let (base, extension) = match upper.split_once('.') {
        Some((base, extension)) => (base, extension),
        None => (upper.as_str(), ""),
    };
    for (index, byte) in base.bytes().take(8).enumerate() {
        out[index] = byte;
    }
    for (index, byte) in extension.bytes().take(3).enumerate() {
        out[8 + index] = byte;
    }
    out
}

/// Generates a 1-second 440 Hz stereo 16-bit WAV at 48 kHz.
///
/// 48 kHz is the AC'97 native rate, so the kernel can play it without enabling
/// variable-rate audio.
pub fn tone_wav() -> Vec<u8> {
    const RATE: u32 = 48_000;
    const SECONDS: u32 = 1;
    const FREQUENCY: f64 = 440.0;
    let frames = RATE * SECONDS;
    let mut samples = Vec::with_capacity(frames as usize * 4);

    for frame in 0..frames {
        let phase = std::f64::consts::TAU * FREQUENCY * f64::from(frame) / f64::from(RATE);
        // Fade the first and last 50 ms so the tone does not click.
        let fade = (f64::from(frame) / (f64::from(RATE) * 0.05))
            .min(1.0)
            .min((f64::from(frames - frame) / (f64::from(RATE) * 0.05)).min(1.0));
        // The scaled sine is bounded by 0.4 * i16::MAX, so the cast can never
        // overflow; the fractional part is intentionally discarded.
        #[allow(clippy::cast_possible_truncation)]
        let value = (phase.sin() * 0.4 * fade * f64::from(i16::MAX)) as i16;
        samples.extend_from_slice(&value.to_le_bytes());
        samples.extend_from_slice(&value.to_le_bytes());
    }

    let data_len = u32::try_from(samples.len()).expect("WAV data fits in u32");
    let mut wav = Vec::with_capacity(44 + samples.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&2u16.to_le_bytes()); // stereo
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 4).to_le_bytes()); // byte rate
    wav.extend_from_slice(&4u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(&samples);
    wav
}

/// `USER_BASE` from the kernel's `usermode` module. A flat user binary is linked
/// against this address, so the two constants must agree.
const USER_BASE: u64 = 0x0000_0080_0000_0000;

/// A hand-assembled ring-3 program that writes a line and exits, stored on the
/// data disk as `HELLO.BIN`.
///
/// The layout matches the kernel's built-in `demo_program`: the entry point is
/// `USER_BASE`, the message sits 36 bytes in, and the only system calls used are
/// `write(1, ...)` (number 1) and `exit(0)` (number 0) through `int 0x80`.
pub fn hello_bin() -> Vec<u8> {
    const MESSAGE: &[u8] = b"Hello from HELLO.BIN running in ring 3!\r\n";
    const MESSAGE_OFFSET: usize = 36;

    let mut program = Vec::with_capacity(MESSAGE_OFFSET + MESSAGE.len());
    program.extend_from_slice(&[0xb8, 0x01, 0x00, 0x00, 0x00]); // mov eax, 1
    program.extend_from_slice(&[0xbf, 0x01, 0x00, 0x00, 0x00]); // mov edi, 1
    program.push(0x48); // movabs rsi, message
    program.push(0xbe);
    program.extend_from_slice(&(USER_BASE + MESSAGE_OFFSET as u64).to_le_bytes());
    program.push(0xba); // mov edx, len
    program.extend_from_slice(
        &u32::try_from(MESSAGE.len())
            .expect("message fits")
            .to_le_bytes(),
    );
    program.extend_from_slice(&[0xcd, 0x80]); // int 0x80
    program.extend_from_slice(&[0xb8, 0x00, 0x00, 0x00, 0x00]); // mov eax, 0
    program.extend_from_slice(&[0x31, 0xff]); // xor edi, edi
    program.extend_from_slice(&[0xcd, 0x80]); // int 0x80
    assert_eq!(program.len(), MESSAGE_OFFSET);
    program.extend_from_slice(MESSAGE);
    program
}
