//! Host-side launcher: builds nothing itself, but exposes the disk images the
//! build script produced and boots them in QEMU.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const BIOS_IMAGE: &str = env!("BIOS_PATH");
const UEFI_IMAGE: &str = env!("UEFI_PATH");
const ISO_IMAGE: &str = env!("ISO_PATH");
const DATA_PATH: &str = env!("DATA_PATH");

/// The launcher's own options, after parsing `argv`.
struct Options {
    firmware: String,
    medium: String,
    headless: bool,
    /// Audio backend for the AC'97 device: `wav` (default, always works),
    /// `dsound` (live output on Windows), `sdl`, or `none`.
    audio: String,
    passthrough: Vec<String>,
}

fn main() -> ExitCode {
    let options = match parse_args() {
        Ok(options) => options,
        Err(code) => return code,
    };

    let Some(qemu) = find_qemu() else {
        eprintln!(
            "error: qemu-system-x86_64 not found.\n\
             Install QEMU (scripts\\setup.ps1 does this) or set the QEMU environment\n\
             variable to the full path of qemu-system-x86_64."
        );
        return ExitCode::FAILURE;
    };

    let mut command = match build_command(&qemu, &options) {
        Ok(command) => command,
        Err(code) => return code,
    };
    command.args(&options.passthrough);
    println!("starting {}", qemu.display());

    run(command)
}

/// Parses the launcher's arguments. `Err` carries the exit code to return
/// (help was printed, or the arguments were invalid).
fn parse_args() -> Result<Options, ExitCode> {
    let mut options = Options {
        firmware: String::from("bios"),
        medium: String::from("disk"),
        headless: false,
        audio: String::from("wav"),
        passthrough: Vec::new(),
    };

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "bios" | "uefi" => options.firmware = arg,
            "disk" | "iso" => options.medium = arg,
            "--headless" | "-H" => options.headless = true,
            "--audio" => {
                let Some(backend) = args.next() else {
                    eprintln!("error: --audio needs a backend: wav, dsound, sdl or none\n");
                    return Err(ExitCode::from(2));
                };
                if !matches!(backend.as_str(), "wav" | "dsound" | "sdl" | "none") {
                    eprintln!("error: unknown audio backend '{backend}'");
                    eprintln!("       expected one of: wav, dsound, sdl, none\n");
                    return Err(ExitCode::from(2));
                }
                options.audio = backend;
            }
            "-h" | "--help" => {
                print_help();
                return Err(ExitCode::SUCCESS);
            }
            "--" => {
                options.passthrough.extend(args);
                break;
            }
            other => {
                eprintln!("error: unknown argument '{other}'\n");
                print_help();
                return Err(ExitCode::from(2));
            }
        }
    }

    Ok(options)
}

/// Assembles the QEMU command line: machine options, serial wiring, firmware
/// and the disk image.
fn build_command(qemu: &Path, options: &Options) -> Result<Command, ExitCode> {
    let mut command = Command::new(qemu);
    command
        .arg("-name")
        .arg("RustOS")
        .arg("-m")
        .arg("256M")
        .arg("-smp")
        .arg("1")
        .arg("-no-reboot")
        // Lets the guest power the machine off when it writes to port 0xf4.
        .arg("-device")
        .arg("isa-debug-exit,iobase=0xf4,iosize=0x04")
        // User-mode networking with an RTL8139, the NIC the kernel drives.
        // SLIRP gives the guest 10.0.2.15, gateway 10.0.2.2, DNS 10.0.2.3.
        .arg("-nic")
        .arg("user,model=rtl8139");

    add_audio(&mut command, &options.audio);

    if options.headless {
        command
            .arg("-display")
            .arg("none")
            .arg("-monitor")
            .arg("none")
            .arg("-serial")
            .arg("stdio");
    } else {
        let serial_log = PathBuf::from("serial.log");
        command
            .arg("-serial")
            .arg(format!("file:{}", serial_log.display()));
        println!("serial output is mirrored to {}", serial_log.display());
        println!("click inside the window to grab the mouse (Ctrl+Alt+G releases it)");
    }

    if options.firmware == "uefi" {
        add_uefi_firmware(&mut command, qemu)?;
    }

    if options.medium == "iso" {
        command.arg("-drive").arg(format!(
            "if=ide,index=0,format=raw,media=cdrom,file={ISO_IMAGE}"
        ));
        println!("booting {} ISO image {ISO_IMAGE}", options.firmware);
    } else if options.firmware == "uefi" {
        command
            .arg("-drive")
            .arg(format!("if=ide,index=0,format=raw,file={UEFI_IMAGE}"));
        println!("booting UEFI disk image {UEFI_IMAGE}");
    } else {
        command
            .arg("-drive")
            .arg(format!("if=ide,index=0,format=raw,file={BIOS_IMAGE}"));
        println!("booting BIOS disk image {BIOS_IMAGE}");
    }

    // The RustOS data disk rides on the primary IDE slave, which is drive 1 for
    // the kernel's ATA driver. It is the same file the build script seeds with
    // README.TXT and BEEP.WAV, so writes from the guest persist between boots.
    command
        .arg("-drive")
        .arg(format!("if=ide,index=1,format=raw,file={DATA_PATH}"));
    println!("data disk is {DATA_PATH}");

    Ok(command)
}

