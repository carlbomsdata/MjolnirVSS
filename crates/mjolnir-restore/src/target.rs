//! Choosing a disk to erase, and refusing the wrong ones.
//!
//! This is the most dangerous decision in the product. Everything here exists
//! to make the wrong answer impossible rather than unlikely:
//!
//! * the disk holding the backup is refused, because restoring onto it would
//!   destroy the backup mid restore;
//! * a disk too small for the layout is refused before anything is written;
//! * a disk whose sector size differs from the source is refused, because every
//!   offset in the backup is measured in the source's sectors;
//! * nothing is ever selected automatically;
//! * and the operator has to type the target's serial number, which cannot be
//!   done by accident.

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_image::disk_layout::DiskEntry;

/// A disk offered as a restore target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetDisk {
    /// Disk number, the `N` in `\\.\PhysicalDriveN`.
    pub number: u32,
    /// The path to open.
    pub device_path: String,
    /// Total size in bytes.
    pub size_bytes: u64,
    /// Logical sector size.
    pub logical_sector_size: u32,
    /// Model string.
    pub model: Option<String>,
    /// Serial number, when the device reports one.
    pub serial: Option<String>,
    /// How the disk is attached.
    pub bus: String,
    /// What is on the disk now, for the confirmation screen.
    pub existing_partitions: Vec<String>,
    /// Whether the backup being restored is stored on this disk.
    pub holds_the_backup: bool,
}

impl TargetDisk {
    /// The exact words the operator has to type to erase this disk.
    ///
    /// A serial number is used when the disk reports one, because it names the
    /// physical object rather than a number that changes between boots. When
    /// there is no serial, the disk number is used instead and the phrase is
    /// longer, so it still cannot be typed absent-mindedly.
    pub fn erase_phrase(&self) -> String {
        match self
            .serial
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(serial) => format!("ERASE {serial}"),
            None => format!("ERASE DISK {}", self.number),
        }
    }

    /// A description for the confirmation screen.
    pub fn describe(&self) -> String {
        format!(
            "Disk {} - {} - {} - {} ({})",
            self.number,
            self.model.as_deref().unwrap_or("Unknown model"),
            mjolnir_core::progress::format_bytes(self.size_bytes),
            self.bus,
            self.serial.as_deref().unwrap_or("no serial number")
        )
    }
}

/// Proof that the operator typed the erase phrase for a specific disk.
///
/// The only way to obtain one is [`EraseConfirmation::check`], and a restore
/// will not run without it. That makes "somebody forgot to ask" a compile time
/// impossibility rather than a review comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EraseConfirmation {
    disk_number: u32,
    phrase: String,
}

impl EraseConfirmation {
    /// Checks what the operator typed against what this disk requires.
    ///
    /// Leading and trailing spaces are forgiven because they are invisible.
    /// Case is not: `erase` is not accepted for `ERASE`, so the phrase cannot be
    /// produced by autocorrect or by holding a key down.
    pub fn check(target: &TargetDisk, typed: &str) -> Result<Self> {
        let expected = target.erase_phrase();
        if typed.trim() == expected {
            return Ok(Self {
                disk_number: target.number,
                phrase: expected,
            });
        }
        Err(Error::new(
            ExitCode::UnsafeTarget,
            "the confirmation did not match",
            format!("to erase this disk you have to type exactly: {expected}",),
            "check the disk you selected is the right one, then type the phrase exactly as shown",
        ))
    }

    /// The disk this confirmation is for.
    pub fn disk_number(&self) -> u32 {
        self.disk_number
    }

    /// The phrase that was typed.
    pub fn phrase(&self) -> &str {
        &self.phrase
    }

    /// Whether this confirmation applies to `target`.
    pub fn matches(&self, target: &TargetDisk) -> bool {
        self.disk_number == target.number && self.phrase == target.erase_phrase()
    }
}

