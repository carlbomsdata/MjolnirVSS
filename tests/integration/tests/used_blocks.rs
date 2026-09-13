//! Used block imaging, end to end against a synthetic NTFS volume.
//!
//! These are the tests that decide whether skipping free space is safe. They
//! run the real capture code, the real verifier and the real restore engine, on
//! a volume whose free space is deliberately filled with garbage. A capture that
//! quietly read the free space, or a restore that quietly reproduced it, fails
//! here rather than on somebody's computer.

mod common;

use std::collections::HashMap;

use common::*;
use mjolnir_backup::capture::{CaptureSources, CaptureSpec, PartitionCapture};
use mjolnir_core::blockio::{BlockSource, MemoryBlockDevice};
use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::Result;
use mjolnir_core::ids::{BackupName, MachineId, StreamId};
use mjolnir_core::progress::SilentProgress;
use mjolnir_core::timestamp::UtcTimestamp;
use mjolnir_image::manifest::{BackupInfo, BackupKind, CaptureMethod, FirmwareMode, SourceInfo};
use mjolnir_image::verify::{VerifyDepth, VerifyReport};
use mjolnir_image::writer::{BackupWriter, WriterOptions};
use mjolnir_ntfs::bitmap::{plan_used_blocks, UsedBlockPlan};
use mjolnir_testkit::ntfs::{SyntheticNtfs, FREE_SPACE_FILL};
use mjolnir_testkit::{SyntheticDisk, SyntheticPartition};

/// A synthetic disk whose Windows partition holds a synthetic NTFS volume.
struct NtfsDisk {
    disk: SyntheticDisk,
    volume: SyntheticNtfs,
    /// Index of the partition holding the volume.
    index: usize,
}

impl NtfsDisk {
    /// Builds a four partition Windows layout whose third partition is NTFS.
    fn build(
        sector_size: u32,
        sectors_per_cluster: u32,
        fill: impl FnOnce(&mut SyntheticNtfs),
    ) -> Self {
        let disk = SyntheticDisk::build(
            sector_size,
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

        let mut volume = SyntheticNtfs::formatted(
            length,
            u16::try_from(sector_size).expect("sector size fits"),
            sectors_per_cluster,
        );
        fill(&mut volume);

        let mut disk = disk;
        let from = offset as usize;
        let to = (offset + length) as usize;
        volume.write_into(&mut disk.bytes[from..to]);

        Self {
            disk,
            volume,
            index,
        }
    }

    fn partition_offset(&self) -> u64 {
        self.disk.partitions[self.index].1
    }

    fn partition_length(&self) -> u64 {
        self.disk.partitions[self.index].2
    }

    fn plan(&self) -> UsedBlockPlan {
        plan_used_blocks(
            &self.volume.boot(),
            &self.volume.allocation(),
            self.partition_length(),
        )
        .expect("a synthetic volume should plan")
    }
}

/// Capture sources that offer a used block plan for one partition.
struct UsedBlockSources {
    device: MemoryBlockDevice,
    plans: HashMap<usize, UsedBlockPlan>,
    /// How many times a plan was asked for, so a test can prove it was used.
    asked: usize,
}

impl UsedBlockSources {
    fn new(disk: &SyntheticDisk) -> Self {
        Self {
            device: disk.as_device("synthetic-disk"),
            plans: HashMap::new(),
            asked: 0,
        }
    }

    fn with_plan(mut self, index: usize, plan: UsedBlockPlan) -> Self {
        self.plans.insert(index, plan);
        self
    }
}

impl CaptureSources for UsedBlockSources {
    fn open_disk(&mut self) -> Result<Box<dyn BlockSource>> {
        Ok(Box::new(self.device.clone()))
    }

    fn open_partition(&mut self, _index: usize) -> Result<Option<Box<dyn BlockSource>>> {
        Ok(None)
    }

