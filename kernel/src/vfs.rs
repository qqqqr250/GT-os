//! A minimal virtual filesystem over the existing scheme namespace.
//!
//! The scheme layer addresses resources as URLs such as `file:/README.TXT`.
//! Unix programs expect paths, a current directory and directory metadata, so
//! this module adds the small amount of structure needed to bridge the two:
//!
//! * a mount table that maps POSIX paths onto scheme URLs;
//! * `.`/`..` normalization that cannot escape the root;
//! * inode-like metadata and directory listings for mounted resources;
//! * dynamic `/srv/<scheme>` mounts for registered ring-3 scheme servers.
//!
//! The VFS does not duplicate file data. It resolves a path to a scheme URL and
//! leaves the actual read/write to `scheme.rs`, which owns snapshot, file
//! stream and directory stream descriptors.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// A regular file node.
pub const KIND_FILE: u8 = 1;
/// A directory node.
pub const KIND_DIR: u8 = 2;

/// The fixed capacity of the `readdir` syscall's user buffer.
pub const READDIR_BYTES: usize = 1024;

/// Stable metadata returned by the `stat` syscall.
///
/// The layout is part of the guest ABI, so every field has an explicit width.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    /// A stable identifier derived from the absolute path.
    pub inode: u64,
    /// The node's size in bytes, or zero for streams and directories.
    pub size: u64,
    /// Either [`KIND_FILE`] or [`KIND_DIR`].
    pub kind: u8,
    /// Whether ring 3 may write through this node.
    pub writable: u8,
    /// Padding that keeps the struct's layout stable.
    pub reserved: [u8; 6],
}

#[derive(Clone, Copy)]
enum MountKind {
    File,
    Sys,
    Net,
    Virtual,
}

#[derive(Clone, Copy)]
struct Mount {
    path: &'static str,
    scheme: &'static str,
    kind: MountKind,
    read_only: bool,
}

/// The boot-time mount table. Longer prefixes win during resolution.
const MOUNTS: &[Mount] = &[
    Mount {
        path: "/",
        scheme: "file",
        kind: MountKind::File,
        read_only: false,
    },
    Mount {
        path: "/sys",
        scheme: "sys",
        kind: MountKind::Sys,
        read_only: true,
    },
    Mount {
        path: "/net",
        scheme: "net",
        kind: MountKind::Net,
        read_only: true,
    },
    Mount {
        path: "/srv",
        scheme: "",
        kind: MountKind::Virtual,
        read_only: true,
    },
];

/// Kernel state exposed by `sys:`, used for directory listings and metadata.
const SYS_ENTRIES: &[&str] = &["version", "heap", "mem", "net", "uptime", "help", "schemes"];

fn inode_for(path: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in path.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Zero is reserved for "no inode", so fold odd values only.
    hash | 1
}

/// Normalizes `path` relative to `cwd`, returning an absolute path.
///
/// Empty components and `.` are ignored, `..` pops one component, and popping
/// above the root is a no-op. NUL bytes are rejected before a path can reach a
/// C-style consumer.
pub fn normalize(cwd: &str, path: &str) -> Result<String, String> {
    if path.bytes().any(|byte| byte == 0) || cwd.bytes().any(|byte| byte == 0) {
        return Err(String::from("path contains a NUL byte"));
    }
    let base = if cwd.starts_with('/') { cwd } else { "/" };
    let sources: [&str; 2] = if path.starts_with('/') {
        ["/", path]
    } else {
        [base, path]
    };
    let mut parts: Vec<&str> = Vec::new();
    for source in sources {
        for part in source.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
    }
    let mut output = String::from("/");
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            output.push('/');
        }
        output.push_str(part);
    }
    Ok(output)
}

/// Returns true when `name` is currently served by a ring-3 scheme process.
fn is_registered(name: &str) -> bool {
    crate::scheme::is_registered(name)
}

/// Converts a URL into an absolute VFS path so `stat` can accept both forms.
fn path_from_url(url: &str) -> Result<String, String> {
    let (scheme, path) = url
        .split_once(':')
        .ok_or_else(|| String::from("missing scheme in URL"))?;
    let path = path.trim_start_matches('/');
    if scheme.eq_ignore_ascii_case("file") {
        normalize("/", path)
    } else if scheme.eq_ignore_ascii_case("sys") {
        normalize("/sys", path)
    } else if scheme.eq_ignore_ascii_case("net") {
        Ok(format!("/net/{path}"))
    } else if is_registered(scheme) {
        Ok(format!("/srv/{scheme}/{path}"))
    } else {
        Err(format!("unknown scheme '{scheme}'"))
    }
}

