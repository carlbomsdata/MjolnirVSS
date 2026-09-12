//! Reading NTFS structures.
//!
//! MjolnirVSS needs three things from NTFS, and none of them require a
//! filesystem driver:
//!
//! * the boot sector, which says how big a cluster is and where the master file
//!   table lives;
//! * the allocation bitmap, which says which clusters are in use, so a backup
//!   can skip the free space;
//! * the master file table, so files can be listed and extracted from a backup
//!   without restoring a whole disk.
//!
//! Everything here reads through [`mjolnir_core::blockio::BlockSource`], so it
//! works against a shadow copy device, a file, or a buffer in memory, and can
//! be tested without Windows.
//!
//! Copyright (C) the MjolnirVSS contributors.
//! Licensed under the GNU General Public License, version 3 or later.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod boot;

pub use boot::{NtfsBootSector, VolumeSignature};
