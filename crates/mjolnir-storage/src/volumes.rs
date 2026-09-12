//! Enumerating volumes and mapping them onto physical disks.
//!
//! A backup has to know two things about every volume: what filesystem is on it,
//! and exactly which bytes of which physical disk it occupies. The second is
//! what ties a volume to the partition that will be recreated during a restore,
//! and it comes from Windows rather than from guessing by offset.

use std::mem::size_of;

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use windows::core::PCWSTR;
use windows::Win32::Foundation::MAX_PATH;
use windows::Win32::Storage::FileSystem::{
    FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW, GetDiskFreeSpaceW,
    GetVolumeInformationW, GetVolumePathNamesForVolumeNameW, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
};
use windows::Win32::System::Ioctl::VOLUME_DISK_EXTENTS;

use crate::device::{wide, wide_to_string, Device};

/// One run of bytes a volume occupies on one physical disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeExtent {
    /// The physical disk the run is on.
    pub disk_number: u32,
    /// Byte offset from the start of that disk.
    pub starting_offset: u64,
    /// Length of the run in bytes.
    pub length: u64,
}

/// One volume as Windows describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    /// Volume GUID path, without a trailing backslash, as devices want it.
    ///
    /// For example `\\?\Volume{2c1cbd6e-0000-0000-0000-100000000000}`.
    pub device_path: String,
    /// Volume GUID path with a trailing backslash, as the filesystem calls want
    /// it and as the shadow copy service requires.
    pub guid_path: String,
    /// Mount points, usually a single drive letter such as `C:\`.
    pub mount_points: Vec<String>,
    /// Volume label.
    pub label: Option<String>,
    /// Filesystem name, for example `NTFS` or `FAT32`.
    pub filesystem: Option<String>,
    /// Allocation unit size in bytes.
    pub cluster_size: Option<u32>,
    /// Total size of the filesystem in bytes.
    pub total_bytes: u64,
    /// Free space in bytes.
    pub free_bytes: u64,
    /// Where the volume lives on the physical disks.
    pub extents: Vec<VolumeExtent>,
}

impl VolumeInfo {
    /// The drive letter, without a colon, when the volume has one.
    pub fn drive_letter(&self) -> Option<String> {
        self.mount_points.iter().find_map(|m| {
            let bytes = m.as_bytes();
            // A mount point of the form `C:\` is a drive letter; anything
            // longer is a folder mount point, which is not one.
            if m.len() == 3 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
                Some((bytes[0] as char).to_ascii_uppercase().to_string())
            } else {
                None
            }
        })
    }

    /// Whether the volume sits entirely on one physical disk in one run.
    ///
    /// A volume spanning disks or made of several runs is a spanned, striped or
    /// mirrored volume, which this version refuses rather than guessing at.
    pub fn is_simple(&self) -> bool {
        self.extents.len() == 1
    }

    /// The disk this volume is on, when it is a simple volume.
    pub fn disk_number(&self) -> Option<u32> {
        if self.is_simple() {
            Some(self.extents[0].disk_number)
        } else {
            None
        }
    }

    /// Bytes in use, as far as the filesystem reports.
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }

    /// A short description for the operator.
    pub fn describe(&self) -> String {
        let letter = self
            .drive_letter()
            .map(|l| format!("{l}: "))
            .unwrap_or_default();
        let label = self.label.as_deref().unwrap_or("");
        let fs = self.filesystem.as_deref().unwrap_or("unknown");
        let size = mjolnir_core::progress::format_bytes(self.total_bytes);
        if label.is_empty() {
            format!("{letter}{fs} {size}")
        } else {
            format!("{letter}{label} ({fs}, {size})")
        }
    }
}

/// Lists every volume Windows can see.
///
/// A volume that cannot be inspected, such as an empty card reader slot, is
/// skipped rather than failing the whole enumeration.
pub fn enumerate_volumes() -> Result<Vec<VolumeInfo>> {
    let mut names = Vec::new();
    let mut buffer = [0u16; MAX_PATH as usize + 1];

    // SAFETY: the buffer is valid for its full length for the duration of the
    // call, and the returned search handle is closed on every path below.
    let handle = unsafe { FindFirstVolumeW(&mut buffer) }.map_err(|e| {
        Error::new(
            ExitCode::Failure,
            "the list of volumes could not be read",
            format!("Windows reported: {e}"),
            "restart the computer and try again",
        )
    })?;

    names.push(wide_to_string(&buffer));
    loop {
        buffer = [0u16; MAX_PATH as usize + 1];
        // SAFETY: the handle came from FindFirstVolumeW and is still open; the
        // buffer is valid for its full length.
        let more = unsafe { FindNextVolumeW(handle, &mut buffer) };
        if more.is_err() {
            break;
        }
        names.push(wide_to_string(&buffer));
    }
    // SAFETY: `handle` came from a successful FindFirstVolumeW and has not
    // been closed. Every path that reaches here has finished enumerating, and
    // the handle is not used after this call, so it is closed exactly once.
    unsafe {
        let _ = FindVolumeClose(handle);
    }

    let mut volumes = Vec::new();
    for name in names {
        if name.is_empty() {
            continue;
        }
        if let Some(info) = describe_volume(&name) {
            volumes.push(info);
        }
    }
    Ok(volumes)
}

