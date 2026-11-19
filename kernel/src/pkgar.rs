//! Clean-room package archive reader, inspired by Redox `pkgar`.
//!
//! `pkgar` stores a signed, hashed header followed by a table of fixed-size
//! entries and the file data. `GTos` adopts the *shape* — packed header, fixed
//! 308-byte entries, a hash of the entry table, a per-file hash, overflow-safe
//! ranges and path validation — but uses SHA-256 instead of Blake3/Ed25519
//! (neither exists in this kernel) and a distinct magic. The result is **not**
//! binary-compatible with Redox `.pkgar`; it is a small, auditable format for
//! the FAT16 root directory.
//!
//! Header layout (136 bytes, little-endian):
//!   magic[8] | hash[32] | count(u32) | flags(u32) | reserved[88]
//!
//! Entry layout (308 bytes, little-endian):
//!   hash[32] | offset(u64) | size(u64) | mode(u32) | path[256]
//!
//! `flags`: bits 0-7 format version, bits 8-15 architecture, bits 16-23 hash
//! id. The data portion begins at `HEADER_SIZE + count * ENTRY_SIZE`.

use alloc::string::String;
use alloc::vec::Vec;

use sha2::{Digest, Sha256};

/// Bytes at the front of every `GTos` archive.
pub const MAGIC: &[u8; 8] = b"GTOSPKGA";
pub const HEADER_SIZE: usize = 136;
pub const ENTRY_SIZE: usize = 308;

const VERSION_V0: u8 = 0;
const HASH_SHA256: u8 = 0;
const ARCH_X86_64: u8 = 1;

/// Everything that can go wrong while reading an archive. No panics: a corrupt
/// file from the data disk must only produce an error message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveError {
    TooShort,
    BadMagic,
    UnsupportedVersion(u8),
    UnsupportedHash(u8),
    UnsupportedArchitecture(u8),
    Overflow,
    TableTooShort,
    TableHashMismatch,
    InvalidPath,
    EntryOutOfBounds,
    EntryHashMismatch,
}

/// Decoded archive header.
#[derive(Clone, Debug)]
pub struct Header {
    /// SHA-256 of the raw entry-table bytes.
    pub hash: [u8; 32],
    pub count: u32,
    pub version: u8,
    pub arch: u8,
    pub hash_id: u8,
}

impl Header {
    /// Parses and validates the fixed-size header.
    pub fn parse(data: &[u8]) -> Result<Self, ArchiveError> {
        let bytes = data.get(..HEADER_SIZE).ok_or(ArchiveError::TooShort)?;
        if &bytes[..8] != MAGIC {
            return Err(ArchiveError::BadMagic);
        }
        let version = bytes[44];
        let arch = bytes[45];
        let hash_id = bytes[46];
        if version != VERSION_V0 {
            return Err(ArchiveError::UnsupportedVersion(version));
        }
        if hash_id != HASH_SHA256 {
            return Err(ArchiveError::UnsupportedHash(hash_id));
        }
        if arch != ARCH_X86_64 {
            return Err(ArchiveError::UnsupportedArchitecture(arch));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes[8..40]);
        let count = u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]);
        Ok(Self {
            hash,
            count,
            version,
            arch,
            hash_id,
        })
    }

    /// Size of the entry table with a checked multiply.
    pub fn entries_size(&self) -> Result<usize, ArchiveError> {
        (self.count as usize)
            .checked_mul(ENTRY_SIZE)
            .ok_or(ArchiveError::Overflow)
    }

    /// First byte of the data portion with a checked add.
    pub fn data_offset(&self) -> Result<usize, ArchiveError> {
        HEADER_SIZE
            .checked_add(self.entries_size()?)
            .ok_or(ArchiveError::Overflow)
    }
}

/// Rejects traversal and absolute paths before a name can reach the disk.
fn validate_path(path: &str) -> Result<&str, ArchiveError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains('\\')
        || path.contains(':')
    {
        return Err(ArchiveError::InvalidPath);
    }
    for component in path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(ArchiveError::InvalidPath);
        }
    }
    Ok(path)
}

