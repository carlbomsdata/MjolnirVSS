//! A non-destructive diagnostic for BitLocker and shadow copies.
//!
//! # The question it answers
//!
//! MjolnirVSS backs up a volume by reading it through a shadow copy. If the
//! volume is protected by BitLocker, there are two possibilities, and they lead
//! to opposite products:
//!
//! * the shadow copy presents the volume **decrypted**, in which case a backup
//!   captures an ordinary NTFS filesystem and can restore it; or
//! * the shadow copy presents the volume **still encrypted**, in which case a
//!   backup captures ciphertext that is worthless without key material
//!   MjolnirVSS deliberately never touches.
//!
//! Guessing between those two would be indefensible, so this measures it.
//!
//! # How it establishes the answer
//!
//! It reads the same volume twice, from two different places:
//!
//! 1. the partition's first sector **from the physical disk**, below the
//!    encryption filter, which shows `-FVE-FS-` for a BitLocker volume;
//! 2. the same volume's first sector **from a shadow copy device**, above the
//!    encryption filter.
//!
//! If the first says BitLocker and the second says NTFS, the shadow copy is
//! presenting decrypted data, and that is visible rather than assumed.
//!
//! An eight byte signature on its own would be weak evidence, so it goes
//! further: it parses the boot sector, checks every field against the others,
//! then follows the boot sector's own pointer to the master file table and
//! checks that a file record really is there, and does the same for the mirror.
//! Ciphertext does not satisfy several independent structural checks at once.
//!
//! # What it does not do
//!
//! It writes nothing, anywhere. It reads a handful of sectors. It never reads,
//! derives, stores or logs a recovery key or any other secret, and it does not
//! change BitLocker's state.

use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_ntfs::boot::{looks_like_file_record, NtfsBootSector, VolumeSignature};
use mjolnir_storage::bitlocker::Encryption;
use mjolnir_storage::device::Device;

/// What the diagnostic concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conclusion {
    /// The volume is not encrypted, and the shadow copy shows NTFS. The
    /// ordinary case.
    PlainVolume,
    /// The volume is BitLocker protected and unlocked, and the shadow copy
    /// presents it decrypted. A backup is possible, and what it captures is an
    /// ordinary NTFS filesystem.
    EncryptedButSnapshotIsReadable,
    /// The volume is BitLocker protected and the shadow copy presents
    /// ciphertext. A backup would be worthless without key material.
    EncryptedAndSnapshotIsCiphertext,
    /// The volume is locked. Nothing can be read from it at all.
    Locked,
    /// Something prevented the measurement from being made.
    Inconclusive,
}

impl Conclusion {
    /// Whether this volume can be backed up as it stands.
    pub fn can_be_backed_up(self) -> bool {
        matches!(
            self,
            Conclusion::PlainVolume | Conclusion::EncryptedButSnapshotIsReadable
        )
    }

    /// A sentence for the report.
    pub const fn describe(self) -> &'static str {
        match self {
            Conclusion::PlainVolume => {
                "The volume is not encrypted and can be backed up normally."
            }
            Conclusion::EncryptedButSnapshotIsReadable => {
                "The volume is protected by BitLocker and is unlocked. The shadow copy presents it decrypted, so a backup captures an ordinary NTFS filesystem and can be restored."
            }
            Conclusion::EncryptedAndSnapshotIsCiphertext => {
                "The volume is protected by BitLocker and the shadow copy presents encrypted data. A backup taken this way could not be restored without key material MjolnirVSS does not handle."
            }
            Conclusion::Locked => {
                "The volume is locked. Nothing inside it can be read until it is unlocked."
            }
            Conclusion::Inconclusive => {
                "The measurement could not be completed, so no conclusion is drawn."
            }
        }
    }
}

/// Everything the diagnostic observed.
#[derive(Debug, Clone)]
pub struct Diagnosis {
    /// The volume that was examined.
    pub volume: String,
    /// Its drive letter, if it has one.
    pub drive_letter: Option<String>,
    /// Where the partition starts on the disk.
    pub partition_offset: u64,
    /// What the partition's first sector says, read from the physical disk.
    pub on_disk_signature: VolumeSignature,
    /// What Windows reports the filesystem to be.
    pub reported_filesystem: Option<String>,
    /// The encryption state worked out from those two.
    pub encryption: Encryption,
    /// The shadow copy device that was created, if one was.
    pub snapshot_device: Option<String>,
    /// What the same volume's first sector says, read from the shadow copy.
    pub snapshot_signature: Option<VolumeSignature>,
    /// The boot sector parsed from the shadow copy, if it was valid.
    pub boot_sector: Option<NtfsBootSector>,
    /// Whether a file record was found where the boot sector said the master
    /// file table is.
    pub mft_found: bool,
    /// Whether a file record was found at the mirror as well.
    pub mft_mirror_found: bool,
    /// Every observation, in the order it was made.
    pub findings: Vec<String>,
    /// The conclusion.
    pub conclusion: Conclusion,
}

