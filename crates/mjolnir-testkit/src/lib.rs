//! Fixtures for testing MjolnirVSS without a real disk.
//!
//! Everything the backup and restore engines read and write goes through the
//! [`BlockSource`] and [`BlockSink`] traits, so a file or a buffer stands in
//! for a disk perfectly. That is what lets the dangerous half of this product
//! be tested properly: a restore that would destroy a computer can be run
//! against a temporary file a hundred times.
//!
//! The synthetic disks here are real GPT disks, byte for byte. They carry a
//! protective master boot record, a primary and secondary GPT with correct
//! checksums, and partitions whose contents are reproducible. A restore that
//! works against one of these is doing genuine work.
//!
//! Copyright (C) the MjolnirVSS contributors.
//! Licensed under the GNU General Public License, version 3 or later.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod corrupt;
pub mod disk;
pub mod file_device;
pub mod ntfs;
pub mod ntfs_volume;

pub use disk::{SyntheticDisk, SyntheticPartition};
pub use file_device::FileBlockDevice;
pub use ntfs::SyntheticNtfs;
pub use ntfs_volume::{NtfsVolumeBuilder, PlannedFile};
