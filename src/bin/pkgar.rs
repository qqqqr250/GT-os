//! Creates a `GTos` package archive (`.pkga`) for the FAT16 data disk.
//!
//! Usage: `pkgar <output.pkga> <file>...`
//!
//! The format is the clean-room one read by `kernel/src/pkgar.rs`: a 136-byte
//! header, fixed 308-byte entries, then concatenated file data. Entries are
//! sorted by name so packing the same files twice gives identical bytes.

use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

const MAGIC: &[u8; 8] = b"GTOSPKGA";
const HEADER_SIZE: usize = 136;
const ENTRY_SIZE: usize = 308;
const MODE_REGULAR: u32 = 0o100_644;

fn fail(message: &str) -> ! {
    eprintln!("pkgar: {message}");
    std::process::exit(1);
}

/// Applies the same path rules as the kernel reader so the two agree on what a
/// valid entry name is.
fn valid_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() >= 256
        || name.bytes().any(|byte| byte == 0)
        || name.starts_with('/')
        || name.starts_with('\\')
        || name.contains('\\')
        || name.contains(':')
    {
        return false;
    }
    name.split('/')
        .all(|component| !component.is_empty() && component != "." && component != "..")
}

/// Builds an archive from already-named file contents. Entries are sorted by
/// name, making the output reproducible.
fn build_archive(inputs: &[(String, Vec<u8>)]) -> Result<Vec<u8>, &'static str> {
    if inputs.is_empty() {
        return Err("no input files");
    }
    let mut ordered: Vec<&(String, Vec<u8>)> = inputs.iter().collect();
    ordered.sort_by(|left, right| left.0.cmp(&right.0));
    if ordered.iter().any(|(name, _)| !valid_name(name)) {
        return Err("invalid archive path");
    }
    let count = u32::try_from(ordered.len()).map_err(|_| "too many input files")?;

    let mut entries = Vec::with_capacity(ordered.len() * ENTRY_SIZE);
    let mut payload = Vec::new();
    for (name, data) in ordered {
        let mut entry = [0u8; ENTRY_SIZE];
        entry[..32].copy_from_slice(Sha256::digest(data).as_slice());
        entry[32..40].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        entry[40..48].copy_from_slice(&(data.len() as u64).to_le_bytes());
        entry[48..52].copy_from_slice(&MODE_REGULAR.to_le_bytes());
        entry[52..52 + name.len()].copy_from_slice(name.as_bytes());
        entries.extend_from_slice(&entry);
        payload.extend_from_slice(data);
    }

    let mut archive = Vec::with_capacity(HEADER_SIZE + entries.len() + payload.len());
    archive.extend_from_slice(MAGIC);
    archive.extend_from_slice(Sha256::digest(&entries).as_slice());
    archive.extend_from_slice(&count.to_le_bytes());
    // Bits 0-7: version 0; bits 8-15: x86_64; bits 16-23: SHA-256.
    archive.extend_from_slice(&(1u32 << 8).to_le_bytes());
    archive.extend_from_slice(&[0u8; HEADER_SIZE - 48]);
    archive.extend_from_slice(&entries);
    archive.extend_from_slice(&payload);
    Ok(archive)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(output) = args.next() else {
        fail("usage: pkgar <output.pkga> <file>...");
    };
    let files: Vec<String> = args.collect();

    // FAT16 only stores short uppercase names in the root directory.
    let mut inputs: Vec<(String, Vec<u8>)> = Vec::new();
    for file in files {
        let path = Path::new(&file);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_else(|| fail("input has no valid file name"))
            .to_ascii_uppercase();
        let data = fs::read(path).unwrap_or_else(|error| fail(&format!("{file}: {error}")));
        inputs.push((name, data));
    }

    let archive = build_archive(&inputs).unwrap_or_else(|error| fail(error));
    fs::write(&output, &archive).unwrap_or_else(|error| fail(&format!("{output}: {error}")));
    println!(
        "Wrote {output}: {} entries, {} bytes",
        inputs.len(),
        archive.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<(String, Vec<u8>)> {
        vec![
            (String::from("Z.TXT"), b"second".to_vec()),
            (String::from("A.TXT"), b"first".to_vec()),
        ]
    }

    #[test]
    fn header_matches_reader_layout() {
        let bytes = build_archive(&sample()).unwrap();
        assert_eq!(&bytes[..8], MAGIC);
        assert_eq!(bytes[44], 0, "version");
        assert_eq!(bytes[45], 1, "x86_64 architecture");
        assert_eq!(bytes[46], 0, "SHA-256 hash id");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 2);
        assert_eq!(
            &bytes[8..40],
            Sha256::digest(&bytes[136..136 + 2 * ENTRY_SIZE]).as_slice()
        );
    }

    #[test]
    fn entries_are_sorted_and_hashed() {
        let bytes = build_archive(&sample()).unwrap();
        let table = &bytes[136..136 + 2 * ENTRY_SIZE];
        let first = &table[..ENTRY_SIZE];
        let second = &table[ENTRY_SIZE..];

        let name = |entry: &[u8]| {
            let end = entry[52..].iter().position(|&b| b == 0).unwrap_or(256);
            std::str::from_utf8(&entry[52..52 + end])
                .unwrap()
                .to_string()
        };
        assert_eq!(name(first), "A.TXT");
        assert_eq!(name(second), "Z.TXT");

        let offset = |entry: &[u8]| u64::from_le_bytes(entry[32..40].try_into().unwrap());
        let size = |entry: &[u8]| u64::from_le_bytes(entry[40..48].try_into().unwrap());
        assert_eq!(offset(first), 0);
        assert_eq!(size(first), 5);
        assert_eq!(offset(second), 5);
        assert_eq!(size(second), 6);

        let data_offset = 136 + 2 * ENTRY_SIZE;
        assert_eq!(&bytes[data_offset..data_offset + 5], b"first");
        assert_eq!(&bytes[data_offset + 5..], b"second");
        assert_eq!(&first[..32], Sha256::digest(b"first").as_slice());
        assert_eq!(&second[..32], Sha256::digest(b"second").as_slice());
    }

    #[test]
    fn packing_is_reproducible() {
        assert_eq!(
            build_archive(&sample()).unwrap(),
            build_archive(&sample()).unwrap()
        );
    }

    #[test]
    fn rejects_bad_inputs() {
        assert!(build_archive(&[]).is_err());
        for name in ["../evil", "/abs", "C:x", "a\\b", "a/./b", "a//b", ""] {
            assert!(
                build_archive(&[(String::from(name), Vec::new())]).is_err(),
                "{name}"
            );
        }
    }
}
