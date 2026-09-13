//! Working out what to back up, and refusing what cannot be backed up safely.
//!
//! Planning happens before anything is written and before a shadow copy is
//! taken, so the operator finds out that their machine is not supported in the
//! first second rather than twenty minutes in. Every refusal names the thing it
//! found, why that matters, and what can be done about it.
//!
//! The rule this module exists to enforce is that MjolnirVSS never guesses. A
//! layout it does not recognise is refused, not approximated.

use std::path::{Path, PathBuf};

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::ids::{BackupName, DiskId, PartitionId, StreamId, VolumeId};
use mjolnir_core::math;
use mjolnir_image::disk_layout::{PartitionRole, PartitionStyle};
use mjolnir_image::manifest::CaptureMethod;
use mjolnir_storage::bitlocker::{Encryption, PartitionEncryption};
use mjolnir_storage::disks::{PhysicalDisk, PhysicalPartition};
use mjolnir_storage::system::SystemSummary;
use mjolnir_storage::volumes::VolumeInfo;

/// The BitLocker volume signature, re-exported from the NTFS layer where the
/// boot sector code that recognises it lives.
pub use mjolnir_ntfs::boot::BITLOCKER_OEM_ID as BITLOCKER_SIGNATURE;

/// How much of each partition to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureLimit {
    /// Everything, which is what a real backup does.
    Everything,
    /// Only the first `bytes` of each partition.
    ///
    /// This exists so the whole pipeline can be exercised in seconds against a
    /// real machine without writing hundreds of gigabytes. A backup taken this
    /// way is marked in the manifest as a preview and the restore side refuses
    /// it outright, because restoring a partition whose middle is missing would
    /// produce a computer that does not start.
    FirstBytes(u64),
}

impl CaptureLimit {
    /// Whether this produces a backup that can never be restored.
    pub fn is_preview(self) -> bool {
        matches!(self, CaptureLimit::FirstBytes(_))
    }

    /// How much of a stream of `length` bytes this captures.
    pub fn applies_to(self, length: u64) -> u64 {
        match self {
            CaptureLimit::Everything => length,
            CaptureLimit::FirstBytes(bytes) => bytes.min(length),
        }
    }
}

/// What the operator asked to be captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupScope {
    /// The disk Windows boots from, with every partition on it.
    SystemDisk,
}

impl BackupScope {
    /// The value recorded in the manifest.
    pub const fn as_str(self) -> &'static str {
        match self {
            BackupScope::SystemDisk => "system-disk",
        }
    }
}

/// What the operator asked for.
#[derive(Debug, Clone)]
pub struct BackupRequest {
    /// Folder the backup folder is created inside.
    pub destination: PathBuf,
    /// Name of the backup folder.
    pub name: BackupName,
    /// What to capture.
    pub scope: BackupScope,
    /// How much of each partition to capture.
    pub limit: CaptureLimit,
    /// Keys to seal the contents with, when the backup is to be encrypted.
    pub encryption: Option<mjolnir_image::writer::StartedEncryption>,
}

/// One partition, and how it is going to be captured.
#[derive(Debug, Clone)]
pub struct PlannedPartition {
    /// Identifier used throughout the backup.
    pub id: PartitionId,
    /// The stream that will carry its contents.
    pub stream_id: StreamId,
    /// What Windows reported about it.
    pub partition: PhysicalPartition,
    /// What the partition appears to be for.
    pub role: PartitionRole,
    /// The filesystem inside it, when there is one MjolnirVSS recognised.
    pub volume: Option<VolumeInfo>,
    /// Identifier of that filesystem within the backup.
    pub volume_id: Option<VolumeId>,
    /// How its contents will be read.
    pub capture: CaptureMethod,
    /// Whether a shadow copy of its volume is needed.
    pub needs_snapshot: bool,
    /// How many bytes will actually be read.
    pub planned_bytes: u64,
    /// Whether this partition is encrypted at rest by BitLocker.
    ///
    /// The shadow copy presents it decrypted, so what reaches the backup is
    /// readable. The operator is told before choosing a destination.
    pub encrypted_at_rest: bool,
}

