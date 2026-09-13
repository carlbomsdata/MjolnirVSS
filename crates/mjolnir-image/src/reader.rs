//! Reading a captured partition back out of a backup, as if it were a disk.
//!
//! A stream in a backup is a list of segments, each backed by one stored chunk,
//! covering a partition that may have gaps in it. This turns that into a
//! [`BlockSource`], so anything that can read a disk can read a backup instead
//! without knowing it is doing so. That is what lets the NTFS reader browse a
//! backup with exactly the code that would browse a volume.
//!
//! # Every byte is checked on the way out
//!
//! Chunks come through [`ChunkStore::get`], which decompresses and compares the
//! BLAKE3 digest of every chunk it returns. Extracting a file therefore checks
//! the backup as it goes, and a damaged chunk stops the extraction with a
//! message naming it rather than producing a file that is quietly wrong.
//!
//! # Gaps read as zeros
//!
//! A used block capture does not store the free space, and the format defines
//! the uncovered ranges to be zeros. Reading one here produces zeros, which is
//! the same thing a restore writes.
//!
//! Nothing here writes. The backup is opened read only and there is no path
//! from this module to a file being modified.

use std::collections::VecDeque;

use mjolnir_core::blockio::BlockSource;
use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;

use crate::manifest::{Manifest, SparseFill, Stream};
use crate::store::ChunkStore;

/// How many decompressed chunks to keep.
///
/// Walking a master file table reads the same chunk many times in a row, and
/// decompressing it each time makes a scan take minutes instead of seconds.
/// Eight is enough for that pattern and bounds the memory at eight chunks.
const CACHE_CHUNKS: usize = 8;

/// One captured partition, read as though it were a disk.
pub struct StreamReader<'a> {
    stream: &'a Stream,
    manifest: &'a Manifest,
    store: ChunkStore,
    cache: VecDeque<(u32, Vec<u8>)>,
}

impl<'a> StreamReader<'a> {
    /// Opens a stream for reading.
    pub fn new(manifest: &'a Manifest, stream: &'a Stream, store: ChunkStore) -> Self {
        Self {
            stream,
            manifest,
            store,
            cache: VecDeque::with_capacity(CACHE_CHUNKS),
        }
    }

    /// The stream this reads.
    pub fn stream(&self) -> &Stream {
        self.stream
    }

    /// The logical length of the partition.
    pub fn length(&self) -> u64 {
        self.stream.length
    }

    /// Fetches a chunk, from the cache when it is there.
    fn chunk(&mut self, index: u32) -> Result<&[u8]> {
        if let Some(position) = self.cache.iter().position(|(i, _)| *i == index) {
            // Moved to the back so the least recently used one is at the front.
            let entry = self.cache.remove(position).expect("found above");
            self.cache.push_back(entry);
            return Ok(&self.cache.back().expect("just pushed").1);
        }

        let entry = self.manifest.chunks.get(index as usize).ok_or_else(|| {
            Error::corrupt(
                format!("the backup refers to chunk {index}, which is not in its own list"),
                "the manifest disagrees with itself, which means it has been edited or damaged",
                "run the verify command against this backup",
            )
        })?;

        // This is where the digest is checked, so every byte that reaches a
        // caller has been compared against what was captured.
        let data = self.store.get(entry.hash, entry.uncompressed_size)?;

        if self.cache.len() == CACHE_CHUNKS {
            self.cache.pop_front();
        }
        self.cache.push_back((index, data));
        Ok(&self.cache.back().expect("just pushed").1)
    }
}

