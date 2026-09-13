# MjolnirVSS

Portable bare metal backup and recovery for Windows 10 and Windows 11.

---

## Status: early. Tested in virtual machines, never on real hardware.

**A restored Windows has now been booted from a MjolnirVSS backup.** On
13 September 2026, in a disposable VMware virtual machine: a live backup of a
running Windows 11, taken through the shadow copy service with used block
imaging; recovery media built by MjolnirVSS from this computer's own Windows
parts; the backup restored onto a blank 64 GB disk from that media, driven from
the keyboard in Windows PE; and the machine started on its own, with no repair.

Everything the restored machine was checked for came back right: all twelve test
files matched the hashes taken when they were made, every partition kept its
unique identifier, offset and size, the EFI partition held its boot files, the
boot configuration named the Windows loader, and the Windows Recovery
Environment was still registered.

That is one machine, once, and it was a virtual one. **No real computer has been
restored.** Firmware differs, disks differ, and a virtual NVMe disk is not a
Samsung one. Treat MjolnirVSS as something to test on a machine you can afford
to lose, and keep another backup.

What that means in practice is set out honestly below, feature by feature.

---

## What it does

MjolnirVSS takes an image of a whole Windows system disk while Windows is
running, stores it on an external drive as ordinary files, and can write it back
onto a blank replacement disk after the original has failed.

- **Nothing is installed.** Download, extract, run. No service, no driver, no
  scheduled task, no registry entries, no .NET, no runtime of any kind. Deleting
  the folder removes every trace.
- **The computer stays usable** while the backup runs. Microsoft's Volume Shadow
  Copy Service freezes the disk for the instant it takes to start the snapshot,
  and the copy is taken from that frozen view.
- **Every partition needed to boot is captured**: the GUID partition table, the
  EFI system partition, the Microsoft Reserved partition, Windows, and the
  recovery partition. A partition is never silently left out; if one cannot be
  read, the backup fails rather than quietly producing a disk that will not
  start.
- **Free space is not copied.** MjolnirVSS asks the filesystem which clusters are
  in use and reads only those, so a half empty drive makes a half sized backup. A
  volume that will not answer is copied whole instead, and the backup records
  that it had to.
- **The backup is verified before it is called a backup.** Every stored piece is
  read back, decompressed and checked against its BLAKE3 checksum. Only then is
  the completion marker written. A backup that was interrupted is never reported
  as usable.
- **The format is documented and open.** A backup is a plain folder of JSON and
  compressed blocks. See [`docs/backup-format.md`](docs/backup-format.md), which
  contains enough detail to write an independent reader.

---

## Where each feature actually stands

| Feature | State |
|---|---|
| Consistent live backup using VSS | **Implemented**, proven against a real Windows 11 machine |
| Capturing the full GPT layout, EFI, MSR, Windows and recovery partitions | **Implemented**, tested against synthetic disks |
| Used block imaging: skipping free space on NTFS volumes | **Implemented**, proven on a real Windows volume: 14.2 GiB read out of a 62.8 GiB partition, and the result restored and booted |
| Warning before a backup may cost you restore points | **Implemented and measured** on a real machine |
| Verification: decompress and checksum every block | **Implemented and tested**, including against deliberately damaged backups |
| Restoring onto a blank disk | **Implemented**, done from real recovery media onto a blank virtual disk, and the result booted |
| Rebuilding the partition table on the replacement disk | **Implemented**, and the restored table checked partition by partition against the original |
| Refusing unsafe restore targets | **Implemented and tested** |
| Graphical interface for backup | **Implemented**, not yet tested by anyone but its author |
| Graphical recovery wizard | **Implemented**, run in real Windows PE and driven through a whole restore from the keyboard |
| **Booting a restored Windows** | **Done once, in a virtual machine.** Never on real hardware |
| Repairing UEFI boot configuration after a restore | **Implemented**, and not yet needed: the restore that was booted needed no repair |
| Restoring individual files from a backup | **Implemented**, proven against a real Windows volume out of a real backup |
| Creating recovery media | **Implemented** where the Windows ADK is installed, and the media it makes has been booted |
| BitLocker: unlocked volume | **Implemented and measured.** See [`docs/bitlocker.md`](docs/bitlocker.md) |
| BitLocker: locked volume | **Refused**, clearly |
| Incremental backups | Not implemented, and deliberately not started until the above works |

---

## Requirements

- Windows 10 or Windows 11, 64 bit
- A UEFI machine with a GPT system disk
- One physical disk holding Windows
- An external drive formatted NTFS, with room for the backup
- Administrator rights (MjolnirVSS asks when it starts)

**Not supported**, and refused with an explanation rather than attempted:
dynamic disks, Storage Spaces, software RAID, ReFS system volumes, machines that
boot in legacy BIOS mode, Windows spread across several disks, and disks with an
untested sector size.

### Restore points

Taking a shadow copy can cost you older ones. Windows keeps a volume's shadow
copies in one pool and can only release space from the oldest end, so when
MjolnirVSS removes the temporary snapshot it took, Windows sometimes removes
older ones first to reclaim the space. This was measured on a real machine, and
it happens to any program that takes a snapshot, not only this one.

MjolnirVSS removes only the snapshot it created, by its own identifier. It says
so before the backup starts when there is anything to lose, shows the figures
behind **Show details**, and does not claim your restore points will survive,
because it cannot.

Your files are not affected. See
[`docs/vss-lifecycle.md`](docs/vss-lifecycle.md).

### BitLocker

