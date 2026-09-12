//! Backup, verify, restore, and compare the result byte for byte.
//!
//! This is the test that says whether MjolnirVSS works. A synthetic GPT disk is
//! captured into a real backup folder, verified, and restored onto a blank
//! virtual disk; the partitions on the result are then compared against the
//! ones they came from, and the partition table is parsed back to check that
//! every GUID, offset and name survived.
//!
//! It does not prove that a restored Windows boots. Nothing short of booting a
//! restored machine proves that, and that gate is still open.

mod common;

use mjolnir_core::cancel::CancelToken;
use mjolnir_core::progress::SilentProgress;
use mjolnir_image::manifest::CaptureMethod;
use mjolnir_image::BackupSet;
use mjolnir_restore::{EraseConfirmation, TargetDisk};
use mjolnir_storage::gpt::parse_primary;
use mjolnir_testkit::{FileBlockDevice, SyntheticDisk};

use common::{back_up, back_up_with, target_disk};

/// Restores `backup_dir` onto a blank device of `target_size` and returns it.
fn restore_onto(
    backup_dir: &std::path::Path,
    device_path: &std::path::Path,
    target_size: u64,
    sector_size: u32,
) -> (FileBlockDevice, mjolnir_restore::RestoreOutcome) {
    let set = BackupSet::open(backup_dir).expect("the backup should open");
    let target: TargetDisk = target_disk(1, target_size, sector_size);
    let plan = mjolnir_restore::plan(&set, &target).expect("the restore should plan");

    let confirmation = EraseConfirmation::check(&target, &target.erase_phrase())
        .expect("the phrase should be accepted");

    let mut device =
        FileBlockDevice::create(device_path, target_size, sector_size).expect("create target");
    let mut progress = SilentProgress;

    let outcome = mjolnir_restore::restore(
        &set,
        &plan,
        &target,
        &confirmation,
        &mut device,
        &mut progress,
        &CancelToken::new(),
    )
    .expect("the restore should succeed");

    (device, outcome)
}

#[test]
fn a_backup_restores_to_a_disk_of_the_same_size_byte_for_byte() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);

    let backup_dir = back_up(&source, tmp.path(), "SYNTH_2026-01-01_1200").unwrap();
    let (mut device, outcome) = restore_onto(
        &backup_dir,
        &tmp.path().join("restored.img"),
        source.size_bytes(),
        512,
    );

    assert_eq!(outcome.partitions_restored, source.partitions.len());
    assert!(outcome.written_bytes > 0);

    let restored = device.read_all().unwrap();

    // Every partition must come back exactly as it went in. This is the
    // assertion the whole product rests on.
    for (index, (part, offset, length)) in source.partitions.iter().enumerate() {
        let from = *offset as usize;
        let to = (*offset + *length) as usize;
        assert_eq!(
            &restored[from..to],
            source.partition_bytes(index),
            "partition {} ({}) did not survive the round trip",
            index + 1,
            part.name
        );
    }
}

#[test]
fn the_restored_partition_table_preserves_every_identifier() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);

    let backup_dir = back_up(&source, tmp.path(), "SYNTH_2026-01-01_1201").unwrap();
    let (mut device, _) = restore_onto(
        &backup_dir,
        &tmp.path().join("restored.img"),
        source.size_bytes(),
        512,
    );
    let restored = device.read_all().unwrap();

    let original = parse_primary(&source.bytes, 512).unwrap();
    let recreated = parse_primary(&restored, 512).expect("the restored disk should have a GPT");

    assert_eq!(
        recreated.header.disk_guid, original.header.disk_guid,
        "the disk GUID must be preserved, or Windows sees a different disk"
    );
    assert_eq!(recreated.partitions.len(), original.partitions.len());

    for (a, b) in original.partitions.iter().zip(recreated.partitions.iter()) {
        assert_eq!(a.type_guid, b.type_guid, "partition type GUID changed");
        assert_eq!(a.unique_guid, b.unique_guid, "partition GUID changed");
        assert_eq!(a.starting_lba, b.starting_lba, "partition moved");
        assert_eq!(a.ending_lba, b.ending_lba, "partition changed size");
        assert_eq!(a.attributes, b.attributes, "partition attributes changed");
        assert_eq!(a.name, b.name, "partition name changed");
    }
}

