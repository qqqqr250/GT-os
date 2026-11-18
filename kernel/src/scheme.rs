//! A small Redox-inspired scheme namespace: "everything is a URL".
//!
//! Redox exposes system resources as paths such as `file:/etc/passwd` or
//! `net:/http://example.com`. `GTos` cannot host user-space scheme servers, but the
//! same idea is useful inside the kernel: every readable resource is addressed
//! uniformly, can be listed with `schemes`, read with `open`, and — through the
//! descriptor table below — from ring 3 via the `open`/`read`/`close` syscalls.
//!
//! The built-in schemes are:
//!
//! | Scheme | Example | Source |
//! |--------|---------|--------|
//! | `file` | `file:/README.TXT` | mounted FAT16 data disk |
//! | `net`  | `net:/http://example.com` | the HTTP(S) client |
//! | `sys`  | `sys:/mem`, `sys:/net` | kernel status |
//!
//! An opened resource is snapshotted into memory. That is a deliberate
//! simplification of Redox's streaming file descriptors: it keeps the syscall
//! surface tiny while still giving programs a single, uniform read API.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use bootloader_api::BootInfo;
use spin::Mutex;

/// An opened resource: its bytes, a MIME type, and a read cursor.
#[derive(Debug)]
pub struct Resource {
    /// The canonical URL this resource was opened from.
    pub url: String,
    /// A best-effort MIME type, useful to callers that want to display it.
    pub mime: String,
    /// The resource contents.
    pub data: Vec<u8>,
    cursor: usize,
}

impl Resource {
    /// Decodes the resource as text, replacing invalid UTF-8 with U+FFFD.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
}

/// The boot information, stored so `sys:/mem` can render the memory map.
static BOOT_INFO: Mutex<Option<&'static BootInfo>> = Mutex::new(None);

/// Descriptor numbers 0, 1 and 2 are the console streams used by `write`.
const FIRST_FD: u64 = 3;
const MAX_FDS: usize = 16;

/// One open descriptor: its never-reused number and the resource it names.
#[derive(Debug)]
struct Entry {
    fd: u64,
    resource: Resource,
}

/// The global descriptor table, shared by the ring-3 programs.
///
/// Descriptor numbers are handed out monotonically and are **never reused**.
/// Closing fd 3 and opening something else produces a different number, so a
/// stale descriptor held by a program can never silently alias a later,
/// unrelated resource. This is the same guarantee Redox gets from its
/// `HandleMap::insert`, which assigns ever-increasing file handles.
struct FdTable {
    entries: [Option<Entry>; MAX_FDS],
    next_fd: u64,
}

static FDS: Mutex<FdTable> = Mutex::new(FdTable {
    entries: [const { None }; MAX_FDS],
    next_fd: FIRST_FD,
});

/// Remembers the boot information for the `sys` scheme.
pub fn init(boot_info: &'static BootInfo) {
    *BOOT_INFO.lock() = Some(boot_info);
}

/// Splits `scheme:/path` into its scheme and path, ignoring leading slashes.
fn split(url: &str) -> Result<(&str, &str), String> {
    let (scheme, path) = url
        .split_once(':')
        .ok_or_else(|| String::from("missing scheme (try file:/README.TXT)"))?;
    Ok((scheme, path.trim_start_matches('/')))
}

/// Opens `url` and snapshots its contents into a [`Resource`].
pub fn open(url: &str) -> Result<Resource, String> {
    let (scheme, path) = split(url)?;
    if path.is_empty() {
        return Err(String::from("empty resource path"));
    }
    if scheme.eq_ignore_ascii_case("file") {
        open_file(path)
    } else if scheme.eq_ignore_ascii_case("net") {
        open_net(path)
    } else if scheme.eq_ignore_ascii_case("sys") {
        open_sys(path)
    } else {
        Err(format!("unknown scheme '{scheme}' (try file, net or sys)"))
    }
}

