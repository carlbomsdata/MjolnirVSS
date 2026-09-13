//! Writing a backup back onto a disk.
//!
//! Everything is checked before the first byte is written. By the time this
//! module writes anything it has already established that the backup is
//! complete and verified, that every chunk it will need exists, that the target
//! is large enough, that the sector sizes agree, that no two writes overlap,
//! and that the operator typed the target's serial number.
//!
//! The order is: partitions first, then the partition table. A disk whose
//! contents are written but whose table is missing is obviously broken and will
//! not be booted by mistake; a disk with a table pointing at partitions that
//! were never written looks fine and is not.

use mjolnir_core::blockio::BlockSink;
use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::{Error, Result};
use mjolnir_core::math;
use mjolnir_core::progress::Progress;
use mjolnir_image::disk_layout::DiskEntry;
use mjolnir_image::manifest::StreamKind;
use mjolnir_image::BackupSet;
use mjolnir_storage::gpt::{build_gpt, GptBuildRequest, GptPartitionEntry};

use crate::target::{check_target, EraseConfirmation, TargetDisk};

/// Plain language stages, shown to the operator.
pub mod stages {
    /// Checking the backup and the target before anything is written.
    pub const CHECKING: &str = "Checking the backup";
    /// Writing partition contents.
    pub const WRITING_PARTITIONS: &str = "Writing partitions";
    /// Writing the partition table.
    pub const WRITING_TABLE: &str = "Writing the partition table";
    /// Making sure everything reached the disk.
    pub const FLUSHING: &str = "Finishing";
    /// Checking, and if necessary rewriting, the boot configuration.
    pub const REPAIRING_BOOT: &str = "Checking the boot configuration";
    /// Done.
    pub const COMPLETED: &str = "Completed";
}

/// One write the restore intends to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedWrite {
    /// What this covers, for the log and the dry run report.
    pub what: String,
    /// Byte offset on the target disk.
    pub target_offset: u64,
    /// Number of bytes.
    pub length: u64,
}

/// Everything a restore will do, worked out before it does any of it.
#[derive(Debug, Clone)]
pub struct RestorePlan {
    /// The disk from the backup that is being recreated.
    pub source_disk: DiskEntry,
    /// Total bytes that will be written.
    pub total_bytes: u64,
    /// The writes, in the order they will happen.
    pub writes: Vec<PlannedWrite>,
    /// Space at the end of the target that will be left unallocated.
    pub unallocated_bytes: u64,
    /// Things worth telling the operator.
    pub warnings: Vec<String>,
}

impl RestorePlan {
    /// A summary for the review screen.
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "The backup holds disk {} ({}), {}",
            self.source_disk.disk_number,
            self.source_disk.model.as_deref().unwrap_or("unknown model"),
            mjolnir_core::progress::format_bytes(self.source_disk.size_bytes)
        )];
        for p in &self.source_disk.partitions {
            lines.push(format!(
                "  Partition {} - {} - {}",
                p.number,
                p.role.describe(),
                mjolnir_core::progress::format_bytes(p.length)
            ));
        }
        if self.unallocated_bytes > 0 {
            lines.push(format!(
                "{} at the end of the target disk will be left unallocated.",
                mjolnir_core::progress::format_bytes(self.unallocated_bytes)
            ));
        }
        lines
    }
}