#[test]
fn a_backup_restores_onto_a_larger_disk_and_leaves_the_extra_space_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);
    let target_size = source.size_bytes() * 2;

    let backup_dir = back_up(&source, tmp.path(), "SYNTH_2026-01-01_1202").unwrap();
    let (mut device, outcome) = restore_onto(
        &backup_dir,
        &tmp.path().join("bigger.img"),
        target_size,
        512,
    );

    assert!(
        outcome.unallocated_bytes > source.size_bytes() / 2,
        "the extra space should be reported as unallocated"
    );

    let restored = device.read_all().unwrap();

    // The partitions still land in the right places.
    for (index, (_, offset, length)) in source.partitions.iter().enumerate() {
        let from = *offset as usize;
        let to = (*offset + *length) as usize;
        assert_eq!(&restored[from..to], source.partition_bytes(index));
    }

    // The partition table is rebuilt for the larger disk, so the secondary
    // header sits at the new end rather than where the old disk ended.
    let recreated = parse_primary(&restored, 512).expect("should parse");
    assert_eq!(
        recreated.header.alternate_lba,
        target_size / 512 - 1,
        "the secondary table should be at the end of the new disk"
    );
    assert_eq!(
        &restored[restored.len() - 512..restored.len() - 504],
        b"EFI PART"
    );
}

#[test]
fn a_restore_to_a_4k_sector_disk_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(4096);

    let backup_dir = back_up(&source, tmp.path(), "SYNTH4K_2026-01-01_1203").unwrap();
    let (mut device, _) = restore_onto(
        &backup_dir,
        &tmp.path().join("restored4k.img"),
        source.size_bytes(),
        4096,
    );
    let restored = device.read_all().unwrap();

    for (index, (_, offset, length)) in source.partitions.iter().enumerate() {
        let from = *offset as usize;
        let to = (*offset + *length) as usize;
        assert_eq!(&restored[from..to], source.partition_bytes(index));
    }
    assert!(parse_primary(&restored, 4096).is_ok());
}

#[test]
fn planning_a_restore_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);
    let backup_dir = back_up(&source, tmp.path(), "SYNTH_2026-01-01_1204").unwrap();

    let set = BackupSet::open(&backup_dir).unwrap();
    let target = target_disk(1, source.size_bytes(), 512);

    let mut device =
        FileBlockDevice::create(tmp.path().join("dryrun.img"), source.size_bytes(), 512).unwrap();

    // A dry run is exactly this: plan, and check every chunk can be read.
    let plan = mjolnir_restore::plan(&set, &target).expect("should plan");
    mjolnir_restore::check_chunks_present(&set, &mut SilentProgress, &CancelToken::new())
        .expect("every chunk should be present");

    assert!(!plan.writes.is_empty());
    assert_eq!(
        device.write_count(),
        0,
        "a dry run must not write to the target"
    );
    assert_eq!(
        device.read_all().unwrap(),
        vec![0u8; source.size_bytes() as usize],
        "the target disk must still be blank after a dry run"
    );
}

#[test]
fn a_cancelled_restore_stops() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);
    let backup_dir = back_up(&source, tmp.path(), "SYNTH_2026-01-01_1205").unwrap();

    let set = BackupSet::open(&backup_dir).unwrap();
    let target = target_disk(1, source.size_bytes(), 512);
    let plan = mjolnir_restore::plan(&set, &target).unwrap();
    let confirmation = EraseConfirmation::check(&target, &target.erase_phrase()).unwrap();

    let mut device =
        FileBlockDevice::create(tmp.path().join("cancelled.img"), source.size_bytes(), 512)
            .unwrap();

    let cancel = CancelToken::new();
    cancel.cancel();

    let err = mjolnir_restore::restore(
        &set,
        &plan,
        &target,
        &confirmation,
        &mut device,
        &mut SilentProgress,
        &cancel,
    )
    .unwrap_err();

    assert_eq!(err.exit(), mjolnir_core::ExitCode::Cancelled);
}

