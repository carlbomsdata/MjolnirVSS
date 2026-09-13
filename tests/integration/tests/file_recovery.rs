//! Getting files back out of a backup, end to end.
//!
//! A volume with an actual master file table in it is built, put inside a real
//! GPT disk, backed up by the real capture engine with used block imaging, and
//! then opened the way the **Restore files** button opens it: through
//! [`BackupSet`], [`mjolnir_files::volumes_in`] and [`OpenVolume`]. Nothing is
//! read from the original volume after the backup is written, so what these
//! assert is that the bytes came back out of the backup.
//!
//! The files in it are the ones that break readers: a file too big for its own
//! record, a name outside the basic multilingual plane, an alternate data
//! stream, a compressed file, a junction, and one file with two names in two
//! directories.

mod common;

use std::collections::HashMap;
use std::path::Path;

use common::*;
use mjolnir_backup::capture::{CaptureSources, CaptureSpec, PartitionCapture};
use mjolnir_core::blockio::{BlockSource, MemoryBlockDevice};
use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::Result;
use mjolnir_core::ids::{BackupName, MachineId, StreamId};
use mjolnir_core::progress::SilentProgress;
use mjolnir_core::timestamp::UtcTimestamp;
use mjolnir_files::extract::{extract_file, extract_tree, ExtractOptions, Extracted};
use mjolnir_files::{volumes_in, OpenVolume};
use mjolnir_image::manifest::{BackupInfo, BackupKind, CaptureMethod, FirmwareMode, SourceInfo};
use mjolnir_image::set::BackupSet;
use mjolnir_image::verify::{VerifyDepth, VerifyReport};
use mjolnir_image::writer::{BackupWriter, WriterOptions};
use mjolnir_ntfs::bitmap::UsedBlockPlan;
use mjolnir_ntfs::boot::VolumeSignature;
use mjolnir_testkit::{NtfsVolumeBuilder, PlannedFile, SyntheticDisk, SyntheticPartition};

/// Record numbers, so the assertions can name the files.
mod record {
    pub const DOCUMENTS: u64 = 6;
    pub const PHOTOS: u64 = 7;
    pub const README: u64 = 8;
    pub const NOTES: u64 = 9;
    pub const LARGE: u64 = 10;
    pub const UNICODE: u64 = 11;
    pub const COMPRESSED: u64 = 12;
    pub const JUNCTION: u64 = 13;
    pub const LINKED: u64 = 14;
}

