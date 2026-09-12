# Testing

## Running the tests

```powershell
cargo test --workspace
```

334 tests, no administrator rights, no real disks, no network. Everything that
can be tested without hardware is tested this way, including the entire restore
path, which runs against temporary files.

---

## The opt in tests

Two kinds of test are skipped by default because they reach outside the process.

### Live shadow copy

Needs administrator rights. Creates a real shadow copy of this machine's Windows
volume, reads from it, and removes it. Writes nothing to any disk and never
touches a shadow copy it did not create.

```powershell
$env:MJOLNIR_VSS_LIVE = "1"
cargo test -p mjolnir-vss --test live_snapshot -- --nocapture --test-threads=1
```

Afterwards, confirm nothing was left behind:

```powershell
vssadmin list shadows
```

The count should be the same as before the run.

### Physical disk restore

**There is no automated test that writes to a physical disk, and there will not
be one.** A test that could erase the machine it runs on does not belong in a
test suite. Restores are tested against temporary files and, for the acceptance
gate, against virtual machines driven by hand.

---

## What is covered

| Layer | How |
|---|---|
| Checked arithmetic | Property tests comparing every operation against 128 bit arithmetic |
| Extent algebra | Property tests: merging preserves coverage exactly, splitting never leaves its range, cut points are stable, alignment only grows |
| GPT parsing | Synthetic disks with correct checksums; damaged headers, damaged entry arrays, absurd entry counts, truncated reads, 512 and 4096 byte sectors |
| Identifiers and paths | Path traversal, Windows reserved device names, trailing dots, drive letters, case |
| Format documents | Schema validation, version handling, forward compatibility, cross document agreement |
| Block store | Round trip, deduplication, corruption, truncation, decompression bounds, temporary file cleanup |
| Capture | Shared with the Windows engine, exercised against synthetic disks |
| Restore | Round trip against virtual disks, byte compared |
| Safety refusals | Every one has a test that tries to do the forbidden thing |
| Windows discovery | Runs against whatever the machine actually has, asserting only invariants |
| Shadow copy | Opt in, against the real service |

### The test that matters most

`tests/integration/tests/roundtrip.rs` builds a synthetic GPT disk, backs it up
through the same capture code the Windows engine uses, verifies the result,
restores it onto a blank virtual disk, and compares every partition byte for
byte against what it came from. It also checks that every GUID, offset, size,
attribute and partition name survived, and that restoring onto a larger disk puts
the secondary partition table at the new end.

### The tests that prove the refusals

`tests/integration/tests/safety.rs` damages backups the way a failing drive does
and checks each one is caught: a flipped bit in a block, a truncated block, a
missing block, an edited manifest, a missing completion marker. It also checks
that a damaged backup stops a restore **before anything is written to the target
disk**, and that the target is still blank afterwards.

---

## The manual test matrix

These need real machines or virtual machines and have to be done by hand. This
is the honest state of each.

| # | Test | State |
|---|---|---|
| 1 | Backup on Windows 11 x64 | Discovery, planning and the shadow copy path proven on a BitLocker machine; a full live backup has not yet been run end to end |
| 2 | Backup on Windows 10 x64 | Not done |
| 3 | Backup with files changing during the run | Not done |
| 4 | Common GPT layout: EFI, MSR, Windows, recovery | Proven against synthetic disks; not against a real machine end to end |
| 5 | 512e disk | Discovery proven on real hardware (Kingston KC2500, 512 logical / 4096 physical) |
| 6 | 4Kn disk | Unit tested only; no hardware |
| 7 | Destination disconnected during a backup | Not done |
| 8 | Cancel during each stage | Cancellation is unit tested; not exercised against a real backup |
| 9 | Corrupted blocks rejected | **Done**, automated |
| 10 | Missing metadata rejected | **Done**, automated |
| 11 | Restore onto a blank virtual disk | **Done**, automated against file backed disks |
| 12 | Restore onto a larger disk | **Done**, automated |
| 13 | **Restored Windows boots** | **Not done. This is the gate.** |
| 14 | Windows Recovery Environment works after a restore | Not done |
| 15 | Extracting single files | Not implemented |
| 16 | Recovery media boots | Not implemented |
| 17 | Recovery application starts in Windows PE | **Not done.** Its imports have been checked and contain nothing Windows PE lacks, which is evidence, not proof |
| 18 | Keyboard only operation | Built for, not verified by a person |
| 19 | High DPI scaling | Built for, not verified by a person |

---

## How to run the acceptance test

This is the one that decides whether MjolnirVSS works. It has not been run.

**What you need:** a virtualisation product that can boot UEFI virtual machines
(Hyper-V, VMware Workstation or VirtualBox), about 100 GB of free space, and
Windows installation media.

1. **Build a subject.** Create a UEFI virtual machine with a 64 GB disk and
   install Windows 11. Let Setup create the partitions, so the layout is the one
   Windows actually produces. Turn off BitLocker if it is on.
2. **Put something recognisable on it.** A few files with known contents in the
   user profile, so the restored machine can be checked rather than merely
   observed to start.
3. **Attach a second virtual disk** as the backup destination and format it NTFS.
4. **Take a backup.** Copy the MjolnirVSS folder into the virtual machine, run
   `MjolnirVSS.exe`, and back up to the second disk.
5. **Verify it** with `MjolnirVSS.exe verify <folder>`.
6. **Create a third virtual disk**, blank, at least as large as the first.
7. **Shut the virtual machine down. Detach the original disk.** Do not delete it;
   if the restore fails you want to compare.
8. **Boot Windows installation media** in the same virtual machine, press
   Shift+F10, and run `MjolnirVSS.Restore.exe` from the backup drive.
9. **Restore onto the blank disk.**
10. **Remove the media and start the virtual machine.**

**Record what happens, including if it fails.** The result belongs in this
document and in the README, whichever way it goes.

If Windows starts: check the files from step 2, check Disk Management shows the
expected layout, and check the recovery environment still works
(`reagentc /info`).

---

## Test data

No test uses real personal data. Synthetic disks are filled with recognisable
patterns generated in code. Nothing in the repository contains a serial number,
a machine identifier or a path from a real computer.

## Continuous integration

There is none yet, and adding it needs the repository owner's agreement. A
workflow would run `cargo build --workspace`, `cargo test --workspace`,
`cargo clippy` and `cargo fmt --check` on a Windows runner. It must never run the
shadow copy tests, which need administrator rights, and it must never run
anything that writes to a disk.
