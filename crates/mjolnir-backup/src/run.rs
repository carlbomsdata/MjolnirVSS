//! Running a backup.
//!
//! The order of operations is the whole point of this module, so it is spelled
//! out here rather than left to be inferred from the code below:
//!
//! 1. create the backup folder, so a failure to write is found before a shadow
//!    copy has been taken;
//! 2. take one coordinated shadow copy of every NTFS volume on the system disk,
//!    so that everything in the backup comes from the same instant;
//! 3. copy the partitions, reading NTFS from the shadow copy and the boot
//!    partitions from the disk;
//! 4. tell the writers the backup is finished and delete the shadow copy, which
//!    also happens on every failure path because the session owns it;
//! 5. write `manifest.json` and `disk-layout.json`;
//! 6. verify every stored chunk by decompressing and hashing it;
//! 7. only then write `completion.json`.
//!
//! Until step seven the folder is not a backup, and nothing will restore from
//! it. That is what makes an interrupted run safe.

use std::path::PathBuf;
use std::time::Instant;

use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::progress::Progress;
use mjolnir_core::timestamp::UtcTimestamp;
use mjolnir_image::completion::{Verification, VerificationResult};
use mjolnir_image::disk_layout::{DiskEntry, PartitionEntry};
use mjolnir_image::issue::IssueList as _;
use mjolnir_image::manifest::{
    BackupInfo, BackupKind, SnapshotEntry, SourceInfo, VolumeEntry, VssInfo, WriterStatusEntry,
};
use mjolnir_image::verify::{VerifyDepth, VerifyReport};
use mjolnir_image::writer::{BackupWriter, WriterOptions};
use mjolnir_storage::device::Device;

use crate::log::RunLog;
use crate::plan::{BackupPlan, BackupRequest};

/// Plain language stage names, shown to the operator.
///
/// The graphical interface matches on these, so they are constants rather than
/// literals scattered through the code.
pub mod stages {
    /// Taking the shadow copy.
    pub const PREPARING_SNAPSHOT: &str = "Preparing snapshot";
    /// Reading the system disk.
    pub const READING_SYSTEM: &str = "Reading system";
    /// Writing the backup to the destination.
    pub const WRITING_BACKUP: &str = "Writing backup";
    /// Checking what was written.
    pub const VERIFYING_BACKUP: &str = "Verifying backup";
    /// Done.
    pub const COMPLETED: &str = "Completed";
}

/// What a finished backup produced.
#[derive(Debug, Clone)]
pub struct BackupOutcome {
    /// The folder the backup was written to.
    pub backup_dir: PathBuf,
    /// Bytes read from the source.
    pub captured_bytes: u64,
    /// Bytes the backup occupies on the destination.
    pub stored_bytes: u64,
    /// How many distinct chunks were stored.
    pub unique_chunks: u64,
    /// What verification found.
    pub verification: Verification,
    /// How long the run took.
    pub elapsed_seconds: f64,
    /// Things worth telling the operator.
    pub warnings: Vec<String>,
    /// Whether this backup can be restored from.
    pub restorable: bool,
}

impl BackupOutcome {
    /// How much smaller the backup is than the data it holds.
    pub fn compression_ratio(&self) -> f64 {
        if self.stored_bytes == 0 {
            return 0.0;
        }
        self.captured_bytes as f64 / self.stored_bytes as f64
    }
}

