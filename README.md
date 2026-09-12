# MjolnirVSS

Portable bare metal backup and recovery for Windows 10 and Windows 11.

---

## Status: experimental. Do not rely on it yet.

**No restored computer has ever been booted from a MjolnirVSS backup.** The
backup engine, the image format, the verifier and the restore engine all work
and are tested, but the one test that would make this product trustworthy — take
a backup of a real Windows installation, restore it onto a blank disk, and watch
Windows start — has not been carried out.

Until that happens, treat MjolnirVSS as a thing to experiment with on a virtual
machine, not as the backup of anything you care about. Keep another backup.

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
| Verification: decompress and checksum every block | **Implemented and tested**, including against deliberately damaged backups |
| Restoring onto a blank disk | **Implemented**, tested against virtual disks only |
| Rebuilding the partition table on the replacement disk | **Implemented**, tested against virtual disks only |
| Refusing unsafe restore targets | **Implemented and tested** |
| Graphical interface for backup | **Implemented**, not yet tested by anyone but its author |
| Graphical recovery wizard | **Implemented**, **never run inside Windows PE** |
| **Booting a restored Windows** | **Never tested. This is the gate that matters.** |
| Repairing UEFI boot configuration after a restore | **Not implemented.** If Windows does not start, you run Startup Repair yourself |
| Restoring individual files from a backup | **Not implemented.** The index format is designed; the browser is not built |
| Creating recovery media | **Not implemented.** Use Microsoft's Media Creation Tool and copy the recovery program onto it |
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
