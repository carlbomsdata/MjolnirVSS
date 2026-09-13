# Recovery media

You need something to boot the broken computer from. MjolnirVSS makes it for
you, out of the Windows parts your computer already has.

---

## What it does

Press **Recovery media** in the main window, or run:

```powershell
MjolnirVSS.exe recovery-media --iso E:\MjolnirVSS-Recovery.iso
```

MjolnirVSS finds a Windows recovery environment on this computer, puts
`MjolnirVSS.Restore.exe` inside it, makes it start automatically, and writes a
bootable image. The result is about 300 MB.

To see what it would use without making anything:

```powershell
MjolnirVSS.exe recovery-sources
```

| State | |
|---|---|
| Building an ISO | **Implemented.** Needs the Windows ADK, for the reason below |
| Writing a USB drive directly | **Not implemented.** Write the ISO to a stick with Microsoft's `MakeWinPEMedia`, or any tool that writes a bootable image |
| Whether the result boots your firmware | **Only booting it proves that.** See below |

---

## Nothing Microsoft owns is shipped or downloaded

Windows PE cannot be redistributed. MjolnirVSS therefore carries none of it and
downloads none of it. It uses what the computer already has a licence for, in
this order:

1. **The Windows ADK's Windows PE add-on**, if installed. This is the supported
   way to build recovery media and the route Microsoft's own `copype` and
   `MakeWinPEMedia` scripts take. It provides the boot image, the media layout,
   and `oscdimg.exe`, which is what turns a folder into a bootable ISO.
2. **This computer's own recovery image**, `winre.wim` in the recovery
   partition. Every Windows installation has one. It is enough to recover from,
   but it comes with no media layout and no ISO builder, so on its own it cannot
   produce an ISO.

`oscdimg.exe` is the reason ISO output needs the ADK: it ships only there and
**cannot be redistributed**, so MjolnirVSS can offer ISO output only when the
computer it is running on already has it.

The Windows files on the media you make belong to Microsoft and are licensed to
your computer. MjolnirVSS says so when it finishes. Keep the media; do not pass
it on.

---

## What gets built

```text
EFI\Boot\bootx64.efi              what UEFI firmware loads
EFI\Microsoft\Boot\bootmgfw.efi   the Windows boot manager
boot\, bootmgr                    the older BIOS path
sources\boot.wim                  Windows PE, with MjolnirVSS inside it
```

Inside `boot.wim`:

```text
X:\MjolnirVSS\MjolnirVSS.Restore.exe   the recovery application
X:\MjolnirVSS\LICENSE, NOTICE, docs\   so the media explains itself
X:\Windows\System32\startnet.cmd       runs wpeinit, then the application
```

`startnet.cmd` runs `wpeinit` **before** starting anything. Skipping it is the
usual reason a home made Windows PE image cannot see any disks. The command
prompt is left running underneath, so a failure leaves you a prompt to work
from instead of a machine that reboots.

---

## What is checked, and what is not

When the build finishes, MjolnirVSS checks the result:

- the file exists and is a believable size;
- it is a real ISO 9660 image, by reading the volume descriptor;
- it is marked bootable, by finding the El Torito boot record.

It says plainly what it did **not** check:

- what is inside the image, because looking would mean mounting it and changing
  this computer's drive letters;
- **whether your computer's firmware will boot it.** Nothing short of booting it
  proves that.

So boot it once, before you need it. Firmware differs between machines more than
anything else in this process.

---

## Writing a USB drive

Not built into MjolnirVSS yet. Writing a USB drive erases it completely, and
this version has not been tested doing that, so it says so rather than doing it
badly.

The manual route, using Microsoft's own tool:

```powershell
# From an elevated "Deployment and Imaging Tools Environment" prompt.
MakeWinPEMedia /UFD C:\WinPE_amd64 F:
```

Or write the ISO MjolnirVSS made to a stick with any tool that writes a bootable
image. Either way, check it boots.

---

## The older route, which still works

If you would rather not make anything, Windows installation media plus one file
copy also works:

1. On a working computer, use Microsoft's **Media Creation Tool** to write
   Windows installation media to a USB stick of 8 GB or more. Any recent Windows
   10 or 11 media works; it does not have to match the machine being recovered.
2. Copy **`MjolnirVSS.Restore.exe`** onto the stick, at the top level.
3. Boot from it, press **Shift+F10** at the Setup screen, and run the program.

Step by step in [`bare-metal-restore.md`](bare-metal-restore.md).

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

**This is evidence, not proof.** The recovery application has not yet been run
inside Windows PE. Until it has, treat "it will start" as an expectation.

---

## Checking the media yourself

1. Boot it on the machine you might need to recover.
2. The recovery application should start on its own. If it does not, the command
   prompt behind it is still there: run
   `X:\MjolnirVSS\MjolnirVSS.Restore.exe`.
3. Run `MjolnirVSS.Restore.exe find-backups`. If it lists your backup, the whole
   chain works: the media boots, the program runs, the drive is visible and the
   backup is readable.
4. Run a dry run, which checks everything and writes nothing:

```text
MjolnirVSS.Restore.exe restore F:\Backups\<folder> --target \\.\PhysicalDrive1 --dry-run
```

That is as close to a rehearsal as you can get without erasing a disk.