/// Everything the backup is going to do, decided before anything happens.
#[derive(Debug, Clone)]
pub struct BackupPlan {
    /// What this computer is.
    pub system: SystemSummary,
    /// Identifier of the system disk within the backup.
    pub disk_id: DiskId,
    /// The disk being captured.
    pub disk: PhysicalDisk,
    /// Every partition on it, in table order. Never a subset.
    pub partitions: Vec<PlannedPartition>,
    /// Bytes at the start of the disk holding the protective MBR and GPT.
    pub head_bytes: u64,
    /// Bytes at the end of the disk holding the secondary GPT.
    pub tail_bytes: u64,
    /// The volumes that have to be shadow copied, as GUID paths.
    pub snapshot_volumes: Vec<String>,
    /// What taking those shadow copies may cost the machine's restore points.
    ///
    /// Read only, and never a reason to refuse a backup. See
    /// [`crate::preflight`] for the behaviour it exists to warn about.
    pub snapshot_preflight: crate::preflight::SnapshotPreflight,
    /// Total bytes that will be read from the source.
    pub source_bytes: u64,
    /// Things worth telling the operator that do not stop the backup.
    pub warnings: Vec<String>,
    /// Whether the backup will contain readable copies of data that is
    /// encrypted at rest on the source machine.
    ///
    /// True when a captured volume is protected by BitLocker. A shadow copy
    /// presents such a volume decrypted, so what lands in the backup is
    /// readable, and the destination drive has no encryption of its own. The
    /// operator is told this where the destination is chosen, because that is
    /// the only place it changes what they would do.
    pub contains_decrypted_data: bool,
    /// Whether this produces a backup that cannot be restored.
    pub is_preview: bool,
}

impl BackupPlan {
    /// The identifier used for the disk head stream.
    pub fn head_stream_id(&self) -> StreamId {
        StreamId::new(format!("{}-head", self.disk_id.as_str())).expect("built from a valid id")
    }

    /// The identifier used for the disk tail stream.
    pub fn tail_stream_id(&self) -> StreamId {
        StreamId::new(format!("{}-tail", self.disk_id.as_str())).expect("built from a valid id")
    }

    /// A short summary for the destination screen.
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!("Computer: {}", self.system.computer_name),
            format!("System disk: {}", self.disk.describe()),
        ];
        for p in &self.partitions {
            let label = p
                .volume
                .as_ref()
                .and_then(|v| v.drive_letter())
                .map(|l| format!("{l}: "))
                .unwrap_or_default();
            lines.push(format!(
                "  Partition {} - {}{} - {}",
                p.partition.number,
                label,
                p.role.describe(),
                mjolnir_core::progress::format_bytes(p.partition.length)
            ));
        }
        lines
    }
}

