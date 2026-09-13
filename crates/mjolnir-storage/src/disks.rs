//! Enumerating physical disks and reading their partition tables.
//!
//! Everything here asks Windows rather than parsing bytes off the disk, because
//! the kernel's view is the one that matters: it is what the volume layer, the
//! shadow copy service and a later restore all agree with. The GPT parser in
//! [`crate::gpt`] is used to check that view, not to replace it.
//!
//! Disks are opened without any access rights for enumeration, which works
//! without administrator rights. That is what lets the main window show the
//! operator what it found before anything else happens.

use std::mem::{offset_of, size_of};

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::math;
use mjolnir_image::disk_layout::{BusType, PartitionStyle};
use windows::Win32::System::Ioctl::{
    PropertyStandardQuery, StorageAccessAlignmentProperty, StorageDeviceProperty, DISK_GEOMETRY_EX,
    DRIVE_LAYOUT_INFORMATION_EX, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_LAYOUT_EX,
    IOCTL_STORAGE_QUERY_PROPERTY, PARTITION_INFORMATION_EX, PARTITION_STYLE_GPT,
    PARTITION_STYLE_MBR, STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, STORAGE_DEVICE_DESCRIPTOR,
    STORAGE_PROPERTY_QUERY,
};

use crate::device::Device;

/// Highest physical drive number MjolnirVSS looks for.
///
/// Windows numbers disks from zero with no gaps in practice, but a removed disk
/// can leave one, so enumeration probes a fixed range rather than stopping at
/// the first miss.
pub const MAX_DISK_NUMBER: u32 = 64;

/// Bus type value Windows uses for a Storage Spaces virtual disk.
///
/// Recognised so that configuration can be refused with an explanation rather
/// than backed up as though it were an ordinary disk.
const BUS_TYPE_SPACES: i32 = 16;

/// One partition as Windows describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalPartition {
    /// Partition number as Windows reports it.
    pub number: u32,
    /// Byte offset from the start of the disk.
    pub starting_offset: u64,
    /// Length in bytes.
    pub length: u64,
    /// GPT partition type GUID, lower case.
    pub type_guid: String,
    /// GPT unique partition GUID, lower case.
    pub unique_guid: String,
    /// GPT partition name.
    pub name: String,
    /// GPT attribute flags.
    pub attributes: u64,
}

/// One physical disk as Windows describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalDisk {
    /// Disk number, the `N` in `\\.\PhysicalDriveN`.
    pub number: u32,
    /// The path the disk was opened with.
    pub device_path: String,
    /// Total size in bytes.
    pub size_bytes: u64,
    /// Logical sector size, the unit every offset is a multiple of.
    pub logical_sector_size: u32,
    /// Physical sector size.
    pub physical_sector_size: u32,
    /// Model string, if the device reports one.
    pub model: Option<String>,
    /// Serial number, if the device reports one.
    pub serial: Option<String>,
    /// How the disk is attached.
    pub bus_type: BusType,
    /// The raw bus type value, kept so unusual buses can be recognised.
    pub raw_bus_type: i32,
    /// Whether the medium is removable.
    pub removable: bool,
    /// Partition table style.
    pub partition_style: PartitionStyle,
    /// GPT disk GUID, lower case, when the disk is GPT.
    pub disk_guid: Option<String>,
    /// The partitions found on the disk.
    pub partitions: Vec<PhysicalPartition>,
}

impl PhysicalDisk {
    /// Whether this is a Storage Spaces virtual disk.
    pub fn is_storage_spaces(&self) -> bool {
        self.raw_bus_type == BUS_TYPE_SPACES
    }

    /// A one line description for the operator, as shown before an erase.
    pub fn describe(&self) -> String {
        let model = self.model.as_deref().unwrap_or("Unknown model");
        let serial = self.serial.as_deref().unwrap_or("no serial number");
        format!(
            "Disk {} - {} - {} - {} ({})",
            self.number,
            model,
            mjolnir_core::progress::format_bytes(self.size_bytes),
            self.bus_type.describe(),
            serial
        )
    }
}

