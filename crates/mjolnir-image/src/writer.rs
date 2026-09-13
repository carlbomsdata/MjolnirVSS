//! Writing a backup set, in two phases.
//!
//! Phase one writes the chunks and then the two describing documents. Phase two
//! verifies what was written and, only if that passes, writes
//! `completion.json`. Until that last file appears by a rename the folder is a
//! pile of chunks that nothing will restore from, which is exactly what an
//! interrupted backup should be.
//!
//! The two phases are separate types, so it is not possible to mark a backup
//! complete without having gone through verification: [`BackupWriter::finalize`]
//! hands back a [`FinalizedBackup`], and only that can be marked complete.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use mjolnir_core::error::{Error, Result};
use mjolnir_core::ids::{BackupName, StreamId};
use mjolnir_core::math;

use crate::completion::{Completion, CompletionState, DocumentDigest, Verification};
use crate::disk_layout::DiskLayout;
use crate::hash::ChunkHash;
use crate::index::VolumeIndex;
use crate::layout::BackupLayout;
use crate::manifest::{
    BackupInfo, CaptureMethod, ChunkEntry, ChunkStoreSpec, ChunkingSpec, CompressionSpec, HashSpec,
    Manifest, Segment, SourceInfo, SparseFill, Stream, StreamKind, ToolInfo, VolumeEntry, VssInfo,
};
use crate::set::write_json_atomic;
use crate::store::ChunkStore;
use crate::version::{DocumentKind, FormatHeader};

/// Builds a backup set on a destination drive.
pub struct BackupWriter {
    layout: BackupLayout,
    store: ChunkStore,
    manifest: Manifest,
    disk_layout: DiskLayout,
    /// Maps a digest to its position in the manifest chunk table, so the same
    /// bytes appearing twice cost one table entry and one file.
    chunk_index: BTreeMap<ChunkHash, u32>,
}

/// How a backup writer should be configured.
#[derive(Debug, Clone, Default)]
pub struct WriterOptions {
    /// Chunking parameters.
    pub chunking: ChunkingSpec,
    /// Compression parameters.
    pub compression: CompressionSpec,
    /// Chunk store layout.
    pub chunk_store: ChunkStoreSpec,
}

impl BackupWriter {
    /// Creates a new backup folder under `destination`.
    ///
    /// Refuses to reuse an existing folder. Writing a second backup into a
    /// folder that already holds one would mix two chunk tables together, and
    /// the operator would have no way to tell which backup the result is.
    pub fn create(
        destination: impl AsRef<Path>,
        name: &BackupName,
        backup: BackupInfo,
        source: SourceInfo,
        options: WriterOptions,
    ) -> Result<Self> {
        let layout = BackupLayout::new(
            destination.as_ref().join(name.as_str()),
            options.chunk_store.root.clone(),
            options.chunk_store.fanout,
        );
        let dir = layout.dir().to_path_buf();

        if dir.exists() {
            return Err(Error::new(
                mjolnir_core::ExitCode::Destination,
                format!("{} already exists", dir.display()),
                "MjolnirVSS will not write a backup into a folder that already holds one, because the two would share a chunk store and neither could be trusted afterwards",
                "choose a different backup name, or delete the existing folder if you no longer need that backup",
            ));
        }
        fs::create_dir_all(&dir).map_err(|e| Error::io(dir.display(), e))?;
        fs::create_dir_all(layout.logs_dir()).map_err(|e| Error::io(dir.display(), e))?;

        let store = ChunkStore::new(layout.clone(), options.compression);
        let manifest = Manifest {
            format: FormatHeader::current(DocumentKind::Manifest),
            tool: ToolInfo {
                product: mjolnir_core::PRODUCT_NAME.to_owned(),
                version: mjolnir_core::TOOL_VERSION.to_owned(),
            },
            backup,
            source,
            chunking: options.chunking,
            compression: options.compression,
            hash: HashSpec::default(),
            chunk_store: options.chunk_store,
            vss: VssInfo::default(),
            volumes: Vec::new(),
            streams: Vec::new(),
            chunks: Vec::new(),
            required_restore_bytes: 0,
            stats: Default::default(),
        };
        let disk_layout = DiskLayout::new(manifest.backup.uuid.clone());

        Ok(Self {
            layout,
            store,
            manifest,
            disk_layout,
            chunk_index: BTreeMap::new(),
        })
    }

    /// Paths inside the backup being written.
    pub fn layout(&self) -> &BackupLayout {
        &self.layout
    }

    /// The chunk size in use.
    pub fn chunk_size(&self) -> u32 {
        self.manifest.chunking.chunk_size
    }

    /// Records what the shadow copy service did.
    pub fn set_vss_info(&mut self, vss: VssInfo) {
        self.manifest.vss = vss;
    }

    /// Records the disks that were captured.
    pub fn set_disks(&mut self, disks: Vec<crate::disk_layout::DiskEntry>) {
        self.disk_layout.disks = disks;
    }

    /// Records a filesystem found inside a captured partition.
    pub fn add_volume(&mut self, volume: VolumeEntry) {
        self.manifest.volumes.push(volume);
    }

