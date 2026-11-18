//! The interactive shell and the boot-time log helpers.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use core::fmt;

use bootloader_api::info::{BootInfo, MemoryRegionKind};

use crate::console::{print, println};
use crate::{ata, audio, console, fat, input, interrupts, memory, net, task, usermode};

const PROMPT: &str = "rustos> ";
const BANNER_WIDTH: usize = 62;

/// Prints `count` copies of `ch` without allocating.
fn fill(ch: char, count: usize) {
    for _ in 0..count {
        print!("{ch}");
    }
}

/// Draws the boot banner.
///
/// Deliberately allocation-free: it is printed before the heap is usable and by
/// the `logo` command, so `str::repeat` is not an option here.
pub fn banner() {
    console::set_fg(console::DIM);
    print!("+");
    fill('-', BANNER_WIDTH);
    println!("+");

    let right = "x86_64 bare metal";
    let pad = BANNER_WIDTH.saturating_sub(4 + "RUSTOS".len() + right.len());
    print!("|  ");
    console::set_fg(console::CYAN);
    print!("RUSTOS");
    console::set_fg(console::FG);
    fill(' ', pad);
    println!("{right}  |");

    let subtitle = "a small operating system written in Rust";
    let pad = BANNER_WIDTH.saturating_sub(4 + subtitle.len());
    print!("|  {subtitle}");
    fill(' ', pad);
    println!("  |");

    console::set_fg(console::DIM);
    print!("+");
    fill('-', BANNER_WIDTH);
    println!("+");
    console::set_fg(console::FG);
}

/// Prints a `[ ok ] message` / `[ !! ] message` boot log line.
///
/// Takes `fmt::Arguments` so boot logging never needs the heap.
pub fn status(ok: bool, message: fmt::Arguments<'_>) {
    console::set_fg(console::DIM);
    print!("  [ ");
    console::set_fg(if ok { console::GREEN } else { console::RED });
    print!("{}", if ok { "ok" } else { "!!" });
    console::set_fg(console::DIM);
    print!(" ] ");
    console::set_fg(console::FG);
    println!("{message}");
}

pub fn run(boot_info: &'static BootInfo) -> ! {
    println!();
    println!("Type 'help' for the list of commands.");
    println!();

    let mut line = String::new();
    loop {
        console::set_fg(console::GREEN);
        print!("{PROMPT}");
        console::set_fg(console::FG);
        line.clear();
        if read_line(&mut line) {
            execute(&line, boot_info);
        }
    }
}

fn wait_for_input() -> u8 {
    loop {
        // Post-12: the keyboard interrupt only queues raw scancodes, so let the
        // async decoding task run before looking for input.
        task::poll();
        if let Some(byte) = input::pop() {
            return byte;
        }
        // Sleep until the next interrupt instead of burning the CPU.
        x86_64::instructions::hlt();
    }
}

/// Reads one line, echoing it as the user types. Returns `false` if the line
/// was cancelled with Ctrl-C.
fn read_line(line: &mut String) -> bool {
    loop {
        let byte = wait_for_input();
        match byte {
            b'\r' | b'\n' => {
                // Terminals and test harnesses disagree on CR vs LF; swallow a
                // trailing LF/CR so "cmd\r\n" is not two commands.
                if let Some(b'\r' | b'\n') = input::peek() {
                    input::pop();
                }
                println!();
                return true;
            }
            0x08 | 0x7f => {
                if line.pop().is_some() {
                    print!("\u{8} \u{8}");
                }
            }
            0x03 => {
                println!("^C");
                line.clear();
                return false;
            }
            0x0c => {
                console::clear_screen();
                print!("{PROMPT}");
            }
            0x20..=0x7e if line.len() < 200 => {
                line.push(byte as char);
                print!("{}", byte as char);
            }
            _ => {}
        }
    }
}

