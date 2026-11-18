//! Data model for the desktop's file manager.
//!
//! The GUI in [`crate::wm`] owns the interaction state (selection, scrolling,
//! sort direction, the search field). This module owns the parts that can be
//! tested without a framebuffer: taking a snapshot of the mounted FAT16 volume,
//! filtering it, sorting it and formatting the labels shown in the list.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::fat;

/// Column the file list is sorted by.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Type,
}

/// One row in the file list.
#[derive(Clone)]
pub struct Item {
    pub name: String,
    pub size: u32,
    pub is_dir: bool,
}

impl Item {
    /// Human-readable type shown in the "Type" column.
    pub fn kind(&self) -> &'static str {
        kind(&self.name, self.is_dir)
    }

    /// Human-readable size shown in the "Size" column.
    pub fn size_text(&self) -> String {
        format_size(self.size)
    }
}

/// Reads the mounted FAT16 root directory into a snapshot for the GUI.
///
/// Returns an empty list when no data disk is mounted, so the window still
/// opens and explains why it is empty instead of faulting.
pub fn snapshot() -> Vec<Item> {
    fat::with_volume(|volume| {
        volume
            .list()
            .into_iter()
            .map(|entry| {
                let is_dir = entry.is_directory();
                Item {
                    name: entry.name,
                    size: entry.size,
                    is_dir,
                }
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Builds the visible list: filter, then group folders first and sort.
pub fn view(all: &[Item], filter: &str, key: SortKey, descending: bool) -> Vec<Item> {
    let mut items: Vec<Item> = all
        .iter()
        .filter(|item| matches(&item.name, filter))
        .cloned()
        .collect();
    sort(&mut items, key, descending);
    items
}

/// Case-insensitive search. `*` is a wildcard; without it the filter is a
/// substring match, which makes short searches behave like Sigma's smart search.
pub fn matches(name: &str, filter: &str) -> bool {
    let pattern = filter.trim();
    if pattern.is_empty() {
        return true;
    }
    if pattern.contains('*') {
        wildcard(name, pattern)
    } else {
        find_ci(name, pattern).is_some()
    }
}

/// Sorting used by the column headers. Folders always stay above files; the
/// direction only reverses the ordering inside each group.
pub fn sort(items: &mut [Item], key: SortKey, descending: bool) {
    items.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            return if a.is_dir {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        let order = match key {
            SortKey::Name => a.name.cmp(&b.name),
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Type => a.kind().cmp(b.kind()).then_with(|| a.name.cmp(&b.name)),
        };
        if descending { order.reverse() } else { order }
    });
}

/// Converts a byte count into the compact form used in the Size column.
pub fn format_size(size: u32) -> String {
    if size >= 1024 * 1024 {
        let megabytes = size / (1024 * 1024);
        format!("{megabytes} MB")
    } else if size >= 1024 {
        let kilobytes = size / 1024;
        format!("{kilobytes} KB")
    } else {
        format!("{size} B")
    }
}

/// Label for the Type column, derived from the 8.3 extension.
pub fn kind(name: &str, is_dir: bool) -> &'static str {
    if is_dir {
        return "Folder";
    }
    let extension = name.rsplit_once('.').map_or("", |(_, ext)| ext);
    match extension.to_ascii_uppercase().as_str() {
        "TXT" | "LOG" | "MD" | "ME" | "README" => "Text Document",
        "BIN" | "EXE" | "COM" | "SYS" | "ELF" => "Application",
        "WAV" | "MP3" | "OGG" => "Audio",
        "PNG" | "JPG" | "JPEG" | "BMP" | "GIF" => "Image",
        "ZIP" | "TAR" | "GZ" => "Archive",
        "TOML" | "JSON" | "CFG" | "INI" | "CONF" => "Configuration",
        "HTML" | "HTM" => "HTML Document",
        _ => "File",
    }
}

/// `*` wildcard matching, case-insensitive and applied left to right.
fn wildcard(name: &str, pattern: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return true;
    };
    let Some(start) = find_ci(name, first) else {
        return false;
    };
    if start != 0 {
        return false;
    }
    let mut pos = first.len();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        let Some(offset) = find_ci(&name[pos.min(name.len())..], part) else {
            return false;
        };
        pos += offset + part.len();
    }
    pattern.ends_with('*') || pos == name.len()
}

/// Byte offset of `needle` in `haystack`, ignoring ASCII case.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    for start in 0..=haystack.len() - needle.len() {
        if haystack[start..start + needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            return Some(start);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, size: u32, is_dir: bool) -> Item {
        Item {
            name: String::from(name),
            size,
            is_dir,
        }
    }

    fn sample() -> Vec<Item> {
        alloc::vec![
            item("README.TXT", 1024, false),
            item("BEEP.WAV", 2048, false),
            item("HELLO.BIN", 512, false),
            item("SUBDIR", 0, true),
        ]
    }

    #[test_case]
    fn filter_is_a_case_insensitive_substring_search() {
        assert!(matches("README.TXT", "read"));
        assert!(matches("README.TXT", "me.tx"));
        assert!(!matches("BEEP.WAV", "read"));
        assert!(matches("BEEP.WAV", ""));
    }

    #[test_case]
    fn wildcard_filters_match_extensions() {
        assert!(matches("BEEP.WAV", "*.WAV"));
        assert!(matches("HELLO.BIN", "H*.BIN"));
        assert!(!matches("README.TXT", "*.WAV"));
    }

    #[test_case]
    fn view_filters_and_sorts_with_folders_first() {
        let all = sample();
        let shown = view(&all, "", SortKey::Name, false);
        let names: Vec<&str> = shown.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["SUBDIR", "BEEP.WAV", "HELLO.BIN", "README.TXT"]);

        let shown = view(&all, "TXT", SortKey::Name, false);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].name, "README.TXT");
    }

    #[test_case]
    fn sorting_by_size_can_be_reversed() {
        let all = sample();
        let mut items = view(&all, "", SortKey::Size, false);
        sort(&mut items, SortKey::Size, true);
        // Folders stay above files even when the order is reversed.
        assert_eq!(items[0].name, "SUBDIR");
        assert_eq!(items[1].name, "BEEP.WAV");
        assert_eq!(items[2].name, "README.TXT");
        assert_eq!(items[3].name, "HELLO.BIN");
    }

    #[test_case]
    fn sizes_and_types_have_readable_labels() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2 KB");
        assert_eq!(format_size(1024 * 1024), "1 MB");
        assert_eq!(kind("README.TXT", false), "Text Document");
        assert_eq!(kind("HELLO.BIN", false), "Application");
        assert_eq!(kind("SUBDIR", true), "Folder");
        assert_eq!(kind("UNKNOWN.XYZ", false), "File");
    }
}
