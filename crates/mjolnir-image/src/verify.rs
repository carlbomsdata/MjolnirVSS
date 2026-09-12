//! Verification.
//!
//! Verification is not a formality here. It decompresses every chunk, hashes it
//! with BLAKE3 and compares the digest with the manifest, so a bad sector on
//! the destination drive is found now rather than during a recovery. It also
//! checks that the reconstructed ranges make sense: nothing overlaps, nothing
//! reaches outside its partition, and the regions that are deliberately not
//! carried are reported rather than assumed.
//!
//! A verification that finds anything fatal must leave the backup unusable,
//! which is why `completion.json` is only written after this passes.

use std::collections::BTreeSet;

use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::Result;
use mjolnir_core::math;
use mjolnir_core::progress::Progress;
use mjolnir_core::timestamp::UtcTimestamp;

use crate::completion::{Verification, VerificationResult};
use crate::disk_layout::DiskLayout;
use crate::issue::{Issue, IssueList};
use crate::manifest::Manifest;
use crate::store::ChunkStore;

/// How thoroughly to verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyDepth {
    /// Check the documents and that every chunk file exists and is the right
    /// size on disk. Fast, and enough to catch a truncated copy.
    Structure,
    /// Everything above, plus decompress and hash every chunk.
    ///
    /// This is what runs at the end of a backup and what `verify` does by
    /// default, because a chunk file of the right size full of the wrong bytes
    /// is exactly the failure a backup has to survive.
    Full,
}

/// What verification found.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// Findings, in the order they were discovered.
    pub issues: Vec<Issue>,
    /// How many chunks were decompressed and hashed.
    pub chunks_verified: u64,
    /// How many uncompressed bytes were hashed.
    pub bytes_verified: u64,
    /// Ranges of each stream that no segment covers, reported so a sparse
    /// capture is explicit about what it does not carry.
    pub uncovered: Vec<UncoveredRange>,
}

/// A region of a stream that no segment covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncoveredRange {
    /// The stream it belongs to.
    pub stream: String,
    /// How many bytes in total are uncovered in that stream.
    pub bytes: u64,
    /// How many separate gaps there are.
    pub gaps: u64,
}

impl VerifyReport {
    /// Whether anything found blocks restoration.
    pub fn passed(&self) -> bool {
        !self.issues.has_errors()
    }

    /// Turns the report into the record stored in `completion.json`.
    pub fn to_record(&self, now: UtcTimestamp) -> Verification {
        let problems: Vec<String> = self
            .issues
            .iter()
            .filter(|i| i.is_error())
            .take(32)
            .map(|i| format!("{}: {}", i.object, i.problem))
            .collect();
        Verification {
            result: if self.passed() {
                VerificationResult::Passed
            } else {
                VerificationResult::Failed
            },
            completed_utc: Some(now.to_rfc3339()),
            chunks_verified: self.chunks_verified,
            bytes_verified: self.bytes_verified,
            problems,
        }
    }
}

