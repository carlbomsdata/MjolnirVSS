//! Windows storage discovery, plus the pure byte handling that goes with it.
//!
//! Everything that needs Windows lives behind `#[cfg(windows)]`. The GUID
//! partition table code does not, so it can be exercised against synthetic disk
//! images on any host and by tests that never touch real hardware.
//!
//! Nothing in this crate opens a device for writing. The restore side has its
//! own module for that, so no read only path can acquire write access by
//! accident.
//!
//! Copyright (C) the MjolnirVSS contributors.
//! Licensed under the GNU General Public License, version 3 or later.

#![warn(missing_docs)]

pub mod gpt;

#[cfg(windows)]
pub mod allocation;
#[cfg(windows)]
pub mod bitlocker;
#[cfg(windows)]
pub mod device;
#[cfg(windows)]
pub mod disks;
#[cfg(windows)]
pub mod system;
#[cfg(windows)]
pub mod volumes;
#[cfg(windows)]
pub mod wmi;

pub use gpt::{GptHeader, GptPartitionEntry, ParsedGpt};

#[cfg(windows)]
pub use allocation::read_allocation;
#[cfg(windows)]
pub use bitlocker::{inspect as inspect_encryption, Encryption, PartitionEncryption};
#[cfg(windows)]
pub use device::Device;
#[cfg(windows)]
pub use disks::{describe_disk, enumerate_disks, PhysicalDisk, PhysicalPartition};
#[cfg(windows)]
pub use system::{describe_system, SystemSummary};
#[cfg(windows)]
pub use volumes::{enumerate_volumes, VolumeExtent, VolumeInfo};
#[cfg(windows)]
pub use wmi::{encryptable_volume, encryptable_volumes, EncryptableVolume};
