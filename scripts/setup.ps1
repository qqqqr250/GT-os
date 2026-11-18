<#
.SYNOPSIS
    Installs everything GTos needs on Windows: nightly Rust, the bare-metal
    targets and MinGW-w64 (host linker for build scripts).

.DESCRIPTION
    The script is idempotent: it skips whatever is already installed. It also
    pins this directory to the GNU nightly toolchain with `rustup override`, so
    the rest of the system is left untouched.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\setup.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'env.ps1')

function Write-Step($message) {
    Write-Host ''
    Write-Host "==> $message" -ForegroundColor Cyan
}

Write-Step 'Checking rustup'

$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
    Write-Host 'rustup not found, downloading the official installer...'
    $init = Join-Path $env:TEMP 'rustup-init.exe'
    Invoke-WebRequest -Uri 'https://win.rustup.rs/x86_64' -OutFile $init -UseBasicParsing
    Invoke-Native $init @('-y', '--default-toolchain', 'none', '--profile', 'minimal')
    if ($LASTEXITCODE -ne 0) { throw 'rustup installation failed' }
    Add-ToPath @($cargoBin)
} else {
    Write-Host 'rustup is already installed.'
}

Write-Step 'Installing the nightly GNU toolchain'

$toolchain = 'nightly-x86_64-pc-windows-gnu'
Invoke-Native rustup @('toolchain', 'install', $toolchain, '--profile', 'minimal', '--component', 'llvm-tools-preview,rust-src,clippy,rustfmt')
if ($LASTEXITCODE -ne 0) { throw "failed to install $toolchain" }

Write-Host "Pinning $projectRoot to $toolchain (rustup override)."
Invoke-Native rustup @('override', 'set', $toolchain, '--path', $projectRoot)
if ($LASTEXITCODE -ne 0) { throw "failed to pin $projectRoot to $toolchain" }

Write-Step 'Adding bare-metal targets'

Invoke-Native rustup @('target', 'add', 'x86_64-unknown-none', 'x86_64-unknown-uefi', '--toolchain', $toolchain)
if ($LASTEXITCODE -ne 0) { throw 'failed to add the bare-metal targets' }

Write-Step 'Checking MinGW-w64 (needed to link build scripts on Windows)'

if (-not (Get-Command gcc -ErrorAction SilentlyContinue)) {
    Write-Host 'MinGW-w64 not found, installing WinLibs through winget...'
    Invoke-Native winget @(
        'install', '--id', 'BrechtSanders.WinLibs.POSIX.MSVCRT', '-e',
        '--accept-package-agreements', '--accept-source-agreements', '--silent'
    )
    if ($LASTEXITCODE -ne 0) {
        Write-Warning 'winget could not install MinGW-w64. Install it manually from https://winlibs.com and put its bin directory on PATH.'
    }
    # Re-resolve PATH for this session.
    . (Join-Path $PSScriptRoot 'env.ps1')
} else {
    Write-Host "MinGW-w64 found: $((Get-Command gcc).Source)"
}

Write-Step 'Verifying'

if (Test-Toolchain) {
    Write-Host "rustc: $(Invoke-Native rustc @('--version'))"
    Write-Host "cargo: $(Invoke-Native cargo @('--version'))"
    Write-Host ''
    Write-Host 'Setup complete. Build and run with:' -ForegroundColor Green
    Write-Host '  powershell -ExecutionPolicy Bypass -File scripts\build.ps1'
    Write-Host '  powershell -ExecutionPolicy Bypass -File scripts\virtualbox.ps1 -Start'
} else {
    Write-Host ''
    Write-Warning 'Setup finished with missing tools, see the list above.'
    exit 1
}