/// Verifies a manifest against a chunk store.
///
/// `disk_layout` is optional so that a manifest can be verified on its own
/// during a backup, before the caller has finished assembling the layout
/// document; [`crate::set::BackupSet`] always supplies it.
pub fn verify(
    manifest: &Manifest,
    disk_layout: Option<&DiskLayout>,
    store: &ChunkStore,
    depth: VerifyDepth,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<VerifyReport> {
    let mut issues = manifest.validate();
    if let Some(layout) = disk_layout {
        issues.extend(layout.validate());
    }
    issues.extend(check_reconstruction(manifest));

    let uncovered = collect_uncovered(manifest);

    let total_bytes: u64 = manifest
        .chunks
        .iter()
        .map(|c| u64::from(c.uncompressed_size))
        .sum();
    progress.begin(
        match depth {
            VerifyDepth::Structure => "Checking backup structure",
            VerifyDepth::Full => "Verifying backup",
        },
        Some(total_bytes),
    );

    let mut chunks_verified = 0u64;
    let mut bytes_verified = 0u64;

    for (i, chunk) in manifest.chunks.iter().enumerate() {
        cancel.check()?;
        let object = format!("chunk {i} ({})", chunk.hash);

        match depth {
            VerifyDepth::Structure => {
                let path = store.path_of(chunk.hash);
                match std::fs::metadata(&path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        issues.push(Issue::error(&object, "is missing from the chunk store"));
                    }
                    Err(e) => {
                        issues.push(Issue::error(&object, format!("could not be read: {e}")));
                    }
                    Ok(meta) => {
                        if meta.len() != chunk.compressed_size {
                            issues.push(Issue::error(
                                &object,
                                format!(
                                    "is {} bytes on the drive but the manifest records {}; the file is truncated or was replaced",
                                    meta.len(),
                                    chunk.compressed_size
                                ),
                            ));
                        }
                    }
                }
            }
            VerifyDepth::Full => match store.get(chunk.hash, chunk.uncompressed_size) {
                Ok(bytes) => {
                    chunks_verified += 1;
                    bytes_verified =
                        math::add_u64("verified bytes", bytes_verified, bytes.len() as u64)?;
                }
                Err(e) => {
                    // The store's explanation names the digest, and so does the
                    // object this finding is filed under, so the repetition is
                    // trimmed rather than printed twice.
                    let problem = e
                        .what()
                        .strip_prefix(&format!("chunk {} ", chunk.hash))
                        .map(str::to_owned)
                        .unwrap_or_else(|| e.what().to_owned());
                    issues.push(Issue::error(&object, problem));
                }
            },
        }
        progress.advance(u64::from(chunk.uncompressed_size));
    }
    progress.end();

    Ok(VerifyReport {
        issues,
        chunks_verified,
        bytes_verified,
        uncovered,
    })
}

/// Checks that the streams reconstruct into something sane.
///
/// [`Manifest::validate`] already checks each stream's segments against that
/// stream. This adds the cross stream question: two streams that both claim the
/// same range of the same disk would have a restore write one over the other.
fn check_reconstruction(manifest: &Manifest) -> Vec<Issue> {
    let mut issues = Vec::new();

    let disks: BTreeSet<&mjolnir_core::ids::DiskId> =
        manifest.streams.iter().map(|s| &s.disk_id).collect();

    for disk_id in disks {
        let mut placed: Vec<(&str, u64, u64)> = manifest
            .streams
            .iter()
            .filter(|s| &s.disk_id == disk_id)
            .map(|s| (s.id.as_str(), s.target_offset, s.length))
            .collect();
        placed.sort_by_key(|(_, offset, length)| (*offset, *length));

        for w in placed.windows(2) {
            let (a_id, a_off, a_len) = w[0];
            let (b_id, b_off, b_len) = w[1];
            match math::ranges_overlap(a_off, a_len, b_off, b_len) {
                Ok(true) => issues.push(Issue::error(
                    format!("disk {:?}", disk_id.as_str()),
                    format!(
                        "streams {a_id:?} and {b_id:?} would both be restored over bytes around {}; a restore must never write the same range twice",
                        b_off
                    ),
                )),
                Ok(false) => {}
                Err(e) => issues.push(Issue::error(
                    format!("disk {:?}", disk_id.as_str()),
                    format!("{e}"),
                )),
            }
        }
    }

    issues
}

