<#
.SYNOPSIS
    Boots RustOS headlessly and captures the Windows XP-style desktop to
    target\screenshot.png.

.DESCRIPTION
    COM1 and the QEMU monitor are exposed as TCP sockets. The script boots the
    image with a VGA adapter (the desktop needs a framebuffer), waits for the
    kernel to report its device interrupts as live, injects a small PS/2 mouse
    movement so the pointer is drawn at a known place, then asks the monitor for
    a `screendump` (a P6 PPM). The PPM is converted to PNG with System.Drawing
    and written to target\screenshot.png. Useful for keeping docs\screenshot.png
    up to date.

    The PPM is written to %TEMP% because QEMU's `screendump` does not accept a
    path containing spaces, and the project directory has one.

.PARAMETER Firmware
    Which disk image to boot: `bios` (default) or `uefi` (through OVMF).

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\screenshot.ps1
    powershell -ExecutionPolicy Bypass -File scripts\screenshot.ps1 -Firmware uefi
#>
[CmdletBinding()]
param(
    [ValidateSet('bios', 'uefi')]
    [string]$Firmware = 'bios'
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'env.ps1')
$qemu = (Get-Command qemu-system-x86_64 -ErrorAction Stop).Source
$root = $projectRoot
$image = Get-ChildItem -Path "$root\target" -Recurse -Filter "rustos-$Firmware.img" |
    Sort-Object LastWriteTime | Select-Object -Last 1
