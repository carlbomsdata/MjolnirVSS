# Supported configurations

MjolnirVSS refuses what it has not been built and tested for, rather than
attempting it and producing a backup that turns out not to work. Every refusal
below is a real check with a test behind it, and every one explains itself.

---

## Supported

| | |
|---|---|
| Operating system | Windows 10 x64 and Windows 11 x64 are what the code targets. **Only Windows 11 24H2 (build 26100) has been run.** Windows 10 has never been tested, and no Windows Server release has been tested or looked at |
| Firmware | UEFI |
| Partition table | GPT |
| System disk | One physical disk holding the whole Windows installation |
| Windows volume | NTFS, encrypted or not (BitLocker must be unlocked). An encrypted machine has been backed up, restored and started; the restored disk comes back **unencrypted**, which is explained in [`bitlocker.md`](bitlocker.md) |
| EFI system partition | FAT32 |
| Recovery partition | NTFS |
| Sector sizes | 512 native and 512e (512 logical / 4096 physical); 4Kn is implemented and unit tested but has not been exercised on real hardware |
| Destination | A local NTFS volume on a different physical disk, usually an external drive |
| Backup kind | Full |
| What is captured from an NTFS volume | The clusters the filesystem says are in use, read from the shadow copy, plus the boot sectors and both copies of the master file table. Volumes that will not report their allocation are captured whole, and the manifest records that they were |
| Restore target | A blank disk at least as large as the layout requires, with the same logical sector size |

---

## Refused, with the reason given

| Configuration | Why |
|---|---|
| **Dynamic disks** | A volume made of several regions cannot be recreated correctly by this version. Detected by a volume reporting more than one extent. |
| **Storage Spaces** | A Storage Space is assembled from several physical disks by Windows, so there is no single disk to capture or restore onto. Detected from the bus type. |
| **Software RAID / spanned / striped / mirrored volumes** | Same reason, detected the same way. |
| **ReFS system volumes** | Not handled by this version. Copying one without understanding it would produce a backup that cannot be trusted. |
| **MBR disks and legacy BIOS boot** | Only GPT layouts are captured and recreated. |
| **Windows spread across several disks** | This version restores one system disk at a time. |
| **Untested sector sizes** | Anything other than 512 or 4096 bytes. Every offset in a backup is measured in sectors, so an untested size puts the whole backup in doubt. |
| **BitLocker, locked** | Nothing can read a locked volume, so there is nothing to copy. An **unlocked** volume is supported; see [`bitlocker.md`](bitlocker.md). |
| **Restoring onto a smaller disk** | The partitions would not fit. Checked before anything is written. |
| **Restoring onto a different sector size** | Every offset in the backup is measured in the source's sectors. |
| **Restoring onto the disk holding the backup** | It would destroy the backup partway through the restore. |
| **Restoring a preview backup** | A preview holds only the first part of each partition. The format records this and the restore refuses it. |
| **A backup that is incomplete or failed verification** | Refused when the backup is opened. |
| **Writing the backup onto the disk being backed up** | A backup on the disk it protects is lost with that disk, and writing while copying would change what is being copied. |

---

## BitLocker

**An unlocked BitLocker volume is supported.** It is read through its shadow
copy, which presents it decrypted, so the backup captures an ordinary NTFS
filesystem. This was established by measurement, not assumption; see
[`bitlocker.md`](bitlocker.md) for the evidence and how to reproduce it.

The state is read from `Win32_EncryptableVolume`, which is the interface
Microsoft documents for it, corroborated by the partition header. Only the two
status properties are read; none of the methods that handle key material is
called.

A **locked** volume is refused: Windows itself cannot see inside it, so a backup
would be empty rather than encrypted.

Two consequences, stated rather than implied:

- the backup holds readable, decrypted copies of the files, and is not itself
  encrypted;
- a restored disk comes back unencrypted, and BitLocker can be switched on again
  afterwards.

MjolnirVSS never reads, stores or logs a recovery key, and never changes
BitLocker's state.

---

## Not yet, but planned

These are absent rather than refused; see [`roadmap.md`](roadmap.md).

- Restoring individual files from a backup.
- Creating recovery media.
- Repairing the UEFI boot configuration after a restore.
- Incremental backups.
- Restoring onto a smaller disk by shrinking the Windows partition.
- Restoring onto hardware different from the original.

---

## How to check your machine

```powershell
MjolnirVSS.exe inspect
```

This prints the computer, the disks, the partitions, the volumes, and what a
backup would capture. If the machine is not supported it says exactly what it
found and why that stops it. Nothing is written and no shadow copy is taken.
