<#
.SYNOPSIS
    Boots RustOS headlessly in QEMU, drives the shell over COM1 and asserts on
    the transcript. This is the project's integration test.

.DESCRIPTION
    COM1 is exposed as a TCP socket, so the test can wait for the kernel to come
    up before typing instead of racing the boot sequence the way piped stdin
    does. Because the desktop environment owns the screen when a framebuffer is
    present, the test sends a bare Enter on COM1 first: any serial byte hands the
    console to the shell (see kernel\src\wm.rs).

    The full serial transcript is written to
    target\smoke-test-<firmware>[-iso]-<profile>.log.

.PARAMETER Firmware
    Which image to boot: `bios` (default) or `uefi` (through OVMF).

.PARAMETER TimeoutSeconds
    Per-step timeout while waiting for the guest to print something.

.PARAMETER Release
    Build and test the optimised (`--release`) image instead of the debug one.

.PARAMETER Iso
    Boot the ISO9660/El Torito image from a virtual CD-ROM instead of the raw
    disk image. Only UEFI boots the ISO in QEMU: the BIOS bootloader needs
    extended INT 13h, which SeaBIOS does not provide for emulated CD drives.

.PARAMETER Network
    Also resolve a host name and fetch a page over HTTP. These checks reach the
    real internet through QEMU's user-mode network, so they are opt-in.

.PARAMETER Audio
    Also verify that the AC'97 driver really produced samples: the guest's
    playback is recorded with QEMU's `wav` audio backend and the captured file
    is checked for a non-silent payload.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\smoke-test.ps1
    powershell -ExecutionPolicy Bypass -File scripts\smoke-test.ps1 -Firmware uefi
    powershell -ExecutionPolicy Bypass -File scripts\smoke-test.ps1 -Release
    powershell -ExecutionPolicy Bypass -File scripts\smoke-test.ps1 -Firmware uefi -Iso
    powershell -ExecutionPolicy Bypass -File scripts\smoke-test.ps1 -Network
    powershell -ExecutionPolicy Bypass -File scripts\smoke-test.ps1 -Network -Audio
#>
[CmdletBinding()]
param(
    [ValidateSet('bios', 'uefi')]
    [string]$Firmware = 'bios',
    [int]$TimeoutSeconds = 60,
    [switch]$Release,
    [switch]$Iso,
    [switch]$Network,
    [switch]$Audio
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'env.ps1')

$projectRoot = Split-Path -Parent $PSScriptRoot
Push-Location $projectRoot

$qemu = $null
$process = $null
$client = $null
$transcript = New-Object System.Text.StringBuilder

function Write-Transcript([string]$text) {
    [void]$transcript.Append($text)
}

function Read-Stream([int]$milliseconds) {
    $buffer = New-Object byte[] 8192
    $read = 0
    try {
        $read = $stream.Read($buffer, 0, $buffer.Length)
        if ($read -le 0) { return $false }
        $text = [System.Text.Encoding]::ASCII.GetString($buffer, 0, $read)
        Write-Transcript $text
        Write-Host -NoNewline $text
        return $true
    } catch [System.IO.IOException] {
        if ($_.Exception.InnerException -is [System.Net.Sockets.SocketException]) {
            return $true # read timeout, keep polling
        }
        throw
    }
}

function Wait-For([string]$pattern, [int]$timeoutSeconds = $TimeoutSeconds) {
    $deadline = (Get-Date).AddSeconds($timeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        $text = $script:transcript.ToString()
        # Fail fast instead of waiting out the whole timeout on a dead kernel.
        if ($text.Contains('*** KERNEL PANIC ***')) { return $false }
        if ($script:searchStart -lt $text.Length -and
            $text.Substring($script:searchStart).Contains($pattern)) { return $true }
        [void](Read-Stream 500)
    }
    $text = $script:transcript.ToString()
    return ($script:searchStart -lt $text.Length -and
        $text.Substring($script:searchStart).Contains($pattern))
}

# Only output produced after this offset counts for the next Wait-For, so a
# prompt from an earlier command can never satisfy a later wait.
function Set-Mark { $script:searchStart = $script:transcript.Length }

function Send-Line([string]$line) {
    $bytes = [System.Text.Encoding]::ASCII.GetBytes("$line`r")
    $stream.Write($bytes, 0, $bytes.Length)
    $stream.Flush()
}

$failures = New-Object System.Collections.Generic.List[string]
$script:searchStart = 0

