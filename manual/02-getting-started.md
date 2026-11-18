# Getting started

## Requirements

| Tool | Why | Installed by `scripts\setup.ps1` |
| --- | --- | --- |
| Rust nightly (`x86_64-pc-windows-gnu` host) | `abi_x86_interrupt`, `alloc_error_handler`, Cargo artifact dependencies | yes |
| `x86_64-unknown-none`, `x86_64-unknown-uefi` targets | kernel and UEFI bootloader | yes |
| MinGW-w64 (`gcc`, `as`, `dlltool`) | GNU host linker for build scripts | yes (WinLibs via winget) |
| Oracle VirtualBox 7+ | running the images with a real PS/2 mouse | no |
| PowerShell 5.1+ | helper scripts | Windows built-in |

The repository pins the toolchain per directory (`rustup override`), so the
rest of your machine is untouched. The nightly is required: the kernel uses
`#![feature(abi_x86_interrupt)]`, `#![feature(alloc_error_handler)]` and
`Cargo.toml` uses the `artifact = "bin"` dependency form gated behind
`-Z bindeps`.

## Quick start

```powershell
# 1. Install Rust nightly, the bare-metal targets and MinGW-w64 (idempotent)
powershell -ExecutionPolicy Bypass -File scripts\setup.ps1

# 2. Build the kernel and both disk images (add --release for an optimised build)
powershell -ExecutionPolicy Bypass -File scripts\build.ps1

# 3. Create (or recreate) a VirtualBox VM and boot the BIOS disk image.
#    Omit -Start to create the VM without starting it.
powershell -ExecutionPolicy Bypass -File scripts\virtualbox.ps1 -Start
```

`scripts\virtualbox.ps1` finds VBoxManage on PATH, in the usual install
locations, or at `D:\virtualbox\VBoxManage.exe`; pass
`-VBoxManage "<path>\VBoxManage.exe"` to override. It converts
`gtos-bios.img` and `gtos-data.img` to VDI files under `D:\vbox` (override with
`-VdiDir`) and attaches them as the primary master and slave IDE disks. The VM
is named `GTos` by default and is recreated on each run, so the script is
idempotent.

The guest settings are deliberate:

| Setting | Value |
| --- | --- |
| Firmware | BIOS (the disk image boots in legacy mode) |
| APIC | off |
| Graphics controller | `vboxvga` (supplies the VBE BIOS the bootloader needs) |
| Mouse / keyboard | PS/2 |
| Network | NAT with an Intel 82540EM NIC (`--nic1 nat --nic-type1 82540EM`) |
| Host audio | disabled |

Equivalent raw commands, if you prefer not to use the PowerShell wrappers:

```powershell
cargo build
& 'D:\virtualbox\VBoxManage.exe' list vms
```

`cargo build` writes the disk images and the ISO under
`target\debug\build\gtos\<hash>\out\`; `src\main.rs` is only a build helper and
prints those paths when the build finishes.

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
| `shutdown` | halt the machine (the guest stays powered on in VirtualBox) |

See [Verifying GTos](11-testing.md) for a checklist of things to try once the
VM is running.
