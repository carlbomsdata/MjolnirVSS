<#
.SYNOPSIS
    Checks a Windows that has been restored onto a blank disk and started.

.DESCRIPTION
    Runs inside the restored machine, after it has booted on its own for the
    first time. This is the phase that decides whether a restore is a restore
    rather than a copy of some bytes: the machine starts, it is the same
    installation, and the things Windows needs to keep working are still there.

    It checks, and reports every answer whether it is the expected one or not:

      * the test files, against the hashes recorded when they were made;
      * the partition table: type and unique GUIDs, offsets, sizes, attributes;
      * the filesystems and their labels;
      * the EFI system partition's contents;
      * the boot configuration;
      * the Windows Recovery Environment's registration.

    It reads. It writes nothing except its own report, and it mounts the EFI
    partition read only on a spare letter and unmounts it again.

    Copyright (C) the MjolnirVSS contributors.
    Licensed under the GNU General Public License, version 3 or later.
#>

$ErrorActionPreference = 'Continue'
$TestRoot = 'C:\MjolnirTest'
$EfiLetter = 'S'

$script:Serial = $null
$script:Problems = 0

function Open-Report {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open()
        $script:Serial = $port
    } catch { $script:Serial = $null }
}

function Report {
    param([string] $Message)
    $line = "[restored] $Message"
    Write-Host $line
    if ($script:Serial) { try { $script:Serial.WriteLine($line) } catch { } }
    try { Add-Content -LiteralPath 'C:\mjolnir-restored.log' -Value $line -Encoding UTF8 } catch { }
}

function Problem {
    param([string] $Message)
    $script:Problems++
    Report "PROBLEM: $Message"
}

Open-Report
Report "starting on $(Get-Date -Format s)"
Report "computer $env:COMPUTERNAME, Windows $([System.Environment]::OSVersion.Version)"