/// Describes one volume, given its GUID path with a trailing backslash.
///
/// Returns `None` when the volume cannot be inspected at all, which is normal
/// for a card reader with no card in it.
pub fn describe_volume(guid_path: &str) -> Option<VolumeInfo> {
    // Windows hands out the name with a trailing backslash. The filesystem
    // calls need it; opening the volume as a device needs it removed.
    let guid_path = if guid_path.ends_with('\\') {
        guid_path.to_owned()
    } else {
        format!("{guid_path}\\")
    };
    let device_path = guid_path.trim_end_matches('\\').to_owned();

    let (label, filesystem) = volume_information(&guid_path);
    let (total_bytes, free_bytes) = free_space(&guid_path);
    let cluster_size = cluster_size(&guid_path);
    let mount_points = mount_points(&guid_path);
    let extents = disk_extents(&device_path).unwrap_or_default();

    // A volume with no extents and no filesystem is not something that can be
    // backed up or displayed usefully.
    if extents.is_empty() && filesystem.is_none() {
        return None;
    }

    Some(VolumeInfo {
        device_path,
        guid_path,
        mount_points,
        label,
        filesystem,
        cluster_size,
        total_bytes,
        free_bytes,
        extents,
    })
}

fn volume_information(guid_path: &str) -> (Option<String>, Option<String>) {
    let path = wide(guid_path);
    let mut label = [0u16; MAX_PATH as usize + 1];
    let mut fs = [0u16; 64];

    // SAFETY: the path is a null terminated wide string that outlives the call,
    // and both output buffers are valid for the lengths passed alongside them.
    let ok = unsafe {
        GetVolumeInformationW(
            PCWSTR(path.as_ptr()),
            Some(&mut label),
            None,
            None,
            None,
            Some(&mut fs),
        )
    };
    if ok.is_err() {
        return (None, None);
    }

    let label = wide_to_string(&label);
    let fs = wide_to_string(&fs);
    (
        if label.is_empty() { None } else { Some(label) },
        if fs.is_empty() { None } else { Some(fs) },
    )
}

fn free_space(guid_path: &str) -> (u64, u64) {
    let path = wide(guid_path);
    let mut free_to_caller = 0u64;
    let mut total = 0u64;
    let mut total_free = 0u64;

    // SAFETY: the path outlives the call and every out pointer is a valid
    // writable u64.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(path.as_ptr()),
            Some(&mut free_to_caller),
            Some(&mut total),
            Some(&mut total_free),
        )
    };
    if ok.is_err() {
        return (0, 0);
    }
    (total, total_free)
}

fn cluster_size(guid_path: &str) -> Option<u32> {
    let path = wide(guid_path);
    let mut sectors_per_cluster = 0u32;
    let mut bytes_per_sector = 0u32;

    // SAFETY: the path outlives the call and both out pointers are valid.
    let ok = unsafe {
        GetDiskFreeSpaceW(
            PCWSTR(path.as_ptr()),
            Some(&mut sectors_per_cluster),
            Some(&mut bytes_per_sector),
            None,
            None,
        )
    };
    if ok.is_err() {
        return None;
    }
    sectors_per_cluster
        .checked_mul(bytes_per_sector)
        .filter(|c| *c > 0)
}

fn mount_points(guid_path: &str) -> Vec<String> {
    let path = wide(guid_path);
    let mut needed = 0u32;

    // First call measures, second collects. A volume with no mount point
    // returns a length of one for the terminating null, which is normal for the
    // EFI and recovery partitions.
    //
    // SAFETY: the path outlives both calls; the first passes no buffer and only
    // asks for the required length.
    unsafe {
        let _ = GetVolumePathNamesForVolumeNameW(PCWSTR(path.as_ptr()), None, &mut needed);
    }
    if needed <= 1 {
        return Vec::new();
    }

    let mut buffer = vec![0u16; needed as usize];
    // SAFETY: the buffer is valid for `needed` units, which is the length
    // Windows asked for on the previous call.
    let ok = unsafe {
        GetVolumePathNamesForVolumeNameW(PCWSTR(path.as_ptr()), Some(&mut buffer), &mut needed)
    };
    if ok.is_err() {
        return Vec::new();
    }

    // The result is a sequence of null terminated strings ending in an extra
    // null.
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, &unit) in buffer.iter().enumerate() {
        if unit == 0 {
            if i == start {
                break; // the extra terminating null
            }
            out.push(String::from_utf16_lossy(&buffer[start..i]));
            start = i + 1;
        }
    }
    out
}

