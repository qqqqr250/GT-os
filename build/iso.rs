//! A tiny ISO9660 + El Torito image writer.
//!
//! The `bootloader` crate can turn the kernel into BIOS and UEFI *disk* images,
//! but it cannot produce an optical disc image. Instead of shelling out to
//! `xorriso`/`mkisofs` (which may not be installed), this module writes the
//! handful of ISO9660 structures an El Torito bootable disc needs:
//!
//! * the Primary Volume Descriptor and the El Torito Boot Record Volume
//!   Descriptor,
//! * the two path tables (little- and big-endian),
//! * a minimal directory tree (`/EFI/BOOT/BOOTX64.EFI`),
//! * the El Torito boot catalog with a BIOS default entry and a UEFI section.
//!
//! The BIOS boot image is the MBR image that `bootloader` writes; the UEFI boot
//! image is the FAT partition containing `efi/boot/bootx64.efi`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// ISO9660 and El Torito both count in 2048-byte logical blocks. The same value
/// is needed as a `usize` (buffer sizes), a `u32` (record lengths) and a `u16`
/// (the logical block size field), so each width gets a name instead of a cast.
const SECTOR: usize = 2048;
const SECTOR_U32: u32 = 2048;
const SECTOR_U16: u16 = 2048;

/// Fixed layout of the metadata area, in 2048-byte sectors. Keeping it fixed
/// avoids a second pass over the descriptors.
const PVD_LBA: u32 = 16;
const BOOT_RECORD_LBA: u32 = 17;
const TERMINATOR_LBA: u32 = 18;
const BOOT_CATALOG_LBA: u32 = 19;
const L_PATH_TABLE_LBA: u32 = 20;
const M_PATH_TABLE_LBA: u32 = 21;
const ROOT_DIR_LBA: u32 = 22;
const EFI_DIR_LBA: u32 = 23;
const BOOT_DIR_LBA: u32 = 24;
/// The UEFI FAT image doubles as the `/EFI/BOOT/BOOTX64.EFI` file.
const BOOT_FILE_LBA: u32 = 25;

const VOLUME_ID: &str = "RUSTOS";

/// El Torito media types.
const MEDIA_NO_EMULATION: u8 = 0x00;
/// Hard-disk emulation: the boot image is exposed as drive 0x80.
const MEDIA_HARD_DISK: u8 = 0x04;

/// Builds a bootable ISO9660 image from the BIOS MBR image and the UEFI FAT
/// partition image, and returns the path of the ISO it wrote.
pub fn build(out_dir: &Path, bios_image: &Path, uefi_fat: &Path) -> io::Result<PathBuf> {
    let bios = fs::read(bios_image)?;
    let uefi = fs::read(uefi_fat)?;

    let bios_lba = BOOT_FILE_LBA + sectors(uefi.len());
    let total_sectors = bios_lba + sectors(bios.len());

    let mut image = vec![0u8; total_sectors as usize * SECTOR];

    // The two boot images are stored verbatim. The UEFI one comes first so it
    // can double as the `/EFI/BOOT/BOOTX64.EFI` directory entry.
    copy_into(&mut image, BOOT_FILE_LBA, &uefi);
    copy_into(&mut image, bios_lba, &bios);

    let l_path_table = path_table(false);
    let m_path_table = path_table(true);

    put(
        &mut image,
        PVD_LBA,
        &primary_volume_descriptor(
            total_sectors,
            u32::try_from(l_path_table.len()).expect("path table fits in a u32"),
        ),
    );
    put(
        &mut image,
        BOOT_RECORD_LBA,
        &boot_record_volume_descriptor(),
    );
    put(&mut image, TERMINATOR_LBA, &volume_descriptor_terminator());
    put(
        &mut image,
        BOOT_CATALOG_LBA,
        &boot_catalog(bios_lba, bios.len(), BOOT_FILE_LBA, uefi.len()),
    );
    put(&mut image, L_PATH_TABLE_LBA, &l_path_table);
    put(&mut image, M_PATH_TABLE_LBA, &m_path_table);
    put(&mut image, ROOT_DIR_LBA, &root_directory());
    put(&mut image, EFI_DIR_LBA, &efi_directory());
    put(
        &mut image,
        BOOT_DIR_LBA,
        &boot_directory(
            BOOT_FILE_LBA,
            u32::try_from(uefi.len()).expect("UEFI image fits in a u32"),
        ),
    );

    let iso_path = out_dir.join("rustos.iso");
    fs::write(&iso_path, &image)?;
    Ok(iso_path)
}

