//! Getting individual files back out of a backup, without restoring a disk.
//!
//! This is the read only half of recovery. It opens a finished backup, reads
//! the NTFS volumes inside it, lists what is there, and copies chosen files out
//! to somewhere the operator picked.
//!
//! # It is deliberately separate from the restore engine
//!
//! Nothing in this crate can write to a disk, partition a disk, or open a
//! device. It depends on the backup reader and the NTFS reader and on nothing
//! that can destroy anything. That separation is the point: browsing a backup
//! should not be one mistyped argument away from erasing a drive.
//!
//! # The backup is never modified
//!
//! Every file in the backup is opened for reading. There is no code path here
//! that creates, truncates or writes a file inside a backup folder.
//!
//! # Every byte is checked on the way out
//!
//! Extraction reads through [`mjolnir_image::StreamReader`], which fetches
//! chunks through the chunk store, which decompresses each one and compares its
//! BLAKE3 digest before returning it. A damaged backup therefore stops an
//! extraction with a message naming the chunk, rather than writing a file that
//! is quietly wrong.
//!
//! Copyright (C) the MjolnirVSS contributors.
//! Licensed under the GNU General Public License, version 3 or later.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod extract;
pub mod safepath;

pub use extract::{extract_file, extract_tree, ExtractOptions, ExtractOutcome, Extracted};
pub use safepath::{safe_join, sanitise_component, PathRefusal};

use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_image::manifest::{Stream, StreamKind};
use mjolnir_image::{BackupSet, StreamReader};
use mjolnir_ntfs::volume::{FileIndex, Volume};

/// A partition inside a backup that holds a filesystem worth browsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowsableVolume {
    /// The stream carrying it.
    pub stream_id: String,
    /// Its partition number on the original disk.
    pub partition_number: u32,
    /// What the partition was for.
    pub role: String,
    /// The drive letter it had when the backup was taken, if any.
    pub drive_letter: Option<String>,
    /// Its label at that time.
    pub label: Option<String>,
    /// The filesystem Windows reported.
    pub filesystem: Option<String>,
    /// Size of the partition.
    pub size_bytes: u64,
    /// Whether MjolnirVSS can read inside it.
    pub is_readable: bool,
    /// Why not, when it cannot.
    pub why_not: Option<String>,
}

impl BrowsableVolume {
    /// How to describe it in a list.
    pub fn describe(&self) -> String {
        let letter = self
            .drive_letter
            .as_ref()
            .map(|l| format!("{l}: "))
            .unwrap_or_default();
        let label = self
            .label
            .as_ref()
            .filter(|l| !l.is_empty())
            .map(|l| format!(" \"{l}\""))
            .unwrap_or_default();
        format!(
            "Partition {} - {}{}{} - {}",
            self.partition_number,
            letter,
            self.role,
            label,
            mjolnir_core::progress::format_bytes(self.size_bytes)
        )
    }
}

/// Lists the partitions in a backup that could be browsed.
///
/// Everything is listed, including the ones that cannot be read, with the
/// reason. A recovery interface that silently omits a partition leaves somebody
/// wondering where their files went.
pub fn volumes_in(set: &BackupSet) -> Vec<BrowsableVolume> {
    let layout = set.disk_layout();
    let manifest = set.manifest();
    let mut out = Vec::new();

    for stream in &manifest.streams {
        if stream.kind != StreamKind::Partition {
            continue;
        }
        let partition = layout
            .disks
            .iter()
            .flat_map(|d| d.partitions.iter())
            .find(|p| Some(&p.id) == stream.partition_id.as_ref());

        let volume = manifest
            .volumes
            .iter()
            .find(|v| Some(&v.partition_id) == stream.partition_id.as_ref());

        let filesystem = volume
            .and_then(|v| v.filesystem.clone())
            .or_else(|| partition.and_then(|p| p.filesystem.clone()));

        let is_ntfs = filesystem
            .as_deref()
            .map(|f| f.eq_ignore_ascii_case("NTFS"))
            .unwrap_or(false);

        let why_not = if !is_ntfs {
            Some(format!(
                "MjolnirVSS can only look inside NTFS, and this partition holds {}",
                filesystem
                    .as_deref()
                    .unwrap_or("no filesystem it recognises")
            ))
        } else if !stream.capture.is_restorable() {
            Some(
                "this backup is a preview, so only the first part of the partition was captured"
                    .to_owned(),
            )
        } else {
            None
        };

        out.push(BrowsableVolume {
            stream_id: stream.id.as_str().to_owned(),
            partition_number: partition.map(|p| p.number).unwrap_or(0),
            role: partition
                .map(|p| p.role.describe().to_owned())
                .unwrap_or_else(|| "unknown".to_owned()),
            drive_letter: volume.and_then(|v| v.drive_letter.clone()),
            label: volume.and_then(|v| v.label.clone()),
            filesystem,
            size_bytes: stream.length,
            is_readable: why_not.is_none(),
            why_not,
        });
    }
    out
}

