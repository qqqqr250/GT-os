<#
.SYNOPSIS
    Builds, boots or tests the standalone `examples/minimal` kernel.

.DESCRIPTION
    Builds the minimal "post-02, done properly" kernel for x86_64-unknown-none,
    wraps the ELF in a bootable BIOS disk image with the shared `runner` crate,
    and starts QEMU with COM1 on the terminal.

    The display ends up in graphics mode (the bootloader always asks for a linear
    framebuffer), so the VGA text buffer is not shown; COM1 carries the real
    output. Use -NoBoot to only build the image.

    With -Test the script runs the crate's in-QEMU integration test
    (`tests/qemu.rs`) instead: the test must be run from the crate directory so
    that `examples/minimal/.cargo/config.toml` (which points the runner at
    `../../runner`) is the configuration Cargo reads.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\minimal.ps1
    powershell -ExecutionPolicy Bypass -File scripts\minimal.ps1 -Release
    powershell -ExecutionPolicy Bypass -File scripts\minimal.ps1 -NoBoot
    powershell -ExecutionPolicy Bypass -File scripts\minimal.ps1 -Test
    powershell -ExecutionPolicy Bypass -File scripts\minimal.ps1 -- -smp 2
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

if ($Test) {
    if (-not (Test-Toolchain)) { exit 1 }

    $cargoArgs = @('test', '--target', 'x86_64-unknown-none', '--features', 'qemu-test', '--test', 'qemu')
    if ($Release) { $cargoArgs += '--release' }

    Push-Location (Join-Path $root 'examples\minimal')
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
    $manifest = 'examples\minimal\Cargo.toml'
    $target = 'x86_64-unknown-none'
    $elf = "examples\minimal\target\$target\$buildProfile\rustos-minimal"
    $image = "target\minimal\rustos-minimal-bios.img"

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
