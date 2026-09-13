<#
.SYNOPSIS
    Browses a MjolnirVSS backup and copies files out of it, then checks them.

.DESCRIPTION
    Runs inside a test virtual machine after backup-phase.ps1 has produced a
    backup. It lists the partitions in the backup, browses the Windows volume,
    copies the folder of test files out, and compares every extracted file
    against the hash recorded when the file was made.

    This is what decides whether file recovery works: not that it produced
    files, but that it produced the right bytes.

    It writes only into a folder on the backup drive. The backup itself is
    opened read only and is never modified.

    Copyright (C) the MjolnirVSS contributors.
    Licensed under the GNU General Public License, version 3 or later.
#>

$ErrorActionPreference = 'Continue'
$BackupLetter = 'M'

$script:Serial = $null

function Open-Report {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open()
        $script:Serial = $port
    } catch { $script:Serial = $null }
}

function Report {
    param([string] $Message)
    $line = "[files] $Message"
    Write-Host $line
    if ($script:Serial) { try { $script:Serial.WriteLine($line) } catch { } }
    try { Add-Content -LiteralPath 'C:\mjolnir-phase.log' -Value $line -Encoding UTF8 } catch { }
}

function Report-Many {
    param([string] $Text, [int] $Limit = 120)
    foreach ($line in ($Text -split "`r?`n" | Select-Object -First $Limit)) {
        if ($line.Trim()) { Report "  $($line.TrimEnd())" }
    }
}

Open-Report
Report "starting on $(Get-Date -Format s)"

try {
    $exe = $null
    foreach ($drive in [char[]](68..90)) {
        $candidate = "${drive}:\MjolnirVSS\MjolnirVSS.exe"
        if (Test-Path -LiteralPath $candidate) { $exe = $candidate; break }
    }
    if (-not $exe) { throw 'MjolnirVSS.exe was not found on any drive' }

    $root = "${BackupLetter}:\"
    $folder = Get-ChildItem (Join-Path $root 'Backups') -Directory |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $folder) { throw 'no backup was found to browse' }
    Report "browsing $($folder.FullName)"

    # ---- which partitions can be browsed --------------------------------
    $volumesText = & $exe volumes $folder.FullName 2>&1 | Out-String
    Report-Many $volumesText
    if ($LASTEXITCODE -ne 0) { throw "the volumes command failed with $LASTEXITCODE" }

    $volumesJson = & $exe --json volumes $folder.FullName 2>&1 | Out-String
    $volumes = ($volumesJson | ConvertFrom-Json).volumes
    $windows = $volumes | Where-Object { $_.readable -and $_.drive_letter -eq 'C' } | Select-Object -First 1
    if (-not $windows) {
        $windows = $volumes | Where-Object { $_.readable } |
            Sort-Object -Property size_bytes -Descending | Select-Object -First 1
    }
    if (-not $windows) { throw 'no partition in the backup can be browsed' }
    Report "using volume $($windows.stream_id) ($($windows.role))"

    # ---- browse ----------------------------------------------------------
    Report 'the root of the volume:'
    $rootList = & $exe browse $folder.FullName --volume $windows.stream_id --folder '\' 2>&1 | Out-String
    Report-Many $rootList 30
    if ($LASTEXITCODE -ne 0) { throw "browsing the root failed with $LASTEXITCODE" }

    Report 'the test folder:'
    $testList = & $exe browse $folder.FullName --volume $windows.stream_id --folder '\MjolnirTest' 2>&1 | Out-String
    Report-Many $testList 40
    if ($LASTEXITCODE -ne 0) { throw "browsing the test folder failed with $LASTEXITCODE" }

    # ---- extract ---------------------------------------------------------
    $into = Join-Path $root 'Extracted'
    if (Test-Path $into) { Remove-Item $into -Recurse -Force }
    New-Item -ItemType Directory -Path $into -Force | Out-Null

    Report "copying \MjolnirTest into $into"
    $extractText = & $exe extract $folder.FullName --volume $windows.stream_id --item '\MjolnirTest' --into $into 2>&1 | Out-String
    $extractCode = $LASTEXITCODE
    Report-Many $extractText 80
    Report "extract exit code $extractCode"

    # ---- check every file against the hash taken when it was made --------
    $markersPath = Join-Path $root 'markers.json'
    if (-not (Test-Path $markersPath)) { throw "no markers.json on $root" }
    $markers = Get-Content $markersPath -Raw | ConvertFrom-Json

    $matched = 0
    $missing = 0
    $wrong = 0
    $skippedOnPurpose = 0
    $unusable = 0

    foreach ($file in $markers.files) {
        # A marker with no hash in it cannot decide anything, and comparing
        # against one would pass whatever the extraction produced. That is a
        # broken test, not a passing one.
        if (-not $file.sha256) {
            $unusable++
            Report "NO EXPECTED HASH RECORDED: $($file.path)"
            continue
        }
        $relative = $file.path
        # The stream was written beside the file rather than as a stream.
        if ($relative -like '*:*') {
            $parts = $relative -split ':'
            $relative = "$($parts[0]).stream-$($parts[1])"
        }
        $extracted = Join-Path (Join-Path $into 'MjolnirTest') $relative
        if (-not (Test-Path -LiteralPath $extracted)) {
            # A compressed file is skipped on purpose and reported as such.
            if ($extractText -match [regex]::Escape($file.path)) {
                $skippedOnPurpose++
                Report "skipped on purpose: $($file.path)"
            } else {
                $missing++
                Report "MISSING: $($file.path)"
            }
            continue
        }
        $hash = (Get-FileHash -LiteralPath $extracted -Algorithm SHA256).Hash
        if ($hash -eq $file.sha256) {
            $matched++
        } else {
            $wrong++
            Report "WRONG CONTENTS: $($file.path) expected $($file.sha256) got $hash"
        }
    }

    Report "hashes matched: $matched, wrong: $wrong, missing: $missing, skipped on purpose: $skippedOnPurpose, no expected hash: $unusable"
    if ($wrong -gt 0) { throw "$wrong extracted files did not match their hashes" }
    if ($missing -gt 0) { throw "$missing files were neither extracted nor reported as skipped" }
    if ($unusable -gt 0) { throw "$unusable markers had no hash recorded, so they proved nothing" }
    if ($matched -eq 0) { throw 'nothing was extracted and checked' }

    Report 'PHASE-FILES-COMPLETE'
} catch {
    Report "PHASE-FILES-FAILED: $_"
} finally {
    if ($script:Serial) { try { $script:Serial.Close() } catch { } }
}