/// Runs a backup that has already been planned.
pub fn run(
    request: &BackupRequest,
    plan: &BackupPlan,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<BackupOutcome> {
    let started = Instant::now();
    let now = UtcTimestamp::now();
    let mut log = RunLog::new();

    log.line(format!(
        "MjolnirVSS {} starting a backup of {}",
        mjolnir_core::TOOL_VERSION,
        plan.system.computer_name
    ));
    for line in plan.summary_lines() {
        log.line(line);
    }
    for warning in &plan.warnings {
        log.line(format!("Warning: {warning}"));
    }

    let backup = BackupInfo {
        uuid: new_uuid(now),
        name: request.name.clone(),
        created_utc: now.to_rfc3339(),
        kind: BackupKind::Full,
        scope: if plan.is_preview {
            format!("{} (preview, not restorable)", request.scope.as_str())
        } else {
            request.scope.as_str().to_owned()
        },
    };
    let source = SourceInfo {
        machine_id: plan.system.machine_id.clone(),
        computer_name: plan.system.computer_name.clone(),
        windows: plan.system.windows.clone(),
        firmware: plan.system.firmware,
    };

    let writer = BackupWriter::create(
        &request.destination,
        &request.name,
        backup,
        source,
        WriterOptions::default(),
    )?;
    let backup_dir = writer.layout().dir().to_path_buf();
    log.line(format!("Writing to {}", backup_dir.display()));

    // From here on, a failure has to leave the folder behind without a
    // completion marker rather than half deleted, so the operator can see what
    // happened and delete it themselves.
    let outcome = run_inner(request, plan, writer, progress, cancel, &mut log);

    match outcome {
        Ok(report) => {
            let verification = report.verification.clone();
            log.line(format!(
                "Verification {}: {} chunks, {} hashed",
                match verification.result {
                    VerificationResult::Passed => "passed",
                    VerificationResult::Failed => "FAILED",
                    VerificationResult::NotPerformed => "was not run",
                },
                verification.chunks_verified,
                mjolnir_core::progress::format_bytes(verification.bytes_verified)
            ));
            let _ = log.write_to(&backup_dir.join("logs").join("backup.log"));
            Ok(BackupOutcome {
                backup_dir,
                elapsed_seconds: started.elapsed().as_secs_f64(),
                ..report
            })
        }
        Err(e) => {
            log.line(format!("Backup failed: {}", e.what()));
            log.line(format!("  why:  {}", e.why()));
            log.line(format!("  next: {}", e.next_step()));
            let _ = log.write_to(&backup_dir.join("logs").join("backup.log"));
            Err(e)
        }
    }
}

fn run_inner(
    request: &BackupRequest,
    plan: &BackupPlan,
    mut writer: BackupWriter,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
    log: &mut RunLog,
) -> Result<BackupOutcome> {
    cancel.check()?;

    // ---- Step 2: one coordinated shadow copy --------------------------------
    progress.begin(stages::PREPARING_SNAPSHOT, None);
    let session = take_snapshot(plan, progress, cancel, log)?;
    progress.end();

    // ---- Step 3: copy the data ---------------------------------------------
    // The copying itself lives in capture.rs, written against BlockSource, so
    // the same code runs here and in the tests that use a synthetic disk.
    progress.begin(stages::READING_SYSTEM, Some(plan.source_bytes));
    let spec = build_capture_spec(plan, request)?;
    let mut sources = WindowsSources::new(plan, &session, cancel.clone());
    crate::capture::capture_disk(&spec, &mut sources, &mut writer, progress, cancel)?;

    for partition in &plan.partitions {
        if let (Some(volume), Some(volume_id)) = (&partition.volume, &partition.volume_id) {
            writer.add_volume(VolumeEntry {
                id: volume_id.clone(),
                disk_id: plan.disk_id.clone(),
                partition_id: partition.id.clone(),
                guid_path: Some(volume.guid_path.clone()),
                drive_letter: volume.drive_letter(),
                label: volume.label.clone(),
                filesystem: volume.filesystem.clone(),
                cluster_size: volume.cluster_size,
                total_bytes: Some(volume.total_bytes),
                used_bytes: Some(volume.used_bytes()),
                index_path: None,
            });
        }
    }
    writer.set_vss_info(describe_vss(&session, plan));
    progress.end();

    // ---- Step 4: release the shadow copy ------------------------------------
    // Done before verification so the machine is left alone while the slow part
    // runs. Dropping the session is what actually removes it, and that happens
    // on every path including a panic.
    if let Some(mut session) = session {
        if let Err(e) = session.complete(cancel) {
            log.line(format!(
                "Warning: telling applications the backup finished failed: {}",
                e.what()
            ));
        }
        match session.delete_own_snapshots() {
            Ok(n) => log.line(format!("Removed {n} temporary shadow copies")),
            Err(e) => log.line(format!("Warning: {}", e.what())),
        }
    }

    // ---- Steps 5 to 7: documents, verification, completion ------------------
    progress.begin(stages::WRITING_BACKUP, None);
    let finalized = writer.finalize()?;
    progress.end();

    progress.begin(stages::VERIFYING_BACKUP, None);
    let store = finalized.chunk_store();
    let report: VerifyReport = mjolnir_image::verify::verify(
        finalized.manifest(),
        Some(finalized.disk_layout()),
        &store,
        VerifyDepth::Full,
        progress,
        cancel,
    )?;
    progress.end();

    for uncovered in &report.uncovered {
        log.line(format!(
            "Stream {} does not carry {} across {} gaps (unallocated space is restored as zeroes)",
            uncovered.stream,
            mjolnir_core::progress::format_bytes(uncovered.bytes),
            uncovered.gaps
        ));
    }
    for issue in report.issues.iter().filter(|i| i.is_error()) {
        log.line(format!("Verification problem: {issue}"));
    }

    let verification = report.to_record(UtcTimestamp::now());
    let stats = finalized.manifest().stats;
    let restorable = report.passed() && !plan.is_preview;

    if report.passed() {
        finalized.mark_complete(verification.clone(), &UtcTimestamp::now().to_rfc3339())?;
        progress.begin(stages::COMPLETED, None);
        progress.end();
    } else {
        // Recorded rather than silently left incomplete, so the operator is
        // told what happened instead of finding an unexplained folder later.
        finalized.mark_failed(verification.clone(), &UtcTimestamp::now().to_rfc3339())?;
        return Err(Error::corrupt(
            "the backup was written but failed verification",
            format!(
                "{} of the stored chunks did not match what was read from the disk; the folder has been marked as failed and must not be restored from",
                report.issues.error_count()
            ),
            "check the destination drive's health, then take the backup again; see logs/backup.log inside the backup folder for the full list",
        ));
    }

    let mut warnings = plan.warnings.clone();
    if plan.is_preview {
        warnings.push(
            "This preview backup cannot be restored. Run a full backup for a usable one."
                .to_owned(),
        );
    }

    Ok(BackupOutcome {
        backup_dir: PathBuf::new(), // filled in by the caller
        captured_bytes: stats.captured_bytes,
        stored_bytes: stats.stored_bytes,
        unique_chunks: stats.unique_chunks,
        verification,
        elapsed_seconds: 0.0, // filled in by the caller
        warnings,
        restorable,
    })
}

#[cfg(windows)]
type Session = Option<mjolnir_vss::VssSession>;
#[cfg(not(windows))]
type Session = Option<()>;

#[cfg(windows)]
fn take_snapshot(
    plan: &BackupPlan,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
    log: &mut RunLog,
) -> Result<Session> {
    if plan.snapshot_volumes.is_empty() {
        log.line("No volumes needed a shadow copy");
        return Ok(None);
    }

    progress.note("Asking Windows to prepare applications for the backup");
    let mut session = mjolnir_vss::VssSession::begin(cancel)?;

    for volume in &plan.snapshot_volumes {
        if !session.is_volume_supported(volume)? {
            return Err(Error::unsupported(
                format!("the volume {volume} cannot be shadow copied"),
                "the Volume Shadow Copy Service reports that no provider handles this volume, which usually means it is not NTFS or is locked",
                "check that the Windows volume is NTFS and unlocked, then try again",
            ));
        }
    }

    progress.note("Creating the shadow copy");
    let snapshots = session.snapshot(&plan.snapshot_volumes, cancel)?;
    for s in &snapshots {
        log.line(format!(
            "Shadow copy of {} at {}",
            s.original_volume, s.device_object
        ));
    }

    // A writer that failed has left the volume in a state the snapshot may not
    // represent correctly, so the backup stops rather than producing something
    // that looks fine and is not.
    let writers = session.writer_status(cancel)?;
    let failed: Vec<String> = writers
        .iter()
        .filter(|w| !w.succeeded())
        .map(|w| format!("{} ({})", w.name, w.state_text))
        .collect();
    if !failed.is_empty() {
        return Err(Error::new(
            ExitCode::VssFailure,
            format!(
                "{} of the applications preparing for the backup reported a failure",
                failed.len()
            ),
            format!(
                "these did not finish cleanly, so the snapshot may not hold a consistent copy of what they were writing: {}",
                failed.join(", ")
            ),
            "restart the computer and try again; run `vssadmin list writers` from an administrator command prompt to see which one is failing",
        ));
    }
    log.line(format!("All {} writers reported success", writers.len()));

    Ok(Some(session))
}

#[cfg(not(windows))]
fn take_snapshot(
    _plan: &BackupPlan,
    _progress: &mut dyn Progress,
    _cancel: &CancelToken,
    _log: &mut RunLog,
) -> Result<Session> {
    Err(Error::unsupported(
        "backups can only be taken on Windows",
        "the Volume Shadow Copy Service is a Windows feature",
        "run MjolnirVSS on the Windows computer you want to back up",
    ))
}

#[cfg(windows)]
fn describe_vss(session: &Session, _plan: &BackupPlan) -> VssInfo {
    let Some(session) = session else {
        return VssInfo::default();
    };
    VssInfo {
        used: true,
        snapshot_set_id: session
            .snapshot_set_id()
            .map(|g| mjolnir_storage::disks::guid_to_string(&g)),
        context: Some("backup".to_owned()),
        writers_succeeded: true,
        writers: Vec::<WriterStatusEntry>::new(),
        snapshots: session
            .snapshots()
            .iter()
            .map(|s| SnapshotEntry {
                snapshot_id: mjolnir_storage::disks::guid_to_string(&s.snapshot_id),
                original_volume: s.original_volume.clone(),
                device_object: s.device_object.clone(),
            })
            .collect(),
    }
}

#[cfg(not(windows))]
fn describe_vss(_session: &Session, _plan: &BackupPlan) -> VssInfo {
    VssInfo::default()
}

/// Finds the shadow copy device standing in for a volume.
#[cfg(windows)]
fn snapshot_device_for(session: &Session, guid_path: &str) -> Option<String> {
    let session = session.as_ref()?;
    session
        .snapshots()
        .iter()
        .find(|s| {
            s.original_volume.eq_ignore_ascii_case(guid_path)
                || s.original_volume.trim_end_matches('\\') == guid_path.trim_end_matches('\\')
        })
        .map(|s| s.device_object.clone())
}

#[cfg(not(windows))]
fn snapshot_device_for(_session: &Session, _guid_path: &str) -> Option<String> {
    None
}

fn build_disk_entry(plan: &BackupPlan) -> Result<DiskEntry> {
    let mut partitions = Vec::with_capacity(plan.partitions.len());
    for p in &plan.partitions {
        partitions.push(PartitionEntry {
            id: p.id.clone(),
            number: p.partition.number,
            type_guid: p.partition.type_guid.clone(),
            unique_guid: p.partition.unique_guid.clone(),
            name: p.partition.name.clone(),
            starting_offset: p.partition.starting_offset,
            length: p.partition.length,
            attributes: p.partition.attributes,
            role: p.role,
            filesystem: p.volume.as_ref().and_then(|v| v.filesystem.clone()),
        });
    }

    Ok(DiskEntry {
        id: plan.disk_id.clone(),
        disk_number: plan.disk.number,
        size_bytes: plan.disk.size_bytes,
        logical_sector_size: plan.disk.logical_sector_size,
        physical_sector_size: plan.disk.physical_sector_size,
        partition_style: plan.disk.partition_style,
        disk_guid: plan
            .disk
            .disk_guid
            .clone()
            .unwrap_or_else(|| "00000000-0000-0000-0000-000000000000".to_owned()),
        model: plan.disk.model.clone(),
        serial: plan.disk.serial.clone(),
        bus_type: plan.disk.bus_type,
        partitions,
    })
}

/// Builds the capture plan from the backup plan.
fn build_capture_spec(
    plan: &BackupPlan,
    request: &BackupRequest,
) -> Result<crate::capture::CaptureSpec> {
    let disk = build_disk_entry(plan)?;
    let partitions = plan
        .partitions
        .iter()
        .map(|p| crate::capture::PartitionCapture {
            partition_id: p.id.clone(),
            stream_id: p.stream_id.clone(),
            capture: p.capture,
            planned_bytes: request.limit.applies_to(p.partition.length),
            source_description: format!(
                "partition {} ({}), {}",
                p.partition.number,
                p.role.describe(),
                p.capture.describe()
            ),
        })
        .collect();

    Ok(crate::capture::CaptureSpec {
        disk_id: plan.disk_id.clone(),
        disk,
        head_bytes: plan.head_bytes,
        tail_bytes: plan.tail_bytes,
        partitions,
    })
}

/// Resolves capture sources against the real machine.
///
/// NTFS volumes are read from the shadow copy that was taken for them, so the
/// data is consistent. Everything else is read from the physical disk: the EFI
/// system partition and the Microsoft Reserved partition are not handled by the
/// shadow copy service, and Windows does not write to them while it is running.
struct WindowsSources<'a> {
    plan: &'a BackupPlan,
    #[cfg_attr(not(windows), allow(dead_code))]
    session: &'a Session,
    cancel: CancelToken,
}