/// One decoded entry. `path` is validated before construction.
#[derive(Clone, Debug)]
pub struct Entry {
    pub hash: [u8; 32],
    pub offset: u64,
    pub size: u64,
    pub mode: u32,
    pub path: String,
}

impl Entry {
    fn parse(bytes: &[u8]) -> Result<Self, ArchiveError> {
        if bytes.len() < ENTRY_SIZE {
            return Err(ArchiveError::TableTooShort);
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes[..32]);
        let offset = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
        let size = u64::from_le_bytes(bytes[40..48].try_into().unwrap());
        let mode = u32::from_le_bytes(bytes[48..52].try_into().unwrap());

        let raw_path = &bytes[52..ENTRY_SIZE];
        let end = raw_path
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(raw_path.len());
        let path = core::str::from_utf8(&raw_path[..end]).map_err(|_| ArchiveError::InvalidPath)?;
        validate_path(path).map_err(|_| ArchiveError::InvalidPath)?;

        Ok(Self {
            hash,
            offset,
            size,
            mode,
            path: String::from(path),
        })
    }
}

/// A parsed and fully verified archive. Construction fails if any hash is bad
/// or any entry points outside the data portion.
#[derive(Debug)]
pub struct Archive<'a> {
    pub header: Header,
    pub entries: Vec<Entry>,
    data: &'a [u8],
}

impl<'a> Archive<'a> {
    /// Parses the header, verifies the entry table, then verifies every file.
    pub fn parse(data: &'a [u8]) -> Result<Self, ArchiveError> {
        let header = Header::parse(data)?;
        let table_size = header.entries_size()?;
        let table_end = HEADER_SIZE
            .checked_add(table_size)
            .ok_or(ArchiveError::Overflow)?;
        let table = data
            .get(HEADER_SIZE..table_end)
            .ok_or(ArchiveError::TableTooShort)?;

        let digest = Sha256::digest(table);
        if digest.as_slice() != header.hash.as_slice() {
            return Err(ArchiveError::TableHashMismatch);
        }

        let mut entries = Vec::with_capacity(header.count as usize);
        for chunk in table.chunks_exact(ENTRY_SIZE) {
            entries.push(Entry::parse(chunk)?);
        }

        let data_offset = header.data_offset()?;
        let payload = data
            .get(data_offset..)
            .ok_or(ArchiveError::EntryOutOfBounds)?;
        let archive = Self {
            header,
            entries,
            data: payload,
        };
        for entry in &archive.entries {
            let bytes = archive.entry_data(entry)?;
            let digest = Sha256::digest(bytes);
            if digest.as_slice() != entry.hash.as_slice() {
                return Err(ArchiveError::EntryHashMismatch);
            }
        }
        Ok(archive)
    }

    /// Slices one entry's data using checked conversion and addition, mirroring
    /// `pkgar`'s `PackageSrc::calculate_range`.
    fn entry_data(&self, entry: &Entry) -> Result<&'a [u8], ArchiveError> {
        let start = usize::try_from(entry.offset).map_err(|_| ArchiveError::EntryOutOfBounds)?;
        let length = usize::try_from(entry.size).map_err(|_| ArchiveError::EntryOutOfBounds)?;
        let end = start
            .checked_add(length)
            .ok_or(ArchiveError::EntryOutOfBounds)?;
        self.data
            .get(start..end)
            .ok_or(ArchiveError::EntryOutOfBounds)
    }

    /// Total bytes in the data portion.
    pub fn data_len(&self) -> usize {
        self.data.len()
    }
}

