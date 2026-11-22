# Troubleshooting

**`failed to run custom build command` / `dlltool` or `as` not found.**
The GNU host toolchain needs a MinGW-w64 installation; rustup's bundled
`rust-mingw` does not provide a full binutils. Run `scripts\setup.ps1`, or put
WinLibs/MSYS2 `mingw64\bin` on PATH. `scripts\env.ps1` also searches the winget
package cache, so a winget install works without a new shell.

**`VBoxManage.exe` not found.** Install VirtualBox 7+ or pass the full path to
`scripts\virtualbox.ps1`, e.g.
`-VBoxManage "D:\virtualbox\VBoxManage.exe"`. The script also checks PATH and
the usual install locations.

**`error: the option `Z` is only accepted on the nightly compiler`.** The
project needs nightly for Cargo artifact dependencies and two unstable kernel
features. `scripts\setup.ps1` pins this directory with
`rustup override set nightly-x86_64-pc-windows-gnu`.

**`running scripts is disabled on this system`.** Use
`powershell -ExecutionPolicy Bypass -File scripts\<script>.ps1` (the wrappers
assume nothing about your machine-wide policy).

**Cargo output shows up as red errors, or a successful run reports exit code 1.**
Cargo and rustup print their progress to stderr. Windows PowerShell turns a
native command's stderr into a *terminating* error under
`$ErrorActionPreference = 'Stop'` as soon as the stream is captured or redirected,
so a log (or a parent script doing `script.ps1 2>&1`) shows a bogus
`NativeCommandError` and a non-zero exit code even when the tool succeeded. Every
helper script runs its native commands through `Invoke-Native` (in
`scripts\env.ps1`), which relaxes the preference for the duration of the call
while still forwarding the real `$LASTEXITCODE`. To capture cargo's output
yourself, run it through `cmd`, e.g. `cmd /c "cargo build 2>&1"`.

**UEFI image or ISO will not boot.** `scripts\virtualbox.ps1` deliberately
creates a BIOS VM and boots `gtos-bios.img`; no firmware files are needed. The
UEFI image and ISO expect firmware that can load `EFI\BOOT\BOOTX64.EFI`, such as
Ventoy's UEFI path. See [Bootable ISO and Ventoy](09-bootable-iso.md).

**No framebuffer.** When firmware reports no linear framebuffer the console
falls back to the legacy VGA text buffer (see
[Console and VGA fallback](03-architecture.md#console-fallback)); only when there
is also no physical-memory window does it become serial-only, and the boot log
says so. The VirtualBox VM uses the `vboxvga` graphics controller because the
bootloader needs its VBE services.

**Serial input seems dead.** Verify you are connected to COM1 *after* the boot
log starts; bytes sent while the guest's UART FIFO is unconfigured can be lost.
The shell is already running at boot, so the first byte you send is read as shell
input: a bare Enter is treated as an empty line.

**Keyboard input seems dead in the VM.** VirtualBox only forwards key presses
once the guest has captured the keyboard: click once inside the VM window, then
type (the **Host key**, right Ctrl by default, releases it). The shell ignores the
mouse, but guest programs can read pointer events from the `input` device.