impl Diagnosis {
    /// The report an operator reads.
    pub fn report(&self) -> String {
        let mut out = String::new();
        out.push_str("BitLocker and shadow copy diagnostic\n");
        out.push_str("====================================\n\n");
        out.push_str(&format!("Volume:            {}\n", self.volume));
        if let Some(letter) = &self.drive_letter {
            out.push_str(&format!("Drive letter:      {letter}:\n"));
        }
        out.push_str(&format!("Partition offset:  {}\n", self.partition_offset));
        out.push_str(&format!(
            "On the disk:       {}\n",
            self.on_disk_signature.describe()
        ));
        out.push_str(&format!(
            "Windows reports:   {}\n",
            self.reported_filesystem
                .as_deref()
                .unwrap_or("no filesystem")
        ));
        out.push_str(&format!(
            "Encryption:        {}\n",
            self.encryption.describe()
        ));
        if let Some(device) = &self.snapshot_device {
            out.push_str(&format!("Shadow copy:       {device}\n"));
        }
        if let Some(sig) = self.snapshot_signature {
            out.push_str(&format!("Through the copy:  {}\n", sig.describe()));
        }
        if let Some(boot) = &self.boot_sector {
            out.push_str("\nNTFS structures read through the shadow copy\n");
            out.push_str(&format!(
                "  bytes per sector:     {}\n",
                boot.bytes_per_sector
            ));
            out.push_str(&format!(
                "  sectors per cluster:  {}\n",
                boot.sectors_per_cluster
            ));
            out.push_str(&format!(
                "  cluster size:         {} bytes\n",
                boot.bytes_per_cluster()
            ));
            out.push_str(&format!("  total sectors:        {}\n", boot.total_sectors));
            out.push_str(&format!(
                "  master file table at: cluster {} (byte {})\n",
                boot.mft_cluster,
                boot.mft_offset().unwrap_or(0)
            ));
            out.push_str(&format!(
                "  file record present:  {}\n",
                if self.mft_found { "yes" } else { "NO" }
            ));
            out.push_str(&format!(
                "  mirror record:        {}\n",
                if self.mft_mirror_found { "yes" } else { "NO" }
            ));
        }

        out.push_str("\nObservations\n");
        for (i, finding) in self.findings.iter().enumerate() {
            out.push_str(&format!("  {}. {finding}\n", i + 1));
        }

        out.push_str("\nConclusion\n");
        out.push_str(&format!("  {}\n", self.conclusion.describe()));

        if self.conclusion == Conclusion::EncryptedButSnapshotIsReadable {
            // Built line by line rather than as one continued literal, because
            // a continuation puts a newline in the middle of a sentence and
            // makes the text awkward to search for.
            out.push('\n');
            out.push_str("  Note: what a backup stores is the decrypted filesystem.\n");
            out.push_str("  The backup files themselves are NOT encrypted, by BitLocker or\n");
            out.push_str("  by MjolnirVSS, so the drive holding them needs looking after as\n");
            out.push_str("  carefully as the computer itself.\n");
        }
        out
    }
}

