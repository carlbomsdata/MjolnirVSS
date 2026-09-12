//! Building synthetic GPT disks in memory.
//!
//! These are not mock objects. Each one is a real disk image: protective master
//! boot record at block zero, a primary GPT whose checksums are computed the
//! way the specification says, a partition entry array, partition contents, and
//! a secondary GPT at the end. The GPT parser in `mjolnir-storage` reads them
//! without knowing they are synthetic, which is the point.

use mjolnir_core::blockio::MemoryBlockDevice;
use mjolnir_image::disk_layout::{
    GUID_BASIC_DATA, GUID_EFI_SYSTEM, GUID_MSR, GUID_WINDOWS_RECOVERY,
};
use mjolnir_storage::gpt::{
    crc32, protective_mbr, GptHeader, GptPartitionEntry, DEFAULT_ENTRY_COUNT, PARTITION_ENTRY_SIZE,
};

/// One partition to place on a synthetic disk.
#[derive(Debug, Clone)]
pub struct SyntheticPartition {
    /// GPT partition type GUID, as text.
    pub type_guid: String,
    /// GPT unique partition GUID, as text.
    pub unique_guid: String,
    /// Partition name.
    pub name: String,
    /// Size in bytes. Rounded up to a whole number of sectors.
    pub size_bytes: u64,
    /// A byte used to fill the partition, so its contents are recognisable.
    pub fill: u8,
    /// An eight byte signature written at the start of the partition.
    pub signature: [u8; 8],
}

impl SyntheticPartition {
    /// An EFI system partition, filled like a small FAT32 volume.
    pub fn efi(size_bytes: u64) -> Self {
        Self {
            type_guid: GUID_EFI_SYSTEM.to_owned(),
            unique_guid: "11111111-1111-1111-1111-111111111111".to_owned(),
            name: "EFI system partition".to_owned(),
            size_bytes,
            fill: 0xE1,
            // FAT32 puts its type string here; enough to be recognisable.
            signature: *b"FAT32   ",
        }
    }

    /// A Microsoft Reserved partition, which holds no filesystem at all.
    pub fn msr(size_bytes: u64) -> Self {
        Self {
            type_guid: GUID_MSR.to_owned(),
            unique_guid: "22222222-2222-2222-2222-222222222222".to_owned(),
            name: "Microsoft reserved partition".to_owned(),
            size_bytes,
            fill: 0x00,
            signature: [0u8; 8],
        }
    }

    /// A Windows partition, filled like an NTFS volume.
    pub fn windows(size_bytes: u64) -> Self {
        Self {
            type_guid: GUID_BASIC_DATA.to_owned(),
            unique_guid: "33333333-3333-3333-3333-333333333333".to_owned(),
            name: "Basic data partition".to_owned(),
            size_bytes,
            fill: 0x57,
            signature: *b"NTFS    ",
        }
    }

    /// A Windows recovery partition.
    pub fn recovery(size_bytes: u64) -> Self {
        Self {
            type_guid: GUID_WINDOWS_RECOVERY.to_owned(),
            unique_guid: "44444444-4444-4444-4444-444444444444".to_owned(),
            name: "Basic data partition".to_owned(),
            size_bytes,
            fill: 0x5E,
            signature: *b"NTFS    ",
        }
    }
}

/// A complete GPT disk image held in memory.
#[derive(Debug, Clone)]
pub struct SyntheticDisk {
    /// The raw bytes of the whole disk.
    pub bytes: Vec<u8>,
    /// Logical sector size.
    pub sector_size: u32,
    /// Disk GUID, as text.
    pub disk_guid: String,
    /// Where each partition ended up: offset and length in bytes.
    pub partitions: Vec<(SyntheticPartition, u64, u64)>,
}

impl SyntheticDisk {
    /// Builds a disk that looks like an ordinary UEFI Windows installation.
    ///
    /// Small enough to build and copy in a test, laid out the way Windows Setup
    /// lays one out: EFI, reserved, Windows, recovery.
    pub fn windows_like(sector_size: u32) -> Self {
        Self::build(
            sector_size,
            64 * 1024 * 1024,
            vec![
                SyntheticPartition::efi(4 * 1024 * 1024),
                SyntheticPartition::msr(1024 * 1024),
                SyntheticPartition::windows(32 * 1024 * 1024),
                SyntheticPartition::recovery(4 * 1024 * 1024),
            ],
        )
    }

