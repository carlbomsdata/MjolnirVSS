//! Reading and writing GUID partition tables.
//!
//! This is plain byte handling against the published UEFI specification layout,
//! with no Windows dependency, so it can be exercised on any host and against
//! synthetic disk images rather than against real hardware.
//!
//! MjolnirVSS parses a GPT to describe what it captured, and builds one during
//! a restore to recreate the table on the replacement disk.
//!
//! The table is written as bytes rather than handed to Windows through
//! `IOCTL_DISK_SET_DRIVE_LAYOUT_EX`, for one reason: the same code then
//! produces the table on a real disk and on a file standing in for one, so the
//! destructive path is exercised by ordinary tests rather than only by risking
//! hardware. On a real disk the restore additionally asks Windows to re read
//! the layout afterwards, so the kernel's view matches what is now on it.

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::math;

/// Signature at the start of a GPT header.
pub const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

/// Size of the GPT header this version writes and expects.
pub const GPT_HEADER_SIZE: u32 = 92;

/// Revision 1.0, the only revision in use.
pub const GPT_REVISION: u32 = 0x0001_0000;

/// Size of one partition entry.
pub const PARTITION_ENTRY_SIZE: u32 = 128;

/// Number of partition entries a conventional GPT reserves room for.
pub const DEFAULT_ENTRY_COUNT: u32 = 128;

/// Byte offset of the header's own CRC field, which is zeroed while computing.
const HEADER_CRC_OFFSET: usize = 16;

/// Computes a CRC-32, the reflected IEEE variant the UEFI specification uses.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// One entry of the partition array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptPartitionEntry {
    /// Partition type GUID, as raw bytes in the order the table stores them.
    pub type_guid: [u8; 16],
    /// Unique partition GUID.
    pub unique_guid: [u8; 16],
    /// First logical block of the partition.
    pub starting_lba: u64,
    /// Last logical block of the partition, inclusive.
    pub ending_lba: u64,
    /// Attribute flags.
    pub attributes: u64,
    /// Partition name.
    pub name: String,
}

impl GptPartitionEntry {
    /// Whether this slot is unused.
    ///
    /// The specification marks an unused entry with an all zero type GUID.
    pub fn is_unused(&self) -> bool {
        self.type_guid == [0u8; 16]
    }

    /// Number of logical blocks the partition occupies.
    pub fn block_count(&self) -> Result<u64> {
        if self.ending_lba < self.starting_lba {
            return Err(Error::corrupt(
                "a GPT partition entry ends before it starts",
                format!(
                    "the entry runs from block {} to block {}, which describes a negative length",
                    self.starting_lba, self.ending_lba
                ),
                "the partition table is damaged; do not restore from a backup of this disk without checking it first",
            ));
        }
        // The ending block is inclusive, hence the plus one.
        Ok(math::add_u64(
            "partition block count",
            self.ending_lba - self.starting_lba,
            1,
        )?)
    }

    /// Byte offset of the partition from the start of the disk.
    pub fn byte_offset(&self, sector_size: u32) -> Result<u64> {
        Ok(math::sectors_to_bytes(
            "partition offset",
            self.starting_lba,
            sector_size,
        )?)
    }

    /// Length of the partition in bytes.
    pub fn byte_length(&self, sector_size: u32) -> Result<u64> {
        Ok(math::sectors_to_bytes(
            "partition length",
            self.block_count()?,
            sector_size,
        )?)
    }

