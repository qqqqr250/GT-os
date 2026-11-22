//! Turns the compiled kernel into bootable BIOS and UEFI disk images, plus a
//! bootable ISO9660/El Torito disc image.

use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "build/fat16.rs"]
mod fat16;
#[path = "build/iso.rs"]
mod iso;

fn main() {
    // OUT_DIR is where Cargo wants build-script outputs.
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));

    // Set by Cargo's artifact-dependency feature (see .cargo/config.toml): the
    // path to the `kernel` binary built for x86_64-unknown-none.
    let kernel = PathBuf::from(
        std::env::var_os("CARGO_BIN_FILE_KERNEL_kernel")
            .expect("kernel artifact dependency missing"),
    );

    let bios_path = out_dir.join("gtos-bios.img");
    bootloader::BiosBoot::new(&kernel)
        .create_disk_image(&bios_path)
        .expect("failed to create the BIOS disk image");
    println!("cargo:rustc-env=BIOS_PATH={}", bios_path.display());

    let uefi_path = out_dir.join("gtos-uefi.img");
    bootloader::UefiBoot::new(&kernel)
        .create_disk_image(&uefi_path)
        .expect("failed to create the UEFI disk image");
    println!("cargo:rustc-env=UEFI_PATH={}", uefi_path.display());

    // The ISO reuses the BIOS MBR image and a FAT partition holding
    // `efi/boot/bootx64.efi`, which is exactly what an El Torito UEFI entry
    // wants.
    let uefi_fat_path = out_dir.join("gtos-uefi-fat.img");
    bootloader::DiskImageBuilder::new(kernel)
        .create_uefi_fat_partition(&uefi_fat_path)
        .expect("failed to create the UEFI FAT partition");

    let iso_path =
        iso::build(&out_dir, &bios_path, &uefi_fat_path).expect("failed to create the ISO image");
    println!("cargo:rustc-env=ISO_PATH={}", iso_path.display());

    // The GTos data disk: a small FAT16 volume attached as the primary IDE
    // slave. It is created only when missing, so files written from inside the
    // guest survive a rebuild. It lives in the project root, next to Cargo.toml.
    let manifest = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    );
    seed_data_disk(&manifest, &out_dir);
}

/// Compiles one guest crate into a ring-3 ELF image.
///
/// Cargo is invoked from the guest directory so its `.cargo/config.toml`
/// selects the bare-metal target and the ring-3 entry point. Each guest gets a
/// separate target directory so nested builds do not contend with each other.
fn build_guest(
    manifest: &Path,
    out_dir: &Path,
    crate_path: &str,
    target_dir: &str,
    binary: &str,
) -> Vec<u8> {
    let guest_dir = manifest.join(crate_path);
    let guest_target = out_dir.join(target_dir);
    let status = Command::new("cargo")
        .arg("build")
        .arg("--manifest-path")
        .arg(guest_dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&guest_target)
        .current_dir(&guest_dir)
        .status()
        .unwrap_or_else(|error| panic!("failed to build {binary}: {error}"));
    assert!(status.success(), "{binary} build failed");
    std::fs::read(
        guest_target
            .join("x86_64-unknown-none")
            .join("debug")
            .join(binary),
    )
    .unwrap_or_else(|error| panic!("failed to read {binary} ELF: {error}"))
}

