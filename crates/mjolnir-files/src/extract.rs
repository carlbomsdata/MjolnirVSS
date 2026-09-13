//! Copying files out of a backup.
//!
//! Everything here writes only where the operator pointed it, and reads only
//! from the backup. A file whose contents this version cannot reproduce is
//! refused by name rather than written out wrong, which is the rule that makes
//! the result trustworthy: a file that came out is the file that went in.

use std::path::{Path, PathBuf};

use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::progress::Progress;
use mjolnir_ntfs::volume::{IndexEntry, Volume};

use crate::safepath::safe_join;
use crate::OpenVolume;

/// How much to read at a time.
const COPY_CHUNK: usize = 4 * 1024 * 1024;

/// What to do while extracting.
#[derive(Debug, Clone)]
pub struct ExtractOptions {
    /// Also write each file's alternate data streams, as `name:stream`.
    pub include_streams: bool,
    /// Follow a junction or symbolic link instead of skipping it.
    ///
    /// Off by default and deliberately so: a reparse point in a backup points
    /// at a path on the machine it came from, and following one while writing
    /// somewhere else is how an extraction ends up outside the chosen folder.
    pub follow_reparse_points: bool,
    /// Overwrite a file that is already there.
    pub overwrite: bool,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            include_streams: true,
            follow_reparse_points: false,
            overwrite: false,
        }
    }
}

/// One file that was written, or was not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extracted {
    /// Written.
    Written {
        /// Where it came from in the backup.
        source: String,
        /// Where it was written.
        target: PathBuf,
        /// How many bytes.
        bytes: u64,
    },
    /// Deliberately not written, with the reason.
    Skipped {
        /// Where it is in the backup.
        source: String,
        /// Why.
        reason: String,
    },
    /// Could not be written, with the reason.
    Failed {
        /// Where it is in the backup.
        source: String,
        /// Why.
        reason: String,
    },
}

impl Extracted {
    /// Whether this produced a file.
    pub fn wrote_something(&self) -> bool {
        matches!(self, Extracted::Written { .. })
    }

    /// A line for a report.
    pub fn describe(&self) -> String {
        match self {
            Extracted::Written {
                source,
                target,
                bytes,
            } => format!(
                "wrote {source} to {} ({})",
                target.display(),
                mjolnir_core::progress::format_bytes(*bytes)
            ),
            Extracted::Skipped { source, reason } => format!("skipped {source}: {reason}"),
            Extracted::Failed { source, reason } => format!("FAILED {source}: {reason}"),
        }
    }
}

/// What a whole extraction did.
#[derive(Debug, Clone, Default)]
pub struct ExtractOutcome {
    /// Every file, in the order they were reached.
    pub files: Vec<Extracted>,
    /// Bytes written.
    pub bytes_written: u64,
}

impl ExtractOutcome {
    /// How many files were written.
    pub fn written(&self) -> usize {
        self.files.iter().filter(|f| f.wrote_something()).count()
    }

    /// How many were deliberately skipped.
    pub fn skipped(&self) -> usize {
        self.files
            .iter()
            .filter(|f| matches!(f, Extracted::Skipped { .. }))
            .count()
    }

    /// How many could not be written.
    pub fn failed(&self) -> usize {
        self.files
            .iter()
            .filter(|f| matches!(f, Extracted::Failed { .. }))
            .count()
    }

    /// Whether everything that was meant to be written was.
    pub fn everything_worked(&self) -> bool {
        self.failed() == 0
    }

    /// A summary for the operator.
    pub fn summary(&self) -> String {
        format!(
            "{} written ({}), {} skipped, {} failed",
            self.written(),
            mjolnir_core::progress::format_bytes(self.bytes_written),
            self.skipped(),
            self.failed()
        )
    }
}

/// Copies one file out of a backup.
///
/// `into` is the folder to write inside. The file's path within the volume
/// decides where inside it lands, checked by [`safe_join`].
pub fn extract_file(
    open: &mut OpenVolume<'_>,
    entry: &IndexEntry,
    into: &Path,
    options: &ExtractOptions,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<Vec<Extracted>> {
    let source = open
        .index()
        .path_of(entry.number)
        .unwrap_or_else(|| format!("<record {}>", entry.number));

    if entry.is_directory {
        return Ok(vec![Extracted::Skipped {
            source,
            reason: "it is a directory".to_owned(),
        }]);
    }
    if entry.is_reparse_point && !options.follow_reparse_points {
        return Ok(vec![Extracted::Skipped {
            source,
            reason: "it is a junction or a link, which points at a place on the machine the backup came from".to_owned(),
        }]);
    }
    if let Some(why) = entry.why_unreadable() {
        return Ok(vec![Extracted::Skipped {
            source,
            reason: why.to_owned(),
        }]);
    }

    let mut out = Vec::new();
    let target = match safe_join(into, &source) {
        Ok(target) => target,
        Err(refusal) => {
            return Ok(vec![Extracted::Skipped {
                source,
                reason: refusal.describe(),
            }])
        }
    };

    out.push(write_one(
        open, entry, None, &source, &target, options, progress, cancel,
    ));

    if options.include_streams {
        for (name, _) in entry.streams.clone() {
            cancel.check()?;
            // A stream is written as a separate file, because a stream of a
            // file in a folder somebody chose is not something they asked for
            // and not something every filesystem can hold.
            let stream_source = format!("{source}:{name}");
            let stream_target = target.with_file_name(format!(
                "{}.{}",
                target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                sanitise_stream_name(&name)
            ));
            out.push(write_one(
                open,
                entry,
                Some(&name),
                &stream_source,
                &stream_target,
                options,
                progress,
                cancel,
            ));
        }
    }
    Ok(out)
}

/// Makes a stream name safe to put in a file name.
fn sanitise_stream_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "stream".to_owned()
    } else {
        format!("stream-{cleaned}")
    }
}