/// Checks that a disk can be restored onto, without touching it.
pub fn check_target(source: &DiskEntry, target: &TargetDisk, required_bytes: u64) -> Result<()> {
    if target.holds_the_backup {
        return Err(Error::unsafe_target(
            format!(
                "disk {} holds the backup you are restoring from",
                target.number
            ),
            "erasing it would destroy the backup partway through the restore, leaving the computer with neither a working system nor anything to recover from",
            "choose the blank replacement disk instead, and keep the drive holding the backup connected",
        ));
    }

    if target.size_bytes < required_bytes {
        return Err(Error::unsafe_target(
            format!(
                "disk {} is {} and the backup needs at least {}",
                target.number,
                mjolnir_core::progress::format_bytes(target.size_bytes),
                mjolnir_core::progress::format_bytes(required_bytes)
            ),
            "the partitions in the backup would not fit, so the restore would stop partway and leave an unbootable disk",
            "use a disk at least as large as the one the backup was taken from",
        ));
    }

    if target.logical_sector_size != source.logical_sector_size {
        return Err(Error::unsafe_target(
            format!(
                "disk {} uses {} byte sectors and the backup came from a disk with {} byte sectors",
                target.number, target.logical_sector_size, source.logical_sector_size
            ),
            "every offset in the backup is measured in the original disk's sectors, so writing them to a disk with a different sector size would put every partition in the wrong place",
            "use a replacement disk with the same sector size; MjolnirVSS does not convert between them",
        ));
    }

    if !mjolnir_image::disk_layout::SUPPORTED_SECTOR_SIZES.contains(&target.logical_sector_size) {
        return Err(Error::unsafe_target(
            format!(
                "disk {} reports {} byte sectors",
                target.number, target.logical_sector_size
            ),
            "MjolnirVSS has only been tested with 512 and 4096 byte sectors",
            "this disk cannot be restored to by this version",
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mjolnir_core::ids::{DiskId, PartitionId};
    use mjolnir_image::disk_layout::{BusType, PartitionEntry, PartitionRole, PartitionStyle};

    fn source_disk(sector_size: u32) -> DiskEntry {
        DiskEntry {
            id: DiskId::new("disk-0").unwrap(),
            disk_number: 0,
            size_bytes: 512 << 30,
            logical_sector_size: sector_size,
            physical_sector_size: 4096,
            partition_style: PartitionStyle::Gpt,
            disk_guid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
            model: Some("Source".to_owned()),
            serial: Some("SRC1".to_owned()),
            bus_type: BusType::Nvme,
            partitions: vec![PartitionEntry {
                id: PartitionId::new("p-1").unwrap(),
                number: 1,
                type_guid: mjolnir_image::disk_layout::GUID_EFI_SYSTEM.to_owned(),
                unique_guid: "11111111-2222-3333-4444-555555555555".to_owned(),
                name: String::new(),
                starting_offset: 1 << 20,
                length: 100 << 20,
                attributes: 0,
                role: PartitionRole::EfiSystem,
                filesystem: Some("FAT32".to_owned()),
            }],
        }
    }

    fn target(number: u32, size: u64, sector_size: u32, serial: Option<&str>) -> TargetDisk {
        TargetDisk {
            number,
            device_path: format!("\\\\.\\PhysicalDrive{number}"),
            size_bytes: size,
            logical_sector_size: sector_size,
            model: Some("Replacement SSD".to_owned()),
            serial: serial.map(str::to_owned),
            bus: "NVMe".to_owned(),
            existing_partitions: Vec::new(),
            holds_the_backup: false,
        }
    }

    #[test]
    fn a_suitable_disk_is_accepted() {
        let source = source_disk(512);
        let t = target(1, 1024 << 30, 512, Some("ABC123"));
        assert!(check_target(&source, &t, 512 << 30).is_ok());
    }

    #[test]
    fn the_disk_holding_the_backup_is_refused() {
        let source = source_disk(512);
        let mut t = target(1, 1024 << 30, 512, Some("ABC123"));
        t.holds_the_backup = true;

        let err = check_target(&source, &t, 512 << 30).unwrap_err();
        assert_eq!(err.exit(), ExitCode::UnsafeTarget);
        assert!(err.what().contains("holds the backup"));
        assert!(err.why().contains("destroy the backup"));
    }

    #[test]
    fn a_disk_that_is_too_small_is_refused() {
        let source = source_disk(512);
        let t = target(1, 100 << 30, 512, Some("ABC123"));
        let err = check_target(&source, &t, 512 << 30).unwrap_err();
        assert_eq!(err.exit(), ExitCode::UnsafeTarget);
        assert!(err.what().contains("needs at least"));
    }

    #[test]
    fn a_disk_exactly_the_required_size_is_accepted() {
        let source = source_disk(512);
        let t = target(1, 512 << 30, 512, Some("ABC123"));
        assert!(check_target(&source, &t, 512 << 30).is_ok());
    }

    #[test]
    fn a_sector_size_mismatch_is_refused_in_both_directions() {
        let source = source_disk(512);
        let t = target(1, 1024 << 30, 4096, Some("ABC123"));
        let err = check_target(&source, &t, 512 << 30).unwrap_err();
        assert!(err.what().contains("4096 byte sectors"));

        let source = source_disk(4096);
        let t = target(1, 1024 << 30, 512, Some("ABC123"));
        assert!(check_target(&source, &t, 512 << 30).is_err());
    }

    #[test]
    fn an_untested_sector_size_is_refused_even_when_it_matches() {
        let mut source = source_disk(512);
        source.logical_sector_size = 520;
        let t = target(1, 1024 << 30, 520, Some("ABC123"));
        let err = check_target(&source, &t, 512 << 30).unwrap_err();
        assert!(err.what().contains("520 byte sectors"));
    }

    #[test]
    fn the_erase_phrase_uses_the_serial_when_there_is_one() {
        let t = target(3, 1 << 40, 512, Some("S4NV7X0T123"));
        assert_eq!(t.erase_phrase(), "ERASE S4NV7X0T123");
    }

    #[test]
    fn the_erase_phrase_falls_back_to_the_disk_number() {
        let t = target(3, 1 << 40, 512, None);
        assert_eq!(t.erase_phrase(), "ERASE DISK 3");
        // A blank serial counts as none.
        let t = target(3, 1 << 40, 512, Some("   "));
        assert_eq!(t.erase_phrase(), "ERASE DISK 3");
    }

    #[test]
    fn the_exact_phrase_is_required() {
        let t = target(1, 1 << 40, 512, Some("ABC123"));

        // Correct, including with stray spaces around it.
        assert!(EraseConfirmation::check(&t, "ERASE ABC123").is_ok());
        assert!(EraseConfirmation::check(&t, "  ERASE ABC123  ").is_ok());

        // Everything else is refused.
        for wrong in [
            "y",
            "yes",
            "YES",
            "erase abc123",
            "ERASE abc123",
            "Erase ABC123",
            "ERASE",
            "ERASE ABC12",
            "ERASE ABC1234",
            "ERASE DISK 1",
            "",
        ] {
            let err = EraseConfirmation::check(&t, wrong).unwrap_err();
            assert_eq!(err.exit(), ExitCode::UnsafeTarget, "{wrong:?} was accepted");
            assert!(err.why().contains("ERASE ABC123"));
        }
    }

    #[test]
    fn a_confirmation_does_not_transfer_to_another_disk() {
        let a = target(1, 1 << 40, 512, Some("AAA"));
        let b = target(2, 1 << 40, 512, Some("BBB"));
        let confirmation = EraseConfirmation::check(&a, "ERASE AAA").unwrap();

        assert!(confirmation.matches(&a));
        assert!(
            !confirmation.matches(&b),
            "a confirmation for one disk must not authorise erasing another"
        );
    }

    #[test]
    fn a_confirmation_does_not_survive_the_disk_changing_underneath_it() {
        let mut t = target(1, 1 << 40, 512, Some("AAA"));
        let confirmation = EraseConfirmation::check(&t, "ERASE AAA").unwrap();
        assert!(confirmation.matches(&t));

        // The same disk number, a different physical disk.
        t.serial = Some("BBB".to_owned());
        assert!(!confirmation.matches(&t));
    }

    #[test]
    fn the_description_names_everything_the_operator_needs() {
        let t = target(2, 1000 << 30, 512, Some("S4NV7X0T123"));
        let text = t.describe();
        assert!(text.contains("Disk 2"));
        assert!(text.contains("Replacement SSD"));
        assert!(text.contains("S4NV7X0T123"));
        assert!(text.contains("NVMe"));
    }
}
