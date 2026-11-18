//! Build-only helper for the `GTos` disk images.
//!
//! `build.rs` compiles the bare-metal kernel artifact and produces the bootable
//! BIOS/UEFI images, the ISO and the FAT16 data disk. This binary exists so that
//! `cargo build` has a host target to drive that build script; boot the results
//! in `VirtualBox` with `scripts\virtualbox.ps1`.

fn main() {
    println!("GTos build complete.");
    println!("BIOS image: {}", env!("BIOS_PATH"));
    println!("UEFI image: {}", env!("UEFI_PATH"));
    println!("ISO image:  {}", env!("ISO_PATH"));
    println!("Data disk:  {}", env!("DATA_PATH"));
    println!();
    println!("To create and boot a VirtualBox VM, run:");
    println!("  powershell -ExecutionPolicy Bypass -File scripts\\virtualbox.ps1 -Start");
}
