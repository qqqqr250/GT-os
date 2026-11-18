//! Host-side tool that wraps a bare-metal ELF in a bootable BIOS disk image.
//!
//! Cargo invokes it through the `runner` key in `.cargo/config.toml`, passing the
//! path to a freshly linked test binary. In that (default) mode the runner wraps
//! the ELF, boots it in QEMU with the `isa-debug-exit` device and COM1 on stdout,
//! and maps QEMU's exit code back onto its own so `cargo test` reports pass or
//! fail.
//!
//! `--image <elf> [<out.img>]` only builds the disk image and prints its path,
//! which is how `scripts/minimal.ps1` turns the `examples/minimal` kernel into
//! something bootable.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

/// QEMU's `isa-debug-exit` reports `(value << 1) | 1`; the test framework
/// writes `0x10` (`QemuExitCode::Success`) on a clean run.
const TEST_SUCCESS_CODE: i32 = 33;

/// How long a single test binary may run before it is killed.
const TEST_TIMEOUT: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--image") => return run_image_mode(args),
        Some("-h" | "--help") => {
            print_usage();
            return ExitCode::SUCCESS;
        }
        Some(argument) => return run_test_mode(PathBuf::from(argument)),
        None => {
            print_usage();
            return ExitCode::from(2);
        }
    }
}

/// Prints the accepted command lines.
fn print_usage() {
    eprintln!(
        "usage:\n  \
         gtos-test-runner <kernel-test-binary>     build + boot a test ELF in QEMU\n  \
         gtos-test-runner --image <elf> [<out>]    build a bootable disk image only"
    );
}

/// `--image <elf> [<out.img>]`: builds the disk image and prints its path.
fn run_image_mode(mut args: impl Iterator<Item = String>) -> ExitCode {
    let Some(kernel) = args.next() else {
        eprintln!("error: --image needs the path to a kernel ELF");
        return ExitCode::from(2);
    };
    let kernel = PathBuf::from(kernel);
    let out = args.next().map(PathBuf::from);

    let image = match out {
        Some(out) => {
            let image = match build_image_to(&kernel, &out) {
                Ok(image) => image,
                Err(error) => {
                    eprintln!("error: failed to build the disk image: {error}");
                    return ExitCode::FAILURE;
                }
            };
            image
        }
        None => match build_image(&kernel) {
            Ok(image) => image,
            Err(error) => {
                eprintln!("error: failed to build the disk image: {error}");
                return ExitCode::FAILURE;
            }
        },
    };

    // The path is the program's result: scripts read it from stdout.
    println!("{}", image.display());
    ExitCode::SUCCESS
}

/// The default mode: boot a test ELF and map QEMU's exit code.
fn run_test_mode(kernel: PathBuf) -> ExitCode {
    let Some(qemu) = find_qemu() else {
        eprintln!(
            "error: qemu-system-x86_64 not found.\n\
             Install QEMU (scripts\\setup.ps1 does this) or set the QEMU environment\n\
             variable to the full path of qemu-system-x86_64."
        );
        return ExitCode::FAILURE;
    };

    let image = match build_image(&kernel) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("error: failed to build the test disk image: {error}");
            return ExitCode::FAILURE;
        }
    };

    let mut command = Command::new(&qemu);
    command
        .arg("-name")
        .arg("GTos tests")
        .arg("-m")
        .arg("256M")
        .arg("-no-reboot")
        .arg("-device")
        .arg("isa-debug-exit,iobase=0xf4,iosize=0x04")
        .arg("-display")
        .arg("none")
        .arg("-serial")
        .arg("stdio")
        .arg("-drive")
        .arg(format!("format=raw,file={}", image.display()));

    match run_with_timeout(&mut command, TEST_TIMEOUT) {
        Ok(Some(TEST_SUCCESS_CODE)) => ExitCode::SUCCESS,
        Ok(Some(code)) => {
            eprintln!("test run failed (QEMU exit code {code})");
            ExitCode::FAILURE
        }
        Ok(None) => {
            eprintln!("test run timed out after {}s", TEST_TIMEOUT.as_secs());
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("error: failed to start QEMU: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Wraps the ELF in a bootable BIOS disk image in the temp directory.
fn build_image(kernel: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let stem = kernel
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("gtos-test");
    let image = std::env::temp_dir().join(format!("{stem}-disk.img"));
    build_image_to(kernel, &image)
}

/// Wraps the ELF in a bootable BIOS disk image at `out`, creating its parent
/// directory if needed, and returns `out`.
fn build_image_to(kernel: &Path, out: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    bootloader::BiosBoot::new(kernel).create_disk_image(out)?;
    Ok(out.to_path_buf())
}

/// Runs `command`, killing it if it outlives `timeout`.
///
/// Returns `Ok(None)` on timeout, `Ok(Some(code))` when it exits, and the I/O
/// error if QEMU could not be spawned.
fn run_with_timeout(command: &mut Command, timeout: Duration) -> std::io::Result<Option<i32>> {
    let mut child = command.spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code());
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Locates `qemu-system-x86_64`, honouring the `QEMU` environment variable.
fn find_qemu() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("QEMU") {
        return Some(PathBuf::from(path));
    }

    let exe = if cfg!(windows) {
        "qemu-system-x86_64.exe"
    } else {
        "qemu-system-x86_64"
    };

    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
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