fn sectors(len: usize) -> u32 {
    u32::try_from(len.div_ceil(SECTOR)).expect("boot image fits in a u32 of sectors")
}

fn sectors_512(len: usize) -> u32 {
    u32::try_from(len.div_ceil(512)).expect("boot image fits in a u32 of 512-byte sectors")
}

fn copy_into(image: &mut [u8], lba: u32, bytes: &[u8]) {
    let offset = lba as usize * SECTOR;
    image[offset..offset + bytes.len()].copy_from_slice(bytes);
}

fn put(image: &mut [u8], lba: u32, bytes: &[u8]) {
    assert!(bytes.len() <= SECTOR, "descriptor larger than one sector");
    copy_into(image, lba, bytes);
}

/// Writes `bytes` at the start of a fresh 2048-byte sector.
fn sector_with(bytes: &[u8]) -> Vec<u8> {
    let mut sector = vec![0u8; SECTOR];
    sector[..bytes.len()].copy_from_slice(bytes);
    sector
}

/// ISO9660 stores multi-byte integers twice: little-endian then big-endian.
fn both_endian_u32(value: u32) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&value.to_le_bytes());
    out[4..].copy_from_slice(&value.to_be_bytes());
    out
}

fn both_endian_u16(value: u16) -> [u8; 4] {
    let mut out = [0u8; 4];
    out[..2].copy_from_slice(&value.to_le_bytes());
    out[2..].copy_from_slice(&value.to_be_bytes());
    out
}

/// A fixed, valid recording date/time: 2024-01-01 00:00:00 UTC.
const DIRECTORY_TIMESTAMP: [u8; 7] = [124, 1, 1, 0, 0, 0, 0];
/// The 17-byte ASCII form used by the volume descriptors.
const VOLUME_TIMESTAMP: &[u8; 16] = b"2024010100000000";

/// Builds one directory record. `name` is the raw file identifier: `[0x00]`
/// for the directory itself, `[0x01]` for its parent, otherwise the ASCII name.
fn directory_record(lba: u32, length: u32, is_directory: bool, name: &[u8]) -> Vec<u8> {
    let mut record = Vec::with_capacity(33 + name.len() + 1);
    record.push(0); // record length, patched at the end
    record.push(0); // extended attribute length
    record.extend_from_slice(&both_endian_u32(lba));
    record.extend_from_slice(&both_endian_u32(length));
    record.extend_from_slice(&DIRECTORY_TIMESTAMP);
    record.push(u8::from(is_directory) << 1);
    record.push(0); // file unit size
    record.push(0); // interleave gap
    record.extend_from_slice(&both_endian_u16(1)); // volume sequence number
    record.push(u8::try_from(name.len()).expect("file identifier fits in a byte"));
    record.extend_from_slice(name);
    if record.len() % 2 == 1 {
        record.push(0); // records are padded to an even length
    }
    record[0] = u8::try_from(record.len()).expect("directory record fits in a byte");
    record
}

/// A directory record is only valid if it does not cross a sector boundary, so
/// pad the directory out with zeros once a record would not fit.
fn push_directory_record(directory: &mut Vec<u8>, record: &[u8]) {
    let offset = directory.len() % SECTOR;
    if offset + record.len() > SECTOR {
        directory.resize(directory.len() + (SECTOR - offset), 0);
    }
    directory.extend_from_slice(record);
}

