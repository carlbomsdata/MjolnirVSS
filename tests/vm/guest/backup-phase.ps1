<#
.SYNOPSIS
    Takes a real MjolnirVSS backup of this virtual machine, and reports.

.DESCRIPTION
    Runs inside MjolnirVSS-Test-Source, from an elevated prompt. It prepares the
    second virtual disk as a backup destination, takes a live backup through the
    same engine the window uses, verifies it, damages a copy to prove the
    verifier catches it, and reports everything over the serial port so the
    harness on the host can read it without VMware Tools.

    This is the test that decides whether the backup half of MjolnirVSS works on
    a real Windows installation rather than on a synthetic disk.

    It writes only to the second disk. The Windows disk is read, never written.

    Copyright (C) the MjolnirVSS contributors.
    Licensed under the GNU General Public License, version 3 or later.
#>

$ErrorActionPreference = 'Continue'

# The backup volume's letter. Deliberately not one of the first free ones: the
# payload disc takes those, and formatting over it would remove the program
# this script is running from.
$BackupLetter = 'M'

# --------------------------------------------------------------- reporting ---

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
    $line = "[backup] $Message"
    Write-Host $line
    if ($script:Serial) { try { $script:Serial.WriteLine($line) } catch { } }
    try { Add-Content -LiteralPath 'C:\mjolnir-phase.log' -Value $line -Encoding UTF8 } catch { }
}

function Report-Many {
    param([string] $Text, [int] $Limit = 200)
    foreach ($line in ($Text -split "`r?`n" | Select-Object -First $Limit)) {
        if ($line.Trim()) { Report "  $($line.TrimEnd())" }
    }
}

Open-Report
Report "starting on $(Get-Date -Format s)"