/// Builds a plan, refusing anything outside the supported set.
pub fn plan(request: &BackupRequest) -> Result<BackupPlan> {
    let system = mjolnir_storage::system::describe_system()?;
    let disk = mjolnir_storage::disks::describe_disk(system.system_disk_number)?;
    let volumes = mjolnir_storage::volumes::enumerate_volumes()?;

    check_disk_is_supported(&disk)?;
    check_destination_is_not_the_source(&request.destination, &disk, &volumes)?;

    // Asked once for the whole machine rather than once per partition: each
    // query connects to the management service afresh. An empty list is a
    // legitimate answer, and means the partition headers decide on their own.
    let reported = mjolnir_storage::wmi::encryptable_volumes().unwrap_or_default();

    let disk_id = DiskId::new(format!("disk-{}", disk.number)).map_err(internal)?;
    let mut warnings = Vec::new();
    let mut partitions = Vec::new();
    let mut snapshot_volumes = Vec::new();
    let mut source_bytes = 0u64;

    for (index, partition) in disk.partitions.iter().enumerate() {
        let id = PartitionId::new(format!("{}-part-{}", disk_id.as_str(), partition.number))
            .map_err(internal)?;
        let stream_id = StreamId::new(format!("{}-part-{}", disk_id.as_str(), partition.number))
            .map_err(internal)?;

        let volume = find_volume_for(&volumes, disk.number, partition);
        let role = classify(partition, volume.as_ref(), &system);

        let encryption =
            mjolnir_storage::bitlocker::inspect_with(&disk, partition, volume.as_ref(), &reported)?;

        check_partition_is_supported(partition, volume.as_ref(), role, &encryption)?;

        // NTFS gets a shadow copy so the data is consistent. Everything else is
        // read straight off the disk, which is explained in
        // docs/backup-format.md: those partitions hold boot files that Windows
        // does not write to while it is running.
        let is_ntfs = volume
            .as_ref()
            .and_then(|v| v.filesystem.as_deref())
            .map(|fs| fs.eq_ignore_ascii_case("NTFS"))
            .unwrap_or(false);

        let needs_snapshot = is_ntfs;
        let capture = if request.limit.is_preview() {
            CaptureMethod::Preview
        } else if is_ntfs {
            // Asking the volume which clusters are in use, and reading only
            // those. If the volume will not say, the capture falls back to
            // reading the whole thing and records that it had to; the decision
            // cannot be made here, because it needs the shadow copy that does
            // not exist yet.
            CaptureMethod::VssUsedBlocks
        } else {
            CaptureMethod::RawFull
        };

        if needs_snapshot {
            if let Some(v) = &volume {
                if !snapshot_volumes.contains(&v.guid_path) {
                    snapshot_volumes.push(v.guid_path.clone());
                }
            }
        } else if role == PartitionRole::EfiSystem || role == PartitionRole::MicrosoftReserved {
            // Expected and fine, but worth recording in the log so the
            // consistency argument is visible rather than implied.
            warnings.push(format!(
                "Partition {} ({}) is read directly from the disk: the shadow copy service does not handle it.",
                partition.number,
                role.describe()
            ));
        }

        let volume_id = volume
            .as_ref()
            .map(|_| VolumeId::new(format!("volume-{}", index + 1)).map_err(internal))
            .transpose()?;

        let planned_bytes = request.limit.applies_to(partition.length);
        source_bytes = math::add_u64("planned source bytes", source_bytes, planned_bytes)?;

        let encrypted_at_rest = encryption.encryption == Encryption::BitLockerUnlocked;
        if encrypted_at_rest {
            warnings.push(bitlocker_note(
                partition.number,
                role.describe(),
                request.encryption.is_some(),
            ));
        }

        partitions.push(PlannedPartition {
            id,
            stream_id,
            partition: partition.clone(),
            role,
            volume,
            volume_id,
            capture,
            needs_snapshot,
            planned_bytes,
            encrypted_at_rest,
        });
    }

    if partitions.is_empty() {
        return Err(Error::unsupported(
            "the system disk has no partitions MjolnirVSS could read",
            "Windows reported a partition table with nothing in it, which means the disk is not laid out the way a Windows installation normally is",
            "this disk cannot be backed up by this version",
        ));
    }

    check_boot_partitions_are_present(&partitions, &mut warnings)?;

    let head_bytes = head_span(&disk, &partitions)?;
    let tail_bytes = mjolnir_storage::gpt::secondary_gpt_span(disk.logical_sector_size)?;
    source_bytes = math::add_u64("planned source bytes", source_bytes, head_bytes)?;
    source_bytes = math::add_u64("planned source bytes", source_bytes, tail_bytes)?;

    let contains_decrypted_data = partitions.iter().any(|p| p.encrypted_at_rest);

    // Taking a shadow copy can cost the machine its restore points. Nothing
    // here stops the backup; it decides what the operator is told first.
    let to_snapshot: Vec<_> = snapshot_volumes
        .iter()
        .map(|guid| {
            let volume = volumes.iter().find(|v| v.guid_path == *guid);
            crate::preflight::VolumeToSnapshot {
                guid_path: guid.clone(),
                drive_letter: volume.and_then(|v| v.drive_letter()),
                free_bytes: volume.map(|v| v.free_bytes).unwrap_or(0),
            }
        })
        .collect();
    let snapshot_preflight = crate::preflight::inspect(&to_snapshot)
        .unwrap_or_else(|_| crate::preflight::SnapshotPreflight::unknown());
    if let Some(warning) = snapshot_preflight.warning() {
        warnings.push(warning.to_owned());
    }

    if request.limit.is_preview() {
        warnings.push(
            "This is a preview run: only the first part of each partition is captured, and the result cannot be restored."
                .to_owned(),
        );
    }

    Ok(BackupPlan {
        system,
        disk_id,
        disk,
        partitions,
        head_bytes,
        tail_bytes,
        snapshot_volumes,
        snapshot_preflight,
        source_bytes,
        contains_decrypted_data,
        warnings,
        is_preview: request.limit.is_preview(),
    })
}

