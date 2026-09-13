//! Building a small but real NTFS volume, for testing file recovery.
//!
//! [`crate::ntfs::SyntheticNtfs`] describes a volume's *allocation* and is
//! enough to test used block imaging. This builds a volume with an actual
//! master file table in it: file records, names, contents, directories,
//! alternate data streams and the rest, laid out the way NTFS lays them out.
//!
//! It is not a formatter. It writes only what a reader has to understand, which
//! is a boot sector, an `$MFT`, and the records the caller asks for. Windows
//! would not mount the result, and nothing here needs it to: what it is for is
//! proving that MjolnirVSS can read a volume out of a backup and get the right
//! bytes back.

use mjolnir_ntfs::boot::NTFS_OEM_ID;

/// Bytes in one file record. The size NTFS has used since Windows 2000.
pub const RECORD_BYTES: usize = 1024;

/// What one file in the volume is.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    /// Record number in the master file table.
    pub number: u64,
    /// Record number of the directory holding it.
    pub parent: u64,
    /// Its name.
    pub name: String,
    /// Whether it is a directory.
    pub is_directory: bool,
    /// Its contents, when it is a file.
    pub contents: Vec<u8>,
    /// Named streams, as name and contents.
    pub streams: Vec<(String, Vec<u8>)>,
    /// Whether to mark it compressed, which a reader must refuse to extract.
    pub compressed: bool,
    /// Whether to mark it a reparse point.
    pub reparse_point: bool,
    /// How many names the file has, for the hard link count in its record.
    pub hard_links: u16,
    /// Store the contents outside the record even when they would fit.
    pub force_non_resident: bool,
    /// Further names in further directories, which is what a hard link is.
    pub extra_names: Vec<(u64, String)>,
}

impl PlannedFile {
    /// A file with contents.
    pub fn file(number: u64, parent: u64, name: &str, contents: Vec<u8>) -> Self {
        Self {
            number,
            parent,
            name: name.to_owned(),
            is_directory: false,
            contents,
            streams: Vec::new(),
            compressed: false,
            reparse_point: false,
            hard_links: 1,
            force_non_resident: false,
            extra_names: Vec::new(),
        }
    }

    /// A directory.
    pub fn directory(number: u64, parent: u64, name: &str) -> Self {
        Self {
            is_directory: true,
            ..Self::file(number, parent, name, Vec::new())
        }
    }

    /// Adds a named stream.
    pub fn with_stream(mut self, name: &str, contents: Vec<u8>) -> Self {
        self.streams.push((name.to_owned(), contents));
        self
    }

    /// Marks the contents compressed.
    pub fn compressed(mut self) -> Self {
        self.compressed = true;
        self
    }

    /// Marks the file a junction or link.
    pub fn reparse_point(mut self) -> Self {
        self.reparse_point = true;
        self
    }

    /// Stores the contents outside the record.
    pub fn large(mut self) -> Self {
        self.force_non_resident = true;
        self
    }

    /// Gives the file another name in another directory: a hard link.
    pub fn hard_linked_as(mut self, parent: u64, name: &str) -> Self {
        self.extra_names.push((parent, name.to_owned()));
        self.hard_links = 1 + self.extra_names.len() as u16;
        self
    }
}

/// Builds a volume image.
#[derive(Debug, Clone)]
pub struct NtfsVolumeBuilder {
    /// Size of the partition the volume sits in.
    pub partition_bytes: u64,
    /// Logical sector size.
    pub bytes_per_sector: u16,
    /// Sectors per cluster.
    pub sectors_per_cluster: u32,
    /// Cluster the master file table starts at.
    pub mft_cluster: u64,
    /// How many clusters the master file table occupies.
    pub mft_clusters: u64,
    /// First cluster file contents are written at.
    pub data_cluster: u64,
    /// The files.
    pub files: Vec<PlannedFile>,
}

impl NtfsVolumeBuilder {
    /// A volume of `partition_bytes` with the usual geometry.
    pub fn new(partition_bytes: u64) -> Self {
        Self {
            partition_bytes,
            bytes_per_sector: 512,
            sectors_per_cluster: 8,
            mft_cluster: 16,
            mft_clusters: 16,
            data_cluster: 64,
            files: Vec::new(),
        }
    }

    /// Bytes in one cluster.
    pub fn cluster_size(&self) -> u64 {
        u64::from(self.bytes_per_sector) * u64::from(self.sectors_per_cluster)
    }

