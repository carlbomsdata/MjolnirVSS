# The MjolnirVSS backup format

Version 1.0.

This document describes the format completely enough to write an independent
reader. If something here is ambiguous, that is a bug in this document; please
report it.

---

## Principles

**A backup is a folder, not a file.** There is no container, no database and no
index that has to be rebuilt. A person can open the folder, read the JSON, copy
it somewhere else and delete it, using nothing but a file manager and a text
editor.

**Nothing is trusted.** A reader treats every document as hostile input. Offsets
and lengths are bounds checked, paths are validated before they are joined, and
decompression is bounded by the size the manifest declares.

**A backup is not a backup until it says so.** The completion marker is written
last, after every block and every other document is on stable storage and
verification has passed. Its absence means the run did not finish.

---

## Layout

```text
DESKTOP-1A2B_2026-09-12_1015/
├── manifest.json       what was captured, and where the bytes are
├── disk-layout.json    the physical disks and their partitions
├── completion.json     written last; without it the backup is incomplete
├── logs/
│   └── backup.log      what happened during the run
├── indexes/
│   └── volume-<id>.json   per volume file index, for file recovery
└── chunks/
    └── <blake3>.zst    one file per distinct block of data
```

The folder name is chosen by the operator and defaults to
`COMPUTERNAME_YYYY-MM-DD_HHMM`. It carries no meaning; everything a reader needs
is inside the documents.

---

## Common fields

Every document starts with the same block:

```json
{
  "format": {
    "name": "MjolnirVSS Image",
    "magic": "MJOLNIRVSS",
    "major": 1,
    "minor": 0,
    "min_reader_minor": 0,
    "document": "manifest"
  }
}
```

- `magic` is always `MJOLNIRVSS`. A file without it is not a MjolnirVSS document.
- `major` is the incompatible version. **A reader must refuse a document whose
  `major` it does not implement.**
- `minor` increases when optional fields are added.
- `min_reader_minor` is the lowest minor version that can read the document
  correctly. **A reader must refuse a document whose `min_reader_minor` is
  greater than its own minor version**, even if the major matches. This is how a
  future version can add something a reader must act on without older readers
  silently restoring something incomplete.
- `document` is `manifest`, `disk-layout`, `completion` or `volume-index`, and
  must match the file being read.

Unknown fields are ignored. That is what makes a minor version increase
backwards compatible.

---

## `manifest.json`

Describes what was captured and where the bytes went.

```json
{
  "format": { "...": "as above, document = manifest" },
  "tool":   { "product": "MjolnirVSS", "version": "0.1.0" },
  "backup": {
    "uuid": "3f2a1c94-...-a1b2c3d4e5f6",
    "name": "DESKTOP-1A2B_2026-09-12_1015",
    "created_utc": "2026-09-12T10:15:00Z",
    "kind": "full",
    "scope": "system-disk"
  },
  "source": {
    "machine_id": "desktop-1a2b-0fd74ec17a9c",
    "computer_name": "DESKTOP-1A2B",
    "windows": { "product_name": "Windows 11 Pro", "build": "26100",
                 "edition": "Professional", "architecture": "x64" },
    "firmware": "uefi"
  },
  "chunking":    { "algorithm": "fixed", "chunk_size": 4194304 },
  "compression": { "algorithm": "zstd", "level": 3 },
  "hash":        { "algorithm": "blake3", "digest_bytes": 32 },
  "chunk_store": { "kind": "local-directory", "root": "chunks", "fanout": 0 },
  "vss": { "used": true, "snapshot_set_id": "...", "context": "backup",
           "writers_succeeded": true, "writers": [], "snapshots": [ ... ] },
  "volumes": [ ... ],
  "streams": [ ... ],
  "chunks":  [ ... ],
  "required_restore_bytes": 2000409231360,
  "stats": { ... }
}
```

### `chunks`: the block table

```json
{ "hash": "af1349b9f5f9...3262", "uncompressed_size": 4194304, "compressed_size": 118273 }
```

- `hash` is the BLAKE3 digest of the **uncompressed** bytes, lowercase hex, 64
  characters. Hashing the uncompressed form makes a block's identity independent
  of the compression level.
