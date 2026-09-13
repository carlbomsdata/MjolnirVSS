<#
.SYNOPSIS
    Turns BitLocker on in a test virtual machine, so a backup of an encrypted
    Windows can be tried.

.DESCRIPTION
    Runs inside MjolnirVSS-Test-Source. It allows BitLocker without a TPM,
    because a virtual machine has none, turns it on for the Windows volume with
    a password, and writes the recovery password onto the backup drive.

    This machine is disposable and holds nothing real. The password is written
    into this file on purpose: it protects nothing, it is the same on every run,
    and a test that needs somebody to remember a password is not automated. The
    recovery password goes on the backup drive rather than into the repository,
    and is destroyed with the virtual machine.

    After this, the machine needs restarting, and will ask for the password
    before Windows starts.

    Copyright (C) the MjolnirVSS contributors.
    Licensed under the GNU General Public License, version 3 or later.
#>

$ErrorActionPreference = 'Continue'
$BackupLetter = 'M'

# Test only. See the note above: this guards nothing.
$TestPassword = 'Mjolnir!Test2026'

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
    $line = "[bitlocker] $Message"
    Write-Host $line
    if ($script:Serial) { try { $script:Serial.WriteLine($line) } catch { } }
    try { Add-Content -LiteralPath 'C:\mjolnir-bitlocker.log' -Value $line -Encoding UTF8 } catch { }
}

Open-Report
Report "starting on $(Get-Date -Format s)"

try {
    # ---- allow BitLocker on a machine with no TPM -------------------------
    # A virtual machine has no TPM, so without this Windows refuses to protect
    # the operating system volume at all.
    $fve = 'HKLM:\SOFTWARE\Policies\Microsoft\FVE'
    if (-not (Test-Path $fve)) { New-Item -Path $fve -Force | Out-Null }
    Set-ItemProperty -Path $fve -Name 'EnableBDEWithNoTPM' -Value 1 -Type DWord
    Set-ItemProperty -Path $fve -Name 'UseAdvancedStartup' -Value 1 -Type DWord
    Set-ItemProperty -Path $fve -Name 'UseTPM' -Value 2 -Type DWord
    Set-ItemProperty -Path $fve -Name 'UseTPMPIN' -Value 2 -Type DWord
    Set-ItemProperty -Path $fve -Name 'UseTPMKey' -Value 2 -Type DWord
    Set-ItemProperty -Path $fve -Name 'UseTPMKeyPIN' -Value 2 -Type DWord
    Report 'BitLocker is allowed without a TPM on this machine'

    $before = Get-BitLockerVolume -MountPoint 'C:'
    Report "before: protection $($before.ProtectionStatus), status $($before.VolumeStatus)"

    if ($before.ProtectionStatus -eq 'On') {
        Report 'BitLocker is already on; nothing to do'
    } else {
        $secure = ConvertTo-SecureString $TestPassword -AsPlainText -Force
        # Used space only, because encrypting 60 GB of free space proves nothing
        # and takes an hour.
        try {
            Enable-BitLocker -MountPoint 'C:' -PasswordProtector -Password $secure `
                -UsedSpaceOnly -SkipHardwareTest -ErrorAction Stop | Out-Null
            Report 'turned BitLocker on with a password protector'
        } catch {
            Report "the password protector was refused: $_"
            Report 'trying manage-bde instead'
            $out = & manage-bde.exe -on C: -password -UsedSpaceOnly 2>&1 | Out-String
            foreach ($line in ($out -split "`r?`n")) {
                if ($line.Trim()) { Report "  $($line.TrimEnd())" }
            }
        }
    }

    # ---- a recovery password, kept off this machine -----------------------
    $volume = Get-BitLockerVolume -MountPoint 'C:'
    if (-not ($volume.KeyProtector | Where-Object { $_.KeyProtectorType -eq 'RecoveryPassword' })) {
        Add-BitLockerKeyProtector -MountPoint 'C:' -RecoveryPasswordProtector | Out-Null
        $volume = Get-BitLockerVolume -MountPoint 'C:'
    }

    $recovery = $volume.KeyProtector | Where-Object { $_.KeyProtectorType -eq 'RecoveryPassword' }
    $keyFile = "${BackupLetter}:\bitlocker-recovery.txt"
    $lines = @(
        'MjolnirVSS test machine only. This protects nothing real.',
        "Written $(Get-Date -Format s)",
        "Startup password: $TestPassword"
    )
    foreach ($r in $recovery) {
        $lines += "Recovery password ($($r.KeyProtectorId)): $($r.RecoveryPassword)"
    }
    try {
        Set-Content -LiteralPath $keyFile -Value $lines -Encoding UTF8
        Report "recovery details written to $keyFile"
    } catch {
        Report "could not write the recovery details: $_"
    }

    foreach ($r in $volume.KeyProtector) {
        Report "protector: $($r.KeyProtectorType)"
    }
    Report "after: protection $($volume.ProtectionStatus), status $($volume.VolumeStatus), $($volume.EncryptionPercentage)% encrypted"

    if ($volume.VolumeStatus -eq 'FullyDecrypted') {
        Report 'PHASE-BITLOCKER-FAILED: the volume is still not being encrypted'
    } else {
        Report 'PHASE-BITLOCKER-COMPLETE: restart the machine and give it the password'
    }
} catch {
    Report "PHASE-BITLOCKER-FAILED: $_"
} finally {
    if ($script:Serial) { try { $script:Serial.Close() } catch { } }
}