/// Writes one attribute of one file.
#[allow(clippy::too_many_arguments)]
fn write_one(
    open: &mut OpenVolume<'_>,
    entry: &IndexEntry,
    stream: Option<&str>,
    source: &str,
    target: &Path,
    options: &ExtractOptions,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Extracted {
    if target.exists() && !options.overwrite {
        return Extracted::Skipped {
            source: source.to_owned(),
            reason: format!("{} is already there", target.display()),
        };
    }
    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Extracted::Failed {
                source: source.to_owned(),
                reason: format!("the folder {} could not be made: {e}", parent.display()),
            };
        }
    }

    let number = entry.number;
    let stream_name = stream.map(str::to_owned);
    let result = open.with_volume(|volume| {
        let record = volume.record(number)?;
        let attribute = match &stream_name {
            None => record.data().cloned(),
            Some(name) => record
                .alternate_streams()
                .into_iter()
                .find(|a| a.name == *name)
                .cloned(),
        };
        let Some(attribute) = attribute else {
            return Err(Error::new(
                ExitCode::CorruptBackup,
                "the file has no contents in the backup",
                "its record does not carry the attribute holding its data".to_owned(),
                "restore the whole disk instead, which reproduces the volume exactly",
            ));
        };
        copy_attribute(volume, &attribute, target, progress, cancel)
    });

    match result {
        Ok(bytes) => Extracted::Written {
            source: source.to_owned(),
            target: target.to_path_buf(),
            bytes,
        },
        Err(e) => {
            // A half written file is worse than none: it looks like the file.
            let _ = std::fs::remove_file(target);
            if e.exit() == ExitCode::Unsupported {
                Extracted::Skipped {
                    source: source.to_owned(),
                    reason: format!("{}: {}", e.what(), e.why()),
                }
            } else {
                Extracted::Failed {
                    source: source.to_owned(),
                    reason: format!("{}: {}", e.what(), e.why()),
                }
            }
        }
    }
}