pub fn execute(line: &str, boot_info: &BootInfo) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }

    let (command, rest) = match line.split_once(' ') {
        Some((command, rest)) => (command, rest.trim()),
        None => (line, ""),
    };

    match command {
        "help" => help(),
        "echo" => println!("{rest}"),
        "clear" => console::clear_screen(),
        "mem" => memory_map(boot_info),
        "heap" => heap_info(),
        "alloc" => alloc_test(),
        "uptime" => uptime(),
        "cpu" => cpu(),
        "logo" => {
            banner();
            println!();
        }
        "about" => about(),
        "net" => net_info(),
        "dhcp" => dhcp_command(),
        "ping" => ping(rest),
        "dns" => dns(rest),
        "http" => http(rest),
        "beep" => beep(),
        "tone" => tone(rest),
        "disks" => disks(),
        "ls" | "dir" => ls(),
        "cat" | "type" => cat(rest),
        "write" => write_file(rest),
        "rm" | "del" => rm(rest),
        "play" => play(rest),
        "format" => format_disk(),
        "run" => run_program(rest),
        "panic" => panic!("deliberate panic requested by the shell"),
        "reboot" => reboot(),
        "shutdown" | "poweroff" | "exit" => shutdown(),
        "pkg" => package_manager(rest, boot_info),
        other => {
            console::set_fg(console::RED);
            println!("unknown command: {other}");
            console::set_fg(console::FG);
            println!("try 'help'");
        }
    }
}

// ---------------------------------------------------------------------------
// Package manager
// ---------------------------------------------------------------------------

/// A simple package registry. In a real OS this would be backed by a
/// filesystem and a remote repository; here the packages are built in.
struct Package {
    name: &'static str,
    version: &'static str,
    description: &'static str,
    installed: bool,
}

static mut PACKAGES: [Package; 8] = [
    Package {
        name: "terminal",
        version: "1.0",
        description: "Terminal emulator window",
        installed: true,
    },
    Package {
        name: "about",
        version: "1.0",
        description: "About RustOS information window",
        installed: true,
    },
    Package {
        name: "mycomputer",
        version: "1.0",
        description: "System information and drives",
        installed: true,
    },
    Package {
        name: "mydocuments",
        version: "1.0",
        description: "Document browser",
        installed: true,
    },
    Package {
        name: "browser",
        version: "1.0",
        description: "Web browser (HTTP over the RTL8139 NIC)",
        installed: true,
    },
    Package {
        name: "controlpanel",
        version: "1.0",
        description: "System control panel",
        installed: true,
    },
    Package {
        name: "cmd",
        version: "1.0",
        description: "Command prompt window",
        installed: true,
    },
    Package {
        name: "calculator",
        version: "1.0",
        description: "Calculator (not yet implemented)",
        installed: false,
    },
];

#[allow(clippy::manual_find, clippy::needless_range_loop)]
fn find_package(name: &str) -> Option<usize> {
    for i in 0..8 {
        // SAFETY: single-threaded, no concurrent access.
        if unsafe { PACKAGES[i].name } == name {
            return Some(i);
        }
    }
    None
}

