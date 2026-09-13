//! Shared helpers: turning a synthetic disk into a real backup set.
//!
//! Each integration test binary compiles this module separately and uses a
//! different part of it, so anything one of them does not call looks unused
//! from that binary's point of view. That is a property of how Rust builds
//! integration tests, not of the code.
#![allow(dead_code)]

//!
//! These drive the same `capture_disk` function the Windows backup engine
//! drives, with a synthetic disk standing in for the machine. What comes out is
//! an ordinary MjolnirVSS backup folder that the verifier and the restore
//! engine cannot tell apart from one taken on real hardware.

use std::path::{Path, PathBuf};

use mjolnir_backup::capture::{CaptureSources, CaptureSpec, PartitionCapture};
use mjolnir_core::blockio::{BlockSource, MemoryBlockDevice};
use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::Result;
use mjolnir_core::ids::{BackupName, DiskId, MachineId, PartitionId, StreamId};
use mjolnir_core::progress::SilentProgress;
use mjolnir_core::timestamp::UtcTimestamp;
use mjolnir_image::disk_layout::{
    BusType, DiskEntry, PartitionEntry, PartitionRole, PartitionStyle,
};
use mjolnir_image::manifest::{BackupInfo, BackupKind, CaptureMethod, FirmwareMode, SourceInfo};
use mjolnir_image::verify::{VerifyDepth, VerifyReport};
use mjolnir_image::writer::{BackupWriter, WriterOptions};
use mjolnir_testkit::SyntheticDisk;

/// A synthetic disk offered as a capture source.
pub struct SyntheticSources {
    device: MemoryBlockDevice,
}

impl SyntheticSources {
    pub fn new(disk: &SyntheticDisk) -> Self {
        Self {
            device: disk.as_device("synthetic-disk"),
        }
    }
}

impl CaptureSources for SyntheticSources {
    fn open_disk(&mut self) -> Result<Box<dyn BlockSource>> {
        Ok(Box::new(self.device.clone()))
    }

    fn open_partition(&mut self, _index: usize) -> Result<Option<Box<dyn BlockSource>>> {
        // A synthetic disk has no shadow copy, so everything is read from the
        // disk itself. That is the same path the EFI and reserved partitions
        // take on a real machine.
        Ok(None)
    }
}

/// Builds the layout document for a synthetic disk.
pub fn disk_entry_for(disk: &SyntheticDisk) -> DiskEntry {
    let partitions = disk
        .partitions
        .iter()
        .enumerate()
        .map(|(i, (part, offset, length))| PartitionEntry {
            id: PartitionId::new(format!("disk-0-part-{}", i + 1)).unwrap(),
            number: i as u32 + 1,
            type_guid: part.type_guid.clone(),
            unique_guid: part.unique_guid.clone(),
            name: part.name.clone(),
            starting_offset: *offset,
            length: *length,
            attributes: 0,
            role: PartitionRole::from_type_guid(&part.type_guid),
            filesystem: None,
        })
        .collect();

    DiskEntry {
        id: DiskId::new("disk-0").unwrap(),
        disk_number: 0,
        size_bytes: disk.size_bytes(),
        logical_sector_size: disk.sector_size,
        physical_sector_size: disk.sector_size,
        partition_style: PartitionStyle::Gpt,
        disk_guid: disk.disk_guid.clone(),
        model: Some("Synthetic Test Disk".to_owned()),
        serial: Some("SYNTH0001".to_owned()),
        bus_type: BusType::Virtual,
        partitions,
    }
}

/// Builds the capture plan for a synthetic disk.
pub fn capture_spec_for(
    disk: &SyntheticDisk,
    capture: CaptureMethod,
    limit: Option<u64>,
) -> CaptureSpec {
    let entry = disk_entry_for(disk);
    let partitions = entry
        .partitions
        .iter()
        .map(|p| PartitionCapture {
            partition_id: p.id.clone(),
            stream_id: StreamId::new(format!("disk-0-part-{}", p.number)).unwrap(),
            capture,
            planned_bytes: limit.map(|l| l.min(p.length)).unwrap_or(p.length),
            source_description: format!("partition {} of the synthetic disk", p.number),
        })
        .collect();

    // The head runs from byte zero to the first partition; the tail is the
    // space the secondary partition table needs at the end.
    let first_partition = entry
        .partitions
        .iter()
        .map(|p| p.starting_offset)
        .min()
        .unwrap_or(1 << 20);

    CaptureSpec {
        disk_id: entry.id.clone(),
        disk: entry,
        head_bytes: first_partition,
        tail_bytes: mjolnir_storage::gpt::secondary_gpt_span(disk.sector_size).unwrap(),
        partitions,
    }
}