    /// Parses one entry from exactly [`PARTITION_ENTRY_SIZE`] bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < PARTITION_ENTRY_SIZE as usize {
            return Err(short_read(
                "a GPT partition entry",
                bytes.len(),
                PARTITION_ENTRY_SIZE as usize,
            ));
        }
        let mut type_guid = [0u8; 16];
        type_guid.copy_from_slice(&bytes[0..16]);
        let mut unique_guid = [0u8; 16];
        unique_guid.copy_from_slice(&bytes[16..32]);

        // The name is 36 UTF-16 code units, terminated by a zero unit if it is
        // shorter. Anything unpaired is replaced rather than rejected: a
        // surprising name must not stop a backup.
        let mut units = Vec::with_capacity(36);
        for i in 0..36 {
            let at = 56 + i * 2;
            let unit = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
            if unit == 0 {
                break;
            }
            units.push(unit);
        }

        Ok(Self {
            type_guid,
            unique_guid,
            starting_lba: read_u64(bytes, 32),
            ending_lba: read_u64(bytes, 40),
            attributes: read_u64(bytes, 48),
            name: String::from_utf16_lossy(&units),
        })
    }

    /// Serialises the entry into [`PARTITION_ENTRY_SIZE`] bytes.
    pub fn to_bytes(&self) -> [u8; PARTITION_ENTRY_SIZE as usize] {
        let mut out = [0u8; PARTITION_ENTRY_SIZE as usize];
        out[0..16].copy_from_slice(&self.type_guid);
        out[16..32].copy_from_slice(&self.unique_guid);
        out[32..40].copy_from_slice(&self.starting_lba.to_le_bytes());
        out[40..48].copy_from_slice(&self.ending_lba.to_le_bytes());
        out[48..56].copy_from_slice(&self.attributes.to_le_bytes());
        for (i, unit) in self.name.encode_utf16().take(35).enumerate() {
            let at = 56 + i * 2;
            out[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        }
        out
    }
}

/// A parsed GPT header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptHeader {
    /// Block this header was read from.
    pub my_lba: u64,
    /// Block holding the other copy of the header.
    pub alternate_lba: u64,
    /// First block a partition may occupy.
    pub first_usable_lba: u64,
    /// Last block a partition may occupy, inclusive.
    pub last_usable_lba: u64,
    /// Disk GUID, as raw bytes.
    pub disk_guid: [u8; 16],
    /// Block where the partition entry array starts.
    pub partition_entry_lba: u64,
    /// How many entries the array has room for.
    pub num_partition_entries: u32,
    /// Size of one entry.
    pub size_of_partition_entry: u32,
    /// CRC of the entry array, as recorded in the header.
    pub partition_entry_array_crc32: u32,
}

