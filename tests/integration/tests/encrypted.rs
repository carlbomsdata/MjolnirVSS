//! An encrypted backup, all the way round.
//!
//! The same synthetic disk, the same capture engine, the same verifier and the
//! same restore engine as every other test here — with sealing switched on.
//! What these check is that encryption is a property of how blocks are stored
//! and nothing else: a restored disk has to come back byte for byte identical
//! to the one that was backed up, or encryption has quietly broken the product
//! it was added to.

mod common;

use common::*;
use mjolnir_core::blockio::BlockSource;
use mjolnir_core::cancel::CancelToken;
use mjolnir_core::progress::SilentProgress;
use mjolnir_core::ExitCode;
use mjolnir_image::verify::{VerifyDepth, VerifyReport};
use mjolnir_image::BackupSet;
use mjolnir_restore::EraseConfirmation;
use mjolnir_testkit::{FileBlockDevice, SyntheticDisk};

const PASSWORD: &str = "a password nobody would guess";

/// The whole point: an encrypted backup restores to the same bytes.
#[test]
fn an_encrypted_backup_restores_byte_for_byte() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);
    let dir = back_up_encrypted(&source, tmp.path(), "ENC_2026-01-01_1400", PASSWORD).unwrap();

    let mut set = BackupSet::open(&dir).unwrap();
    assert!(set.is_encrypted(), "this backup should be encrypted");
    assert!(!set.is_unlocked(), "and locked until a password is given");
    set.unlock(PASSWORD).expect("the password should open it");

    let target = target_disk(1, source.size_bytes(), 512);
    let plan = mjolnir_restore::plan(&set, &target).unwrap();
    let confirmation = EraseConfirmation::check(&target, &target.erase_phrase()).unwrap();
    let mut device =
        FileBlockDevice::create(tmp.path().join("restored.img"), source.size_bytes(), 512).unwrap();

    mjolnir_restore::restore(
        &set,
        &plan,
        &target,
        &confirmation,
        &mut device,
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .expect("an unlocked encrypted backup should restore");

    let mut restored = vec![0u8; source.size_bytes() as usize];
    device.read_exact_at(0, &mut restored).unwrap();
    assert_eq!(
        restored, source.bytes,
        "the restored disk must be the disk that was backed up"
    );
}

/// Without the password there is nothing to be had, and the refusal says so
/// plainly rather than looking like damage.
#[test]
fn without_the_password_nothing_comes_out() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);
    let dir = back_up_encrypted(&source, tmp.path(), "ENCLOCK_2026-01-01_1401", PASSWORD).unwrap();

    let set = BackupSet::open(&dir).unwrap();
    let store = set.chunk_store();
    let first = set
        .manifest()
        .chunks
        .first()
        .expect("the backup holds chunks");

    let err = store.get(first.hash, first.uncompressed_size).unwrap_err();
    // Not corruption: a missing password. Reporting it as damage would send
    // somebody to check their drive for a fault that is not there.
    assert_eq!(err.exit(), ExitCode::Failure);
    assert!(
        err.what().contains("no password has been given"),
        "the message should name the real problem: {}",
        err.what()
    );
    assert!(
        err.next_step().contains("password"),
        "and should say what to do: {}",
        err.next_step()
    );
}

/// The wrong password is answered at once, by name, not after a long read.
#[test]
fn the_wrong_password_is_refused_when_it_is_given() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);
    let dir = back_up_encrypted(&source, tmp.path(), "ENCWRONG_2026-01-01_1402", PASSWORD).unwrap();

    let mut set = BackupSet::open(&dir).unwrap();
    let err = set.unlock("not the password").unwrap_err();
    assert!(
        err.what().contains("does not open"),
        "the message should name the problem: {}",
        err.what()
    );
    assert!(!set.is_unlocked());
}

