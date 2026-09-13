//! `manifest.json`: what was captured, how it was cut up, and where the bytes
//! went.
//!
//! The manifest owns the captured data. The physical facts about the disk live
//! in `disk-layout.json` next to it, and the two are checked against each other
//! when a backup set is opened. Keeping them apart means a restore can show the
//! operator the target layout without parsing a chunk table with fifty thousand
//! entries in it.

use std::collections::BTreeSet;

use mjolnir_core::ids::{DiskId, PartitionId, StreamId, VolumeId};
use mjolnir_core::math;
use serde::{Deserialize, Serialize};

use crate::hash::ChunkHash;
use crate::issue::Issue;
use crate::version::{DocumentKind, FormatHeader};

/// Smallest permitted chunk size.
pub const MIN_CHUNK_SIZE: u32 = 64 * 1024;

/// Largest permitted chunk size.
pub const MAX_CHUNK_SIZE: u32 = 64 * 1024 * 1024;

/// Chunk size used unless something else is asked for.
pub const DEFAULT_CHUNK_SIZE: u32 = 4 * 1024 * 1024;

/// Which tool wrote the backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    /// Always `MjolnirVSS`.
    pub product: String,
    /// Version of the tool that wrote this backup.
    pub version: String,
}

/// What kind of backup this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackupKind {
    /// Every captured byte is present in this backup set.
    Full,
}

/// Identity of one backup run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupInfo {
    /// Stable unique identifier of this backup, as a GUID.
    pub uuid: String,
    /// The folder name the operator chose, for example `PC_2026-09-12_1015`.
    pub name: mjolnir_core::ids::BackupName,
    /// RFC 3339 UTC time the snapshot was taken.
    pub created_utc: String,
    /// Full, or later incremental.
    pub kind: BackupKind,
    /// What the operator asked to capture, for example `system-disk`.
    pub scope: String,
}

/// Firmware the machine boots with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FirmwareMode {
    /// UEFI, the only mode supported in this version.
    Uefi,
    /// Legacy BIOS.
    Bios,
    /// Could not be determined.
    Unknown,
}

/// The Windows installation the backup was taken from.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WindowsInfo {
    /// Product name as Windows reports it.
    #[serde(default)]
    pub product_name: Option<String>,
    /// Build number.
    #[serde(default)]
    pub build: Option<String>,
    /// Edition.
    #[serde(default)]
    pub edition: Option<String>,
    /// Architecture, for example `x64`.
    #[serde(default)]
    pub architecture: Option<String>,
}

/// The machine the backup came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    /// Stable identifier of the machine.
    pub machine_id: mjolnir_core::ids::MachineId,
    /// Computer name at capture time.
    pub computer_name: String,
    /// Windows details.
    #[serde(default)]
    pub windows: WindowsInfo,
    /// Firmware mode.
    pub firmware: FirmwareMode,
}

/// How a stream was cut into chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChunkingAlgorithm {
    /// Cuts on multiples of the chunk size measured from offset zero.
    ///
    /// Cutting from zero rather than from the start of each run is what makes
    /// the cut points stable, so a later incremental backup of the same volume
    /// produces byte identical chunks for unchanged regions.
    Fixed,
}

/// Chunking parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkingSpec {
    /// The cutting algorithm.
    pub algorithm: ChunkingAlgorithm,
    /// Maximum uncompressed size of one chunk.
    pub chunk_size: u32,
}

impl Default for ChunkingSpec {
    fn default() -> Self {
        Self {
            algorithm: ChunkingAlgorithm::Fixed,
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }
}

/// How chunk contents are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompressionAlgorithm {
    /// Zstandard.
    Zstd,
    /// Stored uncompressed.
    None,
}

/// Compression parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressionSpec {
    /// The algorithm.
    pub algorithm: CompressionAlgorithm,
    /// Level, ignored when the algorithm is `none`.
    pub level: i32,
}

