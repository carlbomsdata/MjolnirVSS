<#
.SYNOPSIS
    Shared helpers for the MjolnirVSS virtual machine test harness.

.DESCRIPTION
    This is the opt-in end-to-end harness. It builds disposable VMware
    Workstation virtual machines, installs Windows in one of them, takes a real
    backup, restores it onto a blank virtual disk, and boots the result.

    Nothing here runs as part of `cargo test`. It is driven by the scripts next
    to it, and every one of them refuses to touch a path it did not create.

    THE SAFETY RULES, which are enforced rather than documented:

      * every file this harness writes lives under one lab root, and that root
        is resolved and checked before anything is created or deleted;
      * a virtual machine is only ever stopped, reverted or deleted if its
        files are inside that root AND its display name carries the harness
        prefix;
      * nothing writes to a physical disk, ever;
      * the host's own VMware inventory, preferences and existing virtual
        machines are never modified.

    Copyright (C) the MjolnirVSS contributors.
    Licensed under the GNU General Public License, version 3 or later.
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Every virtual machine this harness owns is named with this prefix. It is half
# of the check that stops the harness touching a machine somebody else made.
$script:NamePrefix = 'MjolnirVSS-Test-'

# ---------------------------------------------------------------- the lab ----

function Get-LabRoot {
    <#
    .SYNOPSIS
        The one directory everything this harness creates lives under.
    #>
    [CmdletBinding()]
    param(
        [string] $Root = $env:MJOLNIR_LAB_ROOT
    )
    if ([string]::IsNullOrWhiteSpace($Root)) { $Root = 'C:\MjolnirVSS-TestLab' }

    $full = [System.IO.Path]::GetFullPath($Root)
    if (-not (Test-Path -LiteralPath $full)) {
        New-Item -ItemType Directory -Path $full -Force | Out-Null
    }
    foreach ($sub in 'vms', 'media', 'evidence', 'work') {
        $p = Join-Path $full $sub
        if (-not (Test-Path -LiteralPath $p)) { New-Item -ItemType Directory -Path $p -Force | Out-Null }
    }
    return $full
}

function Assert-InsideLab {
    <#
    .SYNOPSIS
        Refuses a path that is not inside the lab root.
    .DESCRIPTION
        Called before every create and every delete. A harness bug that
        produced a path outside the lab stops here rather than on somebody's
        real virtual machine.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $Path,
        [string] $Root = (Get-LabRoot)
    )
    $full = [System.IO.Path]::GetFullPath($Path)
    $rootFull = [System.IO.Path]::GetFullPath($Root).TrimEnd('\') + '\'
    if (-not $full.StartsWith($rootFull, [StringComparison]::OrdinalIgnoreCase)) {
        throw "refusing to touch '$full': it is outside the lab root '$rootFull'"
    }
    return $full
}

# ------------------------------------------------------------- vmware bits ----

function Get-VMwarePaths {
    <#
    .SYNOPSIS
        Locates the VMware Workstation tools this harness drives.
    #>
    [CmdletBinding()] param()

    $install = $null
    foreach ($key in @(
            'HKLM:\SOFTWARE\WOW6432Node\VMware, Inc.\VMware Workstation',
            'HKLM:\SOFTWARE\VMware, Inc.\VMware Workstation')) {
        if (Test-Path $key) {
            $value = (Get-ItemProperty $key).InstallPath
            if ($value) { $install = $value.TrimEnd('\'); break }
        }
    }
    if (-not $install) { throw 'VMware Workstation is not installed on this computer' }

    $paths = [ordered]@{
        Install       = $install
        VmRun         = Join-Path $install 'vmrun.exe'
        VDiskManager  = Join-Path $install 'vmware-vdiskmanager.exe'
        Vmware        = Join-Path $install 'vmware.exe'
        Version       = (Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\VMware, Inc.\VMware Workstation' -ErrorAction SilentlyContinue).ProductVersion
    }
    foreach ($needed in 'VmRun', 'VDiskManager') {
        if (-not (Test-Path -LiteralPath $paths[$needed])) {
            throw "VMware Workstation is installed but $needed is missing at $($paths[$needed])"
        }
    }
    return [pscustomobject]$paths
}


function Invoke-Native {
    <#
    .SYNOPSIS
        Runs a native program and returns everything it printed.
    .DESCRIPTION
        Native tools write progress to stderr, and with the strict error
        preference this module runs under, PowerShell turns that into a
        terminating error even when the program succeeded. The preference is
        relaxed for the call and the program is judged by its exit code, which
        is returned alongside its output.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $Executable,
        [Parameter(Mandatory)] [string[]] $Arguments
    )
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = & $Executable @Arguments 2>&1 | Out-String
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
    }
    return [pscustomobject]@{ ExitCode = $code; Output = $output.TrimEnd() }
}

function Invoke-VmRun {
    <#
    .SYNOPSIS
        Runs vmrun and returns its output, throwing on failure.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string[]] $Arguments,
        [switch] $AllowFailure
    )
    $vmrun = (Get-VMwarePaths).VmRun
    $result = Invoke-Native -Executable $vmrun -Arguments $Arguments
    if ($result.ExitCode -ne 0 -and -not $AllowFailure) {
        throw "vmrun $($Arguments -join ' ') failed with exit code $($result.ExitCode):`n$($result.Output)"
    }
    return $result.Output
}