/// Reads a file from the mounted FAT16 volume.
fn open_file(path: &str) -> Result<Resource, String> {
    let data = crate::fat::with_volume(|volume| volume.read(path))
        .flatten()
        .ok_or_else(|| format!("file not found: {path}"))?;
    let extension = path.rsplit_once('.').map(|(_, extension)| extension);
    let mime = match extension {
        Some(extension) if extension.eq_ignore_ascii_case("TXT") => "text/plain",
        Some(extension) if extension.eq_ignore_ascii_case("WAV") => "audio/wav",
        _ => "application/octet-stream",
    };
    Ok(Resource {
        url: format!("file:/{path}"),
        mime: String::from(mime),
        data,
        cursor: 0,
    })
}

/// Fetches a URL through the kernel's HTTP(S) client.
fn open_net(path: &str) -> Result<Resource, String> {
    let target = if path.contains("://") {
        String::from(path)
    } else {
        format!("https://{path}")
    };
    let mut response = crate::net::fetch_url(&target)?;
    let mime = response
        .header("content-type")
        .unwrap_or("text/html")
        .to_string();
    Ok(Resource {
        url: core::mem::take(&mut response.final_url),
        mime,
        data: core::mem::take(&mut response.body),
        cursor: 0,
    })
}

/// Builds a `sys:` resource from live kernel state.
fn open_sys(path: &str) -> Result<Resource, String> {
    let text = match path {
        "version" => {
            "GTos 0.1.0\nx86_64 bare-metal kernel\nwritten in Rust, no std, no libc\n".to_string()
        }
        "heap" => {
            let (start, size) = crate::memory::info();
            format!(
                "heap: 0x{start:012x} .. 0x{:012x} ({} KiB)\n",
                start + size,
                size / 1024
            )
        }
        "mem" => {
            let guard = BOOT_INFO.lock();
            let Some(info) = guard.as_ref() else {
                return Err(String::from("boot memory map is unavailable"));
            };
            let mut out = String::from("start            end               size  kind\n");
            for region in info.memory_regions.iter() {
                let size = region.end.saturating_sub(region.start);
                let kind = match region.kind {
                    bootloader_api::info::MemoryRegionKind::Usable => "usable",
                    bootloader_api::info::MemoryRegionKind::Bootloader => "bootloader",
                    _ => "other",
                };
                let _ = core::fmt::Write::write_fmt(
                    &mut out,
                    format_args!(
                        "0x{:012x}  0x{:012x}  {:>8} KiB  {kind}\n",
                        region.start,
                        region.end,
                        size / 1024
                    ),
                );
            }
            out
        }
        "net" => {
            let config = crate::net::address();
            let adapter = crate::net::adapter()
                .map(|info| format!("adapter: {}, {}, IRQ {}\n", info.name, info.bus, info.irq))
                .unwrap_or_default();
            format!(
                "{adapter}address: {}\nmask:    {}\ngateway: {}\ndns:     {}\nsource:  {}\n",
                ip(config.ip),
                ip(config.netmask),
                ip(config.gateway),
                ip(config.dns),
                if config.dhcp {
                    "DHCP lease"
                } else {
                    "static fallback"
                },
            )
        }
        "uptime" => format!(
            "uptime: {} s ({} timer ticks)\n",
            crate::interrupts::uptime_seconds(),
            crate::interrupts::ticks()
        ),
        "help" | "schemes" => String::from(
            "schemes:\n  file:/NAME       file on the FAT16 data disk\n  \
             net:/URL          HTTP or HTTPS resource\n  \
             sys:/version      kernel version\n  \
             sys:/heap         kernel heap range\n  \
             sys:/mem          bootloader memory map\n  \
             sys:/net          network configuration\n  \
             sys:/uptime       time since boot\n  \
             sys:/help         this list\n",
        ),
        other => return Err(format!("unknown sys resource '{other}' (try sys:/help)")),
    };
    Ok(Resource {
        url: format!("sys:/{path}"),
        mime: String::from("text/plain"),
        data: text.into_bytes(),
        cursor: 0,
    })
}

/// Formats an IPv4 address without pulling in a formatting dependency.
fn ip(bytes: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3])
}