fn package_manager(args: &str, _boot_info: &BootInfo) {
    let args = args.trim();
    if args.is_empty() || args == "list" {
        println!("Installed packages:");
        #[allow(clippy::needless_range_loop)]
        for i in 0..8 {
            let (name, version, desc, installed) = unsafe {
                (
                    PACKAGES[i].name,
                    PACKAGES[i].version,
                    PACKAGES[i].description,
                    PACKAGES[i].installed,
                )
            };
            if installed {
                console::set_fg(console::GREEN);
                print!("  [installed] ");
            } else {
                console::set_fg(console::DIM);
                print!("  [available] ");
            }
            console::set_fg(console::CYAN);
            print!("{name}");
            console::set_fg(console::DIM);
            println!(" v{version} - {desc}");
        }
        console::set_fg(console::FG);
        println!();
        println!("Use 'pkg install <name>' or 'pkg remove <name>'.");
    } else if let Some(name) = args.strip_prefix("install ") {
        let name = name.trim();
        if let Some(i) = find_package(name) {
            unsafe {
                if PACKAGES[i].installed {
                    println!("{name} is already installed.");
                } else {
                    PACKAGES[i].installed = true;
                    println!("Installed {name} v{}.", PACKAGES[i].version);
                }
            }
        } else {
            console::set_fg(console::RED);
            println!("Unknown package: {name}");
            console::set_fg(console::FG);
        }
    } else if let Some(name) = args.strip_prefix("remove ") {
        let name = name.trim();
        if let Some(i) = find_package(name) {
            unsafe {
                if PACKAGES[i].installed {
                    PACKAGES[i].installed = false;
                    println!("Removed {name}.");
                } else {
                    println!("{name} is not installed.");
                }
            }
        } else {
            console::set_fg(console::RED);
            println!("Unknown package: {name}");
            console::set_fg(console::FG);
        }
    } else if let Some(name) = args.strip_prefix("info ") {
        let name = name.trim();
        if let Some(i) = find_package(name) {
            let (version, desc, installed) = unsafe {
                (
                    PACKAGES[i].version,
                    PACKAGES[i].description,
                    PACKAGES[i].installed,
                )
            };
            println!("Package: {name}");
            println!("Version: {version}");
            println!("Description: {desc}");
            println!(
                "Status: {}",
                if installed { "installed" } else { "available" }
            );
        } else {
            console::set_fg(console::RED);
            println!("Unknown package: {name}");
            console::set_fg(console::FG);
        }
    } else {
        console::set_fg(console::RED);
        println!("Usage: pkg [list|install <name>|remove <name>|info <name>]");
        console::set_fg(console::FG);
    }
}

fn help() {
    const COMMANDS: &[(&str, &str)] = &[
        ("help", "show this list"),
        ("echo <text>", "print text back"),
        ("clear", "clear the screen"),
        ("mem", "dump the bootloader memory map"),
        ("heap", "show the kernel heap range"),
        ("alloc", "allocate on the heap to test the allocator"),
        ("uptime", "time since boot, driven by the PIT"),
        ("cpu", "CPUID vendor and model"),
        ("logo", "redraw the banner"),
        ("about", "what this OS is"),
        ("net", "show the network adapter and address"),
        ("dhcp", "get an address from a DHCP server"),
        ("ping <ip>", "send an ICMP echo request"),
        ("dns <name>", "resolve a host name"),
        ("http <url>", "fetch a page over HTTP"),
        ("beep", "play a short tone through AC'97 audio"),
        ("tone <hz> [ms]", "play a tone of the given frequency"),
        ("disks", "list the ATA drives and their sizes"),
        ("ls", "list the FAT16 data disk"),
        ("cat <file>", "print a file from the data disk"),
        (
            "write <file> [text]",
            "create a file (no text: end with a line of '.')",
        ),
        ("rm <file>", "delete a file from the data disk"),
        ("play <file>", "play a WAV file through AC'97 audio"),
        ("format", "reformat the FAT16 data disk"),
        (
            "run [file]",
            "run a ring-3 program (no file: the built-in demo)",
        ),
        ("panic", "trigger a kernel panic (for testing)"),
        ("reboot", "reboot the machine"),
        ("shutdown", "power off (QEMU) or halt"),
        ("pkg list", "list available packages"),
        ("pkg install <name>", "install a package"),
        ("pkg remove <name>", "remove a package"),
        ("pkg info <name>", "show package info"),
    ];

    println!("Commands:");
    for (name, description) in COMMANDS {
        console::set_fg(console::CYAN);
        print!("  {name:<18}");
        console::set_fg(console::DIM);
        println!("{description}");
        console::set_fg(console::FG);
    }
}