function Get-RunningVms {
    <#
    .SYNOPSIS
        The vmx paths VMware currently has running.
    #>
    [CmdletBinding()] param()
    $text = Invoke-VmRun -Arguments @('list') -AllowFailure
    $text -split "`r?`n" |
        Where-Object { $_ -match '\.vmx\s*$' } |
        ForEach-Object { $_.Trim() }
}

# ----------------------------------------------------------- the vmx file ----

function New-LabVmx {
    <#
    .SYNOPSIS
        Writes a .vmx for a disposable UEFI Windows virtual machine.

    .DESCRIPTION
        Written by hand rather than by the VMware wizard so the layout is
        reproducible and reviewable: what is in the file is exactly what the
        test depends on, and nothing else.

        The serial port is the evidence channel. It is a plain file on the
        host, so a guest with no VMware Tools installed can still report what
        happened by writing to COM1.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [string] $Directory,
        [int] $MemoryMB = 4096,
        [int] $Cpus = 2,
        [string[]] $Disks = @(),
        [string[]] $IsoPaths = @(),
        [switch] $SecureBoot,
        [string] $SerialLog,
        [int] $VncPort = 0,
        [string] $GuestOS = 'windows11-64'
    )

    if (-not $Name.StartsWith($script:NamePrefix)) {
        throw "a lab virtual machine must be named '$($script:NamePrefix)...', not '$Name'"
    }
    $Directory = Assert-InsideLab -Path $Directory
    if (-not (Test-Path -LiteralPath $Directory)) {
        New-Item -ItemType Directory -Path $Directory -Force | Out-Null
    }

    $lines = [System.Collections.Generic.List[string]]::new()
    $add = { param($text) $lines.Add($text) }

    & $add '#!/usr/bin/env vmware'
    & $add '.encoding = "windows-1252"'
    & $add 'config.version = "8"'
    & $add 'virtualHW.version = "21"'
    & $add "displayName = `"$Name`""
    & $add "guestOS = `"$GuestOS`""
    & $add 'firmware = "efi"'
    if ($SecureBoot) { & $add 'uefi.secureBoot.enabled = "TRUE"' }
    & $add "memsize = `"$MemoryMB`""
    & $add "numvcpus = `"$Cpus`""
    & $add "cpuid.coresPerSocket = `"$Cpus`""

    # The PCI bridges. A virtual machine built by the VMware wizard always has
    # these; one written by hand has to declare them, because without them
    # there are only a handful of PCIe slots and the machine runs out part way
    # through building itself. VMware does not fail gracefully when it does.
    & $add 'pciBridge0.present = "TRUE"'
    foreach ($bridge in 4, 5, 6, 7) {
        & $add "pciBridge$bridge.present = `"TRUE`""
        & $add "pciBridge$bridge.virtualDev = `"pcieRootPort`""
        & $add "pciBridge$bridge.functions = `"8`""
    }
    & $add 'vmci0.present = "TRUE"'
    & $add 'hpet0.present = "TRUE"'

    # NVMe, because it is what a modern machine has and because Windows 11
    # carries the driver in its installation image.
    & $add 'nvme0.present = "TRUE"'
    for ($i = 0; $i -lt $Disks.Count; $i++) {
        $disk = Split-Path -Leaf $Disks[$i]
        & $add "nvme0:$i.present = `"TRUE`""
        & $add "nvme0:$i.fileName = `"$disk`""
        & $add "nvme0:$i.deviceType = `"disk`""
    }

    & $add 'sata0.present = "TRUE"'
    for ($i = 0; $i -lt $IsoPaths.Count; $i++) {
        & $add "sata0:$i.present = `"TRUE`""
        & $add "sata0:$i.deviceType = `"cdrom-image`""
        & $add "sata0:$i.fileName = `"$($IsoPaths[$i])`""
        & $add "sata0:$i.startConnected = `"TRUE`""
    }

    # NAT, so the guest can reach Microsoft for activation-free setup and
    # nothing can reach the guest.
    & $add 'ethernet0.present = "TRUE"'
    & $add 'ethernet0.connectionType = "nat"'
    & $add 'ethernet0.virtualDev = "e1000e"'
    & $add 'ethernet0.addressType = "generated"'

    # The framebuffer, served over VNC on the loopback address. This is how the
    # harness sees a machine with no VMware Tools: Windows Setup while it runs,
    # Windows PE while the recovery application is on screen, and a restored
    # Windows that has never been logged into. Nothing is sent to the machine
    # through it.
    if ($VncPort -gt 0) {
        & $add 'RemoteDisplay.vnc.enabled = "TRUE"'
        & $add "RemoteDisplay.vnc.port = `"$VncPort`""
        & $add 'RemoteDisplay.vnc.ip = "127.0.0.1"'
    }

    if ($SerialLog) {
        & $add 'serial0.present = "TRUE"'
        & $add 'serial0.fileType = "file"'
        & $add "serial0.fileName = `"$SerialLog`""
        & $add 'serial0.tryNoRxLoss = "FALSE"'
        & $add 'serial0.yieldOnMsrRead = "TRUE"'
    }

    & $add 'usb.present = "TRUE"'
    & $add 'ehci.present = "TRUE"'
    & $add 'svga.present = "TRUE"'
    & $add 'sound.present = "FALSE"'
    & $add 'floppy0.present = "FALSE"'
    & $add 'tools.syncTime = "TRUE"'
    & $add 'tools.upgrade.policy = "manual"'
    # Answer VMware's own dialogs automatically: an unattended run must never
    # stop on a question nobody is there to read.
    & $add 'msg.autoAnswer = "TRUE"'
    & $add 'bios.bootDelay = "2000"'

    $vmxPath = Join-Path $Directory "$Name.vmx"
    Set-Content -LiteralPath $vmxPath -Value ($lines -join "`r`n") -Encoding ASCII
    return $vmxPath
}

function Set-VmxSetting {
    <#
    .SYNOPSIS
        Changes or adds one setting in a lab .vmx.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $VmxPath,
        [Parameter(Mandatory)] [string] $Key,
        [Parameter(Mandatory)] [string] $Value
    )
    $VmxPath = Assert-InsideLab -Path $VmxPath
    $lines = @(Get-Content -LiteralPath $VmxPath)
    $pattern = '^\s*' + [regex]::Escape($Key) + '\s*='
    $replacement = "$Key = `"$Value`""

    if ($lines -match $pattern) {
        $lines = $lines | ForEach-Object { if ($_ -match $pattern) { $replacement } else { $_ } }
    } else {
        $lines += $replacement
    }
    Set-Content -LiteralPath $VmxPath -Value ($lines -join "`r`n") -Encoding ASCII
}