impl GptHeader {
    /// Parses and checks a header.
    ///
    /// The header CRC is verified here rather than by the caller, because a
    /// header that fails it must never be used to compute an offset.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < GPT_HEADER_SIZE as usize {
            return Err(short_read(
                "a GPT header",
                bytes.len(),
                GPT_HEADER_SIZE as usize,
            ));
        }
        if &bytes[0..8] != GPT_SIGNATURE {
            return Err(Error::unsupported(
                "this disk does not have a GUID partition table",
                "the first block after the protective MBR does not carry the EFI PART signature, so the disk is either MBR partitioned, unpartitioned, or encrypted",
                "this version of MjolnirVSS supports GPT disks on UEFI machines only",
            ));
        }

        let header_size = read_u32(bytes, 12);
        if !(GPT_HEADER_SIZE..=512).contains(&header_size) || header_size as usize > bytes.len() {
            return Err(Error::corrupt(
                "the GPT header declares an implausible size",
                format!("it says {header_size} bytes, which is outside the range the specification allows"),
                "the partition table is damaged; run a disk check before trusting this disk",
            ));
        }

        // The CRC covers header_size bytes with its own CRC field zeroed.
        let recorded_crc = read_u32(bytes, HEADER_CRC_OFFSET);
        let mut scratch = bytes[..header_size as usize].to_vec();
        scratch[HEADER_CRC_OFFSET..HEADER_CRC_OFFSET + 4].copy_from_slice(&0u32.to_le_bytes());
        let computed = crc32(&scratch);
        if computed != recorded_crc {
            return Err(Error::corrupt(
                "the GPT header failed its checksum",
                format!("the header records checksum {recorded_crc:#010x} but its contents hash to {computed:#010x}, so the partition table has been damaged"),
                "do not back up or restore this disk until the partition table has been repaired; Windows can often rebuild it from the backup copy at the end of the disk",
            ));
        }

        let header = Self {
            my_lba: read_u64(bytes, 24),
            alternate_lba: read_u64(bytes, 32),
            first_usable_lba: read_u64(bytes, 40),
            last_usable_lba: read_u64(bytes, 48),
            disk_guid: {
                let mut g = [0u8; 16];
                g.copy_from_slice(&bytes[56..72]);
                g
            },
            partition_entry_lba: read_u64(bytes, 72),
            num_partition_entries: read_u32(bytes, 80),
            size_of_partition_entry: read_u32(bytes, 84),
            partition_entry_array_crc32: read_u32(bytes, 88),
        };

        if header.size_of_partition_entry < PARTITION_ENTRY_SIZE
            || header.size_of_partition_entry % 8 != 0
        {
            return Err(Error::corrupt(
                "the GPT header declares an implausible partition entry size",
                format!(
                    "it says {} bytes; the specification requires at least {PARTITION_ENTRY_SIZE} and a multiple of 8",
                    header.size_of_partition_entry
                ),
                "the partition table is damaged; run a disk check before trusting this disk",
            ));
        }
        // A hostile or damaged table could otherwise ask the reader to allocate
        // an enormous entry array.
        if header.num_partition_entries > 4096 {
            return Err(Error::corrupt(
                "the GPT header declares an implausible number of partitions",
                format!(
                    "it says room for {} entries; real disks use 128",
                    header.num_partition_entries
                ),
                "the partition table is damaged; run a disk check before trusting this disk",
            ));
        }
        if header.last_usable_lba < header.first_usable_lba {
            return Err(Error::corrupt(
                "the GPT header describes a disk with no usable space",
                format!(
                    "the last usable block {} is before the first usable block {}",
                    header.last_usable_lba, header.first_usable_lba
                ),
                "the partition table is damaged; run a disk check before trusting this disk",
            ));
        }

        Ok(header)
    }

    /// Total bytes the partition entry array occupies.
    pub fn entry_array_bytes(&self) -> Result<u64> {
        Ok(math::mul_u64(
            "partition array size",
            u64::from(self.num_partition_entries),
            u64::from(self.size_of_partition_entry),
        )?)
    }

    /// Serialises the header into a sector, computing both CRCs.
    pub fn to_sector(&self, sector_size: u32, entry_array: &[u8]) -> Vec<u8> {
        let mut sector = vec![0u8; sector_size as usize];
        sector[0..8].copy_from_slice(GPT_SIGNATURE);
        sector[8..12].copy_from_slice(&GPT_REVISION.to_le_bytes());
        sector[12..16].copy_from_slice(&GPT_HEADER_SIZE.to_le_bytes());
        // 16..20 is the header CRC, filled in last.
        sector[20..24].copy_from_slice(&0u32.to_le_bytes());
        sector[24..32].copy_from_slice(&self.my_lba.to_le_bytes());
        sector[32..40].copy_from_slice(&self.alternate_lba.to_le_bytes());
        sector[40..48].copy_from_slice(&self.first_usable_lba.to_le_bytes());
        sector[48..56].copy_from_slice(&self.last_usable_lba.to_le_bytes());
        sector[56..72].copy_from_slice(&self.disk_guid);
        sector[72..80].copy_from_slice(&self.partition_entry_lba.to_le_bytes());
        sector[80..84].copy_from_slice(&self.num_partition_entries.to_le_bytes());
        sector[84..88].copy_from_slice(&self.size_of_partition_entry.to_le_bytes());
        sector[88..92].copy_from_slice(&crc32(entry_array).to_le_bytes());

        let header_crc = crc32(&sector[..GPT_HEADER_SIZE as usize]);
        sector[HEADER_CRC_OFFSET..HEADER_CRC_OFFSET + 4].copy_from_slice(&header_crc.to_le_bytes());
        sector
    }
}

/// A GPT header together with the partitions it describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedGpt {
    /// The header.
    pub header: GptHeader,
    /// The entries that are in use, in table order.
    pub partitions: Vec<GptPartitionEntry>,
}

