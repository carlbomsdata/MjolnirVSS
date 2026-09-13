//! Copying a disk into a backup set.
//!
//! This is the part of a backup that actually moves bytes, and it is written
//! against [`BlockSource`] rather than against a Windows handle. Where those
//! bytes come from is decided by whoever implements [`CaptureSources`]: on a
//! real machine that is a shadow copy device for the NTFS volumes and the
//! physical disk for everything else, and in a test it is a synthetic disk in
//! memory.
//!
//! The seam exists so that the logic deciding what to read, how to cut it up
//! and where to record it is exercised by ordinary tests, instead of only ever
//! running against the one computer a developer happens to have.

use mjolnir_core::blockio::BlockSource;
use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::{Error, Result};
use mjolnir_core::extents::ByteRange;
use mjolnir_core::ids::{DiskId, PartitionId, StreamId};
use mjolnir_core::math;
use mjolnir_core::progress::Progress;
use mjolnir_image::disk_layout::DiskEntry;
use mjolnir_image::manifest::{CaptureMethod, StreamKind};
use mjolnir_image::writer::BackupWriter;
use mjolnir_ntfs::bitmap::UsedBlockPlan;

/// Where the bytes of a capture come from.
pub trait CaptureSources {
    /// Opens the physical disk, for the partition table and any partition that
    /// is read directly.
    fn open_disk(&mut self) -> Result<Box<dyn BlockSource>>;

    /// Opens the consistent source for one partition, by index.
    ///
    /// Returning `None` means "read this one from the disk instead", which is
    /// what happens for the EFI system partition and the Microsoft Reserved
    /// partition, neither of which the shadow copy service handles.
    fn open_partition(&mut self, index: usize) -> Result<Option<Box<dyn BlockSource>>>;

    /// Which parts of a partition hold data, when that can be established.
    ///
    /// Returning `None` means "copy the whole thing", which is always correct
    /// and is what happens for a filesystem this version does not understand.
    /// An error means the same thing: used block imaging is an optimisation of
    /// a correct operation, so failing to plan one falls back rather than
    /// failing the backup. Either way the reason is recorded in the manifest.
    ///
    /// The default implementation declines, so a test source only has to
    /// implement it when it is testing this.
    fn used_blocks(&mut self, _index: usize) -> Result<Option<UsedBlockPlan>> {
        Ok(None)
    }
}

/// How one partition is to be captured.
#[derive(Debug, Clone)]
pub struct PartitionCapture {
    /// The partition this describes, as recorded in `disk-layout.json`.
    pub partition_id: PartitionId,
    /// The stream that will carry its contents.
    pub stream_id: StreamId,
    /// How the contents are being read.
    pub capture: CaptureMethod,
    /// How many bytes to read, starting at the beginning of the partition.
    ///
    /// Equal to the partition length for a real backup, and smaller for a
    /// preview run.
    pub planned_bytes: u64,
    /// Where the bytes came from, in words, for the operator and the log.
    pub source_description: String,
}

/// Everything a capture is going to copy.
#[derive(Debug, Clone)]
pub struct CaptureSpec {
    /// Identifier of the disk within the backup.
    pub disk_id: DiskId,
    /// The disk as it will be recorded.
    pub disk: DiskEntry,
    /// Bytes at the start of the disk holding the protective MBR and GPT.
    pub head_bytes: u64,
    /// Bytes at the end of the disk holding the secondary GPT.
    pub tail_bytes: u64,
    /// One entry per partition, in the same order as `disk.partitions`.
    pub partitions: Vec<PartitionCapture>,
}

impl CaptureSpec {
    /// The identifier used for the disk head stream.
    pub fn head_stream_id(&self) -> StreamId {
        StreamId::new(format!("{}-head", self.disk_id.as_str())).expect("built from a valid id")
    }

    /// The identifier used for the disk tail stream.
    pub fn tail_stream_id(&self) -> StreamId {
        StreamId::new(format!("{}-tail", self.disk_id.as_str())).expect("built from a valid id")
    }