/// Lists every physical disk Windows can see.
///
/// A disk that cannot be opened or queried is left out rather than failing the
/// whole enumeration: a card reader with no card in it should not stop a backup.
pub fn enumerate_disks() -> Vec<PhysicalDisk> {
    let mut disks = Vec::new();
    for number in 0..MAX_DISK_NUMBER {
        if let Ok(disk) = describe_disk(number) {
            disks.push(disk);
        }
    }
    disks
}

/// Describes one physical disk.
pub fn describe_disk(number: u32) -> Result<PhysicalDisk> {
    let path = format!("\\\\.\\PhysicalDrive{number}");
    let device = Device::query(&path)?;

    let (size_bytes, geometry_sector) = read_geometry(&device)?;
    let (logical_sector_size, physical_sector_size) = read_alignment(&device, geometry_sector);
    let (model, serial, bus_type, raw_bus_type, removable) = read_device_descriptor(&device);
    // A disk with no partition table on it still answers for its size, and it
    // is the most important disk there is to a recovery tool: the blank one
    // just fitted to replace a failed drive. Windows refuses to describe the
    // layout of an uninitialised or offline disk, and treating that as "this
    // disk does not exist" left the one disk somebody wants to restore onto
    // missing from the list. Found by restoring onto a blank disk in Windows PE.
    let (partition_style, disk_guid, partitions) = read_layout(&device, logical_sector_size)
        .unwrap_or((PartitionStyle::Raw, None, Vec::new()));

    Ok(PhysicalDisk {
        number,
        device_path: path,
        size_bytes,
        logical_sector_size,
        physical_sector_size,
        model,
        serial,
        bus_type,
        raw_bus_type,
        removable,
        partition_style,
        disk_guid,
        partitions,
    })
}

fn read_geometry(device: &Device) -> Result<(u64, u32)> {
    let mut buffer = vec![0u8; size_of::<DISK_GEOMETRY_EX>() + 512];
    let written = device.control(IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &mut buffer)?;
    if (written as usize) < size_of::<DISK_GEOMETRY_EX>() {
        return Err(short_answer("the disk geometry"));
    }

    // SAFETY: the buffer holds at least one DISK_GEOMETRY_EX, checked above.
    // read_unaligned is used because the buffer is a plain byte vector with no
    // alignment guarantee.
    let geometry: DISK_GEOMETRY_EX =
        unsafe { std::ptr::read_unaligned(buffer.as_ptr() as *const DISK_GEOMETRY_EX) };

    let size = u64::try_from(geometry.DiskSize).map_err(|_| {
        Error::unsupported(
            "Windows reported a negative disk size",
            "the driver returned something MjolnirVSS cannot interpret, so the disk cannot be measured safely",
            "this disk cannot be backed up by this version",
        )
    })?;
    Ok((size, geometry.Geometry.BytesPerSector))
}

fn read_alignment(device: &Device, fallback_sector: u32) -> (u32, u32) {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageAccessAlignmentProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    // SAFETY: reinterpreting a plain data structure as its own bytes, for the
    // length of that structure, to hand to a control code that expects exactly
    // this layout.
    let input = unsafe {
        std::slice::from_raw_parts(
            &query as *const STORAGE_PROPERTY_QUERY as *const u8,
            size_of::<STORAGE_PROPERTY_QUERY>(),
        )
    };

    let mut buffer = vec![0u8; size_of::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>() + 64];
    match device.control_with(IOCTL_STORAGE_QUERY_PROPERTY, input, &mut buffer) {
        Ok(written) if written as usize >= size_of::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>() => {
            // SAFETY: the driver reported writing at least
            // size_of::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>() bytes, checked
            // by the match arm above, so the read stays inside the buffer.
            // read_unaligned is used because `buffer` is a Vec<u8> with only
            // byte alignment while the descriptor wants more.
            let d: STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR = unsafe {
                std::ptr::read_unaligned(
                    buffer.as_ptr() as *const STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR
                )
            };
            let logical = if d.BytesPerLogicalSector == 0 {
                fallback_sector
            } else {
                d.BytesPerLogicalSector
            };
            let physical = if d.BytesPerPhysicalSector == 0 {
                logical
            } else {
                d.BytesPerPhysicalSector
            };
            (logical, physical)
        }
        // Not every device reports alignment. Falling back to the geometry's
        // sector size is correct for a 512 native disk and for a 512e disk
        // reports the same logical size Windows uses for every offset.
        _ => (fallback_sector, fallback_sector),
    }
}