**An unlocked BitLocker volume is backed up normally.** MjolnirVSS reads it
through its shadow copy, which presents it decrypted, so the backup holds an
ordinary NTFS filesystem. This was measured rather than assumed; the evidence is
in [`docs/bitlocker.md`](docs/bitlocker.md) and you can reproduce it with
`MjolnirVSS.exe diagnose-bitlocker`.

A **locked** volume is refused, because nothing can read it.

Two things follow, and MjolnirVSS says both rather than leaving them implied:

- **The backup contains readable copies of your files.** It is not encrypted, by
  BitLocker or by MjolnirVSS. Look after the backup drive as carefully as the
  computer.
- **A restored disk comes back unencrypted.** BitLocker protection does not carry
  over. You can turn it on again after restoring.

MjolnirVSS never reads, stores or logs a recovery key, and never changes
BitLocker's state.

---

## Taking a backup

1. Plug in the external drive.
2. Run `MjolnirVSS.exe`. Windows asks for permission; say yes.
3. Press **Back up this PC**.
4. Check what it found, choose the folder to save into, and press **Start backup**.
5. Wait. The backup verifies itself at the end.

The backup lands in a folder named after the computer and the time, for example
`DESKTOP-1A2B_2026-09-12_1015`.

### From a command prompt

The window is the product; these exist for testing, diagnostics and scheduled
backups.

```powershell
MjolnirVSS.exe inspect
MjolnirVSS.exe backup --destination E:\Backups
MjolnirVSS.exe verify E:\Backups\DESKTOP-1A2B_2026-09-12_1015
MjolnirVSS.exe list E:\Backups
MjolnirVSS.exe cleanup-snapshots
MjolnirVSS.exe diagnose-bitlocker
```

`--preview <bytes>` captures only the first part of each partition, so the whole
pipeline can be exercised in seconds. A preview backup is marked as such and can
never be restored.

MjolnirVSS never registers a scheduled task. If you want backups on a schedule,
create the task yourself and point it at the `backup` command above.

---

## Checking a backup

```powershell
MjolnirVSS.exe verify E:\Backups\DESKTOP-1A2B_2026-09-12_1015
```

This reads every stored block, decompresses it and compares its checksum. It
exits with code 0 if the backup is sound and 6 if it is not.

A backup is verified automatically when it is taken. Verifying again later is
worth doing, because the thing most likely to damage a backup is the drive it is
sitting on.

---

## Recovering a computer

Recovery media creation is not built yet. The supported route today:

1. On any working computer, use Microsoft's own Media Creation Tool to make a
   Windows installation USB.
2. Copy `MjolnirVSS.Restore.exe` onto that USB.
3. Boot the broken computer from it.
4. When Windows Setup appears, press **Shift+F10** for a command prompt.
5. Find the USB's drive letter and run `MjolnirVSS.Restore.exe`.
6. Follow the wizard: find the backup, choose it, choose the blank replacement
   disk, read what is about to be erased, and type the disk's serial number to
   confirm.

Before it erases anything the recovery program shows you the disk number, model,
serial number, size and every partition currently on it, and requires you to type

```text
ERASE <the disk's serial number>
```

exactly. It will not accept `y`. It refuses to erase the drive holding the
backup, and it refuses a disk too small to hold the layout.

Full walkthrough: [`docs/bare-metal-restore.md`](docs/bare-metal-restore.md).

---

## Test your recovery before you trust a backup

This is not boilerplate. A backup you have never restored from is a guess about
the future.

The way to find out whether MjolnirVSS works for your machine is to restore one
of its backups onto a spare disk, or into a virtual machine, and see whether
Windows starts. Until you have done that, keep whatever backup you were using
before.

---

## Building

Needs the stable Rust toolchain and the Visual Studio C++ build tools.

```powershell
rustup target add x86_64-pc-windows-msvc
cargo build --release --workspace
cargo test --workspace
.\scripts\package.ps1
```

`package.ps1` produces `dist\MjolnirVSS`, which is the portable folder. It also
checks that the recovery executable does not import `vssapi.dll`, because that
library does not exist in Windows PE.

The release build links the C runtime statically, so the Visual C++
Redistributable is not required.

---

## Documentation

| Document | What it covers |
|---|---|
| [`docs/architecture.md`](docs/architecture.md) | How the pieces fit together and why |
| [`docs/backup-format.md`](docs/backup-format.md) | The on disk format, in enough detail to reimplement |
| [`docs/vss-lifecycle.md`](docs/vss-lifecycle.md) | How the shadow copy is taken and released |
| [`docs/bitlocker.md`](docs/bitlocker.md) | How BitLocker is handled, and the measurement behind it |
| [`docs/bare-metal-restore.md`](docs/bare-metal-restore.md) | Recovering a computer, step by step |
| [`docs/recovery-media.md`](docs/recovery-media.md) | Making bootable media, and why it is not automated yet |
| [`docs/file-recovery.md`](docs/file-recovery.md) | Getting single files back (designed, not built) |
| [`docs/supported-configurations.md`](docs/supported-configurations.md) | Exactly what is supported and what is refused |
| [`docs/testing.md`](docs/testing.md) | How to run the tests, including the manual matrix |
| [`docs/threat-model.md`](docs/threat-model.md) | What MjolnirVSS treats as hostile, and what it does not defend against |
| [`docs/roadmap.md`](docs/roadmap.md) | What comes next, in order |

---

## Licence

GPL-3.0-or-later. See [`LICENSE`](LICENSE).

MjolnirVSS is an independent, clean room implementation. It is not derived from,
and does not interoperate with, any commercial backup product. It uses
documented Microsoft interfaces and a backup format designed for this project.
