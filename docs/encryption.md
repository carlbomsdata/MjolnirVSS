# Encrypting a backup

Optional. Off unless you ask for it, and a backup taken without it is byte for
byte the backup it would have been before this existed.

```powershell
MjolnirVSS.exe backup --destination E:\Backups --encrypt
```

You are asked for the password twice. Nothing else changes: the same capture,
the same verification, the same restore.

---

## Nothing here was invented

| | |
|---|---|
| Password to key | **Argon2id**, through the `argon2` crate |
| Sealing | **AES-256-GCM**, through the `aes-gcm` crate |
| Naming blocks | **BLAKE3 keyed mode**, which BLAKE3 provides |
| Wiping keys | `zeroize` |

Two keys come out of the password, sixty-four bytes in one derivation: one seals
blocks, one names them. They are separate so that neither job can weaken the
other.

Settings today are 64 MiB of memory and three passes, written into the backup so
that a future version can still open it. A backup asking for something this
version will not do — too little memory, no passes, more memory than it will
allocate — is refused rather than obeyed.

---

## What is hidden, and what is not

**Hidden:** every byte of what was on the disk.

**Not hidden:** which machine the backup came from, when it was taken, how the
disk was laid out, how big each partition was, and roughly how much was stored.

That is deliberate. Somebody standing in front of a broken computer, in a
recovery environment, with an external drive holding three backups, has to be
able to tell which one is theirs before anybody can sensibly ask them for a
password. A backup that cannot say whose it is cannot be chosen in the one
situation it exists for.

So: an attacker holding your drive learns that you have a backup of
`DESKTOP-1A2B` from a particular Tuesday with a 62 GiB Windows partition. They
learn nothing about what is in it.

If that trade is wrong for you, do not put the drive where they can reach it.

### The names of the stored blocks

Blocks are content addressed, and an ordinary digest of the contents would be a
hole in this: anybody holding the drive could take a file they already have,
hash it, and see whether that name is present. They would learn what is in the
backup without opening anything.

So in an encrypted backup the names are a **keyed** digest. Without the password
they are meaningless, and identical blocks within one backup still share a name,
so nothing is stored twice.

---

## The password

**It is never stored.** Not in the backup, not in the registry, not beside it.
There is no recovery mechanism and there is not meant to be one, because a
backup that can be opened without the password is a backup anybody can open.

**If you lose it, the backup is lost.** MjolnirVSS says so before it starts, not
afterwards.

It is asked for twice when a backup is made. A password mistyped once is a
backup nobody can open, and nobody would find out until the day they needed it.

**It is never accepted as a command line argument.** Arguments are visible in
Task Manager, in the process list and in shell history, which would make it a
worse secret than no secret. For a scheduled run, use a file:

```powershell
MjolnirVSS.exe backup --destination E:\Backups --encrypt --password-file C:\keys\backup.txt
```

That file is then exactly as sensitive as the backup. The `MJOLNIRVSS_PASSWORD`
environment variable works too, with the same warning.

---

## Reading one back

The recovery wizard asks for the password on a step of its own, **before a disk
is chosen** — so a wrong password costs nothing, rather than being discovered
after the target had been erased. What is typed is shown as dots.

Every command that reads a backup asks too, when the backup has one, and does
not when it does not:

```powershell
MjolnirVSS.exe verify  E:\Backups\DESKTOP-1A2B_2026-09-12_1015
MjolnirVSS.exe volumes E:\Backups\DESKTOP-1A2B_2026-09-12_1015
MjolnirVSS.exe extract E:\Backups\... --volume disk-0-part-3 --item \Users --into D:\Out
```

A wrong password is answered **immediately**, by name. The backup carries a
check value — a fixed phrase sealed with the key — so a wrong password is caught
before any real work starts rather than surfacing as a failure to read some
block in the middle of a restore.

Opening a sealed backup with no password at all is also answered plainly: *"this
backup is encrypted and no password has been given"*. It would otherwise have
reported the compressed stream as damaged, which would send somebody to check a
drive that is perfectly healthy.

---

## What is checked

Every sealed block is authenticated. A single changed bit — anywhere, including
in the part that is not secret — means the block will not open at all, rather
than opening as something subtly wrong. There is no way to get part of a damaged
block back, and that is the point.

`tests/integration/tests/encrypted.rs` runs the whole cycle: an encrypted backup
of a synthetic disk, verified, restored, and compared byte for byte against the
disk it came from. It also checks the things a round trip alone would not:

- that the stored blocks do not contain recognisable plaintext, so a test that
  only checked round tripping could not pass with encryption switched off;
- that a block's name is not the plain digest of its contents;
- that another password does not open it;
- that a damaged sealed block is still caught;
- that the manifest still says whose backup it is, and still does not contain
  the password or any key material;
- and that an unencrypted backup is completely unaffected.

---

## What is not done yet

| | |
|---|---|
| **The backup window** | Encryption is reachable from the command line only. The window does not offer it, and does not pretend to |
| **Changing the parameters** | A backup is opened with the settings it was written with. There is no way to re-derive an existing backup's key with stronger ones short of taking a new backup |
| **Changing a password** | Would mean rewriting every block, since the key names them. Not built |
| **Real hardware** | Proven in virtual machines only, like everything else here |
| **Review by somebody else** | This has not been looked at by anyone but its author. Standard primitives used in documented ways is the floor, not a substitute |

---

## The whole cycle, done

On 13 September 2026, against a real Windows 11 machine in a virtual machine —
one that was itself BitLocker encrypted, so both kinds of encryption were in
play at once:

| | |
|---|---|
| The backup | `--encrypt` with `--password-file`. 26.3 GiB read, 17.0 GiB written, 21,456 chunks, 952 seconds. Verified with the keys it was written with, without asking again |
| The manifest | Records `aes-256-gcm` and `argon2id`. Searched for the password: **not present** |
| In the recovery environment | Listed as `[encrypted]` by both `find-backups` and the wizard |
| A wrong password | Refused **immediately**, by name, and **before the target disk was touched** |
| The right password | Opened it; the restore ran and rebuilt the boot configuration |
| Starting it | **It booted** |
| The files | 12 of 12 matched the hashes taken when they were made |

Then again through the **window**, with no command line at all: the wizard marked
the backup `[encrypted]`, asked for the password on a step of its own, showed
dots rather than the password, refused a wrong one and stayed put, took the right
one, and restored onto a blank disk. That machine booted too. Every key press was
the keyboard; nothing was clicked.

The order in that table is the point. The password is checked before anything is
erased, so getting it wrong costs you nothing.
