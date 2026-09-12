//! What MjolnirVSS refuses to do.
//!
//! A backup tool is judged by its failures, not its successes. These tests
//! damage backups in the ways a real drive damages them, and check that every
//! one is caught before it could reach a replacement disk. They also check the
//! refusals that protect the operator from themselves: erasing the wrong disk,
//! erasing the disk holding the backup, and restoring onto something too small.

mod common;

use std::fs;

use mjolnir_core::cancel::CancelToken;
use mjolnir_core::progress::SilentProgress;
use mjolnir_core::ExitCode;
use mjolnir_image::verify::{VerifyDepth, VerifyReport};
use mjolnir_image::BackupSet;
use mjolnir_restore::{EraseConfirmation, TargetDisk};
use mjolnir_testkit::{corrupt, FileBlockDevice, SyntheticDisk};

use common::{back_up, target_disk};

/// Verifies a backup folder and returns the report.
fn verify(dir: &std::path::Path) -> VerifyReport {
    let set = BackupSet::open_unchecked(dir).expect("the documents should still parse");
    let store = set.chunk_store();
    mjolnir_image::verify::verify(
        set.manifest(),
        Some(set.disk_layout()),
        &store,
        VerifyDepth::Full,
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .expect("verification should run")
}

fn make_backup(tmp: &tempfile::TempDir, name: &str) -> (SyntheticDisk, std::path::PathBuf) {
    let source = SyntheticDisk::windows_like(512);
    let dir = back_up(&source, tmp.path(), name).unwrap();
    (source, dir)
}

// ---- damage to the stored data -----------------------------------------

#[test]
fn a_corrupted_chunk_is_caught_by_verification() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, dir) = make_backup(&tmp, "CORRUPT_2026-01-01_1300");

    // A backup is fine until it is damaged.
    assert!(verify(&dir).passed());

    let chunks = corrupt::chunk_files(&dir).unwrap();
    assert!(!chunks.is_empty());
    corrupt::corrupt_chunk(&chunks[chunks.len() / 2]).unwrap();

    let report = verify(&dir);
    assert!(!report.passed(), "a flipped bit was not noticed");
    assert!(
        report.issues.iter().any(|i| i.is_error()),
        "{:?}",
        report.issues
    );
}

#[test]
fn a_truncated_chunk_is_caught_by_verification() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, dir) = make_backup(&tmp, "TRUNC_2026-01-01_1301");

    let chunks = corrupt::chunk_files(&dir).unwrap();
    corrupt::truncate_chunk(&chunks[0]).unwrap();

    assert!(!verify(&dir).passed(), "a truncated chunk was not noticed");
}

#[test]
fn a_missing_chunk_is_caught_by_verification() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, dir) = make_backup(&tmp, "MISSING_2026-01-01_1302");

    let chunks = corrupt::chunk_files(&dir).unwrap();
    corrupt::remove_chunk(&chunks[0]).unwrap();

    let report = verify(&dir);
    assert!(!report.passed(), "a missing chunk was not noticed");
    assert!(
        report
            .issues
            .iter()
            .any(|i| i.problem.contains("missing") || i.problem.contains("is missing")),
        "the report should say the chunk is missing: {:?}",
        report.issues
    );
}

#[test]
fn an_edited_manifest_is_caught_by_its_recorded_digest() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, dir) = make_backup(&tmp, "EDITED_2026-01-01_1303");

    // Change the computer name, the way somebody tidying up a backup might.
    corrupt::edit_document(&dir.join("manifest.json"), "SYNTHETIC-PC", "OTHER-PC").unwrap();

    let set = BackupSet::open_unchecked(&dir).unwrap();
    assert!(
        set.issues().iter().any(|i| i.is_error()),
        "an edited manifest should not pass: {:?}",
        set.issues()
    );
    // And the strict open refuses it outright.
    assert!(BackupSet::open(&dir).is_err());
}

// ---- interrupted backups ------------------------------------------------

#[test]
fn a_backup_without_its_completion_marker_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, dir) = make_backup(&tmp, "INCOMPLETE_2026-01-01_1304");

    corrupt::make_incomplete(&dir).unwrap();

    let err = BackupSet::open(&dir).unwrap_err();
    assert_eq!(err.exit(), ExitCode::CorruptBackup);
    assert!(
        err.what().contains("incomplete"),
        "the message should say so plainly: {}",
        err.what()
    );
    assert!(err.why().contains("completion.json"));
}