function Remove-VmxSetting {
    <#
    .SYNOPSIS
        Removes every setting whose key starts with a prefix.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $VmxPath,
        [Parameter(Mandatory)] [string] $KeyPrefix
    )
    $VmxPath = Assert-InsideLab -Path $VmxPath
    $pattern = '^\s*' + [regex]::Escape($KeyPrefix)
    $lines = @(Get-Content -LiteralPath $VmxPath) | Where-Object { $_ -notmatch $pattern }
    Set-Content -LiteralPath $VmxPath -Value ($lines -join "`r`n") -Encoding ASCII
}

# ----------------------------------------------------------------- disks ----

function New-LabDisk {
    <#
    .SYNOPSIS
        Creates a blank growable virtual disk inside the lab.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $Path,
        [Parameter(Mandatory)] [int] $SizeGB,
        [switch] $Force
    )
    $Path = Assert-InsideLab -Path $Path
    if (Test-Path -LiteralPath $Path) {
        if (-not $Force) { return $Path }
        Remove-LabDisk -Path $Path
    }
    $parent = Split-Path -Parent $Path
    if (-not (Test-Path -LiteralPath $parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }

    $vdm = (Get-VMwarePaths).VDiskManager
    # -t 0 is a single growable file, which keeps a 64 GB disk down to what is
    # actually written and makes the lab cheap to throw away.
    $result = Invoke-Native -Executable $vdm -Arguments @('-c', '-s', "${SizeGB}GB", '-a', 'lsilogic', '-t', '0', $Path)
    if (-not (Test-Path -LiteralPath $Path)) {
        throw "creating the virtual disk at $Path failed:`n$($result.Output)"
    }
    return $Path
}

