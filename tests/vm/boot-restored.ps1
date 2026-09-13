<#
.SYNOPSIS
    Points the restore machine at the disk it just restored, and starts it.

.DESCRIPTION
    After a restore, the machine is still set to boot the recovery media, which
    is how it got there. This takes the recovery disc out, puts the boot order
    back to the disk, and starts the machine. What happens next is the answer to
    the only question that matters: does a restored Windows start on its own.

    The payload disc stays in, so the machine that comes up can run
    guest\restored-phase.ps1 without being opened again.

    It refuses to touch anything that is not a MjolnirVSS test machine inside
    the lab, and it never writes to a disk.

.EXAMPLE
    pwsh -File tests\vm\boot-restored.ps1
    pwsh -File tests\vm\boot-restored.ps1 -Name MjolnirVSS-Test-Restore-Large
#>

[CmdletBinding()]
param(
    [string] $Name = 'MjolnirVSS-Test-Restore',
    [switch] $KeepRecoveryDisc,
    [switch] $NoStart
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

Import-Module (Join-Path $PSScriptRoot 'MjolnirLab.psm1') -Force

$lab = Get-LabRoot
$vmxPath = Join-Path (Join-Path (Join-Path $lab 'vms') $Name) "$Name.vmx"
$vmxPath = Assert-LabVm -VmxPath $vmxPath

if (Test-LabVmRunning -VmxPath $vmxPath) {
    throw "$Name is running. Shut it down before changing how it boots."
}

Write-LabStep "machine: $vmxPath"

# ---- take the recovery disc out --------------------------------------------

# The recovery media is the first drive, put there by new-restore-vm.ps1. Left
# connected with the disc first in the boot order, the machine would start the
# recovery application again and prove nothing.
if (-not $KeepRecoveryDisc) {
    Set-VmxSetting -VmxPath $vmxPath -Key 'sata0:0.startConnected' -Value 'FALSE'
    Write-LabStep 'recovery disc disconnected'
} else {
    Write-LabStep 'recovery disc left in, as asked'
}

# ---- boot the disk ----------------------------------------------------------

Set-VmxSetting -VmxPath $vmxPath -Key 'bios.bootOrder' -Value 'hdd,cdrom'
Set-VmxSetting -VmxPath $vmxPath -Key 'bios.bootDelay' -Value '3000'
Write-LabStep 'boot order: the disk first'

$serialLog = Join-Path (Join-Path $lab 'evidence') "$Name-console.log"
Write-LabStep "console log: $serialLog"

if (-not $NoStart) {
    Write-LabStep 'starting the machine; it boots what was restored'
    Start-LabVm -VmxPath $vmxPath
}

[pscustomobject]@{
    Vmx       = $vmxPath
    SerialLog = $serialLog
}