- `uncompressed_size` is at most `chunking.chunk_size`.
- No digest appears twice. A repeated digest is a malformed manifest.

Segments refer to blocks by their **index in this array**, not by digest.

### `streams`: the captured byte spaces

A stream is a region of a disk that was captured, and the list of pieces that
cover it.

```json
{
  "id": "disk-0-part-3",
  "kind": "partition",
  "disk_id": "disk-0",
  "partition_id": "disk-0-part-3",
  "target_offset": 122683392,
  "length": 1998525956096,
  "capture": "vss-raw",
  "sparse_fill": "zero",
  "source": "partition 3 (Windows), every byte from a shadow copy",
  "segments": [
    { "offset": 0, "length": 4194304, "chunk": 0 },
    { "offset": 4194304, "length": 4194304, "chunk": 1 }
  ]
}
```

- `kind` is `disk-head`, `disk-tail` or `partition`.
- `target_offset` is where byte zero of the stream belongs **on the disk**.
- `length` is the logical length of the region.
- `segments[].offset` is measured **from the start of the stream**, not the disk.
- `segments[].length` always equals the referenced block's `uncompressed_size`.
- Segments are sorted ascending by `offset` and never overlap.
- Bytes no segment covers are defined by `sparse_fill`, which is always `zero`
  in version 1.

`capture` records how the bytes were read, and a reader **must** act on it:

| Value | Meaning | Gaps allowed | Restorable |
|---|---|---|---|
| `vss-used-blocks` | Allocated clusters only, from a shadow copy | yes | yes |
| `vss-raw` | Every byte, from a shadow copy | no | yes |
| `raw-full` | Every byte, read directly from the disk | no | yes |
| `preview` | Only the first part of the region | yes | **no** |

A `preview` stream exists so the pipeline can be exercised quickly. **A reader
must refuse to restore any backup containing one.** A stream declaring
`vss-raw` or `raw-full` whose segments do not cover its whole length is
malformed.

### `disk-head` and `disk-tail`

`disk-head` covers byte zero up to the first partition: the protective master
boot record, the primary GPT header and the partition entry array. `disk-tail`
covers the secondary GPT at the end of the disk.

**These are captured but are not replayed verbatim during a restore.** The GPT
records the disk's own size and the location of its backup copy, so writing the
source disk's table onto a larger disk would produce a table that disagrees with
the disk it is on. A restore rebuilds the table from `disk-layout.json` for the
target's geometry, preserving every GUID, offset, size, attribute and name. The
captured regions are kept so the original table can be inspected and compared.

### `volumes`

Descriptive only: drive letter, label, filesystem, cluster size, used space, and
the relative path of this volume's file index if one was written. The bytes are
carried by the stream belonging to the volume's partition.

---

## `disk-layout.json`

The physical facts, kept separate so a restore can show the operator what is
about to happen without parsing a block table with tens of thousands of entries.

```json
{
  "format": { "...": "document = disk-layout" },
  "backup_uuid": "3f2a1c94-...",
  "disks": [{
    "id": "disk-0",
    "disk_number": 0,
    "size_bytes": 2000398934016,
    "logical_sector_size": 512,
    "physical_sector_size": 4096,
    "partition_style": "gpt",
    "disk_guid": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
    "model": "KINGSTON SKC2500M82000G",
    "serial": "0026_B768_568C_6B45",
    "bus_type": "nvme",
    "partitions": [{
      "id": "disk-0-part-1",
      "number": 1,
      "type_guid": "c12a7328-f81f-11d2-ba4b-00a0c93ec93b",
      "unique_guid": "...",
      "name": "EFI system partition",
      "starting_offset": 1048576,
      "length": 104857600,
      "attributes": 0,
      "role": "efi-system",
      "filesystem": "FAT32"
    }]
  }]
}
```

`role` is MjolnirVSS's interpretation (`efi-system`, `microsoft-reserved`,
`windows`, `recovery`, `data`, `unknown`); `type_guid` is the authority.

GUIDs are written in canonical lowercase form with no braces.

---

## `completion.json`

Written last. Its presence is what makes the backup usable.

