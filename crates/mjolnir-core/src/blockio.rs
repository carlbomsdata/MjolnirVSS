//! Block level reading and writing, abstracted away from Windows.
//!
//! The backup and restore engines are written against these two traits. That
//! keeps the interesting logic, which decides what to copy and where it lands,
//! testable against a file or a memory buffer instead of against a physical
//! disk. Nothing in MjolnirVSS writes to a real disk during development.

use crate::error::{Error, Result};
use crate::exit::ExitCode;
use crate::math::ensure_within;

/// A readable block device or image.
pub trait BlockSource {
    /// A short description used in errors, such as a device path.
    fn describe(&self) -> String;

    /// Total readable size in bytes.
    fn size_bytes(&self) -> u64;

    /// The device's logical sector size.
    fn logical_sector_size(&self) -> u32;

    /// Fills `buf` completely from `offset`, or fails.
    ///
    /// A short read is an error: a partially filled buffer would silently put
    /// stale bytes into a backup.
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Checks that `[offset, offset + len)` is inside the device.
    fn ensure_readable(&self, offset: u64, len: u64) -> Result<()> {
        ensure_within("read range", offset, len, self.size_bytes()).map_err(|e| {
            Error::new(
                ExitCode::Io,
                format!("a read past the end of {} was requested: {e}", self.describe()),
                "reading outside the device would return unrelated data or fail, and either way the backup would not match the source",
                "this indicates an inconsistent disk layout; run `MjolnirVSS.exe inspect` and report the output",
            )
        })
    }
}

/// A writable block device or image.
pub trait BlockSink {
    /// A short description used in errors, such as a device path.
    fn describe(&self) -> String;

    /// Total writable size in bytes.
    fn size_bytes(&self) -> u64;

    /// The device's logical sector size.
    fn logical_sector_size(&self) -> u32;

    /// Writes all of `buf` at `offset`, or fails.
    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<()>;

    /// Pushes everything written so far to stable storage.
    ///
    /// Called before a restore is reported as successful. Without it the
    /// operator could reboot into a half written disk.
    fn flush_device(&mut self) -> Result<()>;

    /// Checks that `[offset, offset + len)` is inside the device.
    fn ensure_writable(&self, offset: u64, len: u64) -> Result<()> {
        ensure_within("write range", offset, len, self.size_bytes()).map_err(|e| {
            Error::new(
                ExitCode::UnsafeTarget,
                format!("a write past the end of {} was requested: {e}", self.describe()),
                "writing outside the target device would fail or corrupt an adjacent structure, so the restore is stopped before any of it happens",
                "choose a target disk at least as large as the source disk recorded in the backup manifest",
            )
        })
    }
}

/// An in memory block device, used by tests.
#[derive(Debug, Clone)]
pub struct MemoryBlockDevice {
    name: String,
    data: Vec<u8>,
    sector_size: u32,
}

impl MemoryBlockDevice {
    /// Creates a zero filled device of `size` bytes.
    pub fn zeroed(name: impl Into<String>, size: usize, sector_size: u32) -> Self {
        Self {
            name: name.into(),
            data: vec![0u8; size],
            sector_size,
        }
    }

    /// Creates a device holding `data`.
    pub fn from_vec(name: impl Into<String>, data: Vec<u8>, sector_size: u32) -> Self {
        Self {
            name: name.into(),
            data,
            sector_size,
        }
    }

    /// The bytes behind the device.
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// The bytes behind the device, mutably.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

impl BlockSource for MemoryBlockDevice {
    fn describe(&self) -> String {
        self.name.clone()
    }

    fn size_bytes(&self) -> u64 {
        self.data.len() as u64
    }

    fn logical_sector_size(&self) -> u32 {
        self.sector_size
    }

    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.ensure_readable(offset, buf.len() as u64)?;
        let start = offset as usize;
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        Ok(())
    }
}

impl BlockSink for MemoryBlockDevice {
    fn describe(&self) -> String {
        self.name.clone()
    }

    fn size_bytes(&self) -> u64 {
        self.data.len() as u64
    }

    fn logical_sector_size(&self) -> u32 {
        self.sector_size
    }

    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.ensure_writable(offset, buf.len() as u64)?;
        let start = offset as usize;
        self.data[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn flush_device(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_round_trip() {
        let mut dev = MemoryBlockDevice::zeroed("mem", 1024, 512);
        dev.write_all_at(100, b"hello").unwrap();
        let mut buf = [0u8; 5];
        dev.read_exact_at(100, &mut buf).unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn reading_past_the_end_is_refused() {
        let mut dev = MemoryBlockDevice::zeroed("mem", 16, 512);
        let mut buf = [0u8; 8];
        let err = dev.read_exact_at(10, &mut buf).unwrap_err();
        assert_eq!(err.exit(), ExitCode::Io);
        assert!(err.what().contains("mem"));
    }

    #[test]
    fn writing_past_the_end_is_refused_as_an_unsafe_target() {
        let mut dev = MemoryBlockDevice::zeroed("mem", 16, 512);
        let err = dev.write_all_at(12, b"12345678").unwrap_err();
        assert_eq!(err.exit(), ExitCode::UnsafeTarget);
    }

    #[test]
    fn an_overflowing_offset_is_refused_rather_than_wrapping() {
        let dev = MemoryBlockDevice::zeroed("mem", 16, 512);
        assert!(BlockSource::ensure_readable(&dev, u64::MAX, 8).is_err());
    }
}
