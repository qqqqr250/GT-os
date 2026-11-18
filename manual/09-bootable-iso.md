# Bootable ISO and Ventoy

`cargo build` also writes `gtos.iso` next to the disk images, under
`target\<profile>\build\gtos\<hash>\out\`:

```
target\debug\build\gtos\*\out\gtos.iso
```

The ISO is written by `build\iso.rs`, a small dependency-free ISO9660 + El
Torito writer (there is no `xorriso`/`mkisofs` requirement). It contains:

- a Primary Volume Descriptor and an El Torito Boot Record Volume Descriptor,
- a minimal directory tree with `EFI\BOOT\BOOTX64.EFI`,
- a boot catalog whose default entry is the BIOS MBR image (hard-disk
  emulation) and whose EFI section points at the UEFI FAT image (no emulation).

## Booting the ISO in VirtualBox

`scripts\virtualbox.ps1` boots the BIOS disk image, not the ISO. To boot from
the disc instead, attach `gtos.iso` to a virtual optical drive and make the DVD
drive the first boot device. For example, after creating the VM:

```powershell
$iso = (Get-ChildItem target\debug\build\gtos\*\out\gtos.iso | Select-Object -Last 1).FullName
& 'D:\virtualbox\VBoxManage.exe' storageattach GTos --storagectl IDE --port 1 --device 0 --type dvddrive --medium $iso
& 'D:\virtualbox\VBoxManage.exe' modifyvm GTos --boot1 dvd
& 'D:\virtualbox\VBoxManage.exe' startvm GTos
```

You can do the same in the VirtualBox GUI: Storage → add a CD/DVD device to the
IDE controller, pick `gtos.iso`, then set DVD as the first boot device in System
→ Boot Order.

## Using the ISO on a Ventoy USB stick

1. Build it (`scripts\build.ps1`, or `scripts\build.ps1 --release`).
2. Copy `gtos.iso` to the first, exFAT/NTFS partition of a Ventoy drive.
3. Boot the drive and pick `gtos.iso` from the Ventoy menu.

Ventoy's UEFI path reads the EFI El Torito entry and chainloads its own
`ventoy_x86_64.efi`. Ventoy's legacy-BIOS path loads the El Torito BIOS image
through iPXE, which supplies the extended INT 13h calls the `bootloader` stage 1
needs. That path has **not** been tested on real hardware.

> **BIOS CD-ROM caveat.** A *plain* BIOS boot from the ISO (without Ventoy) does
> **not** work: typical virtual and legacy BIOS implementations only offer
> old-style INT 13h for their El Torito emulated drive, while the `bootloader`
> stage 1 requires INT 13h extensions (AH=0x41/0x42). Booting the BIOS path
> directly from the ISO therefore prints nothing. Use the disk image
> (`scripts\virtualbox.ps1 -Start`), Ventoy, or a real optical drive for the
> BIOS path.

## Ventoy fallback: raw disk images

Ventoy also boots raw `.img` files, so `gtos-bios.img` and `gtos-uefi.img`
can be copied to the drive directly if you prefer not to use the ISO. Pick the
image that matches the firmware you are booting.