    /// How many records the master file table holds.
    pub fn record_count(&self) -> u64 {
        self.mft_clusters * self.cluster_size() / RECORD_BYTES as u64
    }

    /// Adds a file or directory to the volume.
    pub fn with_file(mut self, file: PlannedFile) -> Self {
        self.files.push(file);
        self
    }

    /// Lays the volume out and returns the partition's bytes.
    pub fn build(&self) -> Vec<u8> {
        let cluster = self.cluster_size();
        let mut image = vec![0u8; self.partition_bytes as usize];

        // ---- the boot sector, and NTFS's spare copy of it ----------------
        let boot = self.boot_sector();
        image[..boot.len()].copy_from_slice(&boot);
        let spare_at = image.len() - self.bytes_per_sector as usize;
        image[spare_at..].copy_from_slice(&boot);

        // ---- record 0: the master file table itself ----------------------
        let mft_at = (self.mft_cluster * cluster) as usize;
        let mft_runs = encode_run(self.mft_clusters, self.mft_cluster as i64);
        let mft_record = build_record(
            0,
            RecordShape {
                flags: 0x0001,
                hard_links: 1,
                attributes: vec![
                    resident_attribute(
                        mjolnir_ntfs::record::attribute::FILE_NAME,
                        "",
                        &file_name_value(5, "$MFT", 1, 0),
                    ),
                    non_resident_attribute(
                        mjolnir_ntfs::record::attribute::DATA,
                        "",
                        &mft_runs,
                        self.mft_clusters * cluster,
                        0,
                    ),
                ],
            },
            self.bytes_per_sector as usize,
        );
        image[mft_at..mft_at + RECORD_BYTES].copy_from_slice(&mft_record);

        // ---- every other slot: a record that is not in use ----------------
        // A reader has to walk past these, so they are written properly rather
        // than left as zeros, which is what a freshly formatted volume looks
        // like. The root and the files below overwrite the slots they claim.
        for number in 1..self.record_count() {
            let at = mft_at + (number as usize) * RECORD_BYTES;
            let record = build_record(
                number as u32,
                RecordShape {
                    flags: 0x0000,
                    hard_links: 1,
                    attributes: Vec::new(),
                },
                self.bytes_per_sector as usize,
            );
            image[at..at + RECORD_BYTES].copy_from_slice(&record);
        }

        // ---- record 5: the root directory ---------------------------------
        let root_at = mft_at + 5 * RECORD_BYTES;
        let root = build_record(
            5,
            RecordShape {
                flags: 0x0003,
                hard_links: 1,
                attributes: vec![resident_attribute(
                    mjolnir_ntfs::record::attribute::FILE_NAME,
                    "",
                    // The root names itself, with itself as its parent, which
                    // is what NTFS does.
                    &file_name_value(5, ".", 1, 0),
                )],
            },
            self.bytes_per_sector as usize,
        );
        image[root_at..root_at + RECORD_BYTES].copy_from_slice(&root);

        // ---- the files ----------------------------------------------------
        let mut next_data_cluster = self.data_cluster;

        for file in &self.files {
            let mut attributes = vec![resident_attribute(
                mjolnir_ntfs::record::attribute::FILE_NAME,
                "",
                &file_name_value(file.parent, &file.name, 1, file.contents.len() as u64),
            )];

            for (parent, name) in &file.extra_names {
                attributes.push(resident_attribute(
                    mjolnir_ntfs::record::attribute::FILE_NAME,
                    "",
                    &file_name_value(*parent, name, 1, file.contents.len() as u64),
                ));
            }

            if file.reparse_point {
                attributes.push(resident_attribute(
                    mjolnir_ntfs::record::attribute::REPARSE_POINT,
                    "",
                    &[0u8; 24],
                ));
            }

            if !file.is_directory {
                let flags = if file.compressed {
                    mjolnir_ntfs::record::attribute_flags::COMPRESSED
                } else {
                    0
                };
                // Small contents live in the record, which is what NTFS does
                // and what a reader has to handle.
                let resident = file.contents.len() <= 512 && !file.force_non_resident;
                if resident {
                    let mut attribute = resident_attribute(
                        mjolnir_ntfs::record::attribute::DATA,
                        "",
                        &file.contents,
                    );
                    attribute[12..14].copy_from_slice(&flags.to_le_bytes());
                    attributes.push(attribute);
                } else {
                    let clusters = (file.contents.len() as u64).div_ceil(cluster).max(1);
                    let at = (next_data_cluster * cluster) as usize;
                    image[at..at + file.contents.len()].copy_from_slice(&file.contents);
                    let runs = encode_run(clusters, next_data_cluster as i64);
                    attributes.push(non_resident_attribute(
                        mjolnir_ntfs::record::attribute::DATA,
                        "",
                        &runs,
                        file.contents.len() as u64,
                        flags,
                    ));
                    next_data_cluster += clusters;
                }
            } else {
                attributes.push(resident_attribute(
                    mjolnir_ntfs::record::attribute::INDEX_ROOT,
                    "$I30",
                    &[0u8; 32],
                ));
            }

            for (name, contents) in &file.streams {
                attributes.push(resident_attribute(
                    mjolnir_ntfs::record::attribute::DATA,
                    name,
                    contents,
                ));
            }

            let record = build_record(
                file.number as u32,
                RecordShape {
                    flags: if file.is_directory { 0x0003 } else { 0x0001 },
                    hard_links: file.hard_links,
                    attributes,
                },
                self.bytes_per_sector as usize,
            );
            let at = mft_at + (file.number as usize) * RECORD_BYTES;
            image[at..at + RECORD_BYTES].copy_from_slice(&record);
        }

        image
    }