/// Works out what a restore would do, and refuses anything unsafe.
///
/// Writes nothing. This is what `--dry-run` runs, and it is also the first
/// thing a real restore does.
pub fn plan(set: &BackupSet, target: &TargetDisk) -> Result<RestorePlan> {
    let manifest = set.manifest();

    if !set.is_restorable() {
        return Err(Error::corrupt(
            "this backup cannot be restored",
            "it is incomplete, or it did not pass verification when it was taken",
            "choose a different backup; run the verify command to see exactly what is wrong with this one",
        ));
    }

    // A preview backup is missing most of every partition. The format records
    // that fact, and it is refused here rather than being allowed to produce a
    // disk full of holes.
    for stream in &manifest.streams {
        if !stream.capture.is_restorable() {
            return Err(Error::corrupt(
                "this backup was taken as a preview and cannot be restored",
                format!(
                    "stream {:?} holds only the first part of its partition, so restoring it would leave most of the disk empty",
                    stream.id.as_str()
                ),
                "take a full backup and restore from that one",
            ));
        }
    }

    let layout = set.disk_layout();
    if layout.disks.len() != 1 {
        return Err(Error::unsupported(
            format!("this backup holds {} disks", layout.disks.len()),
            "this version restores one system disk at a time",
            "multi disk restores are not supported yet",
        ));
    }
    let source_disk = layout.disks[0].clone();

    check_target(&source_disk, target, manifest.required_restore_bytes)?;

    let mut writes = Vec::new();
    let mut total_bytes = 0u64;

    for stream in &manifest.streams {
        // The head and tail of the source disk are not replayed. The partition
        // table is rebuilt for the target's geometry instead, because the
        // secondary GPT lives at the end of the disk and the backup LBA fields
        // point at the source disk's size. Writing the captured bytes verbatim
        // onto a larger disk would produce a table that disagrees with the disk
        // it is on.
        if stream.kind != StreamKind::Partition {
            continue;
        }

        for segment in &stream.segments {
            let offset = math::add_u64(
                "restore target offset",
                stream.target_offset,
                segment.offset,
            )?;
            math::ensure_within("restore write", offset, segment.length, target.size_bytes)?;
            total_bytes = math::add_u64("restore total", total_bytes, segment.length)?;
        }

        writes.push(PlannedWrite {
            what: format!("stream {}", stream.id.as_str()),
            target_offset: stream.target_offset,
            length: stream.length,
        });
    }

    // No two writes may cover the same byte of the target.
    let mut sorted = writes.clone();
    sorted.sort_by_key(|w| (w.target_offset, w.length));
    for pair in sorted.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if math::ranges_overlap(a.target_offset, a.length, b.target_offset, b.length)? {
            return Err(Error::corrupt(
                "two parts of this backup would be written to the same place",
                format!(
                    "{} and {} both cover bytes around {} of the target disk",
                    a.what, b.what, b.target_offset
                ),
                "do not restore from this backup; it disagrees with itself and may be damaged",
            ));
        }
    }

    let highest = source_disk.required_target_bytes()?;
    let unallocated_bytes = target.size_bytes.saturating_sub(highest);

    let mut warnings = Vec::new();
    if unallocated_bytes > 1024 * 1024 * 1024 {
        warnings.push(format!(
            "The target disk is larger than the one backed up. {} at the end will be left unallocated; you can extend the Windows partition into it afterwards from Disk Management.",
            mjolnir_core::progress::format_bytes(unallocated_bytes)
        ));
    }

    Ok(RestorePlan {
        source_disk,
        total_bytes,
        writes,
        unallocated_bytes,
        warnings,
    })
}

