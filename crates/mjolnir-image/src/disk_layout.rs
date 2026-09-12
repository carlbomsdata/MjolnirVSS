//! `disk-layout.json`: the physical facts about the disks that were captured.
//!
//! This document is what a restore reads to recreate a partition table, and
//! what the confirmation screen reads to show the operator what is about to be
//! erased. It is kept apart from `manifest.json` so that showing a disk layout
//! never means parsing a chunk table with tens of thousands of entries in it.
//!
//! Every number here becomes an offset in a write against a replacement disk,
//! so validation is strict: partitions must fit, must not overlap, and must sit
//! on sector boundaries.

use std::collections::BTreeSet;

use mjolnir_core::ids::{DiskId, PartitionId};
use mjolnir_core::math;
use serde::{Deserialize, Serialize};

use crate::issue::Issue;
use crate::version::{DocumentKind, FormatHeader};

/// Logical sector sizes MjolnirVSS supports.
pub const SUPPORTED_SECTOR_SIZES: [u32; 2] = [512, 4096];

/// GPT type GUID of an EFI system partition.
pub const GUID_EFI_SYSTEM: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";

/// GPT type GUID of a Microsoft Reserved partition.
pub const GUID_MSR: &str = "e3c9e316-0b5c-4db8-817d-f92df00215ae";

/// GPT type GUID of a basic data partition, which is what a Windows volume uses.
pub const GUID_BASIC_DATA: &str = "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7";

/// GPT type GUID of a Windows recovery partition.
pub const GUID_WINDOWS_RECOVERY: &str = "de94bba4-06d1-4d40-a16a-bfd50179d6ac";

/// A partition table style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PartitionStyle {
    /// GUID partition table, the only style supported in this version.
    Gpt,
    /// Master boot record.
    Mbr,
    /// The disk has no partition table.
    Raw,
}

/// What a partition is for, as far as MjolnirVSS can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PartitionRole {
    /// EFI system partition, FAT32, holds the boot loader.
    EfiSystem,
    /// Microsoft Reserved, no filesystem.
    MicrosoftReserved,
    /// The partition holding the Windows installation.
    Windows,
    /// Windows Recovery Environment.
    Recovery,
    /// Any other data partition.
    Data,
    /// The type GUID was not recognised.
    Unknown,
}

impl PartitionRole {
    /// Guesses the role from a GPT type GUID.
    ///
    /// A basic data partition can be Windows, recovery or plain data, so the
    /// caller refines this using what the filesystem turns out to contain.
    pub fn from_type_guid(type_guid: &str) -> Self {
        let lower = type_guid.to_ascii_lowercase();
        match lower.as_str() {
            GUID_EFI_SYSTEM => PartitionRole::EfiSystem,
            GUID_MSR => PartitionRole::MicrosoftReserved,
            GUID_WINDOWS_RECOVERY => PartitionRole::Recovery,
            GUID_BASIC_DATA => PartitionRole::Data,
            _ => PartitionRole::Unknown,
        }
    }

    /// Whether losing this partition would stop the computer from booting.
    pub fn is_required_for_boot(self) -> bool {
        matches!(
            self,
            PartitionRole::EfiSystem | PartitionRole::MicrosoftReserved | PartitionRole::Windows
        )
    }

    /// A short description for the operator.
    pub const fn describe(self) -> &'static str {
        match self {
            PartitionRole::EfiSystem => "EFI System",
            PartitionRole::MicrosoftReserved => "Microsoft Reserved",
            PartitionRole::Windows => "Windows",
            PartitionRole::Recovery => "Windows Recovery",
            PartitionRole::Data => "Data",
            PartitionRole::Unknown => "Unknown",
        }
    }
}

/// One partition of one disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionEntry {
    /// Identifies the partition within the backup.
    pub id: PartitionId,
    /// Partition number as Windows reported it.
    pub number: u32,
    /// GPT partition type GUID.
    pub type_guid: String,
    /// GPT unique partition GUID, preserved across a restore.
    pub unique_guid: String,
    /// GPT partition name.
    #[serde(default)]
    pub name: String,
    /// Byte offset of the partition from the start of the disk.
    pub starting_offset: u64,
    /// Length of the partition in bytes.
    pub length: u64,
    /// GPT attribute flags, preserved across a restore.
    pub attributes: u64,
    /// What the partition appears to be for.
    pub role: PartitionRole,
    /// Filesystem as Windows reported it, if any.
    #[serde(default)]
    pub filesystem: Option<String>,
}