impl<'a> WindowsSources<'a> {
    fn new(plan: &'a BackupPlan, session: &'a Session, cancel: CancelToken) -> Self {
        Self {
            plan,
            session,
            cancel,
        }
    }
}

impl crate::capture::CaptureSources for WindowsSources<'_> {
    fn open_disk(&mut self) -> Result<Box<dyn mjolnir_core::blockio::BlockSource>> {
        let device = Device::open_read(
            &self.plan.disk.device_path,
            self.plan.disk.logical_sector_size,
            self.plan.disk.size_bytes,
        )?;
        Ok(Box::new(device))
    }

    fn open_partition(
        &mut self,
        index: usize,
    ) -> Result<Option<Box<dyn mjolnir_core::blockio::BlockSource>>> {
        let partition = &self.plan.partitions[index];
        if !partition.needs_snapshot {
            return Ok(None);
        }
        let Some(volume) = &partition.volume else {
            return Ok(None);
        };
        let Some(device_path) = snapshot_device_for(self.session, &volume.guid_path) else {
            return Err(Error::new(
                ExitCode::VssFailure,
                format!(
                    "no shadow copy was created for partition {}",
                    partition.partition.number
                ),
                "the volume was supposed to be captured from a shadow copy, but none of the shadow copies taken match it",
                "this is an internal error; please report it with the command you ran",
            ));
        };

        // The shadow copy device is as long as the volume, which is normally
        // the whole partition. Its length is taken from the extent Windows
        // reported rather than assumed.
        let extent_length = volume
            .extents
            .first()
            .map(|e| e.length)
            .unwrap_or(partition.partition.length);
        let mut device = Device::open_read(
            &device_path,
            self.plan.disk.logical_sector_size,
            extent_length,
        )?;

        // How far this device can actually be read.
        //
        // A shadow copy covers the *filesystem*, and a filesystem is shorter
        // than the partition holding it: NTFS keeps a spare copy of its boot
        // sector in the last sector of the partition, outside the volume.
        // Reading that far through the shadow copy fails with "reached the end
        // of the file", which is how the first backup of a real machine ended.
        //
        // The device's own answer is not to be trusted here. Measured on a
        // Windows 11 machine, `IOCTL_DISK_GET_LENGTH_INFO` on a shadow copy
        // device reports the whole partition, and reads past the filesystem
        // still fail. So the filesystem is asked instead, and the device is
        // only consulted when the volume is not one this version reads.
        let readable = snapshot_readable_bytes(&device, self.plan.disk.logical_sector_size)
            .unwrap_or(extent_length)
            .min(extent_length);
        device.set_geometry(0, readable);

        Ok(Some(Box::new(device)))
    }

    fn used_blocks(&mut self, index: usize) -> Result<Option<mjolnir_ntfs::UsedBlockPlan>> {
        let partition = &self.plan.partitions[index];
        let Some(volume) = &partition.volume else {
            return Ok(None);
        };
        if !partition.needs_snapshot {
            return Ok(None);
        }
        let Some(device_path) = snapshot_device_for(self.session, &volume.guid_path) else {
            return Ok(None);
        };

        // The stream spans the whole partition, and the allocation bitmap
        // describes the volume. When the two are not the same length, every
        // offset in the plan would be measured against a different origin from
        // the one the stream uses, so the whole partition is copied instead.
        let extent_length = volume
            .extents
            .first()
            .map(|e| e.length)
            .unwrap_or(partition.partition.length);
        if extent_length != partition.partition.length {
            return Err(Error::new(
                ExitCode::Unsupported,
                "the volume does not fill its partition",
                format!(
                    "the volume occupies {extent_length} bytes of a {} byte partition",
                    partition.partition.length
                ),
                "the partition is copied in full instead",
            ));
        }

        let device = Device::open_read(
            &device_path,
            self.plan.disk.logical_sector_size,
            extent_length,
        )?;

        // The boot sector is read from the shadow copy, not from the live
        // volume, so the geometry describes the same frozen image the bitmap
        // and the data come from.
        let sector_size = self.plan.disk.logical_sector_size.max(512) as usize;
        let mut sector = vec![0u8; sector_size];
        device.read_at(0, &mut sector)?;
        let boot = mjolnir_ntfs::NtfsBootSector::parse(&sector)?;

        let allocation = mjolnir_storage::allocation::read_allocation(&device, &self.cancel)?;
        let plan = mjolnir_ntfs::plan_used_blocks(&boot, &allocation, partition.partition.length)?;
        Ok(Some(plan))
    }
}

