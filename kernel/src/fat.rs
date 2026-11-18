//! A small FAT16 filesystem: mount, list, read, write, delete and format.
//!
//! Only the root directory is handled and only short (8.3) file names, which is
//! all the data disk needs. Every cluster is addressed through the first FAT
//! copy and both copies are kept in step on writes, so the volume stays valid
//! for other FAT implementations.
//!
//! The on-disk structures follow the FAT16 specification: a boot sector, the
//! reserved sectors, one or more FATs, a fixed-size root directory and then the
//! data region, where cluster 2 is the first cluster.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use spin::Mutex;

use crate::ata;

const SECTOR: usize = ata::SECTOR_BYTES;
const DIR_ENTRY: usize = 32;

const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_LONG_NAME: u8 = 0x0F;
const ATTR_ARCHIVE: u8 = 0x20;

/// End-of-chain marker stored in the last FAT entry of a file.
const FAT_END: u16 = 0xFFFF;

/// The first usable cluster.
const FIRST_CLUSTER: u16 = 2;

/// A mounted FAT16 volume.
pub struct Volume {
    pub drive: usize,
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved: u16,
    pub fats: u8,
    pub root_entries: u16,
    pub fat_size: u16,
    pub total_sectors: u32,
    pub root_start: u32,
    pub root_sectors: u32,
    pub data_start: u32,
    pub cluster_count: u32,
    pub oem: String,
    pub label: String,
}

/// One directory entry.
pub struct Entry {
    pub name: String,
    pub size: u32,
    pub cluster: u16,
    pub attributes: u8,
}

impl Entry {
    /// Whether the entry is a subdirectory.
    pub fn is_directory(&self) -> bool {
        self.attributes & ATTR_DIRECTORY != 0
    }
}

/// The volume mounted at boot, if any.
static MOUNTED: Mutex<Option<Volume>> = Mutex::new(None);

/// Mounts `drive` and remembers the volume for the shell commands.
///
/// Returns `false` when the drive is absent or does not hold a FAT16 volume.
pub fn mount_data_disk(drive: usize) -> bool {
    match mount(drive) {
        Some(volume) => {
            *MOUNTED.lock() = Some(volume);
            true
        }
        None => false,
    }
}

/// Runs `f` against the mounted volume, or returns `None` when nothing is
/// mounted.
pub fn with_volume<R>(f: impl FnOnce(&Volume) -> R) -> Option<R> {
    MOUNTED.lock().as_ref().map(f)
}

/// Reads the boot sector of `drive` and validates it as FAT16.
pub fn mount(drive: usize) -> Option<Volume> {
    let mut sector = [0u8; SECTOR];
    if !ata::read_sectors(drive, 0, 1, &mut sector) {
        return None;
    }
    if sector[510] != 0x55 || sector[511] != 0xAA {
        return None;
    }

    let bytes_per_sector = u16::from_le_bytes([sector[11], sector[12]]);
    if usize::from(bytes_per_sector) != SECTOR {
        return None;
    }
    let sectors_per_cluster = sector[13];
    let reserved = u16::from_le_bytes([sector[14], sector[15]]);
    let fats = sector[16];
    let root_entries = u16::from_le_bytes([sector[17], sector[18]]);
    let fat_size = u16::from_le_bytes([sector[22], sector[23]]);
    if sectors_per_cluster == 0 || reserved == 0 || fats == 0 || root_entries == 0 || fat_size == 0
    {
        return None;
    }

    let total_sectors = match u16::from_le_bytes([sector[19], sector[20]]) {
        0 => u32::from_le_bytes([sector[32], sector[33], sector[34], sector[35]]),
        count => u32::from(count),
    };

    let root_start = u32::from(reserved) + u32::from(fats) * u32::from(fat_size);
    let root_sectors =
        (u32::from(root_entries) * DIR_ENTRY as u32).div_ceil(u32::from(bytes_per_sector));
    let data_start = root_start + root_sectors;
    if total_sectors <= data_start {
        return None;
    }
    let cluster_count = (total_sectors - data_start) / u32::from(sectors_per_cluster);

    // FAT16 has between 4085 and 65524 clusters; anything else is FAT12/FAT32.
    if !(4085..65525).contains(&cluster_count) {
        return None;
    }

    let oem = String::from_utf8_lossy(&sector[3..11]).trim().into();
    let label = if sector[38] == 0x29 {
        String::from_utf8_lossy(&sector[43..54]).trim().into()
    } else {
        String::new()
    };

    Some(Volume {
        drive,
        bytes_per_sector,
        sectors_per_cluster,
        reserved,
        fats,
        root_entries,
        fat_size,
        total_sectors,
        root_start,
        root_sectors,
        data_start,
        cluster_count,
        oem,
        label,
    })
}

