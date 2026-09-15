# Changes

## 0.1.0-alpha.2

One theme: the interface was inconsistent with itself, and now it is not.

### One set of measurements

Every size in both windows used to be picked by hand where it was needed. There
were **nine different command button widths**, two different button heights, two
unrelated text box widths, and gaps of 2, 6, 8, 10, 12, 14, 16 and 22 units with
no relationship between them.

There is now one table of metrics and the windows contain no numbers at all:

- **Two button widths**, which is what Windows' own guidance asks for: one for a
  command and a wider one for the action a screen exists to perform.
- **One button height, one input height**, and the button beside a text box is
  centred on it rather than hung from the same top edge.
- **One spacing scale** of five steps, every one a multiple of four, which is
  what keeps a layout crisp at 125%, 150% and 175% display scaling.
- **One set of line boxes** for captions, body text, headings, titles and
  figures, so two pieces of text on one row share a baseline.

The metrics are checked when the code is compiled rather than when a test runs,
so a size that leaves the grid fails the build. Three did.

### Text that lines up

A Windows static control draws its text against the *top* of its box, so a
heading in a 24 unit box and a figure in a 40 unit box starting at the same
point visibly did not line up. Single line labels now centre their text in their
box and are cut with an ellipsis rather than drawn over their neighbour; blocks
of wrapped text are a separate kind that still flows from the top. Values at the
right of a card are right aligned, so they end on the card's padding.

### Fixed

- The Exit button on the recovery wizard's restoring step was drawn **on top of**
  the panel above it, with half the button hidden. Found by watching a real
  restore in Windows PE.
- A failed restore put its three paragraphs on screen as one run-on sentence. A
  message box accepts a bare newline; a plain edit control does not, and the
  same text goes to both.
- The wizard's explanation on the two list steps was a two line scrolling box
  repeating the line above it. The list now fills the step.

## 0.1.0-alpha.1

The first alpha. Backup, verification, restore and boot have been exercised end
to end on Windows 10 22H2, Windows 11 24H2, Windows Server 2019 and Windows
Server 2025: each was backed up while running, restored onto a blank disk from
recovery media MjolnirVSS built itself, and started unaided. The environments
those runs happened in are recorded in [`docs/testing.md`](docs/testing.md).

Hardware configurations vary. Test recovery in your own environment before
relying on any backup tool, and keep whatever backup you were using before.

### It can do the whole job

A Windows system disk can be copied while Windows runs, written back onto a
blank disk from recovery media MjolnirVSS built itself, and started.

- **Live backup** through the Volume Shadow Copy Service, capturing the GPT, the
  EFI system partition, the Microsoft Reserved partition, Windows, and the
  recovery partition. A partition is never silently left out.
- **Used block imaging.** Only the clusters NTFS says are in use are read:
  14.2 GiB out of a 62.8 GiB partition on the machine this was measured on. A
  volume that will not answer is copied whole, and the backup records that it
  had to.
- **Verification before a backup is called one.** Every stored block is read
  back, decompressed and checked against its BLAKE3 digest. Only then is the
  completion marker written.
- **Bare metal restore** onto a blank disk, from media MjolnirVSS builds out of
  the Windows recovery parts the computer already has. Done onto a disk of the
  same size and onto a larger one; both booted.
- **A window and a command line over one engine.** A navigation rail, a disk
  card showing the partition layout, a progress screen that names its stages,
  and a recovery wizard that can be driven from the keyboard alone. Native Win32
  throughout, so it works inside Windows PE, follows the system theme and text
  size, and reads correctly to a screen reader.
- **Boot repair** for a restored disk that will not start. Proven by breaking
  one deliberately until it failed with `0xc000000f`, repairing it, and starting
  it.
- **File recovery** without restoring anything: browse a backup and copy files
  out of it. Twelve of twelve test files came back matching the hashes taken
  when they were made.
- **Recovery media** built from this computer's own Windows recovery
  environment. Nothing belonging to Microsoft is shipped or downloaded.

### Encryption

Optional and off unless asked for. Argon2id, AES-256-GCM and BLAKE3 keyed mode,
all from RustCrypto, used in documented ways. Nothing invented.

Taken all the way round: an encrypted backup of a running Windows 11, restored
from recovery media and booted. The password is never stored, never accepted as
a command line argument, and a wrong one is refused immediately - **before the
target disk is touched**.

The recovery wizard asks for it on a step of its own. The backup window does not
offer encryption yet; the command line does.

### Safety

- A restore will not run without the disk's erase phrase typed exactly. It is
  not a compile time option to skip it.
- The disk holding the backup is refused as a restore target, and so is a disk
  too small to hold the layout.
- A disk that changed size between being checked and being written stops the
  restore.
- Every block is checked to be present **before** anything is erased.
- The destination is checked for room before the disk is read, rather than
  filling up an hour later.
- Ctrl+C releases the shadow copy and exits cleanly instead of abandoning it.
- MjolnirVSS never claims your restore points will survive. It warns that
  Windows may delete older ones and says it cannot prevent that.

### Driving it from a script

Every command takes `--json` and prints one document on standard output,
failures included, with a stable exit code inside it. Nothing waits for a person:
a password comes from a file or the environment, a restore takes `--confirm`, and
a command that would otherwise have to ask names the flag instead of hanging.

See [`docs/automation.md`](docs/automation.md).

### What is not here

- **Incremental backups.** Not started, deliberately, until the above is solid.
- **Encryption in the backup window.** Command line only.
- **Writing a USB stick directly.** It makes an ISO; write it with any tool that
  writes a bootable image.
- **Changing a backup's password**, which would mean rewriting every block.
- **Backing up anything but the system disk.** Other disks are detected and
  named in the plan so you know what is not being captured.

### Requirements

Windows 10, Windows 11 or Windows Server, 64 bit, UEFI, a GPT system disk, one
physical disk holding Windows, and an NTFS external drive with room. Dynamic disks, Storage Spaces,
software RAID, ReFS system volumes and legacy BIOS boot are refused with an
explanation rather than attempted.