/// Runs the diagnostic against the volume Windows is installed on.
///
/// Reads only. Any shadow copy it creates is released before it returns,
/// including on every failure path.
#[cfg(windows)]
pub fn diagnose_system_volume(cancel: &CancelToken) -> Result<Diagnosis> {
    let system = mjolnir_storage::system::describe_system()?;
    let disk = mjolnir_storage::disks::describe_disk(system.system_disk_number)?;

    let windows_volume = system.windows_volume.clone();
    let extent = windows_volume.extents.first().ok_or_else(|| {
        Error::unsupported(
            "the Windows volume does not map onto a physical disk",
            "Windows did not report which part of which disk the volume occupies",
            "this machine cannot be diagnosed by this version",
        )
    })?;

    let partition = disk
        .partitions
        .iter()
        .find(|p| p.starting_offset == extent.starting_offset)
        .ok_or_else(|| {
            Error::unsupported(
                "the Windows volume does not line up with any partition on the disk",
                "the volume starts at an offset no partition begins at, which MjolnirVSS does not understand",
                "run the inspect command and report its output",
            )
        })?;

    let mut findings = Vec::new();

    // ---- 1. what the partition looks like on the disk ---------------------
    let encryption_info =
        mjolnir_storage::bitlocker::inspect(&disk, partition, Some(&windows_volume))?;
    findings.extend(encryption_info.evidence.iter().cloned());

    let mut diagnosis = Diagnosis {
        volume: windows_volume.guid_path.clone(),
        drive_letter: windows_volume.drive_letter(),
        partition_offset: partition.starting_offset,
        on_disk_signature: encryption_info.on_disk,
        reported_filesystem: encryption_info.filesystem.clone(),
        encryption: encryption_info.encryption,
        snapshot_device: None,
        snapshot_signature: None,
        boot_sector: None,
        mft_found: false,
        mft_mirror_found: false,
        findings,
        conclusion: Conclusion::Inconclusive,
    };

    if diagnosis.encryption == Encryption::BitLockerLocked {
        diagnosis
            .findings
            .push("the volume is locked, so no shadow copy of it was attempted".to_owned());
        diagnosis.conclusion = Conclusion::Locked;
        return Ok(diagnosis);
    }
    if diagnosis.encryption == Encryption::Unknown {
        diagnosis.conclusion = Conclusion::Inconclusive;
        return Ok(diagnosis);
    }

    // ---- 2. the same volume through a shadow copy -------------------------
    let mut session = mjolnir_vss::VssSession::begin(cancel)?;
    let snapshots = session.snapshot(std::slice::from_ref(&windows_volume.guid_path), cancel)?;
    let snapshot = snapshots.first().ok_or_else(|| {
        Error::new(
            ExitCode::VssFailure,
            "no shadow copy was created",
            "the service reported success but returned no shadow copy to read from",
            "restart the computer and try again",
        )
    })?;

    diagnosis.snapshot_device = Some(snapshot.device_object.clone());
    diagnosis.findings.push(format!(
        "a shadow copy of the volume was created at {}",
        snapshot.device_object
    ));

    // Read through the shadow copy. This is the measurement.
    let device = Device::open_read(
        &snapshot.device_object,
        disk.logical_sector_size,
        extent.length,
    )?;

    let sector_size = disk.logical_sector_size.max(512) as usize;
    let mut first = vec![0u8; sector_size];
    device.read_at(0, &mut first)?;

    let snapshot_signature = VolumeSignature::of(&first);
    diagnosis.snapshot_signature = Some(snapshot_signature);
    diagnosis.findings.push(format!(
        "read through the shadow copy, the same volume identifies itself as {}",
        snapshot_signature.describe()
    ));

    // ---- 3. is it actually coherent NTFS, or just a header? ---------------
    match NtfsBootSector::parse(&first) {
        Ok(boot) => {
            diagnosis.boot_sector = Some(boot);
            diagnosis.findings.push(format!(
                "its boot sector is internally consistent: {} byte clusters, {} sectors, master file table at cluster {}",
                boot.bytes_per_cluster(),
                boot.total_sectors,
                boot.mft_cluster
            ));

            // Follow the boot sector's own pointer. Two independent structures
            // agreeing is what makes this proof rather than pattern matching.
            let record_size = boot.bytes_per_file_record.max(1024) as usize;
            let mut record = vec![0u8; record_size];

            match boot
                .mft_offset()
                .and_then(|offset| device.read_at(offset, &mut record).map(|_| offset))
            {
                Ok(offset) => {
                    diagnosis.mft_found = looks_like_file_record(&record);
                    diagnosis.findings.push(format!(
                        "at byte {offset}, where the boot sector says the master file table is, the data {} an NTFS file record",
                        if diagnosis.mft_found { "begins with" } else { "does NOT begin with" }
                    ));
                }
                Err(e) => diagnosis.findings.push(format!(
                    "the master file table could not be read: {}",
                    e.what()
                )),
            }

            let mut mirror = vec![0u8; record_size];
            match boot
                .mft_mirror_offset()
                .and_then(|offset| device.read_at(offset, &mut mirror).map(|_| offset))
            {
                Ok(offset) => {
                    diagnosis.mft_mirror_found = looks_like_file_record(&mirror);
                    diagnosis.findings.push(format!(
                        "at byte {offset}, the mirror of the master file table, the data {} an NTFS file record",
                        if diagnosis.mft_mirror_found { "begins with" } else { "does NOT begin with" }
                    ));
                }
                Err(e) => diagnosis.findings.push(format!(
                    "the mirror of the master file table could not be read: {}",
                    e.what()
                )),
            }
        }
        Err(e) => {
            diagnosis.findings.push(format!(
                "the data read through the shadow copy is not a usable NTFS boot sector: {}",
                e.why()
            ));
        }
    }

    // ---- 4. the conclusion ------------------------------------------------
    let structurally_ntfs =
        diagnosis.boot_sector.is_some() && diagnosis.mft_found && diagnosis.mft_mirror_found;

    diagnosis.conclusion = match (diagnosis.encryption, structurally_ntfs) {
        (Encryption::None, true) => Conclusion::PlainVolume,
        (Encryption::BitLockerUnlocked, true) => Conclusion::EncryptedButSnapshotIsReadable,
        (Encryption::BitLockerUnlocked, false) => Conclusion::EncryptedAndSnapshotIsCiphertext,
        _ => Conclusion::Inconclusive,
    };

    // Release the shadow copy before returning, rather than relying on the
    // session's destructor, so the machine is left alone as early as possible.
    drop(device);
    session.complete(cancel).ok();
    match session.delete_own_snapshots() {
        Ok(n) => diagnosis
            .findings
            .push(format!("the shadow copy was removed ({n} released)")),
        Err(e) => diagnosis.findings.push(format!(
            "the shadow copy could not be removed: {}",
            e.what()
        )),
    }

    Ok(diagnosis)
}