// ---------------------------------------------------------------------------
// Layout helpers
// ---------------------------------------------------------------------------

impl Volume {
    fn cluster_bytes(&self) -> usize {
        usize::from(self.sectors_per_cluster) * SECTOR
    }

    fn first_sector_of_cluster(&self, cluster: u16) -> u32 {
        self.data_start
            + (u32::from(cluster) - u32::from(FIRST_CLUSTER)) * u32::from(self.sectors_per_cluster)
    }

    fn fat_start(&self) -> u32 {
        u32::from(self.reserved)
    }

    fn read_fat(&self, cluster: u16) -> Option<u16> {
        let offset = u32::from(cluster) * 2;
        let mut sector = [0u8; SECTOR];
        if !ata::read_sectors(
            self.drive,
            self.fat_start() + offset / SECTOR as u32,
            1,
            &mut sector,
        ) {
            return None;
        }
        let index = (offset % SECTOR as u32) as usize;
        Some(u16::from_le_bytes([sector[index], sector[index + 1]]))
    }

    fn write_fat(&self, cluster: u16, value: u16) -> bool {
        let offset = u32::from(cluster) * 2;
        let sector_index = offset / SECTOR as u32;
        let mut sector = [0u8; SECTOR];
        if !ata::read_sectors(self.drive, self.fat_start() + sector_index, 1, &mut sector) {
            return false;
        }
        let index = (offset % SECTOR as u32) as usize;
        sector[index..index + 2].copy_from_slice(&value.to_le_bytes());
        // Both FAT copies must agree.
        for copy in 0..u32::from(self.fats) {
            let lba = self.fat_start() + copy * u32::from(self.fat_size) + sector_index;
            if !ata::write_sectors(self.drive, lba, 1, &sector) {
                return false;
            }
        }
        true
    }

    fn read_root_sector(&self, index: u32) -> Option<[u8; SECTOR]> {
        let mut sector = [0u8; SECTOR];
        if !ata::read_sectors(self.drive, self.root_start + index, 1, &mut sector) {
            return None;
        }
        Some(sector)
    }

    fn write_root_sector(&self, index: u32, sector: &[u8; SECTOR]) -> bool {
        ata::write_sectors(self.drive, self.root_start + index, 1, sector)
    }

    /// Frees a whole cluster chain.
    fn free_chain(&self, mut cluster: u16) -> bool {
        while (FIRST_CLUSTER..0xFFF8).contains(&cluster) {
            let Some(next) = self.read_fat(cluster) else {
                return false;
            };
            if !self.write_fat(cluster, 0) {
                return false;
            }
            cluster = next;
        }
        true
    }

    /// Allocates `count` clusters, links them into a chain and returns the first.
    fn allocate(&self, count: u32) -> Option<u16> {
        let mut found: Vec<u16> = Vec::new();
        let limit = self.cluster_count + u32::from(FIRST_CLUSTER);
        for sector_index in 0..u32::from(self.fat_size) {
            if found.len() as u32 >= count {
                break;
            }
            let mut sector = [0u8; SECTOR];
            if !ata::read_sectors(self.drive, self.fat_start() + sector_index, 1, &mut sector) {
                return None;
            }
            for index in (0..SECTOR).step_by(2) {
                let cluster = sector_index * 256 + (index / 2) as u32;
                if cluster < u32::from(FIRST_CLUSTER) {
                    continue;
                }
                if cluster >= limit {
                    break;
                }
                if u16::from_le_bytes([sector[index], sector[index + 1]]) == 0 {
                    found.push(cluster as u16);
                    if found.len() as u32 >= count {
                        break;
                    }
                }
            }
        }
        if found.len() as u32 != count {
            return None;
        }
        for index in 0..found.len() {
            let value = if index + 1 < found.len() {
                found[index + 1]
            } else {
                FAT_END
            };
            if !self.write_fat(found[index], value) {
                return None;
            }
        }
        Some(found[0])
    }