/// Checks that every chunk the restore needs is present and readable.
///
/// Run before a restore begins, so a backup with a missing chunk stops the
/// operation while the old disk is still untouched rather than halfway through
/// writing the new one.
pub fn check_chunks_present(
    set: &BackupSet,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<()> {
    let store = set.chunk_store();
    let manifest = set.manifest();

    let total: u64 = manifest
        .chunks
        .iter()
        .map(|c| u64::from(c.uncompressed_size))
        .sum();
    progress.begin(stages::CHECKING, Some(total));

    for (i, chunk) in manifest.chunks.iter().enumerate() {
        cancel.check()?;
        // Reading each chunk fully is what proves it decompresses and matches
        // its digest. Checking only that the file exists would let a damaged
        // backup get as far as writing to the replacement disk.
        store
            .get(chunk.hash, chunk.uncompressed_size)
            .map_err(|e| {
                Error::corrupt(
                    format!("chunk {i} of the backup could not be read"),
                    format!("{}: {}", e.what(), e.why()),
                    "nothing has been written to the target disk. Use a different backup.",
                )
            })?;
        progress.advance(u64::from(chunk.uncompressed_size));
    }
    progress.end();
    Ok(())
}

/// Restores a backup onto `sink`.
///
/// The [`EraseConfirmation`] is what makes this callable. There is no variant
/// of this function without one.
pub fn restore(
    set: &BackupSet,
    plan: &RestorePlan,
    target: &TargetDisk,
    confirmation: &EraseConfirmation,
    sink: &mut dyn BlockSink,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<RestoreOutcome> {
    if !confirmation.matches(target) {
        return Err(Error::unsafe_target(
            "the confirmation does not match the disk about to be erased",
            "the disk selected is not the one the confirmation was typed for, which can happen if drives were unplugged or reconnected since",
            "start again and confirm the disk you actually mean to erase",
        ));
    }
    if sink.size_bytes() != target.size_bytes {
        return Err(Error::unsafe_target(
            "the disk changed size between being checked and being written",
            format!(
                "it was {} when it was checked and is {} now",
                mjolnir_core::progress::format_bytes(target.size_bytes),
                mjolnir_core::progress::format_bytes(sink.size_bytes())
            ),
            "start again so the disk can be checked as it is now",
        ));
    }

    check_chunks_present(set, progress, cancel)?;

    let manifest = set.manifest();
    let store = set.chunk_store();
    let mut written_bytes = 0u64;

    // ---- partition contents ------------------------------------------------
    progress.begin(stages::WRITING_PARTITIONS, Some(plan.total_bytes));
    for stream in &manifest.streams {
        if stream.kind != StreamKind::Partition {
            continue;
        }
        cancel.check()?;
        progress.note(&format!("Writing {}", stream.id.as_str()));

        for segment in &stream.segments {
            cancel.check()?;
            let chunk = manifest.chunk(segment.chunk).ok_or_else(|| {
                Error::corrupt(
                    "the backup refers to a chunk that is not in its own table",
                    format!(
                        "stream {} names chunk {}",
                        stream.id.as_str(),
                        segment.chunk
                    ),
                    "do not restore from this backup; it is damaged",
                )
            })?;

            let bytes = store.get(chunk.hash, chunk.uncompressed_size)?;
            let offset = math::add_u64(
                "restore target offset",
                stream.target_offset,
                segment.offset,
            )?;
            sink.write_all_at(offset, &bytes)?;
            written_bytes = math::add_u64("restored bytes", written_bytes, bytes.len() as u64)?;
            progress.advance(bytes.len() as u64);
        }
    }
    progress.end();

    // ---- partition table ---------------------------------------------------
    // Written last on purpose: until it exists the disk is visibly unfinished.
    cancel.check()?;
    progress.begin(stages::WRITING_TABLE, None);
    let built = build_partition_table(&plan.source_disk, target)?;
    sink.write_all_at(0, &built.head)?;
    sink.write_all_at(built.tail_offset, &built.tail)?;
    written_bytes = math::add_u64(
        "restored bytes",
        written_bytes,
        (built.head.len() + built.tail.len()) as u64,
    )?;
    progress.end();

    // ---- durability --------------------------------------------------------
    progress.begin(stages::FLUSHING, None);
    sink.flush_device()?;
    progress.end();
    progress.begin(stages::COMPLETED, None);
    progress.end();

    Ok(RestoreOutcome {
        written_bytes,
        partitions_restored: plan.writes.len(),
        unallocated_bytes: plan.unallocated_bytes,
        boot_repair: None,
    })
}

/// Builds the partition table for the target disk.
pub fn build_partition_table(
    source: &DiskEntry,
    target: &TargetDisk,
) -> Result<mjolnir_storage::gpt::BuiltGpt> {
    let sector_size = target.logical_sector_size;
    let total_sectors = math::bytes_to_sectors(
        "target disk size",
        target.size_bytes - (target.size_bytes % u64::from(sector_size)),
        sector_size,
    )?;

    let mut entries = Vec::with_capacity(source.partitions.len());
    for p in &source.partitions {
        let starting_lba =
            math::bytes_to_sectors("partition offset", p.starting_offset, sector_size)?;
        let blocks = math::bytes_to_sectors("partition length", p.length, sector_size)?;
        if blocks == 0 {
            return Err(Error::corrupt(
                format!("partition {} of the backup has no length", p.number),
                "a partition of zero blocks cannot be recreated",
                "do not restore from this backup; it is damaged",
            ));
        }
        entries.push(GptPartitionEntry {
            type_guid: guid_to_bytes(&p.type_guid)?,
            unique_guid: guid_to_bytes(&p.unique_guid)?,
            starting_lba,
            // The ending block is inclusive.
            ending_lba: math::sub_u64(
                "partition end",
                math::add_u64("partition end", starting_lba, blocks)?,
                1,
            )?,
            attributes: p.attributes,
            name: p.name.clone(),
        });
    }

    build_gpt(&GptBuildRequest {
        sector_size,
        total_sectors,
        disk_guid: guid_to_bytes(&source.disk_guid)?,
        partitions: entries,
    })
}

/// What a finished restore produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOutcome {
    /// Bytes written to the target.
    pub written_bytes: u64,
    /// How many partitions were restored.
    pub partitions_restored: usize,
    /// Space left unallocated at the end of the target.
    pub unallocated_bytes: u64,
    /// What the boot repair found and changed, when one was run.
    ///
    /// Absent when the restore was to a file rather than to a disk, which is
    /// what the tests do: there is no disk for Windows to rescan.
    pub boot_repair: Option<crate::boot::BootRepairReport>,
}

/// Parses a GUID string into the byte order a partition table stores.
fn guid_to_bytes(text: &str) -> Result<[u8; 16]> {
    let clean: String = text.chars().filter(|c| *c != '-').collect();
    if clean.len() != 32 || !clean.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::corrupt(
            format!("the backup contains {text:?} where a GUID should be"),
            "a partition cannot be recreated without its identifiers, and a malformed one means the backup has been damaged or edited",
            "do not restore from this backup; run the verify command to see what else is wrong with it",
        ));
    }
    let raw: Vec<u8> = (0..16)
        .map(|i| u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16).expect("checked above"))
        .collect();

    // The first three fields are little endian on disk, the last two are not.
    let mut out = [0u8; 16];
    out[0] = raw[3];
    out[1] = raw[2];
    out[2] = raw[1];
    out[3] = raw[0];
    out[4] = raw[5];
    out[5] = raw[4];
    out[6] = raw[7];
    out[7] = raw[6];
    out[8..16].copy_from_slice(&raw[8..16]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guids_convert_to_the_on_disk_byte_order() {
        let bytes = guid_to_bytes("c12a7328-f81f-11d2-ba4b-00a0c93ec93b").unwrap();
        assert_eq!(
            mjolnir_image::format_guid(&bytes),
            "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
        );
    }

    #[test]
    fn a_malformed_guid_is_refused_rather_than_guessed_at() {
        for bad in [
            "",
            "not-a-guid",
            "c12a7328f81f11d2ba4b00a0c93ec93",
            "zzzzzzzz-f81f-11d2-ba4b-00a0c93ec93b",
        ] {
            let err = guid_to_bytes(bad).unwrap_err();
            assert_eq!(
                err.exit(),
                mjolnir_core::ExitCode::CorruptBackup,
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn stage_names_are_plain_language() {
        for stage in [
            stages::CHECKING,
            stages::WRITING_PARTITIONS,
            stages::WRITING_TABLE,
            stages::FLUSHING,
            stages::COMPLETED,
        ] {
            assert!(!stage.is_empty());
            for jargon in ["GPT", "LBA", "chunk", "blake"] {
                assert!(
                    !stage.to_lowercase().contains(&jargon.to_lowercase()),
                    "{stage:?} contains jargon"
                );
            }
        }
    }
}
