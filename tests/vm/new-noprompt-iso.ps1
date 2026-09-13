<#
.SYNOPSIS
    Rebuilds a Windows installation ISO so it boots unattended.

.DESCRIPTION
    Two things about a stock Windows disc stop it installing by itself.

    It waits for a key press. The EFI image it boots through prints
    "Press any key to boot from CD or DVD" and gives up after a few seconds,
    which with nobody watching means the firmware times out and the machine
    boots nothing:

        Guest: About to do EFI boot: EFI VMware Virtual SATA CDROM Drive (0.0)
        Guest: Status upon boot failure: Time out

    Microsoft ships the answer in the Windows ADK: `efisys_noprompt.bin`, the
    same EFI boot image without the prompt.

    And Setup does not reliably look on a second disc for `autounattend.xml`.
    The place it always looks is the root of the installation media itself, so
    the answer file is put there.

    Nothing in the original ISO is modified, and nothing Microsoft owns leaves
    this computer: the result lives in the lab folder and is deleted with it.

.PARAMETER SourceIso
    The Windows installation ISO to rebuild.

.PARAMETER Inject
    Files to place at the root of the rebuilt disc, as a hashtable of
    name to path.

.EXAMPLE
    pwsh -File tests\vm\new-noprompt-iso.ps1 -SourceIso C:\DL\Win11.iso
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $SourceIso,
    [hashtable] $Inject = @{},
    [string] $OutputIso,
    [switch] $Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

Import-Module (Join-Path $PSScriptRoot 'MjolnirLab.psm1') -Force

$lab = Get-LabRoot
if (-not $OutputIso) {
    $leaf = [System.IO.Path]::GetFileNameWithoutExtension($SourceIso)
    $OutputIso = Join-Path (Join-Path $lab 'media') "$leaf-unattended.iso"
}
$OutputIso = Assert-InsideLab -Path $OutputIso
$stampFile = "$OutputIso.built-from.txt"

# What the result depends on, so a changed answer file rebuilds the disc and an
# unchanged one does not. Rebuilding takes minutes and several gigabytes.
$parts = @("source=$((Get-Item -LiteralPath $SourceIso).Length)")
foreach ($name in ($Inject.Keys | Sort-Object)) {
    $hash = (Get-FileHash -LiteralPath $Inject[$name] -Algorithm SHA256).Hash
    $parts += "$name=$hash"
}
$stamp = $parts -join "`n"

if ((Test-Path -LiteralPath $OutputIso) -and -not $Force) {
    $previous = if (Test-Path -LiteralPath $stampFile) {
        (Get-Content -LiteralPath $stampFile -Raw).TrimEnd()
    } else { '' }
    if ($previous -eq $stamp) {
        Write-LabStep "already built and unchanged: $OutputIso"
        return $OutputIso
    }
    Write-LabStep 'the answer file changed; rebuilding the disc'
}

if (-not (Test-Path -LiteralPath $SourceIso)) { throw "no ISO at $SourceIso" }

$extract = Join-Path (Join-Path $lab 'work') 'windows-iso'
$extract = Assert-InsideLab -Path $extract
if (Test-Path -LiteralPath $extract) { Remove-Item -LiteralPath $extract -Recurse -Force }
New-Item -ItemType Directory -Path $extract -Force | Out-Null

Write-LabStep "mounting $SourceIso read only"
$image = Mount-DiskImage -ImagePath $SourceIso -PassThru -Access ReadOnly
try {
    $letter = ($image | Get-Volume).DriveLetter
    if (-not $letter) { throw 'the ISO mounted but no drive letter appeared' }
    Write-LabStep "copying the disc from ${letter}: (this takes a few minutes)"

    $robo = Invoke-Native -Executable 'robocopy.exe' -Arguments @(
        "${letter}:\", $extract, '/E', '/NJH', '/NJS', '/NP', '/NFL', '/NDL', '/R:1', '/W:1'
    )
    # Robocopy uses exit codes below 8 for success with varying detail.
    if ($robo.ExitCode -ge 8) {
        throw "copying the disc failed with robocopy exit code $($robo.ExitCode):`n$($robo.Output)"
    }
} finally {
    Dismount-DiskImage -ImagePath $SourceIso | Out-Null
    Write-LabStep 'dismounted the source ISO'
}

# The read only flag comes across with the files and stops oscdimg later.
Get-ChildItem -LiteralPath $extract -Recurse -File -Force |
    Where-Object { $_.IsReadOnly } |
    ForEach-Object { $_.IsReadOnly = $false }

foreach ($name in $Inject.Keys) {
    Write-LabStep "placing $name at the root of the disc"
    Copy-Item -LiteralPath $Inject[$name] -Destination (Join-Path $extract $name) -Force

    # Setup looks in \sources as well as at the root. Putting a copy in both
    # means a change in which location it prefers does not silently turn the
    # unattended install back into an interactive one.
    if ($name -ieq 'autounattend.xml') {
        $sources = Join-Path $extract 'sources'
        if (Test-Path -LiteralPath $sources) {
            Copy-Item -LiteralPath $Inject[$name] -Destination (Join-Path $sources 'unattend.xml') -Force
        }
    }
}

$oscdimgDir = Split-Path -Parent (Get-OscdimgPath)
$etfsboot = Join-Path $oscdimgDir 'etfsboot.com'
$efisys = Join-Path $oscdimgDir 'efisys_noprompt.bin'
foreach ($f in $etfsboot, $efisys) {
    if (-not (Test-Path -LiteralPath $f)) { throw "the ADK boot file $f is missing" }
}

Write-LabStep 'building the ISO with the no prompt boot image'
if (Test-Path -LiteralPath $OutputIso) { Remove-Item -LiteralPath $OutputIso -Force }

# -u2 writes UDF, which is required: install.wim is larger than the 4 GB an
# ISO 9660 filesystem can describe.
$bootdata = "2#p0,e,b$etfsboot#pEF,e,b$efisys"
$build = Invoke-Native -Executable (Get-OscdimgPath) -Arguments @(
    '-m', '-o', '-u2', '-udfver102', "-bootdata:$bootdata", $extract, $OutputIso
)
if (-not (Test-Path -LiteralPath $OutputIso)) {
    throw "building the ISO failed with exit code $($build.ExitCode):`n$($build.Output)"
}

Write-LabStep 'removing the extracted copy'
Remove-Item -LiteralPath $extract -Recurse -Force

Set-Content -LiteralPath $stampFile -Value $stamp -Encoding UTF8
$size = (Get-Item -LiteralPath $OutputIso).Length
Write-LabStep ("built {0} ({1:N1} GB)" -f $OutputIso, ($size / 1GB))
return $OutputIso
