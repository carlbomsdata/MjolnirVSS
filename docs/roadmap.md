# Roadmap

In order. Nothing below a line starts before everything above it works, because
a backup tool that does many things badly is worse than one that does one thing
reliably.

---

## Done

- The backup format: documents, content addressed blocks, atomic completion,
  strict verification.
- Windows discovery: disks, partitions, volumes, the system disk, GPT parsing.
- Shadow copy orchestration, proven against a real machine, including cleanup
  that never touches a snapshot MjolnirVSS did not create.
- Backup of a whole system disk, with every partition and both copies of the
  partition table.
- Verification that decompresses and checksums every stored block, tested
  against deliberately damaged backups.
- Restore onto a blank disk, including rebuilding the partition table for the
  target's geometry, tested against virtual disks with byte for byte comparison.
- The safety refusals, each with a test that tries to do the forbidden thing.
- The backup window and the recovery wizard.

---

## Next, in order

### 1. Boot a restored Windows

**The gate.** Everything below is worthless until this passes, and nothing below
should be started before it does.

Back up a real Windows installation in a virtual machine, restore it onto a blank
virtual disk, start it, and see what happens. The procedure is written out in
[`testing.md`](testing.md). The result goes in the README whichever way it goes.

Expected to need work on the UEFI boot configuration, which is item 2.

### 2. Repair the boot configuration after a restore

Today, if the restored machine does not start, the operator runs Startup Repair
by hand. MjolnirVSS should do it: recreate the boot entries on the restored EFI
partition using documented Windows mechanisms, and check the result before
saying the restore succeeded.

### 3. Run the recovery application in Windows PE

Its imports contain nothing Windows PE lacks, which is evidence, not proof. It
has to be booted and run.

### 4. Used block imaging

Today a backup copies every byte of a volume, including free space. Reading the
NTFS allocation bitmap through the shadow copy would skip the free space, making
backups substantially smaller and faster.

The format already supports it, the verifier already honours the resulting gaps,
and the restore path already handles a sparse stream. What is missing is the NTFS
reader, which is shared with item 5.

### 5. File recovery

Browse a backup's volumes read only and extract files. Needs the same NTFS reader
as item 4, so the two arrive together. Designed in
[`file-recovery.md`](file-recovery.md).

### 6. Recovery media creation

Build a bootable USB from the recovery image already on the machine. ISO output
only when the Windows ADK is present, because `oscdimg.exe` cannot be
redistributed. Designed in [`recovery-media.md`](recovery-media.md).

### 7. Controlled hardware validation

A restore onto a real replacement disk in a real machine, done deliberately, with
the result recorded.

---

## After that

- **Encryption.** A real gap, deliberately behind the restore path: an encrypted
  backup that cannot be restored is worse than an unencrypted one that can.
- **Incremental backups.** The format was built for this. Blocks are content
  addressed and cut on stable boundaries, so an unchanged region produces the
  same block, and `chunk_store.root` is a field so several backups of a machine
  can share one store. Needs item 4 first.
- **BitLocker.** Only after its backup and restore behaviour has been designed,
  implemented and tested, and only if the restored machine still boots. Until
  then it is detected and refused.
- **Retention**, so old backups can be removed without breaking a chain.
- **Scheduling** inside the application, without registering anything permanent.
- **Restoring onto a smaller disk**, by shrinking the Windows partition.

---

## Deliberately not planned

- **A kernel driver or filesystem driver.** MjolnirVSS installs nothing, and that
  is a feature rather than a limitation to be worked around.
- **Persistent changed block tracking**, which needs a driver.
- **Mounting a backup as a drive letter**, same reason. A read only browser and
  an extract button covers what people need.
- **Network or cloud destinations.** A backup you cannot restore without the
  internet is not a bare metal backup.
- **Telemetry, analytics, accounts or update checks.** MjolnirVSS makes no
  network connections at all.
- **Restoring onto dissimilar hardware**, which means driver injection and a
  different product.
- **A custom bootable ISO**, which would mean redistributing Microsoft files.