/// Summarises, per stream, how much is deliberately not carried.
fn collect_uncovered(manifest: &Manifest) -> Vec<UncoveredRange> {
    let mut out = Vec::new();
    for stream in &manifest.streams {
        // A used block or preview capture is expected to have gaps; a full one
        // is not, and that case is already an error from Manifest::validate.
        if !stream.capture.may_be_sparse() {
            continue;
        }
        let Ok(captured) = stream.captured_bytes() else {
            continue;
        };
        let uncovered = stream.length.saturating_sub(captured);
        if uncovered == 0 {
            continue;
        }
        // Count the gaps by walking the sorted segments.
        let mut gaps = 0u64;
        let mut cursor = 0u64;
        for seg in &stream.segments {
            if seg.offset > cursor {
                gaps += 1;
            }
            cursor = cursor.max(seg.offset.saturating_add(seg.length));
        }
        if cursor < stream.length {
            gaps += 1;
        }
        out.push(UncoveredRange {
            stream: stream.id.as_str().to_owned(),
            bytes: uncovered,
            gaps,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{CaptureMethod, Segment, Stream, StreamKind};
    use mjolnir_core::ids::{DiskId, StreamId};

    fn stream(id: &str, offset: u64, length: u64, segments: Vec<Segment>) -> Stream {
        Stream {
            id: StreamId::new(id).unwrap(),
            kind: StreamKind::Partition,
            disk_id: DiskId::new("disk-0").unwrap(),
            partition_id: None,
            target_offset: offset,
            length,
            capture: CaptureMethod::VssUsedBlocks,
            sparse_fill: crate::manifest::SparseFill::Zero,
            source: "test".to_owned(),
            segments,
        }
    }

    fn empty_manifest() -> Manifest {
        Manifest {
            format: crate::version::FormatHeader::current(crate::version::DocumentKind::Manifest),
            tool: crate::manifest::ToolInfo {
                product: "MjolnirVSS".to_owned(),
                version: "0.1.0".to_owned(),
            },
            backup: crate::manifest::BackupInfo {
                uuid: "99999999-8888-7777-6666-555555555555".to_owned(),
                name: mjolnir_core::ids::BackupName::new("PC_2026-09-12_1015").unwrap(),
                created_utc: "2026-09-12T10:15:00Z".to_owned(),
                kind: crate::manifest::BackupKind::Full,
                scope: "system-disk".to_owned(),
            },
            source: crate::manifest::SourceInfo {
                machine_id: mjolnir_core::ids::MachineId::new("pc-1").unwrap(),
                computer_name: "PC".to_owned(),
                windows: Default::default(),
                firmware: crate::manifest::FirmwareMode::Uefi,
            },
            chunking: Default::default(),
            compression: Default::default(),
            hash: Default::default(),
            chunk_store: Default::default(),
            vss: Default::default(),
            volumes: Vec::new(),
            streams: Vec::new(),
            chunks: Vec::new(),
            required_restore_bytes: 0,
            stats: Default::default(),
        }
    }

    #[test]
    fn overlapping_streams_on_one_disk_are_caught() {
        let mut m = empty_manifest();
        m.streams.push(stream("a", 0, 1000, vec![]));
        m.streams.push(stream("b", 500, 1000, vec![]));
        let issues = check_reconstruction(&m);
        assert!(
            issues
                .iter()
                .any(|i| i.problem.contains("same range twice")),
            "{issues:?}"
        );
    }

    #[test]
    fn adjacent_streams_do_not_overlap() {
        let mut m = empty_manifest();
        m.streams.push(stream("a", 0, 1000, vec![]));
        m.streams.push(stream("b", 1000, 1000, vec![]));
        assert!(check_reconstruction(&m).is_empty());
    }

    #[test]
    fn uncovered_ranges_are_summarised_for_a_sparse_capture() {
        let mut m = empty_manifest();
        m.streams.push(stream(
            "a",
            0,
            1000,
            vec![
                Segment {
                    offset: 100,
                    length: 100,
                    chunk: 0,
                },
                Segment {
                    offset: 500,
                    length: 100,
                    chunk: 1,
                },
            ],
        ));
        let uncovered = collect_uncovered(&m);
        assert_eq!(uncovered.len(), 1);
        // 1000 total, 200 captured, so 800 uncovered across three gaps:
        // 0..100, 200..500 and 600..1000.
        assert_eq!(uncovered[0].bytes, 800);
        assert_eq!(uncovered[0].gaps, 3);
    }

    #[test]
    fn a_fully_covered_sparse_stream_reports_nothing() {
        let mut m = empty_manifest();
        m.streams.push(stream(
            "a",
            0,
            100,
            vec![Segment {
                offset: 0,
                length: 100,
                chunk: 0,
            }],
        ));
        assert!(collect_uncovered(&m).is_empty());
    }

    #[test]
    fn a_raw_capture_is_not_reported_as_sparse() {
        let mut m = empty_manifest();
        let mut s = stream("a", 0, 1000, vec![]);
        s.capture = CaptureMethod::RawFull;
        m.streams.push(s);
        assert!(collect_uncovered(&m).is_empty());
    }
}
