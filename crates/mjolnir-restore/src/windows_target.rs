//! Opening a real disk for writing, on Windows.
//!
//! This is the only module in MjolnirVSS that asks Windows for write access to
//! a disk. It lives in the restore crate rather than in the storage crate so
//! that no code on the backup side can reach it, even by accident.
//!
//! It does not appear in a backup binary, and it does not use the shadow copy
//! service, so the recovery application that contains it can start inside
//! Windows PE.

use std::ffi::c_void;

use mjolnir_core::blockio::BlockSink;
use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::math;
use mjolnir_storage::disks::PhysicalDisk;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, IOCTL_DISK_UPDATE_PROPERTIES,
};
use windows::Win32::System::IO::{DeviceIoControl, OVERLAPPED};

use crate::target::TargetDisk;

/// Largest single write issued to a device.
const MAX_TRANSFER: usize = 8 * 1024 * 1024;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Lists the disks that could be restored onto.
///
/// `backup_disk_numbers` names the disks holding the backup being restored, so
/// they can be marked and refused. Nothing is ever preselected.
pub fn enumerate_targets(backup_disk_numbers: &[u32]) -> Vec<TargetDisk> {
    mjolnir_storage::disks::enumerate_disks()
        .into_iter()
        .map(|disk| describe_target(&disk, backup_disk_numbers))
        .collect()
}

/// Describes one disk as a possible restore target.
pub fn describe_target(disk: &PhysicalDisk, backup_disk_numbers: &[u32]) -> TargetDisk {
    let existing_partitions = disk
        .partitions
        .iter()
        .map(|p| {
            format!(
                "Partition {} - {} - {} at offset {}",
                p.number,
                mjolnir_image::disk_layout::PartitionRole::from_type_guid(&p.type_guid).describe(),
                mjolnir_core::progress::format_bytes(p.length),
                p.starting_offset
            )
        })
        .collect();

    TargetDisk {
        number: disk.number,
        device_path: disk.device_path.clone(),
        size_bytes: disk.size_bytes,
        logical_sector_size: disk.logical_sector_size,
        model: disk.model.clone(),
        serial: disk.serial.clone(),
        bus: disk.bus_type.describe().to_owned(),
        existing_partitions,
        holds_the_backup: backup_disk_numbers.contains(&disk.number),
    }
}

/// A physical disk open for writing.
///
/// Dropping it closes the handle. Nothing is written that was not asked for.
pub struct WritableDisk {
    handle: HANDLE,
    path: String,
    size_bytes: u64,
    sector_size: u32,
}

// SAFETY: the only field is a Windows file handle, which is a process wide
// kernel object identifier rather than anything thread affine. This type owns
// it exclusively, gives out no references to it, and is used from one thread at
// a time; moving it to the worker thread that performs the restore is the whole
// reason it exists.
unsafe impl Send for WritableDisk {}

impl WritableDisk {
    /// Opens a physical disk for writing.
    ///
    /// Every volume on the disk is locked and dismounted first. Without that,
    /// Windows keeps its own cached view of a filesystem that is being
    /// overwritten underneath it, and will write that stale view back over the
    /// restored data.
    pub fn open(target: &TargetDisk) -> Result<Self> {
        dismount_volumes_on(target.number);

        let path = wide(&target.device_path);
        // SAFETY: the path is a null terminated wide string that outlives the
        // call, and the optional arguments are absent.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(FILE_ATTRIBUTE_NORMAL.0),
                None,
            )
        }
        .map_err(|e| {
            const ERROR_ACCESS_DENIED: i32 = 0x8007_0005u32 as i32;
            if e.code().0 == ERROR_ACCESS_DENIED {
                Error::new(
                    ExitCode::AccessDenied,
                    format!("{} could not be opened for writing", target.device_path),
                    "Windows only allows a program to write directly to a disk when it is running with administrator rights",
                    "start the recovery application again from an administrator command prompt",
                )
            } else {
                Error::new(
                    ExitCode::Io,
                    format!("{} could not be opened for writing", target.device_path),
                    format!("Windows reported: {e}"),
                    "check the disk is still connected, then try again",
                )
            }
        })?;

        Ok(Self {
            handle,
            path: target.device_path.clone(),
            size_bytes: target.size_bytes,
            sector_size: target.logical_sector_size,
        })
    }

    /// Tells Windows to re read the partition table.
    ///
    /// Called once the table has been written, so the kernel's idea of the disk
    /// matches what is now on it and the new partitions appear.
    pub fn refresh_partition_table(&self) -> Result<()> {
        let mut returned = 0u32;
        // SAFETY: `self.handle` was opened for read and write by
        // WritableDisk::open and is closed only by Drop, so it is live here.
        // IOCTL_DISK_UPDATE_PROPERTIES takes neither an input nor an output
        // buffer, which is why both are None with zero lengths; `returned` is a
        // live local the call writes its byte count into.
        unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_DISK_UPDATE_PROPERTIES,
                None,
                0,
                None,
                0,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|e| {
            Error::new(
                ExitCode::Io,
                format!("Windows did not re read the partition table of {}", self.path),
                format!("the restore finished but Windows reported: {e}"),
                "restart the computer; the data has been written and the new layout will be picked up on the next start",
            )
        })
    }
}

impl Drop for WritableDisk {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // SAFETY: the handle came from CreateFileW, is owned solely here,
            // and is not used afterwards.
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

impl BlockSink for WritableDisk {
    fn describe(&self) -> String {
        self.path.clone()
    }

    fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    fn logical_sector_size(&self) -> u32 {
        self.sector_size
    }

    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.ensure_writable(offset, buf.len() as u64)?;

        let sector = u64::from(self.sector_size);
        // A disk only accepts whole sector writes at sector aligned offsets.
        // Everything MjolnirVSS writes is already aligned, because partitions
        // start on sector boundaries and chunks are multiples of 4096; this is
        // a guard against a future change breaking that quietly.
        if offset % sector != 0 || buf.len() as u64 % sector != 0 {
            return Err(Error::new(
                ExitCode::Failure,
                format!(
                    "an unaligned write of {} bytes at offset {offset} was attempted",
                    buf.len()
                ),
                format!(
                    "a disk only accepts writes that start and end on a {sector} byte boundary"
                ),
                "this is an internal error; please report it with the command you ran",
            ));
        }

        let mut done = 0usize;
        while done < buf.len() {
            let want = (buf.len() - done).min(MAX_TRANSFER);
            let at = math::add_u64("disk write offset", offset, done as u64)?;

            let mut overlapped = OVERLAPPED::default();
            overlapped.Anonymous.Anonymous.Offset = (at & 0xFFFF_FFFF) as u32;
            overlapped.Anonymous.Anonymous.OffsetHigh = (at >> 32) as u32;

            let mut written = 0u32;
            // SAFETY: the slice is valid for `want` bytes, the OVERLAPPED lives
            // until this synchronous call returns, and the handle is open for
            // writing.
            unsafe {
                WriteFile(
                    self.handle,
                    Some(&buf[done..done + want]),
                    Some(&mut written),
                    Some(&mut overlapped),
                )
            }
            .map_err(|e| {
                Error::new(
                    ExitCode::Io,
                    format!("writing to {} failed at offset {at}", self.path),
                    format!("Windows reported: {e}"),
                    "the target disk may be failing or may have been disconnected; the restore is incomplete and the disk will not boot",
                )
            })?;

            if written == 0 {
                return Err(Error::new(
                    ExitCode::Io,
                    format!("writing to {} stopped early at offset {at}", self.path),
                    "the disk accepted no bytes, which means the write went past the end of it",
                    "the restore is incomplete and the disk will not boot; check the disk and start again",
                ));
            }
            done += written as usize;
        }
        Ok(())
    }

    fn flush_device(&mut self) -> Result<()> {
        use windows::Win32::Storage::FileSystem::FlushFileBuffers;
        // SAFETY: `self.handle` was opened with write access and is live
        // until Drop. FlushFileBuffers takes only the handle, and is the call
        // that turns "the writes were accepted" into "the writes reached the
        // disk", which is why a restore is not reported as finished until it
        // returns.
        unsafe { FlushFileBuffers(self.handle) }.map_err(|e| {
            Error::new(
                ExitCode::Io,
                format!("the writes to {} could not be flushed", self.path),
                format!("Windows reported: {e}"),
                "do not restart yet; the disk may hold incomplete data. Check the disk and run the restore again",
            )
        })
    }
}

/// Locks and dismounts every volume on a disk, so Windows stops caching them.
///
/// Best effort. A blank replacement disk has no volumes, which is the normal
/// case during a recovery, and a volume that cannot be dismounted is reported by
/// the write itself rather than here.
fn dismount_volumes_on(disk_number: u32) {
    let Ok(volumes) = mjolnir_storage::volumes::enumerate_volumes() else {
        return;
    };
    for volume in volumes {
        if volume.disk_number() != Some(disk_number) {
            continue;
        }
        let path = wide(&volume.device_path);
        // SAFETY: `path` is a null terminated wide string in a local that
        // outlives the call, which copies it rather than retaining the pointer.
        // Opening a volume that has already gone away fails rather than
        // misbehaving, which is why the result is matched rather than assumed.
        let Ok(handle) = (unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(FILE_ATTRIBUTE_NORMAL.0),
                None,
            )
        }) else {
            continue;
        };

        let mut returned = 0u32;
        // SAFETY: the handle is an open volume; neither control code takes
        // buffers. Both are best effort, hence the ignored results.
        unsafe {
            let _ = DeviceIoControl(
                handle,
                FSCTL_LOCK_VOLUME,
                None,
                0,
                None,
                0,
                Some(&mut returned),
                None,
            );
            let _ = DeviceIoControl(
                handle,
                FSCTL_DISMOUNT_VOLUME,
                None,
                0,
                None,
                0,
                Some(&mut returned),
                None,
            );
            let _ = CloseHandle(handle);
        }
    }
    let _ = std::ptr::null::<c_void>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_are_null_terminated() {
        assert_eq!(wide("AB"), vec![65, 66, 0]);
    }

    /// Enumerating targets must be safe on any machine and must never
    /// preselect anything.
    #[test]
    fn enumerating_targets_is_safe_to_run_anywhere() {
        let targets = enumerate_targets(&[0]);
        for t in &targets {
            assert!(!t.describe().is_empty());
            assert!(t.erase_phrase().starts_with("ERASE "));
            if t.number == 0 {
                assert!(
                    t.holds_the_backup,
                    "disk 0 was named as holding the backup and must be marked"
                );
            }
        }
    }

    #[test]
    fn a_disk_holding_the_backup_is_marked_from_its_number() {
        let disks = mjolnir_storage::disks::enumerate_disks();
        if let Some(disk) = disks.first() {
            let marked = describe_target(disk, &[disk.number]);
            assert!(marked.holds_the_backup);
            let unmarked = describe_target(disk, &[disk.number + 100]);
            assert!(!unmarked.holds_the_backup);
        }
    }
}