/// Hashes the seed sources so a guest change regenerates the data disk.
fn seed_hash(manifest: &Path, elfs: &[&[u8]]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for path in [
        "build.rs",
        "build/fat16.rs",
        "guest/Cargo.toml",
        "guest/.cargo/config.toml",
        "guest/src/main.rs",
        "guest/gtos-sys/Cargo.toml",
        "guest/gtos-sys/src/lib.rs",
        "guest/gtos-widgets/Cargo.toml",
        "guest/gtos-widgets/src/lib.rs",
        "guest/gtos-widgets/src/font.rs",
        "guest/gtos-widgets/src/theme.rs",
        "guest/gtos-widgets/src/view.rs",
        "guest/gtos-widgets/src/shell.rs",
        "guest/session/Cargo.toml",
        "guest/session/.cargo/config.toml",
        "guest/session/src/main.rs",
        "guest/service/Cargo.toml",
        "guest/service/.cargo/config.toml",
        "guest/service/src/main.rs",
        "guest/gtos-cabi/Cargo.toml",
        "guest/gtos-cabi/src/lib.rs",
        "guest/cabi-demo/Cargo.toml",
        "guest/cabi-demo/.cargo/config.toml",
        "guest/cabi-demo/src/main.rs",
    ] {
        if let Ok(bytes) = std::fs::read(manifest.join(path)) {
            for byte in bytes {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    for elf in elfs {
        for byte in *elf {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

const README: &str = "GTos data disk\r\n\
    ================\r\n\
    \r\n\
    This is a FAT16 volume on the primary IDE slave (drive 1).\r\n\
    Inside GTos, use the shell commands to work with it:\r\n\
    \r\n\
    \x20 disks          show the ATA drives\r\n\
    \x20 ls             list the root directory\r\n\
    \x20 cat <file>     print a file\r\n\
    \x20 write <file>   create a file (text, or end with a '.' line)\r\n\
    \x20 rm <file>      delete a file\r\n\
    \x20 play <file>    play a WAV file through AC'97 audio\r\n\
    \x20 run <file>     run a flat or ELF ring-3 program\r\n\
    \x20 session        start the ring-3 session manager\r\n\
    \x20 format         reformat the volume\r\n\
    \r\n\
    Try: play BEEP.WAV\r\n\
    Try: run HELLO.BIN\r\n\
    Try: run HELLO.ELF\r\n\
    Try: run RUSTDEMO.ELF\r\n\
    Try: run CABI.ELF\r\n\
    Try: session\r\n";
const APPS: &str = "GTos launcher metadata\r\n\
    term\tTerminal\tHELLO.BIN\r\n\
    edit\tEditor\tHELLO.ELF\r\n\
    files\tFiles\tCABI.ELF\r\n\
    help\tHelp\tHELLO.BIN\r\n";
const AUTOSTART: &str = "# GTos session autostart\r\n\
    # one launcher id per line\r\n\
    term\r\n";
const RUNNERS: &str = "GTos session runner metadata\r\n\
    term\tTerminal\tHELLO.BIN\r\n\
    edit\tEditor\tHELLO.ELF\r\n";
const KCMS: &str = "GTos session configuration modules\r\n\
    theme\tTheme\tTHEME.KCM\r\n\
    clock\tClock\tCLOCK.KCM\r\n";
const THEME: &str = "theme=dark\naccent=teal\n";
const CLOCK: &str = "format=24h\ntimezone=local\n";
const SERVICES: &str = "GTos managed services\r\n\
    demo\tDemo\tSERVICE.ELF\t2\r\n";

/// Creates or refreshes the seeded FAT image, preserving guest-written files.
fn seed_data_disk(manifest: &Path, out_dir: &Path) {
    let rust_elf = build_guest(manifest, out_dir, "guest", "guest-target", "gtos-guest");
    let cabi_elf = build_guest(
        manifest,
        out_dir,
        "guest/cabi-demo",
        "cabi-target",
        "gtos-cabi-demo",
    );
    let session_elf = build_guest(
        manifest,
        out_dir,
        "guest/session",
        "session-target",
        "gtos-session",
    );
    let service_elf = build_guest(
        manifest,
        out_dir,
        "guest/service",
        "service-target",
        "gtos-service",
    );
    let data_path = manifest.join("gtos-data.img");
    let seed_path = manifest.join("gtos-data.seed");
    let seed = seed_hash(
        manifest,
        &[&rust_elf, &cabi_elf, &session_elf, &service_elf],
    );
    let seed_changed = std::fs::read_to_string(&seed_path)
        .ok()
        .and_then(|text| text.parse::<u64>().ok())
        != Some(seed);
    if !data_path.exists() || seed_changed {
        let beep = fat16::tone_wav();
        let hello = fat16::hello_bin();
        let elf = fat16::hello_elf();
        let image = fat16::build(&[
            ("README.TXT", README.as_bytes()),
            ("BEEP.WAV", &beep),
            ("HELLO.BIN", &hello),
            ("HELLO.ELF", &elf),
            ("RUSTDEMO.ELF", &rust_elf),
            ("CABI.ELF", &cabi_elf),
            ("SESSION.ELF", &session_elf),
            ("SERVICE.ELF", &service_elf),
            ("APPS.TSV", APPS.as_bytes()),
            ("AUTO.TSV", AUTOSTART.as_bytes()),
            ("RUNNERS.TSV", RUNNERS.as_bytes()),
            ("KCMS.TSV", KCMS.as_bytes()),
            ("THEME.KCM", THEME.as_bytes()),
            ("CLOCK.KCM", CLOCK.as_bytes()),
            ("SERVICES.TSV", SERVICES.as_bytes()),
        ]);
        std::fs::write(&data_path, &image).expect("failed to create the data disk image");
        std::fs::write(&seed_path, seed.to_string()).expect("failed to write data disk seed");
        println!(
            "cargo:warning=created data disk {} ({} KiB)",
            data_path.display(),
            image.len() / 1024
        );
    }
    // Rebuild the data disk when either build-script source changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/fat16.rs");
    // The boot image embeds the kernel artifact, so kernel-only edits must
    // rerun this script; otherwise the VM keeps booting an older binary.
    println!("cargo:rerun-if-changed=kernel/Cargo.toml");
    println!("cargo:rerun-if-changed=kernel/src");
    println!("cargo:rerun-if-changed=guest/Cargo.toml");
    println!("cargo:rerun-if-changed=guest/.cargo/config.toml");
    println!("cargo:rerun-if-changed=guest/src/main.rs");
    println!("cargo:rerun-if-changed=guest/gtos-sys/Cargo.toml");
    println!("cargo:rerun-if-changed=guest/gtos-sys/src/lib.rs");
    println!("cargo:rerun-if-changed=guest/gtos-widgets/Cargo.toml");
    println!("cargo:rerun-if-changed=guest/gtos-widgets/src");
    println!("cargo:rerun-if-changed=guest/session/Cargo.toml");
    println!("cargo:rerun-if-changed=guest/session/.cargo/config.toml");
    println!("cargo:rerun-if-changed=guest/session/src");
    println!("cargo:rerun-if-changed=guest/service/Cargo.toml");
    println!("cargo:rerun-if-changed=guest/service/.cargo/config.toml");
    println!("cargo:rerun-if-changed=guest/service/src");
    println!("cargo:rerun-if-changed=tools/font8x8_basic.h");
    println!("cargo:rerun-if-changed=guest/gtos-cabi/Cargo.toml");
    println!("cargo:rerun-if-changed=guest/gtos-cabi/src/lib.rs");
    println!("cargo:rerun-if-changed=guest/cabi-demo/Cargo.toml");
    println!("cargo:rerun-if-changed=guest/cabi-demo/.cargo/config.toml");
    println!("cargo:rerun-if-changed=guest/cabi-demo/src/main.rs");
    println!("cargo:rustc-env=DATA_PATH={}", data_path.display());
}
