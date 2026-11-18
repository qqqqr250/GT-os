# Shared environment for the GTos helper scripts.
#
# The toolchain lives outside the project (rustup, MinGW-w64, QEMU), so every
# script dot-sources this file to put the pieces on PATH for its own session
# without permanently changing the user's environment.

# Cargo, rustup and QEMU print their progress to stderr, and every script here
# runs with `$ErrorActionPreference = 'Stop'`. A native command's stderr then
# becomes a *terminating* error as soon as the stream is captured or redirected
# (a CI log, or a parent script calling `script.ps1 2>&1`), which surfaces as a
# bogus `NativeCommandError` and a non-zero exit code even when the tool
# succeeded. Route every native command through `Invoke-Native`, which relaxes
# the preference for the duration of the call; callers still inspect
# `$LASTEXITCODE`, so real failures are not hidden.
#
# PowerShell 7.4+ has a second switch for this, `$PSNativeCommandUseErrorActionPreference`
# (absent on Windows PowerShell 5.1, hence the guard); turn it off so native
# stderr stays ordinary output there too.
if (Get-Variable -Name PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

function Invoke-Native {
    param(
        [Parameter(Mandatory = $true, Position = 0)]
        [string]$Command,
        [Parameter(Position = 1)]
        [string[]]$Arguments
    )

    # Preference variables are dynamically scoped, so `Continue` only affects the
    # native call below, never the caller's `$ErrorActionPreference = 'Stop'`.
    # Merge stderr into the pipeline (so it cannot become a terminating error)
    # and hand it back to the real console error stream as plain text: no bogus
    # `NativeCommandError`, no red noise, and stdout stays clean for callers that
    # capture it. `$LASTEXITCODE` still reports the command's own exit code.
    $ErrorActionPreference = 'Continue'
    & $Command @Arguments 2>&1 | ForEach-Object {
        if ($_ -is [System.Management.Automation.ErrorRecord]) {
            $line = $_.ToString()
            try { [Console]::Error.WriteLine($line) } catch { Write-Host $line }
        } else {
            $_
        }
    }
}

function Add-ToPath {
    param([string[]]$Directories)

    foreach ($dir in $Directories) {
        if ([string]::IsNullOrWhiteSpace($dir)) { continue }
        if (-not (Test-Path -LiteralPath $dir)) { continue }
        $trimmed = $dir.TrimEnd('\')
        $already = $env:Path -split ';' | Where-Object { $_.TrimEnd('\') -eq $trimmed }
        if (-not $already) {
            $env:Path = "$trimmed;$env:Path"
        }
    }
}

# rustup / cargo
Add-ToPath (Join-Path $env:USERPROFILE '.cargo\bin')

# GNU host toolchain: rustc needs gcc, as and dlltool for build scripts.
$mingwRoots = @(
    'C:\msys64\mingw64',
    'C:\mingw64',
    'C:\Program Files\mingw64',
    'C:\Program Files (x86)\mingw64'
)

# WinLibs installs either machine-wide or into the winget package cache.
$searchRoots = @('C:\Program Files', 'C:\Program Files (x86)', (Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages'))
foreach ($searchRoot in $searchRoots) {
    if (-not (Test-Path -LiteralPath $searchRoot)) { continue }
    Get-ChildItem -LiteralPath $searchRoot -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -like '*WinLibs*' -or $_.Name -like '*mingw*' } |
        ForEach-Object { $mingwRoots += $_.FullName }
}

foreach ($mingwRoot in $mingwRoots) {
    foreach ($suffix in @('bin', 'mingw64\bin')) {
        $bin = Join-Path $mingwRoot $suffix
        if (Test-Path -LiteralPath (Join-Path $bin 'gcc.exe')) {
            Add-ToPath @($bin)
            break
        }
    }
}

# QEMU
Add-ToPath @('C:\Program Files\qemu', 'C:\Program Files (x86)\qemu')

function Test-Toolchain {
    $missing = @()

    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        $missing += 'cargo (run scripts\setup.ps1)'
    }
    if (-not (Get-Command gcc -ErrorAction SilentlyContinue)) {
        $missing += 'MinGW-w64 gcc (run scripts\setup.ps1)'
    }
    if (-not (Get-Command qemu-system-x86_64 -ErrorAction SilentlyContinue)) {
        $missing += 'qemu-system-x86_64 (run scripts\setup.ps1)'
    }

    if ($missing.Count -gt 0) {
        Write-Host 'Missing tools:' -ForegroundColor Yellow
        foreach ($tool in $missing) { Write-Host "  - $tool" }
        return $false
    }
    return $true
}
