//! `indexes/volume-<id>.json`: where the files inside a captured volume are.
//!
//! File recovery needs to answer "show me the folders on the Windows volume"
//! without reading two hundred gigabytes of chunks. The index records, per
//! file, the position of its data inside the captured stream, so opening a
//! folder costs a JSON parse and extracting a file costs only the chunks that
//! actually hold it.
//!
//! The index is an accelerator, never an authority. Extraction always verifies
//! the chunks it reads against the manifest, so a damaged or forged index
//! cannot make MjolnirVSS hand back the wrong bytes as though they were right.

use std::collections::BTreeSet;

use mjolnir_core::ids::VolumeId;
use mjolnir_core::math;
use serde::{Deserialize, Serialize};

use crate::issue::Issue;
use crate::version::{DocumentKind, FormatHeader};

/// What an entry in the index is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    /// A directory.
    Directory,
    /// A regular file.
    File,
}

/// One run of bytes belonging to a file, located inside the volume stream.
///
/// A file on NTFS is rarely contiguous, so a file is described by a list of
/// these. `stream_offset` is measured from the start of the captured volume,
/// which is the same coordinate space the manifest's segments use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataRun {
    /// Offset of this run within the file's own contents.
    pub file_offset: u64,
    /// Offset of this run within the captured volume stream.
    pub stream_offset: u64,
    /// Length of the run in bytes.
    pub length: u64,
}

/// One file or directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// Position of this entry in the index, used as its identifier.
    pub id: u32,
    /// The directory this entry sits in. The root is its own parent.
    pub parent: u32,
    /// Name within the parent directory, never a path.
    pub name: String,
    /// File or directory.
    pub kind: EntryKind,
    /// Logical size of the file in bytes. Zero for a directory.
    #[serde(default)]
    pub size: u64,
    /// Last write time in RFC 3339 UTC, when it was recorded.
    #[serde(default)]
    pub modified_utc: Option<String>,
    /// Where the contents live inside the captured stream.
    ///
    /// Empty for a directory, for an empty file, and for a small file whose
    /// contents NTFS stored inside its own record, which this version does not
    /// extract.
    #[serde(default)]
    pub runs: Vec<DataRun>,
}

impl IndexEntry {
    /// Total number of bytes covered by the entry's runs.
    pub fn covered_bytes(&self) -> Result<u64, math::ArithError> {
        let mut total = 0u64;
        for r in &self.runs {
            total = math::add_u64("index entry coverage", total, r.length)?;
        }
        Ok(total)
    }
}

/// `indexes/volume-<id>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeIndex {
    /// Format identity.
    pub format: FormatHeader,
    /// The backup this index belongs to.
    pub backup_uuid: String,
    /// The volume it describes.
    pub volume_id: VolumeId,
    /// Length of the captured stream, used to bounds check every run.
    pub stream_length: u64,
    /// Whether the index covers the whole volume.
    ///
    /// False when indexing stopped early, for example because the filesystem
    /// held more entries than the limit. A partial index is still useful, and
    /// saying so is better than pretending a missing file does not exist.
    pub complete: bool,
    /// The entries. Entry 0 is the root directory.
    pub entries: Vec<IndexEntry>,
}

impl VolumeIndex {
    /// Builds an index holding only a root directory.
    pub fn new(backup_uuid: impl Into<String>, volume_id: VolumeId, stream_length: u64) -> Self {
        Self {
            format: FormatHeader::current(DocumentKind::VolumeIndex),
            backup_uuid: backup_uuid.into(),
            volume_id,
            stream_length,
            complete: true,
            entries: vec![IndexEntry {
                id: 0,
                parent: 0,
                name: String::new(),
                kind: EntryKind::Directory,
                size: 0,
                modified_utc: None,
                runs: Vec::new(),
            }],
        }
    }

    /// The entries directly inside `parent`, excluding the root's self link.
    pub fn children(&self, parent: u32) -> impl Iterator<Item = &IndexEntry> {
        self.entries
            .iter()
            .filter(move |e| e.parent == parent && e.id != parent)
    }

    /// Looks up an entry by identifier.
    pub fn entry(&self, id: u32) -> Option<&IndexEntry> {
        self.entries.get(id as usize).filter(|e| e.id == id)
    }

