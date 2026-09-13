<#
.SYNOPSIS
    Builds MjolnirVSS and produces the portable release folder.

.DESCRIPTION
    Produces a folder that can be copied onto an external drive and run from
    there. Nothing is installed and nothing is registered.

    Two things happen here that the build alone cannot do:

    1. MjolnirVSS-Restore.exe is renamed to MjolnirVSS.Restore.exe. Cargo does
       not allow a dot in a target name, so the file has to be built under a
       different name and renamed.

    2. The imports of the recovery executable are checked. It must not import
       vssapi.dll, because that library is not part of a base Windows PE image
       and a missing import would stop the recovery application from starting at
       the moment it is needed. This is a hard failure, not a warning.

.PARAMETER OutputPath
    Where to write the release folder. Defaults to dist\MjolnirVSS.

.PARAMETER SkipTests
    Skip the test run. Only for iterating locally.

.EXAMPLE
    .\scripts\package.ps1
#>
[CmdletBinding()]
param(
    [string] $OutputPath = "dist\MjolnirVSS",
    [switch] $SkipTests
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# Cargo writes its progress to stderr. With ErrorActionPreference set to Stop,
# PowerShell turns that into a terminating error even when the build succeeded,
# so native commands are run with the preference relaxed and judged by their
# exit code instead, which is the only thing that actually means anything.
function Invoke-Native {
    param([Parameter(Mandatory)][scriptblock] $Command,
          [Parameter(Mandatory)][string] $What)

    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        & $Command
    } finally {
        $ErrorActionPreference = $previous
    }
    if ($LASTEXITCODE -ne 0) { throw "$What failed with exit code $LASTEXITCODE" }
}

$RepoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $RepoRoot
try {
    $Target = "x86_64-pc-windows-msvc"
    $BuildDir = Join-Path $RepoRoot "target\$Target\release"

    Write-Host "Building MjolnirVSS (release, static C runtime)" -ForegroundColor Cyan
    Invoke-Native -What "the build" -Command { cargo build --release --workspace }

    if (-not $SkipTests) {
        Write-Host "Running tests" -ForegroundColor Cyan
        Invoke-Native -What "the tests (nothing is packaged from a failing build)" `
            -Command { cargo test --release --workspace }
    }

    # --- assemble -------------------------------------------------------
    if (Test-Path $OutputPath) { Remove-Item -Recurse -Force $OutputPath }
    New-Item -ItemType Directory -Force -Path $OutputPath | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $OutputPath "docs") | Out-Null

    $backupExe  = Join-Path $BuildDir "MjolnirVSS.exe"
    $restoreSrc = Join-Path $BuildDir "MjolnirVSS-Restore.exe"
    foreach ($required in @($backupExe, $restoreSrc)) {
        if (-not (Test-Path $required)) { throw "the build did not produce $required" }
    }

    Copy-Item $backupExe (Join-Path $OutputPath "MjolnirVSS.exe")
    # Cargo cannot name a target with a dot in it, so the rename happens here.
    Copy-Item $restoreSrc (Join-Path $OutputPath "MjolnirVSS.Restore.exe")

    Copy-Item (Join-Path $RepoRoot "LICENSE")  $OutputPath
    Copy-Item (Join-Path $RepoRoot "README.md") $OutputPath
    if (Test-Path (Join-Path $RepoRoot "NOTICE")) {
        Copy-Item (Join-Path $RepoRoot "NOTICE") $OutputPath
    }
    if (Test-Path (Join-Path $RepoRoot "CHANGELOG.md")) {
        Copy-Item (Join-Path $RepoRoot "CHANGELOG.md") $OutputPath
    }
    Copy-Item (Join-Path $RepoRoot "docs\*.md") (Join-Path $OutputPath "docs")

    # The README shows the window, so the pictures have to come with it or the
    # copy in the package is a document full of broken links.
    $images = Join-Path $RepoRoot "docs\images"
    if (Test-Path $images) {
        Copy-Item $images (Join-Path $OutputPath "docs") -Recurse
    }

    # --- check the recovery executable ----------------------------------
    # This is the check that matters. If it ever fails, the recovery
    # application has gained a dependency that Windows PE does not have.
    $dumpbin = Get-ChildItem "C:\Program Files\Microsoft Visual Studio\*\*\VC\Tools\MSVC\*\bin\HostX64\x64\dumpbin.exe" -ErrorAction SilentlyContinue |
        Select-Object -Last 1

    if ($null -eq $dumpbin) {
        Write-Warning "dumpbin.exe was not found, so the recovery executable's imports were NOT checked."
        Write-Warning "Install the Visual Studio C++ build tools and run this script again before releasing."
    } else {
        $restoreExe = Join-Path $OutputPath "MjolnirVSS.Restore.exe"
        $ErrorActionPreference = "Continue"
        $imports = & $dumpbin.FullName /imports $restoreExe |
            Select-String -Pattern "^\s{4}\S+\.dll" |
            ForEach-Object { $_.ToString().Trim().ToLowerInvariant() } |
            Sort-Object -Unique
        $ErrorActionPreference = "Stop"

        Write-Host "Recovery executable imports:" -ForegroundColor Cyan
        $imports | ForEach-Object { Write-Host "  $_" }

        if ($imports -contains "vssapi.dll") {
            throw "MjolnirVSS.Restore.exe imports vssapi.dll, which is not present in Windows PE. The recovery application would fail to start. Check that nothing in apps/mjolnirvss-restore depends on mjolnir-vss."
        }

        # Everything the recovery application is allowed to need. Anything else
        # has to be checked against a real Windows PE image before it ships.
        $allowed = @(
            "kernel32.dll", "ntdll.dll", "user32.dll", "gdi32.dll",
            "comctl32.dll", "oleaut32.dll", "advapi32.dll", "ole32.dll",
            "combase.dll", "api-ms-win-core-synch-l1-2-0.dll"
        )
        $unexpected = $imports | Where-Object { $allowed -notcontains $_ }
        if ($unexpected) {
            Write-Warning "The recovery executable imports libraries that have not been checked against Windows PE:"
            $unexpected | ForEach-Object { Write-Warning "  $_" }
            Write-Warning "Confirm each one exists in your Windows PE image before relying on this build."
        } else {
            Write-Host "No unexpected imports. Every library is one Windows PE provides." -ForegroundColor Green
        }
    }

    # --- report ---------------------------------------------------------
    Write-Host ""
    Write-Host "Release folder: $OutputPath" -ForegroundColor Green
    Get-ChildItem -Recurse -File $OutputPath |
        Select-Object @{n = "File"; e = { $_.FullName.Substring($RepoRoot.Length + 1) } },
                      @{n = "Size";  e = { "{0:N0} bytes" -f $_.Length } } |
        Format-Table -AutoSize

    Write-Host "Copy this folder to an external drive. Nothing needs installing." -ForegroundColor Green
}
finally {
    Pop-Location
}
