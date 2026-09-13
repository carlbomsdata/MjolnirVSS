# Testing

## Running the tests

```powershell
cargo test --workspace
```

677 tests, no administrator rights, no real disks, no network. Everything that
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

No shadow copy created by the run should remain.

The count may nonetheless be **lower** than before, and that is not a leak in
the other direction: releasing a shadow copy can make the volume snapshot driver
delete older ones of the same volume, which it logs as Volsnap event 95 in the
System log. This is measured and explained in
[`vss-lifecycle.md`](vss-lifecycle.md). On a machine whose shadow copies matter,
run the opt in tests knowing that.

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
| Used block imaging | Bitmap decoding, pagination, geometry refusals; and end to end capture, verify and restore against synthetic NTFS volumes whose free space holds garbage, so a capture that read it would be caught |
| Restore point preflight | The whole decision table, and the wording of the warning |
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
missing block, an edited manifest, an edited disk layout, a missing completion
marker. It also checks that a damaged backup stops a restore **before anything is
written to the target disk**, and that the target is still blank afterwards.

It covers the ways a run is interrupted, too. A destination that cannot be
written to fails the backup and leaves nothing that opens as usable. A backup, a
verification and a restore are each cancelled **part way through**, after real
work has been done, rather than before they start: cancelling at the door only
proves the check at the door.

---

## The manual test matrix

These need real machines or virtual machines and have to be done by hand. This
is the honest state of each.

| # | Test | State |
|---|---|---|
| 1 | Backup on Windows 11 x64 | **Done.** Live backup of a running Windows 11, in a virtual machine, verified |
| 1b | BitLocker state read through the documented API | **Done.** `Win32_EncryptableVolume` queried on a real encrypted machine, agreeing with `manage-bde` |
| 1c | Used block imaging against a real NTFS volume | **Done.** 3,722,337 of 16,460,799 clusters on a 62.8 GiB Windows volume, and the result restored and booted |
| 1d | Restore point preflight on a real machine | **Done.** `MjolnirVSS.exe inspect` reported the machine's shadow copy storage and restore point count correctly |
| 1e | Backup of a BitLocker machine | **Done.** Fully encrypted, unlocked, backed up with used block imaging working through the shadow copy |
| 2 | Backup on Windows 10 x64 | Not done |
| 3 | Backup with files changing during the run | Not done. The machine was idle both times |
| 4 | Common GPT layout: EFI, MSR, Windows, recovery | **Done.** The layout Windows Setup produced, captured and restored, checked partition by partition |
| 5 | 512e disk | Discovery proven on real hardware (Kingston KC2500, 512 logical / 4096 physical) |
| 6 | 4Kn disk | Unit tested only; no hardware |
| 7 | Destination disconnected during a backup | **Partly.** Automated against a destination that cannot be written to; not against a drive physically pulled out |
| 8 | Cancel during each stage | **Done**, automated, cancelling part way through a backup, a verification and a restore rather than before they start |
| 9 | Corrupted blocks rejected | **Done**, automated, and again by hand against a real backup |
| 10 | Missing metadata rejected | **Done**, automated |
| 11 | Restore onto a blank virtual disk | **Done.** From real recovery media, onto a blank disk, and it booted |
| 12 | Restore onto a larger disk | **Done.** 64 GiB backup onto a 96 GiB disk, 32 GiB left unallocated, and it booted |
| 13 | **Restored Windows boots** | **Done.** Unaided, with no repair needed |
| 14 | Windows Recovery Environment works after a restore | **Partly.** `reagentc /info` on the restored machine reports it enabled and pointing at partition 4. It has not been started |
| 15 | Extracting single files | **Done.** 12 files out of a real backup, every one checked against the hash taken when it was made |
| 16 | Recovery media boots | **Done.** On UEFI firmware |
| 17 | Recovery application starts in Windows PE | **Done.** It draws its window and runs a whole restore |
| 18 | Keyboard only operation | **Done** for the recovery wizard, in Windows PE, with no mouse at any point. The backup window has not been driven this way |
| 19 | High DPI scaling | Built for and unit tested at 1x, 2x and 3x; not looked at by a person on a high DPI screen |
| 20 | Boot repair when it is needed | **Done.** The boot files were deleted from a restored disk, the machine then failed to start, and the repair is what made it start again |

All of the above marked done were done in disposable virtual machines. **None of
it was done on real hardware.** The run is described in
[`vm-testing.md`](vm-testing.md).

---

## How to run the acceptance test

This is the one that decides whether MjolnirVSS works. **It has been run once,
in virtual machines**, on 13 September 2026; what happened is in
[`vm-testing.md`](vm-testing.md). It is written out here so it can be run again,
by somebody else, on different hardware, which is the only thing that would turn
one result into a reason to trust this.

Most of it is now automated by the harness in `tests/vm`, which builds the
machines, drives them without VMware Tools and checks the result. Doing it by
hand is still worth it once, because watching a machine you did not script
start up is different evidence from reading that a script says it did.

**What you need:** a virtualisation product that can boot UEFI virtual machines
(Hyper-V, VMware Workstation or VirtualBox), about 200 GB of free space, and
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
