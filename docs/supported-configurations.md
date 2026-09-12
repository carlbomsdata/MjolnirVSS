# Supported configurations

MjolnirVSS refuses what it has not been built and tested for, rather than
attempting it and producing a backup that turns out not to work. Every refusal
below is a real check with a test behind it, and every one explains itself.

---

## Supported

| | |
|---|---|
| Operating system | Windows 10 x64, Windows 11 x64 |
| Firmware | UEFI |
| Partition table | GPT |
| System disk | One physical disk holding the whole Windows installation |
| Windows volume | NTFS, not encrypted |
| EFI system partition | FAT32 |
| Recovery partition | NTFS |
| Sector sizes | 512 native and 512e (512 logical / 4096 physical); 4Kn is implemented and unit tested but has not been exercised on real hardware |
| Destination | A local NTFS volume on a different physical disk, usually an external drive |
| Backup kind | Full |
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
| **BitLocker** | See below. |
| **Restoring onto a smaller disk** | The partitions would not fit. Checked before anything is written. |
| **Restoring onto a different sector size** | Every offset in the backup is measured in the source's sectors. |
| **Restoring onto the disk holding the backup** | It would destroy the backup partway through the restore. |
| **Restoring a preview backup** | A preview holds only the first part of each partition. The format records this and the restore refuses it. |
| **A backup that is incomplete or failed verification** | Refused when the backup is opened. |
| **Writing the backup onto the disk being backed up** | A backup on the disk it protects is lost with that disk, and writing while copying would change what is being copied. |

---

## BitLocker

**MjolnirVSS detects BitLocker and stops.**

An unlocked BitLocker volume looks exactly like ordinary NTFS through the
filesystem, so checking there would miss it. MjolnirVSS reads the first sector of
the partition from the physical disk instead and looks for the `-FVE-FS-`
signature, which is present whether or not Windows currently has the volume
unlocked.

The refusal is deliberate and not a limitation of effort. Backing up an
encrypted volume and restoring it so that Windows still starts involves
decisions that have not been made yet:

- Is the backup of the encrypted bytes, or of the decrypted contents? One
  produces a backup that is useless without the key; the other produces a backup
  that silently removes the encryption.
- What happens to the recovery key, which must never be written into a backup or
  a log?
- Does the restored disk still unlock against the TPM, which is bound to the
  original machine?

Until those are answered, designed, implemented and tested, MjolnirVSS says so
rather than producing something that appears to work.

**To use MjolnirVSS today, turn BitLocker off for the system drive.** In Windows:
Settings, then Privacy and security, then Device encryption or BitLocker, and
turn it off. Decryption takes a while and the machine stays usable throughout.

---

## Not yet, but planned

These are absent rather than refused; see [`roadmap.md`](roadmap.md).

- Used block imaging. Today a backup copies every byte of a volume, including
  free space, which makes it larger and slower than it needs to be. The format
  already supports it and the verifier already honours it.
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
