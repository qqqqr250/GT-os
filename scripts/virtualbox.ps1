<#
.SYNOPSIS
    Creates (or recreates) a VirtualBox VM that boots GTos from the built BIOS
    disk image, with the FAT16 data disk attached as the primary IDE slave.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\virtualbox.ps1
    powershell -ExecutionPolicy Bypass -File scripts\virtualbox.ps1 -VdiDir E:\vbox -Start
#>
[CmdletBinding()]
param(
    [string]$VdiDir = 'D:\vbox',
    [string]$VmName = 'GTos',
    [string]$VBoxManage = '',
    [switch]$Start
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'env.ps1')

$projectRoot = Split-Path -Parent $PSScriptRoot

# VBoxManage is not added to PATH by the Windows installer, so probe the usual
# locations as well. -VBoxManage overrides all of them.
$candidates = @(
    $VBoxManage,
    (Get-Command VBoxManage -ErrorAction SilentlyContinue).Source,
    (Join-Path $env:ProgramFiles 'Oracle\VirtualBox\VBoxManage.exe'),
    (Join-Path ${env:ProgramFiles(x86)} 'Oracle\VirtualBox\VBoxManage.exe'),
    (Join-Path $env:LOCALAPPDATA 'Programs\Oracle\VirtualBox\VBoxManage.exe'),
    'D:\virtualbox\VBoxManage.exe'
) | Where-Object { $_ -and (Test-Path -LiteralPath $_) }
if (-not $candidates) {
    throw 'VBoxManage.exe not found. Pass -VBoxManage "C:\path\to\VBoxManage.exe".'
}
# Force an array: a single pipeline result would otherwise be a scalar string,
# and $candidates[0] would be its first character ('D') rather than the path.
$vbm = @($candidates)[0]
Write-Host "Using VBoxManage: $vbm"

$bios = Get-ChildItem -Path (Join-Path $projectRoot 'target\debug\build\gtos\*\out\gtos-bios.img') |
    Sort-Object LastWriteTime | Select-Object -Last 1
if (-not $bios) {
    throw 'gtos-bios.img not found. Build first with scripts\build.ps1.'
}
$data = Join-Path $projectRoot 'gtos-data.img'
if (-not (Test-Path -LiteralPath $data)) {
    throw 'gtos-data.img not found. Build first with scripts\build.ps1.'
}

New-Item -ItemType Directory -Force -Path $VdiDir | Out-Null
$biosVdi = Join-Path $VdiDir 'gtos-bios.vdi'
$dataVdi = Join-Path $VdiDir 'gtos-data.vdi'

function Invoke-Vbm([string[]]$Arguments) {
    Write-Host "VBoxManage $($Arguments -join ' ')"
    & $vbm @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "VBoxManage failed with exit code $LASTEXITCODE"
    }
}

# Recreate the VM so rerunning the script is idempotent.
if (& $vbm list vms | Select-String -SimpleMatch "`"$VmName`"") {
    # Power off if it is running. A stopped VM makes this command complain on
    # stderr, which PowerShell would turn into a terminating error, so lower the
    # preference for this one call and ignore the result.
    $ErrorActionPreference = 'Continue'
    & $vbm controlvm $VmName poweroff 2>&1 | Out-Null
    $ErrorActionPreference = 'Stop'
    Start-Sleep -Seconds 2
    Invoke-Vbm @('unregistervm', $VmName, '--delete')
}

# convertfromraw refuses to overwrite an existing file.
foreach ($path in @($biosVdi, $dataVdi)) {
    if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force }
}
Invoke-Vbm @('convertfromraw', $bios.FullName, $biosVdi, '--format', 'VDI')
Invoke-Vbm @('convertfromraw', $data, $dataVdi, '--format', 'VDI')

Invoke-Vbm @('createvm', '--name', $VmName, '--ostype', 'Other', '--register')
# VirtualBox 7.x renamed --vga to --graphicscontroller and --audio to
# --audio-driver/--audio-enabled. vboxvga supplies the VBE BIOS the bootloader
# needs to hand the kernel a graphics framebuffer.
Invoke-Vbm @('modifyvm', $VmName,
    '--memory', '256', '--graphicscontroller', 'vboxvga', '--vram', '8',
    '--audio-enabled', 'off', '--audio-driver', 'none',
    '--mouse', 'ps2', '--keyboard', 'ps2',
    '--nic1', 'nat', '--nic-type1', '82540EM',
    '--firmware', 'bios', '--apic', 'off', '--boot1', 'disk')
Invoke-Vbm @('storagectl', $VmName, '--name', 'IDE', '--add', 'ide')
Invoke-Vbm @('storageattach', $VmName, '--storagectl', 'IDE',
    '--port', '0', '--device', '0', '--type', 'hdd', '--medium', $biosVdi)
Invoke-Vbm @('storageattach', $VmName, '--storagectl', 'IDE',
    '--port', '0', '--device', '1', '--type', 'hdd', '--medium', $dataVdi)

Write-Host ''
Write-Host "VM '$VmName' is ready. BIOS image: $($bios.FullName)"
Write-Host 'Keyboard: click inside the VM window once so VirtualBox captures input.'
Write-Host 'GTos boots straight to the text shell; type help to list commands.'

if ($Start) {
    Invoke-Vbm @('startvm', $VmName)
}
