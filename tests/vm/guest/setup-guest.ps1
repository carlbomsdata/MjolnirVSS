<#
.SYNOPSIS
    Prepares the MjolnirVSS test source machine, from inside it.

.DESCRIPTION
    Runs once, at the first logon after an unattended install. It puts a set of
    files on the Windows volume that are chosen to exercise the parts of NTFS a
    block level backup can get wrong, records a hash of each one, and reports
    what it did over the serial port.

    The serial port is how this machine talks to the harness. It has no VMware
    Tools, so there is no other channel, and that is deliberate: a test that
    needs an agent installed is testing the agent as well.

    Nothing here is personal, and nothing here is random. Every file's contents
    are derived from its name, so the same machine built twice holds the same
    bytes and a restored copy can be checked against a rule rather than against
    a saved copy.

    Copyright (C) the MjolnirVSS contributors.
    Licensed under the GNU General Public License, version 3 or later.
#>

$ErrorActionPreference = 'Continue'

$TestRoot = 'C:\MjolnirTest'
$MarkerFile = 'C:\mjolnir-guest-ready.txt'

# --------------------------------------------------------------- reporting ---

$script:Serial = $null

function Open-Report {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open()
        $script:Serial = $port
    } catch {
        $script:Serial = $null
    }
}

function Report {
    param([string] $Message)
    $line = "[guest] $Message"
    Write-Host $line
    if ($script:Serial) {
        try { $script:Serial.WriteLine($line) } catch { }
    }
    try { Add-Content -LiteralPath 'C:\mjolnir-guest-setup.log' -Value $line -Encoding UTF8 } catch { }
}

function Close-Report {
    if ($script:Serial) {
        try { $script:Serial.Close() } catch { }
    }
}

# ----------------------------------------------------------- deterministic ---

function Get-DeterministicBytes {
    <#
    .SYNOPSIS
        Bytes derived from a seed string, so content is reproducible.
    .DESCRIPTION
        A counter run through SHA-256 with the seed. Not cryptography: this only
        has to be incompressible enough that a backup cannot make the test
        meaningless by storing one chunk and pointing everything at it.
    #>
    param(
        [Parameter(Mandatory)] [string] $Seed,
        [Parameter(Mandatory)] [int] $Length
    )
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $out = New-Object byte[] $Length
    $written = 0
    $counter = 0
    while ($written -lt $Length) {
        $input = [System.Text.Encoding]::UTF8.GetBytes("$Seed/$counter")
        $block = $sha.ComputeHash($input)
        $take = [Math]::Min($block.Length, $Length - $written)
        [Array]::Copy($block, 0, $out, $written, $take)
        $written += $take
        $counter++
    }
    $sha.Dispose()
    return $out
}

function New-TestFile {
    param(
        [Parameter(Mandatory)] [string] $Path,
        [Parameter(Mandatory)] [int] $Length,
        [string] $Seed
    )
    if (-not $Seed) { $Seed = Split-Path -Leaf $Path }
    [System.IO.File]::WriteAllBytes($Path, (Get-DeterministicBytes -Seed $Seed -Length $Length))
}

# --------------------------------------------------------------- the work ---

Open-Report
Report "setup starting on $(Get-Date -Format s)"

