<#
.SYNOPSIS
    Boots RustOS in QEMU.

.DESCRIPTION
    Passes everything after the script name to the launcher, e.g.:
      scripts\run.ps1 uefi
      scripts\run.ps1 --headless
      scripts\run.ps1 -- -cpu max -smp 2

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\run.ps1
#>
[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$LauncherArgs
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'env.ps1')

Push-Location (Split-Path -Parent $PSScriptRoot)
try {
    if (-not (Test-Toolchain)) { exit 1 }
    # `--` separates cargo's own flags from the launcher's arguments.
    $cargoArgs = @('run', '--')
    if ($LauncherArgs) { $cargoArgs += $LauncherArgs }
    Invoke-Native cargo $cargoArgs
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