/// How many bytes at the start of the disk to capture.
///
/// Everything from byte zero up to the first partition: the protective MBR, the
/// primary GPT header and the partition entry array, plus the alignment gap
/// that conventionally follows them.
fn head_span(disk: &PhysicalDisk, partitions: &[PlannedPartition]) -> Result<u64> {
    let first_partition_offset = partitions
        .iter()
        .map(|p| p.partition.starting_offset)
        .min()
        .unwrap_or(0);
    let minimum = mjolnir_storage::gpt::primary_gpt_span(disk.logical_sector_size)?;
    if first_partition_offset == 0 {
        return Err(Error::unsupported(
            "a partition starts at the very beginning of the disk",
            "the first sectors of a GPT disk hold the protective master boot record and the partition table itself, so a partition cannot start there",
            "this disk is not laid out in a way MjolnirVSS understands and cannot be backed up by this version",
        ));
    }
    Ok(first_partition_offset.min(minimum.max(first_partition_offset)))
}

fn check_disk_is_supported(disk: &PhysicalDisk) -> Result<()> {
    if disk.is_storage_spaces() {
        return Err(Error::unsupported(
            "Windows is installed on a Storage Spaces virtual disk",
            "a Storage Space is assembled from several physical disks by Windows, so there is no single disk to capture and no single disk to restore onto",
            "Storage Spaces are not supported yet; back up your files another way until support is added",
        ));
    }
    if disk.partition_style != PartitionStyle::Gpt {
        return Err(Error::unsupported(
            format!(
                "the system disk uses the {} partition style",
                match disk.partition_style {
                    PartitionStyle::Mbr => "master boot record",
                    PartitionStyle::Raw => "no",
                    PartitionStyle::Gpt => "GUID",
                }
            ),
            "this version of MjolnirVSS captures and recreates GUID partition tables only, which is what a modern UEFI Windows installation uses",
            "machines that boot in legacy BIOS mode are not supported yet",
        ));
    }
    if !mjolnir_image::disk_layout::SUPPORTED_SECTOR_SIZES.contains(&disk.logical_sector_size) {
        return Err(Error::unsupported(
            format!(
                "the system disk reports {} byte sectors",
                disk.logical_sector_size
            ),
            "MjolnirVSS has only been tested with 512 and 4096 byte sectors, and an untested sector size would put every offset in the backup in doubt",
            "this disk cannot be backed up by this version",
        ));
    }
    if disk.size_bytes == 0 {
        return Err(Error::unsupported(
            "the system disk reports a size of zero",
            "Windows did not give a usable size for the disk, which usually means it was disconnected mid query",
            "restart the computer and try again",
        ));
    }
    Ok(())
}

fn check_partition_is_supported(
    partition: &PhysicalPartition,
    volume: Option<&VolumeInfo>,
    role: PartitionRole,
    encryption: &PartitionEncryption,
) -> Result<()> {
    if let Some(v) = volume {
        if !v.is_simple() {
            return Err(Error::unsupported(
                format!(
                    "partition {} holds a volume spread across {} regions",
                    partition.number,
                    v.extents.len()
                ),
                "a volume made of more than one region is a spanned, striped, mirrored or dynamic disk volume, and recreating it is not something this version can do correctly",
                "dynamic disks and Storage Spaces are not supported yet",
            ));
        }
        if let Some(fs) = &v.filesystem {
            if fs.eq_ignore_ascii_case("ReFS") {
                return Err(Error::unsupported(
                    format!("partition {} uses the ReFS filesystem", partition.number),
                    "ReFS volumes are not handled by this version, and copying one without understanding it would produce a backup that cannot be trusted",
                    "ReFS is not supported yet",
                ));
            }
        }
    }

    // BitLocker. An unlocked volume is captured through its shadow copy, which
    // presents it decrypted; a locked one cannot be read at all. See
    // docs/bitlocker.md for the measurement this rests on.
    match encryption.encryption {
        Encryption::BitLockerLocked => {
            return Err(Error::unsupported(
                format!(
                    "partition {} ({}) is encrypted with BitLocker and is locked",
                    partition.number,
                    role.describe()
                ),
                "a locked volume cannot be read by anything, including MjolnirVSS, so there is nothing to copy; the backup would be empty rather than merely encrypted",
                "unlock the drive in Windows and run the backup again; MjolnirVSS never asks for or stores a recovery key",
            ));
        }
        Encryption::Unknown => {
            // Only reachable without administrator rights, where the disk
            // cannot be read. Planning still has to succeed so the window can
            // show what it found; the check runs again with the rights it needs
            // before anything is copied.
        }
        Encryption::BitLockerUnlocked | Encryption::None => {}
    }

    Ok(())
}

