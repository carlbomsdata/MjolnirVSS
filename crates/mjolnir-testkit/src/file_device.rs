//! A block device backed by an ordinary file.
//!
//! This is what stands in for a replacement disk during a restore test. It
//! implements both [`BlockSource`] and [`BlockSink`], enforces the same bounds
//! a real device does, and lives in a temporary folder, so a restore that would
//! wipe a computer can be run repeatedly and its result compared byte for byte
//! against what it should have produced.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use mjolnir_core::blockio::{BlockSink, BlockSource};
use mjolnir_core::error::{Error, Result};

/// A fixed size block device stored in a file.
#[derive(Debug)]
pub struct FileBlockDevice {
    file: File,
    path: PathBuf,
    size_bytes: u64,
    sector_size: u32,
    /// Counts writes, so a test can assert that a dry run wrote nothing.
    writes: u64,
}

impl FileBlockDevice {
    /// Creates a zero filled device of `size_bytes` at `path`.
    pub fn create(path: impl AsRef<Path>, size_bytes: u64, sector_size: u32) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| Error::io(path.display(), e))?;
        file.set_len(size_bytes)
            .map_err(|e| Error::io(path.display(), e))?;

        Ok(Self {
            file,
            path,
            size_bytes,
            sector_size,
            writes: 0,
        })
    }

    /// Opens an existing device file.
    pub fn open(path: impl AsRef<Path>, sector_size: u32) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| Error::io(path.display(), e))?;
        let size_bytes = file
            .metadata()
            .map_err(|e| Error::io(path.display(), e))?
            .len();

        Ok(Self {
            file,
            path,
            size_bytes,
            sector_size,
            writes: 0,
        })
    }

    /// Writes `bytes` as the whole contents of a new device.
    pub fn from_bytes(path: impl AsRef<Path>, bytes: &[u8], sector_size: u32) -> Result<Self> {
        let mut device = Self::create(path, bytes.len() as u64, sector_size)?;
        device.write_all_at(0, bytes)?;
        device.writes = 0;
        Ok(device)
    }

    /// The path of the backing file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many writes have been issued.
    pub fn write_count(&self) -> u64 {
        self.writes
    }

    /// Reads the whole device into memory, for comparison in a test.
    pub fn read_all(&mut self) -> Result<Vec<u8>> {
        let mut bytes = vec![0u8; self.size_bytes as usize];
        self.read_exact_at(0, &mut bytes)?;
        Ok(bytes)
    }

    fn seek_to(&mut self, offset: u64) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(offset))
            .map(|_| ())
            .map_err(|e| Error::io(self.path.display(), e))
    }
}

impl BlockSource for FileBlockDevice {
    fn describe(&self) -> String {
        self.path.display().to_string()
    }

    fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    fn logical_sector_size(&self) -> u32 {
        self.sector_size
    }

    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.ensure_readable(offset, buf.len() as u64)?;
        self.seek_to(offset)?;
        self.file
            .read_exact(buf)
            .map_err(|e| Error::io(self.path.display(), e))
    }
}

impl BlockSink for FileBlockDevice {
    fn describe(&self) -> String {
        self.path.display().to_string()
    }

    fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    fn logical_sector_size(&self) -> u32 {
        self.sector_size
    }

    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.ensure_writable(offset, buf.len() as u64)?;
        self.seek_to(offset)?;
        self.file
            .write_all(buf)
            .map_err(|e| Error::io(self.path.display(), e))?;
        self.writes += 1;
        Ok(())
    }

    fn flush_device(&mut self) -> Result<()> {
        self.file
            .sync_all()
            .map_err(|e| Error::io(self.path.display(), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_device_is_zero_filled_and_the_right_size() {
        let tmp = tempfile::tempdir().unwrap();
        let mut device = FileBlockDevice::create(tmp.path().join("disk.img"), 4096, 512).unwrap();
        assert_eq!(BlockSource::size_bytes(&device), 4096);
        assert_eq!(device.read_all().unwrap(), vec![0u8; 4096]);
    }

    #[test]
    fn writes_are_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let mut device = FileBlockDevice::create(tmp.path().join("disk.img"), 4096, 512).unwrap();
        device.write_all_at(1024, b"hello world").unwrap();
        device.flush_device().unwrap();

        let mut buf = [0u8; 11];
        device.read_exact_at(1024, &mut buf).unwrap();
        assert_eq!(&buf, b"hello world");
        assert_eq!(device.write_count(), 1);
    }

    #[test]
    fn reading_past_the_end_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut device = FileBlockDevice::create(tmp.path().join("disk.img"), 512, 512).unwrap();
        let mut buf = [0u8; 16];
        assert!(device.read_exact_at(500, &mut buf).is_err());
    }

    #[test]
    fn writing_past_the_end_is_refused_as_an_unsafe_target() {
        let tmp = tempfile::tempdir().unwrap();
        let mut device = FileBlockDevice::create(tmp.path().join("disk.img"), 512, 512).unwrap();
        let err = device.write_all_at(500, &[0u8; 16]).unwrap_err();
        assert_eq!(err.exit(), mjolnir_core::ExitCode::UnsafeTarget);
        // Nothing was written.
        assert_eq!(device.write_count(), 0);
    }

    #[test]
    fn a_device_built_from_bytes_matches_them() {
        let tmp = tempfile::tempdir().unwrap();
        let source = crate::SyntheticDisk::windows_like(512);
        let mut device =
            FileBlockDevice::from_bytes(tmp.path().join("disk.img"), &source.bytes, 512).unwrap();
        assert_eq!(device.read_all().unwrap(), source.bytes);
        // from_bytes resets the counter, so a later test can assert on writes.
        assert_eq!(device.write_count(), 0);
    }

    #[test]
    fn a_device_survives_being_closed_and_reopened() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("disk.img");
        {
            let mut device = FileBlockDevice::create(&path, 4096, 512).unwrap();
            device.write_all_at(0, b"persisted").unwrap();
            device.flush_device().unwrap();
        }
        let mut device = FileBlockDevice::open(&path, 512).unwrap();
        let mut buf = [0u8; 9];
        device.read_exact_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"persisted");
    }
}