/// Adds the AC'97 sound card and its audio backend.
///
/// `wav` records everything the guest plays to `audio.wav`, which is why it is
/// the default: it always works, even on a headless or sound-less machine.
/// `dsound` (Windows) and `sdl` route the audio to the host speakers.
fn add_audio(command: &mut Command, backend: &str) {
    let audiodev = match backend {
        "none" => String::from("none,id=snd0"),
        "dsound" => String::from("dsound,id=snd0"),
        "sdl" => String::from("sdl,id=snd0"),
        // `wav` writes to the current directory.
        _ => {
            let path = PathBuf::from("audio.wav");
            println!("guest audio is recorded to {}", path.display());
            format!("wav,id=snd0,path={}", path.display())
        }
    };
    command
        .arg("-audiodev")
        .arg(audiodev)
        .arg("-device")
        .arg("AC97,audiodev=snd0");
}

/// Adds the OVMF pflash drives the UEFI image needs.
fn add_uefi_firmware(command: &mut Command, qemu: &Path) -> Result<(), ExitCode> {
    let Some((code, vars)) = find_ovmf(qemu) else {
        eprintln!(
            "error: OVMF firmware not found, so the UEFI image cannot boot.\n\
             Install the QEMU package that ships edk2-x86_64-code.fd, or use 'bios'."
        );
        return Err(ExitCode::FAILURE);
    };

    command.arg("-drive").arg(format!(
        "if=pflash,format=raw,unit=0,readonly=on,file={}",
        code.display()
    ));
    if let Some(vars) = vars {
        command.arg("-drive").arg(format!(
            "if=pflash,format=raw,unit=1,snapshot=on,file={}",
            vars.display()
        ));
    }

    Ok(())
}

/// Runs QEMU and maps its exit status onto ours.
fn run(mut command: Command) -> ExitCode {
    match command.status() {
        Ok(status) => match status.code() {
            // isa-debug-exit reports (value << 1) | 1: 0x21 means the guest
            // asked for a clean shutdown.
            Some(0x21 | 0) => {
                println!("guest shut down cleanly");
                ExitCode::SUCCESS
            }
            Some(code) => {
                eprintln!("QEMU exited with status {code}");
                ExitCode::FAILURE
            }
            None => ExitCode::FAILURE,
        },
        Err(error) => {
            eprintln!("error: failed to start QEMU: {error}");
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    println!("RustOS launcher");
    println!();
    println!(
        "usage: cargo run -- [bios|uefi] [disk|iso] [--headless] [--audio <backend>] [-- <extra QEMU args>]"
    );
    println!();
    println!("  bios       boot through the legacy BIOS firmware (default)");
    println!("  uefi       boot through UEFI/OVMF");
    println!("  disk       boot the raw disk image (default)");
    println!("  iso        boot the ISO9660/El Torito disc image");
    println!("  --headless run without a window, COM1 on stdout");
    println!("  --audio    AC'97 backend: wav (default, records audio.wav),");
    println!("             dsound (Windows speakers), sdl, or none");
    println!();
    println!("Inside the guest, type 'help' for shell commands and 'shutdown' to quit.");
}

fn find_qemu() -> Option<PathBuf> {
    if let Some(path) = env::var_os("QEMU") {
        return Some(PathBuf::from(path));
    }

    let exe = if cfg!(windows) {
        "qemu-system-x86_64.exe"
    } else {
        "qemu-system-x86_64"
    };

    if let Some(paths) = env::var_os("PATH") {
        for dir in env::split_paths(&paths) {
            let candidate = dir.join(exe);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let known = [
        "C:/Program Files/qemu",
        "C:/Program Files (x86)/qemu",
        "/usr/local/bin",
        "/usr/bin",
    ];
    known
        .iter()
        .map(|dir| Path::new(dir).join(exe))
        .find(|candidate| candidate.is_file())
}

/// Locates an OVMF/edk2 code image and (optionally) its variable store.
fn find_ovmf(qemu: &Path) -> Option<(PathBuf, Option<PathBuf>)> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(dir) = qemu.parent() {
        roots.push(dir.join("share"));
    }
    roots.push(PathBuf::from("/usr/share/OVMF"));
    roots.push(PathBuf::from("/usr/share/edk2/ovmf"));

    let code_names = [
        "edk2-x86_64-code.fd",
        "OVMF_CODE.fd",
        "OVMF_CODE_4M.fd",
        "OVMF_CODE.secboot.fd",
    ];
    let vars_names = [
        "edk2-i386-vars.fd",
        "OVMF_VARS.fd",
        "OVMF_VARS_4M.fd",
        "OVMF_VARS.secboot.fd",
    ];

    for root in roots {
        let code = code_names
            .iter()
            .map(|name| root.join(name))
            .find(|p| p.is_file());
        if let Some(code) = code {
            let vars = vars_names
                .iter()
                .map(|name| root.join(name))
                .find(|p| p.is_file());
            return Some((code, vars));
        }
    }
    None
}