impl PartitionEntry {
    /// One past the last byte of the partition.
    pub fn end_offset(&self) -> Result<u64, math::ArithError> {
        math::range_end("partition end", self.starting_offset, self.length)
    }
}

/// How a disk is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BusType {
    /// NVM Express.
    Nvme,
    /// SATA or an ATA attached device.
    Sata,
    /// SCSI.
    Scsi,
    /// Serial attached SCSI.
    Sas,
    /// USB attached storage.
    Usb,
    /// Secure Digital.
    Sd,
    /// A virtual disk.
    Virtual,
    /// Anything else, or not reported.
    Other,
}

impl BusType {
    /// A short description for the operator.
    pub const fn describe(self) -> &'static str {
        match self {
            BusType::Nvme => "NVMe",
            BusType::Sata => "SATA",
            BusType::Scsi => "SCSI",
            BusType::Sas => "SAS",
            BusType::Usb => "USB",
            BusType::Sd => "SD",
            BusType::Virtual => "Virtual",
            BusType::Other => "Other",
        }
    }
}

/// One physical disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskEntry {
    /// Identifies the disk within the backup.
    pub id: DiskId,
    /// Disk number at capture time. Numbers move between boots, so this is for
    /// the operator to recognise the disk, never for matching one.
    pub disk_number: u32,
    /// Total size in bytes.
    pub size_bytes: u64,
    /// Logical sector size, the unit every offset is a multiple of.
    pub logical_sector_size: u32,
    /// Physical sector size.
    pub physical_sector_size: u32,
    /// Partition table style.
    pub partition_style: PartitionStyle,
    /// GPT disk GUID, preserved across a restore.
    pub disk_guid: String,
    /// Model string.
    #[serde(default)]
    pub model: Option<String>,
    /// Serial number, used in the erase confirmation the operator types.
    #[serde(default)]
    pub serial: Option<String>,
    /// How the disk is attached.
    pub bus_type: BusType,
    /// Every partition found on the disk, in table order.
    pub partitions: Vec<PartitionEntry>,
}

impl DiskEntry {
    /// The partition holding Windows, if one was identified.
    pub fn windows_partition(&self) -> Option<&PartitionEntry> {
        self.partitions
            .iter()
            .find(|p| p.role == PartitionRole::Windows)
    }

    /// The smallest disk this one can be restored onto.
    ///
    /// A restore recreates the partition table rather than copying the disk
    /// byte for byte, so the requirement is that every partition fits and the
    /// secondary GPT has room at the end, not that the disk is the same size.
    pub fn required_target_bytes(&self) -> Result<u64, math::ArithError> {
        let mut highest = 0u64;
        for p in &self.partitions {
            highest = highest.max(p.end_offset()?);
        }
        // The secondary GPT sits in the last sectors of the disk: one header
        // sector plus the partition entry array, which is 16 KiB by convention.
        let sector = u64::from(self.logical_sector_size.max(512));
        let gpt_reserve = math::round_up("secondary gpt reserve", 16 * 1024 + sector, sector)?;
        math::add_u64("required target size", highest, gpt_reserve)
    }
}

/// `disk-layout.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskLayout {
    /// Format identity.
    pub format: FormatHeader,
    /// The backup this layout belongs to, checked against `manifest.json`.
    pub backup_uuid: String,
    /// Every disk captured.
    pub disks: Vec<DiskEntry>,
}

impl DiskLayout {
    /// Builds an empty layout document for this build.
    pub fn new(backup_uuid: impl Into<String>) -> Self {
        Self {
            format: FormatHeader::current(DocumentKind::DiskLayout),
            backup_uuid: backup_uuid.into(),
            disks: Vec::new(),
        }
    }

    /// Looks up a disk by identifier.
    pub fn disk(&self, id: &DiskId) -> Option<&DiskEntry> {
        self.disks.iter().find(|d| &d.id == id)
    }

