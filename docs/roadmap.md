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
- BitLocker: an unlocked volume is captured through its shadow copy, measured
  rather than assumed, with a locked one refused. The state comes from
  `Win32_EncryptableVolume`, the documented interface, corroborated by the
  partition header.
- Used block imaging: the NTFS allocation bitmap is read from the shadow copy
  and only the clusters in use are captured, with a recorded fallback to copying
  the whole volume where that cannot be established.
- A warning before a backup when taking a shadow copy may cost the machine its
  restore points, which was measured rather than anticipated.
- Optional encryption of a backup's contents, using Argon2id and AES-256-GCM
  through their own crates, with the password never stored and never accepted as
  a command line argument.
- The backup window and the recovery wizard, the latter run in real Windows PE
  and driven through a whole restore from the keyboard.
- File recovery: browsing a backup's NTFS volumes read only and copying files
  out, proven against a real Windows volume with every file checked by hash.
- Recovery media built from the Windows parts already on the machine, booted on
  UEFI firmware.
- Boot repair, proven by needing it: a restored disk was deliberately left
  unbootable, and the repair is what made it start again.

---

## The gate, which has been passed once

**A restored Windows has been booted.** In a disposable virtual machine on
13 September 2026: a live backup of a running Windows 11, recovery media built
by MjolnirVSS from this computer's own Windows parts, the backup restored onto a
blank disk from that media with the keyboard alone, and the machine started
unaided. Twelve of twelve test files matched their hashes, every partition kept
its identifier, offset and size, and the recovery environment was still
registered. The run is described in [`vm-testing.md`](vm-testing.md).

Once, in a virtual machine, is not the same as reliably, on hardware. What is
still missing is below.

---

## Next, in order

### 1. The same thing again, and differently

Restoring onto a larger disk has been done, and booted. What is left is a target
whose sector size differs from the source's, and a backup taken while the
machine is genuinely busy rather than idle. A restore that has been done twice
on one machine may still have been lucky.

### 2. Encryption where people can reach it

The engine is built and tested: Argon2id, AES-256-GCM, keyed block names, the
whole cycle proven against synthetic disks, described in
[`encryption.md`](encryption.md). What is left is the window, which does not
offer it, and an encrypted backup of a real machine restored and started.

### 3. Controlled hardware validation

Everything above happens in virtual machines. Real firmware, real disks, real
failures.

### 4. Recovery media on a USB stick

ISO output works and has been booted. Writing a USB stick erases it, so it needs
the same confirmation a restore does, and it is behind the work above. Designed
in [`recovery-media.md`](recovery-media.md).

---

## After that

- **Incremental backups.** The format was built for this. Blocks are content
  addressed and cut on stable boundaries, so an unchanged region produces the
  same block, and `chunk_store.root` is a field so several backups of a machine
  can share one store.
- **Preserving BitLocker across a restore.** Backing up an unlocked BitLocker
  volume works today, but the restored disk comes back unencrypted. Putting the
  encryption back automatically is a separate piece of work, and is behind the
  boot test like everything else.
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
