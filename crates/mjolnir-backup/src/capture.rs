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
use mjolnir_core::ids::{DiskId, PartitionId, StreamId};
use mjolnir_core::math;
use mjolnir_core::progress::Progress;
use mjolnir_image::disk_layout::DiskEntry;
use mjolnir_image::manifest::{CaptureMethod, StreamKind};
use mjolnir_image::writer::BackupWriter;

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

        // Either a consistent source for this partition, or the disk itself at
        // the partition's offset.
        let (mut source, source_offset) = match sources.open_partition(index)? {
            Some(source) => (source, 0u64),
            None => (sources.open_disk()?, partition.starting_offset),
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

        copy_range(
            source.as_mut(),
            source_offset,
            capture.planned_bytes,
            &mut stream,
            progress,
            cancel,
        )?;
        stream.finish()?;
    }

    capture_table(spec, sources, writer, progress, cancel)
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