    /// Looks up a partition by identifier, with its disk.
    pub fn partition(&self, id: &PartitionId) -> Option<(&DiskEntry, &PartitionEntry)> {
        self.disks
            .iter()
            .find_map(|d| d.partitions.iter().find(|p| &p.id == id).map(|p| (d, p)))
    }

    /// Checks the document on its own.
    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = self.format.check(DocumentKind::DiskLayout);

        if !crate::is_guid(&self.backup_uuid) {
            issues.push(Issue::error(
                "disk-layout.json",
                format!("backup_uuid {:?} is not a GUID", self.backup_uuid),
            ));
        }
        if self.disks.is_empty() {
            issues.push(Issue::error(
                "disk-layout.json",
                "records no disks at all, so there is nothing to restore",
            ));
        }

        let mut disk_ids: BTreeSet<&DiskId> = BTreeSet::new();
        let mut partition_ids: BTreeSet<&PartitionId> = BTreeSet::new();

        for disk in &self.disks {
            let object = format!("disk {:?}", disk.id.as_str());
            if !disk_ids.insert(&disk.id) {
                issues.push(Issue::error(&object, "duplicate disk identifier"));
            }
            self.validate_disk(disk, &object, &mut partition_ids, &mut issues);
        }
        issues
    }

    fn validate_disk<'a>(
        &'a self,
        disk: &'a DiskEntry,
        object: &str,
        partition_ids: &mut BTreeSet<&'a PartitionId>,
        issues: &mut Vec<Issue>,
    ) {
        if disk.size_bytes == 0 {
            issues.push(Issue::error(object, "size is zero"));
        }
        if disk.partition_style != PartitionStyle::Gpt {
            issues.push(Issue::error(
                object,
                format!(
                    "uses the {:?} partition style; this version of MjolnirVSS supports GPT only",
                    disk.partition_style
                ),
            ));
        }
        if !crate::is_guid(&disk.disk_guid) {
            issues.push(Issue::error(
                object,
                format!("disk GUID {:?} is not a GUID", disk.disk_guid),
            ));
        }

        let sector_ok = SUPPORTED_SECTOR_SIZES.contains(&disk.logical_sector_size);
        if !sector_ok {
            issues.push(Issue::error(
                object,
                format!(
                    "logical sector size {} is not one of {:?}",
                    disk.logical_sector_size, SUPPORTED_SECTOR_SIZES
                ),
            ));
        } else {
            if disk.size_bytes % u64::from(disk.logical_sector_size) != 0 {
                issues.push(Issue::error(
                    object,
                    format!(
                        "size {} is not a whole number of {} byte sectors",
                        disk.size_bytes, disk.logical_sector_size
                    ),
                ));
            }
            if disk.physical_sector_size < disk.logical_sector_size
                || disk.physical_sector_size % disk.logical_sector_size != 0
            {
                issues.push(Issue::error(
                    object,
                    format!(
                        "physical sector size {} is not a multiple of the logical size {}",
                        disk.physical_sector_size, disk.logical_sector_size
                    ),
                ));
            }
        }

        if disk.partitions.is_empty() {
            issues.push(Issue::error(
                object,
                "has no partitions; MjolnirVSS never records a disk whose table it could not read",
            ));
        }

        let mut numbers: BTreeSet<u32> = BTreeSet::new();
        for part in &disk.partitions {
            let p_object = format!("{object} partition {:?}", part.id.as_str());
            if !partition_ids.insert(&part.id) {
                issues.push(Issue::error(&p_object, "duplicate partition identifier"));
            }
            if !numbers.insert(part.number) {
                issues.push(Issue::error(
                    &p_object,
                    format!(
                        "partition number {} appears twice on this disk",
                        part.number
                    ),
                ));
            }
            if part.length == 0 {
                issues.push(Issue::error(&p_object, "length is zero"));
            }
            if !crate::is_guid(&part.type_guid) {
                issues.push(Issue::error(
                    &p_object,
                    format!("type GUID {:?} is not a GUID", part.type_guid),
                ));
            }
            if !crate::is_guid(&part.unique_guid) {
                issues.push(Issue::error(
                    &p_object,
                    format!("unique GUID {:?} is not a GUID", part.unique_guid),
                ));
            }
            if let Err(e) = math::ensure_within(
                "partition extent",
                part.starting_offset,
                part.length,
                disk.size_bytes,
            ) {
                issues.push(Issue::error(
                    &p_object,
                    format!("does not fit inside the disk: {e}"),
                ));
            }
            if sector_ok {
                let sector = u64::from(disk.logical_sector_size);
                if part.starting_offset % sector != 0 || part.length % sector != 0 {
                    issues.push(Issue::error(
                        &p_object,
                        format!(
                            "offset {} and length {} are not both multiples of the {sector} byte sector size",
                            part.starting_offset, part.length
                        ),
                    ));
                }
            }
            // The first usable block of a GPT disk is after the primary header
            // and the entry array. A partition starting at zero would sit on
            // top of the table itself.
            if part.starting_offset == 0 {
                issues.push(Issue::error(
                    &p_object,
                    "starts at offset 0, which is where the protective MBR and GPT header live",
                ));
            }
        }

        let mut sorted: Vec<&PartitionEntry> = disk.partitions.iter().collect();
        sorted.sort_by_key(|p| (p.starting_offset, p.length));
        for w in sorted.windows(2) {
            let (a, b) = (w[0], w[1]);
            match math::ranges_overlap(a.starting_offset, a.length, b.starting_offset, b.length) {
                Ok(true) => issues.push(Issue::error(
                    object,
                    format!(
                        "partitions {:?} and {:?} overlap on the disk",
                        a.id.as_str(),
                        b.id.as_str()
                    ),
                )),
                Ok(false) => {}
                Err(e) => issues.push(Issue::error(object, format!("{e}"))),
            }
        }

        if let Err(e) = disk.required_target_bytes() {
            issues.push(Issue::error(
                object,
                format!("required target size cannot be computed: {e}"),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(
        id: &str,
        number: u32,
        offset: u64,
        length: u64,
        role: PartitionRole,
    ) -> PartitionEntry {
        PartitionEntry {
            id: PartitionId::new(id).unwrap(),
            number,
            type_guid: GUID_BASIC_DATA.to_owned(),
            unique_guid: "11111111-2222-3333-4444-555555555555".to_owned(),
            name: String::new(),
            starting_offset: offset,
            length,
            attributes: 0,
            role,
            filesystem: Some("NTFS".to_owned()),
        }
    }

    fn disk_with(parts: Vec<PartitionEntry>, size: u64) -> DiskEntry {
        DiskEntry {
            id: DiskId::new("disk-0").unwrap(),
            disk_number: 0,
            size_bytes: size,
            logical_sector_size: 512,
            physical_sector_size: 4096,
            partition_style: PartitionStyle::Gpt,
            disk_guid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
            model: Some("Test Disk".to_owned()),
            serial: Some("SER123".to_owned()),
            bus_type: BusType::Nvme,
            partitions: parts,
        }
    }

    fn layout_with(disk: DiskEntry) -> DiskLayout {
        let mut l = DiskLayout::new("99999999-8888-7777-6666-555555555555");
        l.disks.push(disk);
        l
    }

    #[test]
    fn role_is_recognised_from_the_type_guid() {
        assert_eq!(
            PartitionRole::from_type_guid(GUID_EFI_SYSTEM),
            PartitionRole::EfiSystem
        );
        // Windows reports GUIDs in uppercase; the match must not care.
        assert_eq!(
            PartitionRole::from_type_guid(&GUID_MSR.to_uppercase()),
            PartitionRole::MicrosoftReserved
        );
        assert_eq!(
            PartitionRole::from_type_guid(GUID_WINDOWS_RECOVERY),
            PartitionRole::Recovery
        );
        assert_eq!(
            PartitionRole::from_type_guid("00000000-0000-0000-0000-000000000000"),
            PartitionRole::Unknown
        );
    }

    #[test]
    fn a_reasonable_layout_validates() {
        let l = layout_with(disk_with(
            vec![
                part("p-1", 1, 1 << 20, 100 << 20, PartitionRole::EfiSystem),
                part(
                    "p-2",
                    2,
                    101 << 20,
                    16 << 20,
                    PartitionRole::MicrosoftReserved,
                ),
                part("p-3", 3, 117 << 20, 800 << 20, PartitionRole::Windows),
            ],
            1024 << 20,
        ));
        let issues = l.validate();
        assert!(issues.is_empty(), "unexpected findings: {issues:?}");
    }

    #[test]
    fn overlapping_partitions_are_caught() {
        let l = layout_with(disk_with(
            vec![
                part("p-1", 1, 1 << 20, 100 << 20, PartitionRole::EfiSystem),
                part("p-2", 2, 50 << 20, 100 << 20, PartitionRole::Windows),
            ],
            1024 << 20,
        ));
        let issues = l.validate();
        assert!(
            issues.iter().any(|i| i.problem.contains("overlap")),
            "{issues:?}"
        );
    }

    #[test]
    fn a_partition_running_off_the_end_is_caught() {
        let l = layout_with(disk_with(
            vec![part("p-1", 1, 1 << 20, 4096 << 20, PartitionRole::Windows)],
            1024 << 20,
        ));
        assert!(l
            .validate()
            .iter()
            .any(|i| i.problem.contains("does not fit")));
    }

    #[test]
    fn a_misaligned_partition_is_caught() {
        let l = layout_with(disk_with(
            vec![part("p-1", 1, 1_048_577, 100 << 20, PartitionRole::Windows)],
            1024 << 20,
        ));
        assert!(l
            .validate()
            .iter()
            .any(|i| i.problem.contains("multiples of the 512")));
    }

    #[test]
    fn a_partition_at_offset_zero_is_caught() {
        let l = layout_with(disk_with(
            vec![part("p-1", 1, 0, 100 << 20, PartitionRole::Windows)],
            1024 << 20,
        ));
        assert!(l
            .validate()
            .iter()
            .any(|i| i.problem.contains("protective MBR")));
    }

    #[test]
    fn an_mbr_disk_is_refused() {
        let mut d = disk_with(
            vec![part("p-1", 1, 1 << 20, 100 << 20, PartitionRole::Windows)],
            1024 << 20,
        );
        d.partition_style = PartitionStyle::Mbr;
        assert!(layout_with(d)
            .validate()
            .iter()
            .any(|i| i.problem.contains("GPT only")));
    }

    #[test]
    fn a_4kn_sector_size_is_accepted_but_an_odd_one_is_not() {
        let mut d = disk_with(
            vec![part("p-1", 1, 1 << 20, 100 << 20, PartitionRole::Windows)],
            1024 << 20,
        );
        d.logical_sector_size = 4096;
        d.physical_sector_size = 4096;
        assert!(layout_with(d.clone()).validate().is_empty());

        d.logical_sector_size = 520;
        assert!(layout_with(d)
            .validate()
            .iter()
            .any(|i| i.problem.contains("logical sector size")));
    }

    #[test]
    fn required_target_size_leaves_room_for_the_secondary_gpt() {
        let d = disk_with(
            vec![part("p-1", 1, 1 << 20, 100 << 20, PartitionRole::Windows)],
            1024 << 20,
        );
        let required = d.required_target_bytes().unwrap();
        let last_byte = (1u64 << 20) + (100u64 << 20);
        assert!(
            required > last_byte,
            "no room reserved for the secondary GPT"
        );
        assert!(required <= last_byte + 64 * 1024);
    }

    #[test]
    fn duplicate_identifiers_are_caught() {
        let l = layout_with(disk_with(
            vec![
                part("p-1", 1, 1 << 20, 10 << 20, PartitionRole::EfiSystem),
                part("p-1", 2, 20 << 20, 10 << 20, PartitionRole::Windows),
            ],
            1024 << 20,
        ));
        assert!(l
            .validate()
            .iter()
            .any(|i| i.problem.contains("duplicate partition identifier")));
    }

    #[test]
    fn an_empty_document_is_refused() {
        let l = DiskLayout::new("99999999-8888-7777-6666-555555555555");
        assert!(l
            .validate()
            .iter()
            .any(|i| i.problem.contains("no disks at all")));
    }

    #[test]
    fn boot_critical_roles_are_flagged() {
        assert!(PartitionRole::EfiSystem.is_required_for_boot());
        assert!(PartitionRole::Windows.is_required_for_boot());
        assert!(PartitionRole::MicrosoftReserved.is_required_for_boot());
        assert!(!PartitionRole::Data.is_required_for_boot());
    }
}