#[test]
fn an_interrupted_backup_never_looks_complete() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);

    // Simulate the drive being unplugged: capture, write the documents, and
    // stop before marking it complete. This is exactly what the writer does if
    // the process dies, because the completion marker is written last.
    use mjolnir_image::writer::{BackupWriter, WriterOptions};
    let name = mjolnir_core::ids::BackupName::new("INTERRUPTED_2026-01-01_1305").unwrap();
    let mut writer = BackupWriter::create(
        tmp.path(),
        &name,
        mjolnir_image::manifest::BackupInfo {
            uuid: "99999999-8888-7777-6666-555555555555".to_owned(),
            name: name.clone(),
            created_utc: "2026-01-01T13:05:00Z".to_owned(),
            kind: mjolnir_image::manifest::BackupKind::Full,
            scope: "system-disk".to_owned(),
        },
        mjolnir_image::manifest::SourceInfo {
            machine_id: mjolnir_core::ids::MachineId::new("synthetic-pc").unwrap(),
            computer_name: "SYNTHETIC-PC".to_owned(),
            windows: Default::default(),
            firmware: mjolnir_image::manifest::FirmwareMode::Uefi,
        },
        WriterOptions::default(),
    )
    .unwrap();

    let spec = common::capture_spec_for(
        &source,
        mjolnir_image::manifest::CaptureMethod::RawFull,
        None,
    );
    let mut sources = common::SyntheticSources::new(&source);
    mjolnir_backup::capture::capture_disk(
        &spec,
        &mut sources,
        &mut writer,
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .unwrap();

    let finalized = writer.finalize().unwrap();
    let dir = finalized.layout().dir().to_path_buf();
    drop(finalized); // no mark_complete: the run was interrupted here

    assert!(
        dir.join("manifest.json").is_file(),
        "the documents were written"
    );
    assert!(
        !dir.join("completion.json").exists(),
        "an interrupted run must not leave a completion marker"
    );

    let err = BackupSet::open(&dir).unwrap_err();
    assert_eq!(err.exit(), ExitCode::CorruptBackup);

    // And a restore will not touch it.
    let set = BackupSet::open_unchecked(&dir).unwrap();
    assert!(!set.is_restorable());
    let target = target_disk(1, source.size_bytes(), 512);
    assert!(mjolnir_restore::plan(&set, &target).is_err());
}

#[test]
fn a_backup_that_fails_verification_is_never_marked_complete() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);

    use mjolnir_image::completion::{Verification, VerificationResult};
    use mjolnir_image::writer::{BackupWriter, WriterOptions};
    let name = mjolnir_core::ids::BackupName::new("FAILED_2026-01-01_1306").unwrap();
    let writer = BackupWriter::create(
        tmp.path(),
        &name,
        mjolnir_image::manifest::BackupInfo {
            uuid: "99999999-8888-7777-6666-555555555555".to_owned(),
            name: name.clone(),
            created_utc: "2026-01-01T13:06:00Z".to_owned(),
            kind: mjolnir_image::manifest::BackupKind::Full,
            scope: "system-disk".to_owned(),
        },
        mjolnir_image::manifest::SourceInfo {
            machine_id: mjolnir_core::ids::MachineId::new("synthetic-pc").unwrap(),
            computer_name: "SYNTHETIC-PC".to_owned(),
            windows: Default::default(),
            firmware: mjolnir_image::manifest::FirmwareMode::Uefi,
        },
        WriterOptions::default(),
    )
    .unwrap();

    let finalized = writer.finalize().unwrap();

    // Marking it complete with a failed verification must be refused. This is
    // the single gate that keeps a bad backup from looking usable.
    let failed = Verification {
        result: VerificationResult::Failed,
        completed_utc: Some("2026-01-01T13:06:00Z".to_owned()),
        chunks_verified: 0,
        bytes_verified: 0,
        problems: vec!["chunk 3 failed its digest check".to_owned()],
    };
    let err = finalized
        .mark_complete(failed, "2026-01-01T13:06:00Z")
        .unwrap_err();
    assert_eq!(err.exit(), ExitCode::CorruptBackup);
    assert!(err.what().contains("not marked complete"));
    drop(source);
}

// ---- unsafe restore targets --------------------------------------------

#[test]
fn restoring_onto_the_disk_holding_the_backup_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (source, dir) = make_backup(&tmp, "SAFE_2026-01-01_1307");

    let set = BackupSet::open(&dir).unwrap();
    let mut target: TargetDisk = target_disk(1, source.size_bytes() * 2, 512);
    target.holds_the_backup = true;

    let err = mjolnir_restore::plan(&set, &target).unwrap_err();
    assert_eq!(err.exit(), ExitCode::UnsafeTarget);
    assert!(err.what().contains("holds the backup"));
}