/// Opens a resource and installs it in the next free descriptor slot.
pub fn open_fd(url: &str) -> Result<u64, String> {
    let resource = open(url)?;
    let mut table = FDS.lock();
    let slot = table
        .entries
        .iter()
        .position(Option::is_none)
        .ok_or_else(|| String::from("too many open resources"))?;

    // Take the next number, skipping any still in use. With only MAX_FDS live
    // descriptors and a u64 counter this loop terminates even if the counter
    // ever wraps (which would need 2^64 opens).
    let mut fd = table.next_fd;
    while table
        .entries
        .iter()
        .any(|entry| entry.as_ref().is_some_and(|entry| entry.fd == fd))
    {
        fd = fd.wrapping_add(1);
    }
    table.next_fd = fd.wrapping_add(1);
    table.entries[slot] = Some(Entry { fd, resource });
    Ok(fd)
}

/// Reads up to `out.len()` bytes from a descriptor, advancing its cursor.
pub fn read_fd(fd: u64, out: &mut [u8]) -> Result<usize, String> {
    let mut table = FDS.lock();
    let entry = table
        .entries
        .iter_mut()
        .filter_map(Option::as_mut)
        .find(|entry| entry.fd == fd)
        .ok_or_else(|| String::from("bad file descriptor"))?;
    let resource = &mut entry.resource;
    let start = resource.cursor;
    if start >= resource.data.len() {
        return Ok(0);
    }
    let end = start.saturating_add(out.len()).min(resource.data.len());
    let count = end - start;
    out[..count].copy_from_slice(&resource.data[start..end]);
    resource.cursor = end;
    Ok(count)
}

/// Closes a descriptor, returning whether it was open.
pub fn close_fd(fd: u64) -> bool {
    let mut table = FDS.lock();
    let slot = table
        .entries
        .iter_mut()
        .find(|slot| slot.as_ref().is_some_and(|entry| entry.fd == fd));
    slot.is_some_and(|slot| slot.take().is_some())
}

/// Releases every descriptor still held when a user program exits.
///
/// The table is global, so a program that opens resources without closing them
/// would otherwise exhaust the slots for every program that runs after it.
pub fn close_all_fds() {
    let mut table = FDS.lock();
    for slot in &mut table.entries {
        *slot = None;
    }
    // `next_fd` is deliberately not reset: descriptors are never reused, even
    // between different user programs.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn splits_scheme_urls() {
        assert_eq!(split("file:/README.TXT").unwrap(), ("file", "README.TXT"));
        assert_eq!(
            split("net:/http://example.com").unwrap(),
            ("net", "http://example.com")
        );
        assert!(split("README.TXT").is_err());
    }

    #[test_case]
    fn sys_resources_are_readable() {
        let version = open("sys:/version").unwrap();
        assert!(version.text().contains("GTos"));
        assert!(open("sys:/help").unwrap().text().contains("file:/"));
        assert!(open("bogus:/x").is_err());
    }

    #[test_case]
    fn descriptor_lifecycle() {
        let fd = open_fd("sys:/version").unwrap();
        assert!(fd >= FIRST_FD);
        let mut buffer = [0u8; 16];
        let count = read_fd(fd, &mut buffer).unwrap();
        assert!(count > 0);
        assert_eq!(&buffer[..5], b"GTos ");
        assert!(close_fd(fd));
        assert!(!close_fd(fd));
        assert!(read_fd(fd, &mut buffer).is_err());
    }

    #[test_case]
    fn descriptors_are_never_reused() {
        let first = open_fd("sys:/version").unwrap();
        assert!(close_fd(first));

        // A later open must not hand out `first` again: a stale reference to a
        // closed descriptor has to stay invalid, not silently read a new
        // resource that happens to land in the same slot.
        let second = open_fd("sys:/net").unwrap();
        assert_ne!(first, second);

        let mut buffer = [0u8; 8];
        assert!(read_fd(first, &mut buffer).is_err());
        assert!(read_fd(second, &mut buffer).is_ok());
        assert!(close_fd(second));
    }
}