    /// Builds a disk of `total_bytes` holding `partitions`.
    ///
    /// Partitions are laid out in order starting at one mebibyte, aligned to
    /// one mebibyte, which is what Windows does.
    pub fn build(sector_size: u32, total_bytes: u64, partitions: Vec<SyntheticPartition>) -> Self {
        assert!(
            sector_size.is_power_of_two(),
            "sector size must be a power of two"
        );
        assert!(
            total_bytes % u64::from(sector_size) == 0,
            "size must be whole sectors"
        );

        let sector = sector_size as usize;
        let total_sectors = total_bytes / u64::from(sector_size);
        let array_bytes = DEFAULT_ENTRY_COUNT as usize * PARTITION_ENTRY_SIZE as usize;
        let array_sectors = array_bytes.div_ceil(sector);

        let mut bytes = vec![0u8; total_bytes as usize];
        let alignment = 1024 * 1024u64;

        // Lay the partitions out and fill them.
        let mut placed = Vec::new();
        let mut entries = Vec::new();
        let mut cursor = alignment;

        for part in partitions {
            let length = part.size_bytes.div_ceil(u64::from(sector_size)) * u64::from(sector_size);
            let offset = cursor;
            assert!(
                offset + length <= total_bytes,
                "partition {} does not fit on the disk",
                part.name
            );

            let start = offset as usize;
            let end = (offset + length) as usize;
            bytes[start..end].fill(part.fill);
            // A recognisable header so a restored partition can be checked
            // against the one it came from.
            bytes[start + 3..start + 11].copy_from_slice(&part.signature);
            bytes[start + 510] = 0x55;
            bytes[start + 511] = 0xAA;
            // Some variation so compression cannot make the test meaningless
            // by turning every chunk into the same one.
            for (i, b) in bytes[start + 4096..end.min(start + 4096 + 65536)]
                .iter_mut()
                .enumerate()
            {
                *b = (i % 251) as u8;
            }

            entries.push(GptPartitionEntry {
                type_guid: guid_bytes(&part.type_guid),
                unique_guid: guid_bytes(&part.unique_guid),
                starting_lba: offset / u64::from(sector_size),
                ending_lba: (offset + length) / u64::from(sector_size) - 1,
                attributes: 0,
                name: part.name.clone(),
            });
            placed.push((part, offset, length));
            cursor = (offset + length).div_ceil(alignment) * alignment;
        }

        // The partition entry array, primary and secondary.
        let mut array = vec![0u8; array_bytes];
        for (i, e) in entries.iter().enumerate() {
            let at = i * PARTITION_ENTRY_SIZE as usize;
            array[at..at + PARTITION_ENTRY_SIZE as usize].copy_from_slice(&e.to_bytes());
        }

        let disk_guid_text = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let disk_guid = guid_bytes(disk_guid_text);

        let primary = GptHeader {
            my_lba: 1,
            alternate_lba: total_sectors - 1,
            first_usable_lba: 2 + array_sectors as u64,
            last_usable_lba: total_sectors - 2 - array_sectors as u64,
            disk_guid,
            partition_entry_lba: 2,
            num_partition_entries: DEFAULT_ENTRY_COUNT,
            size_of_partition_entry: PARTITION_ENTRY_SIZE,
            partition_entry_array_crc32: crc32(&array),
        };
        let secondary = GptHeader {
            my_lba: total_sectors - 1,
            alternate_lba: 1,
            partition_entry_lba: total_sectors - 1 - array_sectors as u64,
            ..primary
        };

        // Protective MBR, primary header, primary array.
        bytes[..sector].copy_from_slice(&protective_mbr(sector_size, total_sectors));
        bytes[sector..sector * 2].copy_from_slice(&primary.to_sector(sector_size, &array));
        bytes[sector * 2..sector * 2 + array_bytes].copy_from_slice(&array);

        // Secondary array then secondary header, both at the end of the disk.
        let secondary_array_at = (secondary.partition_entry_lba * u64::from(sector_size)) as usize;
        bytes[secondary_array_at..secondary_array_at + array_bytes].copy_from_slice(&array);
        let secondary_header_at = ((total_sectors - 1) * u64::from(sector_size)) as usize;
        bytes[secondary_header_at..secondary_header_at + sector]
            .copy_from_slice(&secondary.to_sector(sector_size, &array));

        Self {
            bytes,
            sector_size,
            disk_guid: disk_guid_text.to_owned(),
            partitions: placed,
        }
    }

    /// The disk as an in memory block device.
    pub fn as_device(&self, name: &str) -> MemoryBlockDevice {
        MemoryBlockDevice::from_vec(name, self.bytes.clone(), self.sector_size)
    }

