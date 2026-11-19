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
| `logo` | redraw the GT logo and banner |
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
| `pkg [list]` | list packages (installed, auto or available) |
| `pkg search <text>` | search package names and descriptions |
| `pkg info <name>` | show metadata, dependencies and reverse dependencies |
| `pkg depends <name>` | show build and runtime dependency edges |
| `pkg recipe <name>` | show cookbook-style recipe metadata |
| `pkg install <name>...` | install packages and their missing dependencies |
| `pkg remove <name>...` | remove packages and orphaned automatic dependencies |
| `pkg mark <manual\|auto> <name>...` | change a package's install flag |
| `pkg pack <out> <file>...` | build a `.pkga` archive on the data disk |
| `pkg archive <file>` | verify a `.pkga` archive on the data disk |
| `panic` | trigger a kernel panic (tests the panic path) |
| `reboot` | pulse the keyboard controller reset line |
| `shutdown` | halt the machine (the guest stays powered on in VirtualBox) |

### Packages

`pkg` tracks packages as a dependency graph, following the model of Redox's
[pkgutils](https://gitlab.redox-os.org/redox-os/pkgutils). Each entry records
its dependencies and the packages that depend on it (reverse edges), whether
it was installed manually or pulled in automatically, and whether it is
protected. Installing resolves the full transitive closure; removing refuses
to break a live dependent and cleans up automatic dependencies that become
orphaned. Protected core packages (`base`, `terminal` and the default apps)
cannot be removed.

There is no remote repository yet — the catalog is compiled into the kernel,
so `pkg install` only changes this in-memory state. Package names follow the
Redox grammar `[host:|target:]recipe[.suffix]`; for example `host:foo` and
`foo.pkg` are distinct, valid names. Try `pkg install calculator`: it pulls in
`mathlib` automatically, and `pkg remove calculator` removes both again unless
you first run `pkg mark manual mathlib`.

Each catalog entry also has a **recipe** modelled on Redox's
[cookbook](https://gitlab.redox-os.org/redox-os/cookbook): a `source` (`builtin`,
`archive:<file>` or `meta`), a build kind, and separate `[build]` and `[package]`
dependency lists. Build dependencies are needed only to compile a package;
runtime dependencies are the ones the install graph follows. `pkg recipe <name>`
prints the recipe, for example:

```
pkg recipe calculator
pkg recipe desktop
```

`desktop` is a meta package — like cookbook's `groups` category — with no files
of its own, so `pkg install desktop` pulls in the whole default desktop as one
transaction. `demo` demonstrates an archive-sourced entry (`archive:DEMO.PKGA`);
since GTos has no in-guest build backend, installing it tracks state only.

### Package archives

Inspired by Redox's [pkgar](https://gitlab.redox-os.org/redox-os/pkgar) format,
`kernel/src/pkgar.rs` implements a small **`.pkga`** archive: a fixed 136-byte
header, 308-byte entries holding a SHA-256 file hash, an offset, a size, a mode
and a validated path, followed by the concatenated file data. The header hashes
the entire entry table, so a single flipped bit in any entry is detected before
a file is read; every file is hashed again on extraction. Paths are checked for
`..`, absolute components and Windows drive syntax so an archive cannot escape
the FAT root directory.

The layout mirrors pkgar's reproducible, hashed-header design but is **not**
binary-compatible with Redox: GTos uses SHA-256 instead of Blake3/Ed25519 and a
different magic, because the kernel has neither a signature scheme nor a remote
repository. Try it in the VM:

```
write HELLO.TXT Hello from GTos
pkg pack TEST.PKGA HELLO.TXT
pkg archive TEST.PKGA
```

The host build tool `cargo run --bin pkgar -- out.pkga file...` creates the same
bytes, so archives can also be prepared outside the guest and copied to the
data disk.

See [Verifying GTos](11-testing.md) for a checklist of things to try once the
VM is running.
