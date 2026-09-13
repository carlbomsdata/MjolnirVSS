//! Shared foundations for MjolnirVSS: errors, exit codes, identifiers,
//! checked arithmetic, cancellation and progress.
//!
//! MjolnirVSS is a portable Windows bare metal backup and recovery tool. This
//! crate deliberately has no Windows dependency so that the format and policy
//! layers above it can be unit tested on any host.
//!
//! Copyright (C) the MjolnirVSS contributors.
//! Licensed under the GNU General Public License, version 3 or later.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod blockio;
pub mod cancel;
pub mod error;
pub mod exit;
pub mod extents;
pub mod ids;
pub mod math;
pub mod progress;
pub mod timestamp;

pub use blockio::{BlockSink, BlockSource};
pub use cancel::CancelToken;
pub use error::{Error, Result};
pub use exit::ExitCode;
pub use extents::{ByteRange, ExtentList};
pub use ids::{BackupId, BackupName, DiskId, MachineId, PartitionId, StreamId, VolumeId};
pub use timestamp::UtcTimestamp;

/// The product name, used in output, manifests and logs.
pub const PRODUCT_NAME: &str = "MjolnirVSS";

/// The version of the MjolnirVSS tools, taken from the crate version.
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_name_is_spelled_consistently() {
        assert_eq!(PRODUCT_NAME, "MjolnirVSS");
    }

    #[test]
    fn tool_version_looks_like_a_version() {
        // It is written into every manifest, so a reader on another machine
        // has to be able to make sense of it. A pre release suffix is allowed
        // and is part of saying honestly what a build is: `0.1.0-alpha.1` is a
        // more useful thing to find in a manifest than `0.1.0`.
        let (numbers, pre) = match TOOL_VERSION.split_once('-') {
            Some((numbers, pre)) => (numbers, Some(pre)),
            None => (TOOL_VERSION, None),
        };

        let parts: Vec<&str> = numbers.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "expected major.minor.patch, got {TOOL_VERSION:?}"
        );
        for part in parts {
            assert!(
                !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()),
                "{TOOL_VERSION:?} has a non numeric component"
            );
        }

        if let Some(pre) = pre {
            assert!(
                !pre.is_empty()
                    && pre
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'),
                "{TOOL_VERSION:?} has a pre release part that is not usable: {pre:?}"
            );
        }
    }
}