function Remove-LabDisk {
    <#
    .SYNOPSIS
        Deletes a virtual disk, and only one inside the lab.
    #>
    [CmdletBinding()]
    param([Parameter(Mandatory)] [string] $Path)
    $Path = Assert-InsideLab -Path $Path
    if (Test-Path -LiteralPath $Path) {
        Remove-Item -LiteralPath $Path -Force
    }
    # A growable disk can have extents beside it.
    $base = [System.IO.Path]::GetFileNameWithoutExtension($Path)
    $dir = Split-Path -Parent $Path
    Get-ChildItem -LiteralPath $dir -Filter "$base-s*.vmdk" -ErrorAction SilentlyContinue |
        ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force }
}

# ------------------------------------------------------------ power state ----

function Assert-LabVm {
    <#
    .SYNOPSIS
        Refuses to act on a virtual machine this harness does not own.
    .DESCRIPTION
        Both conditions have to hold: the files are inside the lab root, and
        the display name carries the harness prefix. Either one alone could be
        satisfied by accident.
    #>
    [CmdletBinding()]
    param([Parameter(Mandatory)] [string] $VmxPath)

    $full = Assert-InsideLab -Path $VmxPath
    if (-not (Test-Path -LiteralPath $full)) { throw "no virtual machine at $full" }

    $name = (Get-Content -LiteralPath $full |
        Where-Object { $_ -match '^\s*displayName\s*=' } |
        ForEach-Object { ($_ -split '=', 2)[1].Trim().Trim('"') }) | Select-Object -First 1

    if (-not $name -or -not $name.StartsWith($script:NamePrefix)) {
        throw "refusing to act on '$full': its display name '$name' is not a lab machine"
    }
    return $full
}

function Start-LabVm {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $VmxPath,
        [switch] $Headless
    )
    $VmxPath = Assert-LabVm -VmxPath $VmxPath
    $mode = if ($Headless) { 'nogui' } else { 'gui' }
    Invoke-VmRun -Arguments @('start', $VmxPath, $mode) | Out-Null
}

function Stop-LabVm {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $VmxPath,
        [switch] $Hard
    )
    $VmxPath = Assert-LabVm -VmxPath $VmxPath
    $how = if ($Hard) { 'hard' } else { 'soft' }
    Invoke-VmRun -Arguments @('stop', $VmxPath, $how) -AllowFailure | Out-Null
}

function Test-LabVmRunning {
    [CmdletBinding()]
    param([Parameter(Mandatory)] [string] $VmxPath)
    $VmxPath = [System.IO.Path]::GetFullPath($VmxPath)
    $running = Get-RunningVms
    foreach ($vm in $running) {
        if ([System.IO.Path]::GetFullPath($vm) -ieq $VmxPath) { return $true }
    }
    return $false
}

function Wait-LabVmStopped {
    <#
    .SYNOPSIS
        Waits for a virtual machine to power itself off.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $VmxPath,
        [int] $TimeoutMinutes = 60,
        [string] $Because = 'the virtual machine to finish'
    )
    $VmxPath = Assert-LabVm -VmxPath $VmxPath
    $deadline = (Get-Date).AddMinutes($TimeoutMinutes)
    while ((Get-Date) -lt $deadline) {
        if (-not (Test-LabVmRunning -VmxPath $VmxPath)) { return $true }
        Start-Sleep -Seconds 10
    }
    throw "timed out after $TimeoutMinutes minutes waiting for $Because"
}

