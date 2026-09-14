<#
.SYNOPSIS
    Builds the disposable Windows 11 machine the MjolnirVSS tests back up.

.DESCRIPTION
    Creates MjolnirVSS-Test-Source: a UEFI, GPT, NVMe virtual machine with an
    EFI system partition, a Microsoft Reserved partition, Windows, and a
    recovery partition, plus a second blank disk to write backups to.

    Windows installs unattended. At its first logon the machine runs
    guest\setup-guest.ps1, which lays down the files the backup has to survive
    and reports over the serial port.

    Everything is created under the lab root and nothing else is touched. No
    existing virtual machine is read, started or modified.

.PARAMETER WindowsIso
    Path to a Windows 11 x64 installation ISO. Found automatically if omitted.

.PARAMETER Force
    Rebuild even if the machine already exists. Deletes the old one first.

.EXAMPLE
    pwsh -File tests\vm\new-source-vm.ps1
#>

[CmdletBinding()]
param(
    [string] $WindowsIso,
    # Which machine this is. The name keeps the virtual machines apart, and the
    # answer file differs per Windows: Setup is told which image to install by
    # name, and evaluation media takes no product key.
    [string] $VmName = 'MjolnirVSS-Test-Source',
    [string] $AnswerFile,
    [int] $SystemDiskGB = 64,
    [int] $BackupDiskGB = 64,
    # Enough for Windows to install and run a backup, and small enough that two
    # of these plus the host do not exhaust a 16 GB machine. The lab runs one
    # virtual machine at a time for the same reason.
    [int] $MemoryMB = 4096,
    [int] $Cpus = 4,
    [switch] $Force,
    [switch] $NoWait
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

Import-Module (Join-Path $PSScriptRoot 'MjolnirLab.psm1') -Force

$lab = Get-LabRoot
$vmDir = Join-Path (Join-Path $lab 'vms') $VmName
$work = Join-Path (Join-Path $lab 'work') "$VmName-payload"
$payloadIso = Join-Path (Join-Path $lab 'media') "$VmName-payload.iso"
$serialLog = Join-Path (Join-Path $lab 'evidence') "$VmName-console.log"

Write-LabStep "lab root:  $lab"
Write-LabStep "vm folder: $vmDir"

# ---------------------------------------------------------- prerequisites ---

$vmware = Get-VMwarePaths
Write-LabStep "VMware Workstation $($vmware.Version) at $($vmware.Install)"

if (-not $WindowsIso) {
    $candidates = @(
        'C:\DL\Win11_24H2_EnglishInternational_x64.iso',
        'C:\DL\Windows 11\Win11_24H2_EnglishInternational_x64.iso'
    )
    $WindowsIso = $candidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
}
if (-not $WindowsIso -or -not (Test-Path -LiteralPath $WindowsIso)) {
    throw 'No Windows 11 installation ISO was found. Pass one with -WindowsIso.'
}
Write-LabStep "windows iso: $WindowsIso"

# Windows Server 2025 and the other new Setup media need two key presses that
# the older media did not: the redesigned Setup asks for a language and a
# keyboard before it looks at the answer file, and no answer file setting
# suppresses those two pages. Everything after them - the image, the licence,
# the partitioning, the account - is still unattended. Press Return twice at the
# console once it has booted.
#
# The stock disc waits for a key press before it boots, and Setup does not
# reliably look on a second disc for an answer file. Both are fixed by
# rebuilding the disc once, with Microsoft's own no prompt boot image and the
# answer file at the root. The result is cached and only rebuilt when the
# answer file changes.
$answerFile = if ($AnswerFile) {
    if (-not (Test-Path -LiteralPath $AnswerFile)) { throw "no answer file at $AnswerFile" }
    (Resolve-Path -LiteralPath $AnswerFile).Path
} else {
    Join-Path $PSScriptRoot 'answer-files\source-autounattend.xml'
}
$prepared = & (Join-Path $PSScriptRoot 'new-noprompt-iso.ps1') `
    -SourceIso $WindowsIso -Inject @{ 'autounattend.xml' = $answerFile }
$WindowsIso = $prepared | Select-Object -Last 1
Write-LabStep "booting from: $WindowsIso"

Get-OscdimgPath | Out-Null

# --------------------------------------------------------- existing machine --

$vmxPath = Join-Path $vmDir "$VmName.vmx"
if (Test-Path -LiteralPath $vmxPath) {
    if (-not $Force) {
        throw "$VmName already exists at $vmxPath. Pass -Force to rebuild it, which deletes it."
    }
    Write-LabStep 'removing the previous source machine'
    if (Test-LabVmRunning -VmxPath $vmxPath) { Stop-LabVm -VmxPath $vmxPath -Hard }
    Start-Sleep -Seconds 3
    $checked = Assert-InsideLab -Path $vmDir
    Remove-Item -LiteralPath $checked -Recurse -Force
}

# ------------------------------------------------------------- the payload --

Write-LabStep 'building the payload disc'
if (Test-Path -LiteralPath $work) { Remove-Item -LiteralPath (Assert-InsideLab -Path $work) -Recurse -Force }
New-Item -ItemType Directory -Path $work -Force | Out-Null

# The answer file is on the installation disc itself; this disc carries what
# the machine needs after Windows is installed.
Copy-Item -Path (Join-Path $PSScriptRoot 'guest\*.ps1') -Destination $work

# A build of MjolnirVSS rides along so the guest has it without needing VMware
# Tools or a network share. It is rebuilt by build-payload.ps1 between runs.
$dist = Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) 'dist\MjolnirVSS'
if (Test-Path -LiteralPath $dist) {
    Write-LabStep "including the build at $dist"
    Copy-Item -LiteralPath $dist -Destination (Join-Path $work 'MjolnirVSS') -Recurse
} else {
    Write-LabStep 'no dist\MjolnirVSS yet; the disc carries the answer file only'
}

New-LabIso -SourceDirectory $work -IsoPath $payloadIso -Label 'MJOLNIRPAY' | Out-Null
Write-LabStep "payload disc: $payloadIso"

# --------------------------------------------------------------- the disks --

New-Item -ItemType Directory -Path $vmDir -Force | Out-Null
Write-LabStep "creating a ${SystemDiskGB} GB system disk and a ${BackupDiskGB} GB backup disk"
$systemDisk = New-LabDisk -Path (Join-Path $vmDir 'system.vmdk') -SizeGB $SystemDiskGB -Force
$backupDisk = New-LabDisk -Path (Join-Path $vmDir 'backup.vmdk') -SizeGB $BackupDiskGB -Force

# ----------------------------------------------------------------- the vmx --

if (Test-Path -LiteralPath $serialLog) { Remove-Item -LiteralPath $serialLog -Force }

$vmxPath = New-LabVmx `
    -Name $VmName `
    -Directory $vmDir `
    -MemoryMB $MemoryMB `
    -Cpus $Cpus `
    -Disks @($systemDisk, $backupDisk) `
    -IsoPaths @($WindowsIso, $payloadIso) `
    -SecureBoot `
    -SerialLog $serialLog `
    -VncPort 5990

Write-LabStep "wrote $vmxPath"

# ------------------------------------------------------------------- go ----

Write-LabStep 'starting the machine; Windows Setup runs unattended from here'
Start-LabVm -VmxPath $vmxPath

if ($NoWait) {
    Write-LabStep 'not waiting, as asked'
    return [pscustomobject]@{ Vmx = $vmxPath; SerialLog = $serialLog }
}

Write-LabStep "watching $serialLog for the guest to report it is ready"
$deadline = (Get-Date).AddMinutes(90)
$lastSize = -1
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 20
    if (Test-Path -LiteralPath $serialLog) {
        $text = Get-Content -LiteralPath $serialLog -Raw -ErrorAction SilentlyContinue
        if ($text) {
            if ($text.Length -ne $lastSize) {
                $lastSize = $text.Length
                $tail = ($text -split "`r?`n" | Where-Object { $_ } | Select-Object -Last 1)
                if ($tail) { Write-LabStep "guest: $tail" }
            }
            if ($text -match 'GUEST-SETUP-COMPLETE') {
                Write-LabStep 'the guest reported it is ready'
                break
            }
            if ($text -match 'GUEST-SETUP-FAILED') {
                throw "the guest reported a failure; see $serialLog"
            }
        }
    }
}

if (-not (Test-Path -LiteralPath $serialLog) -or
    -not ((Get-Content -LiteralPath $serialLog -Raw) -match 'GUEST-SETUP-COMPLETE')) {
    throw "the guest did not report ready within 90 minutes; look at the machine and at $serialLog"
}

Write-LabStep 'taking a clean snapshot so tests can start from a known state'
New-LabSnapshot -VmxPath $vmxPath -Name 'clean'

[pscustomobject]@{
    Vmx        = $vmxPath
    SystemDisk = $systemDisk
    BackupDisk = $backupDisk
    SerialLog  = $serialLog
}
