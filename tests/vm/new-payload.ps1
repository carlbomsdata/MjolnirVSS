<#
.SYNOPSIS
    Rebuilds the payload disc a test virtual machine reads its scripts from.

.DESCRIPTION
    The test machines have no VMware Tools, so there is no way to copy a file
    into one. What there is instead is a CD: this builds one holding a fresh
    MjolnirVSS release and the guest scripts, and swaps it into a machine's
    virtual drive.

    Run it between test phases. The machine has to be powered off, because
    changing which disc is in the drive while it is running needs the Tools
    that are not there.

.PARAMETER Vmx
    The virtual machine to put the disc into. Optional; the disc is built
    either way.

.EXAMPLE
    pwsh -File tests\vm\new-payload.ps1 -Vmx C:\MjolnirVSS-TestLab\vms\...\x.vmx
#>

[CmdletBinding()]
param(
    [string] $Vmx,
    [switch] $SkipBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

Import-Module (Join-Path $PSScriptRoot 'MjolnirLab.psm1') -Force

$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$lab = Get-LabRoot
$work = Join-Path (Join-Path $lab 'work') 'payload'
$iso = Join-Path (Join-Path $lab 'media') 'mjolnir-payload.iso'

# ---- a fresh build ---------------------------------------------------------

if (-not $SkipBuild) {
    Write-LabStep 'building the portable release'
    $package = Join-Path $repo 'scripts\package.ps1'
    $result = Invoke-Native -Executable 'powershell.exe' -Arguments @(
        '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $package, '-SkipTests'
    )
    if ($result.ExitCode -ne 0) {
        throw "the release build failed with exit code $($result.ExitCode):`n$($result.Output)"
    }
}

$dist = Join-Path $repo 'dist\MjolnirVSS'
if (-not (Test-Path -LiteralPath $dist)) {
    throw "no release folder at $dist; run scripts\package.ps1 first"
}

# ---- assemble the disc -----------------------------------------------------

Write-LabStep 'assembling the payload disc'
if (Test-Path -LiteralPath $work) { Remove-Item -LiteralPath (Assert-InsideLab -Path $work) -Recurse -Force }
New-Item -ItemType Directory -Path $work -Force | Out-Null

Copy-Item -LiteralPath $dist -Destination (Join-Path $work 'MjolnirVSS') -Recurse
Copy-Item -Path (Join-Path $PSScriptRoot 'guest\*.ps1') -Destination $work

# A marker the guest scripts look for, so they can find the disc without
# assuming a drive letter.
Set-Content -LiteralPath (Join-Path $work 'mjolnir-payload.txt') `
    -Value (Get-Date -Format s) -Encoding ASCII

New-LabIso -SourceDirectory $work -IsoPath $iso -Label 'MJOLNIRPAY' | Out-Null
$size = (Get-Item -LiteralPath $iso).Length
Write-LabStep ("payload disc: {0} ({1:N1} MB)" -f $iso, ($size / 1MB))

# ---- put it in the machine -------------------------------------------------

if ($Vmx) {
    $Vmx = Assert-LabVm -VmxPath $Vmx
    if (Test-LabVmRunning -VmxPath $Vmx) {
        throw "$Vmx is running. Shut it down before changing its disc."
    }
    # sata0:1 is the payload drive; sata0:0 is the installation disc.
    Set-VmxSetting -VmxPath $Vmx -Key 'sata0:1.present' -Value 'TRUE'
    Set-VmxSetting -VmxPath $Vmx -Key 'sata0:1.deviceType' -Value 'cdrom-image'
    Set-VmxSetting -VmxPath $Vmx -Key 'sata0:1.fileName' -Value $iso
    Set-VmxSetting -VmxPath $Vmx -Key 'sata0:1.startConnected' -Value 'TRUE'
    Write-LabStep "put the disc into $(Split-Path -Leaf $Vmx)"
}

return $iso