fn memory_map(boot_info: &BootInfo) {
    println!("{:<16} {:<16} {:>12}  kind", "start", "end", "size");

    let mut usable_bytes = 0u64;

    for region in boot_info.memory_regions.iter() {
        let size = region.end.saturating_sub(region.start);
        let kind = match region.kind {
            MemoryRegionKind::Usable => {
                usable_bytes += size;
                "usable"
            }
            MemoryRegionKind::Bootloader => "bootloader",
            MemoryRegionKind::UnknownUefi(tag) => {
                println!(
                    "0x{:012x}   0x{:012x}   {:>8} KiB  uefi-{}",
                    region.start,
                    region.end,
                    size / 1024,
                    tag
                );
                continue;
            }
            MemoryRegionKind::UnknownBios(tag) => {
                println!(
                    "0x{:012x}   0x{:012x}   {:>8} KiB  bios-{}",
                    region.start,
                    region.end,
                    size / 1024,
                    tag
                );
                continue;
            }
            _ => "unknown",
        };

        let color = if region.kind == MemoryRegionKind::Usable {
            console::GREEN
        } else {
            console::DIM
        };
        print!(
            "0x{:012x}   0x{:012x}   {:>8} KiB  ",
            region.start,
            region.end,
            size / 1024
        );
        console::set_fg(color);
        println!("{kind}");
        console::set_fg(console::FG);
    }

    println!();
    println!(
        "{} regions, {}.{} MiB usable",
        boot_info.memory_regions.len(),
        usable_bytes / (1024 * 1024),
        (usable_bytes % (1024 * 1024)) * 10 / (1024 * 1024)
    );
}

fn heap_info() {
    let (start, size) = memory::info();
    println!(
        "heap: 0x{start:012x} .. 0x{:012x} ({} KiB)",
        start + size,
        size / 1024
    );
}

fn alloc_test() {
    let values: Vec<u64> = (0..10_000).collect();
    let sum = values.iter().copied().sum::<u64>();
    println!(
        "allocated {} KiB on the heap, {} elements, checksum {}",
        values.len() * core::mem::size_of::<u64>() / 1024,
        values.len(),
        sum
    );
    drop(values);
    println!("heap allocation and free succeeded");
}

fn uptime() {
    let ticks = interrupts::ticks();
    println!(
        "uptime: {} s ({} ticks at {} Hz)",
        interrupts::uptime_seconds(),
        ticks,
        interrupts::TICK_HZ
    );
}

fn cpu() {
    println!("vendor: {}", cpuid_vendor());
    match cpuid_brand() {
        Some(brand) => println!("model:  {brand}"),
        None => println!("model:  (not reported by CPUID)"),
    }
    println!("mode:   x86_64 long mode, ring 0");
}

fn cpuid_vendor() -> String {
    let result = core::arch::x86_64::__cpuid(0);
    let mut bytes = [0u8; 12];
    bytes[0..4].copy_from_slice(&result.ebx.to_le_bytes());
    bytes[4..8].copy_from_slice(&result.edx.to_le_bytes());
    bytes[8..12].copy_from_slice(&result.ecx.to_le_bytes());
    String::from_utf8_lossy(&bytes).trim().into()
}

fn cpuid_brand() -> Option<String> {
    let max_leaf = core::arch::x86_64::__cpuid(0x8000_0000).eax;
    if max_leaf < 0x8000_0004 {
        return None;
    }

    let mut bytes = [0u8; 48];
    for leaf in 0..3u32 {
        let result = core::arch::x86_64::__cpuid(0x8000_0002 + leaf);
        let offset = leaf as usize * 16;
        bytes[offset..offset + 4].copy_from_slice(&result.eax.to_le_bytes());
        bytes[offset + 4..offset + 8].copy_from_slice(&result.ebx.to_le_bytes());
        bytes[offset + 8..offset + 12].copy_from_slice(&result.ecx.to_le_bytes());
        bytes[offset + 12..offset + 16].copy_from_slice(&result.edx.to_le_bytes());
    }

    let brand = String::from_utf8_lossy(&bytes);
    let brand = brand.trim_matches(|ch: char| ch == '\0' || ch == ' ');
    if brand.is_empty() {
        None
    } else {
        Some(brand.into())
    }
}

