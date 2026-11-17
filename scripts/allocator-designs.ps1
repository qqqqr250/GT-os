<#
.SYNOPSIS
    Builds, boots or tests the standalone `examples/allocator-designs` kernel.

.DESCRIPTION
    The `examples/allocator-designs` crate is the `blog_os` post-11 "done
    properly" kernel: it replaces the `linked_list_allocator` implementation of
    the heap with three designs written from scratch — a bump allocator, a
    linked-list (free-list) allocator and a fixed-size block allocator — and
    installs the last as the `#[global_allocator]`. It carries the post-06
    GDT/TSS/IST, the post-07 PIC/PIT/keyboard, the post-08+09 paging layer and
    post-10's heap interface forward.

    Without -Test the script builds the kernel for x86_64-unknown-none, wraps the
    ELF in a bootable BIOS disk image with the shared `runner` crate, and starts
    QEMU with COM1 on the terminal. The kernel prints the physical-memory offset
    and the active level-4 frame, builds the heap, prints where a `Box`, a `Vec`
    and an `Rc` land, survives many short-lived allocations while one stays live
    (the pattern that starves the bump allocator), then reports how many timer
    interrupts arrived in about two seconds and halts.

    With -Test it runs the crate's in-QEMU test suite instead: `cargo test` links
    the bare-metal test binaries, the runner boots each one in QEMU and maps
    QEMU's exit code back onto cargo. The tests are run from the crate directory
    so that `examples/allocator-designs/.cargo/config.toml` (which points the
    runner at `../../runner`) is the configuration Cargo reads.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\allocator-designs.ps1 -Test
    powershell -ExecutionPolicy Bypass -File scripts\allocator-designs.ps1 -Test -Release
    powershell -ExecutionPolicy Bypass -File scripts\allocator-designs.ps1
    powershell -ExecutionPolicy Bypass -File scripts\allocator-designs.ps1 -NoBoot
#>
[CmdletBinding()]
param(
    [switch]$Test,
    [switch]$Release,
    [switch]$NoBoot,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$QemuArgs
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'env.ps1')

$root = Split-Path -Parent $PSScriptRoot
$manifest = 'examples\allocator-designs\Cargo.toml'
$target = 'x86_64-unknown-none'

if ($Test) {
    if (-not (Test-Toolchain)) { exit 1 }

    Write-Host "running the in-QEMU test suite ($(if ($Release) { 'release' } else { 'debug' }))" -ForegroundColor Cyan
    $cargoArgs = @('test', '--target', $target)
    if ($Release) { $cargoArgs += '--release' }

    # Run from the crate directory: Cargo reads `.cargo/config.toml` from the
    # current directory upwards, and it is that file which wires up the runner.
    Push-Location (Join-Path $root 'examples\allocator-designs')
    try {
        Invoke-Native cargo $cargoArgs
        exit $LASTEXITCODE
    } finally {
        Pop-Location
    }
}

Push-Location $root
try {
    if (-not (Test-Toolchain)) { exit 1 }

    $buildProfile = if ($Release) { 'release' } else { 'debug' }
    $elf = "examples\allocator-designs\target\$target\$buildProfile\rustos-allocator-designs"
    $image = "target\allocator-designs\rustos-allocator-designs-bios.img"

    Write-Host "building $manifest ($buildProfile)" -ForegroundColor Cyan
    $cargoArgs = @('build', '--manifest-path', $manifest, '--target', $target)
    if ($Release) { $cargoArgs += '--release' }
    Invoke-Native cargo $cargoArgs
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    if (-not (Test-Path -LiteralPath $elf)) {
        Write-Host "error: expected the kernel at $elf" -ForegroundColor Red
        exit 1
    }

    Write-Host "building the disk image at $image" -ForegroundColor Cyan
    $imagePath = Invoke-Native cargo @(
        'run', '--quiet', '--manifest-path', 'runner\Cargo.toml', '--', '--image', $elf, $image
    )
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    if ($NoBoot) {
        Write-Host "image: $imagePath" -ForegroundColor Green
        exit 0
    }

    Write-Host 'booting (Ctrl+C or close the window to stop)' -ForegroundColor Cyan
    $qemuCommand = @(
        '-m', '128M', '-no-reboot', '-serial', 'stdio',
        '-drive', "format=raw,file=$imagePath"
    )
    if ($QemuArgs) { $qemuCommand += $QemuArgs }
    Invoke-Native qemu-system-x86_64 $qemuCommand
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
