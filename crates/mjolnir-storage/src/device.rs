//! Opening and reading Windows block devices.
//!
//! Three kinds of path go through here and behave the same way: a physical disk
//! (`\\.\PhysicalDrive0`), a volume (`\\?\Volume{...}`) and a shadow copy
//! device (`\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy3`).
//!
//! Reads against a device have to start and end on a sector boundary, which the
//! callers of this module should not have to think about. [`Device`] takes any
//! offset and length, reads the enclosing aligned window, and hands back the
//! bytes that were asked for.
//!
//! Nothing here ever opens a device for writing. The restore side has its own
//! module for that, so a read only path cannot acquire write access by mistake.

use std::ffi::c_void;

use mjolnir_core::blockio::BlockSource;
use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::math;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::{DeviceIoControl, OVERLAPPED};

/// Largest single read issued to a device.
///
/// Reads are broken into pieces of at most this size so that a caller asking
/// for an enormous range does not make the driver allocate one enormous
/// transfer, and so cancellation is noticed promptly.
pub const MAX_TRANSFER: usize = 8 * 1024 * 1024;

/// Converts a Rust string into a null terminated wide string.
pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reads a null terminated wide string out of a buffer.
pub(crate) fn wide_to_string(buffer: &[u16]) -> String {
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

/// An owned Windows handle that closes itself.
#[derive(Debug)]
pub struct OwnedHandle {
    handle: HANDLE,
}

impl OwnedHandle {
    /// The raw handle. Valid for as long as this wrapper lives.
    pub fn raw(&self) -> HANDLE {
        self.handle
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // SAFETY: the handle came from CreateFileW, is owned solely by this
            // wrapper, has not been closed, and is not used again afterwards.
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

// A handle is just a kernel object identifier; moving it between threads is
// what lets the backup run on a worker thread while the window stays live.
unsafe impl Send for OwnedHandle {}

/// How much access to ask Windows for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceAccess {
    /// No access rights at all.
    ///
    /// Enough for the informational control codes, and it succeeds without
    /// administrator rights, which is what lets the main window list disks
    /// before the user has approved anything.
    QueryOnly,
    /// Read access, needed to copy data. Requires administrator rights for a
    /// physical disk or a volume.
    Read,
}

/// An open block device.
#[derive(Debug)]
pub struct Device {
    handle: OwnedHandle,
    path: String,
    sector_size: u32,
    size_bytes: u64,
}

impl Device {
    /// Opens a device without asking for any access rights.
    pub fn query(path: &str) -> Result<Self> {
        Self::open(path, DeviceAccess::QueryOnly, 512, 0)
    }

    /// Opens a device for reading.
    ///
    /// `sector_size` and `size_bytes` come from whoever already queried the
    /// geometry. Passing zero for the size means "unknown", and bounds checks
    /// against the end of the device are then left to Windows.
    pub fn open_read(path: &str, sector_size: u32, size_bytes: u64) -> Result<Self> {
        Self::open(path, DeviceAccess::Read, sector_size, size_bytes)
    }

    fn open(path: &str, access: DeviceAccess, sector_size: u32, size_bytes: u64) -> Result<Self> {
        let wide_path = wide(path);
        let desired = match access {
            DeviceAccess::QueryOnly => 0u32,
            DeviceAccess::Read => FILE_GENERIC_READ.0,
        };

        // Sharing both read and write is required: the volumes being copied are
        // the ones Windows is running from, and refusing to share would fail
        // immediately.
        //
        // SAFETY: the path is a null terminated wide string that outlives the
        // call, and the optional arguments are all None.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide_path.as_ptr()),
                desired,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(FILE_ATTRIBUTE_NORMAL.0),
                None,
            )
        }
        .map_err(|e| open_error(path, access, e))?;

        Ok(Self {
            handle: OwnedHandle { handle },
            path: path.to_owned(),
            sector_size: if sector_size == 0 { 512 } else { sector_size },
            size_bytes,
        })
    }

    /// The path this device was opened with.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The raw handle, for control codes issued by other modules.
    pub fn handle(&self) -> HANDLE {
        self.handle.raw()
    }

    /// Records the geometry once it is known.
    pub fn set_geometry(&mut self, sector_size: u32, size_bytes: u64) {
        if sector_size != 0 {
            self.sector_size = sector_size;
        }
        self.size_bytes = size_bytes;
    }

    /// Issues a control code that takes no input and fills `output`.
    ///
    /// Returns the number of bytes written into `output`.
    pub fn control(&self, code: u32, output: &mut [u8]) -> Result<u32> {
        self.control_with(code, &[], output)
    }

    /// Issues a control code with both input and output buffers.
    pub fn control_with(&self, code: u32, input: &[u8], output: &mut [u8]) -> Result<u32> {
        let mut returned = 0u32;
        let input_ptr = if input.is_empty() {
            None
        } else {
            Some(input.as_ptr() as *const c_void)
        };
        let output_ptr = if output.is_empty() {
            None
        } else {
            Some(output.as_mut_ptr() as *mut c_void)
        };

        // SAFETY: both buffers are valid for the lengths passed alongside them
        // and live for the duration of the call. The handle is open. No
        // OVERLAPPED is supplied, so the call is synchronous.
        unsafe {
            DeviceIoControl(
                self.handle.raw(),
                code,
                input_ptr,
                input.len() as u32,
                output_ptr,
                output.len() as u32,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|e| {
            Error::new(
                ExitCode::Io,
                format!("a device query against {} failed", self.path),
                format!("Windows rejected control code {code:#010x}: {e}"),
                "check that the drive is still connected; if this is a physical disk, MjolnirVSS must be running as administrator",
            )
        })?;

        Ok(returned)
    }

    /// Issues a control code, returning `None` when Windows says the device
    /// does not support it.
    ///
    /// Several descriptors are optional: a USB enclosure may not report a
    /// serial number, and a virtual disk may not report alignment. Those are
    /// facts about the device, not failures.
    pub fn control_optional(&self, code: u32, output: &mut [u8]) -> Option<u32> {
        self.control(code, output).ok()
    }

    /// Reads exactly `buf.len()` bytes starting at `offset`.
    ///
    /// Handles sector alignment internally, so `offset` and the length may be
    /// anything.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let sector = u64::from(self.sector_size);
        let aligned_start = offset - (offset % sector);
        let end = math::add_u64("device read end", offset, buf.len() as u64)?;
        let aligned_end = math::round_up("device read end", end, sector)?;
        let span = math::to_usize("device read length", aligned_end - aligned_start)?;

        if aligned_start == offset && span == buf.len() {
            // Already aligned, so read straight into the caller's buffer.
            return self.read_raw(offset, buf);
        }

        let mut scratch = vec![0u8; span];
        self.read_raw(aligned_start, &mut scratch)?;
        let skip = math::to_usize("device read offset", offset - aligned_start)?;
        buf.copy_from_slice(&scratch[skip..skip + buf.len()]);
        Ok(())
    }

    /// Reads at a sector aligned offset into a sector sized multiple buffer.
    fn read_raw(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let want = (buf.len() - done).min(MAX_TRANSFER);
            let at = math::add_u64("device read offset", offset, done as u64)?;

            let mut overlapped = OVERLAPPED::default();
            // A positioned read. The handle was not opened for overlapped I/O,
            // so the call is synchronous and the structure only carries the
            // offset.
            overlapped.Anonymous.Anonymous.Offset = (at & 0xFFFF_FFFF) as u32;
            overlapped.Anonymous.Anonymous.OffsetHigh = (at >> 32) as u32;

            let mut read = 0u32;
            // SAFETY: the slice is valid for `want` bytes, the OVERLAPPED lives
            // until the synchronous call returns, and the handle is open.
            let result = unsafe {
                ReadFile(
                    self.handle.raw(),
                    Some(&mut buf[done..done + want]),
                    Some(&mut read),
                    Some(&mut overlapped),
                )
            };
            result.map_err(|e| {
                Error::new(
                    ExitCode::Io,
                    format!("reading {} failed at offset {at}", self.path),
                    format!("Windows reported: {e}"),
                    "check that the drive is still connected and healthy; if this is an external drive, try a different cable or port",
                )
            })?;

            if read == 0 {
                return Err(Error::new(
                    ExitCode::Io,
                    format!("reading {} stopped early at offset {at}", self.path),
                    format!(
                        "{} bytes were requested but the device returned nothing, which means the read went past the end of it",
                        want
                    ),
                    "this indicates an inconsistent disk layout; run MjolnirVSS inspect and report the output",
                ));
            }
            done += read as usize;
        }
        Ok(())
    }
}

