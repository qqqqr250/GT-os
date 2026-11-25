//! The interactive shell and the boot-time log helpers.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use core::fmt;

use bootloader_api::info::{BootInfo, MemoryRegionKind};
use spin::Mutex;

use crate::console::{print, println};
use crate::{ata, audio, console, fat, input, interrupts, memory, net, pkg, task, usermode, vfs};

const PROMPT: &str = "gtos> ";
const BANNER_WIDTH: usize = 62;

/// How many command lines the Ion-style history keeps.
const HISTORY_LIMIT: usize = 32;

/// Oldest-to-newest command history, shared by the serial and GUI shells.
static HISTORY: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// `Some(index)` while the user is browsing history, `None` while editing a
/// fresh line.
static HISTORY_CURSOR: Mutex<Option<usize>> = Mutex::new(None);
/// The half-typed line to restore when browsing past the newest entry.
static HISTORY_DRAFT: Mutex<String> = Mutex::new(String::new());
/// The shell's own VFS working directory. Empty means `/`.
static SHELL_CWD: Mutex<String> = Mutex::new(String::new());

/// Commands offered by Tab. File names are completed separately in [`complete`].
const COMMAND_NAMES: &[&str] = &[
    "help", "echo", "clear", "mem", "heap", "alloc", "uptime", "cpu", "logo", "about", "net",
    "dhcp", "ping", "dns", "http", "open", "schemes", "cd", "pwd", "stat", "mounts", "beep",
    "tone", "disks", "ls", "dir", "cat", "type", "write", "rm", "del", "play", "format", "run",
    "session", "panic", "reboot", "shutdown", "poweroff", "exit", "pkg",
];

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
    let pad = BANNER_WIDTH.saturating_sub(4 + "GTOS".len() + right.len());
    print!("|  ");
    console::set_fg(console::CYAN);
    print!("GTOS");
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

/// Forgets the current browsing position. Called after Enter, Ctrl-C and a new
/// command, so Up always starts from the newest entry.
pub fn history_reset() {
    *HISTORY_CURSOR.lock() = None;
    HISTORY_DRAFT.lock().clear();
}

/// Records a command after it has been submitted. Consecutive duplicates are
/// collapsed, just like a typical Unix shell.
pub fn history_push(line: &str) {
    let command = line.trim();
    history_reset();
    if command.is_empty() {
        return;
    }
    let mut history = HISTORY.lock();
    if history.last().is_some_and(|last| last == command) {
        return;
    }
    history.push(String::from(command));
    if history.len() > HISTORY_LIMIT {
        history.remove(0);
    }
}

/// Recalls the previous command, saving `current` so Down can restore it.
pub fn history_prev(current: &str) -> Option<String> {
    let history = HISTORY.lock();
    if history.is_empty() {
        return None;
    }
    let mut cursor = HISTORY_CURSOR.lock();
    let index = if let Some(index) = *cursor {
        index.saturating_sub(1)
    } else {
        *HISTORY_DRAFT.lock() = String::from(current);
        history.len() - 1
    };
    *cursor = Some(index);
    history.get(index).cloned()
}

/// Walks forward through history, returning the saved draft at the end.
pub fn history_next() -> Option<String> {
    let history = HISTORY.lock();
    let mut cursor = HISTORY_CURSOR.lock();
    let index = (*cursor)?;
    if index + 1 < history.len() {
        let next = index + 1;
        *cursor = Some(next);
        history.get(next).cloned()
    } else {
        *cursor = None;
        Some(HISTORY_DRAFT.lock().clone())
    }
}