fn about() {
    println!("RustOS is a small x86_64 kernel written entirely in Rust.");
    println!("It boots via the rust-osdev bootloader (BIOS or UEFI), owns the");
    println!("framebuffer, heap, IDT, PIC and PIT, and serves this shell over");
    println!("the PS/2 keyboard and COM1. No Linux, no std, no libc.");
}

// ---------------------------------------------------------------------------
// Networking
// ---------------------------------------------------------------------------

/// Formats a dotted-quad IPv4 address.
fn format_ip(address: [u8; 4]) -> String {
    format!(
        "{}.{}.{}.{}",
        address[0], address[1], address[2], address[3]
    )
}

/// Prints a hint when a network command runs without an adapter.
///
/// Returns `true` if the caller should stop.
fn require_network() -> bool {
    if net::is_up() {
        return false;
    }
    console::set_fg(console::RED);
    println!("no network adapter is up");
    console::set_fg(console::FG);
    println!("boot QEMU with: -nic user,model=rtl8139");
    true
}

fn net_info() {
    if require_network() {
        return;
    }
    let (io, irq) = net::adapter().unwrap_or((0, 0));
    let config = net::address();
    println!("adapter: RTL8139, I/O 0x{io:04x}, IRQ {irq}");
    println!("address: {}", format_ip(net::ip()));
    println!("mask:    {}", format_ip(config.netmask));
    println!(
        "mac:     {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        net::MAC[0],
        net::MAC[1],
        net::MAC[2],
        net::MAC[3],
        net::MAC[4],
        net::MAC[5]
    );
    println!("gateway: {}", format_ip(net::gateway()));
    println!("dns:     {}", format_ip(net::dns()));
    println!(
        "source:  {}",
        if config.dhcp {
            "DHCP lease"
        } else {
            "static fallback"
        }
    );
}

/// `dhcp`: (re)runs the DHCP client and reports the lease.
fn dhcp_command() {
    if require_network() {
        return;
    }
    print!("DHCP ... ");
    if net::dhcp() {
        console::set_fg(console::GREEN);
        println!("lease acquired");
        console::set_fg(console::FG);
        println!("address: {}", format_ip(net::ip()));
        println!("mask:    {}", format_ip(net::address().netmask));
        println!("gateway: {}", format_ip(net::gateway()));
        println!("dns:     {}", format_ip(net::dns()));
    } else {
        console::set_fg(console::RED);
        println!("no DHCP server replied");
        console::set_fg(console::FG);
    }
}

fn ping(rest: &str) {
    if rest.is_empty() {
        console::set_fg(console::RED);
        println!("usage: ping <ipv4 address>");
        console::set_fg(console::FG);
        return;
    }
    if require_network() {
        return;
    }
    let Some(address) = net::parse_address(rest) else {
        console::set_fg(console::RED);
        println!("invalid address: {rest}");
        console::set_fg(console::FG);
        return;
    };

    print!("PING {rest} ... ");
    if let Some(milliseconds) = net::ping(address) {
        console::set_fg(console::GREEN);
        println!("reply in {milliseconds} ms");
    } else {
        console::set_fg(console::RED);
        println!("request timed out");
    }
    console::set_fg(console::FG);
}

fn dns(rest: &str) {
    if rest.is_empty() {
        console::set_fg(console::RED);
        println!("usage: dns <host name>");
        console::set_fg(console::FG);
        return;
    }
    if require_network() {
        return;
    }

    print!("resolving {rest} ... ");
    if let Some(address) = net::resolve(rest) {
        console::set_fg(console::GREEN);
        println!("{rest} -> {}", format_ip(address));
    } else {
        console::set_fg(console::RED);
        println!("failed");
    }
    console::set_fg(console::FG);
}