    /// Lists the root directory.
    pub fn list(&self) -> Vec<Entry> {
        let mut entries = Vec::new();
        for index in 0..self.root_sectors {
            let Some(sector) = self.read_root_sector(index) else {
                break;
            };
            for chunk in sector.chunks_exact(DIR_ENTRY) {
                if chunk[0] == 0x00 {
                    return entries; // end of directory
                }
                if chunk[0] == 0xE5
                    || chunk[11] == ATTR_LONG_NAME
                    || chunk[11] & ATTR_VOLUME_ID != 0
                {
                    continue;
                }
                entries.push(decode_entry(chunk));
            }
        }
        entries
    }

    /// Finds a root-directory entry by name, returning its sector and slot.
    fn find(&self, name: &str) -> Option<(u32, usize, Entry)> {
        let wanted = short_name_bytes(name);
        for index in 0..self.root_sectors {
            let Some(sector) = self.read_root_sector(index) else {
                break;
            };
            for (slot, chunk) in sector.chunks_exact(DIR_ENTRY).enumerate() {
                if chunk[0] == 0x00 {
                    return None;
                }
                if chunk[0] == 0xE5 || chunk[11] == ATTR_LONG_NAME {
                    continue;
                }
                if chunk[0..11] == wanted {
                    return Some((index, slot, decode_entry(chunk)));
                }
            }
        }
        None
    }

    /// The first free root-directory slot, as `(sector, slot)`.
    fn free_slot(&self) -> Option<(u32, usize)> {
        for index in 0..self.root_sectors {
            let Some(sector) = self.read_root_sector(index) else {
                break;
            };
            for (slot, chunk) in sector.chunks_exact(DIR_ENTRY).enumerate() {
                if chunk[0] == 0x00 || chunk[0] == 0xE5 {
                    return Some((index, slot));
                }
            }
        }
        None
    }

    fn mark_deleted(&self, sector_index: u32, slot: usize) -> bool {
        let Some(mut sector) = self.read_root_sector(sector_index) else {
            return false;
        };
        sector[slot * DIR_ENTRY] = 0xE5;
        self.write_root_sector(sector_index, &sector)
    }

    fn write_entry(
        &self,
        sector_index: u32,
        slot: usize,
        name: &str,
        cluster: u16,
        size: u32,
    ) -> bool {
        let Some(mut sector) = self.read_root_sector(sector_index) else {
            return false;
        };
        let entry = &mut sector[slot * DIR_ENTRY..slot * DIR_ENTRY + DIR_ENTRY];
        entry.fill(0);
        entry[0..11].copy_from_slice(&short_name_bytes(name));
        entry[11] = ATTR_ARCHIVE;
        entry[26..28].copy_from_slice(&cluster.to_le_bytes());
        entry[28..32].copy_from_slice(&size.to_le_bytes());
        self.write_root_sector(sector_index, &sector)
    }

    /// Reads a whole file into memory.
    pub fn read(&self, name: &str) -> Option<Vec<u8>> {
        let (_, _, entry) = self.find(name)?;
        if entry.is_directory() {
            return None;
        }

        let size = entry.size as usize;
        let cluster_bytes = self.cluster_bytes();
        let mut data = Vec::with_capacity(size);
        let mut buffer = alloc::vec![0u8; cluster_bytes];
        let mut cluster = entry.cluster;
        while cluster >= FIRST_CLUSTER && data.len() < size {
            let lba = self.first_sector_of_cluster(cluster);
            if !ata::read_sectors(self.drive, lba, self.sectors_per_cluster, &mut buffer) {
                return None;
            }
            let remaining = size - data.len();
            let take = remaining.min(cluster_bytes);
            data.extend_from_slice(&buffer[..take]);
            cluster = self.read_fat(cluster)?;
        }
        Some(data)
    }

