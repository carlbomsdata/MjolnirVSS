# Recovery media

You need something to boot the broken computer from. This document explains the
route that works today, the route being built, and why they are different.

**Automatic recovery media creation is not implemented.** The button in the main
window says so rather than pretending.

---

## What works today

Use Microsoft's own tool, and copy one file onto the result.

1. On a working computer, download the **Media Creation Tool** from Microsoft's
   website and use it to write Windows installation media to a USB stick of 8 GB
   or more. Any recent Windows 10 or 11 media works; it does not have to match
   the machine being recovered.
2. Copy **`MjolnirVSS.Restore.exe`** onto the USB stick, at the top level.
3. Boot from it, press **Shift+F10** at the Setup screen, and run the program.

That is the whole procedure. It is described step by step in
[`bare-metal-restore.md`](bare-metal-restore.md).

It is worth doing this **before** anything goes wrong, and booting the stick once
to check it works on your machine's firmware.

---

## Why MjolnirVSS does not just make the USB for you

It will, but there are two obstacles and they have different answers.

### A bootable USB can be made without redistributing anything

Everything needed is already on a running Windows machine:

- the recovery image, at `C:\Windows\System32\Recovery\Winre.wim`, or in the
  recovery partition;
- the boot loader files, in the EFI system partition.

MjolnirVSS can format a USB stick as FAT32, copy those files from the machine it
is running on, write a boot configuration, and copy the recovery application
alongside. Nothing Microsoft owns is redistributed: the files come from the
user's own installation and never leave their computer.

This is the route being built, and it is milestone 6 on the roadmap.

### An ISO cannot be made without the Windows ADK

Producing a bootable ISO needs `oscdimg.exe`, which ships only in the Windows
Assessment and Deployment Kit and **cannot be redistributed**. So ISO output can
only ever be offered when the ADK happens to be installed, and will be greyed out
with an explanation otherwise.

That is why the USB route is the primary one and ISO is a convenience for people
who already have the ADK.

---

## What the wizard will do

When it exists:

1. **Choose a USB drive**, or an ISO file if the ADK is present.
2. **Confirm.** Creating a recovery USB **erases the drive completely**. The
   confirmation will show the drive's model, size and contents, in the same
   shape as the erase confirmation in the recovery application.
3. **Create.** Format FAT32, copy the boot files and recovery image from this
   machine, write the boot configuration, copy `MjolnirVSS.Restore.exe`.
4. **Check.** Confirm the recovery application and the files it needs are present
   and readable on the finished media.

The one thing the wizard cannot check is whether the stick actually boots on your
firmware. That needs you to restart the computer and try it, and no amount of
verification substitutes for it.

---

## Why the recovery application runs in Windows PE at all

Windows PE is a cut down Windows. Most of what a normal program expects is
missing: no .NET, no WinUI, no installed runtimes, and only a basic display
driver with no GPU acceleration.

`MjolnirVSS.Restore.exe` is built for that:

- **Native Win32 controls only.** No graphics stack beyond what the basic display
  adapter provides. This is the same interface technology Windows Setup itself
  uses, which is why Setup can show a window in this environment.
- **Static C runtime.** No Visual C++ Redistributable to install.
- **No shadow copy code.** `vssapi.dll` is not part of a base Windows PE image.
  The dependency graph makes it impossible for the recovery application to
  require it, and `scripts/package.ps1` fails the build if it ever appears in the
  binary's imports.

The libraries the recovery application does import are `kernel32`, `ntdll`,
`user32`, `gdi32`, `comctl32`, `oleaut32` and one API set — all present in a base
Windows PE image.

**This is evidence, not proof.** The recovery application has never been run
inside Windows PE. Until it has, treat "it will start" as an expectation.

---

## Checking the media yourself

Before trusting a recovery USB:

1. Boot it on the machine you might need to recover. Firmware differs between
   machines more than anything else in this process.
2. At the Setup screen press **Shift+F10**.
3. Run `MjolnirVSS.Restore.exe find-backups`. If it lists your backup, the whole
   chain works: the media boots, the program runs, the drive is visible and the
   backup is readable.
4. Run a dry run, which checks everything and writes nothing:

```text
MjolnirVSS.Restore.exe restore F:\Backups\<folder> --target \\.\PhysicalDrive1 --dry-run
```

That is as close to a rehearsal as you can get without erasing a disk.