try {
    if (-not (Test-Toolchain)) { throw 'toolchain incomplete' }

    if ($Iso -and $Firmware -ne 'uefi') {
        throw 'the BIOS ISO does not boot in QEMU (SeaBIOS has no INT 13h extensions); use -Firmware uefi'
    }

    $qemu = (Get-Command qemu-system-x86_64 -ErrorAction SilentlyContinue).Source
    if (-not $qemu) { throw 'qemu-system-x86_64 not found on PATH' }

    $buildProfile = if ($Release) { 'release' } else { 'debug' }

    Write-Host "==> Building ($buildProfile)" -ForegroundColor Cyan
    $cargoArgs = @()
    if ($Release) { $cargoArgs += '--release' }
    Invoke-Native cargo (@('build') + $cargoArgs)
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

    $image = if ($Iso) {
        Get-ChildItem -Path (Join-Path $projectRoot "target\$buildProfile") -Recurse -Filter 'rustos.iso' -ErrorAction Stop |
            Sort-Object LastWriteTime |
            Select-Object -Last 1
    } else {
        Get-ChildItem -Path (Join-Path $projectRoot "target\$buildProfile") -Recurse -Filter "rustos-$Firmware.img" -ErrorAction Stop |
            Sort-Object LastWriteTime |
            Select-Object -Last 1
    }
    if (-not $image) { throw "the $Firmware image was not found under target\$buildProfile\" }

    $medium = if ($Iso) { 'iso' } else { 'disk' }

    # Guest audio is captured here so the AC'97 driver can be verified. The
    # path must not contain spaces: Start-Process joins the argument array with
    # spaces and does not quote.
    $audioPath = Join-Path $env:TEMP 'rustos-smoke-audio.wav'
    Remove-Item -LiteralPath $audioPath -ErrorAction SilentlyContinue

    # The RustOS data disk rides on the primary IDE slave, exactly as the
    # launcher attaches it. The test works on a throwaway copy so the project's
    # own rustos-data.img is never modified.
    $dataImage = Join-Path $projectRoot 'rustos-data.img'
    if (-not (Test-Path -LiteralPath $dataImage)) {
        throw "the data disk was not created at $dataImage (run 'cargo build' first)"
    }
    $smokeData = Join-Path $projectRoot 'target\smoke-data.img'
    New-Item -ItemType Directory -Force (Split-Path $smokeData) | Out-Null
    Copy-Item -LiteralPath $dataImage -Destination $smokeData -Force

    # Ask the OS for a free port.
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()

    Write-Host "==> Booting $Firmware $medium image $($image.FullName) (COM1 on tcp:$port)" -ForegroundColor Cyan
    # A framebuffer makes the kernel boot into the Windows XP-style desktop,
    # which owns the screen. Any byte on COM1 hands the console to the shell
    # (see kernel\src\wm.rs), so the test sends one below before waiting for the
    # prompt. Keep the default VGA adapter: the bootloader needs VBE, so
    # `-vga none` produces no output at all.
    $qemuArgs = @(
        '-name', "RustOS-smoke-test-$Firmware",
        '-m', '256M',
        '-smp', '1',
        '-no-reboot',
        '-device', 'isa-debug-exit,iobase=0xf4,iosize=0x04',
        '-nic', 'user,model=rtl8139',
        '-audiodev', "wav,id=snd0,path=$audioPath",
        '-device', 'AC97,audiodev=snd0',
        '-display', 'none',
        '-monitor', 'none',
        '-serial', "tcp:127.0.0.1:$port,server=on,wait=off"
    )

    if ($Firmware -eq 'uefi') {
        # QEMU ships OVMF's edk2 images next to the binaries, in share\.
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

    # Start-Process joins the array with spaces without quoting, so the disk
    # path must carry its own quotes to survive paths with spaces.
    if ($Iso) {
        $qemuArgs += @('-drive', "`"format=raw,media=cdrom,file=$($image.FullName)`"")
    } else {
        $qemuArgs += @('-drive', "`"format=raw,file=$($image.FullName)`"")
    }
    $qemuArgs += @('-drive', "`"if=ide,index=1,format=raw,file=$smokeData`"")
    $process = Start-Process -FilePath $qemu -ArgumentList $qemuArgs -PassThru -WindowStyle Hidden

    # Connect as soon as the socket is up so the boot log is captured.
    $client = New-Object System.Net.Sockets.TcpClient
    $deadline = (Get-Date).AddSeconds(15)
    while ($true) {
        try {
            $client.Connect('127.0.0.1', $port)
            break
        } catch {
            if ((Get-Date) -gt $deadline) { throw 'could not connect to the QEMU serial socket' }
            if ($process.HasExited) { throw "QEMU exited early with code $($process.ExitCode)" }
            Start-Sleep -Milliseconds 100
        }
    }
    $stream = $client.GetStream()
    $stream.ReadTimeout = 500

    Write-Host ''
    # The desktop owns the screen once the kernel is up, so wait for the device
    # interrupts to go live and then nudge it into the shell with a bare Enter.
    # Resend in case an early byte was dropped while COM1 was being configured.
    [void](Wait-For 'COM1 IRQs live')
    $prompted = $false
    for ($attempt = 0; $attempt -lt 10 -and -not $prompted; $attempt++) {
        Set-Mark
        Send-Line ''
        $prompted = Wait-For 'rustos> ' 3
    }
    if (-not $prompted) { $failures.Add('shell prompt never appeared') }

    $checks = @(
        @{ Command = 'help'; Expect = 'shutdown' },
        @{ Command = 'echo hello-from-rustos'; Expect = 'hello-from-rustos' },
        @{ Command = 'cpu'; Expect = 'vendor:' },
        @{ Command = 'mem'; Expect = 'usable' },
        @{ Command = 'heap'; Expect = 'heap: 0x' },
        @{ Command = 'alloc'; Expect = 'heap allocation and free succeeded' },
        @{ Command = 'uptime'; Expect = 'uptime:' },
        @{ Command = 'net'; Expect = 'gateway:' },
        @{ Command = 'dhcp'; Expect = 'lease acquired' },
        @{ Command = 'beep'; Expect = 'played 880 Hz' },
        @{ Command = 'tone 440 200'; Expect = 'played' },
        @{ Command = 'disks'; Expect = 'drive 1' },
        @{ Command = 'ls'; Expect = 'README.TXT' },
        @{ Command = 'cat README.TXT'; Expect = 'FAT16' },
        @{ Command = 'write SMOKE.TXT hello from the smoke test'; Expect = 'to SMOKE.TXT' },
        @{ Command = 'cat SMOKE.TXT'; Expect = 'hello from the smoke test' },
        @{ Command = 'rm SMOKE.TXT'; Expect = 'deleted SMOKE.TXT' },
        @{ Command = 'play BEEP.WAV'; Expect = 'playing BEEP.WAV' },
        @{ Command = 'run'; Expect = 'Hello from ring 3!' },
        @{ Command = 'run HELLO.BIN'; Expect = 'Hello from HELLO.BIN' },
        @{ Command = 'ping 10.0.2.2'; Expect = 'reply in' }
    )

    if ($Network) {
        $checks += @(
            @{ Command = 'dns example.com'; Expect = 'example.com ->' },
            @{ Command = 'http http://example.com/'; Expect = 'HTTP/1.' }
        )
    }

    foreach ($check in $checks) {
        Set-Mark
        Send-Line $check.Command
        if (-not (Wait-For $check.Expect)) {
            $failures.Add("command '$($check.Command)' did not produce '$($check.Expect)'")
        }
        [void](Wait-For 'rustos> ')
    }

    Set-Mark
    Send-Line 'shutdown'
    [void](Wait-For 'Powering off')

    if (-not $process.WaitForExit(10000)) {
        $failures.Add('QEMU did not exit after the shutdown command')
    }

    if ($Audio) {
        # The AC'97 driver must have produced real samples. A WAV written by
        # QEMU has a 44-byte header; anything non-zero after it is audio.
        if (-not (Test-Path -LiteralPath $audioPath)) {
            $failures.Add("audio: QEMU wrote no capture file at $audioPath")
        } else {
            $bytes = [System.IO.File]::ReadAllBytes($audioPath)
            if ($bytes.Length -le 44) {
                $failures.Add("audio: capture file is only $($bytes.Length) bytes (no samples)")
            } elseif (-not ($bytes[44..($bytes.Length - 1)] | Where-Object { $_ -ne 0 })) {
                $failures.Add('audio: captured samples are all zero (silence)')
            } else {
                Write-Host "audio: captured $($bytes.Length) bytes of non-silent PCM" -ForegroundColor Green
            }
        }
    }
}
catch {
    $failures.Add($_.Exception.Message)
}
finally {
    if ($client) { $client.Close() }
    if ($process -and -not $process.HasExited) {
        $process.Kill()
    }
    if ($buildProfile) {
        $logName = if ($Iso) {
            "smoke-test-$Firmware-iso-$buildProfile.log"
        } else {
            "smoke-test-$Firmware-$buildProfile.log"
        }
        $logPath = Join-Path $projectRoot "target\$logName"
        New-Item -ItemType Directory -Force (Split-Path $logPath) | Out-Null
        [System.IO.File]::WriteAllText($logPath, $transcript.ToString(), (New-Object System.Text.UTF8Encoding($false)))
        Write-Host ''
        Write-Host "transcript: $logPath"
    }
    Pop-Location
}

$transcriptText = $transcript.ToString()
if ($transcriptText.Contains('KERNEL PANIC') -or $transcriptText.Contains('CPU EXCEPTION')) {
    $failures.Add('the kernel reported a panic or exception')
}

if ($failures.Count -gt 0) {
    Write-Host ''
    Write-Host 'SMOKE TEST FAILED' -ForegroundColor Red
    foreach ($failure in $failures) { Write-Host "  - $failure" -ForegroundColor Red }
    exit 1
}

Write-Host ''
Write-Host 'SMOKE TEST PASSED' -ForegroundColor Green
exit 0
