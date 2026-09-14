# The virtual machine test harness

The tests that decide whether MjolnirVSS works cannot run on a build server.
They need a real Windows installation to back up, a blank disk to restore onto,
and somebody to watch the machine start. This harness does all three inside
disposable VMware Workstation virtual machines.

It is **opt in**. Nothing here runs as part of `cargo test`, and nothing here is
needed to build MjolnirVSS.

---

## What it will not touch

Enforced in code, in [`MjolnirLab.psm1`](../tests/vm/MjolnirLab.psm1), not left
as a rule to remember:

- every file it creates lives under one lab root, resolved and checked before
  anything is written or deleted;
- a virtual machine is only stopped, reverted or deleted when **both** its files
  are inside that root **and** its name begins with `MjolnirVSS-Test-`;
- it never writes to a physical disk;
- it never reads, starts or modifies a virtual machine it did not create.

The lab root defaults to `C:\MjolnirVSS-TestLab` and can be moved with the
`MJOLNIR_LAB_ROOT` environment variable. Nothing in it belongs in the
repository, and nothing in it is committed.

---

## What you need

| | |
|---|---|
| VMware Workstation | 17 or later, for `vmrun` and `vmware-vdiskmanager` |
| Windows 11 x64 installation media | An ordinary ISO. Nothing is redistributed |
| Windows ADK | With the Deployment Tools and the Windows PE add-on |
| Free space | About 200 GB for the whole cycle |
| Python 3 | With Pillow, for the screen tools below |

---

## The machines

| Name | What it is |
|---|---|
| `MjolnirVSS-Test-Source` | Windows 11, UEFI, GPT, NVMe. EFI, Microsoft Reserved, Windows and recovery partitions, plus a second disk to write backups to |
| `MjolnirVSS-Test-Restore` | A blank target disk, the source's backup disk, and MjolnirVSS recovery media in the drive |

```powershell
pwsh -File tests\vm\new-source-vm.ps1      # installs Windows, unattended
pwsh -File tests\vm\new-payload.ps1 -Vmx <vmx>   # a disc with a fresh build on it
pwsh -File tests\vm\new-restore-vm.ps1     # the machine the restore is proved on
```

---

## Talking to a machine that has no VMware Tools

Every machine the harness drives is one Tools cannot be installed into: Windows
Setup while it runs, Windows PE while the recovery application is on screen, a
restored Windows that has never been logged into. So the harness uses two
channels that need nothing inside the guest.

**A serial port, for what the guest says.** Each machine has `COM1` backed by a
file on the host. The guest scripts write their progress to it, and the harness
reads that file. This is how a phase reports `PHASE-BACKUP-COMPLETE` or the
reason it did not.

**VNC, for what the guest shows and what it is told.** VMware Workstation serves
the framebuffer on the loopback address, and the same connection carries key and
pointer events.

```powershell
python tests\vm\tools\vnc_screenshot.py --port 5990 --out screen.png --text
python tests\vm\tools\vnc_input.py --port 5990 --keys "Tab Tab Return"
python tests\vm\tools\vnc_input.py --port 5990 --type "hello"
```

The screenshot tool is what proves the recovery application draws its window in
Windows PE, and the input tool is what proves the window can be operated from
the keyboard alone.

**The payload disc, for getting files in.** A build of MjolnirVSS and the guest
scripts are put on a small ISO and left in the machine's second drive.

---

## Things that went wrong, and why they are written down

Each of these cost a run, and each is now handled by the harness rather than by
somebody remembering.

**VMware crashed on a hand written machine.** A `.vmx` without the PCI bridge
devices runs out of PCIe slots part way through building itself, and
`vmware-vmx` exits with an access violation rather than a message. The bridges
are now always written.

**The installation disc waited for a key press.** A Windows disc boots through
an EFI image that prints "Press any key to boot from CD or DVD" and gives up
after a few seconds. With nobody watching, the firmware times out and boots
nothing. The disc is rebuilt once with `efisys_noprompt.bin` from the ADK.

