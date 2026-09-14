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
| Backup kind | An image of the whole system disk. Declared to Windows as a **copy** backup, which is what stops it disturbing anybody else's backups: see below |
| What is captured from an NTFS volume | The clusters the filesystem says are in use, read from the shadow copy, plus the boot sectors and both copies of the master file table. Volumes that will not report their allocation are captured whole, and the manifest records that they were |
| Restore target | A blank disk at least as large as the layout requires, with the same logical sector size |

---

## What a backup does to other backup software

MjolnirVSS declares a **copy** backup (`VSS_BT_COPY`), not a full one.

That distinction does nothing at all on a machine with no application writers,
and matters a great deal on a server. `vss.h` defines a full backup as one where
"each file's backup history will be updated to reflect that it was backed up",
and writers act on that: SQL Server and Exchange treat a completed full backup
as theirs to account for and **truncate their transaction logs**. A copy backup
is defined as copying files "regardless of the state of each file's backup
history", which "will not be updated".

MjolnirVSS images a disk. It cannot restore a single database, it keeps no
backup history, and it is in no position to take responsibility for anybody's
log chain. Declaring a full backup would tell every writer on the machine
something untrue, and the cost on a real server is somebody else's backup chain
quietly broken by a tool that was only supposed to be reading.

So: **taking a MjolnirVSS backup does not disturb the backup software already on
the machine**, and does not truncate any logs.

---

## Windows Server

Nothing in MjolnirVSS refuses a server. The checks are about the shape of the
disk - GPT, one system disk, a sector size it knows, no Storage Spaces - and a
UEFI Windows Server installation is the same shape as a UEFI Windows 11 one.

What is different about a server, and worth knowing before trusting it:

| | |
|---|---|
| **Data on other disks is not captured** | MjolnirVSS backs up the system disk. A server with its data on a second disk gets that data backed up by **nothing here**. This is the single biggest thing to get wrong on a server |
| **Storage Spaces and dynamic disks** | Common on servers, and refused with an explanation. See the table above |
| **Server Core** | Has no desktop to open a window on. Every command works from the prompt, and if the window cannot be opened the reason is printed to the console rather than shown in a message box nobody can see |
| **No recovery partition** | Many server installations have no Windows RE partition. Recovery media is then built from the Windows ADK instead, which `recovery-sources` reports |
| **Application writers** | Their files are captured as they sit on the frozen volume, consistent to that instant. They are not backed up as components and cannot be restored individually |

---

## Windows 7 and 8.1

**Not supported, and not a small job to support.** Three separate reasons, any
one of which is on its own enough:

1. **The program will not load.** It imports `SetProcessDpiAwarenessContext`,
   `GetDpiForWindow` and `SystemParametersInfoForDpi` from `user32.dll`, none of
   which exist before Windows 10, and links `combase.dll`, which is Windows 8
   and later. A static import that cannot be resolved stops the process before
   `main` runs.
2. **The toolchain does not target it.** Rust's `x86_64-pc-windows-msvc` target
   requires Windows 10. Building for Windows 7 means the tier 3
   `x86_64-win7-windows-msvc` target, which is a different support level.
3. **The disks are the wrong shape.** Windows 7 machines are overwhelmingly BIOS
   and master boot record, which MjolnirVSS refuses by design. A GPT and UEFI
   Windows 7 installation exists but is rare.

Supporting it would mean a second build target, replacing those imports with
runtime lookups, and implementing MBR capture and restore. It is not a
configuration flag.

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