/// An encrypted backup still verifies, end to end, once unlocked.
#[test]
fn an_encrypted_backup_verifies() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);
    let dir = back_up_encrypted(&source, tmp.path(), "ENCVER_2026-01-01_1403", PASSWORD).unwrap();

    let mut set = BackupSet::open(&dir).unwrap();
    set.unlock(PASSWORD).unwrap();

    let report: VerifyReport = mjolnir_image::verify::verify(
        set.manifest(),
        Some(set.disk_layout()),
        &set.chunk_store(),
        VerifyDepth::Full,
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .expect("verification should run");
    assert!(report.passed(), "{:?}", report.issues);
}

/// Damage is still caught, and caught by the seal rather than by the digest,
/// which means it is caught before anything is decompressed.
#[test]
fn a_damaged_encrypted_chunk_is_still_caught() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);
    let dir = back_up_encrypted(&source, tmp.path(), "ENCDMG_2026-01-01_1404", PASSWORD).unwrap();

    let chunks = mjolnir_testkit::corrupt::chunk_files(&dir).unwrap();
    mjolnir_testkit::corrupt::corrupt_chunk(&chunks[0]).unwrap();

    let mut set = BackupSet::open(&dir).unwrap();
    set.unlock(PASSWORD).unwrap();
    let report: VerifyReport = mjolnir_image::verify::verify(
        set.manifest(),
        Some(set.disk_layout()),
        &set.chunk_store(),
        VerifyDepth::Full,
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .expect("verification should run and report");
    assert!(!report.passed(), "a damaged chunk must fail verification");
}

/// The drive must not carry the data in the clear, and a test that only checks
/// round tripping would pass even if nothing were encrypted at all.
#[test]
fn the_backup_folder_does_not_hold_the_data_in_the_clear() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);
    let dir =
        back_up_encrypted(&source, tmp.path(), "ENCOPAQUE_2026-01-01_1405", PASSWORD).unwrap();

    // The synthetic disk fills its partitions with recognisable content. None
    // of it may appear anywhere in the stored chunks.
    let needle = b"MJOLNIR";
    let mut looked_at = 0usize;
    for chunk in mjolnir_testkit::corrupt::chunk_files(&dir).unwrap() {
        let bytes = std::fs::read(&chunk).unwrap();
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle),
            "{} holds recognisable plaintext",
            chunk.display()
        );
        looked_at += 1;
    }
    assert!(looked_at > 0, "there should be chunks to look at");
}

/// And the manifest must stay readable, so a backup can be identified in a
/// recovery environment before anybody is asked for a password.
#[test]
fn an_encrypted_backup_still_says_whose_it_is() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);
    let dir = back_up_encrypted(&source, tmp.path(), "ENCNAME_2026-01-01_1406", PASSWORD).unwrap();

    let set = BackupSet::open(&dir).unwrap();
    assert_eq!(set.manifest().source.computer_name, "SYNTHETIC-PC");
    assert!(set.is_encrypted());
    assert_eq!(
        set.manifest()
            .encryption
            .as_ref()
            .map(|e| e.cipher.as_str()),
        Some("aes-256-gcm")
    );

    // What it must NOT say is anything that helps open it.
    let manifest = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
    assert!(
        !manifest.to_lowercase().contains(PASSWORD),
        "the password must never be written down"
    );
    assert!(
        !manifest.contains("\"key\"") && !manifest.contains("sealing"),
        "no key material belongs in the manifest"
    );
}

/// An unencrypted backup must be completely unaffected by encryption existing.
#[test]
fn an_unencrypted_backup_is_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);
    let dir = back_up(&source, tmp.path(), "PLAIN_2026-01-01_1407").unwrap();

    let set = BackupSet::open(&dir).unwrap();
    assert!(!set.is_encrypted());
    assert!(set.is_unlocked(), "nothing to unlock");
    assert!(set.manifest().encryption.is_none());

    // And the word does not appear in the document at all, so a backup written
    // by this version is the document it would have been before.
    let manifest = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
    assert!(
        !manifest.contains("encryption"),
        "an unencrypted manifest should not mention encryption"
    );
}