try {
    # ---- find the program on the payload disc ---------------------------
    $exe = $null
    foreach ($drive in [char[]](68..90)) {
        $candidate = "${drive}:\MjolnirVSS\MjolnirVSS.exe"
        if (Test-Path -LiteralPath $candidate) { $exe = $candidate; break }
    }
    if (-not $exe) { throw 'MjolnirVSS.exe was not found on any drive' }
    Report "using $exe"

    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'this script has to run from an elevated prompt'
    }

    # ---- prepare the destination disk -----------------------------------
    # Only a disk with no partition table is touched, and only one. A disk
    # that already has something on it is left alone.
    $root = "${BackupLetter}:\"
    if (-not (Test-Path $root)) {
        $target = Get-Disk | Where-Object { $_.PartitionStyle -eq 'RAW' } | Select-Object -First 1
        if (-not $target) { throw 'no blank disk was found to use as a backup destination' }
        Report "preparing disk $($target.Number) ($([math]::Round($target.Size/1GB)) GB) as $root"
        $target | Initialize-Disk -PartitionStyle GPT -PassThru |
            New-Partition -UseMaximumSize -DriveLetter $BackupLetter |
            Format-Volume -FileSystem NTFS -NewFileSystemLabel 'BACKUP' -Confirm:$false |
            Out-Null
    } else {
        Report "the backup destination $root is already prepared"
    }
    if (-not (Test-Path $root)) { throw "the backup destination $root is not there" }
    $destination = Join-Path $root 'Backups'
    New-Item -ItemType Directory -Path $destination -Force | Out-Null

    # Every earlier run's backup is removed, finished or not.
    #
    # Removing only the unfinished ones was not enough: five completed backups
    # accumulated over a day of milestone runs and filled the 64 GB destination,
    # and the next run spent five minutes copying before Windows reported the
    # drive full. The phase takes its own backup and needs nothing from a
    # previous one, so the destination starts empty every time.
    #
    # Only folders directly inside $destination are touched, and only ones whose
    # name is a MjolnirVSS backup name. Nothing else on the drive is looked at.
    foreach ($old in Get-ChildItem $destination -Directory -ErrorAction SilentlyContinue) {
        if ($old.Name -match '^[A-Za-z0-9-]+_\d{4}-\d{2}-\d{2}_\d{4}$') {
            Report "removing a backup from an earlier run: $($old.Name)"
            Remove-Item $old.FullName -Recurse -Force
        }
    }
    # The damaged copy this phase makes is removed on success, but not when the
    # phase throws first, and it is as large as the backup itself.
    $leftover = Join-Path $root 'Backups-damaged'
    if (Test-Path $leftover) {
        Report 'removing the damaged copy left by an earlier run'
        Remove-Item $leftover -Recurse -Force
    }

    $free = (Get-Volume -DriveLetter $BackupLetter).SizeRemaining
    Report ("destination has {0:N1} GB free" -f ($free / 1GB))

    # ---- what would be captured -----------------------------------------
    Report 'inspect:'
    Report-Many (& $exe inspect 2>&1 | Out-String)
    Report "inspect exit code $LASTEXITCODE"

    # ---- the backup ------------------------------------------------------
    Report 'taking a live backup, which takes a while'
    $started = Get-Date
    $output = & $exe backup --destination $destination 2>&1 | Out-String
    $code = $LASTEXITCODE
    $elapsed = (Get-Date) - $started
    Report-Many $output
    Report ("backup exit code {0} after {1:N0} seconds" -f $code, $elapsed.TotalSeconds)
    if ($code -ne 0) { throw "the backup failed with exit code $code" }

    $folder = Get-ChildItem $destination -Directory | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $folder) { throw 'the backup produced no folder' }
    Report "backup folder: $($folder.FullName)"

    $onDisk = (Get-ChildItem $folder.FullName -Recurse -File | Measure-Object -Property Length -Sum).Sum
    Report ("backup occupies {0:N2} GB on the destination" -f ($onDisk / 1GB))

    # ---- what the manifest says -----------------------------------------
    $manifest = Get-Content (Join-Path $folder.FullName 'manifest.json') -Raw | ConvertFrom-Json
    foreach ($stream in $manifest.streams) {
        $captured = ($stream.segments | Measure-Object -Property length -Sum).Sum
        $line = "stream $($stream.id): capture=$($stream.capture) length=$($stream.length) captured=$captured"
        if ($stream.PSObject.Properties.Name -contains 'used_blocks' -and $stream.used_blocks) {
            $u = $stream.used_blocks
            $line += " clusters=$($u.clusters_allocated)/$($u.clusters_total) cluster=$($u.cluster_size) extents=$($u.extent_count) tail=$($u.undescribed_tail_bytes)"
        }
        if ($stream.PSObject.Properties.Name -contains 'fallback_reason' -and $stream.fallback_reason) {
            $line += " FELL BACK: $($stream.fallback_reason)"
        }
        Report $line
    }

    # ---- verify it -------------------------------------------------------
    Report 'verifying the backup'
    $verify = & $exe verify $folder.FullName 2>&1 | Out-String
    $verifyCode = $LASTEXITCODE
    Report-Many $verify 40
    Report "verify exit code $verifyCode"
    if ($verifyCode -ne 0) { throw "verification failed with exit code $verifyCode" }

    # ---- an incomplete copy has to be refused ----------------------------
    # A chunk is removed rather than rewritten. It is the same failure a drive
    # with a bad sector produces, and a script that rewrites the bytes of
    # somebody's files looks like something no script should look like.
    Report 'making a copy with a chunk missing and checking it is refused'
    $damaged = Join-Path $root 'Backups-damaged'
    if (Test-Path $damaged) { Remove-Item $damaged -Recurse -Force }
    Copy-Item $folder.FullName $damaged -Recurse

    $chunk = Get-ChildItem (Join-Path $damaged 'chunks') -Recurse -File | Select-Object -First 1
    if ($chunk) {
        Remove-Item -LiteralPath $chunk.FullName -Force
        Report "removed $($chunk.Name) from the copy"

        $damagedOut = & $exe verify $damaged 2>&1 | Out-String
        $damagedCode = $LASTEXITCODE
        Report "damaged backup verify exit code $damagedCode"
        if ($damagedCode -eq 0) {
            throw 'a damaged backup passed verification, which it must never do'
        }
        Report-Many $damagedOut 15
        Remove-Item $damaged -Recurse -Force
    }

    Copy-Item 'C:\MjolnirTest\markers.json' (Join-Path $root 'markers.json') -Force
    Report 'copied the marker hashes to the backup drive'

    Report 'PHASE-BACKUP-COMPLETE'
} catch {
    Report "PHASE-BACKUP-FAILED: $_"
} finally {
    if ($script:Serial) { try { $script:Serial.Close() } catch { } }
}