    /// Creates or replaces a file with `data`.
    pub fn write(&self, name: &str, data: &[u8]) -> bool {
        // Replace an existing file: free its clusters and reuse its slot.
        let mut slot = None;
        if let Some((sector_index, index, entry)) = self.find(name) {
            if entry.is_directory() {
                return false;
            }
            if !self.free_chain(entry.cluster) {
                return false;
            }
            slot = Some((sector_index, index));
        }
        let Some((sector_index, index)) = slot.or_else(|| self.free_slot()) else {
            return false;
        };

        let cluster_bytes = self.cluster_bytes();
        let needed = data.len().div_ceil(cluster_bytes) as u32;
        let first = if needed == 0 {
            0
        } else {
            let Some(first) = self.allocate(needed) else {
                return false;
            };
            first
        };

        if needed > 0 {
            let mut buffer = alloc::vec![0u8; cluster_bytes];
            let mut cluster = first;
            let mut written = 0usize;
            while cluster >= FIRST_CLUSTER {
                let take = (data.len() - written).min(cluster_bytes);
                buffer[..take].copy_from_slice(&data[written..written + take]);
                for byte in &mut buffer[take..] {
                    *byte = 0;
                }
                let lba = self.first_sector_of_cluster(cluster);
                if !ata::write_sectors(self.drive, lba, self.sectors_per_cluster, &buffer) {
                    return false;
                }
                written += take;
                if written >= data.len() {
                    break;
                }
                let Some(next) = self.read_fat(cluster) else {
                    return false;
                };
                cluster = next;
            }
        }

        self.write_entry(sector_index, index, name, first, data.len() as u32)
    }

    /// Deletes a file.
    pub fn delete(&self, name: &str) -> bool {
        let Some((sector_index, index, entry)) = self.find(name) else {
            return false;
        };
        if entry.is_directory() {
            return false;
        }
        if !self.free_chain(entry.cluster) {
            return false;
        }
        self.mark_deleted(sector_index, index)
    }

    /// A one-line description of the volume geometry, used by the boot log.
    pub fn summary(&self) -> String {
        let label = if self.label.is_empty() {
            "(none)"
        } else {
            self.label.as_str()
        };
        let oem = if self.oem.is_empty() {
            "(none)"
        } else {
            self.oem.as_str()
        };
        let megabytes =
            u64::from(self.total_sectors) * u64::from(self.bytes_per_sector) / (1024 * 1024);
        let cluster_kib =
            u32::from(self.sectors_per_cluster) * u32::from(self.bytes_per_sector) / 1024;
        format!(
            "{megabytes} MiB, {cluster_kib} KiB clusters, label {label}, oem {oem}, \
             {} FATs, {} root entries, {} clusters",
            self.fats, self.root_entries, self.cluster_count,
        )
    }
}

/// Decodes one 32-byte directory entry.
fn decode_entry(chunk: &[u8]) -> Entry {
    Entry {
        name: short_name(&chunk[0..11]),
        size: u32::from_le_bytes([chunk[28], chunk[29], chunk[30], chunk[31]]),
        cluster: u16::from_le_bytes([chunk[26], chunk[27]]),
        attributes: chunk[11],
    }
}