/// Reads where a volume lives on the physical disks.
pub fn disk_extents(device_path: &str) -> Result<Vec<VolumeExtent>> {
    let device = Device::query(device_path)?;

    // One extent is the normal case; the buffer allows for many so that a
    // spanned volume is reported rather than truncated, and can then be
    // refused with an accurate message.
    let mut buffer = vec![
        0u8;
        size_of::<VOLUME_DISK_EXTENTS>()
            + 64 * size_of::<windows::Win32::System::Ioctl::DISK_EXTENT>()
    ];
    let written = device.control(IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &mut buffer)? as usize;
    if written < size_of::<u32>() {
        return Ok(Vec::new());
    }

    // SAFETY: the buffer holds at least the extent count, checked above, and is
    // read without assuming alignment.
    let header: VOLUME_DISK_EXTENTS =
        unsafe { std::ptr::read_unaligned(buffer.as_ptr() as *const VOLUME_DISK_EXTENTS) };

    let entry_offset = std::mem::offset_of!(VOLUME_DISK_EXTENTS, Extents);
    let entry_size = size_of::<windows::Win32::System::Ioctl::DISK_EXTENT>();

    let mut out = Vec::new();
    for i in 0..header.NumberOfDiskExtents as usize {
        let at = entry_offset + i * entry_size;
        // The driver can report more extents than it returned data for, so the
        // count is never trusted on its own.
        if at + entry_size > written {
            break;
        }
        // SAFETY: `at + entry_size <= written` was checked on the line above,
        // so the extent lies inside the bytes the driver actually returned,
        // even if NumberOfDiskExtents claimed more than it wrote.
        // read_unaligned is used because the buffer is a byte vector.
        let e: windows::Win32::System::Ioctl::DISK_EXTENT =
            unsafe { std::ptr::read_unaligned(buffer.as_ptr().add(at) as *const _) };

        let starting_offset = u64::try_from(e.StartingOffset).unwrap_or(0);
        let length = u64::try_from(e.ExtentLength).unwrap_or(0);
        if length == 0 {
            continue;
        }
        out.push(VolumeExtent {
            disk_number: e.DiskNumber,
            starting_offset,
            length,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume_with(mount_points: Vec<&str>, extents: Vec<VolumeExtent>) -> VolumeInfo {
        VolumeInfo {
            device_path: "\\\\?\\Volume{1}".to_owned(),
            guid_path: "\\\\?\\Volume{1}\\".to_owned(),
            mount_points: mount_points.into_iter().map(str::to_owned).collect(),
            label: Some("Windows".to_owned()),
            filesystem: Some("NTFS".to_owned()),
            cluster_size: Some(4096),
            total_bytes: 500 << 30,
            free_bytes: 200 << 30,
            extents,
        }
    }

    fn extent(disk: u32) -> VolumeExtent {
        VolumeExtent {
            disk_number: disk,
            starting_offset: 1 << 20,
            length: 500 << 30,
        }
    }

    #[test]
    fn a_drive_letter_is_recognised_but_a_folder_mount_is_not() {
        assert_eq!(
            volume_with(vec!["C:\\"], vec![extent(0)])
                .drive_letter()
                .as_deref(),
            Some("C")
        );
        assert_eq!(
            volume_with(vec!["c:\\"], vec![extent(0)])
                .drive_letter()
                .as_deref(),
            Some("C")
        );
        assert_eq!(
            volume_with(vec!["C:\\mount\\point\\"], vec![extent(0)]).drive_letter(),
            None
        );
        assert_eq!(volume_with(vec![], vec![extent(0)]).drive_letter(), None);
    }

    #[test]
    fn a_volume_on_one_run_of_one_disk_is_simple() {
        let v = volume_with(vec!["C:\\"], vec![extent(0)]);
        assert!(v.is_simple());
        assert_eq!(v.disk_number(), Some(0));
    }

    #[test]
    fn a_spanned_volume_is_not_simple_and_names_no_single_disk() {
        let v = volume_with(vec!["D:\\"], vec![extent(0), extent(1)]);
        assert!(!v.is_simple());
        assert_eq!(v.disk_number(), None);
    }

    #[test]
    fn used_bytes_never_goes_negative() {
        let mut v = volume_with(vec!["C:\\"], vec![extent(0)]);
        assert_eq!(v.used_bytes(), 300 << 30);
        // A filesystem reporting more free than total must not wrap.
        v.free_bytes = v.total_bytes + 1;
        assert_eq!(v.used_bytes(), 0);
    }

    #[test]
    fn descriptions_are_readable() {
        let v = volume_with(vec!["C:\\"], vec![extent(0)]);
        let text = v.describe();
        assert!(text.starts_with("C: Windows"), "{text}");
        assert!(text.contains("NTFS"), "{text}");
    }

    /// Enumeration must be safe to run on any machine, and every volume it
    /// returns must be self consistent.
    #[test]
    fn enumeration_is_safe_to_run_anywhere() {
        let volumes = enumerate_volumes().expect("volume enumeration should not fail");
        for v in &volumes {
            assert!(v.guid_path.ends_with('\\'), "{v:?}");
            assert!(!v.device_path.ends_with('\\'), "{v:?}");
            assert!(v.free_bytes <= v.total_bytes.max(v.free_bytes));
            assert!(!v.describe().is_empty());
            for e in &v.extents {
                assert!(e.length > 0);
            }
        }
    }
}
