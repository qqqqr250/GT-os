<#
.SYNOPSIS
    Builds the kernel and both disk images.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\build.ps1
    powershell -ExecutionPolicy Bypass -File scripts\build.ps1 --release
#>
[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CargoArgs
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'env.ps1')

Push-Location (Split-Path -Parent $PSScriptRoot)
try {
    if (-not (Test-Toolchain)) { exit 1 }
    $buildArgs = @('build')
    if ($CargoArgs) { $buildArgs += $CargoArgs }
    Invoke-Native cargo $buildArgs
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
