# Changes

## 0.1.0-alpha.1

The first alpha. Everything below has been run end to end in disposable
virtual machines. **Nothing has been tested on real hardware**, and no real
computer has been restored from a MjolnirVSS backup.

Treat this as something to try on a machine you can afford to lose, and keep
whatever backup you were using before.

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
- **Any evidence from real hardware.** Firmware differs, disks differ, and a
  virtual NVMe disk is not a Samsung one.

### Requirements

Windows 10 or 11, 64 bit, UEFI, a GPT system disk, one physical disk holding
Windows, and an NTFS external drive with room. Dynamic disks, Storage Spaces,
software RAID, ReFS system volumes and legacy BIOS boot are refused with an
explanation rather than attempted.