**The answer file asked for a language the disc did not have.** The media is
English International, which carries `en-GB` only. Asking for `en-US` made Setup
abandon the answer file and show its language page. Setup does not say so; it
simply becomes interactive.

**The answer file did not reach the out of box experience.** Locales set in the
Windows PE pass do not suppress the region and keyboard pages. They need a
`Microsoft-Windows-International-Core` component in the `oobeSystem` pass.

**The guest keyboard is not the host keyboard.** A VNC key event names a
keysym, and the guest translates it through its own layout, so the character
that arrives is the one in that *position* on the guest's keyboard rather than
the one asked for. On the `en-GB` guest this machine installs, a backslash
arrives as `#`, a pipe as `~`, and a double quote as `@`. Commands typed into
the guest therefore use forward slash paths, which PowerShell accepts, and no
quotes at all. Anything that needs quoting goes into a script on the payload
disc instead of being typed.

---

## What the harness found in MjolnirVSS itself

This is the point of it. Both of these passed every synthetic test and failed on
the first real Windows volume.

**A shadow copy is shorter than its partition.** NTFS keeps a spare copy of its
boot sector in the last sector of the partition, outside the filesystem. A
shadow copy covers the filesystem, so reading the last sector through it fails
with "reached the end of the file". MjolnirVSS now reads that tail from the
disk, which is safe for the same reason the boot partitions are.

**The device will not tell you where it ends.** `IOCTL_DISK_GET_LENGTH_INFO` on
a shadow copy device reports the whole partition, and reads past the end of the
filesystem inside it still fail. The filesystem's own boot sector is asked
instead.

**A shadow copy covers whole clusters, not whole sectors.** Asking the boot
sector was still not enough. A volume's own count of sectors can end part way
through a cluster, and the shadow copy stops at the last *complete* cluster. The
failure was a read at offset 67,423,432,704, which is exactly 16,460,799 × 4096:
one cluster short of what the filesystem claims. MjolnirVSS now reads at most
`total_clusters × bytes_per_cluster` through the snapshot and takes the rest
from the disk.

**Hard stopping a machine throws away what the guest just wrote.** The first
attempt to break a restore deliberately deleted the boot files, powered the
machine off hard, and found Windows booting quite happily: the deletion was
still in the guest's cache and never reached the virtual disk. A test that
writes inside a guest has to shut that guest down properly, with
`wpeutil shutdown` in Windows PE, before the result can be trusted.

**A restored disk's volumes already have drive letters.** Boot repair borrows a
letter so it can run `bcdboot`, and a volume cannot be given a second one.
Windows PE mounts what it finds, so repairing a disk restored earlier failed
with "no drive letter was free" — which is the ordinary case for a standalone
repair, and the only case it exists for. It now uses the letter a volume already
has, and gives back only the letters it borrowed.

**The recovery window could not be operated without a mouse.** Four separate
faults, each invisible until the thing was booted with no mouse to fall back on:

* the read only body was a multiline edit control, which tells Windows it wants
  every key, so once the keyboard reached it neither Tab nor Return could leave;
* the Back button was laid out underneath Next, leaving a sliver of it showing
  with no label, which Tab could still reach and Return would press, sending the
  operator backwards;
* Return pressed nothing, because a plain window is not a dialog and has to
  answer `DM_GETDEFID` itself before `IsDialogMessageW` will turn Return into a
  button press;
* and Windows PE starts the application from a command prompt that keeps the
  keyboard, so the window was on screen and deaf. It now asks for the foreground
  when it opens, which is reasonable for the only application on the machine.

**The browser decided what a partition held by reading a note.** The list of
volumes to browse came from the filesystem name Windows recorded at backup time.
Windows does not always record one: a volume it never mounted has no drive
letter and no reported filesystem, and the backup of it would have been listed
as unbrowsable even though the NTFS inside it reads perfectly. It now reads the
partition's first sector out of the backup and decides from that, which also
lets it say "this partition is BitLocker encrypted in the backup" instead of
failing to open something it was told was NTFS.

