<#
.SYNOPSIS
    Interrupts a running backup with Ctrl+C and checks what it left behind.

.DESCRIPTION
    Runs inside MjolnirVSS-Test-Source, from an elevated prompt.

    A backup holds a shadow copy of the system volume. Killing the process
    outright leaves that shadow copy on the volume until the service times it
    out, which is the failure the cancellation flag exists to prevent. The flag
    was documented in three places and set by nothing, so Ctrl+C went to the
    default handler and ended the process where it stood. This is the test that
    would have caught that.

    It starts a real backup, lets it get properly under way, sends a real
    Ctrl+C, and then checks three things:

      * the process exited 9 (cancelled) rather than being killed,
      * no shadow copy was left behind,
      * the abandoned backup folder is not marked complete.

    The Ctrl+C is a real one, raised through the console API rather than
    simulated, because a signal sent any other way would not be the thing a
    person presses. It is raised by a separate throwaway process: the event
    reaches everything on the target's console, so whatever sends it tends not
    to survive, and this script has to.

    It writes only to the second disk. The Windows disk is read, never written.

    Copyright (C) the MjolnirVSS contributors.
    Licensed under the GNU General Public License, version 3 or later.
#>

$ErrorActionPreference = 'Continue'

$BackupLetter = 'M'

# How long to let the backup run before interrupting it. Long enough that the
# shadow copy exists and the copy loop is reading, which is the state the test
# is about; short enough not to add minutes to a run.
$RunSeconds = 75

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
    $line = "[cancel] $Message"
    Write-Host $line
    if ($script:Serial) { try { $script:Serial.WriteLine($line) } catch { } }
    try { Add-Content -LiteralPath 'C:\mjolnir-phase.log' -Value $line -Encoding UTF8 } catch { }
}

# ------------------------------------------------------- sending a Ctrl+C ---

# Raising a real Ctrl+C in another process means attaching to its console, and
# GenerateConsoleCtrlEvent then reaches every process on that console. The first
# version of this did the attaching from here and took itself out with it: the
# attach needs FreeConsole first, so this script lost its own console, and the
# event reached it anyway. No report was written even though the product had
# behaved perfectly - both shadow copies released, nothing left behind.
#
# So the signalling is done by a separate, disposable process. It may well die
# of what it raises. This script keeps its console, keeps reporting, and is
# still there afterwards to check what happened.
function Send-CtrlC {
    param([int] $ProcessId)

    $helper = @"
Add-Type -Namespace H -Name C -MemberDefinition @'
[DllImport("kernel32.dll")] public static extern bool AttachConsole(uint p);
[DllImport("kernel32.dll")] public static extern bool FreeConsole();
[DllImport("kernel32.dll")] public static extern bool GenerateConsoleCtrlEvent(uint e, uint g);
[DllImport("kernel32.dll")] public static extern bool SetConsoleCtrlHandler(IntPtr h, bool a);
'@
[void][H.C]::FreeConsole()
if (-not [H.C]::AttachConsole($ProcessId)) { exit 2 }
[void][H.C]::SetConsoleCtrlHandler([IntPtr]::Zero, `$true)
if (-not [H.C]::GenerateConsoleCtrlEvent(0, 0)) { exit 3 }
Start-Sleep -Milliseconds 800
exit 0
"@

    $script = Join-Path $env:TEMP 'mjolnir-send-ctrlc.ps1'
    Set-Content -LiteralPath $script -Value $helper -Encoding UTF8
    $sender = Start-Process -FilePath 'powershell.exe' `
        -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-WindowStyle', 'Hidden', '-File', $script) `
        -PassThru -WindowStyle Hidden
    # It is expected to be killed by the event it raises, so its exit code says
    # nothing useful. What the event did is measured on the target instead.
    $null = $sender.WaitForExit(20000)
    Remove-Item -LiteralPath $script -Force -ErrorAction SilentlyContinue
}

function Get-MjolnirShadowCount {
    # Every shadow copy on the machine. The test machine takes none of its own,
    # so any that appears is the backup's.
    $text = & vssadmin.exe list shadows 2>&1 | Out-String
    ([regex]::Matches($text, 'Shadow Copy ID')).Count
}

# ------------------------------------------------------------------ the run ---

Open-Report
Report "starting on $(Get-Date -Format s)"

try {
    $exe = $null
    foreach ($drive in [char[]](68..90)) {
        $candidate = "${drive}:\MjolnirVSS\MjolnirVSS.exe"
        if (Test-Path -LiteralPath $candidate) { $exe = $candidate; break }
    }
    if (-not $exe) { throw 'MjolnirVSS.exe was not found on any drive' }

    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'this script has to run from an elevated prompt'
    }

    $root = "${BackupLetter}:\"
    if (-not (Test-Path $root)) { throw "the backup destination $root is not there" }
    $destination = Join-Path $root 'Backups-cancel'
    if (Test-Path $destination) { Remove-Item $destination -Recurse -Force }
    New-Item -ItemType Directory -Path $destination -Force | Out-Null

    $before = Get-MjolnirShadowCount
    Report "shadow copies before: $before"

    # Its own window, so it has a console of its own to attach to.
    Report 'starting a backup to interrupt'
    $process = Start-Process -FilePath $exe `
        -ArgumentList @('backup', '--destination', $destination) `
        -PassThru
    Report "backup is process $($process.Id)"

    Start-Sleep -Seconds $RunSeconds
    if ($process.HasExited) {
        throw "the backup finished in under $RunSeconds seconds, so there was nothing to interrupt"
    }

    $during = Get-MjolnirShadowCount
    Report "shadow copies while it runs: $during"
    if ($during -le $before) {
        throw 'the backup was running but had taken no shadow copy, so this would prove nothing'
    }

    Report 'sending Ctrl+C'
    Send-CtrlC -ProcessId $process.Id

    # One block of work is the documented worst case. Sixty seconds is many.
    if (-not $process.WaitForExit(60000)) {
        throw 'the backup did not stop within 60 seconds of Ctrl+C'
    }
    $code = $process.ExitCode
    Report "exit code after Ctrl+C: $code"
    if ($code -ne 9) {
        throw "expected exit code 9 (cancelled) but got $code"
    }

    # The point of all of it: the shadow copy has to be gone.
    $after = Get-MjolnirShadowCount
    Report "shadow copies after: $after"
    if ($after -ne $before) {
        throw "a shadow copy was left behind: $before before, $after after"
    }

    # And nothing half written may look usable.
    foreach ($folder in Get-ChildItem $destination -Directory -ErrorAction SilentlyContinue) {
        if (Test-Path (Join-Path $folder.FullName 'completion.json')) {
            throw "the interrupted backup $($folder.Name) is marked complete, which it must never be"
        }
        Report "the interrupted backup $($folder.Name) is not marked complete, as it must not be"
    }

    Remove-Item $destination -Recurse -Force
    Report 'PHASE-CANCEL-COMPLETE'
} catch {
    Report "PHASE-CANCEL-FAILED: $_"
} finally {
    if ($script:Serial) { try { $script:Serial.Close() } catch { } }
}