try {
    # ---- 1. this really is a machine that started on its own --------------
    $uptime = (Get-Date) - (Get-CimInstance Win32_OperatingSystem).LastBootUpTime
    Report ("booted {0:N0} seconds ago" -f $uptime.TotalSeconds)
    $installed = (Get-CimInstance Win32_OperatingSystem).InstallDate
    Report "Windows installed on $installed"

    # ---- 2. the files ------------------------------------------------------
    $markersPath = Join-Path $TestRoot 'markers.json'
    if (-not (Test-Path -LiteralPath $markersPath)) {
        Problem "no markers.json at ${markersPath}: the test files did not survive"
    } else {
        $markers = Get-Content $markersPath -Raw | ConvertFrom-Json
        $matched = 0; $wrong = 0; $missing = 0; $unusable = 0
        foreach ($file in $markers.files) {
            if (-not $file.sha256) { $unusable++; continue }
            if ($file.path -like '*:*') {
                # An alternate data stream, which has to be read by name.
                $parts = $file.path -split ':'
                $target = Join-Path $TestRoot $parts[0]
                try {
                    $bytes = [byte[]] (Get-Content -LiteralPath $target -Stream $parts[1] -Encoding Byte -ReadCount 0)
                    $sha = [System.Security.Cryptography.SHA256]::Create()
                    try {
                        $hash = ([System.BitConverter]::ToString($sha.ComputeHash($bytes)) -replace '-', '')
                    } finally { $sha.Dispose() }
                } catch {
                    $missing++; Report "MISSING STREAM: $($file.path)"; continue
                }
            } else {
                $target = Join-Path $TestRoot $file.path
                if (-not (Test-Path -LiteralPath $target)) {
                    $missing++; Report "MISSING: $($file.path)"; continue
                }
                $hash = (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash
            }
            if ($hash -eq $file.sha256) { $matched++ }
            else { $wrong++; Report "WRONG CONTENTS: $($file.path)" }
        }
        Report "files: $matched matched, $wrong wrong, $missing missing, $unusable with no recorded hash"
        if ($wrong -gt 0) { Problem "$wrong files came back with different contents" }
        if ($missing -gt 0) { Problem "$missing files did not come back" }
        if ($unusable -gt 0) { Problem "$unusable markers had no hash, so they proved nothing" }
        if ($matched -eq 0) { Problem 'no file was checked against a hash' }
    }

    # ---- 3. the partition table -------------------------------------------
    Report 'partition table of disk 0:'
    $disk = Get-Disk -Number 0
    Report ("  {0}, {1} bytes, {2}, disk GUID {3}" -f $disk.FriendlyName, $disk.Size, $disk.PartitionStyle, $disk.Guid)
    if ($disk.PartitionStyle -ne 'GPT') { Problem "disk 0 is $($disk.PartitionStyle), not GPT" }

    foreach ($p in (Get-Partition -DiskNumber 0 | Sort-Object PartitionNumber)) {
        Report ('  partition {0} type={1} guid={2} offset={3} size={4} attributes={5}' -f `
                $p.PartitionNumber, $p.GptType, $p.Guid, $p.Offset, $p.Size, $p.Attributes)
    }

    # ---- 4. the filesystems ------------------------------------------------
    Report 'volumes:'
    foreach ($v in (Get-Volume | Sort-Object DriveLetter)) {
        Report ('  {0} label={1} fs={2} size={3} health={4}' -f `
            ($(if ($v.DriveLetter) { "$($v.DriveLetter):" } else { '(no letter)' })), `
                $v.FileSystemLabel, $v.FileSystem, $v.Size, $v.HealthStatus)
    }

    # ---- 5. the EFI system partition ---------------------------------------
    $efi = Get-Partition -DiskNumber 0 | Where-Object { $_.GptType -eq '{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}' }
    if (-not $efi) {
        Problem 'there is no EFI system partition on the restored disk'
    } else {
        Report "EFI system partition is partition $($efi.PartitionNumber), $($efi.Size) bytes"
        $mounted = $false
        try {
            & mountvol.exe "${EfiLetter}:" /s 2>&1 | Out-Null
            $mounted = $true
            foreach ($needed in @(
                    "${EfiLetter}:\EFI\Microsoft\Boot\bootmgfw.efi",
                    "${EfiLetter}:\EFI\Microsoft\Boot\BCD",
                    "${EfiLetter}:\EFI\Boot\bootx64.efi")) {
                if (Test-Path -LiteralPath $needed) {
                    $size = (Get-Item -LiteralPath $needed -Force).Length
                    Report "  present: $needed ($size bytes)"
                } else {
                    Problem "missing from the EFI partition: $needed"
                }
            }
        } catch {
            Problem "could not read the EFI system partition: $_"
        } finally {
            if ($mounted) { & mountvol.exe "${EfiLetter}:" /d 2>&1 | Out-Null }
        }
    }

    # ---- 6. the boot configuration -----------------------------------------
    Report 'boot configuration:'
    $bcd = (& bcdedit.exe /enum '{default}' 2>&1 | Out-String)
    foreach ($line in ($bcd -split "`r?`n")) {
        if ($line.Trim()) { Report "  $($line.TrimEnd())" }
    }
    if ($bcd -notmatch 'winload') { Problem 'the default boot entry does not name a Windows loader' }

    # ---- 7. the recovery environment ---------------------------------------
    Report 'recovery environment:'
    $reagent = (& reagentc.exe /info 2>&1 | Out-String)
    foreach ($line in ($reagent -split "`r?`n")) {
        if ($line.Trim()) { Report "  $($line.Trim())" }
    }
    if ($reagent -notmatch 'Enabled') {
        Report '  note: Windows RE is not enabled on the restored machine'
    }

    # ---- 8. anything Windows itself complained about -----------------------
    $bad = Get-WinEvent -FilterHashtable @{ LogName = 'System'; Level = 1, 2; StartTime = (Get-Date).AddMinutes(-30) } -ErrorAction SilentlyContinue
    if ($bad) {
        Report "Windows logged $($bad.Count) error or critical events since starting:"
        foreach ($e in ($bad | Select-Object -First 15)) {
            Report ('  {0} {1} {2}' -f $e.TimeCreated.ToString('HH:mm:ss'), $e.ProviderName, ($e.Message -split "`r?`n")[0])
        }
    } else {
        Report 'Windows logged no errors since starting'
    }

    if ($script:Problems -gt 0) {
        Report "PHASE-RESTORED-FAILED: $($script:Problems) problems"
    } else {
        Report 'PHASE-RESTORED-COMPLETE'
    }
} catch {
    Report "PHASE-RESTORED-FAILED: $_"
} finally {
    if ($script:Serial) { try { $script:Serial.Close() } catch { } }
}