fn directory(self_lba: u32, parent_lba: u32, entries: &[(u32, u32, bool, &[u8])]) -> Vec<u8> {
    let mut directory = Vec::new();
    push_directory_record(
        &mut directory,
        &directory_record(self_lba, SECTOR_U32, true, &[0]),
    );
    push_directory_record(
        &mut directory,
        &directory_record(parent_lba, SECTOR_U32, true, &[1]),
    );
    for &(lba, length, is_dir, name) in entries {
        push_directory_record(&mut directory, &directory_record(lba, length, is_dir, name));
    }
    sector_with(&directory)
}

fn root_directory() -> Vec<u8> {
    directory(
        ROOT_DIR_LBA,
        ROOT_DIR_LBA,
        &[(EFI_DIR_LBA, SECTOR_U32, true, b"EFI")],
    )
}

fn efi_directory() -> Vec<u8> {
    directory(
        EFI_DIR_LBA,
        ROOT_DIR_LBA,
        &[(BOOT_DIR_LBA, SECTOR_U32, true, b"BOOT")],
    )
}

fn boot_directory(boot_file_lba: u32, boot_file_length: u32) -> Vec<u8> {
    directory(
        BOOT_DIR_LBA,
        EFI_DIR_LBA,
        &[(boot_file_lba, boot_file_length, false, b"BOOTX64.EFI;1")],
    )
}

/// One path table record. The root directory is entry 1, `/EFI` entry 2 and
/// `/EFI/BOOT` entry 3, in breadth-first order.
fn path_table_record(lba: u32, parent: u16, name: &[u8], big_endian: bool) -> Vec<u8> {
    let mut record = Vec::with_capacity(8 + name.len() + 1);
    record.push(u8::try_from(name.len()).expect("path identifier fits in a byte"));
    record.push(0); // extended attribute length
    if big_endian {
        record.extend_from_slice(&lba.to_be_bytes());
        record.extend_from_slice(&parent.to_be_bytes());
    } else {
        record.extend_from_slice(&lba.to_le_bytes());
        record.extend_from_slice(&parent.to_le_bytes());
    }
    record.extend_from_slice(name);
    if record.len() % 2 == 1 {
        record.push(0);
    }
    record
}

fn path_table(big_endian: bool) -> Vec<u8> {
    let mut table = Vec::new();
    table.extend_from_slice(&path_table_record(ROOT_DIR_LBA, 1, &[0], big_endian));
    table.extend_from_slice(&path_table_record(EFI_DIR_LBA, 1, b"EFI", big_endian));
    table.extend_from_slice(&path_table_record(BOOT_DIR_LBA, 2, b"BOOT", big_endian));
    sector_with(&table)
}

fn primary_volume_descriptor(total_sectors: u32, path_table_size: u32) -> Vec<u8> {
    let mut pvd = vec![0u8; SECTOR];
    pvd[0] = 1; // primary volume descriptor
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1; // version
    pvd[8..40].fill(b' '); // system identifier
    write_padded(&mut pvd[40..72], VOLUME_ID.as_bytes(), b' '); // volume identifier
    pvd[80..88].copy_from_slice(&both_endian_u32(total_sectors));
    pvd[120..124].copy_from_slice(&both_endian_u16(1)); // volume set size
    pvd[124..128].copy_from_slice(&both_endian_u16(1)); // volume sequence number
    pvd[128..132].copy_from_slice(&both_endian_u16(SECTOR_U16)); // logical block size
    pvd[132..140].copy_from_slice(&both_endian_u32(path_table_size));
    pvd[140..144].copy_from_slice(&L_PATH_TABLE_LBA.to_le_bytes());
    pvd[148..152].copy_from_slice(&M_PATH_TABLE_LBA.to_be_bytes());
    let root = directory_record(ROOT_DIR_LBA, SECTOR_U32, true, &[0]);
    pvd[156..156 + root.len()].copy_from_slice(&root);
    for range in [190..318, 318..446, 446..574, 574..702] {
        pvd[range].fill(b' '); // volume set, publisher, preparer, application ids
    }
    for range in [702..739, 739..776, 776..813] {
        pvd[range].fill(b' ');
    }
    for start in [813, 830, 847, 864] {
        pvd[start..start + VOLUME_TIMESTAMP.len()].copy_from_slice(VOLUME_TIMESTAMP);
        pvd[start + 16] = 0; // GMT offset, in 15-minute units
    }
    pvd[881] = 1; // file structure version
    pvd
}