    /// Total bytes that will be read.
    pub fn total_bytes(&self) -> Result<u64> {
        let mut total = math::add_u64("capture total", self.head_bytes, self.tail_bytes)?;
        for p in &self.partitions {
            total = math::add_u64("capture total", total, p.planned_bytes)?;
        }
        Ok(total)
    }
}

/// Copies a disk into `writer`, following `spec`.
///
/// Records the disk layout, every partition, and the two regions holding the
/// partition table. Never a subset: if a partition cannot be read the whole
/// capture fails, because a backup missing a partition is worse than no backup
/// at all.
pub fn capture_disk(
    spec: &CaptureSpec,
    sources: &mut dyn CaptureSources,
    writer: &mut BackupWriter,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<()> {
    if spec.partitions.len() != spec.disk.partitions.len() {
        return Err(Error::new(
            mjolnir_core::ExitCode::Failure,
            "the capture plan does not match the disk layout",
            format!(
                "the plan covers {} partitions and the disk has {}",
                spec.partitions.len(),
                spec.disk.partitions.len()
            ),
            "this is an internal error; please report it with the command you ran",
        ));
    }

    writer.set_disks(vec![spec.disk.clone()]);

    for (index, capture) in spec.partitions.iter().enumerate() {
        cancel.check()?;
        let partition = &spec.disk.partitions[index];

        // Asked for before the source is opened, because planning needs the
        // sources by mutable reference and the open source borrows them for as
        // long as it lives.
        let used = if capture.capture == CaptureMethod::VssUsedBlocks {
            plan_used_blocks(index, sources)
        } else {
            UsedBlockDecision::NotWanted
        };

        // Either a consistent source for this partition, or the disk itself at
        // the partition's offset.
        let snapshot = sources.open_partition(index)?;
        let reading_a_snapshot = snapshot.is_some();
        let (mut source, source_offset) = match snapshot {
            Some(source) => (source, 0u64),
            None => (sources.open_disk()?, partition.starting_offset),
        };

        // A shadow copy device is as long as the *volume*, and a volume is a
        // little shorter than the partition holding it: NTFS keeps a spare copy
        // of its boot sector in the last sector of the partition, outside the
        // filesystem. Reading that far through the shadow copy fails with "the
        // end of the file", so the tail comes from the disk instead. Windows
        // does not write to it while it is running, which is the same argument
        // the boot partitions are captured under.
        let snapshot_covers = if reading_a_snapshot {
            source.size_bytes()
        } else {
            u64::MAX
        };
        let mut disk_for_tail = if reading_a_snapshot && snapshot_covers < capture.planned_bytes {
            Some(sources.open_disk()?)
        } else {
            None
        };

        let mut stream = writer.begin_stream(
            capture.stream_id.clone(),
            StreamKind::Partition,
            spec.disk_id.clone(),
            Some(capture.partition_id.clone()),
            partition.starting_offset,
            partition.length,
            capture.capture,
            capture.source_description.clone(),
        );

        let whole = [ByteRange::new(0, capture.planned_bytes)];
        match used {
            UsedBlockDecision::Use(plan) => {
                stream.set_used_blocks(used_block_info(&plan)?);
                copy_ranges(
                    source.as_mut(),
                    source_offset,
                    &mut disk_for_tail,
                    partition.starting_offset,
                    snapshot_covers,
                    plan.extents.ranges(),
                    &mut stream,
                    progress,
                    cancel,
                )?;
                // The planned figure counted the whole partition, so the
                // progress total has to be told about the bytes nobody read.
                let captured = plan.captured_bytes()?;
                if captured < capture.planned_bytes {
                    progress.skipped(capture.planned_bytes - captured);
                }
            }
            UsedBlockDecision::FallBack(reason) => {
                stream.fall_back_to(CaptureMethod::VssRaw, reason);
                copy_ranges(
                    source.as_mut(),
                    source_offset,
                    &mut disk_for_tail,
                    partition.starting_offset,
                    snapshot_covers,
                    &whole,
                    &mut stream,
                    progress,
                    cancel,
                )?;
            }
            UsedBlockDecision::NotWanted => {
                copy_ranges(
                    source.as_mut(),
                    source_offset,
                    &mut disk_for_tail,
                    partition.starting_offset,
                    snapshot_covers,
                    &whole,
                    &mut stream,
                    progress,
                    cancel,
                )?;
            }
        }
        stream.finish()?;
    }

    capture_table(spec, sources, writer, progress, cancel)
}

/// What the capture decided to do about used block imaging for one partition.
enum UsedBlockDecision {
    /// Read only these ranges.
    Use(Box<UsedBlockPlan>),
    /// Read the whole partition, for this reason.
    FallBack(String),
    /// Used block imaging was never asked for.
    NotWanted,
}

/// Asks the sources what is in use, turning any answer into a decision.
///
/// A failure here is deliberately not a failure of the backup. Used block
/// imaging makes a correct operation faster; when it cannot be planned, the
/// correct operation still happens, and the manifest says why it had to.
fn plan_used_blocks(index: usize, sources: &mut dyn CaptureSources) -> UsedBlockDecision {
    match sources.used_blocks(index) {
        Ok(Some(plan)) => UsedBlockDecision::Use(Box::new(plan)),
        Ok(None) => UsedBlockDecision::FallBack(
            "the volume did not report which of its clusters are in use".to_owned(),
        ),
        Err(e) => UsedBlockDecision::FallBack(format!("{}: {}", e.what(), e.why())),
    }
}

/// Turns a plan into the figures the manifest records.
fn used_block_info(plan: &UsedBlockPlan) -> Result<mjolnir_image::manifest::UsedBlockInfo> {
    Ok(mjolnir_image::manifest::UsedBlockInfo {
        cluster_size: plan.cluster_size,
        clusters_total: plan.clusters_total,
        clusters_allocated: plan.clusters_allocated,
        bitmap_bytes: plan.described_bytes,
        undescribed_tail_bytes: plan.undescribed_tail_bytes,
        reserved_bytes: plan.reserved_bytes,
        extent_count: plan.extent_count() as u64,
    })
}

/// Copies a set of ranges, leaving everything between them uncaptured.
///
/// `ranges` are offsets within the stream. They are required to be ascending
/// and non overlapping, which is what [`ExtentList`](mjolnir_core::extents::ExtentList)
/// guarantees, and the stream writer checks again on every segment.
///
/// Two sources, because one of them may not reach the end of the partition. A
/// shadow copy device is as long as the volume, and the last sector of the
/// partition is outside the volume. Anything at or past `primary_covers` is
/// read from `tail` instead, at `partition_offset` plus the offset within the
/// stream.
#[allow(clippy::too_many_arguments)]
fn copy_ranges(
    primary: &mut dyn BlockSource,
    primary_offset: u64,
    tail: &mut Option<Box<dyn BlockSource>>,
    partition_offset: u64,
    primary_covers: u64,
    ranges: &[ByteRange],
    stream: &mut mjolnir_image::writer::StreamWriter<'_>,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<()> {
    let chunk_size = u64::from(mjolnir_image::manifest::DEFAULT_CHUNK_SIZE);
    let mut buffer = vec![0u8; math::to_usize("chunk buffer", chunk_size)?];

    for range in ranges {
        let end = range.end()?;
        let mut at = range.offset;
        while at < end {
            cancel.check()?;
            // Cut on multiples of the chunk size measured from the start of the
            // stream, so the same region produces the same chunk boundaries in
            // every backup regardless of where a range happens to begin.
            let to_boundary = chunk_size - (at % chunk_size);
            let mut want = to_boundary.min(end - at);

            // A piece must not straddle the point where the source changes.
            if at < primary_covers && at + want > primary_covers {
                want = primary_covers - at;
            }
            let slice = &mut buffer[..math::to_usize("chunk length", want)?];

            if at < primary_covers {
                let from = math::add_u64("copy offset", primary_offset, at)?;
                primary.read_exact_at(from, slice)?;
            } else {
                let Some(tail) = tail.as_deref_mut() else {
                    return Err(Error::new(
                        mjolnir_core::ExitCode::Failure,
                        "a partition is longer than the thing it is being read from",
                        format!(
                            "byte {at} of the partition is past the {primary_covers} bytes the source covers, and no disk was opened to read the rest"
                        ),
                        "this is an internal error; please report it with the command you ran",
                    ));
                };
                let from = math::add_u64("copy offset", partition_offset, at)?;
                tail.read_exact_at(from, slice)?;
            }

            stream.write_segment(at, slice)?;
            progress.advance(want);
            at = math::add_u64("copy cursor", at, want)?;
        }
    }
    Ok(())
}

/// Copies the two regions holding the partition table.
///
/// Always captured in full, even during a preview run: they are small, and a
/// backup that does not describe its own disk describes nothing.
fn capture_table(
    spec: &CaptureSpec,
    sources: &mut dyn CaptureSources,
    writer: &mut BackupWriter,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<()> {
    let mut device = sources.open_disk()?;

    let mut head = writer.begin_stream(
        spec.head_stream_id(),
        StreamKind::DiskHead,
        spec.disk_id.clone(),
        None,
        0,
        spec.head_bytes,
        CaptureMethod::RawFull,
        "protective MBR and primary partition table".to_owned(),
    );
    copy_range(
        device.as_mut(),
        0,
        spec.head_bytes,
        &mut head,
        progress,
        cancel,
    )?;
    head.finish()?;

    let tail_offset = spec
        .disk
        .size_bytes
        .checked_sub(spec.tail_bytes)
        .ok_or_else(|| {
            Error::unsupported(
                "the disk is too small to hold a partition table",
                "the disk is smaller than the space a GUID partition table needs at its end",
                "this disk cannot be backed up by this version",
            )
        })?;

    let mut tail = writer.begin_stream(
        spec.tail_stream_id(),
        StreamKind::DiskTail,
        spec.disk_id.clone(),
        None,
        tail_offset,
        spec.tail_bytes,
        CaptureMethod::RawFull,
        "secondary partition table".to_owned(),
    );
    copy_range(
        device.as_mut(),
        tail_offset,
        spec.tail_bytes,
        &mut tail,
        progress,
        cancel,
    )?;
    tail.finish()
}

/// Copies `length` bytes from `source` at `source_offset` into `stream`.
///
/// Cancellation is checked once per chunk, which bounds how long Cancel takes
/// to be noticed to the time it takes to read and compress one chunk.
pub fn copy_range(
    source: &mut dyn BlockSource,
    source_offset: u64,
    length: u64,
    stream: &mut mjolnir_image::writer::StreamWriter<'_>,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<()> {
    if length == 0 {
        return Ok(());
    }
    let chunk_size = u64::from(mjolnir_image::manifest::DEFAULT_CHUNK_SIZE);
    let mut buffer = vec![0u8; math::to_usize("chunk buffer", chunk_size)?];
    let mut done = 0u64;

    while done < length {
        cancel.check()?;
        let want = chunk_size.min(length - done);
        let slice = &mut buffer[..math::to_usize("chunk length", want)?];

        let at = math::add_u64("copy offset", source_offset, done)?;
        source.read_exact_at(at, slice)?;

        stream.write_segment(done, slice)?;
        done = math::add_u64("copy cursor", done, want)?;
        progress.advance(want);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mjolnir_core::blockio::MemoryBlockDevice;
    use mjolnir_image::disk_layout::{BusType, PartitionEntry, PartitionRole, PartitionStyle};

    struct MemorySources {
        disk: MemoryBlockDevice,
    }

    impl CaptureSources for MemorySources {
        fn open_disk(&mut self) -> Result<Box<dyn BlockSource>> {
            Ok(Box::new(self.disk.clone()))
        }

        fn open_partition(&mut self, _index: usize) -> Result<Option<Box<dyn BlockSource>>> {
            Ok(None)
        }
    }

    fn spec_for(size: u64, partitions: &[(u64, u64)]) -> CaptureSpec {
        let entries: Vec<PartitionEntry> = partitions
            .iter()
            .enumerate()
            .map(|(i, (offset, length))| PartitionEntry {
                id: PartitionId::new(format!("p-{}", i + 1)).unwrap(),
                number: i as u32 + 1,
                type_guid: mjolnir_image::disk_layout::GUID_BASIC_DATA.to_owned(),
                unique_guid: "11111111-2222-3333-4444-555555555555".to_owned(),
                name: String::new(),
                starting_offset: *offset,
                length: *length,
                attributes: 0,
                role: PartitionRole::Data,
                filesystem: None,
            })
            .collect();

        let captures: Vec<PartitionCapture> = entries
            .iter()
            .map(|e| PartitionCapture {
                partition_id: e.id.clone(),
                stream_id: StreamId::new(format!("s-{}", e.number)).unwrap(),
                capture: CaptureMethod::RawFull,
                planned_bytes: e.length,
                source_description: "test".to_owned(),
            })
            .collect();

        CaptureSpec {
            disk_id: DiskId::new("disk-0").unwrap(),
            disk: DiskEntry {
                id: DiskId::new("disk-0").unwrap(),
                disk_number: 0,
                size_bytes: size,
                logical_sector_size: 512,
                physical_sector_size: 512,
                partition_style: PartitionStyle::Gpt,
                disk_guid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
                model: Some("Test".to_owned()),
                serial: Some("SER".to_owned()),
                bus_type: BusType::Virtual,
                partitions: entries,
            },
            head_bytes: 1 << 20,
            tail_bytes: 64 * 1024,
            partitions: captures,
        }
    }

    /// Sources whose snapshot of a partition is shorter than the partition, the
    /// way a real shadow copy is.
    ///
    /// A shadow copy device covers the volume, and NTFS keeps a spare copy of
    /// its boot sector in the last sector of the partition, outside the volume.
    /// Reading that far through the shadow copy fails, which is what happened
    /// the first time a backup ran against a real Windows machine.
    struct ShortSnapshotSources {
        disk: MemoryBlockDevice,
        /// Where the partition begins on the disk.
        partition_offset: u64,
        /// How much of it the snapshot covers.
        covers: u64,
    }

    impl CaptureSources for ShortSnapshotSources {
        fn open_disk(&mut self) -> Result<Box<dyn BlockSource>> {
            Ok(Box::new(self.disk.clone()))
        }

        fn open_partition(&mut self, _index: usize) -> Result<Option<Box<dyn BlockSource>>> {
            // The snapshot holds the same bytes as the partition, but stops
            // short of its end.
            let mut bytes = vec![0u8; self.covers as usize];
            self.disk.read_exact_at(self.partition_offset, &mut bytes)?;
            Ok(Some(Box::new(MemoryBlockDevice::from_vec(
                "snapshot", bytes, 512,
            ))))
        }
    }

    /// The regression test for the failure a real machine produced: a capture
    /// must reach the end of the partition even when the snapshot does not.
    #[test]
    fn a_snapshot_shorter_than_its_partition_still_captures_the_last_sector() {
        let partition_offset = 1u64 << 20;
        let partition_length = 4u64 << 20;
        let covers = partition_length - 512;

        // Recognisable bytes at the very end of the partition, which only the
        // disk can supply.
        let mut disk = MemoryBlockDevice::zeroed("disk", 16 << 20, 512);
        let mut spare = vec![0u8; 512];
        spare[..8].copy_from_slice(b"SPAREBOO");
        spare[510] = 0x55;
        spare[511] = 0xAA;
        let at = (partition_offset + partition_length - 512) as usize;
        disk.bytes_mut()[at..at + 512].copy_from_slice(&spare);

        let mut spec = spec_for(16 << 20, &[(partition_offset, partition_length)]);
        spec.partitions[0].capture = CaptureMethod::VssRaw;

        let tmp = tempfile::tempdir().unwrap();
        let mut writer = test_writer(tmp.path());
        let mut sources = ShortSnapshotSources {
            disk,
            partition_offset,
            covers,
        };
        let mut progress = mjolnir_core::progress::SilentProgress;

        capture_disk(
            &spec,
            &mut sources,
            &mut writer,
            &mut progress,
            &CancelToken::new(),
        )
        .expect("the capture should reach the end of the partition");

        let finalized = writer.finalize().unwrap();
        let manifest = finalized.manifest();
        let stream = manifest
            .streams
            .iter()
            .find(|s| s.id.as_str() == "s-1")
            .expect("the partition stream");

        assert_eq!(
            stream.captured_bytes().unwrap(),
            partition_length,
            "the whole partition should have been captured"
        );

        // And the bytes only the disk could supply really are in there.
        let store = finalized.chunk_store();
        let last = stream.segments.last().expect("a last segment");
        let chunk = &manifest.chunks[last.chunk as usize];
        let data = store.get(chunk.hash, chunk.uncompressed_size).unwrap();
        assert_eq!(
            &data[data.len() - 512..data.len() - 504],
            b"SPAREBOO",
            "the spare boot sector did not come from the disk"
        );
    }

    /// The same, for a used block capture: the tail is one of the ranges the
    /// plan deliberately includes, and it has to come from the disk too.
    #[test]
    fn used_block_capture_reads_its_tail_from_the_disk() {
        let partition_offset = 1u64 << 20;
        let partition_length = 4u64 << 20;
        let covers = partition_length - 512;

        let mut disk = MemoryBlockDevice::zeroed("disk", 16 << 20, 512);
        let mut spare = vec![0u8; 512];
        spare[..8].copy_from_slice(b"SPAREBOO");
        let at = (partition_offset + partition_length - 512) as usize;
        disk.bytes_mut()[at..at + 512].copy_from_slice(&spare);

        let mut spec = spec_for(16 << 20, &[(partition_offset, partition_length)]);
        spec.partitions[0].capture = CaptureMethod::VssUsedBlocks;

        struct WithPlan {
            inner: ShortSnapshotSources,
            plan: mjolnir_ntfs::bitmap::UsedBlockPlan,
        }
        impl CaptureSources for WithPlan {
            fn open_disk(&mut self) -> Result<Box<dyn BlockSource>> {
                self.inner.open_disk()
            }
            fn open_partition(&mut self, i: usize) -> Result<Option<Box<dyn BlockSource>>> {
                self.inner.open_partition(i)
            }
            fn used_blocks(&mut self, _i: usize) -> Result<Option<UsedBlockPlan>> {
                Ok(Some(self.plan.clone()))
            }
        }

        // Two ranges: something near the front, and the last sector.
        let extents = mjolnir_core::extents::ExtentList::from_unsorted(vec![
            ByteRange::new(0, 65536),
            ByteRange::new(partition_length - 512, 512),
        ])
        .unwrap();
        let plan = mjolnir_ntfs::bitmap::UsedBlockPlan {
            extents,
            cluster_size: 4096,
            clusters_total: partition_length / 4096,
            clusters_allocated: 16,
            described_bytes: partition_length - 4096,
            undescribed_tail_bytes: 512,
            reserved_bytes: 512,
        };

        let tmp = tempfile::tempdir().unwrap();
        let mut writer = test_writer(tmp.path());
        let mut sources = WithPlan {
            inner: ShortSnapshotSources {
                disk,
                partition_offset,
                covers,
            },
            plan,
        };
        let mut progress = mjolnir_core::progress::SilentProgress;

        capture_disk(
            &spec,
            &mut sources,
            &mut writer,
            &mut progress,
            &CancelToken::new(),
        )
        .expect("the capture should reach the end of the partition");

        let finalized = writer.finalize().unwrap();
        let manifest = finalized.manifest();
        let stream = manifest
            .streams
            .iter()
            .find(|s| s.id.as_str() == "s-1")
            .unwrap();

        assert_eq!(stream.capture, CaptureMethod::VssUsedBlocks);
        assert_eq!(stream.captured_bytes().unwrap(), 65536 + 512);

        let store = finalized.chunk_store();
        let last = stream.segments.last().unwrap();
        assert_eq!(last.offset, partition_length - 512);
        let chunk = &manifest.chunks[last.chunk as usize];
        let data = store.get(chunk.hash, chunk.uncompressed_size).unwrap();
        assert_eq!(&data[..8], b"SPAREBOO");
    }

    #[test]
    fn total_bytes_adds_up_the_whole_capture() {
        let spec = spec_for(16 << 20, &[(1 << 20, 4 << 20), (8 << 20, 4 << 20)]);
        assert_eq!(
            spec.total_bytes().unwrap(),
            (1 << 20) + 64 * 1024 + (4 << 20) + (4 << 20)
        );
    }

    #[test]
    fn stream_identifiers_are_derived_from_the_disk() {
        let spec = spec_for(16 << 20, &[]);
        assert_eq!(spec.head_stream_id().as_str(), "disk-0-head");
        assert_eq!(spec.tail_stream_id().as_str(), "disk-0-tail");
    }

    #[test]
    fn a_plan_that_disagrees_with_the_layout_is_refused() {
        let mut spec = spec_for(16 << 20, &[(1 << 20, 4 << 20)]);
        spec.partitions.clear();

        let tmp = tempfile::tempdir().unwrap();
        let mut writer = test_writer(tmp.path());
        let mut sources = MemorySources {
            disk: MemoryBlockDevice::zeroed("disk", 16 << 20, 512),
        };
        let mut progress = mjolnir_core::progress::SilentProgress;

        let err = capture_disk(
            &spec,
            &mut sources,
            &mut writer,
            &mut progress,
            &CancelToken::new(),
        )
        .unwrap_err();
        assert!(err.what().contains("does not match the disk layout"));
    }

    #[test]
    fn cancelling_stops_the_copy() {
        let spec = spec_for(16 << 20, &[(1 << 20, 8 << 20)]);
        let tmp = tempfile::tempdir().unwrap();
        let mut writer = test_writer(tmp.path());
        let mut sources = MemorySources {
            disk: MemoryBlockDevice::zeroed("disk", 16 << 20, 512),
        };
        let mut progress = mjolnir_core::progress::SilentProgress;

        let cancel = CancelToken::new();
        cancel.cancel();

        let err =
            capture_disk(&spec, &mut sources, &mut writer, &mut progress, &cancel).unwrap_err();
        assert_eq!(err.exit(), mjolnir_core::ExitCode::Cancelled);
    }

    fn test_writer(dir: &std::path::Path) -> BackupWriter {
        use mjolnir_image::manifest::{BackupInfo, BackupKind, FirmwareMode, SourceInfo};
        BackupWriter::create(
            dir,
            &mjolnir_core::ids::BackupName::new("TEST_2026-01-01_0000").unwrap(),
            BackupInfo {
                uuid: "99999999-8888-7777-6666-555555555555".to_owned(),
                name: mjolnir_core::ids::BackupName::new("TEST_2026-01-01_0000").unwrap(),
                created_utc: "2026-01-01T00:00:00Z".to_owned(),
                kind: BackupKind::Full,
                scope: "system-disk".to_owned(),
            },
            SourceInfo {
                machine_id: mjolnir_core::ids::MachineId::new("test-pc").unwrap(),
                computer_name: "TEST-PC".to_owned(),
                windows: Default::default(),
                firmware: FirmwareMode::Uefi,
            },
            Default::default(),
        )
        .unwrap()
    }
}