fn http(rest: &str) {
    let rest = rest.trim();
    if rest.is_empty() {
        console::set_fg(console::RED);
        println!("usage: http <url>   e.g. https://example.com/");
        console::set_fg(console::FG);
        return;
    }
    if require_network() {
        return;
    }

    let Some(url) = net::parse_url(rest) else {
        console::set_fg(console::RED);
        println!("invalid URL '{rest}'");
        console::set_fg(console::FG);
        return;
    };
    console::set_fg(console::DIM);
    println!("GET {}://{}{}", url.scheme, url.host, url.path);
    console::set_fg(console::FG);

    match net::fetch_url(rest) {
        Ok(response) => {
            console::set_fg(console::DIM);
            println!("final URL: {}", response.final_url);
            console::set_fg(console::FG);
            print_response(&response.raw_text());
        }
        Err(error) => {
            console::set_fg(console::RED);
            println!("{error}");
            console::set_fg(console::FG);
        }
    }
}

/// Prints an HTTP response: the status line and headers, then a capped body.
fn print_response(response: &str) {
    // Cap the body so a large page cannot flood the console.
    const LIMIT: usize = 4000;

    let (head, body) = match response.split_once("\r\n\r\n") {
        Some((head, body)) => (head, body),
        None => (response, ""),
    };

    console::set_fg(console::CYAN);
    for line in head.lines() {
        println!("{line}");
    }
    console::set_fg(console::FG);
    println!();

    // Iterate by `char` so the cut never lands inside a UTF-8 sequence.
    let body = body.trim_start_matches(['\r', '\n']);
    let mut shown = 0usize;
    for ch in body.chars() {
        if shown >= LIMIT {
            console::set_fg(console::DIM);
            println!("\n... ({} more bytes)", body.len() - shown);
            console::set_fg(console::FG);
            break;
        }
        print!("{ch}");
        shown += ch.len_utf8();
    }
    println!();
}

// ---------------------------------------------------------------------------
// Disks (ATA + FAT16)
// ---------------------------------------------------------------------------

/// Prints `message` in red on its own line.
fn error(message: fmt::Arguments<'_>) {
    console::set_fg(console::RED);
    println!("{message}");
    console::set_fg(console::FG);
}

/// Whether a FAT16 volume is mounted, complaining if not.
fn require_volume() -> bool {
    if fat::with_volume(|_| ()).is_some() {
        return true;
    }
    error(format_args!("no FAT16 volume is mounted"));
    println!("boot QEMU with a data disk (the launcher attaches one automatically)");
    false
}

/// `disks`: lists the ATA drives.
fn disks() {
    let mut any = false;
    for index in 0..ata::DRIVES {
        if !ata::present(index) {
            continue;
        }
        any = true;
        let model = ata::model(index).unwrap_or_default();
        let role = if index == 0 { "master" } else { "slave" };
        println!(
            "drive {index} ({role}): {} MiB, {} sectors",
            ata::sectors(index) / 2048,
            ata::sectors(index)
        );
        if !model.is_empty() {
            println!("  model: {model}");
        }
    }
    if !any {
        error(format_args!("no ATA drives detected"));
    }
}

/// `ls`: lists the root directory of the data disk.
fn ls() {
    if !require_volume() {
        return;
    }
    let entries = fat::with_volume(fat::Volume::list).unwrap_or_default();
    if entries.is_empty() {
        println!("(the volume is empty)");
        return;
    }
    println!("{:>10}  name", "size");
    for entry in &entries {
        if entry.is_directory() {
            println!("{:>10}  {}/", "<DIR>", entry.name);
        } else {
            println!("{:>10}  {}", entry.size, entry.name);
        }
    }
    println!("{} entries", entries.len());
}