fn boot_record_volume_descriptor() -> Vec<u8> {
    let mut descriptor = vec![0u8; SECTOR];
    descriptor[0] = 0; // boot record
    descriptor[1..6].copy_from_slice(b"CD001");
    descriptor[6] = 1; // version
    write_padded(&mut descriptor[7..39], b"EL TORITO SPECIFICATION", 0);
    descriptor[71..75].copy_from_slice(&BOOT_CATALOG_LBA.to_le_bytes());
    descriptor
}

fn volume_descriptor_terminator() -> Vec<u8> {
    let mut descriptor = vec![0u8; SECTOR];
    descriptor[0] = 0xFF;
    descriptor[1..6].copy_from_slice(b"CD001");
    descriptor[6] = 1;
    descriptor
}

fn boot_catalog(bios_lba: u32, bios_length: usize, uefi_lba: u32, uefi_length: usize) -> Vec<u8> {
    let mut catalog = vec![0u8; SECTOR];

    // Validation entry.
    catalog[0] = 0x01; // header id
    catalog[1] = 0x00; // platform id: 80x86
    write_padded(&mut catalog[4..28], b"RustOS", 0);
    catalog[30] = 0x55;
    catalog[31] = 0xAA;
    let checksum = validation_checksum(&catalog[..32]);
    catalog[28..30].copy_from_slice(&checksum.to_le_bytes());

    // Initial/default entry: the BIOS disk image, using hard-disk emulation so
    // that the guest sees it as a normal 512-byte-sector drive.
    // The El Torito boot-catalog Sector Count is always counted in 512-byte
    // virtual sectors, even for a hard-disk-emulation entry (the emulated drive
    // exposes 512-byte sectors), so this uses `sectors_512`, not `sectors`.
    write_boot_entry(
        &mut catalog[32..64],
        MEDIA_HARD_DISK,
        bios_lba,
        sectors_512(bios_length),
    );

    // Section header introducing the UEFI entry.
    catalog[64] = 0x91;
    catalog[65] = 0xEF; // platform id: EFI
    catalog[66..68].copy_from_slice(&1u16.to_le_bytes());
    write_padded(&mut catalog[68..96], b"RustOS UEFI", 0);

    // UEFI entry: the FAT image, loaded without emulation. In no-emulation mode
    // the El Torito sector count is in 512-byte (virtual) sectors.
    write_boot_entry(
        &mut catalog[96..128],
        MEDIA_NO_EMULATION,
        uefi_lba,
        sectors_512(uefi_length),
    );

    catalog
}

fn write_boot_entry(entry: &mut [u8], media: u8, lba: u32, sector_count: u32) {
    entry[0] = 0x88; // bootable
    entry[1] = media;
    // Load segment 0 means "use the default", i.e. 0x07C0 for no emulation.
    let count = u16::try_from(sector_count.min(u32::from(u16::MAX))).expect("clamped");
    entry[6..8].copy_from_slice(&count.to_le_bytes());
    entry[8..12].copy_from_slice(&lba.to_le_bytes());
}

/// The El Torito validation entry checksum makes the sum of its 16-bit words
/// zero.
fn validation_checksum(entry: &[u8]) -> u16 {
    let mut sum: u16 = 0;
    for word in entry.chunks_exact(2) {
        sum = sum.wrapping_add(u16::from_le_bytes([word[0], word[1]]));
    }
    0u16.wrapping_sub(sum)
}

fn write_padded(target: &mut [u8], text: &[u8], pad: u8) {
    target.fill(pad);
    target[..text.len()].copy_from_slice(text);
}