    /// Writes a volume's file index and records where it went.
    pub fn write_volume_index(&mut self, index: &VolumeIndex) -> Result<()> {
        let path = self.layout.volume_index_path(&index.volume_id);
        write_json_atomic(&path, index)?;
        let relative = BackupLayout::volume_index_relative(&index.volume_id);
        for volume in &mut self.manifest.volumes {
            if volume.id == index.volume_id {
                volume.index_path = Some(relative.clone());
            }
        }
        Ok(())
    }

    /// Stores one chunk and returns its position in the chunk table.
    ///
    /// Repeated content costs one table entry and one file, no matter how many
    /// segments point at it.
    pub fn put_chunk(&mut self, data: &[u8]) -> Result<u32> {
        let outcome = self.store.put(data)?;
        if let Some(existing) = self.chunk_index.get(&outcome.hash) {
            return Ok(*existing);
        }
        let index = u32::try_from(self.manifest.chunks.len()).map_err(|_| {
            Error::new(
                mjolnir_core::ExitCode::Failure,
                "this backup holds more chunks than the format can address",
                "the chunk table is indexed with a 32 bit number, which caps a single backup at about four billion chunks",
                "back up less data in one run, or use a larger chunk size",
            )
        })?;
        let uncompressed_size = math::to_u32("chunk size", data.len() as u64)?;
        self.manifest.chunks.push(ChunkEntry {
            hash: outcome.hash,
            uncompressed_size,
            compressed_size: outcome.compressed_size,
        });
        self.chunk_index.insert(outcome.hash, index);
        Ok(index)
    }

    /// Starts describing one captured byte space.
    ///
    /// The argument list is long because a stream is described by exactly this
    /// much: what it is, which disk and partition it belongs to, where it goes
    /// and how it was read. Grouping them into a struct would move the same
    /// fields somewhere else without making any call site clearer.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_stream(
        &mut self,
        id: StreamId,
        kind: StreamKind,
        disk_id: mjolnir_core::ids::DiskId,
        partition_id: Option<mjolnir_core::ids::PartitionId>,
        target_offset: u64,
        length: u64,
        capture: CaptureMethod,
        source: impl Into<String>,
    ) -> StreamWriter<'_> {
        StreamWriter {
            writer: self,
            stream: Stream {
                id,
                kind,
                disk_id,
                partition_id,
                target_offset,
                length,
                capture,
                sparse_fill: SparseFill::Zero,
                source: source.into(),
                segments: Vec::new(),
                used_blocks: None,
                fallback_reason: None,
            },
            last_end: 0,
        }
    }

    /// Removes temporary files left behind by an interrupted run.
    pub fn sweep_temp_files(&self) -> Result<usize> {
        self.store.sweep_temp_files()
    }

    /// Writes `manifest.json` and `disk-layout.json`.
    ///
    /// Everything the backup consists of is on the drive after this returns,
    /// except the completion marker. Verification runs next.
    pub fn finalize(mut self) -> Result<FinalizedBackup> {
        self.manifest.stats = self.manifest.recompute_stats()?;
        self.manifest.required_restore_bytes = self.compute_required_restore_bytes()?;
        self.disk_layout.backup_uuid = self.manifest.backup.uuid.clone();

        let manifest_digest = write_json_atomic(&self.layout.manifest_path(), &self.manifest)?;
        let layout_digest = write_json_atomic(&self.layout.disk_layout_path(), &self.disk_layout)?;

        let mut documents = vec![manifest_digest, layout_digest];
        for volume in &self.manifest.volumes {
            let Some(relative) = &volume.index_path else {
                continue;
            };
            let Some(path) = self.layout.resolve_relative(relative) else {
                continue;
            };
            let bytes = fs::read(&path).map_err(|e| Error::io(path.display(), e))?;
            documents.push(DocumentDigest {
                path: relative.clone(),
                blake3: ChunkHash::of(&bytes),
                bytes: bytes.len() as u64,
            });
        }

        Ok(FinalizedBackup {
            layout: self.layout,
            manifest: self.manifest,
            disk_layout: self.disk_layout,
            documents,
        })
    }

    fn compute_required_restore_bytes(&self) -> Result<u64> {
        let mut required = 0u64;
        for disk in &self.disk_layout.disks {
            required = required.max(disk.required_target_bytes()?);
        }
        Ok(required)
    }
}

/// Accumulates the segments of one stream.
pub struct StreamWriter<'a> {
    writer: &'a mut BackupWriter,
    stream: Stream,
    last_end: u64,
}