fn read_device_descriptor(device: &Device) -> (Option<String>, Option<String>, BusType, i32, bool) {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    // SAFETY: `query` is a live local, and the slice is built with exactly
    // its own size, so it cannot describe memory beyond it. The structure is
    // plain data with no padding the control code cares about, and it is only
    // read by the driver for the duration of the call.
    let input = unsafe {
        std::slice::from_raw_parts(
            &query as *const STORAGE_PROPERTY_QUERY as *const u8,
            size_of::<STORAGE_PROPERTY_QUERY>(),
        )
    };

    let mut buffer = vec![0u8; 4096];
    let Ok(written) = device.control_with(IOCTL_STORAGE_QUERY_PROPERTY, input, &mut buffer) else {
        return (None, None, BusType::Other, 0, false);
    };
    let written = written as usize;
    if written < size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
        return (None, None, BusType::Other, 0, false);
    }

    // SAFETY: `written` was checked against
    // size_of::<STORAGE_DEVICE_DESCRIPTOR>() above, so the fixed part of the
    // descriptor is wholly inside the buffer. read_unaligned is used because
    // the buffer is a byte vector. The variable length strings that follow are
    // not read here; ascii_at bounds checks each one against `written`.
    let d: STORAGE_DEVICE_DESCRIPTOR =
        unsafe { std::ptr::read_unaligned(buffer.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };

    // The strings sit after the fixed part, at byte offsets the descriptor
    // gives. An offset of zero means the device did not report that field. Each
    // one is bounds checked against what the driver actually returned, because
    // trusting the offset blindly would read past the buffer.
    let vendor = ascii_at(&buffer[..written], d.VendorIdOffset as usize);
    let product = ascii_at(&buffer[..written], d.ProductIdOffset as usize);
    let serial = ascii_at(&buffer[..written], d.SerialNumberOffset as usize);

    let model = match (vendor, product) {
        (Some(v), Some(p)) if !v.is_empty() && !p.is_empty() => Some(format!("{v} {p}")),
        (Some(v), None) if !v.is_empty() => Some(v),
        (None, Some(p)) | (Some(_), Some(p)) if !p.is_empty() => Some(p),
        _ => None,
    };

    let raw_bus = d.BusType.0;
    (
        model,
        serial.filter(|s| !s.is_empty()),
        map_bus_type(raw_bus),
        raw_bus,
        d.RemovableMedia,
    )
}

/// Reads a null terminated ASCII string at `offset` inside `buffer`.
fn ascii_at(buffer: &[u8], offset: usize) -> Option<String> {
    if offset == 0 || offset >= buffer.len() {
        return None;
    }
    let tail = &buffer[offset..];
    let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
    let text = String::from_utf8_lossy(&tail[..end]).trim().to_owned();
    Some(text)
}

fn map_bus_type(raw: i32) -> BusType {
    match raw {
        1 => BusType::Scsi,
        2 | 3 => BusType::Sata, // ATAPI and ATA
        7 => BusType::Usb,
        10 => BusType::Sas,
        11 => BusType::Sata,
        12 | 13 => BusType::Sd, // SD and MMC
        14 | 15 => BusType::Virtual,
        17 => BusType::Nvme,
        _ => BusType::Other,
    }
}