/// How much of a snapshot device can be read, from the filesystem inside it.
///
/// Returns `None` when the volume is not one this version understands, in which
/// case the caller falls back to what Windows said the extent was.
#[cfg(windows)]
fn snapshot_readable_bytes(device: &Device, sector_size: u32) -> Option<u64> {
    let mut sector = vec![0u8; sector_size.max(512) as usize];
    device.read_at(0, &mut sector).ok()?;

    match mjolnir_ntfs::NtfsBootSector::parse(&sector) {
        // Whole clusters of the filesystem, which is where a shadow copy of it
        // stops. Not the sector count: NTFS counts one sector fewer than the
        // partition holds, and the last partial cluster left over from that is
        // not in the snapshot either. Measured on a Windows 11 machine, a
        // shadow copy of a 67,423,436,800 byte partition could be read to
        // 67,423,432,704, which is exactly 16,460,799 clusters of 4,096 bytes.
        Ok(boot) => {
            let whole_clusters = boot
                .total_clusters()
                .checked_mul(boot.bytes_per_cluster())?;
            let by_sectors = boot.volume_bytes().ok()?;
            Some(whole_clusters.min(by_sectors))
        }
        // Not NTFS, or not readable. The device's answer is better than
        // nothing.
        Err(_) => device.query_length(),
    }
}