    /// Builds the full path of an entry, using backslashes.
    ///
    /// Returns `None` if the parent chain is broken or loops, which is what a
    /// forged index would look like.
    pub fn path_of(&self, id: u32) -> Option<String> {
        let mut parts: Vec<&str> = Vec::new();
        let mut current = id;
        let mut seen: BTreeSet<u32> = BTreeSet::new();
        loop {
            if !seen.insert(current) {
                return None; // a loop in the parent chain
            }
            let entry = self.entry(current)?;
            if entry.parent == entry.id {
                break; // reached the root
            }
            parts.push(&entry.name);
            current = entry.parent;
            if parts.len() > 4096 {
                return None;
            }
        }
        parts.reverse();
        Some(parts.join("\\"))
    }

    /// Checks the document.
    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = self.format.check(DocumentKind::VolumeIndex);
        let object = format!("index for volume {:?}", self.volume_id.as_str());

        if !crate::is_guid(&self.backup_uuid) {
            issues.push(Issue::error(
                &object,
                format!("backup_uuid {:?} is not a GUID", self.backup_uuid),
            ));
        }
        if self.entries.is_empty() {
            issues.push(Issue::error(&object, "holds no entries, not even a root"));
            return issues;
        }

        let root = &self.entries[0];
        if root.id != 0 || root.parent != 0 || root.kind != EntryKind::Directory {
            issues.push(Issue::error(
                &object,
                "entry 0 is not a root directory that is its own parent",
            ));
        }

        let count = self.entries.len() as u64;
        for (position, entry) in self.entries.iter().enumerate() {
            let e_object = format!("{object} entry {position}");
            if u64::from(entry.id) != position as u64 {
                issues.push(Issue::error(
                    &e_object,
                    format!("declares id {} but sits at position {position}", entry.id),
                ));
            }
            if u64::from(entry.parent) >= count {
                issues.push(Issue::error(
                    &e_object,
                    format!(
                        "names parent {} but there are only {count} entries",
                        entry.parent
                    ),
                ));
            }
            if position != 0 && entry.parent == entry.id {
                issues.push(Issue::error(
                    &e_object,
                    "is its own parent, which only the root may be",
                ));
            }
            // A name is joined into a path shown to the operator and used as an
            // extraction target, so separators and dot segments are refused.
            if position != 0 {
                if entry.name.is_empty() {
                    issues.push(Issue::error(&e_object, "has an empty name"));
                } else if entry.name.contains('\\')
                    || entry.name.contains('/')
                    || entry.name.contains(':')
                    || entry.name == "."
                    || entry.name == ".."
                {
                    issues.push(Issue::error(
                        &e_object,
                        format!("name {:?} is not a single path component", entry.name),
                    ));
                }
            }
            if entry.kind == EntryKind::Directory && !entry.runs.is_empty() {
                issues.push(Issue::error(
                    &e_object,
                    "is a directory but carries data runs",
                ));
            }

            let mut previous_end: Option<u64> = None;
            for (i, run) in entry.runs.iter().enumerate() {
                let r_object = format!("{e_object} run {i}");
                if run.length == 0 {
                    issues.push(Issue::error(&r_object, "length is zero"));
                    continue;
                }
                if let Err(e) = math::ensure_within(
                    "index run",
                    run.stream_offset,
                    run.length,
                    self.stream_length,
                ) {
                    issues.push(Issue::error(
                        &r_object,
                        format!("points outside the captured volume: {e}"),
                    ));
                }
                match math::range_end("index run end", run.file_offset, run.length) {
                    Ok(end) => {
                        if end > entry.size {
                            issues.push(Issue::error(
                                &r_object,
                                format!(
                                    "covers file bytes {}..{end} but the file is {} bytes",
                                    run.file_offset, entry.size
                                ),
                            ));
                        }
                        if let Some(prev) = previous_end {
                            if run.file_offset < prev {
                                issues.push(Issue::error(
                                    &r_object,
                                    "starts before the previous run ends; runs must be sorted and must not overlap",
                                ));
                            }
                        }
                        previous_end = Some(end);
                    }
                    Err(e) => issues.push(Issue::error(&r_object, format!("{e}"))),
                }
            }
        }
        issues
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "99999999-8888-7777-6666-555555555555";

    fn volume() -> VolumeId {
        VolumeId::new("volume-1").unwrap()
    }

    fn index_with(entries: Vec<IndexEntry>) -> VolumeIndex {
        let mut idx = VolumeIndex::new(UUID, volume(), 1 << 30);
        idx.entries.extend(entries);
        idx
    }

    fn dir(id: u32, parent: u32, name: &str) -> IndexEntry {
        IndexEntry {
            id,
            parent,
            name: name.to_owned(),
            kind: EntryKind::Directory,
            size: 0,
            modified_utc: None,
            runs: Vec::new(),
        }
    }