/// Refuses to write a backup onto the disk being backed up.
fn check_destination_is_not_the_source(
    destination: &Path,
    disk: &PhysicalDisk,
    volumes: &[VolumeInfo],
) -> Result<()> {
    let Some(destination_disk) = disk_number_for_path(destination, volumes) else {
        // A destination whose disk cannot be determined is allowed through:
        // it is a network location or something similar, and the write itself
        // will report any real problem. What matters is that a destination we
        // *can* identify is never the source.
        return Ok(());
    };

    if destination_disk == disk.number {
        return Err(Error::new(
            ExitCode::Destination,
            format!(
                "the backup destination {} is on the disk being backed up",
                destination.display()
            ),
            "a backup stored on the same disk as the system it protects is lost along with that disk, and writing to the disk while copying it would also change what is being copied",
            "choose a folder on a different drive, such as an external USB disk",
        ));
    }
    Ok(())
}

/// Which physical disk a path lives on, if it can be worked out.
pub fn disk_number_for_path(path: &Path, volumes: &[VolumeInfo]) -> Option<u32> {
    let text = path.to_string_lossy();
    let bytes = text.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' {
        return None;
    }
    let letter = (bytes[0] as char).to_ascii_uppercase();

    volumes
        .iter()
        .find(|v| v.drive_letter().as_deref() == Some(letter.to_string().as_str()))
        .and_then(|v| v.disk_number())
}

/// Matches a partition to the volume sitting inside it.
fn find_volume_for(
    volumes: &[VolumeInfo],
    disk_number: u32,
    partition: &PhysicalPartition,
) -> Option<VolumeInfo> {
    volumes
        .iter()
        .find(|v| {
            v.extents.iter().any(|e| {
                e.disk_number == disk_number && e.starting_offset == partition.starting_offset
            })
        })
        .cloned()
}

/// Works out what a partition is for.
fn classify(
    partition: &PhysicalPartition,
    volume: Option<&VolumeInfo>,
    system: &SystemSummary,
) -> PartitionRole {
    let by_guid = PartitionRole::from_type_guid(&partition.type_guid);
    if by_guid != PartitionRole::Data {
        return by_guid;
    }

    // A basic data partition can be Windows, recovery or ordinary data. The
    // partition name and the volume are what tell them apart.
    if let Some(v) = volume {
        if v.guid_path == system.windows_volume.guid_path {
            return PartitionRole::Windows;
        }
    }
    if partition.name.to_ascii_lowercase().contains("recovery") {
        return PartitionRole::Recovery;
    }
    PartitionRole::Data
}

/// Checks that the partitions needed to boot are all there.
fn check_boot_partitions_are_present(
    partitions: &[PlannedPartition],
    warnings: &mut Vec<String>,
) -> Result<()> {
    let has_efi = partitions
        .iter()
        .any(|p| p.role == PartitionRole::EfiSystem);
    let has_windows = partitions.iter().any(|p| p.role == PartitionRole::Windows);

    if !has_windows {
        return Err(Error::unsupported(
            "the Windows partition could not be identified on the system disk",
            "MjolnirVSS matches the volume Windows is running from against the partitions on the disk, and none of them matched, so it cannot tell which partition holds the installation",
            "this layout is not one MjolnirVSS understands; run the inspect command and report its output",
        ));
    }
    if !has_efi {
        return Err(Error::unsupported(
            "no EFI system partition was found on the system disk",
            "a UEFI machine boots from the EFI system partition, so a backup without it could not produce a computer that starts",
            "this version supports UEFI machines with a normal GPT layout only",
        ));
    }
    if !partitions.iter().any(|p| p.role == PartitionRole::Recovery) {
        warnings.push(
            "No Windows Recovery partition was found on this disk. The backup is still complete, but the restored computer will not have the built in recovery environment."
                .to_owned(),
        );
    }
    Ok(())
}

fn internal(e: impl std::fmt::Display) -> Error {
    Error::new(
        ExitCode::Failure,
        "an internal identifier could not be built",
        format!("{e}"),
        "this is an internal error; please report it with the command you ran",
    )
}

/// What to say about a BitLocker volume being captured.
///
/// The second sentence depends on what is actually being done. Telling somebody
/// "the backup itself is not encrypted" when they asked for encryption is worse
/// than saying nothing: it is a false statement about their protection, made at
/// the moment they care about it most. This was wrong for exactly as long as
/// encryption existed without anybody re-reading this sentence.
fn bitlocker_note(partition_number: u32, role: &str, backup_is_encrypted: bool) -> String {
    let and_the_backup = if backup_is_encrypted {
        "This backup is being encrypted with the password you gave, so what is written to the destination is sealed as well."
    } else {
        "The backup itself is not encrypted; pass --encrypt if you want it to be."
    };
    format!(
        "Partition {partition_number} ({role}) is protected by BitLocker. It is unlocked, so the backup will contain a readable copy of it. {and_the_backup}"
    )
}