function New-LabSnapshot {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $VmxPath,
        [Parameter(Mandatory)] [string] $Name
    )
    $VmxPath = Assert-LabVm -VmxPath $VmxPath
    Invoke-VmRun -Arguments @('snapshot', $VmxPath, $Name) | Out-Null
}

function Restore-LabSnapshot {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $VmxPath,
        [Parameter(Mandatory)] [string] $Name
    )
    $VmxPath = Assert-LabVm -VmxPath $VmxPath
    Invoke-VmRun -Arguments @('revertToSnapshot', $VmxPath, $Name) | Out-Null
}

# ------------------------------------------------------------------ isos ----

function Get-OscdimgPath {
    <#
    .SYNOPSIS
        Locates oscdimg from the Windows ADK Deployment Tools.
    #>
    [CmdletBinding()] param()
    $candidates = @(
        'C:\Program Files (x86)\Windows Kits\10\Assessment and Deployment Kit\Deployment Tools\amd64\Oscdimg\oscdimg.exe',
        'C:\Program Files\Windows Kits\10\Assessment and Deployment Kit\Deployment Tools\amd64\Oscdimg\oscdimg.exe'
    )
    foreach ($c in $candidates) { if (Test-Path -LiteralPath $c) { return $c } }
    throw 'oscdimg.exe was not found. Install the Windows ADK Deployment Tools.'
}

function New-LabIso {
    <#
    .SYNOPSIS
        Builds a data ISO from a folder.

    .DESCRIPTION
        Used for the answer file and for carrying a build of MjolnirVSS into a
        guest that has no VMware Tools. Data only, not bootable: the bootable
        one is built by the recovery media scripts, which have their own rules
        about Microsoft components.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $SourceDirectory,
        [Parameter(Mandatory)] [string] $IsoPath,
        [string] $Label = 'MJOLNIR'
    )
    $SourceDirectory = Assert-InsideLab -Path $SourceDirectory
    $IsoPath = Assert-InsideLab -Path $IsoPath
    if (Test-Path -LiteralPath $IsoPath) { Remove-Item -LiteralPath $IsoPath -Force }

    $oscdimg = Get-OscdimgPath
    $result = Invoke-Native -Executable $oscdimg -Arguments @('-n', '-m', "-l$Label", $SourceDirectory, $IsoPath)
    if (-not (Test-Path -LiteralPath $IsoPath)) {
        throw "building the ISO at $IsoPath failed:`n$($result.Output)"
    }
    return $IsoPath
}

# -------------------------------------------------------------- evidence ----

function Write-LabEvidence {
    <#
    .SYNOPSIS
        Saves a piece of evidence under the lab's evidence folder.
    #>
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [string] $Content
    )
    $path = Join-Path (Join-Path (Get-LabRoot) 'evidence') $Name
    $path = Assert-InsideLab -Path $path
    $parent = Split-Path -Parent $path
    if (-not (Test-Path -LiteralPath $parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
    Set-Content -LiteralPath $path -Value $Content -Encoding UTF8
    return $path
}

function Write-LabStep {
    <#
    .SYNOPSIS
        Prints a progress line that is easy to find in a long transcript.
    #>
    [CmdletBinding()]
    param([Parameter(Mandatory)] [string] $Message)
    $stamp = (Get-Date).ToString('HH:mm:ss')
    Write-Host "[$stamp] $Message" -ForegroundColor Cyan
}

Export-ModuleMember -Function @(
    'Get-LabRoot', 'Assert-InsideLab', 'Get-VMwarePaths', 'Invoke-Native', 'Invoke-VmRun',
    'Get-RunningVms',
    'New-LabVmx', 'Set-VmxSetting', 'Remove-VmxSetting',
    'New-LabDisk', 'Remove-LabDisk',
    'Assert-LabVm', 'Start-LabVm', 'Stop-LabVm', 'Test-LabVmRunning', 'Wait-LabVmStopped',
    'New-LabSnapshot', 'Restore-LabSnapshot',
    'Get-OscdimgPath', 'New-LabIso',
    'Write-LabEvidence', 'Write-LabStep'
)
