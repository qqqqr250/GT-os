# Disks and the FAT16 filesystem

The kernel talks to the primary IDE channel with a polled **ATA PIO** driver
(`kernel/src/ata.rs`, LBA28, ports `0x1F0`/`0x3F6`). On top of it sits a
read/write **FAT16** implementation (`kernel/src/fat.rs`) with `mount`, `list`,
`read`, `write`, `delete` and `format`, exposed through the `disks`, `ls`, `cat`,
`write`, `rm`, `play` and `format` shell commands.

Two drives are attached: the boot image on the primary master (drive 0) and a
16 MiB data disk on the primary slave (drive 1). `build.rs` creates the data disk
at the project root as `gtos-data.img` **only if it is missing**, so anything
you write from inside GTos survives a rebuild. It is seeded once with
`README.TXT`, a tone (`BEEP.WAV`) and a ring-3 program (`HELLO.BIN`). Delete the
file and rebuild to regenerate a pristine image.

```
gtos> disks
drive 0 (master): 5 MiB, 11202 sectors
  model: QEMU HARDDISK
drive 1 (slave): 16 MiB, 32768 sectors
  model: QEMU HARDDISK
gtos> write NOTES.TXT hello from GTos
wrote 19 bytes to NOTES.TXT
gtos> cat NOTES.TXT
hello from GTos
```
