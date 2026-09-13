//! Restoring a MjolnirVSS backup onto a replacement disk.
//!
//! This is the half of the product that destroys data, so it is deliberately
//! kept apart from everything else: it has no dependency on the shadow copy
//! code, it never runs as part of a backup, and the function that writes to a
//! disk cannot be called without a confirmation value that only exists once the
//! operator has typed the target disk's serial number.
//!
//! It is written against the [`mjolnir_core::blockio::BlockSink`] trait rather
//! than against a Windows disk handle, so the whole destructive path is
//! exercised by ordinary tests writing to a temporary file.
//!
//! Copyright (C) the MjolnirVSS contributors.
//! Licensed under the GNU General Public License, version 3 or later.

#![warn(missing_docs)]

pub mod boot;
pub mod run;
pub mod target;

#[cfg(windows)]
pub mod windows_boot;
pub mod windows_target;

pub use boot::{decide as decide_boot_repair, BootDecision, BootRepairReport, BootState};
pub use run::{
    build_partition_table, check_chunks_present, plan, restore, stages, PlannedWrite,
    RestoreOutcome, RestorePlan,
};
pub use target::{check_target, EraseConfirmation, TargetDisk};

#[cfg(windows)]
pub use windows_boot::{repair_disk, RestoredVolumes};
pub use windows_target::{describe_target, enumerate_targets, WritableDisk};