/// Builds a GUID for this backup from the clock and some process entropy.
///
/// Not a version 4 UUID: MjolnirVSS has no random number generator dependency,
/// and this value only has to be unique among backups, not unpredictable.
fn new_uuid(now: UtcTimestamp) -> String {
    let seconds = now.unix_seconds() as u64;
    let pid = u64::from(std::process::id());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);

    let a = mjolnir_image::ChunkHash::of(&seconds.to_le_bytes());
    let b = mjolnir_image::ChunkHash::of(&[pid.to_le_bytes(), nanos.to_le_bytes()].concat());
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&a.as_bytes()[..8]);
    bytes[8..].copy_from_slice(&b.as_bytes()[..8]);
    mjolnir_image::format_guid(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_names_are_plain_language() {
        for stage in [
            stages::PREPARING_SNAPSHOT,
            stages::READING_SYSTEM,
            stages::WRITING_BACKUP,
            stages::VERIFYING_BACKUP,
            stages::COMPLETED,
        ] {
            assert!(!stage.is_empty());
            // No jargon: an operator should recognise every word.
            for word in ["VSS", "chunk", "blake", "zstd", "IOCTL"] {
                assert!(
                    !stage.to_lowercase().contains(&word.to_lowercase()),
                    "{stage:?} contains jargon"
                );
            }
        }
    }

    #[test]
    fn generated_uuids_are_valid_and_distinct() {
        let a = new_uuid(UtcTimestamp::from_unix_seconds(1_789_235_195));
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_uuid(UtcTimestamp::from_unix_seconds(1_789_235_195));
        assert!(mjolnir_image::is_guid(&a), "{a}");
        assert!(mjolnir_image::is_guid(&b), "{b}");
        assert_ne!(a, b, "two backups in the same second must differ");
    }

    #[test]
    fn compression_ratio_handles_an_empty_backup() {
        let outcome = BackupOutcome {
            backup_dir: PathBuf::new(),
            captured_bytes: 0,
            stored_bytes: 0,
            unique_chunks: 0,
            verification: Verification::not_performed(),
            elapsed_seconds: 0.0,
            warnings: Vec::new(),
            restorable: false,
        };
        assert_eq!(outcome.compression_ratio(), 0.0);

        let outcome = BackupOutcome {
            captured_bytes: 1000,
            stored_bytes: 250,
            ..outcome
        };
        assert_eq!(outcome.compression_ratio(), 4.0);
    }
}