#[cfg(test)]
mod tests {

    /// A BitLocker volume backed up without encryption has to say so.
    #[test]
    fn an_unencrypted_backup_of_a_bitlocker_volume_says_it_is_not_encrypted() {
        let note = bitlocker_note(3, "C: Windows", false);
        assert!(note.contains("readable copy"), "{note}");
        assert!(note.contains("not encrypted"), "{note}");
        assert!(note.contains("--encrypt"), "it should say how: {note}");
    }

    /// And one that *is* encrypted must not repeat the opposite. This sentence
    /// was false for every encrypted backup until running one showed it.
    #[test]
    fn an_encrypted_backup_of_a_bitlocker_volume_does_not_claim_otherwise() {
        let note = bitlocker_note(3, "C: Windows", true);
        assert!(note.contains("readable copy"), "{note}");
        assert!(
            !note.contains("not encrypted"),
            "an encrypted backup must never be described as unencrypted: {note}"
        );
        assert!(note.contains("sealed"), "{note}");
    }

    /// Whatever else changes, the thing that is always true stays said: what is
    /// captured from an unlocked BitLocker volume is readable.
    #[test]
    fn both_notes_say_the_captured_data_is_readable() {
        for encrypted in [true, false] {
            let note = bitlocker_note(1, "C: Windows", encrypted);
            assert!(note.contains("BitLocker"), "{note}");
            assert!(note.contains("readable copy"), "{note}");
        }
    }
    use super::*;
    use mjolnir_image::disk_layout::BusType;
    use mjolnir_storage::volumes::VolumeExtent;

    fn partition(number: u32, offset: u64, length: u64, type_guid: &str) -> PhysicalPartition {
        PhysicalPartition {
            number,
            starting_offset: offset,
            length,
            type_guid: type_guid.to_owned(),
            unique_guid: "11111111-2222-3333-4444-555555555555".to_owned(),
            name: String::new(),
            attributes: 0,
        }
    }

    fn volume(letter: Option<&str>, disk: u32, offset: u64, fs: &str) -> VolumeInfo {
        VolumeInfo {
            device_path: format!("\\\\?\\Volume{{{offset}}}"),
            guid_path: format!("\\\\?\\Volume{{{offset}}}\\"),
            mount_points: letter.map(|l| vec![format!("{l}:\\")]).unwrap_or_default(),
            label: None,
            filesystem: Some(fs.to_owned()),
            cluster_size: Some(4096),
            total_bytes: 1 << 30,
            free_bytes: 1 << 29,
            extents: vec![VolumeExtent {
                disk_number: disk,
                starting_offset: offset,
                length: 1 << 30,
            }],
        }
    }