/// Resolves `path` to an absolute VFS path, accepting URLs as a convenience.
pub fn absolute_path(cwd: &str, path: &str) -> Result<String, String> {
    // `/net/` and `/srv/` tails can themselves be URLs containing `//`, so
    // handle those prefixes before the general normalizer collapses slashes.
    if let Some(rest) = path.strip_prefix("/net/") {
        return Ok(format!("/net/{rest}"));
    }
    if let Some(rest) = path.strip_prefix("/srv/") {
        let (name, tail) = rest.split_once('/').unwrap_or((rest, ""));
        if is_registered(name) {
            return Ok(format!("/srv/{name}/{tail}"));
        }
    }
    if path.contains(':') {
        return path_from_url(path);
    }
    normalize(cwd, path)
}

fn mount_rest<'a>(mount: &Mount, absolute: &'a str) -> Option<&'a str> {
    if mount.path == "/" {
        return absolute.strip_prefix('/');
    }
    if absolute == mount.path {
        return Some("");
    }
    absolute.strip_prefix(mount.path)?.strip_prefix('/')
}

fn best_mount(absolute: &str) -> Option<(usize, &'static Mount, &str)> {
    let mut best: Option<(usize, &'static Mount, &str)> = None;
    for mount in MOUNTS {
        if let Some(rest) = mount_rest(mount, absolute) {
            let replace = best.is_none_or(|(length, _, _)| mount.path.len() > length);
            if replace {
                best = Some((mount.path.len(), mount, rest));
            }
        }
    }
    best
}

/// Splits `/srv/<registered-scheme>/<tail>` into its scheme and tail.
fn scheme_mount(absolute: &str) -> Option<(&str, &str)> {
    let rest = absolute.strip_prefix("/srv/")?;
    let (name, tail) = rest.split_once('/').unwrap_or((rest, ""));
    if name.is_empty() || !is_registered(name) {
        return None;
    }
    Some((name, tail))
}

/// Resolves an absolute path to the scheme URL that will open it.
pub fn absolute_to_url(absolute: &str) -> Result<String, String> {
    if let Some((name, tail)) = scheme_mount(absolute) {
        if tail.is_empty() {
            return Err(String::from("scheme mount is a directory"));
        }
        return Ok(format!("{name}:/{tail}"));
    }
    let Some((_, mount, rest)) = best_mount(absolute) else {
        return Err(String::from("path is not mounted"));
    };
    if rest.is_empty() {
        return Err(String::from("path is a directory"));
    }
    Ok(format!("{}:/{}", mount.scheme, rest))
}

/// Resolves a caller path (absolute, relative, or URL) to a scheme URL.
pub fn resolve(cwd: &str, path: &str) -> Result<String, String> {
    absolute_to_url(&absolute_path(cwd, path)?)
}

fn file_entry(rest: &str) -> Option<(bool, u32)> {
    let entries = crate::fat::with_volume(crate::fat::Volume::list)?;
    entries
        .into_iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(rest))
        .map(|entry| (entry.is_directory(), entry.size))
}