impl BlockSource for Device {
    fn describe(&self) -> String {
        self.path.clone()
    }

    fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    fn logical_sector_size(&self) -> u32 {
        self.sector_size
    }

    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if self.size_bytes > 0 {
            self.ensure_readable(offset, buf.len() as u64)?;
        }
        self.read_at(offset, buf)
    }
}

fn open_error(path: &str, access: DeviceAccess, e: windows::core::Error) -> Error {
    const ERROR_ACCESS_DENIED: i32 = 0x8007_0005u32 as i32;
    const ERROR_FILE_NOT_FOUND: i32 = 0x8007_0002u32 as i32;
    const ERROR_PATH_NOT_FOUND: i32 = 0x8007_0003u32 as i32;
    const ERROR_SHARING_VIOLATION: i32 = 0x8007_0020u32 as i32;

    match e.code().0 {
        ERROR_ACCESS_DENIED => Error::new(
            ExitCode::AccessDenied,
            format!("{path} could not be opened"),
            "Windows only lets a program read a disk or volume directly when it is running with administrator rights".to_owned(),
            "close MjolnirVSS and start it again, choosing Yes when Windows asks for permission",
        ),
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Error::new(
            ExitCode::Io,
            format!("{path} does not exist"),
            "the disk or volume was there a moment ago but Windows cannot find it now, which usually means it was unplugged".to_owned(),
            "reconnect the drive and try again",
        ),
        ERROR_SHARING_VIOLATION => Error::new(
            ExitCode::Io,
            format!("{path} is locked by another program"),
            "another program has opened the device in a way that does not allow MjolnirVSS to read it".to_owned(),
            "close any disk tools, antivirus scans or backup programs that may be using the drive, then try again",
        ),
        _ => Error::new(
            if access == DeviceAccess::Read {
                ExitCode::Io
            } else {
                ExitCode::Failure
            },
            format!("{path} could not be opened"),
            format!("Windows reported: {e}"),
            "check that the drive is connected, then try again",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_are_null_terminated() {
        assert_eq!(wide("AB"), vec![65, 66, 0]);
        assert_eq!(wide(""), vec![0]);
    }

    #[test]
    fn wide_to_string_stops_at_the_terminator() {
        let buffer: Vec<u16> = vec![67, 58, 92, 0, 88, 88];
        assert_eq!(wide_to_string(&buffer), "C:\\");
        assert_eq!(wide_to_string(&[]), "");
        // A buffer with no terminator must still produce something rather than
        // running off the end.
        assert_eq!(wide_to_string(&[65, 66]), "AB");
    }

    #[test]
    fn opening_a_device_that_does_not_exist_explains_itself() {
        let err = Device::query("\\\\.\\PhysicalDrive999").unwrap_err();
        assert!(!err.what().is_empty());
        assert!(!err.why().is_empty());
        assert!(!err.next_step().is_empty());
    }

    /// Checked at compile time rather than by a test, because both sides are
    /// constants and a runtime assertion on them is optimised away.
    const _: () = {
        assert!(
            MAX_TRANSFER >= 1024 * 1024,
            "the transfer cap is too small to be efficient"
        );
        assert!(
            MAX_TRANSFER <= 64 * 1024 * 1024,
            "the transfer cap would make one read allocate too much"
        );
        assert!(
            MAX_TRANSFER % 4096 == 0,
            "the transfer cap must be a whole number of sectors"
        );
    };
}