/// Completes a command name, or a file name for file-oriented commands.
///
/// Returns the fully rewritten line only when there is exactly one candidate,
/// so an ambiguous prefix never silently changes what the user typed.
pub fn complete(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return None;
    }

    if !trimmed.contains(char::is_whitespace) {
        let mut candidates = COMMAND_NAMES
            .iter()
            .filter(|name| name.starts_with(trimmed));
        let first = candidates.next()?;
        if candidates.next().is_some() {
            return None;
        }
        return Some((*first).to_string());
    }

    let (command, tail) = trimmed.split_once(char::is_whitespace)?;
    if !matches!(
        command,
        "cat" | "type" | "play" | "rm" | "del" | "run" | "open"
    ) {
        return None;
    }
    let argument = tail.trim_start();
    let (scheme_prefix, name_prefix) = if let Some(rest) = argument.strip_prefix("file:/") {
        ("file:/", rest.trim_start_matches('/'))
    } else {
        ("", argument.trim_start_matches('/'))
    };
    if name_prefix.is_empty() {
        return None;
    }
    let names = crate::fat::with_volume(|volume| {
        volume
            .list()
            .into_iter()
            .map(|entry| entry.name)
            .filter(|name| name.starts_with(name_prefix))
            .collect::<alloc::vec::Vec<_>>()
    })
    .unwrap_or_default();
    let mut candidates = names.into_iter();
    let name = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    let prefix_len = line.len().checked_sub(tail.len())?;
    let mut result = String::from(&line[..prefix_len]);
    result.push_str(scheme_prefix);
    result.push_str(&name);
    Some(result)
}