/// Returns metadata for an absolute VFS path.
pub fn stat_absolute(absolute: &str) -> Result<Stat, String> {
    if absolute == "/" {
        return Ok(Stat {
            inode: inode_for("/"),
            size: 0,
            kind: KIND_DIR,
            writable: 0,
            reserved: [0; 6],
        });
    }
    if let Some((_, tail)) = scheme_mount(absolute) {
        return Ok(Stat {
            inode: inode_for(absolute),
            size: 0,
            kind: if tail.is_empty() { KIND_DIR } else { KIND_FILE },
            writable: u8::from(!tail.is_empty()),
            reserved: [0; 6],
        });
    }
    let Some((_, mount, rest)) = best_mount(absolute) else {
        return Err(String::from("path is not mounted"));
    };
    let (kind, size, writable) = match mount.kind {
        MountKind::File if rest.is_empty() => (KIND_DIR, 0, u8::from(!mount.read_only)),
        MountKind::File => {
            let (directory, size) =
                file_entry(rest).ok_or_else(|| format!("no such file: {rest}"))?;
            (
                if directory { KIND_DIR } else { KIND_FILE },
                u64::from(size),
                u8::from(!mount.read_only),
            )
        }
        MountKind::Sys | MountKind::Net if rest.is_empty() => (KIND_DIR, 0, 0),
        MountKind::Sys => {
            if !SYS_ENTRIES
                .iter()
                .any(|entry| entry.eq_ignore_ascii_case(rest))
            {
                return Err(format!("no such sys resource: {rest}"));
            }
            let url = format!("sys:/{rest}");
            let size = crate::scheme::open(&url).map_or(0, |resource| resource.data.len() as u64);
            (KIND_FILE, size, 0)
        }
        MountKind::Net => (KIND_FILE, 0, 0),
        MountKind::Virtual => {
            if rest.is_empty() || is_registered(rest) {
                (KIND_DIR, 0, 0)
            } else {
                return Err(format!("no such scheme: {rest}"));
            }
        }
    };
    Ok(Stat {
        inode: inode_for(absolute),
        size,
        kind,
        writable,
        reserved: [0; 6],
    })
}

/// Returns metadata for a caller path (absolute, relative, or URL).
pub fn stat(cwd: &str, path: &str) -> Result<Stat, String> {
    stat_absolute(&absolute_path(cwd, path)?)
}

/// Lists an absolute directory's child names.
pub fn read_dir_absolute(absolute: &str) -> Result<Vec<String>, String> {
    if absolute == "/" {
        let mut names: Vec<String> = ["srv", "sys", "net"]
            .iter()
            .map(|s| String::from(*s))
            .collect();
        if let Some(entries) = crate::fat::with_volume(crate::fat::Volume::list) {
            names.extend(entries.into_iter().map(|entry| entry.name));
        }
        return Ok(names);
    }
    if let Some((name, tail)) = scheme_mount(absolute) {
        return if tail.is_empty() {
            Ok(Vec::new())
        } else {
            Err(format!("{name} stream is not a directory"))
        };
    }
    let Some((_, mount, rest)) = best_mount(absolute) else {
        return Err(String::from("path is not mounted"));
    };
    match mount.kind {
        MountKind::File if rest.is_empty() => {
            let entries = crate::fat::with_volume(crate::fat::Volume::list)
                .ok_or_else(|| String::from("no FAT16 volume is mounted"))?;
            Ok(entries.into_iter().map(|entry| entry.name).collect())
        }
        MountKind::Sys if rest.is_empty() => {
            Ok(SYS_ENTRIES.iter().map(|s| String::from(*s)).collect())
        }
        MountKind::Net if rest.is_empty() => {
            Ok(["http", "https"].iter().map(|s| String::from(*s)).collect())
        }
        MountKind::Virtual if rest.is_empty() => Ok(crate::scheme::names()),
        _ => Err(String::from("not a directory")),
    }
}

/// Lists a caller path's child names.
pub fn read_dir(cwd: &str, path: &str) -> Result<Vec<String>, String> {
    read_dir_absolute(&absolute_path(cwd, path)?)
}

/// Encodes names as NUL-separated bytes for the `readdir` syscall.
pub fn encode_names(names: &[String]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for name in names {
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
    }
    bytes
}

/// Formats the mount table for the shell's `mounts` command.
pub fn mount_table() -> Vec<String> {
    let mut rows: Vec<String> = MOUNTS
        .iter()
        .map(|mount| {
            format!(
                "{} -> {} [{}]",
                mount.path,
                if mount.scheme.is_empty() {
                    "scheme servers"
                } else {
                    mount.scheme
                },
                if mount.read_only { "ro" } else { "rw" }
            )
        })
        .collect();
    for name in crate::scheme::names() {
        rows.push(format!("/srv/{name} -> {name}: [rw]"));
    }
    rows
}

/// Initializes the mount table. The table itself is static; this hook exists so
/// boot code has a clear point that establishes the filesystem namespace.
pub fn init() {
    // Resolving a path can consult registered schemes, so nothing is cached yet.
}
