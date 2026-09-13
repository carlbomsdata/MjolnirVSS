<#
.SYNOPSIS
    Builds the disposable machine a backup is restored onto, and boots it from
    MjolnirVSS recovery media.

.DESCRIPTION
    Creates MjolnirVSS-Test-Restore: a UEFI virtual machine with a blank target
    disk, the source machine's backup disk attached, and the MjolnirVSS recovery
    ISO in its drive. This is the machine the gate is decided on: whether a
    restored Windows starts.

    The backup disk is the same file the source machine writes to. Both machines
    are never run at once, and this script refuses to start if the source is
    running.

.PARAMETER TargetDiskGB
    Size of the blank disk to restore onto. Defaults to the same size as the
    source machine's system disk; pass a larger number for the larger target
    test.

.PARAMETER ShareBackupDisk
    Attach the source machine's own backup disk instead of a copy. Faster, and
    what you want when the source has no snapshots. By default a consolidated
    copy is made, so the source machine keeps its snapshots and a restore test
    cannot damage the backup it is restoring from.

.EXAMPLE
    pwsh -File tests\vm\new-restore-vm.ps1
    pwsh -File tests\vm\new-restore-vm.ps1 -TargetDiskGB 96 -Name MjolnirVSS-Test-Restore-Large
#>

[CmdletBinding()]
param(
    [string] $Name = 'MjolnirVSS-Test-Restore',
    [int] $TargetDiskGB = 64,
    [int] $MemoryMB = 4096,
    [int] $Cpus = 2,
    [int] $VncPort = 5991,
    [string] $RecoveryIso,
    [switch] $ShareBackupDisk,
    [switch] $Force,
    [switch] $NoStart
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

Import-Module (Join-Path $PSScriptRoot 'MjolnirLab.psm1') -Force

$lab = Get-LabRoot
$vmDir = Join-Path (Join-Path $lab 'vms') $Name
$vmxPath = Join-Path $vmDir "$Name.vmx"
$serialLog = Join-Path (Join-Path $lab 'evidence') "$Name-console.log"

$sourceVmx = Join-Path (Join-Path (Join-Path $lab 'vms') 'MjolnirVSS-Test-Source') 'MjolnirVSS-Test-Source.vmx'
$backupDisk = Join-Path (Split-Path -Parent $sourceVmx) 'backup.vmdk'

Write-LabStep "lab root:  $lab"
Write-LabStep "vm folder: $vmDir"

# ---- the source has to be off, and its backup disk consolidated ------------

if ((Test-Path -LiteralPath $sourceVmx) -and (Test-LabVmRunning -VmxPath $sourceVmx)) {
    throw 'MjolnirVSS-Test-Source is running. Shut it down: both machines share the backup disk.'
}
if (-not (Test-Path -LiteralPath $backupDisk)) {
    throw "no backup disk at $backupDisk; run the backup phase first"
}
$deltas = @(Get-ChildItem -LiteralPath (Split-Path -Parent $backupDisk) -Filter 'backup-0*.vmdk' -ErrorAction SilentlyContinue)
if ($deltas -and $ShareBackupDisk) {
    throw ("the backup disk is part of a snapshot chain ({0}), so it cannot be shared. " -f ($deltas.Name -join ', ')) +
          'Delete the source machine snapshots, or drop -ShareBackupDisk to use a copy.'
}

# ---- the recovery media ----------------------------------------------------

if (-not $RecoveryIso) {
    $RecoveryIso = Join-Path (Join-Path $lab 'media') 'MjolnirVSS-Recovery.iso'
}
if (-not (Test-Path -LiteralPath $RecoveryIso)) {
    throw "no recovery media at $RecoveryIso; build it with MjolnirVSS.exe recovery-media --iso $RecoveryIso"
}
Write-LabStep "recovery media: $RecoveryIso"

# ---- an existing machine ---------------------------------------------------

if (Test-Path -LiteralPath $vmxPath) {
    if (-not $Force) {
        throw "$Name already exists at $vmxPath. Pass -Force to rebuild it, which deletes it."
    }
    Write-LabStep 'removing the previous restore machine'
    if (Test-LabVmRunning -VmxPath $vmxPath) { Stop-LabVm -VmxPath $vmxPath -Hard }
    Start-Sleep -Seconds 3
    Remove-Item -LiteralPath (Assert-InsideLab -Path $vmDir) -Recurse -Force
}

New-Item -ItemType Directory -Path $vmDir -Force | Out-Null

# ---- the backup disk, as a copy unless sharing was asked for ---------------

if (-not $ShareBackupDisk) {
    # A snapshot chain is read through its newest link, which references its
    # parent. Cloning that produces one file holding what the source machine
    # actually wrote, and leaves the source machine's snapshots alone.
    $newest = if ($deltas) {
        ($deltas | Sort-Object Name | Select-Object -Last 1).FullName
    } else {
        $backupDisk
    }
    $copy = Join-Path $vmDir 'backup.vmdk'
    Write-LabStep "copying the backup disk from $(Split-Path -Leaf $newest); this takes a few minutes"
    $copy = Assert-InsideLab -Path $copy
    $vdm = (Get-VMwarePaths).VDiskManager
    $result = Invoke-Native -Executable $vdm -Arguments @('-r', $newest, '-t', '0', $copy)
    if (-not (Test-Path -LiteralPath $copy)) {
        throw "copying the backup disk to $copy failed:`n$($result.Output)"
    }
    $backupDisk = $copy
}

# ---- the blank target ------------------------------------------------------

Write-LabStep "creating a blank ${TargetDiskGB} GB target disk"
$target = New-LabDisk -Path (Join-Path $vmDir 'target.vmdk') -SizeGB $TargetDiskGB -Force

# ---- the machine -----------------------------------------------------------

if (Test-Path -LiteralPath $serialLog) { Remove-Item -LiteralPath $serialLog -Force }

$vmxPath = New-LabVmx `
    -Name $Name `
    -Directory $vmDir `
    -MemoryMB $MemoryMB `
    -Cpus $Cpus `
    -Disks @($target, $backupDisk) `
    -IsoPaths @($RecoveryIso) `
    -SecureBoot `
    -SerialLog $serialLog `
    -VncPort $VncPort

# The firmware must try the disc before the blank disk, or it finds nothing.
Set-VmxSetting -VmxPath $vmxPath -Key 'bios.bootOrder' -Value 'cdrom,hdd'
Set-VmxSetting -VmxPath $vmxPath -Key 'bios.bootDelay' -Value '3000'

Write-LabStep "wrote $vmxPath"
Write-LabStep "target disk: $target"
Write-LabStep "backup disk: $backupDisk"

if (-not $NoStart) {
    Write-LabStep 'starting the machine; it boots the recovery media'
    Start-LabVm -VmxPath $vmxPath
    Write-LabStep "watch it with: python tests\vm\tools\vnc_screenshot.py --port $VncPort --out screen.png"
}

[pscustomobject]@{
    Vmx        = $vmxPath
    TargetDisk = $target
    BackupDisk = $backupDisk
    SerialLog  = $serialLog
    VncPort    = $VncPort
}