impl BlockSource for StreamReader<'_> {
    fn read_exact_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<()> {
        if buffer.is_empty() {
            return Ok(());
        }
        let end = offset.checked_add(buffer.len() as u64).ok_or_else(|| {
            Error::new(
                ExitCode::Failure,
                "a read from the backup runs past what can be addressed",
                format!("{offset} plus {} bytes overflows", buffer.len()),
                "this is an internal error; please report it with the command you ran",
            )
        })?;
        if end > self.stream.length {
            return Err(Error::new(
                ExitCode::Failure,
                "a read runs past the end of the captured partition",
                format!(
                    "{} bytes at {offset} of a {} byte partition",
                    buffer.len(),
                    self.stream.length
                ),
                "this is an internal error; please report it with the command you ran",
            ));
        }

        // Anything no segment covers is defined to be zeros, so the buffer is
        // filled first and only the covered parts are overwritten.
        match self.stream.sparse_fill {
            SparseFill::Zero => buffer.fill(0),
        }

        // Segments are sorted and do not overlap, which the writer enforces, so
        // the first one that could reach `offset` is found by bisection.
        let first = self
            .stream
            .segments
            .partition_point(|s| s.offset + s.length <= offset);

        for index in first..self.stream.segments.len() {
            let segment = self.stream.segments[index];
            if segment.offset >= end {
                break;
            }
            let from = segment.offset.max(offset);
            let to = (segment.offset + segment.length).min(end);
            if from >= to {
                continue;
            }

            let chunk_index = segment.chunk;
            let into_chunk = (from - segment.offset) as usize;
            let take = (to - from) as usize;
            let into_buffer = (from - offset) as usize;

            let chunk = self.chunk(chunk_index)?;
            if into_chunk + take > chunk.len() {
                return Err(Error::corrupt(
                    format!("chunk {chunk_index} is smaller than the segment using it"),
                    format!(
                        "the segment wants {take} bytes at {into_chunk} of a {} byte chunk",
                        chunk.len()
                    ),
                    "run the verify command against this backup",
                ));
            }
            buffer[into_buffer..into_buffer + take]
                .copy_from_slice(&chunk[into_chunk..into_chunk + take]);
        }
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.stream.length
    }

    fn logical_sector_size(&self) -> u32 {
        // The backup does not record a sector size per stream, and nothing
        // reading a captured partition needs one: every read is by byte offset.
        // 512 is what every layout MjolnirVSS supports uses or is a multiple of.
        512
    }

    fn describe(&self) -> String {
        format!("captured partition {}", self.stream.id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{CaptureMethod, ChunkEntry, Segment, StreamKind};
    use crate::store::ChunkStore;
    use crate::version::{DocumentKind, FormatHeader};
    use mjolnir_core::ids::{DiskId, StreamId};

    /// Builds a backup folder holding one stream made of the given pieces.
    ///
    /// Each piece is `(offset, contents)`. Anything not covered is a gap.
    fn backup_with(
        dir: &std::path::Path,
        length: u64,
        pieces: &[(u64, Vec<u8>)],
    ) -> (Manifest, Stream, ChunkStore) {
        let layout = crate::layout::BackupLayout::with_defaults(dir.to_path_buf());
        std::fs::create_dir_all(layout.dir()).unwrap();
        let store = ChunkStore::new(layout, crate::manifest::CompressionSpec::default());

        let mut chunks = Vec::new();
        let mut segments = Vec::new();
        for (offset, data) in pieces {
            let outcome = store.put(data).unwrap();
            let index = chunks.len() as u32;
            chunks.push(ChunkEntry {
                hash: outcome.hash,
                uncompressed_size: data.len() as u32,
                compressed_size: outcome.compressed_size,
            });
            segments.push(Segment {
                offset: *offset,
                length: data.len() as u64,
                chunk: index,
            });
        }
        segments.sort_by_key(|s| s.offset);

        let stream = Stream {
            id: StreamId::new("disk-0-part-1").unwrap(),
            kind: StreamKind::Partition,
            disk_id: DiskId::new("disk-0").unwrap(),
            partition_id: None,
            target_offset: 0,
            length,
            capture: CaptureMethod::VssUsedBlocks,
            sparse_fill: SparseFill::Zero,
            source: "test".to_owned(),
            segments,
            used_blocks: None,
            fallback_reason: None,
        };

        let manifest = Manifest {
            format: FormatHeader::current(DocumentKind::Manifest),
            tool: crate::manifest::ToolInfo {
                product: "MjolnirVSS".to_owned(),
                version: "0.1.0".to_owned(),
            },
            backup: crate::manifest::BackupInfo {
                uuid: "11111111-2222-3333-4444-555555555555".to_owned(),
                name: mjolnir_core::ids::BackupName::new("test").unwrap(),
                created_utc: "2026-01-01T00:00:00Z".to_owned(),
                kind: crate::manifest::BackupKind::Full,
                scope: "system-disk".to_owned(),
            },
            source: crate::manifest::SourceInfo {
                machine_id: mjolnir_core::ids::MachineId::new("test-pc").unwrap(),
                computer_name: "TEST-PC".to_owned(),
                windows: Default::default(),
                firmware: crate::manifest::FirmwareMode::Uefi,
            },
            chunking: Default::default(),
            compression: Default::default(),
            hash: Default::default(),
            chunk_store: Default::default(),
            encryption: None,
            vss: Default::default(),
            volumes: Vec::new(),
            streams: vec![stream.clone()],
            chunks,
            stats: Default::default(),
            required_restore_bytes: length,
        };

        let store = ChunkStore::new(
            crate::layout::BackupLayout::with_defaults(dir.to_path_buf()),
            crate::manifest::CompressionSpec::default(),
        );
        (manifest, stream, store)
    }

    #[test]
    fn a_covered_range_reads_back_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let contents: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        let (manifest, stream, store) = backup_with(dir.path(), 1 << 20, &[(0, contents.clone())]);
        let mut reader = StreamReader::new(&manifest, &stream, store);

        let mut got = vec![0u8; 4096];
        reader.read_exact_at(0, &mut got).unwrap();
        assert_eq!(got, contents);
    }

    /// The property used block imaging depends on: a range nobody captured
    /// reads as zeros rather than as whatever was nearby.
    #[test]
    fn a_gap_reads_as_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let (manifest, stream, store) = backup_with(
            dir.path(),
            1 << 20,
            &[(0, vec![0xAA; 4096]), (65536, vec![0xBB; 4096])],
        );
        let mut reader = StreamReader::new(&manifest, &stream, store);

        let mut got = vec![0xFF; 8192];
        reader.read_exact_at(8192, &mut got).unwrap();
        assert!(got.iter().all(|b| *b == 0), "a gap did not read as zeros");
    }

    #[test]
    fn a_read_spanning_a_gap_gets_both_sides_and_the_hole() {
        let dir = tempfile::tempdir().unwrap();
        let (manifest, stream, store) = backup_with(
            dir.path(),
            1 << 20,
            &[(0, vec![0xAA; 4096]), (8192, vec![0xBB; 4096])],
        );
        let mut reader = StreamReader::new(&manifest, &stream, store);

        let mut got = vec![0xFF; 12288];
        reader.read_exact_at(0, &mut got).unwrap();
        assert!(got[..4096].iter().all(|b| *b == 0xAA));
        assert!(got[4096..8192].iter().all(|b| *b == 0));
        assert!(got[8192..].iter().all(|b| *b == 0xBB));
    }

    #[test]
    fn a_read_inside_one_segment_is_offset_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let contents: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let (manifest, stream, store) =
            backup_with(dir.path(), 1 << 20, &[(16384, contents.clone())]);
        let mut reader = StreamReader::new(&manifest, &stream, store);

        let mut got = vec![0u8; 100];
        reader.read_exact_at(16384 + 1000, &mut got).unwrap();
        assert_eq!(got, contents[1000..1100]);
    }

    #[test]
    fn reading_past_the_end_of_the_partition_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (manifest, stream, store) = backup_with(dir.path(), 8192, &[(0, vec![1; 8192])]);
        let mut reader = StreamReader::new(&manifest, &stream, store);

        let mut got = vec![0u8; 8193];
        assert!(reader.read_exact_at(0, &mut got).is_err());
        assert!(reader.read_exact_at(1, &mut got[..8192]).is_err());
        assert!(reader.read_exact_at(0, &mut got[..8192]).is_ok());
    }

    /// The check that makes extraction trustworthy: a damaged chunk is caught
    /// when it is read, not later.
    #[test]
    fn a_damaged_chunk_is_caught_when_it_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let (manifest, stream, store) = backup_with(dir.path(), 8192, &[(0, vec![0x5A; 4096])]);

        // Damage the stored chunk.
        let path = store.path_of(manifest.chunks[0].hash);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = StreamReader::new(&manifest, &stream, store);
        let mut got = vec![0u8; 4096];
        let err = reader.read_exact_at(0, &mut got).unwrap_err();
        assert_eq!(err.exit(), ExitCode::CorruptBackup);
    }

    #[test]
    fn a_missing_chunk_is_reported_rather_than_read_around() {
        let dir = tempfile::tempdir().unwrap();
        let (manifest, stream, store) = backup_with(dir.path(), 8192, &[(0, vec![0x5A; 4096])]);
        std::fs::remove_file(store.path_of(manifest.chunks[0].hash)).unwrap();

        let mut reader = StreamReader::new(&manifest, &stream, store);
        let mut got = vec![0u8; 4096];
        let err = reader.read_exact_at(0, &mut got).unwrap_err();
        assert!(err.what().contains("missing"), "{}", err.what());
    }

    /// Reading the same region repeatedly is what walking a master file table
    /// does, and it must not decompress the chunk every time.
    #[test]
    fn a_repeated_read_uses_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let (manifest, stream, store) = backup_with(dir.path(), 1 << 20, &[(0, vec![0x11; 4096])]);
        let mut reader = StreamReader::new(&manifest, &stream, store);

        let mut got = vec![0u8; 16];
        for _ in 0..100 {
            reader.read_exact_at(0, &mut got).unwrap();
        }
        assert_eq!(got, vec![0x11; 16]);

        // Removing the stored chunk afterwards proves the reads came from the
        // cache: the first one filled it, and the rest never touched the disk.
        std::fs::remove_file(reader.store.path_of(manifest.chunks[0].hash)).unwrap();
        reader.read_exact_at(0, &mut got).unwrap();
    }

    #[test]
    fn an_empty_read_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (manifest, stream, store) = backup_with(dir.path(), 8192, &[]);
        let mut reader = StreamReader::new(&manifest, &stream, store);
        reader.read_exact_at(0, &mut []).unwrap();
        assert_eq!(reader.size_bytes(), 8192);
        assert!(reader.describe().contains("disk-0-part-1"));
    }

    /// A stream with no segments at all is a partition nobody captured. It
    /// reads as zeros rather than failing, because that is what the format
    /// says it holds.
    #[test]
    fn a_stream_with_nothing_in_it_reads_as_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let (manifest, stream, store) = backup_with(dir.path(), 8192, &[]);
        let mut reader = StreamReader::new(&manifest, &stream, store);

        let mut got = vec![0xFF; 8192];
        reader.read_exact_at(0, &mut got).unwrap();
        assert!(got.iter().all(|b| *b == 0));
    }
}