/// The contents of the large file, which has to survive being fragmented
/// across clusters and read back through the chunk store.
fn large_contents() -> Vec<u8> {
    // Not a repeating pattern: a chunk store that returned the wrong chunk
    // would otherwise still produce the right bytes.
    (0..200_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect()
}

/// Builds the volume the tests browse.
fn planned_volume(partition_bytes: u64) -> NtfsVolumeBuilder {
    NtfsVolumeBuilder::new(partition_bytes)
        .with_file(PlannedFile::directory(record::DOCUMENTS, 5, "Documents"))
        .with_file(PlannedFile::directory(record::PHOTOS, 5, "Photos"))
        .with_file(PlannedFile::file(
            record::README,
            5,
            "readme.txt",
            b"MjolnirVSS test volume.\r\n".to_vec(),
        ))
        .with_file(
            PlannedFile::file(
                record::NOTES,
                record::DOCUMENTS,
                "notes.txt",
                b"the visible contents".to_vec(),
            )
            .with_stream(
                "Zone.Identifier",
                b"[ZoneTransfer]\r\nZoneId=3\r\n".to_vec(),
            ),
        )
        .with_file(
            PlannedFile::file(
                record::LARGE,
                record::DOCUMENTS,
                "large.bin",
                large_contents(),
            )
            .large(),
        )
        .with_file(PlannedFile::file(
            record::UNICODE,
            record::PHOTOS,
            // Outside the basic multilingual plane, so it is a surrogate pair,
            // plus an accented name Windows would normalise differently.
            "smörgås-🔨.jpg",
            b"not really a photograph".to_vec(),
        ))
        .with_file(
            PlannedFile::file(record::COMPRESSED, 5, "compressed.dat", vec![0x42; 8000])
                .large()
                .compressed(),
        )
        .with_file(PlannedFile::directory(record::JUNCTION, 5, "junction").reparse_point())
        .with_file(
            PlannedFile::file(
                record::LINKED,
                record::DOCUMENTS,
                "linked.txt",
                b"one file, two names".to_vec(),
            )
            .hard_linked_as(record::PHOTOS, "also-linked.txt")
            // And a second name in the *same* folder, which is what a real
            // Windows volume produced and what used to be dropped.
            .hard_linked_as(record::DOCUMENTS, "linked-again.txt"),
        )
}

/// A synthetic disk whose Windows partition holds that volume.
struct Subject {
    disk: SyntheticDisk,
    volume: NtfsVolumeBuilder,
    index: usize,
}

impl Subject {
    fn build() -> Self {
        let disk = SyntheticDisk::build(
            512,
            64 * 1024 * 1024,
            vec![
                SyntheticPartition::efi(4 * 1024 * 1024),
                SyntheticPartition::msr(1024 * 1024),
                SyntheticPartition::windows(32 * 1024 * 1024),
                SyntheticPartition::recovery(4 * 1024 * 1024),
            ],
        );
        let index = 2;
        let (_, offset, length) = disk.partitions[index].clone();

        let volume = planned_volume(length);
        let bytes = volume.build();

        let mut disk = disk;
        let from = offset as usize;
        disk.bytes[from..from + bytes.len()].copy_from_slice(&bytes);

        Self {
            disk,
            volume,
            index,
        }
    }

    fn stream_id(&self) -> String {
        format!("disk-0-part-{}", self.index + 1)
    }
}

/// Capture sources offering the volume's used block plan.
struct Sources {
    device: MemoryBlockDevice,
    plans: HashMap<usize, UsedBlockPlan>,
}

impl CaptureSources for Sources {
    fn open_disk(&mut self) -> Result<Box<dyn BlockSource>> {
        Ok(Box::new(self.device.clone()))
    }

    fn open_partition(&mut self, _index: usize) -> Result<Option<Box<dyn BlockSource>>> {
        Ok(None)
    }

    fn used_blocks(&mut self, index: usize) -> Result<Option<UsedBlockPlan>> {
        Ok(self.plans.get(&index).cloned())
    }
}

/// Backs the subject up, verifies it, and marks it complete.
fn back_up(subject: &Subject, destination: &Path) -> std::path::PathBuf {
    let backup_name = BackupName::new("file-recovery").expect("valid name");
    let mut writer = BackupWriter::create(
        destination,
        &backup_name,
        BackupInfo {
            uuid: "33333333-4444-5555-6666-777777777777".to_owned(),
            name: backup_name.clone(),
            created_utc: UtcTimestamp::now().to_rfc3339(),
            kind: BackupKind::Full,
            scope: "system-disk".to_owned(),
        },
        SourceInfo {
            machine_id: MachineId::new("synthetic-pc").unwrap(),
            computer_name: "SYNTHETIC-PC".to_owned(),
            windows: Default::default(),
            firmware: FirmwareMode::Uefi,
        },
        WriterOptions::default(),
    )
    .expect("the writer should create a backup");

    let entry = disk_entry_for(&subject.disk);
    let partitions = entry
        .partitions
        .iter()
        .enumerate()
        .map(|(i, p)| PartitionCapture {
            partition_id: p.id.clone(),
            stream_id: StreamId::new(format!("disk-0-part-{}", p.number)).unwrap(),
            capture: if i == subject.index {
                CaptureMethod::VssUsedBlocks
            } else {
                CaptureMethod::RawFull
            },
            planned_bytes: p.length,
            source_description: format!("partition {} of the synthetic disk", p.number),
        })
        .collect();

    let first = entry
        .partitions
        .iter()
        .map(|p| p.starting_offset)
        .min()
        .unwrap_or(1 << 20);
    let spec = CaptureSpec {
        disk_id: entry.id.clone(),
        disk: entry,
        head_bytes: first,
        tail_bytes: mjolnir_storage::gpt::secondary_gpt_span(subject.disk.sector_size).unwrap(),
        partitions,
    };

    let mut plans = HashMap::new();
    plans.insert(subject.index, subject.volume.plan());
    let mut sources = Sources {
        device: subject.disk.as_device("synthetic-disk"),
        plans,
    };
    let mut progress = SilentProgress;
    let cancel = CancelToken::new();

    mjolnir_backup::capture::capture_disk(&spec, &mut sources, &mut writer, &mut progress, &cancel)
        .expect("the capture should succeed");

    let finalized = writer.finalize().expect("finalize");
    let dir = finalized.layout().dir().to_path_buf();
    let store = finalized.chunk_store();
    let report: VerifyReport = mjolnir_image::verify::verify(
        finalized.manifest(),
        Some(finalized.disk_layout()),
        &store,
        VerifyDepth::Full,
        &mut progress,
        &cancel,
    )
    .expect("verification should run");
    assert!(report.passed(), "{:?}", report.issues);
    finalized
        .mark_complete(
            report.to_record(UtcTimestamp::now()),
            &UtcTimestamp::now().to_rfc3339(),
        )
        .expect("mark complete");
    dir
}

/// Everything a test needs: a finished backup on disk and the subject it came
/// from. The temporary directory is returned so it outlives the backup.
fn prepared() -> (tempfile::TempDir, Subject, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let subject = Subject::build();
    let dir = back_up(&subject, temp.path());
    (temp, subject, dir)
}

#[test]
fn the_ntfs_partition_is_offered_as_a_browsable_volume() {
    let (_temp, subject, dir) = prepared();
    let set = BackupSet::open(&dir).expect("the backup should open");

    let volumes = volumes_in(&set);
    let volume = volumes
        .iter()
        .find(|v| v.stream_id == subject.stream_id())
        .unwrap_or_else(|| panic!("the Windows partition should be browsable: {volumes:?}"));

    // Windows recorded no filesystem for this synthetic disk, which is exactly
    // the case that used to make the browser refuse a volume it can read
    // perfectly well. It is decided by reading the partition instead.
    assert_eq!(volume.filesystem, None);
    assert_eq!(volume.signature, Some(VolumeSignature::Ntfs));
    assert!(volume.is_readable, "{:?}", volume.why_not);

    // The others are listed too, each with the reason, rather than omitted.
    let reserved = volumes
        .iter()
        .find(|v| v.stream_id == "disk-0-part-2")
        .expect("every partition is listed, readable or not");
    assert!(!reserved.is_readable);
    assert!(
        reserved
            .why_not
            .as_deref()
            .unwrap_or_default()
            .contains("NTFS"),
        "{:?}",
        reserved.why_not
    );
    assert_eq!(volumes.len(), 4, "{volumes:?}");
}

#[test]
fn the_tree_that_comes_out_of_the_backup_is_the_tree_that_went_in() {
    let (_temp, subject, dir) = prepared();
    let set = BackupSet::open(&dir).unwrap();
    let stream = mjolnir_files::stream_in(&set, &subject.stream_id()).unwrap();
    let open = OpenVolume::open(&set, stream, &CancelToken::new()).expect("the volume should open");
    let index = open.index();

    assert_eq!(
        index.path_of(record::README).as_deref(),
        Some("\\readme.txt")
    );
    assert_eq!(
        index.path_of(record::NOTES).as_deref(),
        Some("\\Documents\\notes.txt")
    );
    assert_eq!(
        index.path_of(record::UNICODE).as_deref(),
        Some("\\Photos\\smörgås-🔨.jpg")
    );

    let root: Vec<&str> = index
        .children_of(5)
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    for expected in ["Documents", "Photos", "readme.txt", "compressed.dat"] {
        assert!(root.contains(&expected), "{expected} missing from {root:?}");
    }

    // Resolving a path the way the window and the CLI do.
    let entry = index
        .resolve("\\Documents\\large.bin")
        .expect("the large file should resolve by path");
    assert_eq!(entry.number, record::LARGE);
    assert_eq!(entry.size, large_contents().len() as u64);
}

#[test]
fn a_file_with_two_names_appears_in_both_directories() {
    let (_temp, subject, dir) = prepared();
    let set = BackupSet::open(&dir).unwrap();
    let stream = mjolnir_files::stream_in(&set, &subject.stream_id()).unwrap();
    let open = OpenVolume::open(&set, stream, &CancelToken::new()).unwrap();
    let index = open.index();

    let documents: Vec<&str> = index
        .children_of(record::DOCUMENTS)
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    let photos: Vec<&str> = index
        .children_of(record::PHOTOS)
        .iter()
        .map(|e| e.name.as_str())
        .collect();

    assert!(documents.contains(&"linked.txt"), "{documents:?}");
    assert!(photos.contains(&"also-linked.txt"), "{photos:?}");
    assert!(
        index
            .names_of(record::LINKED)
            .iter()
            .all(|e| e.is_hard_linked),
        "a file with two names should be reported as hard linked"
    );
}

#[test]
fn the_bytes_that_come_out_are_the_bytes_that_went_in() {
    let (_temp, subject, dir) = prepared();
    let out = tempfile::tempdir().unwrap();
    let set = BackupSet::open(&dir).unwrap();
    let stream = mjolnir_files::stream_in(&set, &subject.stream_id()).unwrap();
    let mut open = OpenVolume::open(&set, stream, &CancelToken::new()).unwrap();

    let options = ExtractOptions::default();
    let mut progress = SilentProgress;
    let cancel = CancelToken::new();

    for number in [
        record::README,
        record::NOTES,
        record::LARGE,
        record::UNICODE,
    ] {
        let entry = open.index().entry(number).cloned().unwrap();
        let results = extract_file(
            &mut open,
            &entry,
            out.path(),
            &options,
            &mut progress,
            &cancel,
        )
        .expect("extraction should run");
        assert!(
            results.iter().any(Extracted::wrote_something),
            "record {number} produced nothing: {results:?}"
        );
    }

    let written = |relative: &str| std::fs::read(out.path().join(relative)).unwrap_or_default();

    assert_eq!(
        written("readme.txt"),
        b"MjolnirVSS test volume.\r\n".to_vec()
    );
    assert_eq!(
        written("Documents\\notes.txt"),
        b"the visible contents".to_vec()
    );
    // The one that does not fit in a record, read back through the chunk store.
    assert_eq!(written("Documents\\large.bin"), large_contents());
    assert_eq!(
        written("Photos\\smörgås-🔨.jpg"),
        b"not really a photograph".to_vec()
    );
}

#[test]
fn an_alternate_data_stream_is_written_beside_its_file() {
    let (_temp, subject, dir) = prepared();
    let out = tempfile::tempdir().unwrap();
    let set = BackupSet::open(&dir).unwrap();
    let stream = mjolnir_files::stream_in(&set, &subject.stream_id()).unwrap();
    let mut open = OpenVolume::open(&set, stream, &CancelToken::new()).unwrap();

    let entry = open.index().entry(record::NOTES).cloned().unwrap();
    assert_eq!(entry.streams.len(), 1, "{:?}", entry.streams);

    let results = extract_file(
        &mut open,
        &entry,
        out.path(),
        &ExtractOptions::default(),
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .unwrap();

    assert_eq!(
        results.iter().filter(|r| r.wrote_something()).count(),
        2,
        "the file and its stream should both be written: {results:?}"
    );

    let beside = out
        .path()
        .join("Documents\\notes.txt.stream-Zone_Identifier");
    assert!(beside.is_file(), "the stream should be beside the file");
    assert_eq!(
        std::fs::read(&beside).unwrap(),
        b"[ZoneTransfer]\r\nZoneId=3\r\n".to_vec()
    );
}

#[test]
fn a_compressed_file_is_refused_by_name_rather_than_written_wrong() {
    let (_temp, subject, dir) = prepared();
    let out = tempfile::tempdir().unwrap();
    let set = BackupSet::open(&dir).unwrap();
    let stream = mjolnir_files::stream_in(&set, &subject.stream_id()).unwrap();
    let mut open = OpenVolume::open(&set, stream, &CancelToken::new()).unwrap();

    let entry = open.index().entry(record::COMPRESSED).cloned().unwrap();
    assert!(entry.is_compressed, "the fixture should be compressed");
    assert!(entry.why_unreadable().is_some());

    let results = extract_file(
        &mut open,
        &entry,
        out.path(),
        &ExtractOptions::default(),
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .unwrap();

    match results.as_slice() {
        [Extracted::Skipped { source, reason }] => {
            assert!(source.contains("compressed.dat"), "{source}");
            assert!(reason.to_lowercase().contains("compress"), "{reason}");
        }
        other => panic!("a compressed file should be skipped with a reason: {other:?}"),
    }
    assert!(
        !out.path().join("compressed.dat").exists(),
        "nothing should have been written for a file that cannot be read"
    );
}

#[test]
fn a_junction_is_not_followed_unless_it_is_asked_for() {
    let (_temp, subject, dir) = prepared();
    let out = tempfile::tempdir().unwrap();
    let set = BackupSet::open(&dir).unwrap();
    let stream = mjolnir_files::stream_in(&set, &subject.stream_id()).unwrap();
    let mut open = OpenVolume::open(&set, stream, &CancelToken::new()).unwrap();

    let entry = open.index().entry(record::JUNCTION).cloned().unwrap();
    assert!(entry.is_reparse_point, "the fixture should be a junction");

    let outcome = extract_tree(
        &mut open,
        &entry,
        out.path(),
        &ExtractOptions::default(),
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .unwrap();

    assert_eq!(outcome.written(), 0, "{:?}", outcome.files);
    assert!(outcome.skipped() >= 1, "{:?}", outcome.files);
    assert!(
        outcome.files.iter().any(|f| matches!(
            f,
            Extracted::Skipped { reason, .. } if reason.contains("junction") || reason.contains("link")
        )),
        "the reason should say what it is: {:?}",
        outcome.files
    );
}

#[test]
fn copying_a_folder_out_brings_everything_readable_under_it() {
    let (_temp, subject, dir) = prepared();
    let out = tempfile::tempdir().unwrap();
    let set = BackupSet::open(&dir).unwrap();
    let stream = mjolnir_files::stream_in(&set, &subject.stream_id()).unwrap();
    let mut open = OpenVolume::open(&set, stream, &CancelToken::new()).unwrap();

    let entry = open.index().entry(record::DOCUMENTS).cloned().unwrap();
    let outcome = extract_tree(
        &mut open,
        &entry,
        out.path(),
        &ExtractOptions::default(),
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .unwrap();

    assert_eq!(outcome.failed(), 0, "{:?}", outcome.files);
    assert!(outcome.written() >= 4, "{:?}", outcome.files);
    assert_eq!(
        std::fs::read(out.path().join("Documents\\large.bin")).unwrap(),
        large_contents()
    );
    assert_eq!(
        std::fs::read(out.path().join("Documents\\linked.txt")).unwrap(),
        b"one file, two names".to_vec()
    );
    assert_eq!(outcome.bytes_written, {
        let mut total = large_contents().len() as u64;
        total += "the visible contents".len() as u64;
        total += "[ZoneTransfer]\r\nZoneId=3\r\n".len() as u64;
        // Both names of the hard linked file are copied, so its contents are
        // written twice. That is what a hard link is on a filesystem that
        // cannot hold one.
        total += 2 * "one file, two names".len() as u64;
        total
    });
}

#[test]
fn an_existing_file_is_left_alone_unless_overwriting_was_asked_for() {
    let (_temp, subject, dir) = prepared();
    let out = tempfile::tempdir().unwrap();
    let set = BackupSet::open(&dir).unwrap();
    let stream = mjolnir_files::stream_in(&set, &subject.stream_id()).unwrap();
    let mut open = OpenVolume::open(&set, stream, &CancelToken::new()).unwrap();

    std::fs::write(out.path().join("readme.txt"), b"something I already had").unwrap();
    let entry = open.index().entry(record::README).cloned().unwrap();

    let results = extract_file(
        &mut open,
        &entry,
        out.path(),
        &ExtractOptions::default(),
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .unwrap();
    assert!(results.iter().all(|r| !r.wrote_something()), "{results:?}");
    assert_eq!(
        std::fs::read(out.path().join("readme.txt")).unwrap(),
        b"something I already had".to_vec(),
        "the file that was already there should be untouched"
    );

    let results = extract_file(
        &mut open,
        &entry,
        out.path(),
        &ExtractOptions {
            overwrite: true,
            ..Default::default()
        },
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .unwrap();
    assert!(
        results.iter().any(Extracted::wrote_something),
        "{results:?}"
    );
    assert_eq!(
        std::fs::read(out.path().join("readme.txt")).unwrap(),
        b"MjolnirVSS test volume.\r\n".to_vec()
    );
}

#[test]
fn browsing_never_writes_to_the_backup() {
    let (_temp, subject, dir) = prepared();
    let before = fingerprint(&dir);

    let set = BackupSet::open(&dir).unwrap();
    let stream = mjolnir_files::stream_in(&set, &subject.stream_id()).unwrap();
    let mut open = OpenVolume::open(&set, stream, &CancelToken::new()).unwrap();
    let out = tempfile::tempdir().unwrap();
    let entry = open.index().entry(record::DOCUMENTS).cloned().unwrap();
    extract_tree(
        &mut open,
        &entry,
        out.path(),
        &ExtractOptions::default(),
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .unwrap();

    assert_eq!(
        before,
        fingerprint(&dir),
        "opening and copying out of a backup must not change it"
    );
}

/// Every file in a folder, by path and size, so a test can prove nothing moved.
fn fingerprint(dir: &Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut queue = vec![dir.to_path_buf()];
    while let Some(at) = queue.pop() {
        for entry in std::fs::read_dir(&at).expect("the backup folder should be readable") {
            let entry = entry.expect("a directory entry");
            let path = entry.path();
            if path.is_dir() {
                queue.push(path);
            } else {
                let relative = path
                    .strip_prefix(dir)
                    .expect("inside the folder")
                    .to_string_lossy()
                    .into_owned();
                out.push((relative, entry.metadata().expect("metadata").len()));
            }
        }
    }
    out.sort();
    out
}

/// The bug a real Windows volume found: a file with two names in one folder was
/// copied out once, under whichever name its record listed first, and the other
/// name vanished with no file and no message.
#[test]
fn every_name_of_a_hard_linked_file_is_copied_out() {
    let (_temp, subject, dir) = prepared();
    let out = tempfile::tempdir().unwrap();
    let set = BackupSet::open(&dir).unwrap();
    let stream = mjolnir_files::stream_in(&set, &subject.stream_id()).unwrap();
    let mut open = OpenVolume::open(&set, stream, &CancelToken::new()).unwrap();

    let documents = open.index().entry(record::DOCUMENTS).cloned().unwrap();
    let outcome = extract_tree(
        &mut open,
        &documents,
        out.path(),
        &ExtractOptions::default(),
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .unwrap();

    assert_eq!(outcome.failed(), 0, "{:?}", outcome.files);
    for name in ["linked.txt", "linked-again.txt"] {
        let at = out.path().join("Documents").join(name);
        assert!(
            at.is_file(),
            "{name} was not copied out: {:?}",
            outcome.files
        );
        assert_eq!(std::fs::read(&at).unwrap(), b"one file, two names".to_vec());
    }

    // The name in the folder that was not copied stays where it is.
    assert!(
        !out.path().join("Photos\\also-linked.txt").exists(),
        "copying one folder must not reach into another"
    );
}

/// A short name is not a second place a file lives, so a file that has one is
/// not hard linked. On a real Windows volume almost every file has one, and
/// counting them marked the whole listing.
#[test]
fn a_file_with_one_name_is_not_reported_as_hard_linked() {
    let (_temp, subject, dir) = prepared();
    let set = BackupSet::open(&dir).unwrap();
    let stream = mjolnir_files::stream_in(&set, &subject.stream_id()).unwrap();
    let open = OpenVolume::open(&set, stream, &CancelToken::new()).unwrap();
    let index = open.index();

    let readme = index.entry(record::README).unwrap();
    assert!(!readme.is_hard_linked, "one name is not a hard link");

    assert!(
        index
            .names_of(record::LINKED)
            .iter()
            .all(|e| e.is_hard_linked),
        "three names is"
    );
    assert_eq!(index.names_of(record::LINKED).len(), 3);
}

/// A path is a name in a folder, so a record with several names has several.
#[test]
fn each_name_of_a_file_has_its_own_path() {
    let (_temp, subject, dir) = prepared();
    let set = BackupSet::open(&dir).unwrap();
    let stream = mjolnir_files::stream_in(&set, &subject.stream_id()).unwrap();
    let open = OpenVolume::open(&set, stream, &CancelToken::new()).unwrap();
    let index = open.index();

    let mut paths: Vec<String> = index
        .names_of(record::LINKED)
        .iter()
        .filter_map(|e| index.path_of_entry(e))
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "\\Documents\\linked-again.txt".to_owned(),
            "\\Documents\\linked.txt".to_owned(),
            "\\Photos\\also-linked.txt".to_owned(),
        ]
    );
}

/// A BitLocker partition in a backup is not an NTFS volume to browse, and
/// saying so is more honest than reporting the filesystem Windows saw inside it
/// and then failing to open anything.
#[test]
fn a_bitlocker_partition_says_so_rather_than_failing_to_open() {
    let temp = tempfile::tempdir().unwrap();
    let mut subject = Subject::build();

    // The volume header a locked BitLocker partition actually carries. The
    // partition is otherwise the one that was captured, which is the situation
    // a backup of an encrypted machine produces.
    let at = subject.disk.partitions[subject.index].1 as usize;
    subject.disk.bytes[at + 3..at + 11].copy_from_slice(b"-FVE-FS-");

    let dir = back_up(&subject, temp.path());
    let set = BackupSet::open(&dir).unwrap();
    let volume = volumes_in(&set)
        .into_iter()
        .find(|v| v.stream_id == subject.stream_id())
        .expect("the partition is still listed");

    assert_eq!(volume.signature, Some(VolumeSignature::BitLocker));
    assert!(!volume.is_readable);
    let why = volume.why_not.unwrap_or_default();
    assert!(why.contains("BitLocker"), "{why}");
    assert!(
        why.contains("restoring the whole disk"),
        "it should say what does work: {why}"
    );
}

/// A real Windows volume has a master file table with far more slots in it than
/// files, and the spare ones are zeros. Counting those as records that could
/// not be read told somebody recovering their files that 175 of them were
/// damaged when the volume was perfectly healthy.
#[test]
fn slots_that_never_held_a_file_are_not_reported_as_damage() {
    let temp = tempfile::tempdir().unwrap();
    let mut subject = Subject::build();

    // Everything past the files is left as zeros, as a real volume leaves it.
    let (_, offset, length) = subject.disk.partitions[subject.index].clone();
    subject.volume = planned_volume(length).zeroed_from(record::LINKED + 1);
    let bytes = subject.volume.build();
    let from = offset as usize;
    subject.disk.bytes[from..from + bytes.len()].copy_from_slice(&bytes);

    let dir = back_up(&subject, temp.path());
    let set = BackupSet::open(&dir).unwrap();
    let stream = mjolnir_files::stream_in(&set, &subject.stream_id()).unwrap();
    let open = OpenVolume::open(&set, stream, &CancelToken::new()).unwrap();
    let index = open.index();

    assert!(
        index.records_unused > 0,
        "the fixture should have unused slots in it"
    );
    assert!(
        index.unreadable.is_empty(),
        "an unused slot is space, not damage: {:?}",
        index.unreadable
    );
    // And the files are all still there.
    assert_eq!(
        index.path_of(record::README).as_deref(),
        Some("\\readme.txt")
    );
    // Every slot in the table was looked at, and the ones with nothing in them
    // are counted as nothing rather than as a loss.
    assert_eq!(index.records_scanned, subject.volume.record_count());
    assert!(
        index.records_unused + index.len() as u64 <= index.records_scanned,
        "more records were accounted for than were scanned"
    );
}