/// Parses the primary GPT out of the start of a disk.
///
/// `head` must hold at least the first block plus the partition entry array,
/// which for a conventional 512 byte disk means the first 34 blocks.
pub fn parse_primary(head: &[u8], sector_size: u32) -> Result<ParsedGpt> {
    if sector_size == 0 || !sector_size.is_power_of_two() {
        return Err(Error::unsupported(
            format!("a sector size of {sector_size} bytes is not supported"),
            "MjolnirVSS works in whole sectors, and a sector size that is not a power of two would make every offset calculation ambiguous",
            "this disk cannot be backed up by this version",
        ));
    }
    let sector = sector_size as usize;
    if head.len() < sector * 2 {
        return Err(short_read("the start of the disk", head.len(), sector * 2));
    }

    let header = GptHeader::parse(&head[sector..sector * 2])?;

    let array_start = math::to_usize(
        "partition array offset",
        math::sectors_to_bytes(
            "partition array offset",
            header.partition_entry_lba,
            sector_size,
        )?,
    )?;
    let array_len = math::to_usize("partition array size", header.entry_array_bytes()?)?;
    let array_end = array_start.checked_add(array_len).ok_or_else(|| {
        Error::corrupt(
            "the GPT partition array runs past the end of addressable memory",
            "the header's entry count and entry size multiply out to an impossible size",
            "the partition table is damaged; run a disk check before trusting this disk",
        )
    })?;

    if array_end > head.len() {
        return Err(short_read("the GPT partition array", head.len(), array_end));
    }
    let array = &head[array_start..array_end];

    let computed = crc32(array);
    if computed != header.partition_entry_array_crc32 {
        return Err(Error::corrupt(
            "the GPT partition table failed its checksum",
            format!(
                "the header records checksum {:#010x} for the partition array but its contents hash to {computed:#010x}",
                header.partition_entry_array_crc32
            ),
            "the partition table is damaged; Windows can often rebuild it from the backup copy at the end of the disk, and MjolnirVSS will not back up a disk whose table it cannot trust",
        ));
    }

    let mut partitions = Vec::new();
    for i in 0..header.num_partition_entries as usize {
        let at = i * header.size_of_partition_entry as usize;
        let entry = GptPartitionEntry::parse(&array[at..])?;
        if !entry.is_unused() {
            partitions.push(entry);
        }
    }

    Ok(ParsedGpt { header, partitions })
}

/// How much of the start of a disk has to be read to hold the primary GPT.
///
/// One block for the protective MBR, one for the header, and enough for the
/// entry array. Rounded up to the alignment a first partition conventionally
/// starts at so the head stream is a tidy size.
pub fn primary_gpt_span(sector_size: u32) -> Result<u64> {
    let array = u64::from(DEFAULT_ENTRY_COUNT) * u64::from(PARTITION_ENTRY_SIZE);
    let needed = math::add_u64(
        "primary gpt span",
        math::sectors_to_bytes("primary gpt span", 2, sector_size)?,
        array,
    )?;
    Ok(math::round_up("primary gpt span", needed, 1 << 20)?)
}

/// How much of the end of a disk holds the secondary GPT.
pub fn secondary_gpt_span(sector_size: u32) -> Result<u64> {
    let array = u64::from(DEFAULT_ENTRY_COUNT) * u64::from(PARTITION_ENTRY_SIZE);
    let needed = math::add_u64(
        "secondary gpt span",
        array,
        math::sectors_to_bytes("secondary gpt span", 1, sector_size)?,
    )?;
    Ok(math::round_up(
        "secondary gpt span",
        needed,
        u64::from(sector_size),
    )?)
}