/// Copies an attribute's contents into a file.
fn copy_attribute(
    volume: &mut Volume<'_>,
    attribute: &mjolnir_ntfs::record::Attribute,
    target: &Path,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<u64> {
    use std::io::Write;

    let mut file = std::fs::File::create(target).map_err(|e| {
        Error::new(
            ExitCode::Io,
            format!("{} could not be created", target.display()),
            e.to_string(),
            "check there is room on the drive and that the folder can be written to",
        )
    })?;

    let total = attribute.data_size;
    let mut done = 0u64;
    let mut buffer = vec![0u8; COPY_CHUNK];

    while done < total {
        cancel.check()?;
        let take = COPY_CHUNK.min((total - done) as usize);
        volume.read_attribute(attribute, done, &mut buffer[..take])?;
        file.write_all(&buffer[..take]).map_err(|e| {
            Error::new(
                ExitCode::Io,
                format!("{} could not be written", target.display()),
                e.to_string(),
                "check there is room on the drive",
            )
        })?;
        done += take as u64;
        progress.advance(take as u64);
    }
    file.flush().map_err(|e| {
        Error::new(
            ExitCode::Io,
            format!("{} could not be finished", target.display()),
            e.to_string(),
            "check there is room on the drive",
        )
    })?;
    Ok(total)
}

/// Copies a directory and everything under it.
pub fn extract_tree(
    open: &mut OpenVolume<'_>,
    root: &IndexEntry,
    into: &Path,
    options: &ExtractOptions,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<ExtractOutcome> {
    let mut outcome = ExtractOutcome::default();
    let mut queue = vec![root.number];
    let mut seen = std::collections::BTreeSet::new();

    while let Some(number) = queue.pop() {
        cancel.check()?;
        if !seen.insert(number) {
            // A hard link or a loop in a damaged volume would otherwise make
            // this run forever.
            continue;
        }

        let Some(entry) = open.index().entry(number).cloned() else {
            continue;
        };

        if entry.is_directory {
            // A junction is a directory that points somewhere else. Walking
            // into one would copy a part of the volume nobody asked for, and
            // could walk in a circle.
            if entry.is_reparse_point && !options.follow_reparse_points {
                outcome.files.push(Extracted::Skipped {
                    source: open
                        .index()
                        .path_of(number)
                        .unwrap_or_else(|| entry.name.clone()),
                    reason: "it is a junction, and following one copies a part of the volume nobody asked for".to_owned(),
                });
                continue;
            }
            for child in open.index().children_of(number) {
                queue.push(child.number);
            }
            continue;
        }

        for result in extract_file(open, &entry, into, options, progress, cancel)? {
            if let Extracted::Written { bytes, .. } = &result {
                outcome.bytes_written += bytes;
            }
            outcome.files.push(result);
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, number: u64) -> IndexEntry {
        IndexEntry {
            number,
            parent: 5,
            name: name.to_owned(),
            is_directory: false,
            size: 10,
            is_reparse_point: false,
            is_compressed: false,
            is_encrypted: false,
            is_sparse: false,
            is_hard_linked: false,
            streams: Vec::new(),
        }
    }

    #[test]
    fn the_defaults_are_the_cautious_ones() {
        let options = ExtractOptions::default();
        assert!(
            !options.follow_reparse_points,
            "following a link by default can write outside the chosen folder"
        );
        assert!(
            !options.overwrite,
            "overwriting by default can destroy what somebody was recovering"
        );
        assert!(options.include_streams);
    }

    #[test]
    fn a_stream_name_becomes_a_safe_file_name() {
        assert_eq!(sanitise_stream_name("hidden"), "stream-hidden");
        assert_eq!(
            sanitise_stream_name("Zone.Identifier"),
            "stream-Zone_Identifier"
        );
        assert_eq!(sanitise_stream_name("a/b\\c:d"), "stream-a_b_c_d");
        assert_eq!(sanitise_stream_name(""), "stream");
        assert_eq!(sanitise_stream_name("../.."), "stream-_____");
    }

    #[test]
    fn an_outcome_counts_what_happened() {
        let outcome = ExtractOutcome {
            files: vec![
                Extracted::Written {
                    source: "a".to_owned(),
                    target: PathBuf::from("a"),
                    bytes: 100,
                },
                Extracted::Written {
                    source: "b".to_owned(),
                    target: PathBuf::from("b"),
                    bytes: 200,
                },
                Extracted::Skipped {
                    source: "c".to_owned(),
                    reason: "compressed".to_owned(),
                },
                Extracted::Failed {
                    source: "d".to_owned(),
                    reason: "damaged".to_owned(),
                },
            ],
            bytes_written: 300,
        };
        assert_eq!(outcome.written(), 2);
        assert_eq!(outcome.skipped(), 1);
        assert_eq!(outcome.failed(), 1);
        assert!(!outcome.everything_worked());

        let summary = outcome.summary();
        assert!(summary.contains("2 written"));
        assert!(summary.contains("1 skipped"));
        assert!(summary.contains("1 failed"));
    }

    #[test]
    fn an_outcome_with_nothing_wrong_says_so() {
        let outcome = ExtractOutcome {
            files: vec![Extracted::Written {
                source: "a".to_owned(),
                target: PathBuf::from("a"),
                bytes: 1,
            }],
            bytes_written: 1,
        };
        assert!(outcome.everything_worked());
    }

    #[test]
    fn every_result_describes_itself() {
        let results = [
            Extracted::Written {
                source: "\\a\\b.txt".to_owned(),
                target: PathBuf::from(r"C:\out\a\b.txt"),
                bytes: 4096,
            },
            Extracted::Skipped {
                source: "\\c".to_owned(),
                reason: "it is a directory".to_owned(),
            },
            Extracted::Failed {
                source: "\\d".to_owned(),
                reason: "a chunk was missing".to_owned(),
            },
        ];
        for result in &results {
            assert!(!result.describe().is_empty());
        }
        assert!(results[0].describe().contains("4.0 KiB"));
        assert!(results[2].describe().starts_with("FAILED"));
        assert!(results[0].wrote_something());
        assert!(!results[1].wrote_something());
    }

    #[test]
    fn entries_carry_what_the_extractor_needs() {
        let mut e = entry("x.txt", 10);
        assert!(e.is_readable());
        assert!(e.why_unreadable().is_none());

        e.is_compressed = true;
        assert!(!e.is_readable());
        assert!(e.why_unreadable().unwrap().contains("compressed"));

        e.is_compressed = false;
        e.is_encrypted = true;
        assert!(e
            .why_unreadable()
            .unwrap()
            .contains("Encrypting File System"));
    }
}
