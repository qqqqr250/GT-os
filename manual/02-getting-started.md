# Getting started

## Requirements

| Tool | Why | Installed by `scripts\setup.ps1` |
| --- | --- | --- |
| Rust nightly (`x86_64-pc-windows-gnu` host) | `abi_x86_interrupt`, `alloc_error_handler`, Cargo artifact dependencies | yes |
| `x86_64-unknown-none`, `x86_64-unknown-uefi` targets | kernel and UEFI bootloader | yes |
| MinGW-w64 (`gcc`, `as`, `dlltool`) | GNU host linker for build scripts | yes (WinLibs via winget) |
| QEMU 8+ (`qemu-system-x86_64`) | running the images; OVMF `edk2-*.fd` for UEFI | yes |
| Oracle `VirtualBox` 7+ (optional) | alternate GUI host with a real PS/2 mouse | no |
| PowerShell 5.1+ | helper scripts | Windows built-in |

The repository pins the toolchain per directory (`rustup override`), so the
rest of your machine is untouched. The nightly is required: the kernel uses
`#![feature(abi_x86_interrupt)]`, `#![feature(alloc_error_handler)]` and
`#![feature(custom_test_frameworks)]` (the in-QEMU test harness), and
`Cargo.toml` uses the `artifact = "bin"` dependency form gated behind
`-Z bindeps`.

## Quick start

```powershell
# 1. Install Rust nightly, MinGW-w64 and QEMU (idempotent, no admin needed)
powershell -ExecutionPolicy Bypass -File scripts\setup.ps1

# 2. Build the kernel and both disk images (add --release for an optimised build)
powershell -ExecutionPolicy Bypass -File scripts\build.ps1

# 3. Boot it (BIOS by default; use "uefi" for OVMF, "iso" for the disc image)
powershell -ExecutionPolicy Bypass -File scripts\run.ps1
powershell -ExecutionPolicy Bypass -File scripts\run.ps1 uefi
powershell -ExecutionPolicy Bypass -File scripts\run.ps1 uefi iso

# 4. Run the kernel's in-QEMU tests (unit + integration; needs QEMU)
cargo xtest

# 4b. (Optional) create a VirtualBox VM with the BIOS disk, the FAT16 data
#     disk and a NAT Intel 82540EM NIC; add -Start to boot it immediately
powershell -ExecutionPolicy Bypass -File scripts\virtualbox.ps1

# 5. Prove it works without clicking anything
powershell -ExecutionPolicy Bypass -File scripts\smoke-test.ps1
powershell -ExecutionPolicy Bypass -File scripts\smoke-test.ps1 -Firmware uefi
powershell -ExecutionPolicy Bypass -File scripts\smoke-test.ps1 -Release
powershell -ExecutionPolicy Bypass -File scripts\smoke-test.ps1 -Firmware uefi -Iso
```

`scripts\run.ps1` forwards its arguments to the host launcher, so
`scripts\run.ps1 --headless`, `scripts\run.ps1 uefi`, and
`scripts\run.ps1 -- -smp 2 -cpu max` all work. Extra QEMU arguments go after
`--`.

Equivalent raw commands, if you prefer not to use the PowerShell wrappers:

```powershell
cargo build                 # produces target\debug\build\gtos\*\out\gtos-{bios,uefi}.img and gtos.iso
cargo run -- bios --headless
cargo run -- uefi iso       # boot the ISO from a virtual CD-ROM
```

`scripts\screenshot.ps1` boots the BIOS image headlessly, waits for the Windows
XP-style desktop to render, moves the pointer and writes `target\screenshot.png`
via QEMU's `screendump` (useful for docs).

## Shell commands

| Command | Effect |
| --- | --- |
| `help` | list commands |
| `echo <text>` | echo text back |
| `clear` | clear the framebuffer |
| `mem` | dump the bootloader memory map |
| `heap` | show the kernel heap window |
| `alloc` | allocate and free 78 KiB on the heap |
| `uptime` | seconds since boot, from the PIT |
| `cpu` | CPUID vendor and brand string |
| `logo` | redraw the banner |
| `about` | what this OS is |
| `net` | show the adapter, address, gateway and DNS server |
| `dhcp` | (re)acquire an address from a DHCP server |
| `ping <ip>` | send an ICMP echo request |
| `dns <name>` | resolve a host name through the DNS proxy |
| `http <url>` | fetch a page over HTTP and print the response |
| `beep` | play a short tone through AC'97 audio |
| `tone <hz> [ms]` | play a tone of the given frequency and duration |
| `disks` | list the ATA drives and their sizes |
| `ls` / `dir` | list the FAT16 data disk's root directory |
| `cat <file>` / `type` | print a file from the data disk |
| `write <file> <text>` | create or overwrite a file (or read lines until `.`) |
| `rm <file>` | delete a file |
| `play <file>` | play a WAV file from the data disk through AC'97 |
| `format` | reformat the FAT16 data disk |
| `run [file]` | run a flat ring-3 program (no file: the built-in demo) |
| `pkg list` | list the in-OS package registry |
| `panic` | trigger a kernel panic (tests the panic path) |
| `reboot` | pulse the keyboard controller reset line |
| `shutdown` | power off via QEMU's `isa-debug-exit` (halts elsewhere) |
