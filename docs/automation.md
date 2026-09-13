# Driving MjolnirVSS from a script

The window is the product. This is for the other caller: a scheduled task, a
build step, a monitoring check, or a program deciding what to do next without a
person reading the screen.

Everything the window can do, these two executables can do, through the same
engine. There is no separate automation mode to fall behind.

---

## The contract

| | |
|---|---|
| **Status** | The **exit code**. Never parse prose to find out whether something worked |
| **Output** | With `--json`, one JSON document on **standard output**. Without it, prose |
| **Failures** | With `--json`, also a JSON document on standard output. Without it, prose on standard error |
| **Progress** | Standard error, and never JSON. `--quiet` turns it off, and `--json` implies it |

Standard output is the machine's channel and standard error is the person's.
A caller can redirect one and ignore the other.

```powershell
$result = MjolnirVSS.exe --json list E:\Backups | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw $result.error.what }
```

That works whether the command succeeded or failed, which is the point: before
this existed, a failure under `--json` printed nothing to standard output at
all, and a script had to fall back to reading English.

---

## Exit codes

Stable. A number is never reused for a different meaning, and `exit_name` is
the same string in every JSON failure document.

| Code | Name | Means |
|---|---|---|
| 0 | `success` | It worked |
| 1 | `failure` | Something went wrong that has no more specific code |
| 2 | `usage` | The command line was wrong. Nothing was attempted |
| 3 | `access-denied` | Needs an elevated prompt, or the file is not readable |
| 4 | `unsupported` | This computer or this disk is not something MjolnirVSS handles |
| 5 | `vss-failure` | The Volume Shadow Copy Service refused or failed |
| 6 | `corrupt-backup` | The backup is damaged, incomplete, or not a backup |
| 7 | `destination` | The place being written to is wrong: the source disk, or too small |
| 8 | `unsafe-target` | The disk about to be erased is not one it is safe to erase |
| 9 | `cancelled` | Stopped on request. Nothing was left half written |
| 10 | `io` | A read or a write failed. The message names the file |

`crates/mjolnir-core/src/exit.rs` holds the list, and a test fails if a code is
added without a unique name.

### A failure document

```json
{
  "error": {
    "what": "C:\\Backups\\manifest.json is missing",
    "why": "a MjolnirVSS backup folder must contain manifest.json and disk-layout.json; without them there is no way to know what the chunks mean",
    "next_step": "check that the whole backup folder was copied, and that you selected the backup folder itself rather than the drive",
    "exit_code": 6,
    "exit_name": "corrupt-backup"
  }
}
```

Three fields, always all three. `what` happened, `why` it matters, and the
`next_step` that would fix it. They are written to be shown to somebody, so a
program that has nothing better to do with a failure can print `next_step` and
be more useful than most.

---

## Nothing blocks waiting for a person

Every command completes without input, or refuses and says which flag to pass.
Nothing waits at a prompt that a script cannot answer.

| Needs | Flag | Without it |
|---|---|---|
| The password for an encrypted backup | `--password-file <file>` or `MJOLNIRVSS_PASSWORD` | Asked at the console, when there is one |
| Confirming a disk will be erased | `--confirm "ERASE <serial>"` | Asked at the console; **refused** with no terminal, rather than hanging |

The password is **never** accepted as an argument. Arguments are visible to
anything that can list processes, which would make it a worse secret than no
secret. See [encryption.md](encryption.md).

The erase confirmation is deliberately awkward. `MjolnirVSS.Restore.exe
list-disks --json` reports the phrase each disk requires, so a script that
means it can find it, and a script that does not mean it cannot stumble into
it.

---

## Stopping one

Ctrl+C sets a flag. The copy loop finishes the block it is on, releases the
temporary shadow copy, and exits `9 cancelled`. That takes up to one block,
so it is not instant.

A second Ctrl+C ends the process immediately, leaving the shadow copy for
Windows to clean up. The message says so before you press it.

---

## The commands

Every one of these accepts `--json`.

### `MjolnirVSS.exe`

| | |
|---|---|
| `inspect` | What is on this computer and what would be backed up |
| `backup --destination <dir>` | Back up the Windows system disk. `--encrypt`, `--name <name>`, `--preview <bytes>` |
| `verify <backup>` | Decompress every block and compare its digest |
| `list <dir>` | The backups in a folder, with whether each is encrypted |
| `volumes <backup>` | The partitions inside a backup that can be browsed |
| `browse <backup> --volume <id> --folder <path>` | What is in a folder inside a backup |
| `extract <backup> --volume <id> --item <path> --into <dir>` | Copy files out. `--overwrite`, `--follow-links` |
| `recovery-sources` | What recovery media could be built from |
| `recovery-media` | Build it |
| `cleanup-snapshots` | The shadow copies on this computer |
| `diagnose-bitlocker` | How BitLocker and the shadow copy service behave here |

### `MjolnirVSS.Restore.exe`

Runs in Windows PE. Does not link `vssapi.dll`, which is not there.

| | |
|---|---|
| `find-backups` | Search attached drives for backups |
| `inspect-backup <backup>` | What it contains and whether it can be restored |
| `list-disks` | Disks that could be restored onto, with the phrase each requires |
| `restore <backup> --target <n> --confirm "<phrase>"` | **Erases that disk** and writes the backup. `--dry-run` plans without writing |
| `repair-boot --disk <n>` | Rebuild boot configuration. `--dry-run` reports without writing |

---

## A scheduled backup, whole

```powershell
$log = "C:\Logs\backup-$(Get-Date -Format yyyy-MM-dd).json"
$out = & MjolnirVSS.exe --json --quiet backup `
    --destination E:\Backups `
    --encrypt --password-file C:\keys\backup.txt
$code = $LASTEXITCODE
$out | Set-Content $log

switch ($code) {
    0 { }                                          # done
    9 { Write-Warning 'cancelled' }                 # somebody stopped it
    7 { throw "destination: $(($out|ConvertFrom-Json).error.what)" }
    default { throw "MjolnirVSS exit $code" }
}
```

Needs an elevated prompt: reading a shadow copy requires it, and without one
the answer is `3 access-denied` before anything is attempted.

---

## What is not promised

This is `0.1.0-alpha.1`.

| | |
|---|---|
| **Exit codes** | Stable. Treated as a contract from here on |
| **Failure document shape** | Stable. Fields may be added, never removed or repurposed |
| **Success document shapes** | **Not stable yet.** Field names may change before 1.0 |
| **The backup format** | Versioned in the manifest, and a reader refuses what it does not understand |

If a script depends on a field of a success document, pin the version it was
written against.
