# How the shadow copy works

MjolnirVSS backs up a running Windows installation. It does that by asking the
Volume Shadow Copy Service for a frozen view of the disk and copying from that,
rather than reading a filesystem that is being written to underneath it.

This document describes what MjolnirVSS asks for, in what order, and what it does
when something goes wrong.

---

## Why not just read the disk

A plain sector by sector copy of a mounted, running Windows volume produces an
image whose parts come from different moments: the start of the copy may be
minutes older than the end. A filesystem is a set of structures that refer to
each other, so an image assembled from different moments can be internally
inconsistent in ways that are not visible until it is restored.

The shadow copy service exists to solve exactly this. It asks applications to
finish what they are doing, holds writes for a moment, takes a snapshot, and then
lets everything continue. Reads from the snapshot afterwards see the disk as it
was at that instant, however long the copy takes.

---

## The binding

`IVssBackupComponents` is the interface a backup program uses. The `windows`
crate ships the shadow copy *types* but not that interface, because Microsoft's
Win32 metadata does not describe `vsbackup.h` — it declares the interface as a
C++ class rather than through MIDL, so there is nothing for the generator to
read.

MjolnirVSS declares the interface itself, in `crates/mjolnir-vss/src/sys.rs`. It
is an ordinary COM interface deriving from `IUnknown`, so its binary layout is
the three `IUnknown` slots followed by its own methods in declaration order. That
order was taken from the Windows SDK header, is reproduced in a table beside the
struct so a reviewer can check it without reading the code, and is asserted by
tests. Slots MjolnirVSS does not call are typed as opaque pointers rather than as
callable signatures, so calling one by mistake does not compile.

No C++ bridge is involved. `vssapi.lib` is linked directly.

---

## The sequence

1. **Join a multi threaded COM apartment** and set the security level the
   service documents for a backup program. Calling `CoInitializeSecurity` twice
   in one process is refused with `RPC_E_TOO_LATE`, which is harmless and
   ignored: whoever called first has already set the process wide policy.

2. **Create the session** with `CreateVssBackupComponentsInternal`, then
   `InitializeForBackup`.

3. **Set the context to `VSS_CTX_BACKUP`.** This makes the snapshot *non
   persistent*: the service releases it when the session object is released,
   even if the process is killed. It is the first of two things that stop
   MjolnirVSS from leaving clutter on a machine.

4. **Set the backup state** to a full backup of bootable system state, with no
   component selection. MjolnirVSS captures whole volumes rather than
   application components.

5. **Gather writer metadata.** The metadata itself is not used, but the writers
   will not take part in the snapshot unless it is gathered. It is freed again
   immediately.

6. **Start a snapshot set** and **add every NTFS volume on the system disk** to
   it. One set for all of them, so everything in the backup comes from the same
   instant. A multi volume Windows installation restored from snapshots taken at
   different moments would not be consistent.

7. **Prepare for backup.** The writers flush and quiesce.

8. **Take the snapshot.** This is the moment writes are held. It lasts well under
   a second.

9. **Check writer status.** Every writer is asked how it got on. **If any writer
   failed, the backup stops.** A writer that failed has left its data in a state
   the snapshot may not represent correctly, and continuing would produce a
   backup that looks fine and is not.

   A healthy writer at this point reports `VSS_WS_WAITING_FOR_BACKUP_COMPLETE`,
   not `VSS_WS_STABLE`. Treating anything but stable as a failure would condemn
   every successful backup, and an early version of MjolnirVSS did exactly that
   until a live test caught it.

10. **Read the data** from the shadow copy devices, which have paths like
    `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy14`.

11. **Tell the writers the backup finished** with `BackupComplete`. Skipping this
    leaves applications believing a backup is still running.

12. **Delete the snapshot set**, by its own identifier.

---

## Leaving nothing behind

Two independent mechanisms, because this is the thing most likely to annoy
somebody who tries MjolnirVSS once:

- The snapshot is non persistent, so the service releases it when the session
  object is released — including when the process crashes.
- The session deletes its own snapshot set explicitly on every exit path, from a
  destructor, so it happens on success, on failure, on cancellation and while a
  panic is unwinding.

Deletion is **by snapshot set identifier**, which is the identifier the service
gave this session. A shadow copy created by Windows System Restore, File History
or another backup program is never in scope, and MjolnirVSS will not remove one.

There is a test for this. It runs two sessions at once, deletes one, and checks
the other's shadow copy is still there afterwards.

There is one subtlety worth recording. After the service has released a shadow
copy, its device path stays openable **inside the process that created it** for
a surprisingly long time — over twenty seconds in measurements on a real machine.
So "the device still opens" is not evidence of a leak, and the cleanup tests ask
the service what it knows about rather than probing the device.

---

## Cancellation

Every wait on an asynchronous operation is a bounded half second loop that checks
the cancellation flag, rather than an infinite wait. Ctrl+C or the Cancel button
is noticed within about half a second, the operation is cancelled, and the
snapshot is released on the way out. Killing the process outright would leave the
snapshot until the service timed it out, which is why cancellation sets a flag
rather than terminating anything.

---

## When it fails

The failures that actually happen are translated into sentences rather than
shown as a code:

| What the service reports | What the operator is told |
|---|---|
| `E_ACCESSDENIED` | The process is not running as administrator, and how to fix that |
| `VSS_E_SNAPSHOT_SET_IN_PROGRESS` | Another program is creating a shadow copy; Windows allows one at a time |
| `VSS_E_INSUFFICIENT_STORAGE` | Not enough room for the shadow copy storage area; free space on the system disk |
| `VSS_E_VOLUME_NOT_SUPPORTED` | The volume cannot be shadow copied, usually because it is not NTFS or is locked |
| `VSS_E_FLUSH_WRITES_TIMEOUT`, `VSS_E_HOLD_WRITES_TIMEOUT` | The system was too busy to pause writes; close programs writing heavily |
| `VSS_E_WRITER_INFRASTRUCTURE`, `VSS_E_WRITER_NOT_RESPONDING` | A Windows component did not respond; restart, and how to find which one |
| Anything else | The code, plus where to look in the event log |

Every one of these carries what failed, why it matters and what to do next.

---

## Testing it

The shadow copy path cannot be mocked into meaning anything, so it is tested
against the real service, opt in:

```powershell
$env:MJOLNIR_VSS_LIVE = "1"
cargo test -p mjolnir-vss --test live_snapshot -- --nocapture --test-threads=1
```

from an elevated prompt. These tests are skipped unless that variable is set,
because they need administrator rights and briefly change system state. They
never write to a disk and never touch a shadow copy they did not create.

They check that a shadow copy can be created, that an NTFS boot sector can be
read from it, that reading the same range twice gives identical bytes, that every
writer succeeded, that cleanup removes it, that dropping a session without
completing removes it, and that one session's cleanup leaves another session's
shadow copy alone.