    /// Total size in bytes.
    pub fn size_bytes(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// The bytes of one partition, by index.
    pub fn partition_bytes(&self, index: usize) -> &[u8] {
        let (_, offset, length) = &self.partitions[index];
        &self.bytes[*offset as usize..(*offset + *length) as usize]
    }
}

/// Parses a GUID string into the byte order a partition table stores.
fn guid_bytes(text: &str) -> [u8; 16] {
    let clean: String = text.chars().filter(|c| *c != '-').collect();
    assert_eq!(clean.len(), 32, "not a GUID: {text}");
    let raw: Vec<u8> = (0..16)
        .map(|i| u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16).expect("hex"))
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
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mjolnir_storage::gpt::parse_primary;

    #[test]
    fn a_synthetic_disk_parses_as_a_real_gpt_disk() {
        let disk = SyntheticDisk::windows_like(512);
        let parsed = parse_primary(&disk.bytes, 512).expect("should parse as GPT");

        assert_eq!(parsed.partitions.len(), 4);
        assert_eq!(parsed.partitions[0].name, "EFI system partition");
        assert_eq!(
            mjolnir_image::format_guid(&parsed.header.disk_guid),
            disk.disk_guid
        );
    }

    #[test]
    fn the_partition_offsets_match_what_the_builder_recorded() {
        let disk = SyntheticDisk::windows_like(512);
        let parsed = parse_primary(&disk.bytes, 512).unwrap();

        for (i, entry) in parsed.partitions.iter().enumerate() {
            let (_, offset, length) = &disk.partitions[i];
            assert_eq!(entry.byte_offset(512).unwrap(), *offset, "partition {i}");
            assert_eq!(entry.byte_length(512).unwrap(), *length, "partition {i}");
        }
    }

    #[test]
    fn partitions_are_aligned_to_a_megabyte() {
        let disk = SyntheticDisk::windows_like(512);
        for (part, offset, length) in &disk.partitions {
            assert_eq!(offset % (1024 * 1024), 0, "{} is misaligned", part.name);
            assert_eq!(length % 512, 0, "{} is not whole sectors", part.name);
        }
    }

    #[test]
    fn partitions_do_not_overlap() {
        let disk = SyntheticDisk::windows_like(512);
        let mut previous_end = 0u64;
        for (part, offset, length) in &disk.partitions {
            assert!(
                *offset >= previous_end,
                "{} overlaps the partition before it",
                part.name
            );
            previous_end = offset + length;
        }
        assert!(previous_end <= disk.size_bytes());
    }

    #[test]
    fn partition_contents_are_recognisable_and_not_all_one_byte() {
        let disk = SyntheticDisk::windows_like(512);
        let windows = disk.partition_bytes(2);
        assert_eq!(&windows[3..11], b"NTFS    ");
        assert_eq!([windows[510], windows[511]], [0x55, 0xAA]);
        // Varied enough that compressing it is a real operation.
        let distinct: std::collections::BTreeSet<u8> =
            windows[4096..8192].iter().copied().collect();
        assert!(
            distinct.len() > 100,
            "contents are too uniform to test with"
        );
    }

    #[test]
    fn a_4k_sector_disk_is_built_correctly() {
        let disk = SyntheticDisk::windows_like(4096);
        let parsed = parse_primary(&disk.bytes, 4096).expect("should parse");
        assert_eq!(parsed.partitions.len(), 4);
        assert_eq!(disk.sector_size, 4096);
    }

    #[test]
    fn the_secondary_gpt_is_at_the_end_of_the_disk() {
        let disk = SyntheticDisk::windows_like(512);
        let last_sector = &disk.bytes[disk.bytes.len() - 512..];
        assert_eq!(&last_sector[0..8], b"EFI PART");
    }

    #[test]
    fn guid_text_round_trips_through_the_on_disk_byte_order() {
        let text = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";
        assert_eq!(mjolnir_image::format_guid(&guid_bytes(text)), text);
    }

    #[test]
    fn the_disk_can_be_read_as_a_block_device() {
        use mjolnir_core::blockio::BlockSource;
        let disk = SyntheticDisk::windows_like(512);
        let mut device = disk.as_device("synthetic");
        assert_eq!(device.size_bytes(), disk.size_bytes());

        let mut buffer = [0u8; 512];
        device.read_exact_at(0, &mut buffer).unwrap();
        assert_eq!(buffer[450], 0xEE, "protective MBR should be at block zero");
    }
}
