# Contributing

MjolnirVSS writes to disks. A bug here does not produce a wrong answer, it
produces a computer that will not start, or a backup that turns out to be
useless at the worst possible moment. The rules below follow from that.

---

## Building

```powershell
rustup target add x86_64-pc-windows-msvc
cargo build --workspace
cargo test --workspace
```

Needs stable Rust and the Visual Studio C++ build tools. The workspace is
configured to build for `x86_64-pc-windows-msvc` by default, which also keeps the
static C runtime flag off proc macros.

Before opening a pull request:

```powershell
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
```

---

## The rules that are not negotiable

**Never write to a physical disk in a test.** Restores are tested against
temporary files and virtual disks. `mjolnir-testkit` provides both. A test that
could erase the machine it runs on will not be merged.

**Never claim something that has not been tested.** If the code path works but
has not been exercised on real hardware, the documentation says so. The README
has a table of what is implemented, what is tested and what is neither, and it
stays honest. "It should work" is not a state that belongs in it.

**Never silently omit data.** If a partition cannot be read, the backup fails.
There is no path that produces a backup missing something without saying so.

**Never mark a backup usable without verification.** The completion marker is
written last, only after every block has been read back and checked. This is
enforced by the type system: `BackupWriter::finalize` hands back a
`FinalizedBackup`, and only that can be marked complete.

**All arithmetic on offsets, lengths, sectors and counts uses the checked helpers
in `mjolnir_core::math`.** Not `+`, not `as`. A wrapped number here becomes a
write at the wrong place on a disk.

**Treat every byte read from a backup or a disk as hostile.** Bounds check
offsets, validate paths before joining them, bound decompression by the declared
size. Even when it came from the operator's own machine, because a failing drive
produces the same shapes an attacker would.

**Every `unsafe` block carries a comment saying why it is sound.** Unsafe code is
confined to the crates that talk to Windows. `mjolnir-core`, `mjolnir-image` and
`mjolnir-testkit` are `#![forbid(unsafe_code)]` and should stay that way.

**The recovery application must never depend on `mjolnir-vss`.** `vssapi.dll` is
not in a base Windows PE image, and importing it would stop the recovery
application from starting during a recovery. `scripts/package.ps1` checks the
built binary's imports and fails if it appears.

---

## Errors

Every error carries three things:

```rust
Error::unsupported(
    "the system disk is a dynamic disk",           // what failed
    "a volume made of several regions cannot ...",  // why it matters
    "dynamic disks are not supported yet; ...",     // what to do next
)
```

There is no constructor that takes only a message. An `HRESULT` on its own never
reaches the screen: the failures that actually happen get translated into
sentences. Write them for somebody halfway through a recovery, not for yourself.

Exit codes live in `mjolnir_core::exit` and are a published contract. Add new
numbers; never reuse one for a different meaning.

---

## Tests

New behaviour comes with tests. In rough order of value:

1. **A test that tries to do the forbidden thing** and checks it is refused.
   Every safety rule in this project has one.
2. **A round trip**, where the output is compared against the input byte for
   byte.
3. **Property tests** for anything doing arithmetic or parsing.
4. **Unit tests** for the rest.

Test names say what is being asserted:
`a_damaged_backup_stops_the_restore_before_anything_is_written` rather than
`test_restore_3`.

`mjolnir-testkit` gives you real synthetic GPT disks, file backed block devices
that enforce the same bounds a disk does, and helpers that damage a backup the
way a failing drive does.

Tests that need administrator rights or touch real hardware are opt in behind an
environment variable and never run in CI. See [`docs/testing.md`](docs/testing.md).

---

## Comments

Explain **why**, not what. The code says what it does.

Worth a comment: an ordering that looks wrong but is not, a Windows behaviour
that surprised somebody, a check whose absence would be a bug rather than a
missing feature. There are several in this codebase where a live test found the
truth — the shadow copy device that stays openable for twenty seconds after the
service releases it, the writer state that is *not* `VSS_WS_STABLE` during a
successful backup — and those comments are worth more than the code around them.

Not worth a comment: restating the line below it.

---

## Git

This repository follows the owner's git guidelines.

- English, lowercase first letter, no trailing period, under about 70
  characters, normally no body.
- No `feat:` or `fix:` prefixes.
- One logical change per commit. A refactor, a fix and a dependency bump are
  three commits.
- Never `git add .` or `git add -A`. Stage explicit paths.
- Never commit build output, binaries, backup images, logs or secrets.
- Name any new dependency in the pull request, and check its licence is
  compatible with GPL-3.0-or-later.

---

## Licence

GPL-3.0-or-later. By contributing you agree your work is licensed under it.

New files carry the header the existing ones do.