/// Turns `name` into an 8.3 directory-name field.
fn short_name_bytes(name: &str) -> [u8; 11] {
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

/// Renders an 8.3 field as `NAME.EXT`.
fn short_name(bytes: &[u8]) -> String {
    let base = String::from_utf8_lossy(&bytes[0..8]);
    let base = base.trim_end();
    let extension = String::from_utf8_lossy(&bytes[8..11]);
    let extension = extension.trim_end();
    if extension.is_empty() {
        String::from(base)
    } else {
        format!("{base}.{extension}")
    }
}

/// Whether `name` is a short enough 8.3 name.
pub fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 12 || name.contains(['/', '\\', '*', '?', ':']) {
        return false;
    }
    let (base, extension) = match name.split_once('.') {
        Some((base, extension)) => (base, extension),
        None => (name, ""),
    };
    !base.is_empty() && base.len() <= 8 && extension.len() <= 3
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Creates a fresh FAT16 filesystem on `drive`, destroying its contents.
pub fn format(drive: usize) -> bool {
    let total_sectors = ata::sectors(drive);
    if total_sectors < 8192 {
        return false; // too small to hold a FAT16 volume
    }

    let sectors_per_cluster: u8 = 4;
    let reserved: u16 = 1;
    let fats: u8 = 2;
    let root_entries: u16 = 512;
    let root_sectors = (u32::from(root_entries) * DIR_ENTRY as u32).div_ceil(SECTOR as u32);

    // Grow the FAT until it can index every data cluster.
    let mut fat_size = 1u16;
    let cluster_count;
    loop {
        let data_start = u32::from(reserved) + u32::from(fats) * u32::from(fat_size) + root_sectors;
        if data_start >= total_sectors {
            return false;
        }
        let clusters = (total_sectors - data_start) / u32::from(sectors_per_cluster);
        if u32::from(fat_size) * 256 >= clusters + u32::from(FIRST_CLUSTER) {
            cluster_count = clusters;
            break;
        }
        fat_size += 1;
        if fat_size > 512 {
            return false;
        }
    }

    // Boot sector.
    let mut boot = [0u8; SECTOR];
    boot[0] = 0xEB;
    boot[1] = 0x3C;
    boot[2] = 0x90;
    boot[3..11].copy_from_slice(b"GTOS  ");
    boot[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    boot[13] = sectors_per_cluster;
    boot[14..16].copy_from_slice(&reserved.to_le_bytes());
    boot[16] = fats;
    boot[17..19].copy_from_slice(&root_entries.to_le_bytes());
    let total16 = u16::try_from(total_sectors).unwrap_or(0);
    boot[19..21].copy_from_slice(&total16.to_le_bytes());
    boot[21] = 0xF8; // fixed disk
    boot[22..24].copy_from_slice(&fat_size.to_le_bytes());
    boot[24..26].copy_from_slice(&32u16.to_le_bytes()); // sectors per track
    boot[26..28].copy_from_slice(&64u16.to_le_bytes()); // heads
    boot[32..36].copy_from_slice(&total_sectors.to_le_bytes());
    boot[36] = 0x80; // drive number
    boot[38] = 0x29; // extended boot signature
    boot[39..43].copy_from_slice(&0x5255_5354u32.to_le_bytes()); // volume id
    boot[43..54].copy_from_slice(b"GTOS     ");
    boot[54..62].copy_from_slice(b"FAT16   ");
    boot[510] = 0x55;
    boot[511] = 0xAA;
    if !ata::write_sectors(drive, 0, 1, &boot) {
        return false;
    }

    // The first FAT sector holds the media descriptor and the end marker; the
    // rest of every FAT is zeroed.
    let mut first_fat = [0u8; SECTOR];
    first_fat[0..2].copy_from_slice(&0xFFF8u16.to_le_bytes());
    first_fat[2..4].copy_from_slice(&FAT_END.to_le_bytes());
    let zero = [0u8; SECTOR];
    for copy in 0..u32::from(fats) {
        let base = u32::from(reserved) + copy * u32::from(fat_size);
        if !ata::write_sectors(drive, base, 1, &first_fat) {
            return false;
        }
        for index in 1..u32::from(fat_size) {
            if !ata::write_sectors(drive, base + index, 1, &zero) {
                return false;
            }
        }
    }

    // Clear the root directory.
    let root_start = u32::from(reserved) + u32::from(fats) * u32::from(fat_size);
    for index in 0..root_sectors {
        if !ata::write_sectors(drive, root_start + index, 1, &zero) {
            return false;
        }
    }

    let _ = cluster_count;
    ata::flush(drive)
}