    /// How many clusters of the volume are in use, for a used block plan.
    pub fn allocated_runs(&self) -> Vec<(u64, u64)> {
        let cluster = self.cluster_size();
        let mut runs = vec![(0, 16), (self.mft_cluster, self.mft_clusters)];

        let mut at = self.data_cluster;
        for file in &self.files {
            if file.is_directory || (file.contents.len() <= 512 && !file.force_non_resident) {
                continue;
            }
            let clusters = (file.contents.len() as u64).div_ceil(cluster).max(1);
            runs.push((at, clusters));
            at += clusters;
        }
        runs.sort_unstable();
        runs
    }

    /// The volume's boot sector, parsed, for the used block planner.
    pub fn boot(&self) -> mjolnir_ntfs::NtfsBootSector {
        mjolnir_ntfs::NtfsBootSector::parse(&self.boot_sector())
            .expect("a boot sector this builder wrote should parse")
    }

    /// How many clusters the filesystem covers.
    pub fn cluster_count(&self) -> u64 {
        self.boot().total_clusters()
    }

    /// The allocation the used block planner would read out of `$Bitmap`.
    pub fn allocation(&self) -> mjolnir_ntfs::Allocation {
        mjolnir_ntfs::Allocation::from_runs(self.cluster_count(), self.allocated_runs())
            .expect("the runs this builder plans should form an allocation")
    }

    /// The used block plan for this volume inside a partition of `partition_bytes`.
    pub fn plan(&self) -> mjolnir_ntfs::UsedBlockPlan {
        mjolnir_ntfs::plan_used_blocks(&self.boot(), &self.allocation(), self.partition_bytes)
            .expect("a volume this builder wrote should plan")
    }