/// Redraws the editable prompt after a history recall or Tab completion.
fn redraw_line(line: &str, drawn: &mut usize) {
    print!("\r{PROMPT}{line}");
    if *drawn > line.len() {
        for _ in 0..(*drawn - line.len()) {
            print!(" ");
        }
        print!("\r{PROMPT}{line}");
    }
    *drawn = (*drawn).max(line.len());
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
    let mut drawn = 0usize;
    loop {
        let byte = wait_for_input();
        match byte {
            b'\r' | b'\n' => {
                // Terminals and test harnesses disagree on CR vs LF; swallow a
                // trailing LF/CR so "cmd\r\n" is not two commands.
                if let Some(b'\r' | b'\n') = input::peek() {
                    input::pop();
                }
                history_push(line);
                println!();
                return true;
            }
            input::KEY_UP => {
                if let Some(previous) = history_prev(line) {
                    *line = previous;
                    redraw_line(line, &mut drawn);
                }
            }
            input::KEY_DOWN => {
                if let Some(next) = history_next() {
                    *line = next;
                    redraw_line(line, &mut drawn);
                }
            }
            b'\t' => {
                if let Some(completed) = complete(line) {
                    *line = completed;
                    redraw_line(line, &mut drawn);
                }
            }
            0x08 | 0x7f => {
                if line.pop().is_some() {
                    print!("\u{8} \u{8}");
                    drawn = drawn.saturating_sub(1);
                    history_reset();
                }
            }
            0x03 => {
                println!("^C");
                line.clear();
                history_reset();
                return false;
            }
            0x0c => {
                console::clear_screen();
                print!("{PROMPT}");
                drawn = 0;
            }
            0x20..=0x7e if line.len() < 200 => {
                line.push(byte as char);
                print!("{}", byte as char);
                drawn += 1;
                history_reset();
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
            crate::logo::redraw();
            banner();
            println!();
        }
        "about" => about(),
        "net" => net_info(),
        "dhcp" => dhcp_command(),
        "ping" => ping(rest),
        "dns" => dns(rest),
        "http" => http(rest),
        "open" => open_url(rest),
        "schemes" => schemes(),
        "cd" => cd(rest),
        "pwd" => pwd(),
        "stat" => stat_command(rest),
        "mounts" => mounts(),
        "beep" => beep(),
        "tone" => tone(rest),
        "disks" => disks(),
        "ls" | "dir" => ls(rest),
        "cat" | "type" => cat(rest),
        "write" => write_file(rest),
        "rm" | "del" => rm(rest),
        "play" => play(rest),
        "format" => format_disk(),
        "run" => run_program(rest),
        "session" => run_program("SESSION.ELF"),
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

/// Lazily initialised dependency graph. A `Mutex` replaces the old
/// `static mut` flags, so state access is explicit even though the kernel is
/// currently single-threaded.
static PACKAGE_STATE: Mutex<Option<pkg::PackageState>> = Mutex::new(None);

fn with_packages<T>(f: impl FnOnce(&mut pkg::PackageState) -> T) -> T {
    let mut guard = PACKAGE_STATE.lock();
    if guard.is_none() {
        *guard = Some(pkg::initial_state());
    }
    // `guard` was just populated above.
    f(guard.as_mut().unwrap())
}

fn pkg_error(error: &pkg::PackageError) {
    console::set_fg(console::RED);
    println!("pkg: {error}");
    console::set_fg(console::FG);
}

fn pkg_name_list(names: &[pkg::PackageName]) -> String {
    if names.is_empty() {
        return String::from("[none]");
    }
    names
        .iter()
        .map(pkg::PackageName::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_names(args: &[&str]) -> Option<Vec<pkg::PackageName>> {
    let mut names = Vec::new();
    for arg in args {
        match pkg::PackageName::new(arg) {
            Ok(name) => names.push(name),
            Err(error) => {
                pkg_error(&error);
                // One bad name rejects the whole command: callers must not
                // mutate state with the subset that happened to parse.
                return None;
            }
        }
    }
    if names.is_empty() {
        pkg_error(&pkg::PackageError::InvalidName(String::from(
            "expected at least one package name",
        )));
        return None;
    }
    Some(names)
}

/// Parses a single required package name, distinguishing a missing argument
/// from a malformed one so the error names the real cause.
fn parse_one_name(arg: Option<&str>, command: &str) -> Option<pkg::PackageName> {
    let Some(arg) = arg else {
        pkg_error(&pkg::PackageError::InvalidName(format!(
            "{command} needs a name"
        )));
        return None;
    };
    match pkg::PackageName::new(arg) {
        Ok(name) => Some(name),
        Err(error) => {
            pkg_error(&error);
            None
        }
    }
}

fn pkg_find(name: &pkg::PackageName) -> Option<pkg::Package> {
    pkg::catalog().into_iter().find(|entry| &entry.name == name)
}

fn pkg_list() {
    let catalog = pkg::catalog();
    with_packages(|state| {
        println!("Packages:");
        for entry in &catalog {
            let (label, color) = if !state.is_installed(&entry.name) {
                ("available", console::DIM)
            } else if state.installed[&entry.name].manual {
                ("installed", console::GREEN)
            } else {
                ("auto", console::CYAN)
            };
            console::set_fg(color);
            print!("  [{label:^9}] ");
            console::set_fg(console::CYAN);
            print!("{}", entry.name.as_str());
            console::set_fg(console::DIM);
            println!(" v{} - {}", entry.version, entry.description);
        }
        console::set_fg(console::FG);
        println!();
        println!("Manual packages are ones you asked for; auto packages are dependencies.");
    });
}

fn pkg_info(name: &pkg::PackageName) {
    let Some(entry) = pkg_find(name) else {
        pkg_error(&pkg::PackageError::NotFound(name.clone()));
        return;
    };
    with_packages(|state| {
        let manual = state.installed.get(name).is_some_and(|s| s.manual);
        let status = if !state.is_installed(name) {
            "available"
        } else if manual {
            "installed (manual)"
        } else {
            "installed (auto)"
        };
        let dependencies = pkg_name_list(entry.runtime_dependencies());
        let dependents: Vec<String> = state
            .installed
            .iter()
            .filter(|(_, s)| s.dependencies.contains(name))
            .map(|(name, _)| name.to_string())
            .collect();
        let required_by = if dependents.is_empty() {
            String::from("[none]")
        } else {
            dependents.join(", ")
        };
        println!("Package: {}", name.as_str());
        println!("Version: {}", entry.version);
        println!("Description: {}", entry.description);
        println!("Status: {status}");
        println!("Source: {}", entry.recipe.source);
        println!("Build: {}", entry.recipe.build);
        println!("Runtime deps: {dependencies}");
        println!("Required by: {required_by}");
    });
}

fn pkg_depends(name: &pkg::PackageName) {
    let Some(entry) = pkg_find(name) else {
        pkg_error(&pkg::PackageError::NotFound(name.clone()));
        return;
    };
    let build = pkg_name_list(entry.build_dependencies());
    let runtime = pkg_name_list(entry.runtime_dependencies());
    println!("{} build deps: {build}", name.as_str());
    println!("{} runtime deps: {runtime}", name.as_str());
    with_packages(|state| {
        let dependents: Vec<String> = state
            .installed
            .iter()
            .filter(|(_, s)| s.dependencies.contains(name))
            .map(|(name, _)| name.to_string())
            .collect();
        let required_by = if dependents.is_empty() {
            String::from("[none]")
        } else {
            dependents.join(", ")
        };
        println!("Installed packages needing {name}: {required_by}");
    });
}

fn pkg_recipe(name: &pkg::PackageName) {
    let Some(entry) = pkg_find(name) else {
        pkg_error(&pkg::PackageError::NotFound(name.clone()));
        return;
    };
    println!("recipe.toml for {name}");
    println!("  version = \"{}\"", entry.version);
    println!("  source  = {}", entry.recipe.source);
    println!("  build   = {}", entry.recipe.build);
    println!(
        "  [build] dependencies      = {}",
        pkg_name_list(entry.build_dependencies())
    );
    println!(
        "  [package] dependencies    = {}",
        pkg_name_list(entry.runtime_dependencies())
    );
}

fn pkg_install(args: &[&str]) {
    let Some(names) = parse_names(args) else {
        return;
    };
    let catalog = pkg::catalog();
    with_packages(|state| match state.install(&catalog, &names) {
        Ok(added) if added.is_empty() => println!("Already installed; nothing to do."),
        Ok(added) => {
            for name in added {
                println!("Installed {name}.");
            }
        }
        Err(error) => pkg_error(&error),
    });
}

fn pkg_remove(args: &[&str]) {
    let Some(names) = parse_names(args) else {
        return;
    };
    with_packages(|state| match state.uninstall(&names) {
        Ok(removed) if removed.is_empty() => println!("Not installed; nothing to do."),
        Ok(removed) => {
            for name in removed {
                println!("Removed {name}.");
            }
        }
        Err(error) => pkg_error(&error),
    });
}

fn pkg_search(text: &str) {
    let needle = text.to_lowercase();
    let catalog = pkg::catalog();
    with_packages(|state| {
        let mut found = 0usize;
        for entry in &catalog {
            let haystack = format!("{} {}", entry.name.as_str(), entry.description);
            if haystack.to_lowercase().contains(&needle) {
                found += 1;
                let label = if state.is_installed(&entry.name) {
                    "*"
                } else {
                    " "
                };
                console::set_fg(console::CYAN);
                println!(
                    "{label} {} v{} - {}",
                    entry.name.as_str(),
                    entry.version,
                    entry.description
                );
            }
        }
        console::set_fg(console::FG);
        if found == 0 {
            println!("No packages match '{text}'.");
        }
    });
}

fn pkg_mark(mode: &str, args: &[&str]) {
    if mode != "manual" && mode != "auto" {
        console::set_fg(console::RED);
        println!("Usage: pkg mark <manual|auto> <name>...");
        console::set_fg(console::FG);
        return;
    }
    let Some(names) = parse_names(args) else {
        return;
    };
    with_packages(|state| {
        let changed = state.mark_as_manual(mode == "manual", &names);
        if changed.is_empty() {
            println!("Nothing changed.");
        } else {
            for name in changed {
                println!("Marked {name} as {mode}.");
            }
        }
    });
}

fn pkg_archive(name: &str) {
    let Some(bytes) = fat::with_volume(|volume| volume.read(name)).flatten() else {
        console::set_fg(console::RED);
        println!("pkg: cannot read '{name}' from the data disk");
        console::set_fg(console::FG);
        return;
    };
    match crate::pkgar::Archive::parse(&bytes) {
        Ok(archive) => {
            println!(
                "Archive '{name}': {} entries, {} data bytes",
                archive.entries.len(),
                archive.data_len()
            );
            println!(
                "  format v{} / arch {} / SHA-256",
                archive.header.version, archive.header.arch
            );
            for entry in &archive.entries {
                println!(
                    "  {:>8} bytes  mode {:o}  {}",
                    entry.size, entry.mode, entry.path
                );
            }
            console::set_fg(console::GREEN);
            println!("  integrity: ok (entry table and every file verified)");
            console::set_fg(console::FG);
        }
        Err(error) => {
            console::set_fg(console::RED);
            println!("pkg: archive error: {error:?}");
            console::set_fg(console::FG);
        }
    }
}

fn pkg_pack(output: &str, inputs: &[&str]) {
    if inputs.is_empty() {
        console::set_fg(console::RED);
        println!("pkg: pack needs at least one input file");
        console::set_fg(console::FG);
        return;
    }
    let mut files = Vec::new();
    for name in inputs {
        let Some(data) = fat::with_volume(|volume| volume.read(name)).flatten() else {
            console::set_fg(console::RED);
            println!("pkg: cannot read '{name}' from the data disk");
            console::set_fg(console::FG);
            return;
        };
        files.push((String::from(*name), data));
    }
    match crate::pkgar::build(&files) {
        Ok(bytes) => {
            let written = fat::with_volume(|volume| volume.write(output, &bytes)).unwrap_or(false);
            if written {
                println!(
                    "Packed {} file(s) into '{output}' ({} bytes).",
                    files.len(),
                    bytes.len()
                );
                println!("Verify with: pkg archive {output}");
            } else {
                console::set_fg(console::RED);
                println!("pkg: cannot write '{output}' to the data disk");
                console::set_fg(console::FG);
            }
        }
        Err(error) => {
            console::set_fg(console::RED);
            println!("pkg: archive error: {error:?}");
            console::set_fg(console::FG);
        }
    }
}

fn package_manager(args: &str, _boot_info: &BootInfo) {
    let mut parts = args.split_whitespace();
    let Some(command) = parts.next() else {
        pkg_list();
        return;
    };
    match command {
        "list" => pkg_list(),
        "search" => {
            let text = parts.collect::<Vec<_>>().join(" ");
            pkg_search(&text);
        }
        "info" => {
            if let Some(name) = parse_one_name(parts.next(), "info") {
                pkg_info(&name);
            }
        }
        "depends" => {
            if let Some(name) = parse_one_name(parts.next(), "depends") {
                pkg_depends(&name);
            }
        }
        "recipe" => {
            if let Some(name) = parse_one_name(parts.next(), "recipe") {
                pkg_recipe(&name);
            }
        }
        "archive" => {
            if let Some(name) = parts.next() {
                pkg_archive(name);
            } else {
                println!("Usage: pkg archive <file>");
            }
        }
        "pack" => {
            let output = parts.next().unwrap_or("");
            pkg_pack(output, &parts.collect::<Vec<_>>());
        }
        "install" => pkg_install(&parts.collect::<Vec<_>>()),
        "remove" => pkg_remove(&parts.collect::<Vec<_>>()),
        "mark" => {
            let mode = parts.next().unwrap_or("");
            pkg_mark(mode, &parts.collect::<Vec<_>>());
        }
        _ => {
            console::set_fg(console::RED);
            println!("Usage: pkg [list|search <text>|info <name>|depends <name>|recipe <name>]");
            println!(
                "       pkg [install <name>...|remove <name>...|mark <manual|auto> <name>...]"
            );
            println!("       pkg pack <out> <file>...   (build a .pkga on the data disk)");
            println!("       pkg archive <file>         (verify a .pkga on the data disk)");
            console::set_fg(console::FG);
        }
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
        ("logo", "redraw the GT logo and banner"),
        ("about", "what this OS is"),
        ("net", "show the network adapter and address"),
        ("dhcp", "get an address from a DHCP server"),
        ("ping <ip>", "send an ICMP echo request"),
        ("dns <name>", "resolve a host name"),
        ("http <url>", "fetch a page over HTTP"),
        (
            "open <url|path>",
            "read a scheme URL or a VFS path (for example /sys/version)",
        ),
        ("schemes", "list the available schemes and example URLs"),
        ("cd [path]", "change the shell's VFS working directory"),
        ("pwd", "print the shell's VFS working directory"),
        ("stat <path>", "show VFS metadata for a path"),
        ("mounts", "list VFS mount points"),
        ("beep", "play a short tone through AC'97 audio"),
        ("tone <hz> [ms]", "play a tone of the given frequency"),
        ("disks", "list the ATA drives and their sizes"),
        ("ls [path]", "list a VFS directory (default: /)"),
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
        ("run scheme", "demo the open/read/close syscalls in ring 3"),
        ("run fork", "demo fork/yield/waitpid with two processes"),
        (
            "run preempt",
            "demo PIT preemption between two ring-3 processes",
        ),
        (
            "run sleep",
            "demo timer-driven sleep with two ring-3 processes",
        ),
        ("run exec", "demo replacing the current ring-3 image"),
        (
            "session",
            "start the ring-3 session manager from SESSION.ELF",
        ),
        ("panic", "trigger a kernel panic (for testing)"),
        ("reboot", "reboot the machine"),
        ("shutdown", "halt the machine"),
        ("pkg [list]", "list packages (installed/auto/available)"),
        ("pkg search <text>", "search names and descriptions"),
        ("pkg info <name>", "show package info and reverse deps"),
        (
            "pkg depends <name>",
            "show build and runtime dependency edges",
        ),
        ("pkg recipe <name>", "show cookbook-style recipe metadata"),
        (
            "pkg install <name>...",
            "install packages with dependencies",
        ),
        ("pkg remove <name>...", "remove packages and orphaned deps"),
        ("pkg mark <mode> <name>...", "set manual/auto install flag"),
        (
            "pkg pack <out> <file>...",
            "build a .pkga archive on the data disk",
        ),
        (
            "pkg archive <file>",
            "verify a .pkga archive on the data disk",
        ),
    ];

    println!("Commands:");
    for (name, description) in COMMANDS {
        console::set_fg(console::CYAN);
        print!("  {name:<20}");
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
    println!("GTos is a small x86_64 kernel written entirely in Rust.");
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
    println!("attach an RTL8139 or Intel 82540EM NIC to the VM");
    true
}

fn net_info() {
    if require_network() {
        return;
    }
    if let Some(info) = net::adapter() {
        println!("adapter: {}, {}, IRQ {}", info.name, info.bus, info.irq);
    }
    let config = net::address();
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

/// `open <url>`: reads any scheme resource and prints its text.
fn open_url(rest: &str) {
    if rest.is_empty() {
        error(format_args!("usage: open <url|path> (try 'schemes')"));
        return;
    }
    let target = vfs_resolve(rest).unwrap_or_else(|| String::from(rest));
    match crate::scheme::open(&target) {
        Ok(resource) => {
            println!("{} [{}]", resource.url, resource.mime);
            for line in resource.text().lines() {
                println!("{line}");
            }
        }
        Err(message) => error(format_args!("open: {message}")),
    }
}

/// The shell's working directory, with an empty slot treated as `/`.
fn shell_cwd() -> String {
    let guard = SHELL_CWD.lock();
    if guard.is_empty() {
        String::from("/")
    } else {
        guard.clone()
    }
}

/// Resolves a shell argument against its current directory.
fn shell_absolute(rest: &str) -> Result<String, String> {
    let path = rest.trim();
    vfs::absolute_path(&shell_cwd(), if path.is_empty() { "/" } else { path })
}

/// Resolves a shell argument to a scheme URL, falling back to the raw input.
fn vfs_resolve(rest: &str) -> Option<String> {
    vfs::resolve(&shell_cwd(), rest.trim()).ok()
}

fn cd(rest: &str) {
    let Ok(absolute) = shell_absolute(rest) else {
        error(format_args!("cd: invalid path"));
        return;
    };
    match vfs::stat_absolute(&absolute) {
        Ok(info) if info.kind == vfs::KIND_DIR => *SHELL_CWD.lock() = absolute,
        Ok(_) => error(format_args!("cd: not a directory: {absolute}")),
        Err(message) => error(format_args!("cd: {message}")),
    }
}

fn pwd() {
    println!("{}", shell_cwd());
}

fn stat_command(rest: &str) {
    if rest.trim().is_empty() {
        error(format_args!("usage: stat <path>"));
        return;
    }
    match shell_absolute(rest)
        .and_then(|absolute| vfs::stat_absolute(&absolute).map(|info| (absolute, info)))
    {
        Ok((absolute, info)) => {
            let kind = if info.kind == vfs::KIND_DIR {
                "directory"
            } else {
                "file"
            };
            let writable = if info.writable == 0 { "ro" } else { "rw" };
            println!("{absolute}");
            println!(
                "  inode {}  {kind}  {} bytes  {writable}",
                info.inode, info.size
            );
        }
        Err(message) => error(format_args!("stat: {message}")),
    }
}

fn mounts() {
    for row in vfs::mount_table() {
        println!("{row}");
    }
}

/// `schemes`: prints the same help resource as `open sys:/help`.
fn schemes() {
    match crate::scheme::open("sys:/help") {
        Ok(resource) => print!("{}", resource.text()),
        Err(message) => error(format_args!("schemes: {message}")),
    }
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
    println!("attach the gtos-data.img disk to the VM");
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

/// `ls [path]`: lists a VFS directory, defaulting to the root mount.
fn ls(rest: &str) {
    let Ok(absolute) = shell_absolute(rest) else {
        error(format_args!("ls: invalid path"));
        return;
    };
    match vfs::read_dir_absolute(&absolute) {
        Ok(names) => {
            if names.is_empty() {
                println!("(the directory is empty)");
                return;
            }
            for name in names {
                println!("{name}");
            }
        }
        Err(message) => error(format_args!("ls: {message}")),
    }
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
        println!("type the file, end with a line containing only '.' (Ctrl-C cancels)");
        loop {
            let Some(line) = prompt_line() else {
                // Ctrl-C: abandon the file without creating or writing it.
                return;
            };
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
        println!("attach an AC'97 audio controller to the VM");
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

/// `run [file]`: runs a flat/ELF ring-3 binary, or a built-in demo.
///
/// The built-ins are `demo` (no argument), `scheme`, `fork`, `preempt`, `sleep`
/// and `exec`. A flat binary must be linked against the kernel's user base (see
/// [`usermode::USER_BASE`]); `HELLO.BIN` on the data disk is one such program.
fn run_program(command: &str) {
    if !usermode::is_ready() {
        error(format_args!("ring 3 is not initialized"));
        return;
    }

    let mut words = command.split_whitespace();
    let name = words.next().unwrap_or("");
    let mut startup = String::new();
    startup.push_str(if name.is_empty() { "demo" } else { name });
    startup.push('\0');
    for word in words {
        startup.push_str(word);
        startup.push('\0');
    }
    startup.push('\0');

    let (label, program) = if name.is_empty() {
        ("demo program", usermode::demo_program())
    } else if name == "scheme" {
        ("scheme demo", usermode::scheme_demo_program())
    } else if name == "fork" {
        ("fork demo", usermode::fork_demo_program())
    } else if name == "preempt" {
        ("preemption demo", usermode::preempt_demo_program())
    } else if name == "sleep" {
        ("sleep demo", usermode::sleep_demo_program())
    } else if name == "exec" {
        ("exec demo", usermode::exec_demo_program())
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
    // Flat binaries live in the legacy 64 KiB code window. ELF images are
    // mapped segment by segment and have their own, larger limit.
    let is_elf = program.starts_with(&[0x7f, b'E', b'L', b'F']);
    let limit = if is_elf {
        usermode::MAX_ELF_BYTES
    } else {
        usermode::MAX_PROGRAM_BYTES
    };
    if program.len() > limit {
        error(format_args!(
            "{label} is {} bytes, the limit is {limit}",
            program.len()
        ));
        return;
    }

    println!("running {label} ({} bytes) in ring 3 ...", program.len());
    let code = usermode::run(&program, startup.as_bytes());
    console::set_fg(console::GREEN);
    println!("{label} exited with status {code}");
    console::set_fg(console::FG);
}

/// Reads one more line from the console, used by `write`. Returns `None` when
/// the user cancels with Ctrl-C so the caller can abort the whole operation.
fn prompt_line() -> Option<String> {
    console::set_fg(console::DIM);
    print!("> ");
    console::set_fg(console::FG);
    let mut line = String::new();
    read_line(&mut line).then_some(line)
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
        println!("attach an AC'97 audio controller to the VM");
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
    if frequency == 0 {
        console::set_fg(console::RED);
        println!("invalid frequency: must be greater than zero");
        console::set_fg(console::FG);
        return;
    }
    let milliseconds = match parts.next() {
        // An explicitly supplied but unparseable duration is an error: silently
        // falling back to 200 ms would play something the user did not ask for.
        Some(value) => {
            let Ok(duration) = value.parse::<u32>() else {
                console::set_fg(console::RED);
                println!("invalid duration: {value}");
                console::set_fg(console::FG);
                return;
            };
            duration.clamp(1, audio::max_tone_ms())
        }
        None => 200,
    };

    if !audio::is_up() {
        console::set_fg(console::RED);
        println!("no audio device is up");
        console::set_fg(console::FG);
        println!("attach an AC'97 audio controller to the VM");
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
    println!("Halting the machine...");
    loop {
        x86_64::instructions::interrupts::disable();
        x86_64::instructions::hlt();
    }
}