impl Default for CompressionSpec {
    fn default() -> Self {
        Self {
            algorithm: CompressionAlgorithm::Zstd,
            level: 3,
        }
    }
}

/// Which digest identifies a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HashAlgorithm {
    /// BLAKE3, over the uncompressed chunk contents.
    Blake3,
}

/// Digest parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashSpec {
    /// The algorithm.
    pub algorithm: HashAlgorithm,
    /// Digest length in bytes.
    pub digest_bytes: u32,
}

impl Default for HashSpec {
    fn default() -> Self {
        Self {
            algorithm: HashAlgorithm::Blake3,
            digest_bytes: crate::hash::DIGEST_BYTES as u32,
        }
    }
}

/// Where the chunks live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChunkStoreKind {
    /// A directory of one file per chunk, relative to the backup folder.
    LocalDirectory,
}

/// Chunk store layout.
///
/// `root` and `fanout` are fields rather than constants so that a later format
/// minor can point a backup at a store shared by every backup of the machine,
/// which is what an incremental chain needs, without changing anything else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkStoreSpec {
    /// The kind of store.
    pub kind: ChunkStoreKind,
    /// Directory name relative to the backup folder.
    pub root: String,
    /// How many leading hex characters of the digest name a subdirectory.
    /// Zero puts every chunk directly in `root`.
    pub fanout: u8,
}

impl Default for ChunkStoreSpec {
    fn default() -> Self {
        Self {
            kind: ChunkStoreKind::LocalDirectory,
            root: "chunks".to_owned(),
            fanout: 0,
        }
    }
}

/// State of one VSS writer at the end of the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriterStatusEntry {
    /// Writer name, for example `Registry Writer`.
    pub name: String,
    /// Writer class identifier.
    pub writer_id: String,
    /// Writer instance identifier.
    pub instance_id: String,
    /// State as reported by the service, in words.
    pub state: String,
    /// Whether the writer finished without failing the snapshot.
    pub succeeded: bool,
}

/// One shadow copy that took part in the backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotEntry {
    /// Shadow copy identifier.
    pub snapshot_id: String,
    /// The volume that was snapshotted, as a volume GUID path.
    pub original_volume: String,
    /// The shadow copy device the data was read from.
    pub device_object: String,
}

/// What the Volume Shadow Copy Service did during the backup.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VssInfo {
    /// Whether a shadow copy was used at all.
    pub used: bool,
    /// Snapshot set identifier.
    #[serde(default)]
    pub snapshot_set_id: Option<String>,
    /// Snapshot context, normally `backup`.
    #[serde(default)]
    pub context: Option<String>,
    /// Whether every required writer reported success.
    #[serde(default)]
    pub writers_succeeded: bool,
    /// Per writer outcome.
    #[serde(default)]
    pub writers: Vec<WriterStatusEntry>,
    /// The shadow copies that were created.
    #[serde(default)]
    pub snapshots: Vec<SnapshotEntry>,
}

/// How a partition's contents were read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureMethod {
    /// Allocated clusters only, read from a shadow copy device.
    ///
    /// The unread regions are recorded as gaps and are defined to be zero on
    /// restore. This is safe only because the allocation information comes from
    /// the frozen shadow copy, not from the live volume.
    VssUsedBlocks,
    /// Every byte of the volume, read from a shadow copy device.
    VssRaw,
    /// Every byte, read from the physical disk at the partition offset.
    ///
    /// Used for partitions the shadow copy service does not handle, such as the
    /// EFI system partition and the Microsoft Reserved partition. See
    /// `docs/backup-format.md` for the consistency argument.
    RawFull,
    /// Only the first part of the partition was captured.
    ///
    /// Produced by a preview run, which exists so the whole pipeline can be
    /// exercised in seconds rather than hours. A stream marked this way is
    /// missing its middle and its end, so restoring it would produce a
    /// partition full of holes. The restore side refuses it outright, and this
    /// variant exists precisely so that refusal can be a property of the
    /// format rather than a rule someone has to remember.
    Preview,
}

