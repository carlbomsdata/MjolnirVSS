//! The MjolnirVSS image format: documents, chunk store, writing and
//! verification.
//!
//! A backup is a plain folder that a person can open, copy and inspect. There
//! is no database and no index that has to be rebuilt: three JSON documents
//! describe what was captured, and a directory of immutable content addressed
//! chunks holds the bytes.
//!
//! ```text
//! DESKTOP-1A2B_2026-09-12_1015/
//!   manifest.json      what was captured, how it was cut up, the chunk table
//!   disk-layout.json   the physical disks and their partitions
//!   completion.json    written last; without it the backup is incomplete
//!   logs/backup.log
//!   indexes/volume-<id>.json
//!   chunks/<blake3>.zst
//! ```
//!
//! Everything in this crate treats its input as hostile. A manifest can arrive
//! from a drive that has been sitting in a drawer, or from a file someone
//! edited, and its numbers are about to be turned into writes against a
//! replacement disk. Offsets are bounds checked, paths are validated before
//! they are joined, and decompression is bounded by the size the manifest
//! declares.
//!
//! Copyright (C) the MjolnirVSS contributors.
//! Licensed under the GNU General Public License, version 3 or later.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod completion;
pub mod disk_layout;
pub mod hash;
pub mod index;
pub mod issue;
pub mod layout;
pub mod manifest;
pub mod set;
pub mod store;
pub mod verify;
pub mod version;
pub mod writer;

pub use completion::{Completion, CompletionState, Verification, VerificationResult};
pub use disk_layout::{DiskEntry, DiskLayout, PartitionEntry, PartitionRole};
pub use hash::ChunkHash;
pub use issue::{Issue, IssueList, Severity};
pub use layout::BackupLayout;
pub use manifest::{Manifest, Segment, Stream, StreamKind};
pub use set::BackupSet;
pub use store::ChunkStore;
pub use version::{DocumentKind, FormatHeader, FORMAT_MAGIC, FORMAT_MAJOR, FORMAT_MINOR};
pub use writer::BackupWriter;

/// Whether `text` is a GUID in the canonical braceless form.
///
/// Windows hands GUIDs back in upper case and the format writes them in lower
/// case, so the check is case insensitive. Braces are rejected: the format
/// stores one spelling so two documents never disagree about the same disk.
pub fn is_guid(text: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut parts = text.split('-');
    for expected in GROUPS {
        let Some(part) = parts.next() else {
            return false;
        };
        if part.len() != expected || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
    }
    parts.next().is_none()
}

/// Formats 16 raw GUID bytes, laid out as Windows stores them, in the canonical
/// lower case form.
///
/// The first three fields are little endian and the last two are big endian,
/// which is why this cannot be a plain hex dump.
pub fn format_guid(bytes: &[u8; 16]) -> String {
    let d1 = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let d2 = u16::from_le_bytes([bytes[4], bytes[5]]);
    let d3 = u16::from_le_bytes([bytes[6], bytes[7]]);
    format!(
        "{d1:08x}-{d2:04x}-{d3:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_recogniser_is_strict() {
        assert!(is_guid("c12a7328-f81f-11d2-ba4b-00a0c93ec93b"));
        assert!(is_guid("C12A7328-F81F-11D2-BA4B-00A0C93EC93B"));
        assert!(!is_guid("{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}"));
        assert!(!is_guid("c12a7328-f81f-11d2-ba4b-00a0c93ec93"));
        assert!(!is_guid("c12a7328f81f11d2ba4b00a0c93ec93b"));
        assert!(!is_guid("c12a7328-f81f-11d2-ba4b-00a0c93ec93b-extra"));
        assert!(!is_guid(""));
        assert!(!is_guid("zzzzzzzz-f81f-11d2-ba4b-00a0c93ec93b"));
    }

    #[test]
    fn guid_bytes_format_with_the_right_endianness() {
        // The EFI system partition type GUID as Windows lays it out in memory.
        let bytes: [u8; 16] = [
            0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e,
            0xc9, 0x3b,
        ];
        assert_eq!(format_guid(&bytes), disk_layout::GUID_EFI_SYSTEM);
        assert!(is_guid(&format_guid(&bytes)));
    }

    #[test]
    fn an_all_zero_guid_still_formats_validly() {
        assert_eq!(
            format_guid(&[0u8; 16]),
            "00000000-0000-0000-0000-000000000000"
        );
        assert!(is_guid(&format_guid(&[0u8; 16])));
    }

    #[test]
    fn every_byte_pattern_formats_to_something_valid() {
        for seed in 0u8..=255 {
            let bytes = [seed; 16];
            assert!(
                is_guid(&format_guid(&bytes)),
                "seed {seed} produced an invalid GUID"
            );
        }
    }
}