/// Takes a complete, verified backup of a synthetic disk.
///
/// Returns the backup folder. Follows the same order a real backup does:
/// capture, write the documents, verify, and only then mark it complete.
pub fn back_up(disk: &SyntheticDisk, destination: &Path, name: &str) -> Result<PathBuf> {
    back_up_with(disk, destination, name, CaptureMethod::RawFull, None)
}

/// Takes a backup with a specific capture method and optional byte limit.
pub fn back_up_with(
    disk: &SyntheticDisk,
    destination: &Path,
    name: &str,
    capture: CaptureMethod,
    limit: Option<u64>,
) -> Result<PathBuf> {
    let backup_name = BackupName::new(name).expect("valid test backup name");
    let mut writer = BackupWriter::create(
        destination,
        &backup_name,
        BackupInfo {
            uuid: "99999999-8888-7777-6666-555555555555".to_owned(),
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

    let spec = capture_spec_for(disk, capture, limit);
    let mut sources = SyntheticSources::new(disk);
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
        "a freshly written backup failed its own verification: {:?}",
        report.issues
    );

    finalized.mark_complete(
        report.to_record(UtcTimestamp::now()),
        &UtcTimestamp::now().to_rfc3339(),
    )?;

    Ok(dir)
}

/// A restore target describing a blank disk of `size_bytes`.
pub fn target_disk(number: u32, size_bytes: u64, sector_size: u32) -> mjolnir_restore::TargetDisk {
    mjolnir_restore::TargetDisk {
        number,
        device_path: format!("\\\\.\\PhysicalDrive{number}"),
        size_bytes,
        logical_sector_size: sector_size,
        model: Some("Blank Replacement".to_owned()),
        serial: Some("REPL0001".to_owned()),
        bus: "NVMe".to_owned(),
        existing_partitions: Vec::new(),
        holds_the_backup: false,
    }
}

/// Restores a finished backup onto a blank file backed disk and returns its
/// bytes.
///
/// The target is created blank, which is what the restore engine requires, so
/// anything nonzero in the result was written by the restore.
pub fn restore_to_file(backup_dir: &Path, temp: &Path, target_size: u64) -> Vec<u8> {
    let set = mjolnir_image::BackupSet::open(backup_dir).expect("the backup should open");
    let sector_size = set
        .disk_layout()
        .disks
        .first()
        .map(|d| d.logical_sector_size)
        .unwrap_or(512);
    let target = target_disk(1, target_size, sector_size);
    let plan = mjolnir_restore::plan(&set, &target).expect("the restore should plan");
    let confirmation = mjolnir_restore::EraseConfirmation::check(&target, &target.erase_phrase())
        .expect("the phrase should be accepted");

    let name = backup_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "restored".to_owned());
    let mut device = mjolnir_testkit::FileBlockDevice::create(
        temp.join(format!("{name}-restored.img")),
        target_size,
        sector_size,
    )
    .expect("create target");

    let mut progress = SilentProgress;
    mjolnir_restore::restore(
        &set,
        &plan,
        &target,
        &confirmation,
        &mut device,
        &mut progress,
        &CancelToken::new(),
    )
    .expect("the restore should succeed");

    device.read_all().expect("read the restored disk")
}

/// Reads a finished backup's manifest.
pub fn read_manifest(backup_dir: &Path) -> mjolnir_image::manifest::Manifest {
    mjolnir_image::BackupSet::open(backup_dir)
        .expect("the backup should open")
        .manifest()
        .clone()
}

/// Verifies a finished backup from its folder, as a later check would.
pub fn verify_backup(backup_dir: &Path) -> VerifyReport {
    let set = mjolnir_image::BackupSet::open(backup_dir).expect("the backup should open");
    mjolnir_image::verify::verify(
        set.manifest(),
        Some(set.disk_layout()),
        &set.chunk_store(),
        VerifyDepth::Full,
        &mut SilentProgress,
        &CancelToken::new(),
    )
    .expect("verification should run")
}

/// Progress that pulls the plug part way through.
///
/// Cancelling before anything starts only proves the check at the door. This
/// cancels after real work has been done, which is what somebody pressing
/// Cancel actually does.
pub struct CancelAfter {
    cancel: CancelToken,
    remaining: usize,
}

impl CancelAfter {
    /// Cancels `token` after `updates` progress reports.
    pub fn new(token: &CancelToken, updates: usize) -> Self {
        Self {
            cancel: token.clone(),
            remaining: updates,
        }
    }

    fn tick(&mut self) {
        if self.remaining == 0 {
            self.cancel.cancel();
        } else {
            self.remaining -= 1;
        }
    }
}

impl mjolnir_core::progress::Progress for CancelAfter {
    fn begin(&mut self, _phase: &str, _total: Option<u64>) {}
    fn advance(&mut self, _bytes: u64) {
        self.tick();
    }
    fn end(&mut self) {}
    fn note(&mut self, _message: &str) {}
}