impl CaptureMethod {
    /// Whether a backup containing this capture may ever be restored to a disk.
    pub fn is_restorable(self) -> bool {
        !matches!(self, CaptureMethod::Preview)
    }

    /// Whether a stream captured this way is expected to have gaps.
    pub fn may_be_sparse(self) -> bool {
        matches!(self, CaptureMethod::VssUsedBlocks | CaptureMethod::Preview)
    }

    /// A short description for the operator and the log.
    pub const fn describe(self) -> &'static str {
        match self {
            CaptureMethod::VssUsedBlocks => "used blocks from a shadow copy",
            CaptureMethod::VssRaw => "every byte from a shadow copy",
            CaptureMethod::RawFull => "every byte read directly from the disk",
            CaptureMethod::Preview => "preview only, not restorable",
        }
    }
}

/// A filesystem found inside a partition.
///
/// Descriptive only: the bytes are carried by the stream the partition names.
/// This exists so the restore and file recovery interfaces can show something
/// an operator recognises, such as a drive letter and a volume label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeEntry {
    /// Identifies the volume within the backup.
    pub id: VolumeId,
    /// The disk the volume sits on.
    pub disk_id: DiskId,
    /// The partition the volume occupies.
    pub partition_id: PartitionId,
    /// Volume GUID path at capture time.
    #[serde(default)]
    pub guid_path: Option<String>,
    /// Drive letter at capture time, without a colon.
    #[serde(default)]
    pub drive_letter: Option<String>,
    /// Volume label.
    #[serde(default)]
    pub label: Option<String>,
    /// Filesystem, for example `NTFS` or `FAT32`.
    #[serde(default)]
    pub filesystem: Option<String>,
    /// Allocation unit size in bytes.
    #[serde(default)]
    pub cluster_size: Option<u32>,
    /// Total size of the filesystem.
    #[serde(default)]
    pub total_bytes: Option<u64>,
    /// Bytes in use at capture time.
    #[serde(default)]
    pub used_bytes: Option<u64>,
    /// Relative path of this volume's file index, when one was written.
    #[serde(default)]
    pub index_path: Option<String>,
}

/// What a used block capture measured, recorded so a reader can tell how much
/// of a volume was skipped and why the result is the size it is.
///
/// Present only on a stream captured with [`CaptureMethod::VssUsedBlocks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsedBlockInfo {
    /// Cluster size of the filesystem, in bytes.
    pub cluster_size: u32,
    /// Clusters the allocation bitmap described.
    pub clusters_total: u64,
    /// Clusters the bitmap said were in use.
    pub clusters_allocated: u64,
    /// Bytes of the partition the bitmap covers, which is
    /// `clusters_total * cluster_size`.
    pub bitmap_bytes: u64,
    /// Bytes at the end of the partition the filesystem describes nothing
    /// about, and which were therefore captured in full.
    pub undescribed_tail_bytes: u64,
    /// Bytes captured beyond the allocated clusters, because the format
    /// requires them: the boot sectors and both copies of the master file
    /// table.
    pub reserved_bytes: u64,
    /// Number of extents the capture read, after merging.
    pub extent_count: u64,
}

/// What a stream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StreamKind {
    /// Bytes from the start of the disk up to the first partition: the
    /// protective MBR, the primary GPT header and the partition entry array.
    DiskHead,
    /// The secondary GPT at the end of the disk.
    DiskTail,
    /// The contents of one partition.
    Partition,
}

/// What fills the bytes a sparse stream does not carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SparseFill {
    /// Zero. A restore target is required to be blank, so nothing is written
    /// over these ranges.
    Zero,
}

/// One piece of a stream, backed by exactly one chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    /// Offset from the start of the stream.
    pub offset: u64,
    /// Length in bytes. Always equals the chunk's uncompressed size.
    pub length: u64,
    /// Index into [`Manifest::chunks`].
    pub chunk: u32,
}