    fn file(id: u32, parent: u32, name: &str, size: u64, runs: Vec<DataRun>) -> IndexEntry {
        IndexEntry {
            id,
            parent,
            name: name.to_owned(),
            kind: EntryKind::File,
            size,
            modified_utc: None,
            runs,
        }
    }

    #[test]
    fn a_fresh_index_has_only_a_root() {
        let idx = VolumeIndex::new(UUID, volume(), 1024);
        assert!(idx.validate().is_empty(), "{:?}", idx.validate());
        assert_eq!(idx.children(0).count(), 0);
        assert_eq!(idx.path_of(0).unwrap(), "");
    }

    #[test]
    fn paths_are_built_from_the_parent_chain() {
        let idx = index_with(vec![
            dir(1, 0, "Users"),
            dir(2, 1, "tobias"),
            file(
                3,
                2,
                "notes.txt",
                10,
                vec![DataRun {
                    file_offset: 0,
                    stream_offset: 4096,
                    length: 10,
                }],
            ),
        ]);
        assert!(idx.validate().is_empty(), "{:?}", idx.validate());
        assert_eq!(idx.path_of(3).unwrap(), "Users\\tobias\\notes.txt");
        assert_eq!(idx.children(1).count(), 1);
    }

    #[test]
    fn a_run_pointing_outside_the_volume_is_refused() {
        let idx = index_with(vec![file(
            1,
            0,
            "big.bin",
            100,
            vec![DataRun {
                file_offset: 0,
                stream_offset: (1 << 30) - 10,
                length: 100,
            }],
        )]);
        assert!(idx
            .validate()
            .iter()
            .any(|i| i.problem.contains("outside the captured volume")));
    }

    #[test]
    fn a_run_longer_than_its_file_is_refused() {
        let idx = index_with(vec![file(
            1,
            0,
            "small.bin",
            10,
            vec![DataRun {
                file_offset: 0,
                stream_offset: 0,
                length: 4096,
            }],
        )]);
        assert!(idx
            .validate()
            .iter()
            .any(|i| i.problem.contains("but the file is 10 bytes")));
    }

    #[test]
    fn a_name_that_is_really_a_path_is_refused() {
        for hostile in ["..", ".", "a\\b", "a/b", "C:", ""] {
            let idx = index_with(vec![file(1, 0, hostile, 0, vec![])]);
            assert!(
                idx.validate().iter().any(|i| i.is_error()),
                "{hostile:?} was accepted"
            );
        }
    }

    #[test]
    fn a_loop_in_the_parent_chain_does_not_hang() {
        let mut idx = index_with(vec![dir(1, 2, "a"), dir(2, 1, "b")]);
        idx.entries[1].parent = 2;
        idx.entries[2].parent = 1;
        assert_eq!(idx.path_of(1), None);
        assert_eq!(idx.path_of(2), None);
    }

    #[test]
    fn a_parent_that_does_not_exist_is_refused() {
        let idx = index_with(vec![dir(1, 99, "orphan")]);
        assert!(idx
            .validate()
            .iter()
            .any(|i| i.problem.contains("but there are only")));
    }

    #[test]
    fn out_of_order_runs_are_refused() {
        let idx = index_with(vec![file(
            1,
            0,
            "frag.bin",
            200,
            vec![
                DataRun {
                    file_offset: 100,
                    stream_offset: 0,
                    length: 100,
                },
                DataRun {
                    file_offset: 0,
                    stream_offset: 4096,
                    length: 100,
                },
            ],
        )]);
        assert!(idx
            .validate()
            .iter()
            .any(|i| i.problem.contains("starts before the previous run ends")));
    }

    #[test]
    fn a_directory_carrying_data_is_refused() {
        let mut e = dir(1, 0, "Windows");
        e.runs.push(DataRun {
            file_offset: 0,
            stream_offset: 0,
            length: 10,
        });
        let idx = index_with(vec![e]);
        assert!(idx
            .validate()
            .iter()
            .any(|i| i.problem.contains("directory but carries data runs")));
    }

    #[test]
    fn covered_bytes_adds_up_the_runs() {
        let e = file(
            1,
            0,
            "f",
            300,
            vec![
                DataRun {
                    file_offset: 0,
                    stream_offset: 0,
                    length: 100,
                },
                DataRun {
                    file_offset: 100,
                    stream_offset: 8192,
                    length: 200,
                },
            ],
        );
        assert_eq!(e.covered_bytes().unwrap(), 300);
    }

    #[test]
    fn round_trips_through_json() {
        let idx = index_with(vec![dir(1, 0, "Users")]);
        let json = serde_json::to_string(&idx).unwrap();
        let back: VolumeIndex = serde_json::from_str(&json).unwrap();
        assert_eq!(back, idx);
    }
}