    /// The bytes of the volume's boot sector.
    pub fn boot_sector(&self) -> Vec<u8> {
        let sector = u64::from(self.bytes_per_sector);
        let total_sectors = self.partition_bytes / sector - 1;

        let mut s = vec![0u8; self.bytes_per_sector as usize];
        s[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
        s[3..11].copy_from_slice(NTFS_OEM_ID);
        s[11..13].copy_from_slice(&self.bytes_per_sector.to_le_bytes());
        s[13] = u8::try_from(self.sectors_per_cluster).expect("small cluster factor");
        s[40..48].copy_from_slice(&total_sectors.to_le_bytes());
        s[48..56].copy_from_slice(&self.mft_cluster.to_le_bytes());
        s[56..64].copy_from_slice(&2u64.to_le_bytes());
        s[64] = 0xF6; // -10, meaning 1024 byte records
        s[72..80].copy_from_slice(&0x0BAD_C0DE_DEAD_BEEFu64.to_le_bytes());
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }
}

/// The parts of a record that vary.
struct RecordShape {
    flags: u16,
    hard_links: u16,
    attributes: Vec<Vec<u8>>,
}

/// Builds a file record, update sequence included.
fn build_record(number: u32, shape: RecordShape, sector: usize) -> Vec<u8> {
    let mut r = vec![0u8; RECORD_BYTES];
    let usa_offset = 48usize;
    let usa_count = RECORD_BYTES / sector + 1;

    r[0..4].copy_from_slice(b"FILE");
    r[4..6].copy_from_slice(&(usa_offset as u16).to_le_bytes());
    r[6..8].copy_from_slice(&(usa_count as u16).to_le_bytes());
    r[16..18].copy_from_slice(&1u16.to_le_bytes());
    r[18..20].copy_from_slice(&shape.hard_links.to_le_bytes());
    r[22..24].copy_from_slice(&shape.flags.to_le_bytes());
    r[44..48].copy_from_slice(&number.to_le_bytes());

    let attrs_offset = (usa_offset + usa_count * 2).div_ceil(8) * 8;
    r[20..22].copy_from_slice(&(attrs_offset as u16).to_le_bytes());

    let mut at = attrs_offset;
    for a in &shape.attributes {
        assert!(
            at + a.len() + 4 <= RECORD_BYTES,
            "the attributes do not fit in one record"
        );
        r[at..at + a.len()].copy_from_slice(a);
        at += a.len();
    }
    r[at..at + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    at += 4;
    r[24..28].copy_from_slice(&(at as u32).to_le_bytes());

    // The update sequence: the last two bytes of every sector are taken out and
    // kept in the array, and a sequence number is put in their place.
    let usn: u16 = 0x5A5A;
    r[usa_offset..usa_offset + 2].copy_from_slice(&usn.to_le_bytes());
    for s in 0..(usa_count - 1) {
        let tail = (s + 1) * sector - 2;
        let entry = usa_offset + 2 + s * 2;
        r[entry] = r[tail];
        r[entry + 1] = r[tail + 1];
        r[tail..tail + 2].copy_from_slice(&usn.to_le_bytes());
    }
    r
}

/// An attribute whose contents are inside the record.
fn resident_attribute(type_code: u32, name: &str, value: &[u8]) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let name_offset = 24usize;
    let value_offset = (name_offset + units.len() * 2).div_ceil(8) * 8;
    let length = (value_offset + value.len()).div_ceil(8) * 8;

    let mut a = vec![0u8; length];
    a[0..4].copy_from_slice(&type_code.to_le_bytes());
    a[4..8].copy_from_slice(&(length as u32).to_le_bytes());
    a[9] = units.len() as u8;
    a[10..12].copy_from_slice(&(name_offset as u16).to_le_bytes());
    a[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
    a[20..22].copy_from_slice(&(value_offset as u16).to_le_bytes());
    for (i, u) in units.iter().enumerate() {
        a[name_offset + i * 2..name_offset + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
    }
    a[value_offset..value_offset + value.len()].copy_from_slice(value);
    a
}

/// An attribute whose contents are elsewhere on the volume.
fn non_resident_attribute(
    type_code: u32,
    name: &str,
    runs: &[u8],
    size: u64,
    flags: u16,
) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let name_offset = 64usize;
    let runs_offset = (name_offset + units.len() * 2).div_ceil(8) * 8;
    let length = (runs_offset + runs.len()).div_ceil(8) * 8;

    let mut a = vec![0u8; length];
    a[0..4].copy_from_slice(&type_code.to_le_bytes());
    a[4..8].copy_from_slice(&(length as u32).to_le_bytes());
    a[8] = 1;
    a[9] = units.len() as u8;
    a[10..12].copy_from_slice(&(name_offset as u16).to_le_bytes());
    a[12..14].copy_from_slice(&flags.to_le_bytes());
    a[32..34].copy_from_slice(&(runs_offset as u16).to_le_bytes());
    a[48..56].copy_from_slice(&size.to_le_bytes());
    a[56..64].copy_from_slice(&size.to_le_bytes());
    for (i, u) in units.iter().enumerate() {
        a[name_offset + i * 2..name_offset + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
    }
    a[runs_offset..runs_offset + runs.len()].copy_from_slice(runs);
    a
}

/// The contents of a `$FILE_NAME` attribute.
fn file_name_value(parent: u64, name: &str, namespace: u8, size: u64) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let mut v = vec![0u8; 66 + units.len() * 2];
    v[0..8].copy_from_slice(&(parent | (1u64 << 48)).to_le_bytes());
    v[40..48].copy_from_slice(&size.to_le_bytes());
    v[48..56].copy_from_slice(&size.to_le_bytes());
    v[64] = units.len() as u8;
    v[65] = namespace;
    for (i, u) in units.iter().enumerate() {
        v[66 + i * 2..66 + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
    }
    v
}

/// One run, encoded the way NTFS encodes one.
fn encode_run(clusters: u64, at: i64) -> Vec<u8> {
    let length_bytes = minimal_unsigned(clusters);
    let offset_bytes = minimal_signed(at);
    let mut out = vec![((offset_bytes.len() as u8) << 4) | length_bytes.len() as u8];
    out.extend_from_slice(&length_bytes);
    out.extend_from_slice(&offset_bytes);
    out.push(0);
    out
}

fn minimal_unsigned(value: u64) -> Vec<u8> {
    let mut bytes = value.to_le_bytes().to_vec();
    while bytes.len() > 1 && bytes[bytes.len() - 1] == 0 && bytes[bytes.len() - 2] & 0x80 == 0 {
        bytes.pop();
    }
    bytes
}

fn minimal_signed(value: i64) -> Vec<u8> {
    let mut bytes = value.to_le_bytes().to_vec();
    while bytes.len() > 1 {
        let last = bytes[bytes.len() - 1];
        let next = bytes[bytes.len() - 2];
        let redundant = (last == 0x00 && next & 0x80 == 0) || (last == 0xFF && next & 0x80 != 0);
        if !redundant {
            break;
        }
        bytes.pop();
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use mjolnir_core::blockio::MemoryBlockDevice;
    use mjolnir_core::cancel::CancelToken;
    use mjolnir_ntfs::volume::{FileIndex, Volume};

    fn sample() -> NtfsVolumeBuilder {
        NtfsVolumeBuilder::new(8 * 1024 * 1024)
            .with_file(PlannedFile::file(
                6,
                5,
                "readme.txt",
                b"hello world".to_vec(),
            ))
            .with_file(PlannedFile::directory(7, 5, "Documents"))
            .with_file(
                PlannedFile::file(8, 7, "notes.txt", b"some notes".to_vec())
                    .with_stream("hidden", b"the stream".to_vec()),
            )
            .with_file(PlannedFile::file(9, 7, "big.bin", vec![0xAB; 20000]).large())
    }

    #[test]
    fn the_volume_it_builds_can_be_read_back() {
        let builder = sample();
        let mut disk = MemoryBlockDevice::from_vec("volume", builder.build(), 512);
        let mut volume = Volume::open(&mut disk).expect("the volume should open");
        let index = FileIndex::build(&mut volume, &CancelToken::new()).expect("the tree");

        assert_eq!(index.path_of(6).as_deref(), Some("\\readme.txt"));
        assert_eq!(index.path_of(8).as_deref(), Some("\\Documents\\notes.txt"));

        let names: Vec<&str> = index
            .children_of(5)
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert!(names.contains(&"readme.txt"));
        assert!(names.contains(&"Documents"));
    }

    #[test]
    fn contents_come_back_byte_for_byte() {
        let builder = sample();
        let mut disk = MemoryBlockDevice::from_vec("volume", builder.build(), 512);
        let mut volume = Volume::open(&mut disk).unwrap();

        let record = volume.record(6).unwrap();
        let data = record.data().unwrap().clone();
        assert_eq!(
            volume.read_attribute_fully(&data, 1 << 20).unwrap(),
            b"hello world".to_vec()
        );

        // And the one that does not fit inside its record.
        let record = volume.record(9).unwrap();
        let data = record.data().unwrap().clone();
        assert!(!data.is_resident());
        assert_eq!(
            volume.read_attribute_fully(&data, 1 << 20).unwrap(),
            vec![0xAB; 20000]
        );
    }

    #[test]
    fn a_named_stream_is_there_and_separate() {
        let builder = sample();
        let mut disk = MemoryBlockDevice::from_vec("volume", builder.build(), 512);
        let mut volume = Volume::open(&mut disk).unwrap();

        let record = volume.record(8).unwrap();
        let streams = record.alternate_streams();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].name, "hidden");
        assert_eq!(
            streams[0].resident_value.as_deref(),
            Some(&b"the stream"[..])
        );
    }

    #[test]
    fn the_allocated_runs_cover_everything_that_was_written() {
        let builder = sample();
        let runs = builder.allocated_runs();
        assert!(runs.iter().any(|(start, _)| *start == 0));
        assert!(runs.iter().any(|(start, _)| *start == builder.mft_cluster));
        // The one large file needs clusters of its own.
        assert!(runs.iter().any(|(start, _)| *start >= builder.data_cluster));
    }

    #[test]
    fn the_spare_boot_sector_is_at_the_end_of_the_partition() {
        let builder = sample();
        let image = builder.build();
        let sector = builder.bytes_per_sector as usize;
        assert_eq!(&image[..sector], &image[image.len() - sector..]);
        assert_eq!(&image[3..11], NTFS_OEM_ID);
    }
}