/// A captured byte space, restored to a known offset on a disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stream {
    /// Identifies the stream within the backup.
    pub id: StreamId,
    /// What the stream carries.
    pub kind: StreamKind,
    /// The disk this stream belongs to, as named in `disk-layout.json`.
    pub disk_id: DiskId,
    /// The partition this stream carries, when `kind` is `partition`.
    #[serde(default)]
    pub partition_id: Option<PartitionId>,
    /// Offset on the disk where byte zero of this stream belongs.
    pub target_offset: u64,
    /// Logical length of the stream.
    pub length: u64,
    /// How the bytes were read.
    pub capture: CaptureMethod,
    /// What the uncovered bytes are defined to contain.
    pub sparse_fill: SparseFill,
    /// Where the bytes came from, in words, for the operator and the log.
    pub source: String,
    /// The covered pieces, sorted ascending and never overlapping.
    pub segments: Vec<Segment>,
    /// What a used block capture measured, when this stream was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_blocks: Option<UsedBlockInfo>,
    /// Why this stream was captured whole when used block imaging was wanted.
    ///
    /// Present only when the fallback was taken, so that a backup never leaves
    /// the operator to infer from the size that something did not work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}

impl Stream {
    /// Total number of bytes actually captured by this stream.
    pub fn captured_bytes(&self) -> Result<u64, math::ArithError> {
        let mut total = 0u64;
        for s in &self.segments {
            total = math::add_u64("stream captured bytes", total, s.length)?;
        }
        Ok(total)
    }
}

/// One entry in the deduplicated chunk table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkEntry {
    /// BLAKE3 of the uncompressed contents.
    pub hash: ChunkHash,
    /// Size of the uncompressed contents.
    pub uncompressed_size: u32,
    /// Size of the stored file in bytes.
    pub compressed_size: u64,
}

/// Derived totals, recomputed and checked during validation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    /// Bytes actually captured, that is the sum of every segment length.
    pub captured_bytes: u64,
    /// Bytes the chunk files occupy.
    pub stored_bytes: u64,
    /// Number of distinct chunks.
    pub unique_chunks: u64,
    /// Number of segments across every stream.
    pub total_segments: u64,
    /// Number of segments that reused a chunk already referenced elsewhere.
    pub deduplicated_segments: u64,
}

/// `manifest.json`.
///
/// Unknown fields are accepted and ignored so a newer writer that only adds
/// optional information stays readable. A newer writer that adds something a
/// reader must act on raises `format.min_reader_minor` instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Format identity.
    pub format: FormatHeader,
    /// The tool that wrote this.
    pub tool: ToolInfo,
    /// Identity of this backup.
    pub backup: BackupInfo,
    /// The machine it came from.
    pub source: SourceInfo,
    /// Chunking parameters.
    pub chunking: ChunkingSpec,
    /// Compression parameters.
    pub compression: CompressionSpec,
    /// Digest parameters.
    pub hash: HashSpec,
    /// Chunk store layout.
    pub chunk_store: ChunkStoreSpec,
    /// What the shadow copy service did.
    pub vss: VssInfo,
    /// Filesystems found inside the captured partitions.
    pub volumes: Vec<VolumeEntry>,
    /// Every captured byte space.
    pub streams: Vec<Stream>,
    /// Deduplicated chunk table.
    pub chunks: Vec<ChunkEntry>,
    /// Smallest target disk, in bytes, that this backup can be restored to.
    pub required_restore_bytes: u64,
    /// Derived totals.
    pub stats: Stats,
}

impl Manifest {
    /// Looks up a chunk entry by index.
    pub fn chunk(&self, index: u32) -> Option<&ChunkEntry> {
        self.chunks.get(index as usize)
    }

    /// Looks up a stream by identifier.
    pub fn stream(&self, id: &StreamId) -> Option<&Stream> {
        self.streams.iter().find(|s| &s.id == id)
    }

    /// The stream carrying a given partition, if there is one.
    pub fn stream_for_partition(&self, id: &PartitionId) -> Option<&Stream> {
        self.streams
            .iter()
            .find(|s| s.kind == StreamKind::Partition && s.partition_id.as_ref() == Some(id))
    }