/// Finds a stream in a backup by its identifier.
pub fn stream_in<'a>(set: &'a BackupSet, stream_id: &str) -> Result<&'a Stream> {
    set.manifest()
        .streams
        .iter()
        .find(|s| s.id.as_str() == stream_id)
        .ok_or_else(|| {
            Error::new(
                ExitCode::Failure,
                "that partition is not in this backup",
                format!("no stream in the backup is called {stream_id}"),
                "run the list command to see what the backup holds",
            )
        })
}

/// An NTFS volume from a backup, opened and indexed.
pub struct OpenVolume<'a> {
    reader: StreamReader<'a>,
    index: FileIndex,
}

impl<'a> OpenVolume<'a> {
    /// Opens a partition from a backup and builds its file tree.
    ///
    /// Reads the whole master file table, which for a volume with a few hundred
    /// thousand files takes a few seconds.
    pub fn open(set: &'a BackupSet, stream: &'a Stream, cancel: &CancelToken) -> Result<Self> {
        let mut reader = StreamReader::new(set.manifest(), stream, set.chunk_store());
        let index = {
            let mut volume = Volume::open(&mut reader)?;
            FileIndex::build(&mut volume, cancel)?
        };
        Ok(Self { reader, index })
    }

    /// The file tree.
    pub fn index(&self) -> &FileIndex {
        &self.index
    }

    /// Runs something with the volume open for reading.
    ///
    /// The volume borrows the reader, so it cannot be kept alongside the index
    /// without a self referential structure. This hands it out for the length
    /// of one operation instead.
    pub fn with_volume<T>(&mut self, f: impl FnOnce(&mut Volume<'_>) -> Result<T>) -> Result<T> {
        let mut volume = Volume::open(&mut self.reader)?;
        f(&mut volume)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_volume_describes_itself_for_a_list() {
        let volume = BrowsableVolume {
            stream_id: "disk-0-part-3".to_owned(),
            partition_number: 3,
            role: "Windows".to_owned(),
            drive_letter: Some("C".to_owned()),
            label: Some("Windows".to_owned()),
            filesystem: Some("NTFS".to_owned()),
            size_bytes: 64 * 1024 * 1024 * 1024,
            is_readable: true,
            why_not: None,
        };
        let text = volume.describe();
        assert!(text.contains("Partition 3"));
        assert!(text.contains("C:"));
        assert!(text.contains("Windows"));
        assert!(text.contains("64.0 GiB"));
    }

    #[test]
    fn a_volume_without_a_letter_or_a_label_still_describes_itself() {
        let volume = BrowsableVolume {
            stream_id: "disk-0-part-1".to_owned(),
            partition_number: 1,
            role: "EFI System".to_owned(),
            drive_letter: None,
            label: None,
            filesystem: Some("FAT32".to_owned()),
            size_bytes: 100 * 1024 * 1024,
            is_readable: false,
            why_not: Some("not NTFS".to_owned()),
        };
        let text = volume.describe();
        assert!(text.contains("Partition 1"));
        assert!(!text.contains("\"\""));
        assert!(!text.contains(": :"));
    }
}
