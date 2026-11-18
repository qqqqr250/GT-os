# Bootable ISO and Ventoy

`cargo build` also writes `rustos.iso` next to the disk images, under
`target\<profile>\build\rustos\<hash>\out\`:

```
target\debug\build\rustos\*\out\rustos.iso
```

The ISO is written by `build\iso.rs`, a small dependency-free ISO9660 + El
Torito writer (there is no `xorriso`/`mkisofs` requirement). It contains:

- a Primary Volume Descriptor and an El Torito Boot Record Volume Descriptor,
- a minimal directory tree with `EFI\BOOT\BOOTX64.EFI`,
- a boot catalog whose default entry is the BIOS MBR image (hard-disk
  emulation) and whose EFI section points at the UEFI FAT image (no emulation).

Boot it in QEMU from a virtual CD-ROM with:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\run.ps1 uefi iso
# or: cargo run -- uefi iso
```

## Using the ISO on a Ventoy USB stick

1. Build it (`scripts\build.ps1`, or `scripts\build.ps1 --release`).
2. Copy `rustos.iso` to the first, exFAT/NTFS partition of a Ventoy drive.
3. Boot the drive and pick `rustos.iso` from the Ventoy menu.

Ventoy's UEFI path reads the EFI El Torito entry and chainloads its own
`ventoy_x86_64.efi`; this has been verified in QEMU (`scripts\smoke-test.ps1
-Firmware uefi -Iso`). Ventoy's legacy-BIOS path loads the El Torito BIOS image
through iPXE, which supplies the extended INT 13h calls the `bootloader` stage 1
needs. That path has **not** been tested on real hardware.

> **BIOS CD-ROM caveat.** A *plain* BIOS boot from the ISO (SeaBIOS/`-cdrom`
> without Ventoy) does **not** work: SeaBIOS only implements old-style INT 13h
> for its El Torito emulated drive, while the `bootloader` stage 1 requires
> INT 13h extensions (AH=0x41/0x42). Booting the BIOS path directly from the ISO
> therefore prints nothing. Use the disk image (`scripts\run.ps1 bios`), Ventoy,
> or a real optical drive for the BIOS path.

## Ventoy fallback: raw disk images

Ventoy also boots raw `.img` files, so `rustos-bios.img` and `rustos-uefi.img`
can be copied to the drive directly if you prefer not to use the ISO. Pick the
image that matches the firmware you are booting.