    /// Recomputes [`Stats`] from the rest of the manifest.
    pub fn recompute_stats(&self) -> Result<Stats, math::ArithError> {
        let mut stats = Stats::default();
        let mut seen_chunks: BTreeSet<u32> = BTreeSet::new();
        for stream in &self.streams {
            for seg in &stream.segments {
                stats.captured_bytes =
                    math::add_u64("captured bytes", stats.captured_bytes, seg.length)?;
                stats.total_segments = math::add_u64("segment count", stats.total_segments, 1)?;
                if !seen_chunks.insert(seg.chunk) {
                    stats.deduplicated_segments =
                        math::add_u64("deduplicated segments", stats.deduplicated_segments, 1)?;
                }
            }
        }
        for chunk in &self.chunks {
            stats.stored_bytes =
                math::add_u64("stored bytes", stats.stored_bytes, chunk.compressed_size)?;
            stats.unique_chunks = math::add_u64("chunk count", stats.unique_chunks, 1)?;
        }
        Ok(stats)
    }

    /// Checks the manifest on its own, without the other documents.
    ///
    /// Cross document checks, such as "every partition has a stream", live in
    /// [`crate::set::BackupSet`], because they need `disk-layout.json` too.
    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = self.format.check(DocumentKind::Manifest);
        self.validate_parameters(&mut issues);
        self.validate_chunk_table(&mut issues);
        self.validate_streams(&mut issues);
        self.validate_volumes(&mut issues);
        self.validate_stats(&mut issues);
        issues
    }

    fn validate_parameters(&self, issues: &mut Vec<Issue>) {
        let cs = self.chunking.chunk_size;
        if !(MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&cs) {
            issues.push(Issue::error(
                "chunking",
                format!(
                    "chunk size {cs} is outside the permitted range {MIN_CHUNK_SIZE}..={MAX_CHUNK_SIZE}"
                ),
            ));
        }
        if cs % 4096 != 0 {
            issues.push(Issue::error(
                "chunking",
                format!("chunk size {cs} is not a multiple of 4096"),
            ));
        }
        if self.hash.digest_bytes != crate::hash::DIGEST_BYTES as u32 {
            issues.push(Issue::error(
                "hash",
                format!(
                    "digest length {} is not the {} bytes BLAKE3 produces",
                    self.hash.digest_bytes,
                    crate::hash::DIGEST_BYTES
                ),
            ));
        }
        if self.compression.algorithm == CompressionAlgorithm::Zstd
            && !(-7..=22).contains(&self.compression.level)
        {
            issues.push(Issue::error(
                "compression",
                format!(
                    "zstd level {} is outside the range -7..=22",
                    self.compression.level
                ),
            ));
        }
        if !is_safe_component(&self.chunk_store.root) {
            issues.push(Issue::error(
                "chunk-store",
                format!(
                    "root {:?} is not a safe single directory name",
                    self.chunk_store.root
                ),
            ));
        }
        if self.chunk_store.fanout > 4 {
            issues.push(Issue::error(
                "chunk-store",
                format!("fanout {} is larger than 4", self.chunk_store.fanout),
            ));
        }
        if !crate::is_guid(&self.backup.uuid) {
            issues.push(Issue::error(
                "backup",
                format!("uuid {:?} is not a GUID", self.backup.uuid),
            ));
        }
        if self.backup.created_utc.is_empty() {
            issues.push(Issue::error("backup", "created_utc is empty"));
        }
    }

    fn validate_chunk_table(&self, issues: &mut Vec<Issue>) {
        if self.chunks.len() > u32::MAX as usize {
            issues.push(Issue::error(
                "chunk table",
                "holds more chunks than a 32 bit index can address",
            ));
        }
        let mut seen: std::collections::BTreeMap<ChunkHash, usize> =
            std::collections::BTreeMap::new();
        for (i, chunk) in self.chunks.iter().enumerate() {
            let object = format!("chunk {i} ({})", chunk.hash);
            if chunk.uncompressed_size == 0 {
                issues.push(Issue::error(&object, "uncompressed size is zero"));
            }
            if chunk.uncompressed_size > self.chunking.chunk_size {
                issues.push(Issue::error(
                    &object,
                    format!(
                        "uncompressed size {} exceeds the chunk size {}",
                        chunk.uncompressed_size, self.chunking.chunk_size
                    ),
                ));
            }
            if chunk.compressed_size == 0 {
                issues.push(Issue::error(&object, "stored size is zero"));
            }
            if let Some(first) = seen.insert(chunk.hash, i) {
                issues.push(Issue::error(
                    &object,
                    format!(
                        "duplicates chunk {first}; a content addressed table must list each digest once"
                    ),
                ));
            }
        }
    }

    fn validate_streams(&self, issues: &mut Vec<Issue>) {
        let mut ids: BTreeSet<&StreamId> = BTreeSet::new();
        for stream in &self.streams {
            let object = format!("stream {:?}", stream.id.as_str());
            if !ids.insert(&stream.id) {
                issues.push(Issue::error(&object, "duplicate stream identifier"));
            }
            if stream.length == 0 {
                issues.push(Issue::error(&object, "length is zero"));
            }
            match stream.kind {
                StreamKind::Partition if stream.partition_id.is_none() => {
                    issues.push(Issue::error(
                        &object,
                        "is a partition stream but names no partition",
                    ));
                }
                StreamKind::DiskHead | StreamKind::DiskTail if stream.partition_id.is_some() => {
                    issues.push(Issue::error(
                        &object,
                        "a disk head or tail stream must not name a partition",
                    ));
                }
                _ => {}
            }
            if !stream.capture.may_be_sparse() {
                // A full capture must leave no gap, otherwise the label is a
                // lie and a restore would silently zero part of a filesystem.
                match stream.captured_bytes() {
                    Ok(captured) if captured != stream.length => issues.push(Issue::error(
                        &object,
                        format!(
                            "claims a complete capture but covers only {captured} of {} bytes",
                            stream.length
                        ),
                    )),
                    Ok(_) => {}
                    Err(e) => issues.push(Issue::error(&object, format!("{e}"))),
                }
            }
            self.validate_segments(stream, &object, issues);
        }
    }

    fn validate_segments(&self, stream: &Stream, object: &str, issues: &mut Vec<Issue>) {
        let mut previous_end: Option<u64> = None;
        for (i, seg) in stream.segments.iter().enumerate() {
            let seg_object = format!("{object} segment {i}");

            if seg.length == 0 {
                issues.push(Issue::error(&seg_object, "length is zero"));
                continue;
            }

            // Bounds first: everything below assumes offset + length is sane.
            let end = match math::range_end("segment end", seg.offset, seg.length) {
                Ok(end) => end,
                Err(e) => {
                    issues.push(Issue::error(&seg_object, format!("{e}")));
                    continue;
                }
            };
            if end > stream.length {
                issues.push(Issue::error(
                    &seg_object,
                    format!(
                        "covers bytes {}..{end} but the stream is only {} bytes long",
                        seg.offset, stream.length
                    ),
                ));
            }
            if let Some(prev) = previous_end {
                if seg.offset < prev {
                    issues.push(Issue::error(
                        &seg_object,
                        format!(
                            "starts at {} which is before the previous segment ends at {prev}; segments must be sorted and must not overlap",
                            seg.offset
                        ),
                    ));
                }
            }
            previous_end = Some(end);

            match self.chunk(seg.chunk) {
                None => issues.push(Issue::error(
                    &seg_object,
                    format!(
                        "refers to chunk {} but the table holds {} entries",
                        seg.chunk,
                        self.chunks.len()
                    ),
                )),
                Some(chunk) => {
                    if u64::from(chunk.uncompressed_size) != seg.length {
                        issues.push(Issue::error(
                            &seg_object,
                            format!(
                                "is {} bytes but chunk {} holds {} bytes",
                                seg.length, seg.chunk, chunk.uncompressed_size
                            ),
                        ));
                    }
                }
            }
        }
    }

    fn validate_volumes(&self, issues: &mut Vec<Issue>) {
        let mut ids: BTreeSet<&VolumeId> = BTreeSet::new();
        for volume in &self.volumes {
            let object = format!("volume {:?}", volume.id.as_str());
            if !ids.insert(&volume.id) {
                issues.push(Issue::error(&object, "duplicate volume identifier"));
            }
            if self.stream_for_partition(&volume.partition_id).is_none() {
                issues.push(Issue::error(
                    &object,
                    format!(
                        "sits on partition {:?}, which no stream carries",
                        volume.partition_id.as_str()
                    ),
                ));
            }
            if let Some(path) = &volume.index_path {
                if !is_safe_relative_path(path) {
                    issues.push(Issue::error(
                        &object,
                        format!("index path {path:?} is not a safe relative path"),
                    ));
                }
            }
            if let (Some(total), Some(used)) = (volume.total_bytes, volume.used_bytes) {
                if used > total {
                    issues.push(Issue::error(
                        &object,
                        format!("reports {used} bytes used of a {total} byte filesystem"),
                    ));
                }
            }
        }
    }

    fn validate_stats(&self, issues: &mut Vec<Issue>) {
        match self.recompute_stats() {
            Err(e) => issues.push(Issue::error("stats", format!("cannot be recomputed: {e}"))),
            Ok(expected) if expected != self.stats => issues.push(Issue::error(
                "stats",
                format!(
                    "do not match the manifest contents: recorded {:?}, computed {:?}",
                    self.stats, expected
                ),
            )),
            Ok(_) => {}
        }
    }
}

