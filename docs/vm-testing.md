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

**The browser decided what a partition held by reading a note.** The list of
volumes to browse came from the filesystem name Windows recorded at backup time.
Windows does not always record one: a volume it never mounted has no drive
letter and no reported filesystem, and the backup of it would have been listed
as unbrowsable even though the NTFS inside it reads perfectly. It now reads the
partition's first sector out of the backup and decides from that, which also
lets it say "this partition is BitLocker encrypted in the backup" instead of
failing to open something it was told was NTFS.

---

## The phases

Each phase is a script the guest runs, reporting over the serial port.

| Phase | What it proves |
|---|---|
| `setup-guest.ps1` | The source machine exists, with files chosen to exercise fragmentation, sparse files, NTFS compression, Unicode names, alternate data streams, hard links and reparse points, each with a recorded hash |
| `backup-phase.ps1` | A live backup of a running Windows completes, verifies, and a damaged copy of it is refused |
| `files-phase.ps1` | Files come back out of the backup with the bytes they went in with, checked against the recorded hashes |

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
| A BitLocker machine, all the way round | BitLocker turned on with a password and no TPM, fully encrypted; backed up live with used block imaging working through the shadow copy; restored onto a blank disk; **started with no password prompt**, because the restored volume is not encrypted; 12 of 12 files matched |
| The same backup onto a larger disk | Restored onto a blank 96 GiB disk from the command line: four partitions with the same type and unique identifiers, offsets and sizes as before, 32.0 GiB left unallocated at the end, 12 of 12 files matched, and it booted |
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
