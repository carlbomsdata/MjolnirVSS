# BitLocker

MjolnirVSS backs up a BitLocker protected Windows volume, provided the volume is
unlocked. This document says what that means, how it was established, and what
it does **not** mean.

---

## The short version

| | |
|---|---|
| Unlocked BitLocker volume | **Backed up.** The shadow copy presents it decrypted, so the backup holds an ordinary NTFS filesystem. |
| Locked BitLocker volume | **Refused**, with an explanation. Nothing can read it, so there is nothing to copy. |
| What the backup contains | **Readable, decrypted files.** Not ciphertext. |
| Is the backup encrypted? | **No.** Neither by BitLocker nor by MjolnirVSS. |
| Does a restored disk come back encrypted? | **No.** See [after a restore](#after-a-restore). |
| Does MjolnirVSS touch the recovery key? | **Never.** It is not read, derived, stored or logged. |
| Does MjolnirVSS change BitLocker's state? | **No.** It does not suspend, disable or decrypt anything. |

---

## Why this needed establishing rather than assuming

A volume is read through a shadow copy. If BitLocker sat *above* the shadow copy
in the storage stack, the shadow copy would hand back ciphertext, and a backup
taken from it would be worthless without key material MjolnirVSS deliberately
does not handle. If BitLocker sits *below*, the shadow copy hands back the
decrypted filesystem and an ordinary backup works.

Those two possibilities produce opposite products, and the difference is not
visible from the outside. Guessing would have been indefensible, so it was
measured.

---

## The measurement

`MjolnirVSS.exe diagnose-bitlocker` reads the same volume from two places and
compares them. It writes nothing, and it releases the shadow copy it creates.

1. **The partition's first sector, read from the physical disk.** This is below
   the encryption filter. A BitLocker volume carries `-FVE-FS-` here where a
   plain NTFS volume carries `NTFS`.
2. **The same volume's first sector, read through a shadow copy device.** This
   is above the encryption filter.

If the first says BitLocker and the second says NTFS, the shadow copy is
presenting decrypted data.

Eight bytes are weak evidence on their own, so the diagnostic goes further. It
parses the boot sector and checks every field against the others, then follows
the boot sector's own pointer to the master file table and checks that a file
record really is there, and does the same for the mirror at the other end of the
volume. Ciphertext does not satisfy several independent structural checks at
once.

### Result on the development machine

Run on 2026-09-12 against a Windows 11 machine whose C: drive is fully encrypted
with XTS-AES 128, protection on:

```text
Volume:            \\?\Volume{12c957bd-eeab-4a25-a25a-21f437bf5179}\
Drive letter:      C:
Partition offset:  122683392
On the disk:       BitLocker encrypted
Windows reports:   NTFS
Encryption:        BitLocker, unlocked
Shadow copy:       \\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy29
Through the copy:  NTFS

NTFS structures read through the shadow copy
  bytes per sector:     512
  sectors per cluster:  8
  cluster size:         4096 bytes
  total sectors:        3905323007
  master file table at: cluster 786432 (byte 3221225472)
  file record present:  yes
  mirror record:        yes
```

The same volume reports `-FVE-FS-` on the disk and a complete, self consistent
NTFS filesystem through the shadow copy. Two independent structures agree, so
this is not a coincidental signature match.

Reproduce it with:

```powershell
MjolnirVSS.exe diagnose-bitlocker
```

Exit code 0 means the volume can be backed up; 4 means it cannot, and the report
says why.

---

## How MjolnirVSS tells locked from unlocked

An unlocked BitLocker volume is indistinguishable from plain NTFS when asked
through the filesystem, because `GetVolumeInformationW` reports `NTFS` for both.
Checking there alone would miss BitLocker entirely, which is why MjolnirVSS
reads the partition itself.

Combining the two answers separates the three states:

| First sector on the disk | Filesystem Windows reports | Conclusion |
|---|---|---|
| `NTFS` | NTFS | not encrypted |
| `-FVE-FS-` | NTFS | **BitLocker, unlocked** |
| `-FVE-FS-` | none, or RAW | **BitLocker, locked** |

A locked volume is refused, because Windows itself cannot see inside it: a
backup would be empty rather than merely encrypted. The message says to unlock
the drive in Windows, and states that MjolnirVSS never asks for a recovery key.

---

## What this costs you, stated plainly

**The backup contains readable copies of your files.** Everything BitLocker was
protecting on the disk is, in the backup, sitting unencrypted on whatever drive
you chose.

This is not a flaw in MjolnirVSS so much as a consequence of what a backup is,
and every image based backup tool that supports BitLocker has the same property.
But it is a genuine change in your security position, and it is easy not to
notice, so MjolnirVSS says so in three places:

- the destination screen, where the drive is chosen, which is the only place it
  changes what you would do;
- the plan summary and the backup log;
- the diagnostic report.

Until MjolnirVSS supports encrypted backups, the practical answers are to keep
the backup drive somewhere you would keep the computer, or to put BitLocker To
Go on the backup drive itself, which works today and is outside MjolnirVSS.

---

## After a restore

**A restored disk is not encrypted.**

What is captured is the decrypted filesystem, so what is written back is a plain
NTFS volume. The restored machine boots without BitLocker, and BitLocker can be
turned on again afterwards from Windows, which re-encrypts in the background.

MjolnirVSS does not claim to preserve BitLocker, and will not until a restored
machine has actually been booted and checked. The restored disk will also not
unlock against the original machine's TPM, because the encryption is simply not
there any more.

That claim is deliberately absent from the README and from the product, rather
than being asserted and hedged.

---

## What was not used, and why

**No key material of any kind.** The detection reads one 512 byte sector and
compares eight bytes of it. It does not call the BitLocker unlock interfaces,
does not enumerate protectors, and does not touch the TPM.

**The recovery key is never read, stored or logged.** There is no code path in
MjolnirVSS that obtains one.

**BitLocker's state is never changed.** Nothing suspends, disables or decrypts a
volume. The development machine's drive was still `Fully Encrypted, Protection
On` after every test run.

---

## Still unproven

The measurement above establishes that the data captured from an unlocked
BitLocker volume is an ordinary NTFS filesystem. It does **not** establish that a
machine restored from such a backup boots. That needs the virtual machine boot
test, which has not been run.

Until it has, a backup of a BitLocker machine is in exactly the same position as
a backup of an unencrypted one: verified as internally sound, not yet proven
restorable.