#[test]
fn restoring_onto_a_disk_that_is_too_small_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (source, dir) = make_backup(&tmp, "SMALL_2026-01-01_1308");

    let set = BackupSet::open(&dir).unwrap();
    let target = target_disk(1, source.size_bytes() / 2, 512);

    let err = mjolnir_restore::plan(&set, &target).unwrap_err();
    assert_eq!(err.exit(), ExitCode::UnsafeTarget);
    assert!(err.what().contains("needs at least"));
}

#[test]
fn restoring_onto_a_disk_with_a_different_sector_size_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (source, dir) = make_backup(&tmp, "SECTOR_2026-01-01_1309");

    let set = BackupSet::open(&dir).unwrap();
    let target = target_disk(1, source.size_bytes() * 2, 4096);

    let err = mjolnir_restore::plan(&set, &target).unwrap_err();
    assert_eq!(err.exit(), ExitCode::UnsafeTarget);
    assert!(err.what().contains("4096 byte sectors"));
}

#[test]
fn a_restore_cannot_be_started_without_typing_the_erase_phrase() {
    let tmp = tempfile::tempdir().unwrap();
    let (source, dir) = make_backup(&tmp, "CONFIRM_2026-01-01_1310");

    let set = BackupSet::open(&dir).unwrap();
    let target = target_disk(1, source.size_bytes(), 512);
    let plan = mjolnir_restore::plan(&set, &target).unwrap();

    // A confirmation for a different disk must not authorise this one.
    let other = target_disk(2, source.size_bytes(), 512);
    let wrong = EraseConfirmation::check(&other, &other.erase_phrase()).unwrap();

    let mut device =
        FileBlockDevice::create(tmp.path().join("target.img"), source.size_bytes(), 512).unwrap();

    let err = mjolnir_restore::restore(
        &set,
        &plan,
        &target,
        &wrong,
        &mut device,
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .unwrap_err();

    assert_eq!(err.exit(), ExitCode::UnsafeTarget);
    assert_eq!(
        device.write_count(),
        0,
        "nothing may be written without the right confirmation"
    );
}

#[test]
fn a_damaged_backup_stops_the_restore_before_anything_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    let (source, dir) = make_backup(&tmp, "DAMAGED_2026-01-01_1311");

    // Damage a chunk after the backup was taken, the way a failing drive does.
    let chunks = corrupt::chunk_files(&dir).unwrap();
    corrupt::corrupt_chunk(&chunks[chunks.len() - 1]).unwrap();

    let set = BackupSet::open(&dir).expect("the documents are still intact");
    let target = target_disk(1, source.size_bytes(), 512);
    let plan = mjolnir_restore::plan(&set, &target).expect("planning reads no chunks");
    let confirmation = EraseConfirmation::check(&target, &target.erase_phrase()).unwrap();

    let mut device =
        FileBlockDevice::create(tmp.path().join("target.img"), source.size_bytes(), 512).unwrap();

    let err = mjolnir_restore::restore(
        &set,
        &plan,
        &target,
        &confirmation,
        &mut device,
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .unwrap_err();

    assert_eq!(err.exit(), ExitCode::CorruptBackup);
    assert!(
        err.next_step().contains("nothing has been written")
            || err.next_step().contains("Nothing has been written"),
        "the operator must be told the disk is untouched: {}",
        err.next_step()
    );
    assert_eq!(
        device.write_count(),
        0,
        "a damaged backup must not reach the target disk"
    );
    assert_eq!(
        device.read_all().unwrap(),
        vec![0u8; source.size_bytes() as usize],
        "the target must still be blank"
    );
}

// ---- the backup destination --------------------------------------------

#[test]
fn a_backup_folder_that_already_exists_is_not_written_into() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);

    let name = "TWICE_2026-01-01_1312";
    back_up(&source, tmp.path(), name).unwrap();

    // A second backup with the same name must be refused rather than mixed in
    // with the first.
    let err = back_up(&source, tmp.path(), name).unwrap_err();
    assert_eq!(err.exit(), ExitCode::Destination);
    assert!(err.what().contains("already exists"));
}

#[test]
fn the_chunk_store_holds_no_temporary_files_after_a_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, dir) = make_backup(&tmp, "CLEAN_2026-01-01_1313");

    let mut leftovers = Vec::new();
    let mut stack = vec![dir.clone()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.to_string_lossy().ends_with(".tmp") {
                leftovers.push(path);
            }
        }
    }
    assert!(
        leftovers.is_empty(),
        "a finished backup left temporary files behind: {leftovers:?}"
    );
}