#[cfg(not(windows))]
pub fn diagnose_system_volume(_cancel: &CancelToken) -> Result<Diagnosis> {
    Err(Error::unsupported(
        "the diagnostic only runs on Windows",
        "it uses the Volume Shadow Copy Service",
        "run it on the Windows computer you want to examine",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnosis_with(encryption: Encryption, conclusion: Conclusion) -> Diagnosis {
        Diagnosis {
            volume: "\\\\?\\Volume{test}\\".to_owned(),
            drive_letter: Some("C".to_owned()),
            partition_offset: 122_683_392,
            on_disk_signature: VolumeSignature::BitLocker,
            reported_filesystem: Some("NTFS".to_owned()),
            encryption,
            snapshot_device: Some(
                "\\\\?\\GLOBALROOT\\Device\\HarddiskVolumeShadowCopy1".to_owned(),
            ),
            snapshot_signature: Some(VolumeSignature::Ntfs),
            boot_sector: None,
            mft_found: true,
            mft_mirror_found: true,
            findings: vec!["something was observed".to_owned()],
            conclusion,
        }
    }

    #[test]
    fn only_readable_volumes_can_be_backed_up() {
        assert!(Conclusion::PlainVolume.can_be_backed_up());
        assert!(Conclusion::EncryptedButSnapshotIsReadable.can_be_backed_up());

        assert!(!Conclusion::EncryptedAndSnapshotIsCiphertext.can_be_backed_up());
        assert!(!Conclusion::Locked.can_be_backed_up());
        assert!(
            !Conclusion::Inconclusive.can_be_backed_up(),
            "an unmeasured volume must never be treated as backable"
        );
    }

    #[test]
    fn every_conclusion_explains_itself() {
        for c in [
            Conclusion::PlainVolume,
            Conclusion::EncryptedButSnapshotIsReadable,
            Conclusion::EncryptedAndSnapshotIsCiphertext,
            Conclusion::Locked,
            Conclusion::Inconclusive,
        ] {
            assert!(c.describe().len() > 30, "{c:?} is not explained");
        }
    }

    #[test]
    fn the_report_warns_that_the_backup_itself_is_not_encrypted() {
        let d = diagnosis_with(
            Encryption::BitLockerUnlocked,
            Conclusion::EncryptedButSnapshotIsReadable,
        );
        let report = d.report();
        assert!(report.contains("NOT encrypted"), "{report}");
        assert!(
            report.contains("needs looking after as"),
            "the report must say the backup drive needs protecting: {report}"
        );
    }

    #[test]
    fn a_plain_volume_report_carries_no_encryption_warning() {
        let d = diagnosis_with(Encryption::None, Conclusion::PlainVolume);
        assert!(!d.report().contains("NOT encrypted"));
    }

    #[test]
    fn the_report_shows_both_readings_so_they_can_be_compared() {
        let d = diagnosis_with(
            Encryption::BitLockerUnlocked,
            Conclusion::EncryptedButSnapshotIsReadable,
        );
        let report = d.report();
        assert!(report.contains("On the disk:"));
        assert!(report.contains("Through the copy:"));
        assert!(report.contains("BitLocker"));
        assert!(report.contains("NTFS"));
    }
}