    fn used_blocks(&mut self, index: usize) -> Result<Option<UsedBlockPlan>> {
        self.asked += 1;
        Ok(self.plans.get(&index).cloned())
    }
}

/// Runs a complete backup with used block imaging for one partition.
fn back_up_used_blocks(
    subject: &NtfsDisk,
    destination: &std::path::Path,
    name: &str,
) -> Result<std::path::PathBuf> {
    let backup_name = BackupName::new(name).expect("valid test backup name");
    let mut writer = BackupWriter::create(
        destination,
        &backup_name,
        BackupInfo {
            uuid: "11111111-2222-3333-4444-555555555555".to_owned(),
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
    )?;

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

    let first_partition = entry
        .partitions
        .iter()
        .map(|p| p.starting_offset)
        .min()
        .unwrap_or(1 << 20);
    let spec = CaptureSpec {
        disk_id: entry.id.clone(),
        disk: entry,
        head_bytes: first_partition,
        tail_bytes: mjolnir_storage::gpt::secondary_gpt_span(subject.disk.sector_size).unwrap(),
        partitions,
    };

    let mut sources = UsedBlockSources::new(&subject.disk).with_plan(subject.index, subject.plan());
    let mut progress = SilentProgress;
    let cancel = CancelToken::new();

    mjolnir_backup::capture::capture_disk(
        &spec,
        &mut sources,
        &mut writer,
        &mut progress,
        &cancel,
    )?;

    let finalized = writer.finalize()?;
    let dir = finalized.layout().dir().to_path_buf();
    let store = finalized.chunk_store();
    let report: VerifyReport = mjolnir_image::verify::verify(
        finalized.manifest(),
        Some(finalized.disk_layout()),
        &store,
        VerifyDepth::Full,
        &mut progress,
        &cancel,
    )?;
    assert!(
        report.passed(),
        "a used block backup failed its own verification: {:?}",
        report.issues
    );
    finalized.mark_complete(
        report.to_record(UtcTimestamp::now()),
        &UtcTimestamp::now().to_rfc3339(),
    )?;
    Ok(dir)
}

/// The whole point: a mostly empty volume produces a much smaller backup, and
/// what it does capture is exactly what was in use.
#[test]
fn a_mostly_empty_volume_is_captured_without_its_free_space() {
    let temp = tempfile::tempdir().unwrap();
    let subject = NtfsDisk::build(512, 8, |_| {});

    let dir = back_up_used_blocks(&subject, temp.path(), "used-blocks").unwrap();
    let manifest = read_manifest(&dir);

    let stream = manifest
        .streams
        .iter()
        .find(|s| s.id.as_str() == "disk-0-part-3")
        .expect("the NTFS partition has a stream");

    assert_eq!(stream.capture, CaptureMethod::VssUsedBlocks);
    assert!(
        stream.fallback_reason.is_none(),
        "used block imaging should not have fallen back: {:?}",
        stream.fallback_reason
    );

    let used = stream.used_blocks.expect("used block figures are recorded");
    assert_eq!(used.cluster_size, 4096);
    assert_eq!(used.clusters_total, subject.volume.cluster_count());
    assert_eq!(used.clusters_allocated, 32);
    assert!(used.extent_count >= 2, "{used:?}");

    // The bitmap describes whole clusters, and the filesystem counts every
    // sector of the partition but the last. So the undescribed tail is the
    // partial cluster at the end plus that spare sector, which for a 32 MiB
    // partition with 4 KiB clusters comes to one cluster.
    assert_eq!(used.undescribed_tail_bytes, 4096);

    let captured = stream.captured_bytes().unwrap();
    assert!(
        captured < subject.partition_length() / 100,
        "captured {captured} of {}",
        subject.partition_length()
    );

    // And the stream still describes the whole partition, so a restore puts
    // everything back where it belongs.
    assert_eq!(stream.length, subject.partition_length());
    assert_eq!(stream.target_offset, subject.partition_offset());
}

/// Restoring a used block backup must put the allocated data back byte for
/// byte, and must leave the free space alone. The free space in the source is
/// garbage, so a restored volume containing that garbage would prove it was
/// captured when it should not have been.
#[test]
fn a_restored_volume_has_the_data_back_and_zeros_where_the_free_space_was() {
    let temp = tempfile::tempdir().unwrap();
    let subject = NtfsDisk::build(512, 8, |volume| {
        // Something in the middle, and something at the very end, which is
        // where an off by one would lose data.
        volume.allocate(1000, 40);
        volume.allocate(volume.cluster_count() - 8, 8);
    });

    let dir = back_up_used_blocks(&subject, temp.path(), "used-blocks-restore").unwrap();
    let restored = restore_to_file(&dir, temp.path(), subject.disk.size_bytes());

    let offset = subject.partition_offset() as usize;
    let length = subject.partition_length() as usize;
    let volume = &restored[offset..offset + length];
    let source = &subject.disk.bytes[offset..offset + length];

    let cluster_size = subject.volume.cluster_size() as usize;
    let mut checked_allocated = 0usize;
    let mut checked_free = 0usize;

    for cluster in 0..subject.volume.cluster_count() {
        let from = cluster as usize * cluster_size;
        let to = from + cluster_size;
        if subject.volume.is_allocated(cluster) {
            assert_eq!(
                &volume[from..to],
                &source[from..to],
                "allocated cluster {cluster} did not come back"
            );
            checked_allocated += 1;
        } else {
            assert!(
                volume[from..to].iter().all(|b| *b == 0),
                "free cluster {cluster} came back holding data, so it was captured"
            );
            assert!(
                source[from..to].contains(&FREE_SPACE_FILL),
                "the test is not testing anything: free cluster {cluster} was already blank"
            );
            checked_free += 1;
        }
    }

    assert!(checked_allocated >= 40);
    assert!(checked_free > 1000);

    // The spare boot sector at the end of the partition is outside the bitmap
    // and has to come back anyway.
    let spare_at = length - 512;
    assert_eq!(
        &volume[spare_at..],
        &source[spare_at..],
        "the spare boot sector at the end of the partition was lost"
    );

    // Both copies of the boot sector, and the rest of the disk, are unchanged.
    assert_eq!(&volume[..512], &source[..512], "the boot sector was lost");
    let efi_offset = subject.disk.partitions[0].1 as usize;
    let efi_length = subject.disk.partitions[0].2 as usize;
    assert_eq!(
        &restored[efi_offset..efi_offset + efi_length],
        &subject.disk.bytes[efi_offset..efi_offset + efi_length],
        "a partition captured whole was damaged"
    );
}

/// A heavily fragmented volume is the case used block imaging is for, and the
/// case where a run tracking mistake shows up as missing data.
#[test]
fn a_fragmented_volume_restores_every_allocated_cluster() {
    let temp = tempfile::tempdir().unwrap();
    let subject = NtfsDisk::build(512, 8, |volume| {
        let total = volume.cluster_count();
        // Six hundred separate two cluster runs, spread across the volume.
        for i in 0..600 {
            let start = 100 + i * 13;
            if start + 2 < total {
                volume.allocate(start, 2);
            }
        }
    });

    let plan = subject.plan();
    assert!(
        plan.extent_count() > 500,
        "the volume is not fragmented enough to test anything: {} extents",
        plan.extent_count()
    );

    let dir = back_up_used_blocks(&subject, temp.path(), "fragmented").unwrap();
    let restored = restore_to_file(&dir, temp.path(), subject.disk.size_bytes());

    let offset = subject.partition_offset() as usize;
    let length = subject.partition_length() as usize;
    let volume = &restored[offset..offset + length];
    let source = &subject.disk.bytes[offset..offset + length];
    let cluster_size = subject.volume.cluster_size() as usize;

    for cluster in 0..subject.volume.cluster_count() {
        let from = cluster as usize * cluster_size;
        let to = from + cluster_size;
        if subject.volume.is_allocated(cluster) {
            assert_eq!(
                &volume[from..to],
                &source[from..to],
                "allocated cluster {cluster} did not come back"
            );
        }
    }
}

/// Every supported sector layout has to round trip, including the 4K one that
/// has never run on real hardware.
#[test]
fn every_sector_layout_round_trips() {
    for (sector_size, sectors_per_cluster) in [(512u32, 8u32), (512, 1), (4096, 1), (4096, 2)] {
        let temp = tempfile::tempdir().unwrap();
        let subject = NtfsDisk::build(sector_size, sectors_per_cluster, |volume| {
            volume.allocate(64, 32);
            volume.allocate(volume.cluster_count() - 4, 4);
        });

        let name = format!("layout-{sector_size}-{sectors_per_cluster}");
        let dir = back_up_used_blocks(&subject, temp.path(), &name).unwrap();
        let restored = restore_to_file(&dir, temp.path(), subject.disk.size_bytes());

        let offset = subject.partition_offset() as usize;
        let length = subject.partition_length() as usize;
        let volume = &restored[offset..offset + length];
        let source = &subject.disk.bytes[offset..offset + length];
        let cluster_size = subject.volume.cluster_size() as usize;

        for cluster in 0..subject.volume.cluster_count() {
            let from = cluster as usize * cluster_size;
            let to = from + cluster_size;
            if subject.volume.is_allocated(cluster) {
                assert_eq!(
                    &volume[from..to],
                    &source[from..to],
                    "{sector_size}/{sectors_per_cluster}: cluster {cluster} did not come back"
                );
            } else {
                assert!(
                    volume[from..to].iter().all(|b| *b == 0),
                    "{sector_size}/{sectors_per_cluster}: free cluster {cluster} was captured"
                );
            }
        }

        assert_eq!(
            &volume[length - sector_size as usize..],
            &source[length - sector_size as usize..],
            "{sector_size}/{sectors_per_cluster}: the spare boot sector was lost"
        );
    }
}

/// A full volume produces a capture the same size as the partition, and the
/// result is indistinguishable from a raw one.
#[test]
fn a_full_volume_captures_everything() {
    let temp = tempfile::tempdir().unwrap();
    let subject = NtfsDisk::build(512, 8, |volume| {
        let total = volume.cluster_count();
        volume.allocate(16, total - 16 - 16);
        volume.allocate(total - 16, 16);
    });

    let dir = back_up_used_blocks(&subject, temp.path(), "full").unwrap();
    let restored = restore_to_file(&dir, temp.path(), subject.disk.size_bytes());

    let offset = subject.partition_offset() as usize;
    let length = subject.partition_length() as usize;
    assert_eq!(
        &restored[offset..offset + length],
        &subject.disk.bytes[offset..offset + length],
        "a fully allocated volume must restore byte for byte"
    );
}

/// When the volume will not say what is in use, the capture reads the whole
/// partition and records that it had to. A backup must never look smaller than
/// it should without saying why.
#[test]
fn declining_to_report_allocation_falls_back_and_says_so() {
    let temp = tempfile::tempdir().unwrap();
    let subject = NtfsDisk::build(512, 8, |volume| volume.allocate(500, 20));

    // Same spec, but the sources offer no plan for any partition.
    let backup_name = BackupName::new("fallback").unwrap();
    let mut writer = BackupWriter::create(
        temp.path(),
        &backup_name,
        BackupInfo {
            uuid: "11111111-2222-3333-4444-555555555555".to_owned(),
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
    .unwrap();

    let entry = disk_entry_for(&subject.disk);
    let partitions = entry
        .partitions
        .iter()
        .map(|p| PartitionCapture {
            partition_id: p.id.clone(),
            stream_id: StreamId::new(format!("disk-0-part-{}", p.number)).unwrap(),
            capture: CaptureMethod::VssUsedBlocks,
            planned_bytes: p.length,
            source_description: "synthetic".to_owned(),
        })
        .collect();
    let spec = CaptureSpec {
        disk_id: entry.id.clone(),
        head_bytes: entry.partitions[0].starting_offset,
        tail_bytes: mjolnir_storage::gpt::secondary_gpt_span(subject.disk.sector_size).unwrap(),
        disk: entry,
        partitions,
    };

    let mut sources = UsedBlockSources::new(&subject.disk);
    mjolnir_backup::capture::capture_disk(
        &spec,
        &mut sources,
        &mut writer,
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .unwrap();

    assert_eq!(sources.asked, 4, "every partition should have been asked");

    let finalized = writer.finalize().unwrap();
    let manifest = finalized.manifest();
    for stream in &manifest.streams {
        if stream.kind != mjolnir_image::manifest::StreamKind::Partition {
            continue;
        }
        assert_eq!(
            stream.capture,
            CaptureMethod::VssRaw,
            "a stream that fell back must not still claim used block imaging"
        );
        let reason = stream
            .fallback_reason
            .as_deref()
            .expect("the fallback has to be recorded");
        assert!(reason.contains("clusters are in use"), "{reason}");
        assert!(stream.used_blocks.is_none());
        // Falling back means everything was read.
        assert_eq!(stream.captured_bytes().unwrap(), stream.length);
    }
}

/// A sparse stream's gaps are part of the format, and verification has to
/// report them rather than treat a hole as damage.
#[test]
fn verification_accepts_the_gaps_a_used_block_capture_leaves() {
    let temp = tempfile::tempdir().unwrap();
    let subject = NtfsDisk::build(512, 8, |volume| volume.allocate(2000, 64));

    // back_up_used_blocks already runs a full verification and asserts it
    // passed. Running it again from the finished folder proves the result is a
    // property of the backup rather than of the writer that made it.
    let dir = back_up_used_blocks(&subject, temp.path(), "gaps").unwrap();
    let report = verify_backup(&dir);
    assert!(report.passed(), "{:?}", report.issues);

    let manifest = read_manifest(&dir);
    let stream = manifest
        .streams
        .iter()
        .find(|s| s.id.as_str() == "disk-0-part-3")
        .unwrap();
    let covered = mjolnir_core::extents::ExtentList::from_unsorted(
        stream
            .segments
            .iter()
            .map(|s| mjolnir_core::extents::ByteRange::new(s.offset, s.length))
            .collect(),
    )
    .unwrap();
    let gaps = covered.gaps_within(stream.length).unwrap();
    assert!(!gaps.is_empty(), "a used block capture should leave gaps");
    let gap_bytes: u64 = gaps.iter().map(|g| g.length).sum();
    assert_eq!(gap_bytes + covered.total_bytes().unwrap(), stream.length);
}

/// The bitmap a volume reports, paged, has to reconstruct the allocation it
/// came from. Several page sizes are used, including one that splits runs.
#[test]
fn paged_bitmap_responses_reconstruct_the_allocation() {
    use mjolnir_ntfs::bitmap::{AllocationScan, BitmapPage};

    let mut volume = SyntheticNtfs::formatted(32 * 1024 * 1024, 512, 8);
    volume.allocate(300, 17);
    volume.allocate(1000, 1);
    volume.allocate(2048, 500);
    volume.allocate(volume.cluster_count() - 3, 3);

    let expected = volume.allocation();

    for clusters_per_page in [8u64, 64, 512, 4096, 65_536] {
        let pages = volume.bitmap_pages(clusters_per_page);
        assert!(!pages.is_empty());

        let mut scan: Option<AllocationScan> = None;
        for buffer in &pages {
            let page = BitmapPage::parse(buffer, buffer.len()).expect("a valid page");
            match &mut scan {
                None => scan = Some(AllocationScan::begin(&page).expect("the first page")),
                Some(s) => s.accept(&page).expect("a following page"),
            }
        }
        let got = scan.expect("at least one page").finish().expect("complete");

        assert_eq!(
            got.runs(),
            expected.runs(),
            "{clusters_per_page} clusters a page produced a different allocation"
        );
        assert_eq!(got.allocated_clusters(), expected.allocated_clusters());
        assert_eq!(got.total_clusters(), volume.cluster_count());
    }
}

/// Implementations differ about whether a later page restates the remaining
/// cluster count or the whole volume. Only the first page's figure is used, so
/// either convention has to produce the same answer.
#[test]
fn a_bitmap_that_restates_its_size_differently_is_still_read_correctly() {
    use mjolnir_ntfs::bitmap::{AllocationScan, BitmapPage};

    let mut volume = SyntheticNtfs::formatted(32 * 1024 * 1024, 512, 8);
    volume.allocate(700, 64);
    let expected = volume.allocation();
    let total = volume.cluster_count();

    let pages = volume.bitmap_pages(512);
    assert!(
        pages.len() > 4,
        "the volume needs several pages to test this"
    );

    // Rewrite every page after the first so it reports the whole volume rather
    // than the remainder, which is the other convention seen in the wild.
    let mut rewritten = pages.clone();
    for page in rewritten.iter_mut().skip(1) {
        page[8..16].copy_from_slice(&total.to_le_bytes());
    }

    let mut scan: Option<AllocationScan> = None;
    for buffer in &rewritten {
        let page = BitmapPage::parse(buffer, buffer.len()).expect("a valid page");
        match &mut scan {
            None => scan = Some(AllocationScan::begin(&page).expect("the first page")),
            Some(s) => s.accept(&page).expect("a following page"),
        }
    }
    let got = scan.unwrap().finish().expect("complete");
    assert_eq!(got.runs(), expected.runs());
}

/// A capture interrupted partway must leave a folder that is visibly not a
/// backup, whatever the capture method was.
#[test]
fn an_interrupted_used_block_capture_leaves_no_usable_backup() {
    let temp = tempfile::tempdir().unwrap();
    let subject = NtfsDisk::build(512, 8, |volume| volume.allocate(400, 2000));

    let backup_name = BackupName::new("interrupted").unwrap();
    let mut writer = BackupWriter::create(
        temp.path(),
        &backup_name,
        BackupInfo {
            uuid: "11111111-2222-3333-4444-555555555555".to_owned(),
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
    .unwrap();
    let dir = writer.layout().dir().to_path_buf();

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
            source_description: "synthetic".to_owned(),
        })
        .collect();
    let spec = CaptureSpec {
        disk_id: entry.id.clone(),
        head_bytes: entry.partitions[0].starting_offset,
        tail_bytes: mjolnir_storage::gpt::secondary_gpt_span(512).unwrap(),
        disk: entry,
        partitions,
    };

    // Cancelled before anything is read.
    let cancel = CancelToken::new();
    cancel.cancel();
    let mut sources = UsedBlockSources::new(&subject.disk).with_plan(subject.index, subject.plan());
    let err = mjolnir_backup::capture::capture_disk(
        &spec,
        &mut sources,
        &mut writer,
        &mut SilentProgress,
        &cancel,
    )
    .expect_err("a cancelled capture must fail");
    assert_eq!(err.exit(), mjolnir_core::ExitCode::Cancelled);

    drop(writer);
    assert!(
        !dir.join("completion.json").exists(),
        "an interrupted capture must not leave a completion marker"
    );
    assert!(
        mjolnir_image::BackupSet::open(&dir).is_err(),
        "an interrupted capture must not open as a backup"
    );
}