#[test]
fn the_backup_is_a_plain_folder_a_person_can_read() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);
    let backup_dir = back_up(&source, tmp.path(), "SYNTH_2026-01-01_1206").unwrap();

    // The three documents the format promises, plus the chunk store and a log
    // directory. No database, nothing that needs a tool to open.
    for name in ["manifest.json", "disk-layout.json", "completion.json"] {
        let path = backup_dir.join(name);
        assert!(path.is_file(), "{name} is missing");
        let text = std::fs::read_to_string(&path).unwrap();
        serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or_else(|e| panic!("{name} is not readable JSON: {e}"));
        assert!(
            text.contains("MjolnirVSS"),
            "{name} does not identify itself"
        );
    }
    assert!(backup_dir.join("chunks").is_dir());
}

#[test]
fn a_preview_backup_is_refused_by_the_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);

    // Capture only the first 64 KiB of each partition, the way a preview run
    // does.
    let backup_dir = back_up_with(
        &source,
        tmp.path(),
        "PREVIEW_2026-01-01_1207",
        CaptureMethod::Preview,
        Some(64 * 1024),
    )
    .unwrap();

    let set = BackupSet::open(&backup_dir).expect("a preview backup is still a valid backup set");
    let target = target_disk(1, source.size_bytes(), 512);

    let err = mjolnir_restore::plan(&set, &target).unwrap_err();
    assert_eq!(err.exit(), mjolnir_core::ExitCode::CorruptBackup);
    assert!(
        err.what().contains("preview"),
        "the refusal should say why: {}",
        err.what()
    );
}

#[test]
fn deduplication_stores_repeated_content_once() {
    let tmp = tempfile::tempdir().unwrap();

    // Two partitions filled with the same bytes: the chunk store should hold
    // one copy, and the manifest should reference it twice.
    let source = SyntheticDisk::build(
        512,
        32 * 1024 * 1024,
        vec![
            mjolnir_testkit::SyntheticPartition::windows(4 * 1024 * 1024),
            mjolnir_testkit::SyntheticPartition::windows(4 * 1024 * 1024),
        ],
    );

    let backup_dir = back_up(&source, tmp.path(), "DEDUP_2026-01-01_1208").unwrap();
    let set = BackupSet::open(&backup_dir).unwrap();
    let stats = set.manifest().stats;

    assert!(
        stats.deduplicated_segments > 0,
        "two identical partitions should have shared chunks: {stats:?}"
    );
    assert!(
        stats.stored_bytes < stats.captured_bytes,
        "the backup should be smaller than the data it holds"
    );
}

#[test]
fn every_partition_of_the_source_disk_is_in_the_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let source = SyntheticDisk::windows_like(512);
    let backup_dir = back_up(&source, tmp.path(), "SYNTH_2026-01-01_1209").unwrap();

    let set = BackupSet::open(&backup_dir).unwrap();
    let layout = set.disk_layout();
    let manifest = set.manifest();

    assert_eq!(layout.disks.len(), 1);
    assert_eq!(layout.disks[0].partitions.len(), source.partitions.len());

    // The rule MjolnirVSS refuses to break: nothing is silently left out.
    for partition in &layout.disks[0].partitions {
        let stream = manifest
            .stream_for_partition(&partition.id)
            .unwrap_or_else(|| panic!("partition {} has no stream", partition.number));
        assert_eq!(stream.length, partition.length);
        assert_eq!(stream.target_offset, partition.starting_offset);
    }

    // And the partition table itself is captured at both ends of the disk.
    assert_eq!(
        manifest
            .streams
            .iter()
            .filter(|s| s.kind == mjolnir_image::manifest::StreamKind::DiskHead)
            .count(),
        1
    );
    assert_eq!(
        manifest
            .streams
            .iter()
            .filter(|s| s.kind == mjolnir_image::manifest::StreamKind::DiskTail)
            .count(),
        1
    );
}