/// `cat <file>`: prints a file, mapping control bytes to a visible form.
fn cat(name: &str) {
    if name.is_empty() {
        error(format_args!("usage: cat <file>"));
        return;
    }
    if !require_volume() {
        return;
    }
    let Some(data) = fat::with_volume(|volume| volume.read(name)).flatten() else {
        error(format_args!("no such file: {name}"));
        return;
    };

    let mut line = String::new();
    for &byte in &data {
        match byte {
            b'\r' => {}
            b'\n' => {
                println!("{line}");
                line.clear();
            }
            b'\t' => line.push_str("    "),
            0x20..=0x7e => line.push(byte as char),
            _ => line.push('.'),
        }
    }
    if !line.is_empty() {
        println!("{line}");
    }
    println!("({} bytes)", data.len());
}

/// `write <file> [text]`: creates or replaces a file.
///
/// With inline text the file holds that one line; without it the shell reads
/// lines until a line containing only `.` is entered.
fn write_file(rest: &str) {
    let (name, inline) = match rest.split_once(' ') {
        Some((name, text)) => (name, Some(text.trim_end_matches('\r'))),
        None => (rest, None),
    };
    if !fat::valid_name(name) {
        error(format_args!(
            "invalid file name: {name} (use 8.3, e.g. HELLO.TXT)"
        ));
        return;
    }
    if !require_volume() {
        return;
    }

    let mut content = String::new();
    if let Some(text) = inline {
        content.push_str(text);
        content.push_str("\r\n");
    } else {
        println!("type the file, end with a line containing only '.'");
        loop {
            let line = prompt_line();
            if line == "." {
                break;
            }
            content.push_str(&line);
            content.push_str("\r\n");
        }
    }

    if fat::with_volume(|volume| volume.write(name, content.as_bytes())).unwrap_or(false) {
        console::set_fg(console::GREEN);
        println!("wrote {} bytes to {name}", content.len());
        console::set_fg(console::FG);
    } else {
        error(format_args!("could not write {name} (disk full?)"));
    }
}

/// `rm <file>`: deletes a file.
fn rm(name: &str) {
    if name.is_empty() {
        error(format_args!("usage: rm <file>"));
        return;
    }
    if !require_volume() {
        return;
    }
    match fat::with_volume(|volume| volume.delete(name)) {
        Some(true) => {
            console::set_fg(console::GREEN);
            println!("deleted {name}");
            console::set_fg(console::FG);
        }
        _ => error(format_args!("no such file: {name}")),
    }
}

/// `play <file>`: plays a 16-bit PCM WAV file through AC'97.
fn play(name: &str) {
    if name.is_empty() {
        error(format_args!("usage: play <file>"));
        return;
    }
    if !audio::is_up() {
        error(format_args!("no audio device is up"));
        println!("boot QEMU with: -device AC97,audiodev=snd0");
        return;
    }
    if !require_volume() {
        return;
    }
    let Some(data) = fat::with_volume(|volume| volume.read(name)).flatten() else {
        error(format_args!("no such file: {name}"));
        return;
    };

    print!("playing {name} ({} bytes) ... ", data.len());
    if audio::play_wav(&data) {
        console::set_fg(console::GREEN);
        println!("done");
    } else {
        console::set_fg(console::RED);
        println!("not a supported 16-bit PCM WAV");
    }
    console::set_fg(console::FG);
}

/// `format`: rebuilds an empty FAT16 volume on the data disk.
fn format_disk() {
    let drive = 1;
    if !ata::present(drive) {
        error(format_args!("no data disk on drive {drive}"));
        return;
    }
    print!("formatting drive {drive} ... ");
    if fat::format(drive) && fat::mount_data_disk(drive) {
        console::set_fg(console::GREEN);
        println!("done");
    } else {
        console::set_fg(console::RED);
        println!("failed");
    }
    console::set_fg(console::FG);
}

// ---------------------------------------------------------------------------
// User mode
// ---------------------------------------------------------------------------