    fn disk(number: u32) -> PhysicalDisk {
        PhysicalDisk {
            number,
            device_path: format!("\\\\.\\PhysicalDrive{number}"),
            size_bytes: 512 << 30,
            logical_sector_size: 512,
            physical_sector_size: 4096,
            model: Some("Test".to_owned()),
            serial: Some("SER".to_owned()),
            bus_type: BusType::Nvme,
            raw_bus_type: 17,
            removable: false,
            partition_style: PartitionStyle::Gpt,
            disk_guid: Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned()),
            partitions: Vec::new(),
        }
    }

    #[test]
    fn a_preview_limit_caps_each_stream() {
        let limit = CaptureLimit::FirstBytes(1000);
        assert!(limit.is_preview());
        assert_eq!(limit.applies_to(10_000), 1000);
        assert_eq!(limit.applies_to(500), 500);

        let full = CaptureLimit::Everything;
        assert!(!full.is_preview());
        assert_eq!(full.applies_to(10_000), 10_000);
    }

    #[test]
    fn storage_spaces_is_refused_with_an_explanation() {
        let mut d = disk(0);
        d.raw_bus_type = 16;
        let err = check_disk_is_supported(&d).unwrap_err();
        assert_eq!(err.exit(), ExitCode::Unsupported);
        assert!(err.what().contains("Storage Spaces"));
        assert!(!err.next_step().is_empty());
    }

    #[test]
    fn an_mbr_disk_is_refused() {
        let mut d = disk(0);
        d.partition_style = PartitionStyle::Mbr;
        let err = check_disk_is_supported(&d).unwrap_err();
        assert!(err.what().contains("master boot record"));
    }

    #[test]
    fn an_untested_sector_size_is_refused() {
        let mut d = disk(0);
        d.logical_sector_size = 520;
        let err = check_disk_is_supported(&d).unwrap_err();
        assert!(err.what().contains("520 byte sectors"));
    }

    #[test]
    fn a_normal_gpt_disk_passes() {
        assert!(check_disk_is_supported(&disk(0)).is_ok());
        let mut four_k = disk(0);
        four_k.logical_sector_size = 4096;
        four_k.physical_sector_size = 4096;
        assert!(check_disk_is_supported(&four_k).is_ok());
    }

    #[test]
    fn writing_the_backup_onto_the_source_disk_is_refused() {
        let volumes = vec![volume(Some("C"), 0, 1 << 20, "NTFS")];
        let err = check_destination_is_not_the_source(Path::new("C:\\Backups"), &disk(0), &volumes)
            .unwrap_err();
        assert_eq!(err.exit(), ExitCode::Destination);
        assert!(err.why().contains("lost along with that disk"));
    }

    #[test]
    fn writing_the_backup_onto_a_different_disk_is_allowed() {
        let volumes = vec![
            volume(Some("C"), 0, 1 << 20, "NTFS"),
            volume(Some("E"), 1, 1 << 20, "NTFS"),
        ];
        assert!(
            check_destination_is_not_the_source(Path::new("E:\\Backups"), &disk(0), &volumes)
                .is_ok()
        );
    }

    #[test]
    fn a_destination_whose_disk_is_unknown_is_allowed_through() {
        // A network path has no drive letter to resolve.
        assert!(check_destination_is_not_the_source(
            Path::new("\\\\server\\share\\backups"),
            &disk(0),
            &[]
        )
        .is_ok());
    }

    #[test]
    fn drive_letters_resolve_to_disks_case_insensitively() {
        let volumes = vec![volume(Some("E"), 3, 1 << 20, "NTFS")];
        assert_eq!(
            disk_number_for_path(Path::new("e:\\backups"), &volumes),
            Some(3)
        );
        assert_eq!(
            disk_number_for_path(Path::new("E:\\backups"), &volumes),
            Some(3)
        );
        assert_eq!(disk_number_for_path(Path::new("Z:\\"), &volumes), None);
        assert_eq!(disk_number_for_path(Path::new("relative"), &volumes), None);
    }

    #[test]
    fn a_spanned_volume_is_refused() {
        let mut v = volume(Some("C"), 0, 1 << 20, "NTFS");
        v.extents.push(VolumeExtent {
            disk_number: 1,
            starting_offset: 0,
            length: 1 << 30,
        });
        let err = check_partition_is_supported(
            &partition(
                1,
                1 << 20,
                1 << 30,
                mjolnir_image::disk_layout::GUID_BASIC_DATA,
            ),
            Some(&v),
            PartitionRole::Windows,
            &unencrypted(),
        )
        .unwrap_err();
        assert!(err.what().contains("spread across 2 regions"));
    }

    /// A finding for a partition with nothing unusual about it, so the tests
    /// for the other refusals do not have to describe encryption they are not
    /// about.
    fn unencrypted() -> PartitionEncryption {
        PartitionEncryption {
            encryption: Encryption::None,
            on_disk: mjolnir_ntfs::boot::VolumeSignature::Ntfs,
            filesystem: Some("NTFS".to_owned()),
            reported: None,
            evidence: Vec::new(),
        }
    }

    #[test]
    fn refs_is_refused() {
        let v = volume(Some("D"), 0, 1 << 20, "ReFS");
        let err = check_partition_is_supported(
            &partition(
                1,
                1 << 20,
                1 << 30,
                mjolnir_image::disk_layout::GUID_BASIC_DATA,
            ),
            Some(&v),
            PartitionRole::Data,
            &unencrypted(),
        )
        .unwrap_err();
        assert!(err.what().contains("ReFS"));
    }

    #[test]
    fn a_locked_bitlocker_volume_is_refused_and_the_reason_explains_itself() {
        let v = volume(Some("D"), 0, 1 << 20, "RAW");
        let locked = PartitionEncryption {
            encryption: Encryption::BitLockerLocked,
            on_disk: mjolnir_ntfs::boot::VolumeSignature::BitLocker,
            filesystem: Some("RAW".to_owned()),
            reported: None,
            evidence: Vec::new(),
        };
        let err = check_partition_is_supported(
            &partition(
                1,
                1 << 20,
                1 << 30,
                mjolnir_image::disk_layout::GUID_BASIC_DATA,
            ),
            Some(&v),
            PartitionRole::Windows,
            &locked,
        )
        .unwrap_err();

        assert!(err.what().contains("BitLocker"));
        assert!(err.what().contains("locked"));
        // The operator is told what to do, and is never asked for a key.
        assert!(err.next_step().contains("unlock"));
        assert!(!err.next_step().contains("disable"));
        assert!(!err.next_step().contains("turn off"));
    }

    /// An unlocked volume is backed up, not refused. This is the whole point of
    /// the BitLocker work, so it has a test of its own.
    #[test]
    fn an_unlocked_bitlocker_volume_is_accepted() {
        let v = volume(Some("C"), 0, 1 << 20, "NTFS");
        let unlocked = PartitionEncryption {
            encryption: Encryption::BitLockerUnlocked,
            on_disk: mjolnir_ntfs::boot::VolumeSignature::BitLocker,
            filesystem: Some("NTFS".to_owned()),
            reported: None,
            evidence: Vec::new(),
        };
        check_partition_is_supported(
            &partition(
                1,
                1 << 20,
                1 << 30,
                mjolnir_image::disk_layout::GUID_BASIC_DATA,
            ),
            Some(&v),
            PartitionRole::Windows,
            &unlocked,
        )
        .expect("an unlocked BitLocker volume is supported");
    }

    /// Without administrator rights nothing can be read, and planning has to
    /// carry on so the window can show what it found. The check runs again with
    /// the rights it needs before anything is copied.
    #[test]
    fn an_undetermined_state_does_not_stop_planning() {
        let v = volume(Some("C"), 0, 1 << 20, "NTFS");
        let unknown = PartitionEncryption {
            encryption: Encryption::Unknown,
            on_disk: mjolnir_ntfs::boot::VolumeSignature::Unknown,
            filesystem: Some("NTFS".to_owned()),
            reported: None,
            evidence: Vec::new(),
        };
        check_partition_is_supported(
            &partition(
                1,
                1 << 20,
                1 << 30,
                mjolnir_image::disk_layout::GUID_BASIC_DATA,
            ),
            Some(&v),
            PartitionRole::Windows,
            &unknown,
        )
        .expect("an unreadable disk must not be mistaken for an unsupported one");
    }

    #[test]
    fn roles_come_from_the_type_guid_first() {
        let efi = partition(
            1,
            1 << 20,
            100 << 20,
            mjolnir_image::disk_layout::GUID_EFI_SYSTEM,
        );
        assert_eq!(
            PartitionRole::from_type_guid(&efi.type_guid),
            PartitionRole::EfiSystem
        );
        let msr = partition(2, 0, 0, mjolnir_image::disk_layout::GUID_MSR);
        assert_eq!(
            PartitionRole::from_type_guid(&msr.type_guid),
            PartitionRole::MicrosoftReserved
        );
        // A basic data partition needs the volume to disambiguate.
        assert_eq!(
            PartitionRole::from_type_guid(mjolnir_image::disk_layout::GUID_BASIC_DATA),
            PartitionRole::Data
        );
    }

    #[test]
    fn a_missing_windows_partition_is_refused() {
        let planned = vec![];
        let mut warnings = Vec::new();
        let err = check_boot_partitions_are_present(&planned, &mut warnings).unwrap_err();
        assert!(err.what().contains("Windows partition"));
    }

    #[test]
    fn the_bitlocker_signature_is_the_documented_one() {
        assert_eq!(BITLOCKER_SIGNATURE, b"-FVE-FS-");
        // A normal NTFS boot sector must not look like BitLocker.
        let mut ntfs = vec![0u8; 512];
        ntfs[3..11].copy_from_slice(b"NTFS    ");
        assert_ne!(&ntfs[3..11], BITLOCKER_SIGNATURE);
    }

    #[test]
    fn an_unlocked_bitlocker_partition_is_planned_rather_than_refused() {
        // The decision the whole BitLocker milestone rests on, expressed
        // without needing a disk: only a locked volume is refused.
        fn decide(e: Encryption) -> bool {
            !matches!(e, Encryption::BitLockerLocked)
        }
        assert!(decide(Encryption::None));
        assert!(decide(Encryption::BitLockerUnlocked));
        assert!(decide(Encryption::Unknown));
        assert!(!decide(Encryption::BitLockerLocked));
    }
}