/// Builds an archive from named file contents. Entries are sorted by name so
/// packing the same files twice produces identical bytes, like `pkgar`.
pub fn build(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>, ArchiveError> {
    if files.is_empty() {
        return Err(ArchiveError::InvalidPath);
    }
    let mut ordered: Vec<&(String, Vec<u8>)> = files.iter().collect();
    ordered.sort_by(|left, right| left.0.cmp(&right.0));
    let count = u32::try_from(ordered.len()).map_err(|_| ArchiveError::Overflow)?;

    let mut entries = Vec::with_capacity(ordered.len() * ENTRY_SIZE);
    let mut payload = Vec::new();
    for (name, data) in ordered {
        validate_path(name)?;
        let name_bytes = name.as_bytes();
        if name_bytes.len() >= 256 {
            return Err(ArchiveError::InvalidPath);
        }
        let mut entry = [0u8; ENTRY_SIZE];
        entry[..32].copy_from_slice(Sha256::digest(data).as_slice());
        entry[32..40].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        entry[40..48].copy_from_slice(&(data.len() as u64).to_le_bytes());
        entry[48..52].copy_from_slice(&0o100_644u32.to_le_bytes());
        entry[52..52 + name_bytes.len()].copy_from_slice(name_bytes);
        entries.extend_from_slice(&entry);
        payload.extend_from_slice(data);
    }

    let mut archive = Vec::with_capacity(HEADER_SIZE + entries.len() + payload.len());
    archive.extend_from_slice(MAGIC);
    archive.extend_from_slice(Sha256::digest(&entries).as_slice());
    archive.extend_from_slice(&count.to_le_bytes());
    // Version 0, x86_64 architecture, SHA-256 hash.
    archive.extend_from_slice(&((ARCH_X86_64 as u32) << 8).to_le_bytes());
    archive.extend_from_slice(&[0u8; HEADER_SIZE - 48]);
    archive.extend_from_slice(&entries);
    archive.extend_from_slice(&payload);
    Ok(archive)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_archive() -> Vec<u8> {
        // One entry for the file "README.TXT" with the bytes b"hello".
        let payload = b"hello";
        let mut entry = [0u8; ENTRY_SIZE];
        entry[..32].copy_from_slice(Sha256::digest(payload).as_slice());
        entry[32..40].copy_from_slice(&0u64.to_le_bytes());
        entry[40..48].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        entry[48..52].copy_from_slice(&0o100644u32.to_le_bytes());
        entry[52..62].copy_from_slice(b"README.TXT");
        let mut archive = Vec::new();
        archive.extend_from_slice(MAGIC);
        archive.extend_from_slice(Sha256::digest(entry).as_slice());
        archive.extend_from_slice(&1u32.to_le_bytes());
        // version 0, architecture x86_64 (bits 8-15), hash id 0.
        archive.extend_from_slice(&((ARCH_X86_64 as u32) << 8).to_le_bytes());
        archive.extend_from_slice(&[0u8; HEADER_SIZE - 48]);
        archive.extend_from_slice(&entry);
        archive.extend_from_slice(payload);
        archive
    }

    #[test_case]
    fn parses_a_valid_archive() {
        let bytes = sample_archive();
        let archive = Archive::parse(&bytes).unwrap();
        assert_eq!(archive.entries.len(), 1);
        assert_eq!(archive.entries[0].path, "README.TXT");
        assert_eq!(archive.data_len(), 5);
    }

    #[test_case]
    fn rejects_a_corrupted_entry_table() {
        let mut bytes = sample_archive();
        bytes[HEADER_SIZE + 60] ^= 0xFF;
        assert_eq!(Archive::parse(&bytes), Err(ArchiveError::TableHashMismatch));
    }

    #[test_case]
    fn rejects_a_corrupted_file() {
        let mut bytes = sample_archive();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        assert_eq!(Archive::parse(&bytes), Err(ArchiveError::EntryHashMismatch));
    }

    #[test_case]
    fn rejects_traversal_paths() {
        for path in ["../evil", "/abs", "C:x", "a\\b", "a/./b", "a//b"] {
            assert!(validate_path(path).is_err(), "accepted {path}");
        }
        assert!(validate_path("DOCS/NOTE.TXT").is_ok());
    }
}