if (-not $image) { throw "the $Firmware disk image was not found under target\" }

function Get-FreePort {
    $l = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $l.Start(); $p = $l.LocalEndpoint.Port; $l.Stop(); return $p
}
$serialPort = Get-FreePort
$monitorPort = Get-FreePort

$png = Join-Path $root 'target\screenshot.png'
# `screendump` cannot handle spaces in the path; %TEMP% is space-free here.
$ppm = Join-Path $env:TEMP 'rustos-screenshot.ppm'

$qemuArgs = @(
    '-m', '256M', '-smp', '1', '-no-reboot',
    '-display', 'none', '-vga', 'std',
    '-nic', 'user,model=rtl8139',
    '-audiodev', 'none,id=snd0',
    '-device', 'AC97,audiodev=snd0',
    '-serial', "tcp:127.0.0.1:$serialPort,server=on,wait=off",
    '-monitor', "tcp:127.0.0.1:$monitorPort,server=on,wait=off"
)

if ($Firmware -eq 'uefi') {
    $share = Join-Path (Split-Path -Parent $qemu) 'share'
    $code = Join-Path $share 'edk2-x86_64-code.fd'
    $vars = Join-Path $share 'edk2-i386-vars.fd'
    if (-not (Test-Path -LiteralPath $code)) {
        throw "OVMF code image not found at $code (cannot boot the UEFI image)"
    }
    $qemuArgs += @('-drive', "`"if=pflash,format=raw,unit=0,readonly=on,file=$code`"")
    if (Test-Path -LiteralPath $vars) {
        $qemuArgs += @('-drive', "`"if=pflash,format=raw,unit=1,snapshot=on,file=$vars`"")
    }
}

$qemuArgs += @('-drive', "`"format=raw,file=$($image.FullName)`"")
$dataImage = Join-Path $root 'rustos-data.img'
if (Test-Path -LiteralPath $dataImage) {
    $qemuArgs += @('-drive', "`"if=ide,index=1,format=raw,file=$dataImage`"")
}
$proc = Start-Process -FilePath $qemu -ArgumentList $qemuArgs -PassThru -WindowStyle Hidden

function Connect([int]$port) {
    $client = [System.Net.Sockets.TcpClient]::new()
    $deadline = (Get-Date).AddSeconds(15)
    while ($true) {
        try { $client.Connect('127.0.0.1', $port); return $client } catch {
            if ((Get-Date) -gt $deadline) { throw "connect to $port failed" }
            Start-Sleep -Milliseconds 100
        }
    }
}

# Watch COM1 until the kernel reports its device interrupts as live; the
# desktop is created immediately afterwards. Do not send anything on the serial
# port: any byte there would hand the console to the shell.
$serial = Connect $serialPort
$stream = $serial.GetStream()
$stream.ReadTimeout = 500
$buffer = New-Object byte[] 65536
$sb = New-Object System.Text.StringBuilder
$deadline = (Get-Date).AddSeconds(25)
while ((Get-Date) -lt $deadline -and -not $sb.ToString().Contains('COM1 IRQs live')) {
    try {
        $n = $stream.Read($buffer, 0, $buffer.Length)
        if ($n -gt 0) { [void]$sb.Append([Text.Encoding]::ASCII.GetString($buffer, 0, $n)) }
    } catch [System.IO.IOException] { }
}
if (-not $sb.ToString().Contains('COM1 IRQs live')) { throw 'the kernel never reached the desktop' }

# Give the initial (debug builds are slow) render time to finish.
Start-Sleep -Seconds 4

# Move the pointer so the cursor is drawn at a known place, then let the
# re-render settle before the screendump.
$monitor = Connect $monitorPort
$mstream = $monitor.GetStream()
$mstream.ReadTimeout = 1000
function Monitor([string]$command) {
    $bytes = [Text.Encoding]::ASCII.GetBytes("$command`n")
    $mstream.Write($bytes, 0, $bytes.Length)
    $mstream.Flush()
}
Monitor 'mouse_move 120 80'
Start-Sleep -Seconds 3

if (Test-Path $ppm) { Remove-Item $ppm }
Monitor "screendump `"$($ppm -replace '\\', '/')`""
Start-Sleep -Seconds 2

Monitor 'quit'
Start-Sleep -Milliseconds 500
if (-not $proc.HasExited) { $proc.Kill() }
try { $serial.Close() } catch { }
try { $monitor.Close() } catch { }

if (-not (Test-Path $ppm)) { throw 'screendump did not produce a file' }

# Convert P6 PPM -> PNG with System.Drawing.
Add-Type -AssemblyName System.Drawing
$raw = [System.IO.File]::ReadAllBytes($ppm)
$newlines = 0
$headerEnd = 0
for ($i = 0; $i -lt 128; $i++) {
    if ($raw[$i] -eq 10) {
        $newlines++
        if ($newlines -eq 3) { $headerEnd = $i + 1; break }
    }
}
$header = [Text.Encoding]::ASCII.GetString($raw, 0, $headerEnd)
$parts = $header -split "`n" | Where-Object { $_ -and $_ -notlike '#*' }
$dims = $parts[1] -split ' '
$width = [int]$dims[0]
$height = [int]$dims[1]

$bitmap = [System.Drawing.Bitmap]::new($width, $height, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
$rect = [System.Drawing.Rectangle]::new(0, 0, $width, $height)
$data = $bitmap.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::WriteOnly, $bitmap.PixelFormat)
$rowBytes = $width * 3
$stride = $data.Stride
$row = New-Object byte[] $rowBytes
for ($y = 0; $y -lt $height; $y++) {
    [Array]::Copy($raw, $headerEnd + $y * $rowBytes, $row, 0, $rowBytes)
    # QEMU's screendump stores pixels as (B, G, R); the PNG is (R, G, B),
    # so swap the red and blue channels while copying.
    for ($x = 0; $x -lt $width; $x++) {
        $tmp = $row[$x * 3]
        $row[$x * 3] = $row[$x * 3 + 2]
        $row[$x * 3 + 2] = $tmp
    }
    [System.Runtime.InteropServices.Marshal]::Copy($row, 0, [IntPtr]($data.Scan0.ToInt64() + $y * $stride), $rowBytes)
}
$bitmap.UnlockBits($data)
$bitmap.Save($png, [System.Drawing.Imaging.ImageFormat]::Png)
$bitmap.Dispose()

"screenshot: $png ($width x $height)"
exit 0