/// Builds a protective MBR for a disk of `total_sectors` blocks.
pub fn protective_mbr(sector_size: u32, total_sectors: u64) -> Vec<u8> {
    let mut mbr = vec![0u8; sector_size as usize];
    // A single entry of type 0xEE covering the whole disk, which is what stops
    // a tool that only understands MBR from thinking the disk is empty.
    let size_in_lba = u32::try_from(total_sectors.saturating_sub(1)).unwrap_or(u32::MAX);
    mbr[446] = 0x00; // not bootable
    mbr[447] = 0x00;
    mbr[448] = 0x02;
    mbr[449] = 0x00;
    mbr[450] = 0xEE; // GPT protective
    mbr[451] = 0xFF;
    mbr[452] = 0xFF;
    mbr[453] = 0xFF;
    mbr[454..458].copy_from_slice(&1u32.to_le_bytes());
    mbr[458..462].copy_from_slice(&size_in_lba.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    mbr
}

/// A request to lay out a fresh GUID partition table.
#[derive(Debug, Clone)]
pub struct GptBuildRequest {
    /// Logical sector size of the target disk.
    pub sector_size: u32,
    /// Total number of logical blocks on the target disk.
    pub total_sectors: u64,
    /// Disk GUID to write, preserved from the backup.
    pub disk_guid: [u8; 16],
    /// The partitions to record, in table order.
    pub partitions: Vec<GptPartitionEntry>,
}

/// A partition table ready to be written to a disk.
#[derive(Debug, Clone)]
pub struct BuiltGpt {
    /// Protective master boot record, primary header and primary entry array,
    /// starting at byte zero of the disk.
    pub head: Vec<u8>,
    /// Secondary entry array followed by the secondary header.
    pub tail: Vec<u8>,
    /// Byte offset the tail belongs at.
    pub tail_offset: u64,
    /// First block a partition may occupy on this disk.
    pub first_usable_lba: u64,
    /// Last block a partition may occupy, inclusive.
    pub last_usable_lba: u64,
}

/// Builds a GUID partition table for a target disk.
///
/// Written byte for byte rather than handed to Windows, so the same code path
/// produces the table on a real disk and on a file standing in for one during a
/// test. The partition GUIDs, type GUIDs, names and attributes come from the
/// backup unchanged, which is what makes Windows recognise the restored disk as
/// the same installation.
pub fn build_gpt(request: &GptBuildRequest) -> Result<BuiltGpt> {
    let sector_size = request.sector_size;
    if sector_size == 0 || !sector_size.is_power_of_two() {
        return Err(Error::unsupported(
            format!("a sector size of {sector_size} bytes is not supported"),
            "MjolnirVSS works in whole sectors, and a sector size that is not a power of two would make every offset ambiguous",
            "this disk cannot be restored to by this version",
        ));
    }
    let sector = sector_size as usize;
    let array_bytes = DEFAULT_ENTRY_COUNT as usize * PARTITION_ENTRY_SIZE as usize;
    let array_sectors = array_bytes.div_ceil(sector) as u64;

    // Two blocks for the protective MBR and primary header, plus the array at
    // each end. A disk that cannot hold those cannot hold a GPT at all.
    let minimum_sectors = math::add_u64(
        "gpt minimum",
        4,
        math::mul_u64("gpt minimum", array_sectors, 2)?,
    )?;
    if request.total_sectors < minimum_sectors {
        return Err(Error::unsupported(
            "the target disk is too small to hold a partition table",
            format!(
                "a GUID partition table needs at least {minimum_sectors} blocks and this disk has {}",
                request.total_sectors
            ),
            "use a larger disk",
        ));
    }
    if request.partitions.len() > DEFAULT_ENTRY_COUNT as usize {
        return Err(Error::unsupported(
            format!("the backup records {} partitions", request.partitions.len()),
            format!("a conventional GUID partition table holds {DEFAULT_ENTRY_COUNT} entries"),
            "this layout cannot be restored by this version",
        ));
    }

    let first_usable_lba = math::add_u64("first usable block", 2, array_sectors)?;
    let last_usable_lba = math::sub_u64(
        "last usable block",
        request.total_sectors,
        math::add_u64("last usable block", 2, array_sectors)?,
    )?;

    // Every partition has to fit inside the usable area, or the table would
    // describe something the firmware will refuse.
    for entry in &request.partitions {
        if entry.starting_lba < first_usable_lba || entry.ending_lba > last_usable_lba {
            return Err(Error::unsupported(
                format!(
                    "a partition of the backup does not fit on the target disk: blocks {}..{}",
                    entry.starting_lba, entry.ending_lba
                ),
                format!(
                    "this disk can hold partitions between blocks {first_usable_lba} and {last_usable_lba}"
                ),
                "use a disk at least as large as the one the backup was taken from",
            ));
        }
    }

    let mut array = vec![0u8; array_bytes];
    for (i, entry) in request.partitions.iter().enumerate() {
        let at = i * PARTITION_ENTRY_SIZE as usize;
        array[at..at + PARTITION_ENTRY_SIZE as usize].copy_from_slice(&entry.to_bytes());
    }
    let array_crc = crc32(&array);

    let primary = GptHeader {
        my_lba: 1,
        alternate_lba: request.total_sectors - 1,
        first_usable_lba,
        last_usable_lba,
        disk_guid: request.disk_guid,
        partition_entry_lba: 2,
        num_partition_entries: DEFAULT_ENTRY_COUNT,
        size_of_partition_entry: PARTITION_ENTRY_SIZE,
        partition_entry_array_crc32: array_crc,
    };
    let secondary_array_lba = math::sub_u64(
        "secondary array block",
        request.total_sectors - 1,
        array_sectors,
    )?;
    let secondary = GptHeader {
        my_lba: request.total_sectors - 1,
        alternate_lba: 1,
        partition_entry_lba: secondary_array_lba,
        ..primary
    };

    // Head: MBR, primary header, primary array.
    let head_sectors = math::add_u64("gpt head", 2, array_sectors)?;
    let mut head = vec![
        0u8;
        math::to_usize(
            "gpt head",
            math::sectors_to_bytes("gpt head", head_sectors, sector_size)?
        )?
    ];
    head[..sector].copy_from_slice(&protective_mbr(sector_size, request.total_sectors));
    head[sector..sector * 2].copy_from_slice(&primary.to_sector(sector_size, &array));
    head[sector * 2..sector * 2 + array_bytes].copy_from_slice(&array);

    // Tail: secondary array then secondary header, at the very end of the disk.
    let mut tail = vec![
        0u8;
        math::to_usize(
            "gpt tail",
            math::sectors_to_bytes(
                "gpt tail",
                math::add_u64("gpt tail", array_sectors, 1)?,
                sector_size
            )?
        )?
    ];
    tail[..array_bytes].copy_from_slice(&array);
    let header_at = math::to_usize(
        "gpt tail",
        math::sectors_to_bytes("gpt tail", array_sectors, sector_size)?,
    )?;
    tail[header_at..header_at + sector].copy_from_slice(&secondary.to_sector(sector_size, &array));
    let tail_offset = math::sectors_to_bytes("gpt tail offset", secondary_array_lba, sector_size)?;

    Ok(BuiltGpt {
        head,
        tail,
        tail_offset,
        first_usable_lba,
        last_usable_lba,
    })
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(b)
}

fn short_read(what: &str, got: usize, needed: usize) -> Error {
    Error::new(
        ExitCode::CorruptBackup,
        format!("{what} was shorter than it should be"),
        format!("{needed} bytes are needed but only {got} were available, so the structure is truncated"),
        "the disk or the backup is damaged; do not restore from it",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_known_vectors() {
        // The standard CRC-32 check value for "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(&[0u8; 32]), 0x190A_55AD);
    }

    #[test]
    fn a_partition_entry_round_trips() {
        let entry = GptPartitionEntry {
            type_guid: [1u8; 16],
            unique_guid: [2u8; 16],
            starting_lba: 2048,
            ending_lba: 4095,
            attributes: 0x8000_0000_0000_0000,
            name: "Basic data partition".to_owned(),
        };
        let parsed = GptPartitionEntry::parse(&entry.to_bytes()).unwrap();
        assert_eq!(parsed, entry);
        assert_eq!(parsed.block_count().unwrap(), 2048);
        assert_eq!(parsed.byte_offset(512).unwrap(), 1_048_576);
        assert_eq!(parsed.byte_length(512).unwrap(), 1_048_576);
    }

    #[test]
    fn an_entry_name_longer_than_the_field_is_truncated_not_overflowed() {
        let entry = GptPartitionEntry {
            type_guid: [1u8; 16],
            unique_guid: [2u8; 16],
            starting_lba: 1,
            ending_lba: 2,
            attributes: 0,
            name: "x".repeat(200),
        };
        let bytes = entry.to_bytes();
        let parsed = GptPartitionEntry::parse(&bytes).unwrap();
        assert_eq!(parsed.name.chars().count(), 35);
    }

    #[test]
    fn an_unused_entry_is_recognised() {
        let entry = GptPartitionEntry::parse(&[0u8; 128]).unwrap();
        assert!(entry.is_unused());
    }

    #[test]
    fn an_entry_that_ends_before_it_starts_is_refused() {
        let entry = GptPartitionEntry {
            type_guid: [1u8; 16],
            unique_guid: [2u8; 16],
            starting_lba: 100,
            ending_lba: 50,
            attributes: 0,
            name: String::new(),
        };
        assert!(entry.block_count().is_err());
    }

    #[test]
    fn a_truncated_entry_is_refused() {
        assert!(GptPartitionEntry::parse(&[0u8; 64]).is_err());
    }

    fn build_disk(sector_size: u32, total_sectors: u64, entries: &[GptPartitionEntry]) -> Vec<u8> {
        let sector = sector_size as usize;
        let array_sectors =
            (DEFAULT_ENTRY_COUNT as usize * PARTITION_ENTRY_SIZE as usize).div_ceil(sector);

        let mut array = vec![0u8; DEFAULT_ENTRY_COUNT as usize * PARTITION_ENTRY_SIZE as usize];
        for (i, e) in entries.iter().enumerate() {
            let at = i * PARTITION_ENTRY_SIZE as usize;
            array[at..at + PARTITION_ENTRY_SIZE as usize].copy_from_slice(&e.to_bytes());
        }

        let header = GptHeader {
            my_lba: 1,
            alternate_lba: total_sectors - 1,
            first_usable_lba: 2 + array_sectors as u64,
            last_usable_lba: total_sectors - 2 - array_sectors as u64,
            disk_guid: [7u8; 16],
            partition_entry_lba: 2,
            num_partition_entries: DEFAULT_ENTRY_COUNT,
            size_of_partition_entry: PARTITION_ENTRY_SIZE,
            partition_entry_array_crc32: 0,
        };

        let mut disk = vec![0u8; (total_sectors as usize) * sector];
        disk[..sector].copy_from_slice(&protective_mbr(sector_size, total_sectors));
        disk[sector..sector * 2].copy_from_slice(&header.to_sector(sector_size, &array));
        disk[sector * 2..sector * 2 + array.len()].copy_from_slice(&array);
        disk
    }

    #[test]
    fn a_built_gpt_parses_back() {
        let entries = vec![
            GptPartitionEntry {
                type_guid: [0xC1; 16],
                unique_guid: [0xA1; 16],
                starting_lba: 2048,
                ending_lba: 206_847,
                attributes: 0,
                name: "EFI system partition".to_owned(),
            },
            GptPartitionEntry {
                type_guid: [0xEB; 16],
                unique_guid: [0xA2; 16],
                starting_lba: 206_848,
                ending_lba: 2_097_151,
                attributes: 0,
                name: "Basic data partition".to_owned(),
            },
        ];
        let disk = build_disk(512, 2_097_152, &entries);
        let parsed = parse_primary(&disk, 512).unwrap();

        assert_eq!(parsed.partitions.len(), 2);
        assert_eq!(parsed.partitions[0].name, "EFI system partition");
        assert_eq!(parsed.partitions[0].byte_offset(512).unwrap(), 1_048_576);
        assert_eq!(parsed.header.disk_guid, [7u8; 16]);
        assert_eq!(parsed.header.my_lba, 1);
    }

    #[test]
    fn a_disk_without_the_signature_is_refused_clearly() {
        let mut disk = build_disk(512, 4096, &[]);
        disk[512..520].copy_from_slice(b"NOTGPT!!");
        let err = parse_primary(&disk, 512).unwrap_err();
        assert_eq!(err.exit(), ExitCode::Unsupported);
        assert!(err.what().contains("does not have a GUID partition table"));
    }

    #[test]
    fn a_damaged_header_fails_its_checksum() {
        let mut disk = build_disk(512, 4096, &[]);
        // Corrupt a field the CRC covers, without touching the CRC itself.
        disk[512 + 56] ^= 0xFF;
        let err = parse_primary(&disk, 512).unwrap_err();
        assert_eq!(err.exit(), ExitCode::CorruptBackup);
        assert!(err.what().contains("header failed its checksum"));
    }

    #[test]
    fn a_damaged_partition_array_fails_its_checksum() {
        let entries = vec![GptPartitionEntry {
            type_guid: [0xC1; 16],
            unique_guid: [0xA1; 16],
            starting_lba: 2048,
            ending_lba: 4095,
            attributes: 0,
            name: "p".to_owned(),
        }];
        let mut disk = build_disk(512, 8192, &entries);
        disk[1024 + 40] ^= 0xFF;
        let err = parse_primary(&disk, 512).unwrap_err();
        assert!(err.what().contains("partition table failed its checksum"));
    }

    #[test]
    fn an_absurd_entry_count_is_refused_before_allocating() {
        let mut disk = build_disk(512, 8192, &[]);
        // Rewrite the entry count and repair the header CRC so the only thing
        // wrong is the implausible number itself.
        disk[512 + 80..512 + 84].copy_from_slice(&1_000_000u32.to_le_bytes());
        disk[512 + 16..512 + 20].copy_from_slice(&0u32.to_le_bytes());
        let crc = crc32(&disk[512..512 + GPT_HEADER_SIZE as usize]);
        disk[512 + 16..512 + 20].copy_from_slice(&crc.to_le_bytes());

        let err = parse_primary(&disk, 512).unwrap_err();
        assert!(err.what().contains("implausible number of partitions"));
    }

    #[test]
    fn a_4k_sector_disk_parses() {
        let entries = vec![GptPartitionEntry {
            type_guid: [0xC1; 16],
            unique_guid: [0xA1; 16],
            starting_lba: 256,
            ending_lba: 1023,
            attributes: 0,
            name: "p".to_owned(),
        }];
        let disk = build_disk(4096, 2048, &entries);
        let parsed = parse_primary(&disk, 4096).unwrap();
        assert_eq!(parsed.partitions.len(), 1);
        assert_eq!(parsed.partitions[0].byte_offset(4096).unwrap(), 1_048_576);
    }

    #[test]
    fn a_truncated_head_is_refused_rather_than_indexing_out_of_bounds() {
        let disk = build_disk(512, 8192, &[]);
        for cut in [0usize, 100, 512, 1000, 1024, 5000] {
            // Must return an error, never panic.
            let _ = parse_primary(&disk[..cut.min(disk.len())], 512);
        }
    }

    #[test]
    fn protective_mbr_has_the_expected_shape() {
        let mbr = protective_mbr(512, 2_097_152);
        assert_eq!(mbr[450], 0xEE);
        assert_eq!(mbr[510], 0x55);
        assert_eq!(mbr[511], 0xAA);
        assert_eq!(read_u32(&mbr, 454), 1);
        assert_eq!(read_u32(&mbr, 458), 2_097_151);
    }

    #[test]
    fn protective_mbr_saturates_on_a_disk_larger_than_2tb() {
        let mbr = protective_mbr(512, u64::MAX);
        assert_eq!(read_u32(&mbr, 458), u32::MAX);
    }

    #[test]
    fn a_non_power_of_two_sector_size_is_refused() {
        assert!(parse_primary(&[0u8; 4096], 520).is_err());
        assert!(parse_primary(&[0u8; 4096], 0).is_err());
    }

    #[test]
    fn gpt_spans_leave_room_for_the_conventional_table() {
        assert!(primary_gpt_span(512).unwrap() >= 33 * 512);
        assert_eq!(primary_gpt_span(512).unwrap() % (1 << 20), 0);
        assert!(secondary_gpt_span(512).unwrap() >= 33 * 512);
        assert_eq!(secondary_gpt_span(4096).unwrap() % 4096, 0);
    }
}
