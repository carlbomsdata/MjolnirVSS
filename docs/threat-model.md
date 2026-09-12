# Threat model

What MjolnirVSS treats as hostile, what it defends against, and — more
importantly — what it does not.

---

## The position it is in

MjolnirVSS runs with administrator rights on a machine the operator controls, and
writes to a drive they chose. It is not a security boundary between users. The
interesting risks are elsewhere:

- A **backup is read on a different machine**, often years later, often in a
  recovery environment, and it is about to be turned into writes against a
  physical disk. Its contents are input from an untrusted source.
- A **disk is about to be erased**, and getting that wrong is unrecoverable.
- **Backups contain everything**, including credentials, keys and personal data,
  and they sit on a drive that is easy to lose.

---

## Untrusted input

Everything read from a backup folder or from a disk is treated as hostile, even
though it usually came from the operator's own machine. A drive that has been in
a drawer for two years, a folder somebody edited, a partition table on a failing
disk: all of these produce numbers that must not be believed.

| Attack | Defence |
|---|---|
| Path traversal through an identifier or a document path | Identifiers accept only lowercase letters, digits and single interior dashes; document paths are validated before being joined; backup names reject separators, drive colons, dot segments, trailing dots and Windows reserved device names. Tested against `..`, `C:`, `\`, `/`, `CON`, `NUL` and more. |
| Integer overflow turning into an out of bounds write | Every offset, length, sector count and block count goes through checked arithmetic that returns an error naming the operands. Property tests compare every operation against 128 bit arithmetic. |
| A decompression bomb | Decompression is bounded by the size the manifest declares, and reading one byte past that is what detects the attempt. The declared size is itself bounded by the block size. |
| An enormous allocation from a declared count | The GPT parser refuses an implausible entry count before allocating. The block table is bounded by a 32 bit index. |
| A truncated structure | Every parse checks it has enough bytes first. Tested by truncating a GPT at many lengths and requiring an error rather than a panic. |
| A forged or damaged manifest | `completion.json` records the BLAKE3 digest and size of every metadata document, checked when the backup is opened. An edited manifest is refused. |
| A block whose contents were altered | Every block is checked against the BLAKE3 digest of its uncompressed contents. Size alone is not trusted. |
| Two streams claiming the same range of a disk | Refused during restore planning, before anything is written. |
| A backup that claims to be complete but is not | The completion marker is written last and only after verification passes. Marking a backup complete with a failed verification is refused by the type system. |

None of this assumes an attacker. It is what a failing drive and a
half-finished copy produce, and the same checks catch both.

---

## Destroying the wrong disk

The most expensive mistake available, so it has the most defences.

- **Nothing is preselected.** The target list in the recovery wizard starts with
  no selection.
- **The disk holding the backup is refused**, and marked in the list.
- **A disk too small for the layout is refused**, before anything is written.
- **A sector size mismatch is refused**, because every offset in a backup is
  measured in the source's sectors.
- **The operator types the disk's serial number**: `ERASE S4NV7X0T123`, not `y`.
  Case matters. When a disk reports no serial, the phrase is `ERASE DISK 3` and
  therefore still cannot be typed absent-mindedly.
- **The confirmation is a value, not a flag.** `EraseConfirmation` can only be
  built by passing the exact phrase for a specific disk, `restore` will not
  compile without one, and a confirmation for one disk does not authorise
  another. It also stops matching if the disk's serial changes underneath it,
  which is what happens when drives are unplugged and reconnected between
  screens.
- **Every block is read and checked before the first write.** A damaged backup
  stops the operation while the replacement disk is still blank, and the error
  says so.
- **The partition table is written last**, so an interrupted restore leaves a
  disk that is visibly unfinished rather than one that looks bootable.

---

## What is in a backup

**A MjolnirVSS backup contains everything on the disk**: documents, browser
sessions, saved passwords, certificates, the SAM database, and whatever else
Windows keeps. It is as sensitive as the computer it came from.

**MjolnirVSS does not encrypt backups.** That is a real gap, and it is stated
here rather than hidden. Anyone who can read the drive can read everything on it.

Until encryption exists:

- keep the backup drive somewhere you would keep the computer;
- if the drive leaves your control, treat its contents as disclosed;
- consider BitLocker To Go on the backup drive itself, which is outside
  MjolnirVSS and works today.

Encryption is on the roadmap. It is deliberately behind the restore path, because
an encrypted backup that cannot be restored is worse than an unencrypted one that
can.

---

## What is in a log

Logs are written to the backup folder and are meant to be safe to send to
somebody else. A log records disks, partitions, sizes, offsets, stages and
MjolnirVSS's own decisions.

**A log never contains** credentials, BitLocker recovery keys, file contents, or
the names of files on the machine being backed up.

BitLocker deserves a specific statement: MjolnirVSS never reads, stores or logs a
recovery key. It detects BitLocker by looking for a signature in the first sector
of a partition, which involves no key material at all.

---

## What MjolnirVSS does not defend against

Stated plainly, because a threat model that claims to cover everything is not
useful.

- **A compromised machine.** MjolnirVSS runs as administrator. Malware already
  running with those rights can do anything MjolnirVSS can, including altering a
  backup as it is written. A backup is only as trustworthy as the machine it was
  taken on.
- **Someone with the backup drive.** No encryption, so physical possession is
  full access.
- **Deliberate tampering by someone who can rewrite the whole folder.** The
  digests in `completion.json` detect an edited document, but nothing is signed,
  so somebody able to rewrite every file can produce a self consistent forgery.
  Signing is not planned; the realistic threat is corruption, not forgery.
- **Ransomware.** A backup on a drive that stays plugged in can be encrypted like
  anything else. Unplug the drive between backups.
- **Firmware and hardware attacks.** Out of scope.
- **Denial of service.** A malformed backup can make verification slow. It runs
  on a machine the operator controls, at their request.

---

## Reporting a problem

See [`../SECURITY.md`](../SECURITY.md).
