# Getting single files back

**Not implemented.** The **Restore files** button in the main window says so.

The format supports it and the schema is specified and validated. What is missing
is the NTFS reader that fills the index in, and the browser that shows it. This
document describes the design so the shape of the work is clear.

---

## What it will do

Open a backup, show the volumes inside it, browse the folders read only, select
files or folders, and extract them somewhere else.

Explicitly **not** a drive letter. Mounting a backup as a Windows volume would
need a filesystem driver, and MjolnirVSS installs nothing. A read only browser
and an extract button covers what people actually need, which is getting a file
back after deleting it.

**The backup is never modified while browsing it.** Every file in it is opened
read only, and the extract path writes only to the destination the operator
chose.

---

## Why an index

A backup holds the raw bytes of an NTFS volume. Finding a file means parsing the
master file table, which for a large volume means reading a lot of blocks. Doing
that every time somebody opens a folder would make browsing unusable.

So the index is built once, during the backup, and stored beside the data:

```text
indexes/volume-<id>.json
```

It records, for every file and folder: its name, its parent, its size, when it
was last written, and **where its contents are inside the captured volume**. With
that, opening a folder costs a JSON parse, and extracting a file costs only the
blocks that actually hold it.

The schema is in [`backup-format.md`](backup-format.md) and implemented in
`crates/mjolnir-image/src/index.rs`, with validation and tests already written.

### The index is an accelerator, never an authority

Extraction always reads the blocks through the normal path, which checks each one
against its checksum. A damaged or forged index can make MjolnirVSS fail to find
a file; it cannot make it hand back the wrong bytes as though they were right.

The validator refuses an index whose entries point outside the captured volume,
whose data runs are longer than the file they belong to, whose parent chain
loops, or whose names are really paths. Those are all tested.

---

## What has to be built

1. **An NTFS reader** (`crates/mjolnir-ntfs`, currently empty). Enough to parse
   the boot sector, the master file table, `$MFT` records, the `$I30` directory
   indexes and non resident data runs. It reads from a `BlockSource`, so it can
   be tested against synthetic volumes with no Windows involved.
2. **Index generation** during a backup, writing the document and recording its
   digest in `completion.json` like the other documents.
3. **The browser**, a tree and a list in the main window, reading only the index.
4. **Extraction**, reading the blocks the index names, checking each one, and
   writing the file out.

Step 1 is most of the work and is also what used block imaging needs, so the two
arrive together. Today's backups copy every byte of a volume including free
space; reading the NTFS allocation bitmap is what lets MjolnirVSS skip the free
space, and that makes backups substantially smaller and faster.

---

## What will not be supported at first

- **Alternate data streams.** Only the main contents of a file.
- **Resident files.** A file small enough for NTFS to store inside its own record
  has no data runs. The format records this honestly by giving such an entry no
  runs, rather than pretending it is empty.
- **Compressed, sparse and encrypted files.** NTFS compression and EFS need
  decoding that is not planned yet. Such a file will be listed and marked as not
  extractable, rather than extracted incorrectly.
- **Permissions and ownership.** Extracted files get the permissions of wherever
  they are written. Getting the contents back is the point; reproducing an access
  control list from another machine mostly produces files the operator cannot
  open.
- **FAT32 volumes.** The EFI system partition is captured and restored, but it is
  not browsable. There is nothing in it a person wants to recover individually.

---

## Until then

A full restore into a virtual machine gets a file back. Restore the backup onto
a blank virtual disk, start the virtual machine, and copy the file out. It is
slow and clumsy compared to a browser, but it works today and it uses the path
that has the most testing behind it.