try {
    # Hibernation writes a multi gigabyte file and leaves the volume marked as
    # not cleanly dismounted when fast startup is used, which would make the
    # first restore test measure two things at once. It is turned back on in a
    # later test on purpose.
    Report 'turning hibernation off'
    & powercfg.exe /hibernate off 2>&1 | Out-Null
    & powercfg.exe /change standby-timeout-ac 0 2>&1 | Out-Null
    & powercfg.exe /change monitor-timeout-ac 0 2>&1 | Out-Null
    & powercfg.exe /change disk-timeout-ac 0 2>&1 | Out-Null

    Report 'creating the test files'
    if (Test-Path -LiteralPath $TestRoot) { Remove-Item -LiteralPath $TestRoot -Recurse -Force }
    New-Item -ItemType Directory -Path $TestRoot -Force | Out-Null
    New-Item -ItemType Directory -Path "$TestRoot\target" -Force | Out-Null

    # 1. An ordinary file, and a big one.
    New-TestFile -Path "$TestRoot\plain.bin" -Length (4 * 1024 * 1024)
    New-TestFile -Path "$TestRoot\large.bin" -Length (96 * 1024 * 1024)

    # 2. A fragmented file. Written as many small appends with another file
    #    growing between them, which is how fragmentation happens in practice.
    Report 'creating a fragmented file'
    $fragment = "$TestRoot\fragmented.bin"
    $spacer = "$TestRoot\spacer.tmp"
    $fs = [System.IO.File]::Create($fragment)
    $sp = [System.IO.File]::Create($spacer)
    try {
        for ($i = 0; $i -lt 64; $i++) {
            $block = Get-DeterministicBytes -Seed "fragmented/$i" -Length (128 * 1024)
            $fs.Write($block, 0, $block.Length); $fs.Flush()
            $filler = Get-DeterministicBytes -Seed "spacer/$i" -Length (128 * 1024)
            $sp.Write($filler, 0, $filler.Length); $sp.Flush()
        }
    } finally {
        $fs.Close(); $sp.Close()
    }
    Remove-Item -LiteralPath $spacer -Force

    # 3. A sparse file: real data at each end, a hole in the middle.
    Report 'creating a sparse file'
    $sparse = "$TestRoot\sparse.bin"
    New-Item -ItemType File -Path $sparse -Force | Out-Null
    & fsutil.exe sparse setflag $sparse 2>&1 | Out-Null
    $sfs = [System.IO.File]::Open($sparse, 'Open', 'ReadWrite')
    try {
        $head = Get-DeterministicBytes -Seed 'sparse/head' -Length 65536
        $sfs.Write($head, 0, $head.Length)
        $sfs.SetLength(64 * 1024 * 1024)
        $sfs.Seek(-65536, 'End') | Out-Null
        $tail = Get-DeterministicBytes -Seed 'sparse/tail' -Length 65536
        $sfs.Write($tail, 0, $tail.Length)
    } finally {
        $sfs.Close()
    }
    & fsutil.exe sparse setrange $sparse 65536 (63 * 1024 * 1024) 2>&1 | Out-Null

    # 4. An NTFS compressed file.
    Report 'creating a compressed file'
    $compressed = "$TestRoot\compressed.bin"
    # Repetitive on purpose, so NTFS actually compresses it.
    $pattern = [System.Text.Encoding]::ASCII.GetBytes(('MJOLNIR-COMPRESSIBLE-' * 64))
    $cfs = [System.IO.File]::Create($compressed)
    try {
        for ($i = 0; $i -lt 512; $i++) { $cfs.Write($pattern, 0, $pattern.Length) }
    } finally {
        $cfs.Close()
    }
    & compact.exe /C /A /I $compressed 2>&1 | Out-Null

    # 5. Unicode in the name and in the contents.
    Report 'creating files with unicode names'
    $unicodeName = "$TestRoot\" + [char]0x00E4 + [char]0x00F6 + [char]0x00FC + '-' + `
        [char]0x6587 + [char]0x4EF6 + '-' + [char]0x03A9 + '.txt'
    Set-Content -LiteralPath $unicodeName -Value 'MjolnirVSS unicode marker' -Encoding UTF8
    $emojiDir = "$TestRoot\" + [char]0x00E5 + [char]0x00E4 + [char]0x00F6
    New-Item -ItemType Directory -Path $emojiDir -Force | Out-Null
    New-TestFile -Path "$emojiDir\inside.bin" -Length 131072 -Seed 'unicode-dir/inside'

    # 6. An alternate data stream.
    Report 'creating an alternate data stream'
    $ads = "$TestRoot\has-streams.txt"
    Set-Content -LiteralPath $ads -Value 'the visible part' -Encoding UTF8
    Set-Content -LiteralPath "${ads}:hidden" -Value 'the alternate data stream part' -Encoding UTF8

    # 7. A hard link.
    Report 'creating a hard link'
    & cmd.exe /c mklink /H "$TestRoot\hardlink.bin" "$TestRoot\plain.bin" 2>&1 | Out-Null

    # 8. Reparse points: a junction and a symbolic link.
    Report 'creating reparse points'
    & cmd.exe /c mklink /J "$TestRoot\junction" "$TestRoot\target" 2>&1 | Out-Null
    New-TestFile -Path "$TestRoot\target\behind-the-junction.bin" -Length 65536 -Seed 'junction/behind'
    & cmd.exe /c mklink "$TestRoot\symlink.txt" "$TestRoot\has-streams.txt" 2>&1 | Out-Null

    # 9. A deep path, because path length is its own class of bug.
    $deep = $TestRoot
    foreach ($n in 1..12) { $deep = Join-Path $deep ('level-' + $n.ToString('00')) }
    New-Item -ItemType Directory -Path $deep -Force | Out-Null
    New-TestFile -Path "$deep\deep.bin" -Length 32768 -Seed 'deep/deep.bin'

    # ---- record what is there -------------------------------------------
    Report 'hashing the test files'
    $records = @()
    Get-ChildItem -LiteralPath $TestRoot -Recurse -File -Force |
        Where-Object { $_.LinkType -ne 'SymbolicLink' } |
        ForEach-Object {
            $relative = $_.FullName.Substring($TestRoot.Length + 1)
            try {
                $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
                $records += [pscustomobject]@{
                    path   = $relative
                    length = $_.Length
                    sha256 = $hash
                }
            } catch {
                Report "could not hash ${relative}: $_"
            }
        }

    # The alternate data stream has to be hashed by name, because it is not a
    # file the directory enumeration returns. Get-FileHash does not take a
    # stream path: it returns nothing and does not throw, which once left this
    # marker with an empty hash that every extracted byte then matched. The
    # bytes are read and hashed directly instead.
    try {
        $adsBytes = [byte[]] (Get-Content -LiteralPath $ads -Stream 'hidden' -Encoding Byte -ReadCount 0)
        if (-not $adsBytes -or $adsBytes.Length -eq 0) {
            throw 'the alternate data stream read back empty'
        }
        $sha = [System.Security.Cryptography.SHA256]::Create()
        try {
            $adsHash = ([System.BitConverter]::ToString($sha.ComputeHash($adsBytes)) -replace '-', '')
        } finally { $sha.Dispose() }
        $records += [pscustomobject]@{
            path   = 'has-streams.txt:hidden'
            length = $adsBytes.Length
            sha256 = $adsHash
        }
        Report "alternate data stream: $($adsBytes.Length) bytes, $adsHash"
    } catch {
        throw "could not hash the alternate data stream: $_"
    }

    $manifest = [pscustomobject]@{
        created  = (Get-Date -Format s)
        computer = $env:COMPUTERNAME
        root     = $TestRoot
        files    = $records
    }
    $json = $manifest | ConvertTo-Json -Depth 5
    Set-Content -LiteralPath "$TestRoot\markers.json" -Value $json -Encoding UTF8

    Report "created $($records.Count) test files"

    # ---- report the layout, which the restore has to reproduce ----------
    Report 'recording the disk layout'
    $layout = @()
    Get-Partition -DiskNumber 0 | ForEach-Object {
        $layout += ('  partition {0} type={1} offset={2} size={3}' -f `
                $_.PartitionNumber, $_.GptType, $_.Offset, $_.Size)
    }
    foreach ($line in $layout) { Report $line }

    $reagent = (& reagentc.exe /info 2>&1 | Out-String)
    foreach ($line in ($reagent -split "`r?`n")) {
        if ($line.Trim()) { Report "reagentc: $($line.Trim())" }
    }

    Set-Content -LiteralPath $MarkerFile -Value (Get-Date -Format s) -Encoding UTF8
    Report 'GUEST-SETUP-COMPLETE'
} catch {
    Report "GUEST-SETUP-FAILED: $_"
} finally {
    Close-Report
}