fn read_layout(
    device: &Device,
    sector_size: u32,
) -> Result<(PartitionStyle, Option<String>, Vec<PhysicalPartition>)> {
    // A GPT disk conventionally has room for 128 entries. The buffer is sized
    // for that plus headroom, and grows once if Windows says it is too small.
    let mut capacity =
        size_of::<DRIVE_LAYOUT_INFORMATION_EX>() + 160 * size_of::<PARTITION_INFORMATION_EX>();
    let mut buffer = vec![0u8; capacity];
    let written = loop {
        match device.control(IOCTL_DISK_GET_DRIVE_LAYOUT_EX, &mut buffer) {
            Ok(written) => break written as usize,
            Err(e) if capacity < 1 << 20 => {
                capacity *= 4;
                buffer = vec![0u8; capacity];
                if capacity >= 1 << 20 {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    };

    if written < size_of::<DRIVE_LAYOUT_INFORMATION_EX>() {
        return Err(short_answer("the partition table"));
    }

    // SAFETY: the buffer holds at least the fixed part of the structure,
    // checked above, and is read without assuming alignment.
    let header: DRIVE_LAYOUT_INFORMATION_EX =
        unsafe { std::ptr::read_unaligned(buffer.as_ptr() as *const DRIVE_LAYOUT_INFORMATION_EX) };

    let style = match header.PartitionStyle {
        s if s == PARTITION_STYLE_GPT.0 as u32 => PartitionStyle::Gpt,
        s if s == PARTITION_STYLE_MBR.0 as u32 => PartitionStyle::Mbr,
        _ => PartitionStyle::Raw,
    };

    let disk_guid = if style == PartitionStyle::Gpt {
        // SAFETY: the union is only read through the arm the PartitionStyle
        // field selects, and `style` was just compared against
        // PARTITION_STYLE_GPT, so the GPT arm is the initialised one. It is
        // plain data and is copied out rather than borrowed.
        let gpt = unsafe { header.Anonymous.Gpt };
        Some(guid_to_string(&gpt.DiskId))
    } else {
        None
    };

    let entry_offset = offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry);
    let entry_size = size_of::<PARTITION_INFORMATION_EX>();
    let mut partitions = Vec::new();

    for i in 0..header.PartitionCount as usize {
        let at = entry_offset + i * entry_size;
        // Windows can report a partition count larger than the data it
        // actually returned. Stopping here rather than trusting the count is
        // what keeps this from reading past the buffer.
        if at + entry_size > written {
            break;
        }
        // SAFETY: `at + entry_size <= written` was checked on the line above,
        // so the whole entry lies inside the bytes the driver actually wrote,
        // regardless of the partition count it claimed. read_unaligned is used
        // because the buffer is a byte vector with no alignment guarantee.
        let entry: PARTITION_INFORMATION_EX = unsafe {
            std::ptr::read_unaligned(buffer.as_ptr().add(at) as *const PARTITION_INFORMATION_EX)
        };

        if entry.PartitionStyle != PARTITION_STYLE_GPT {
            // An MBR disk is refused higher up, with a better explanation than
            // a half filled partition list would give.
            continue;
        }
        // SAFETY: the union is only read through the arm the adjacent
        // PartitionStyle field selects, and the `continue` above has already
        // rejected anything that is not GPT, so the GPT arm is the initialised
        // one. The arm is plain data and is copied out rather than borrowed.
        let gpt = unsafe { entry.Anonymous.Gpt };

        let starting_offset = u64::try_from(entry.StartingOffset).map_err(|_| {
            Error::unsupported(
                "Windows reported a partition at a negative offset",
                "the partition table contains something MjolnirVSS cannot interpret",
                "this disk cannot be backed up by this version",
            )
        })?;
        let length = u64::try_from(entry.PartitionLength).map_err(|_| {
            Error::unsupported(
                "Windows reported a partition with a negative length",
                "the partition table contains something MjolnirVSS cannot interpret",
                "this disk cannot be backed up by this version",
            )
        })?;

        // An empty entry is a hole in the table, not a partition.
        if length == 0 {
            continue;
        }
        math::ensure_aligned("partition offset", starting_offset, u64::from(sector_size))?;

        partitions.push(PhysicalPartition {
            number: entry.PartitionNumber,
            starting_offset,
            length,
            type_guid: guid_to_string(&gpt.PartitionType),
            unique_guid: guid_to_string(&gpt.PartitionId),
            name: crate::device::wide_to_string(&gpt.Name),
            attributes: gpt.Attributes.0,
        });
    }

    partitions.sort_by_key(|p| p.starting_offset);
    Ok((style, disk_guid, partitions))
}

/// Formats a Windows `GUID` in the canonical lower case form.
pub fn guid_to_string(guid: &windows::core::GUID) -> String {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&guid.data1.to_le_bytes());
    bytes[4..6].copy_from_slice(&guid.data2.to_le_bytes());
    bytes[6..8].copy_from_slice(&guid.data3.to_le_bytes());
    bytes[8..16].copy_from_slice(&guid.data4);
    mjolnir_image::format_guid(&bytes)
}

fn short_answer(what: &str) -> Error {
    Error::new(
        ExitCode::Io,
        format!("Windows returned an incomplete answer for {what}"),
        "the driver replied with less data than the structure requires, which usually means the device was removed while it was being queried".to_owned(),
        "reconnect the drive and try again",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_types_map_to_something_meaningful() {
        assert_eq!(map_bus_type(17), BusType::Nvme);
        assert_eq!(map_bus_type(11), BusType::Sata);
        assert_eq!(map_bus_type(7), BusType::Usb);
        assert_eq!(map_bus_type(14), BusType::Virtual);
        assert_eq!(map_bus_type(BUS_TYPE_SPACES), BusType::Other);
        assert_eq!(map_bus_type(999), BusType::Other);
    }

    #[test]
    fn ascii_at_is_bounds_checked() {
        let buffer = b"\x00\x00hello\x00world\x00";
        assert_eq!(ascii_at(buffer, 2).as_deref(), Some("hello"));
        // Offset zero means the device did not report the field.
        assert_eq!(ascii_at(buffer, 0), None);
        // An offset past the end must not panic or read out of bounds.
        assert_eq!(ascii_at(buffer, 9999), None);
        assert_eq!(ascii_at(buffer, buffer.len()), None);
        // An unterminated tail still produces a string.
        assert_eq!(ascii_at(b"abc", 1).as_deref(), Some("bc"));
    }

    #[test]
    fn ascii_at_trims_padding() {
        // Device descriptors routinely pad strings with spaces.
        let buffer = b"\x00  Samsung SSD   \x00";
        assert_eq!(ascii_at(buffer, 1).as_deref(), Some("Samsung SSD"));
    }

    #[test]
    fn guids_format_in_the_canonical_order() {
        let guid = windows::core::GUID::from_u128(0xc12a7328_f81f_11d2_ba4b_00a0c93ec93b);
        assert_eq!(
            guid_to_string(&guid),
            "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
        );
    }

    #[test]
    fn storage_spaces_is_recognised() {
        let disk = PhysicalDisk {
            number: 0,
            device_path: "\\\\.\\PhysicalDrive0".to_owned(),
            size_bytes: 1 << 40,
            logical_sector_size: 512,
            physical_sector_size: 4096,
            model: Some("Storage Space".to_owned()),
            serial: None,
            bus_type: BusType::Other,
            raw_bus_type: BUS_TYPE_SPACES,
            removable: false,
            partition_style: PartitionStyle::Gpt,
            disk_guid: None,
            partitions: Vec::new(),
        };
        assert!(disk.is_storage_spaces());
        assert!(disk.describe().contains("Disk 0"));
        assert!(disk.describe().contains("Storage Space"));
    }

    /// Enumeration must never panic, whatever this machine happens to have
    /// attached. It does not assert on the contents, because a build machine
    /// and a developer laptop have nothing in common.
    #[test]
    fn enumeration_is_safe_to_run_anywhere() {
        let disks = enumerate_disks();
        for disk in &disks {
            assert!(disk.size_bytes > 0, "{disk:?}");
            assert!(disk.logical_sector_size.is_power_of_two(), "{disk:?}");
            assert!(!disk.describe().is_empty());
            for p in &disk.partitions {
                assert!(p.length > 0);
            }
        }
    }
}
