# Getting single files back

Open a backup, look inside it, and copy what you need out. No restore, no
erasing anything, no drive letter.

---

## From the window

Press **Restore files**, choose the backup, choose the drive, browse, then
press **Copy out...**

The list shows folders first, then files, with the size of each and a note when
there is something worth knowing: a link, a compressed or encrypted file, or a
file with hidden streams. Double click or **Open** goes in, **Up** comes back
out.

## From a command prompt

```powershell
MjolnirVSS.exe volumes E:\Backups\DESKTOP-1A2B_2026-09-12_1015
MjolnirVSS.exe browse  E:\Backups\... --volume disk-0-part-3 --folder \Users\tobias
MjolnirVSS.exe extract E:\Backups\... --volume disk-0-part-3 --item \Users\tobias\Documents --into D:\Recovered
```

Add `--json` to any of them for output a script can read.

---

## What it will not do

**It is not a drive letter.** Mounting a backup as a Windows volume would need a
filesystem driver, and MjolnirVSS installs nothing. A read only browser and a
copy button covers what people actually need, which is getting a file back.

**It never modifies the backup.** Everything in the backup folder is opened read
only. The code that browses lives in its own crate which depends on nothing that
can write to a disk, so browsing a backup is not one mistyped argument away from
erasing one.

**It does not follow links by default.** A junction or a symbolic link in a
backup points at a path on the machine the backup came from. Following one while
writing somewhere else is how an extraction ends up outside the folder you
chose, so they are skipped and listed as skipped. `--follow-links` overrides it.

**It does not overwrite by default.** A file already in the destination is left
alone and reported. `--overwrite` overrides it.

---

## What is checked on the way out

Everything. Files come out through the chunk store, which decompresses each
stored chunk and compares its BLAKE3 digest before returning it. A backup with a
damaged chunk in it stops the copy with a message naming the chunk rather than
writing a file that is quietly wrong.

A file that is written is therefore the file that went in, or there is an error
saying otherwise. A half written file is deleted rather than left looking
finished.

---

## What cannot be read, and how it says so

| | |
|---|---|
| **NTFS compressed files** | Not read. Handing back the stored clusters would hand back something that is not the file, so it is skipped and named |
| **Encrypted files** (EFS) | Not read. The key lives in a Windows profile, not in the backup |
| **Junctions and symbolic links** | Skipped unless asked for, as above |
| **Sparse files** | Read correctly. The holes come out as zeros, which is what they are |
| **Alternate data streams** | Read, and written beside the file as `name.stream-<name>`, because not every destination can hold a stream |
| **Hard links** | Every name the file has appears in every folder that names it, which is what a hard link is |
| **Unicode names** | Kept exactly, including names outside the basic multilingual plane |

For a compressed file, restoring the whole disk gets it back exactly; the
restore path reproduces the volume byte for byte and does not need to understand
the contents.

---

## Paths out of a backup are not trusted

Every name comes from somebody else's filesystem. Joining one onto a folder is
the single place where "get my files back" can turn into "overwrite something
else", so it has its own module with its own tests
(`crates/mjolnir-files/src/safepath.rs`). Refused, with the reason:

- anything that would climb out of the chosen folder: `..`, an absolute path, a
  drive letter, a leading separator;
- a name Windows reserves for a device, like `CON` or `LPT1`, in any case and
  with any extension;
- a name ending in a dot or a space, which Windows strips, so `secret.txt.`
  cannot be used to land on `secret.txt`;
- a character Windows forbids, including the colon, which would otherwise write
  into an alternate data stream of an existing file.

A refused name is reported rather than repaired. A file whose name could not be
reproduced is something you should know about.

---

## How the tree is built

By walking the master file table from end to end and asking each record what its
parent is, rather than by walking the directory indexes.

That is slower to start, and it is the right trade:

- a file whose directory's index is damaged is still found, because the record
  knows its own parent;
- a file with several names in several directories appears in all of them.

For a volume with a few hundred thousand files this takes a few seconds, and it
happens once when the backup is opened. Browsing afterwards reads nothing.

The reader is in `crates/mjolnir-ntfs`, works through
`mjolnir_core::blockio::BlockSource`, and has no Windows in it, so it is tested
against synthetic volumes on any machine.

---

## What has been proven, and what has not

| | |
|---|---|
| Decoding file records, data runs, names, streams | Tested against synthetic structures, including every malformed shape |
| Reading a volume out of a backup | Tested against a synthetic volume built for it |
| Path safety | Tested against every refusal above |
| Reading a **real** Windows volume out of a **real** backup | **Not done yet.** The harness for it is built and described in [`vm-testing.md`](vm-testing.md) |