```json
{
  "format": { "...": "document = completion" },
  "backup_uuid": "3f2a1c94-...",
  "state": "complete",
  "completed_utc": "2026-09-12T10:47:13Z",
  "verification": {
    "result": "passed",
    "completed_utc": "2026-09-12T10:47:10Z",
    "chunks_verified": 48219,
    "bytes_verified": 202216800256,
    "problems": []
  },
  "documents": [
    { "path": "manifest.json",    "blake3": "...", "bytes": 5218844 },
    { "path": "disk-layout.json", "blake3": "...", "bytes": 2914 }
  ],
  "required_restore_bytes": 2000409231360
}
```

- `state` is `complete` or `failed`.
- `documents` carries the digest and size of every metadata document, so an
  edited or damaged `manifest.json` is caught when the backup is opened.
- A backup is restorable only when `state` is `complete` **and**
  `verification.result` is `passed`. Anything else must be refused.

---

## `chunks/`

One file per distinct block, named `<digest>.zst`, holding the Zstandard frame
of the block's contents. With `chunk_store.fanout` set to *n*, the file lives in
a subdirectory named after the first *n* hex characters of the digest; with
`fanout` 0 every file sits directly in `chunks/`.

Reading a block:

1. Open `chunks/<digest>.zst`.
2. Decompress, refusing to produce more than `uncompressed_size` bytes. This is
   what stops a malformed manifest from causing an unbounded allocation.
3. Check the decompressed length is exactly `uncompressed_size`.
4. Check the BLAKE3 digest of the decompressed bytes equals the filename.

Failing any of these means the backup is damaged.

Blocks are immutable and content addressed, so identical content is stored once
however many times it appears.

---

## Writing a backup: the ordering rules

A writer **must** follow this order, because it is what makes an interrupted run
safe:

1. Write each block to a uniquely named temporary file in its final directory,
   flush it to stable storage, then rename it onto its final name. The rename is
   the commit point; a reader only ever looks for the final name.
2. Write `manifest.json` and `disk-layout.json` the same way: temporary file,
   flush, rename.
3. Verify everything written.
4. Only if verification passes, write `completion.json` by the same
   temporary-file-then-rename route.

A process killed at any point leaves either nothing or a complete object, and
never a folder that claims to be a usable backup when it is not.

---

## Reading a backup: required checks

A conforming reader must reject a backup that fails any of these.

**Documents**
- Magic, major version, `min_reader_minor`, and document kind on each file.
- `backup_uuid` agrees across all three documents.
- Every document's digest matches what `completion.json` records.
- `completion.json` exists, `state` is `complete`, and verification passed.

**Block table**
- Every `uncompressed_size` is between 1 and `chunking.chunk_size`.
- No digest appears twice.

**Streams**
- Every segment: `offset + length` does not overflow and does not exceed the
  stream's length; `length` equals the referenced block's `uncompressed_size`;
  the block index is in range.
- Segments sorted ascending and non overlapping.
- A stream whose `capture` forbids gaps covers its whole length.
- No two streams on the same disk cover the same byte.

**Disks**
- `logical_sector_size` is 512 or 4096; the physical size is a multiple of it.
- Every partition lies inside the disk, is a whole number of sectors, and does
  not start at offset zero.
- Partitions do not overlap.

**Between documents**
- Every partition in `disk-layout.json` is carried by exactly one partition
  stream, whose offset and length match the partition's.
- Each disk has exactly one `disk-head` and one `disk-tail` stream.
- No stream is unreferenced.
- `required_restore_bytes` is at least what the layout needs.

**Before restoring**
- No stream's `capture` is `preview`.
- The target disk is at least `required_restore_bytes`.
- The target's sector size equals the source's.
- The target is not the disk holding the backup.

---

## Designed for, but not yet implemented

- **Shared block stores.** `chunk_store.root` is a field rather than a constant
  so a future minor version can point several backups of one machine at a store
  they share, which is what an incremental chain needs. Nothing else changes.
- **Used block imaging.** `vss-used-blocks` is defined and honoured by the
  verifier and the restore path; the backup engine does not produce it yet, so
  today's backups copy every byte of a volume.
- **File indexes.** `indexes/volume-<id>.json` is specified and validated; the
  backup engine does not write one yet.
