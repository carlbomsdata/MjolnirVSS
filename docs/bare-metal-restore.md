# Recovering a computer

This is what to do when the disk has failed and you have a MjolnirVSS backup.

> **Read this first.** This has been done successfully, in virtual machines, and
> never on real hardware. A Windows 11 machine was backed up while running,
> restored onto a blank disk from MjolnirVSS recovery media, and started. What
> that proves is that the procedure below is real rather than hopeful; it does
> not prove it on your computer, with your firmware and your disk. If this is a
> real emergency and you have another backup, use that one.

---

## What you need

- The external drive holding the backup.
- A blank replacement disk, at least as large as the one that failed, with the
  same logical sector size (almost always 512 bytes).
- A USB stick of 8 GB or more, and a working computer to prepare it on.

---

## Step 1: make bootable media

Do this **before** the disk fails, on the machine being protected, while it still
works. Media made afterwards on somebody else's computer is fine too.

Press **Recovery media** in the MjolnirVSS window, or:

```powershell
MjolnirVSS.exe recovery-media --iso D:\MjolnirVSS-Recovery.iso
```

This builds a bootable Windows PE disc out of the Windows parts already on the
machine, with MjolnirVSS on it. Nothing is downloaded. It needs the Windows
Assessment and Deployment Kit for the part that makes an ISO; without it,
MjolnirVSS says so rather than half doing it.

Write the ISO to a USB stick with Microsoft's `MakeWinPEMedia`, or any tool that
writes a bootable image, or attach it directly if you are recovering a virtual
machine.

**The disc holds Microsoft's files and is licensed to the computer that made it.
Do not pass it on.**

If you would rather not build one, the old way still works: write Windows
installation media with Microsoft's Media Creation Tool and copy
`MjolnirVSS.Restore.exe` onto it. Boot it, press **Shift+F10** at the language
screen for a command prompt, and carry on from step 5.

More detail: [`recovery-media.md`](recovery-media.md).

---

## Step 2: fit the replacement disk

Physically install the new disk. Leave the old one disconnected if it is
failing — a disk that returns errors slows everything down and adds a disk you
might select by mistake.

---

## Step 3: boot from the USB stick

Start the computer and enter the boot menu, usually **F12**, **F10**, **Esc** or
**F2** depending on the manufacturer. Choose the USB stick.

**On MjolnirVSS recovery media**, Windows PE starts and the recovery wizard
opens on its own. Skip to step 7.

**On Windows installation media**, wait for the Setup screen with the language
options and carry on below.

---

## Step 4: open a command prompt

Press **Shift+F10**. A black command prompt window appears.

**Do not press Install now.** Windows Setup would erase the disk and install a
fresh copy of Windows, which is not what you want.

---

## Step 5: find the USB stick

Drive letters inside the recovery environment are not the ones the machine
normally uses. Find the stick:

```text
diskpart
list volume
exit
```

Look for the volume whose label matches your USB stick. Then check:

```text
dir E:\MjolnirVSS.Restore.exe
```

trying each letter until you find it.

---

## Step 6: run the recovery application

```text
E:\MjolnirVSS.Restore.exe
```

The wizard opens. If the window does not appear, the command line works too:

```text
E:\MjolnirVSS.Restore.exe find-backups
E:\MjolnirVSS.Restore.exe list-disks
E:\MjolnirVSS.Restore.exe inspect-backup F:\Backups\DESKTOP-1A2B_2026-09-12_1015
```

---

## Step 7: work through the wizard

**Find a backup.** Press *Search for backups*. Every attached drive is searched,
to a depth of three folders. Nothing is changed.

If nothing is found, check the drive is plugged in, and that the backup folder is
not buried deeper than three folders from the top of the drive.

**Choose the backup.** Each one shows its name, the computer it came from, when
it was taken and its size. A backup marked `INCOMPLETE` was interrupted when it
was taken and cannot be used.

**Choose the disk to restore onto.** Nothing is preselected; you have to pick.
The drive holding the backup is marked and cannot be chosen. Check the model,
size and serial number carefully — this disk is about to be erased completely.

**Check what is about to happen.** This screen shows what will be restored, the
disk it will be written to, and every partition currently on that disk that will
be destroyed.

To continue you must type the disk's erase phrase exactly:

```text
ERASE <the disk's serial number>
```

for example `ERASE S4NV7X0T123`. If the disk reports no serial number, the phrase
is `ERASE DISK <number>` instead. `y` is not accepted, and neither is the wrong
case. The *Restore* button stays disabled until what you have typed is exactly
right.

**Restore.** The partitions are written first, then the partition table. Do not
turn the computer off. Stopping partway leaves a disk that cannot start Windows
and the restore has to be run again from the beginning.

---

## Step 8: restart

Close the wizard, remove the USB stick, and restart.

---

## If Windows does not start

MjolnirVSS repairs the boot configuration itself, as part of every restore and
on its own afterwards. In the one test where this mattered, a restored disk was
deliberately left with no boot files, failed to start with
`0xc000000f`, and started again after the repair below.

Boot the recovery media and run:

```text
X:\MjolnirVSS\MjolnirVSS.Restore.exe repair-boot --disk 0
```

Use `--dry-run` first if you want to see what it would change without changing
anything. `--disk 0` is the disk you restored onto; `list-disks` names them.

It refuses to touch the disk the machine is currently running from, so run it
from the recovery media rather than from a Windows that has started.

If that does not help, Microsoft's own repair is still there: boot the media,
choose **Repair your computer**, then **Troubleshoot**, then **Startup Repair**.
Or, by hand from a command prompt:

```text
diskpart
list disk
select disk 0
list partition
select partition 1
assign letter=S
exit

bcdboot C:\Windows /s S: /f UEFI
```

Replace `C:` with whatever letter the restored Windows partition has, and
`partition 1` with the EFI system partition.

---

## Rehearsing a restore without risking anything

Two ways, both worth doing before you need them.

**A dry run** checks everything and writes nothing. It reads every stored block,
decompresses it, checks its checksum, and reports what would be written:

```text
MjolnirVSS.Restore.exe restore F:\Backups\DESKTOP-1A2B_2026-09-12_1015 ^
    --target \\.\PhysicalDrive1 --dry-run
```

**A virtual machine** is the real rehearsal, and the only thing that answers the
question that matters. Create a virtual machine with a blank disk at least as
large as the original, attach the drive holding the backup, boot the recovery
media inside it, and restore. Then see whether Windows starts.

See [`testing.md`](testing.md) for how this is set up.

---

## What the recovery application will not do

- It will not erase the disk holding the backup you selected.
- It will not restore onto a disk too small for the layout.
- It will not restore onto a disk whose sector size differs from the original.
- It will not restore a backup that is incomplete or failed verification.
- It will not restore a preview backup.
- It will not write anything before every block it needs has been read and
  checked.
- It will not accept `y` in place of the erase phrase.
