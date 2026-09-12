# Architecture

## The shape of it

```text
apps/mjolnirvss              MjolnirVSS.exe          window + command line
apps/mjolnirvss-restore      MjolnirVSS.Restore.exe  recovery wizard + command line

crates/mjolnir-win32-ui      Win32 toolkit: windows, controls, fonts, workers
crates/mjolnir-cli           the backup tool's command line

crates/mjolnir-backup        planning and running a backup
crates/mjolnir-restore       planning and running a restore   (writes to disks)

crates/mjolnir-vss           IVssBackupComponents             (backup only)
crates/mjolnir-storage       disks, volumes, GPT, reading devices
crates/mjolnir-ntfs          NTFS structures                  (for file recovery)

crates/mjolnir-image         the backup format: documents, blocks, verification
crates/mjolnir-core          errors, ids, checked arithmetic, extents, block I/O

crates/mjolnir-testkit       synthetic disks, file backed devices, corruption
tests/integration            round trip and safety tests spanning crates
```

Dependencies point downwards only. `mjolnir-core` depends on nothing of ours.

---

## Four decisions that shape everything else

### 1. The shadow copy code is a separate crate, and the restore application cannot reach it

`vssapi.dll` is not part of a base Windows PE image. If `MjolnirVSS.Restore.exe`
imported it, the recovery application would fail to start at exactly the moment
somebody needed it — and the failure would only ever show up on the broken
machine, never during development.

Making that a matter of discipline was not good enough, so it is a matter of the
dependency graph: `mjolnir-vss` is its own crate, only `mjolnir-backup` depends
on it, and the recovery application depends on neither. `scripts/package.ps1`
checks the built binary's imports and fails the build if `vssapi.dll` appears.

This is also why `mjolnir-win32-ui` is a toolkit that knows nothing about
backups. An earlier version had the backup window inside it, which quietly
dragged the shadow copy code into the recovery binary through the user interface.

### 2. Everything that touches bytes is written against a trait

The backup engine reads through `BlockSource` and the restore engine writes
through `BlockSink`. Neither knows whether it is talking to a physical disk, a
shadow copy device, or a file in a temporary folder.

That is what makes the dangerous half of this product testable. A restore that
would destroy a computer is run against a temporary file on every `cargo test`,
and its output is compared byte for byte against what it should have produced.
Without the seam, the only way to test a restore would be to risk hardware, and
in practice that means it would not be tested.

`mjolnir-backup::capture` exists for the same reason: the loop that decides what
to read and where to record it is shared by the Windows engine and the tests, so
they cannot drift apart.

### 3. The engines know nothing about the interface

`mjolnir_backup::run` takes a plan, a `&mut dyn Progress` and a `&CancelToken`.
That is the entire contract. The window passes a progress sink that writes into
a shared record and a token wired to the Cancel button; the command line passes
one that writes to stderr and a token wired to Ctrl+C. Neither the window nor
the command line contains any backup logic, so there is no second implementation
to keep in step.

Long work runs on a worker thread. The window reads the shared record on a
200 ms timer and repaints. That is the whole of the concurrency design, and it
is why the window never stops responding.

### 4. Safety properties are types, not rules

Where a mistake would be expensive, the compiler enforces the rule:

- `mjolnir_restore::restore` cannot be called without an `EraseConfirmation`,
  and the only way to get one is to pass the exact phrase for a specific disk to
  `EraseConfirmation::check`. A confirmation for disk 1 does not authorise
  erasing disk 2, and it stops matching if the disk's serial changes underneath
  it.
- `BackupWriter::finalize` hands back a `FinalizedBackup`, and only that type can
  be marked complete. There is no path from "wrote some blocks" to "this is a
  usable backup" that skips verification.
- `CaptureMethod::Preview` makes a truncated capture un-restorable by
  construction, rather than by a check somebody has to remember to write.

---

## How a backup runs

1. **Plan.** Find the system disk from the Windows directory, read its layout,
   map volumes to partitions, classify each partition, and refuse anything
   unsupported. Nothing has been written and no shadow copy has been taken, so
   an unsupported machine finds out in the first second.
2. **Create the folder.** A failure to write is found before a shadow copy
   exists.
3. **Snapshot.** One coordinated shadow copy of every NTFS volume on the disk, so
   everything in the backup comes from the same instant. If a writer fails, the
   backup stops.
4. **Copy.** NTFS partitions are read from the shadow copy; the EFI and reserved
   partitions are read from the disk, along with both copies of the partition
   table.
5. **Release.** Tell the writers the backup is finished and delete the shadow
   copy. This also happens on every failure path, because the session owns the
   snapshot and its destructor releases it.
6. **Write the documents**, then **verify every stored block**, then **write the
   completion marker**. In that order, always.

## How a restore runs

1. **Open and check the backup.** Refused unless it is complete and verified.
2. **Check the target**: not the disk holding the backup, large enough, matching
   sector size.
3. **Read every block the restore will need**, decompressing and checking each
   one. A damaged backup stops the operation while the replacement disk is still
   untouched.
4. **Write the partitions.**
5. **Write the partition table last**, rebuilt for the target's geometry. A disk
   whose contents are written but whose table is missing is visibly unfinished; a
   disk whose table points at partitions that were never written looks fine and
   is not.
6. **Flush**, then ask Windows to re read the layout.

---

## Errors

Every error carries three things, because the person reading it is often halfway
through a recovery:

- **what** failed,
- **why** that matters,
- **what to do next**.

`Error::new` takes all three; there is no constructor that takes only a message.
An `HRESULT` on its own never reaches the screen — the shadow copy failures that
actually happen are translated into sentences like "another program is creating
a shadow copy right now, and Windows allows only one at a time".

Exit codes are stable and documented in `mjolnir_core::exit`.

---

## Unsafe code

Confined to the crates that must talk to Windows: `mjolnir-storage`,
`mjolnir-vss`, `mjolnir-win32-ui`, `mjolnir-restore::windows_target`, and the
application binaries. `mjolnir-core`, `mjolnir-image` and `mjolnir-testkit` are
`#![forbid(unsafe_code)]`.

Every `unsafe` block carries a comment saying why it is sound.

The largest piece of unsafe code is the hand written `IVssBackupComponents`
binding in `mjolnir-vss/src/sys.rs`. The `windows` crate ships the shadow copy
*types* but not that interface, because Microsoft's Win32 metadata does not
describe `vsbackup.h`. Rather than add a C++ bridge, the interface is declared
directly: it is an ordinary COM interface, so its layout is the three `IUnknown`
slots followed by its own methods in declaration order. The order was taken from
the Windows SDK header, is reproduced in a table beside the struct so a reviewer
can check it without reading the code, and is asserted by tests. Slots MjolnirVSS
does not call are typed as opaque pointers, so calling one by mistake does not
compile.
