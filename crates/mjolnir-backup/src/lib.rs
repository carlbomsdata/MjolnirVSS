//! Planning and running a MjolnirVSS backup.
//!
//! The engine knows nothing about how it is being driven. It takes a plan, a
//! progress sink and a cancellation flag, and it works. That is what lets the
//! same code sit behind the graphical interface and behind the command line
//! used for testing, with no branching between them.
//!
//! Copyright (C) the MjolnirVSS contributors.
//! Licensed under the GNU General Public License, version 3 or later.

#![warn(missing_docs)]

pub mod capture;
pub mod diagnose;
pub mod log;
pub mod plan;
pub mod run;

pub use capture::{capture_disk, CaptureSources, CaptureSpec, PartitionCapture};
pub use diagnose::{diagnose_system_volume, Conclusion, Diagnosis};
pub use log::RunLog;
pub use plan::{plan, BackupPlan, BackupRequest, BackupScope, CaptureLimit, PlannedPartition};
pub use run::{run, stages, BackupOutcome};