impl StreamWriter<'_> {
    /// The stream's logical length.
    pub fn length(&self) -> u64 {
        self.stream.length
    }

    /// Records what a used block capture measured.
    pub fn set_used_blocks(&mut self, info: crate::manifest::UsedBlockInfo) {
        self.stream.used_blocks = Some(info);
    }

    /// Records that used block imaging was wanted and could not be used.
    ///
    /// Changes the recorded capture method too, so the manifest never claims a
    /// method the stream was not actually captured with.
    pub fn fall_back_to(&mut self, capture: CaptureMethod, reason: impl Into<String>) {
        self.stream.capture = capture;
        self.stream.used_blocks = None;
        self.stream.fallback_reason = Some(reason.into());
    }

    /// Records one piece of the stream.
    ///
    /// `offset` is measured from the start of the stream. Pieces must arrive in
    /// ascending order and must not overlap; both are checked here rather than
    /// left for validation to find later, because getting it wrong means a
    /// restore writes the same range twice.
    pub fn write_segment(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let length = data.len() as u64;
        math::ensure_within("stream segment", offset, length, self.stream.length)?;

        if offset < self.last_end {
            return Err(Error::new(
                mjolnir_core::ExitCode::Failure,
                format!(
                    "stream {:?} was given a segment at {offset} after already covering up to {}",
                    self.stream.id.as_str(),
                    self.last_end
                ),
                "segments have to be written in ascending order and must not overlap, otherwise a restore would write the same range twice and the second write would win",
                "this is an internal error; please report it with the command you ran",
            ));
        }
        if length > u64::from(self.writer.chunk_size()) {
            return Err(Error::new(
                mjolnir_core::ExitCode::Failure,
                format!(
                    "stream {:?} was given a {length} byte segment, larger than the {} byte chunk size",
                    self.stream.id.as_str(),
                    self.writer.chunk_size()
                ),
                "one segment is backed by exactly one chunk, so a segment can never be larger than a chunk",
                "this is an internal error; please report it with the command you ran",
            ));
        }

        let chunk = self.writer.put_chunk(data)?;
        self.stream.segments.push(Segment {
            offset,
            length,
            chunk,
        });
        self.last_end = math::add_u64("stream cursor", offset, length)?;
        Ok(())
    }

    /// Number of bytes covered so far.
    pub fn captured_bytes(&self) -> Result<u64> {
        Ok(self.stream.captured_bytes()?)
    }

    /// Finishes the stream and adds it to the manifest.
    ///
    /// A stream that claims to be a complete capture is checked here for gaps,
    /// so a raw capture that quietly skipped a range is caught while the source
    /// is still open rather than during a recovery months later.
    pub fn finish(self) -> Result<()> {
        let captured = self.stream.captured_bytes()?;
        let complete_capture = matches!(
            self.stream.capture,
            CaptureMethod::RawFull | CaptureMethod::VssRaw
        );
        if complete_capture && captured != self.stream.length {
            return Err(Error::new(
                mjolnir_core::ExitCode::Failure,
                format!(
                    "stream {:?} was declared a complete capture but covers only {captured} of {} bytes",
                    self.stream.id.as_str(),
                    self.stream.length
                ),
                "restoring it would leave the missing ranges as zeroes, which for a boot partition means a computer that does not start",
                "this is an internal error; please report it with the command you ran",
            ));
        }
        self.writer.manifest.streams.push(self.stream);
        Ok(())
    }
}

/// A backup whose chunks and documents are written, awaiting verification.
pub struct FinalizedBackup {
    layout: BackupLayout,
    manifest: Manifest,
    disk_layout: DiskLayout,
    documents: Vec<DocumentDigest>,
}

impl FinalizedBackup {
    /// Paths inside the backup.
    pub fn layout(&self) -> &BackupLayout {
        &self.layout
    }

    /// The manifest as written.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The disk layout as written.
    pub fn disk_layout(&self) -> &DiskLayout {
        &self.disk_layout
    }

    /// A chunk store reading what was just written.
    pub fn chunk_store(&self) -> ChunkStore {
        ChunkStore::new(self.layout.clone(), self.manifest.compression)
    }

    /// Writes `completion.json`, making the backup restorable.
    ///
    /// Refuses unless verification passed. This is the single point where a
    /// folder of chunks becomes a backup, and it deliberately has no override.
    pub fn mark_complete(
        self,
        verification: Verification,
        completed_utc: &str,
    ) -> Result<Completion> {
        if verification.result != crate::completion::VerificationResult::Passed {
            return Err(Error::corrupt(
                "the backup was not marked complete because verification did not pass",
                format!(
                    "verification reported {:?}; marking it complete would tell you later that this backup can be restored from when it cannot",
                    verification.result
                ),
                "the incomplete folder can be deleted; take a new backup, and check the destination drive if this happens again",
            ));
        }
        self.write_completion(CompletionState::Complete, verification, completed_utc)
    }

    /// Records that the run failed, without making the backup restorable.
    ///
    /// Used when verification found problems. Writing this is better than
    /// leaving a bare folder, because the operator gets told what happened
    /// instead of finding an unexplained directory months later.
    pub fn mark_failed(
        self,
        verification: Verification,
        completed_utc: &str,
    ) -> Result<Completion> {
        self.write_completion(CompletionState::Failed, verification, completed_utc)
    }

    fn write_completion(
        self,
        state: CompletionState,
        verification: Verification,
        completed_utc: &str,
    ) -> Result<Completion> {
        let mut completion = Completion::new(
            self.manifest.backup.uuid.clone(),
            state,
            completed_utc,
            verification,
            self.manifest.required_restore_bytes,
        );
        completion.documents = self.documents;
        write_json_atomic(&self.layout.completion_path(), &completion)?;
        Ok(completion)
    }
}