/// Whether `name` is safe to use as a single directory component.
pub(crate) fn is_safe_component(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Whether `path` is a safe relative path inside the backup folder.
///
/// Forward slashes only, no drive letters, no absolute paths, and no segment
/// that could climb out of the folder.
pub(crate) fn is_safe_relative_path(path: &str) -> bool {
    if path.is_empty() || path.len() > 256 || path.contains('\\') || path.contains(':') {
        return false;
    }
    if path.starts_with('/') {
        return false;
    }
    path.split('/').all(|seg| {
        !seg.is_empty()
            && seg != "."
            && seg != ".."
            && seg.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_' || b == b'.'
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_component_rejects_traversal() {
        assert!(is_safe_component("chunks"));
        assert!(!is_safe_component(".."));
        assert!(!is_safe_component("."));
        assert!(!is_safe_component(""));
        assert!(!is_safe_component("a/b"));
        assert!(!is_safe_component("a\\b"));
        assert!(!is_safe_component("C:"));
        assert!(!is_safe_component("Chunks"));
    }

    #[test]
    fn safe_relative_path_rejects_escapes() {
        assert!(is_safe_relative_path("indexes/volume-0.json"));
        assert!(!is_safe_relative_path("../secrets"));
        assert!(!is_safe_relative_path("/etc/passwd"));
        assert!(!is_safe_relative_path("C:/windows"));
        assert!(!is_safe_relative_path("indexes\\volume-0.json"));
        assert!(!is_safe_relative_path("indexes/../../x"));
        assert!(!is_safe_relative_path(""));
        assert!(!is_safe_relative_path("indexes//x"));
    }

    #[test]
    fn default_specs_are_self_consistent() {
        let chunking = ChunkingSpec::default();
        assert!(chunking.chunk_size >= MIN_CHUNK_SIZE);
        assert!(chunking.chunk_size <= MAX_CHUNK_SIZE);
        assert_eq!(chunking.chunk_size % 4096, 0);
        assert_eq!(
            HashSpec::default().digest_bytes,
            crate::hash::DIGEST_BYTES as u32
        );
        assert!(is_safe_component(&ChunkStoreSpec::default().root));
    }
}
