//! Turns the compiled kernel into bootable BIOS and UEFI disk images, plus a
//! bootable ISO9660/El Torito disc image.

use std::path::PathBuf;

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

    let bios_path = out_dir.join("rustos-bios.img");
    bootloader::BiosBoot::new(&kernel)
        .create_disk_image(&bios_path)
        .expect("failed to create the BIOS disk image");
    println!("cargo:rustc-env=BIOS_PATH={}", bios_path.display());

    let uefi_path = out_dir.join("rustos-uefi.img");
    bootloader::UefiBoot::new(&kernel)
        .create_disk_image(&uefi_path)
        .expect("failed to create the UEFI disk image");
    println!("cargo:rustc-env=UEFI_PATH={}", uefi_path.display());

    // The ISO reuses the BIOS MBR image and a FAT partition holding
    // `efi/boot/bootx64.efi`, which is exactly what an El Torito UEFI entry
    // wants.
    let uefi_fat_path = out_dir.join("rustos-uefi-fat.img");
    bootloader::DiskImageBuilder::new(kernel)
        .create_uefi_fat_partition(&uefi_fat_path)
        .expect("failed to create the UEFI FAT partition");

    let iso_path =
        iso::build(&out_dir, &bios_path, &uefi_fat_path).expect("failed to create the ISO image");
    println!("cargo:rustc-env=ISO_PATH={}", iso_path.display());

    // The RustOS data disk: a small FAT16 volume attached as the primary IDE
    // slave. It is created only when missing, so files written from inside the
    // guest survive a rebuild. It lives in the project root, next to Cargo.toml.
    let manifest = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    );
    let data_path = manifest.join("rustos-data.img");
    if !data_path.exists() {
        let readme = "RustOS data disk\r\n\
            ================\r\n\
            \r\n\
            This is a FAT16 volume on the primary IDE slave (drive 1).\r\n\
            Inside RustOS, use the shell commands to work with it:\r\n\
            \r\n\
            \x20 disks          show the ATA drives\r\n\
            \x20 ls             list the root directory\r\n\
            \x20 cat <file>     print a file\r\n\
            \x20 write <file>   create a file (text, or end with a '.' line)\r\n\
            \x20 rm <file>      delete a file\r\n\
            \x20 play <file>    play a WAV file through AC'97 audio\r\n\
            \x20 run <file>     run a flat ring-3 program\r\n\
            \x20 format         reformat the volume\r\n\
            \r\n\
            Try: play BEEP.WAV\r\n\
            Try: run HELLO.BIN\r\n";
        let beep = fat16::tone_wav();
        let hello = fat16::hello_bin();
        let image = fat16::build(&[
            ("README.TXT", readme.as_bytes()),
            ("BEEP.WAV", &beep),
            ("HELLO.BIN", &hello),
        ]);
        std::fs::write(&data_path, &image).expect("failed to create the data disk image");
        println!(
            "cargo:warning=created data disk {} ({} KiB)",
            data_path.display(),
            image.len() / 1024
        );
    }
    // Re-run only when the build script itself changes: the data disk is
    // intentionally left alone between builds.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-env=DATA_PATH={}", data_path.display());
}