---

## Windows Server, and two things that waste an hour

Server 2025 installs from the same harness, with two differences worth knowing
before hunting for a fault that is not there.

**Setup asks whatever the answer file gets wrong, and looks like it is being
ignored.** When a setting cannot be applied, Setup quietly falls back to asking
that one question and carries on honouring the rest. Twice this looked like the
answer file being ignored wholesale, and both times it was one wrong value:

* the language pages appeared because the answer file asked for `en-GB`, which
  the Server and Windows 10 evaluation media do not contain. They are `en-us`;
* the image page appeared on Server 2019 because the name in the answer file did
  not match. Setup matches the image **name**, and `Get-WindowsImage` reports
  something else: the 2019 media's images are named `Windows Server 2019
  SERVERSTANDARD`, while `Get-WindowsImage` calls the same image `Windows Server
  2019 Standard Evaluation (Desktop Experience)`. Read the name off Setup's own
  list, not off the cmdlet.

Both are fixed in the answer files. If a page appears that should not, the
question Setup is asking is the setting that did not apply.

**Server Manager opens itself and takes the keyboard**, seconds after the first
logon, which swallows whatever is being typed at the time. Close it first.

**The Start menu offers the 32 bit PowerShell first.** Searching for
`powershell` on Server hands back *Windows PowerShell (x86)*, and a 32 bit shell
gets WOW64 file system redirection: `C:\Windows\System32` is not what it says
it is, so `vssadmin` and `reagentc` are not where the script looks. Start the
64 bit one explicitly through `$env:WINDIR\sysnative`.

---

## Windows does not agree with itself about junctions

Server 2019 reported a file missing that had in fact been recovered. The file
was `behind-the-junction.bin`, recovered under its real name in `target\`, and
reported missing under `junction\`, which is the same directory reached through
a junction.

The cause was in the harness, not the product. `Get-ChildItem -Recurse`
**descends into a junction on Windows 10 and Server 2019, and does not on
Windows 11 and Server 2025**, so the recorded markers differed by machine.
MjolnirVSS refuses to follow a junction when copying files out, and says so, and
is right to: following one copies a part of the volume nobody asked for.

`setup-guest.ps1` now prunes reparse points from the walk explicitly, so the
markers are the same set on every Windows. It is worth knowing that this class
of difference exists, because it looks exactly like a recovery bug.

---

## The phases

Each phase is a script the guest runs, reporting over the serial port.

| Phase | What it proves |
|---|---|
| `setup-guest.ps1` | The source machine exists, with files chosen to exercise fragmentation, sparse files, NTFS compression, Unicode names, alternate data streams, hard links and reparse points, each with a recorded hash |
| `backup-phase.ps1` | A live backup of a running Windows completes, verifies, and a damaged copy of it is refused |
| `files-phase.ps1` | Files come back out of the backup with the bytes they went in with, checked against the recorded hashes |
| `cancel-phase.ps1` | A real Ctrl+C during a real backup stops it at `9 cancelled`, releases the shadow copy, and leaves nothing marked complete |
| `bitlocker-phase.ps1` | Turns BitLocker on in the source machine, so the backup phases can be run again against an encrypted Windows |

The restore phase runs in the recovery machine, from the recovery media, and is
the one the whole thing exists for.

---

## What the harness has proven

On 13 September 2026, in one run:

| | |
|---|---|
| A live backup of a running Windows 11 | 62.8 GiB partition, 14.2 GiB read: 3,722,337 of 16,460,799 clusters across 17,345 extents. Verified, and a copy with one chunk removed was refused |
| Files out of that backup | 12 written, 3 correctly refused, every one checked against the hash taken when it was made |
| Recovery media | Built from this computer's own Windows parts, booted on UEFI firmware, the application drawn and driven from the keyboard |
| A restore onto a blank disk | 14.9 GiB written, four partitions, driven through the wizard with Tab and Enter |
| **The restored Windows starting** | **It started, unaided.** No boot repair was needed |
| Boot repair, by needing it | `\EFI\Microsoft` was deleted from the restored EFI partition; the machine then failed with `0xc000000f`, and `repair-boot --disk 0` is what made it start again |
| The recovery wizard with no mouse at all | From a cold boot of the media: one Enter searched the drives, Down chose a backup, Tab reached Next. Nothing was clicked. Getting there took four fixes, below |
| A BitLocker machine, all the way round | BitLocker turned on with a password and no TPM, fully encrypted; backed up live with used block imaging working through the shadow copy; restored onto a blank disk; **started with no password prompt**, because the restored volume is not encrypted; 12 of 12 files matched |
| The same backup onto a larger disk | Restored onto a blank 96 GiB disk from the command line: four partitions with the same type and unique identifiers, offsets and sizes as before, 32.0 GiB left unallocated at the end, 12 of 12 files matched, and it booted |
| **Ctrl+C during a real backup** | Interrupted 75 seconds in, with two shadow copies held. It exited `9 cancelled`, **both shadow copies were gone**, and the half written folder was not marked complete. This is what caught the flag being set by nothing at all |
| The alpha build, the whole cycle again | `0.1.0-alpha.1`, from a clean destination: 29,840,781,312 bytes captured from the Windows partition, 7,285,346 of 16,460,799 clusters, 18.28 GB stored in 884 seconds. 17,904 chunks verified over 24.9 GiB read back, and a copy with a chunk removed refused. 11 of 11 hashes matched out of the backup, one refused on purpose. Restored onto a blank disk from the wizard with the keyboard: 28.5 GiB written, four partitions. **It booted unaided**, and the restored machine matched 12 of 12 hashes with every partition's type, identifier, offset and size unchanged, `bootmgfw.efi` and the BCD in place, and Windows RE still registered at `harddisk0\partition4` |
| **Windows Server 2019, the whole cycle** | Build 17763, Desktop Experience. Backed up live in 166 seconds, 4.44 GB stored, verified, and a copy with a block missing refused. 11 of 11 hashes matched out of the backup. Restored from the wizard onto a blank disk: 14.6 GiB, four partitions. **It booted unaided**, and matched 12 of 12 hashes with the EFI partition intact and Windows RE registered. The oldest supported server and the newest behave identically |
| **Windows Server 2025, the whole cycle** | Server 2025 Standard Evaluation with the Desktop Experience, same four partition UEFI layout as a Windows 11 machine. Backed up live in 478 seconds, 2,900,789 of 16,460,799 clusters read, 6.10 GB stored, verified, and a copy with a block missing refused. 11 of 11 hashes matched out of the backup. Restored from the wizard onto a blank disk: 11.7 GiB, four partitions. **It booted unaided**, and the restored server matched 12 of 12 hashes with every partition identifier, offset and size unchanged and Windows RE still registered. It also proved the naming guard: Server 2025 is build 26100, the same as Windows 11 24H2, and was correctly **not** renamed to Windows 11 |
| The restored machine, checked | 12 of 12 files matched by hash; every partition kept its type GUID, unique GUID, offset and size; the EFI partition held `bootmgfw.efi`, `bootx64.efi` and the BCD; the boot entry named `winload.efi`; Windows RE was still registered at `harddisk0\partition4` |

The compressed file is worth a note. File recovery refuses it, because handing
back stored clusters would hand back something that is not the file. The whole
disk restore reproduces it exactly, because it does not need to understand it.
Both are in the numbers above: 12 files recovered individually with one refused,
and 12 of 12 correct after the restore.

---

## Throwing it away

The lab is disposable. Delete the lab root and the machines are gone; nothing
else on the computer was changed, and nothing was installed.
