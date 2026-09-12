# Security

## Reporting a vulnerability

Please report security problems privately, through GitHub's **Report a
vulnerability** button on the repository's Security tab, rather than by opening a
public issue.

Include what you did, what happened, and what you expected. If you have a
reproduction, a synthetic backup folder or a small disk image is worth more than
a description. **Do not send a backup taken from a real computer** — it contains
everything on that machine.

There is no bounty and no service level agreement. This is a project, not a
company.

---

## What counts as a vulnerability

Anything that could destroy data or misrepresent a backup:

- A backup accepted as valid when it is damaged, incomplete or altered.
- A path that writes outside the intended file or disk range: path traversal
  through a document, an offset that escapes a partition, an arithmetic overflow
  that becomes an out of bounds write.
- Restoring onto a disk that should have been refused, or a way to reach the
  destructive path without the erase confirmation.
- A crash or unbounded allocation from a malformed backup or partition table.
- Credentials, keys, file contents or personal filenames appearing in a log.
- The recovery application gaining a dependency Windows PE does not provide,
  which would stop it starting during a recovery.

Please also report anything that makes MjolnirVSS **claim** something it has not
established. Saying a backup is verified when it is not is as serious as
corrupting it.

---

## Known gaps

Stated here rather than waiting to be reported.

**Backups are not encrypted.** Anyone who can read the drive can read everything
on it. Encryption is on the roadmap, deliberately behind the restore path. Until
then, keep the backup drive where you would keep the computer, and consider
BitLocker To Go on the drive itself.

**Backups are not signed.** The digests in `completion.json` detect a damaged or
edited document, but somebody able to rewrite the whole folder could produce a
self consistent forgery. The realistic threat is corruption, not forgery, so
signing is not planned.

**MjolnirVSS runs as administrator.** It has to, to read a disk and create a
shadow copy. It is not a security boundary between users, and a machine already
compromised at that level can alter a backup as it is written.

**No restored computer has ever been booted.** The correctness of the restore
path is established by tests against virtual disks, not by a working machine.

The full picture is in [`docs/threat-model.md`](docs/threat-model.md), including
what MjolnirVSS deliberately does not defend against.

---

## What MjolnirVSS does not do

- It makes no network connections. There is no telemetry, no analytics, no update
  check and no account.
- It installs nothing: no service, no driver, no scheduled task, no registry
  values.
- It never reads, stores or logs a BitLocker recovery key.
- It never deletes a shadow copy it did not create.
- It never writes to a disk without the operator typing that disk's serial
  number.

---

## Supported versions

MjolnirVSS is pre release. Only the latest commit on `main` is supported. Once
there are releases, this section will say which ones receive fixes.
