# Troubleshooting

**`failed to run custom build command` / `dlltool` or `as` not found.**
The GNU host toolchain needs a MinGW-w64 installation; rustup's bundled
`rust-mingw` does not provide a full binutils. Run `scripts\setup.ps1`, or put
WinLibs/MSYS2 `mingw64\bin` on PATH. `scripts\env.ps1` also searches the winget
package cache, so a winget install works without a new shell.

**`qemu-system-x86_64` not found.** Install QEMU (`winget install
SoftwareFreedomConservancy.QEMU`) or set the `QEMU` environment variable to the
full path of the executable. `scripts\env.ps1` adds `C:\Program Files\qemu`.

**`error: the option `Z` is only accepted on the nightly compiler`.** The
project needs nightly for Cargo artifact dependencies and two unstable kernel
features. `scripts\setup.ps1` pins this directory with
`rustup override set nightly-x86_64-pc-windows-gnu`.

**`running scripts is disabled on this system`.** Use
`powershell -ExecutionPolicy Bypass -File scripts\<script>.ps1` (the wrappers
assume nothing about your machine-wide policy).

**Cargo output shows up as red errors, or a successful run reports exit code 1.**
Cargo, rustup and QEMU print their progress to stderr. Windows PowerShell turns a
native command's stderr into a *terminating* error under
`$ErrorActionPreference = 'Stop'` as soon as the stream is captured or redirected,
so a CI log (or a parent script doing `script.ps1 2>&1`) shows a bogus
`NativeCommandError` and a non-zero exit code even when the tool succeeded. Every
helper script runs its native commands through `Invoke-Native` (in
`scripts\env.ps1`), which relaxes the preference for the duration of the call
while still forwarding the real `$LASTEXITCODE`. To capture cargo's output
yourself, run it through `cmd`, e.g. `cmd /c "cargo xtest 2>&1"`.

**UEFI image will not boot.** It needs OVMF: `edk2-x86_64-code.fd` (and
optionally `edk2-i386-vars.fd`) in `share\` next to the QEMU binaries, or in
`/usr/share/OVMF`. The QEMU winget package ships them; the launcher prints a
clear error if they are missing. BIOS booting needs no firmware files.

**No framebuffer.** When firmware reports no linear framebuffer the console
falls back to the legacy VGA text buffer (see
[Console and VGA fallback](03-architecture.md#console-fallback)); only when there
is also no physical-memory window does it become serial-only, and the boot log
says so. Use `scripts\run.ps1 --headless` to run without a window.

**Serial input seems dead.** Verify you are connected to COM1 *after* the boot
log starts; bytes sent while the guest's UART FIFO is unconfigured can be lost.
The smoke test waits for the kernel to report its interrupts as live before it
sends its first byte for exactly this reason. Remember that the first byte also
hands the desktop over to the shell, and the shell then reads it as its first
input (the smoke test sends a bare Enter, which the shell treats as an empty
line).

**The desktop cursor does not move.** QEMU only forwards mouse motion once its
pointer is grabbed: click once inside the guest window, then move the mouse
(**Ctrl+Alt+G** releases the pointer). The PS/2 driver itself needs no host-side
configuration, and `-display none` runs have no pointer at all.