/// `run [file]`: runs a flat ring-3 binary, or the built-in demo program.
///
/// The binary must be linked against the kernel's user base (see
/// [`usermode::USER_BASE`]); `HELLO.BIN` on the data disk is one such program.
fn run_program(name: &str) {
    if !usermode::is_ready() {
        error(format_args!("ring 3 is not initialized"));
        return;
    }

    let (label, program) = if name.is_empty() {
        ("demo program", usermode::demo_program())
    } else {
        if !require_volume() {
            return;
        }
        let Some(data) = fat::with_volume(|volume| volume.read(name)).flatten() else {
            error(format_args!("no such file: {name}"));
            return;
        };
        (name, data)
    };

    if program.is_empty() {
        error(format_args!("{label} is empty"));
        return;
    }
    if program.len() > usermode::MAX_PROGRAM_BYTES {
        error(format_args!(
            "{label} is {} bytes, the limit is {}",
            program.len(),
            usermode::MAX_PROGRAM_BYTES
        ));
        return;
    }

    println!("running {label} ({} bytes) in ring 3 ...", program.len());
    let code = usermode::run(&program);
    console::set_fg(console::GREEN);
    println!("{label} exited with status {code}");
    console::set_fg(console::FG);
}

/// Reads one more line from the console, used by `write`.
fn prompt_line() -> String {
    console::set_fg(console::DIM);
    print!("> ");
    console::set_fg(console::FG);
    let mut line = String::new();
    read_line(&mut line);
    line
}

// ---------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------

/// Plays a short attention tone.
fn beep() {
    if !audio::is_up() {
        console::set_fg(console::RED);
        println!("no audio device is up");
        console::set_fg(console::FG);
        println!("boot QEMU with: -device AC97,audiodev=snd0");
        return;
    }
    print!("beep ... ");
    if audio::beep() {
        console::set_fg(console::GREEN);
        println!("played 880 Hz for 160 ms");
    } else {
        console::set_fg(console::RED);
        println!("playback failed");
    }
    console::set_fg(console::FG);
}

/// `tone <hz> [milliseconds]`: plays a single tone.
fn tone(rest: &str) {
    if rest.is_empty() {
        console::set_fg(console::RED);
        println!("usage: tone <hz> [milliseconds]");
        console::set_fg(console::FG);
        return;
    }
    let mut parts = rest.split_whitespace();
    let Some(frequency) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
        console::set_fg(console::RED);
        println!("invalid frequency: {rest}");
        console::set_fg(console::FG);
        return;
    };
    let milliseconds = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(200)
        .clamp(1, 5000);

    if !audio::is_up() {
        console::set_fg(console::RED);
        println!("no audio device is up");
        console::set_fg(console::FG);
        println!("boot QEMU with: -device AC97,audiodev=snd0");
        return;
    }

    print!("tone {frequency} Hz for {milliseconds} ms ... ");
    if audio::tone(frequency, milliseconds) {
        console::set_fg(console::GREEN);
        println!("played");
    } else {
        console::set_fg(console::RED);
        println!("playback failed");
    }
    console::set_fg(console::FG);
}

fn reboot() -> ! {
    println!("Rebooting...");
    // Pulse the keyboard controller's reset line; the classic PC reset.
    unsafe { x86_64::instructions::port::Port::<u8>::new(0x64).write(0xfe) };
    loop {
        x86_64::instructions::interrupts::disable();
        x86_64::instructions::hlt();
    }
}

pub fn shutdown() -> ! {
    println!("Powering off (QEMU isa-debug-exit); halting if unsupported...");
    // QEMU's isa-debug-exit device: writing 0x10 here exits with status
    // (0x10 << 1) | 1. On real hardware the write is harmless.
    unsafe { x86_64::instructions::port::Port::<u32>::new(0xf4).write(0x10) };
    loop {
        x86_64::instructions::interrupts::disable();
        x86_64::instructions::hlt();
    }
}
