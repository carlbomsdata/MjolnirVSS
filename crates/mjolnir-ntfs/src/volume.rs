//! Reading an NTFS volume, from a backup or from anywhere else.
//!
//! This is what file recovery is built on. It reads through
//! [`BlockSource`](mjolnir_core::blockio::BlockSource), so the same code reads a
//! volume out of a backup, out of a file, or out of a buffer in a test. There is
//! no path from here to a disk and nothing here writes.
//!
//! # How a file is found
//!
//! Rather than walking the directory indexes, which are B-trees with their own
//! on disk format, this walks the master file table from end to end and builds
//! the tree from what each record says its parent is. That is slower to start
//! and much simpler to be sure of, and it has two properties that matter for
//! recovery:
//!
//! * a file whose directory's index is damaged is still found, because the file
//!   record knows its own parent;
//! * a file with several names in several directories appears in all of them,
//!   which is what a hard link is.
//!
//! # What it refuses to do
//!
//! It will not hand back the contents of a compressed or encrypted attribute,
//! because it does not implement either and returning the raw clusters would be
//! returning something that is not the file. It says which, by name, so the
//! operator knows what happened rather than finding out later.

use std::collections::BTreeMap;

use mjolnir_core::blockio::BlockSource;
use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::math;

use crate::boot::NtfsBootSector;
use crate::record::{Attribute, FileRecord, MftReference};
use crate::runs::RunList;

/// Largest master file table this will walk, in records.
///
/// Sixteen million records is a volume with sixteen million files on it. The
/// limit exists so that a damaged `$MFT` claiming an absurd size produces an
/// error rather than an attempt to read for an hour.
pub const MAX_RECORDS: u64 = 16_000_000;

/// An NTFS volume opened for reading.
pub struct Volume<'a> {
    source: &'a mut dyn BlockSource,
    boot: NtfsBootSector,
    mft_runs: RunList,
    record_size: u64,
    volume_bytes: u64,
}

impl<'a> Volume<'a> {
    /// Opens a volume by reading its boot sector and its master file table.
    pub fn open(source: &'a mut dyn BlockSource) -> Result<Self> {
        let mut sector = vec![0u8; 512];
        source.read_exact_at(0, &mut sector)?;
        let boot = NtfsBootSector::parse(&sector)?;

        let record_size = u64::from(boot.bytes_per_file_record);
        if record_size == 0 || record_size > 64 * 1024 {
            return Err(unreadable(format!(
                "the volume says a file record is {record_size} bytes, which is not a size NTFS uses"
            )));
        }
        let volume_bytes = boot.volume_bytes()?;

        // The first record of the master file table describes the table
        // itself. Everything else is found through it.
        let mut first = vec![0u8; record_size as usize];
        source.read_exact_at(boot.mft_offset()?, &mut first)?;
        let record = FileRecord::parse(&first, u32::from(boot.bytes_per_sector))?;

        let data = record.data().ok_or_else(|| {
            unreadable("the master file table's own record does not say where it is".to_owned())
        })?;
        let mft_runs = data.runs.clone().ok_or_else(|| {
            unreadable("the master file table claims to fit inside its own record".to_owned())
        })?;

        Ok(Self {
            source,
            boot,
            mft_runs,
            record_size,
            volume_bytes,
        })
    }

    /// The volume's boot sector.
    pub fn boot(&self) -> &NtfsBootSector {
        &self.boot
    }

    /// Bytes in one cluster.
    pub fn cluster_size(&self) -> u64 {
        self.boot.bytes_per_cluster()
    }

    /// How many records the master file table holds.
    pub fn record_count(&self) -> u64 {
        let bytes = self.mft_runs.cluster_count() * self.cluster_size();
        bytes / self.record_size
    }

    /// Reads and parses one record of the master file table.
    pub fn record(&mut self, number: u64) -> Result<FileRecord> {
        let offset = math::mul_u64("file record offset", number, self.record_size)?;
        let mut bytes = vec![0u8; self.record_size as usize];
        self.read_from_runs(
            &self.mft_runs.clone(),
            u64::MAX,
            u64::MAX,
            offset,
            &mut bytes,
        )?;
        FileRecord::parse(&bytes, u32::from(self.boot.bytes_per_sector))
    }

    /// Reads part of an attribute's contents.
    ///
    /// Refuses a compressed or encrypted attribute by name rather than handing
    /// back clusters that are not the file.
    pub fn read_attribute(
        &mut self,
        attribute: &Attribute,
        offset: u64,
        buffer: &mut [u8],
    ) -> Result<()> {
        if attribute.is_encrypted() {
            return Err(unsupported_contents(
                "encrypted",
                "the file is protected by the Encrypting File System, and the key lives in a Windows profile rather than in the backup",
            ));
        }
        if attribute.is_compressed() {
            return Err(unsupported_contents(
                "compressed",
                "MjolnirVSS does not decompress NTFS compressed files yet, and handing back the stored clusters would hand back something that is not the file",
            ));
        }

        if let Some(value) = &attribute.resident_value {
            let start = math::to_usize("attribute offset", offset)?;
            let end = start
                .checked_add(buffer.len())
                .ok_or_else(|| unreadable("a read runs past what can be addressed".to_owned()))?;
            if end > value.len() {
                return Err(unreadable(format!(
                    "a read of {} bytes at {offset} runs past the {} byte attribute",
                    buffer.len(),
                    value.len()
                )));
            }
            buffer.copy_from_slice(&value[start..end]);
            return Ok(());
        }

        let runs = attribute.runs.clone().ok_or_else(|| {
            unreadable("an attribute has neither contents nor a place to find them".to_owned())
        })?;
        self.read_from_runs(
            &runs,
            attribute.data_size,
            attribute.initialized_size,
            offset,
            buffer,
        )
    }

    /// Reads through a run list, filling holes and uninitialised space with
    /// zeros.
    fn read_from_runs(
        &mut self,
        runs: &RunList,
        data_size: u64,
        initialized_size: u64,
        offset: u64,
        buffer: &mut [u8],
    ) -> Result<()> {
        if buffer.is_empty() {
            return Ok(());
        }
        let end = math::add_u64("attribute read end", offset, buffer.len() as u64)?;
        if data_size != u64::MAX && end > data_size {
            return Err(unreadable(format!(
                "a read of {} bytes at {offset} runs past the {data_size} byte attribute",
                buffer.len()
            )));
        }

        let cluster = self.cluster_size();
        buffer.fill(0);
        let mut done = 0u64;

        while done < buffer.len() as u64 {
            let at = offset + done;

            // Everything past the initialised length is defined to be zeros,
            // whatever happens to be in the clusters.
            if initialized_size != u64::MAX && at >= initialized_size {
                break;
            }

            let vcn = at / cluster;
            let into_cluster = at % cluster;
            let Some((run, run_offset)) = runs.locate(vcn) else {
                return Err(unreadable(format!(
                    "cluster {vcn} of the file is not described by its layout"
                )));
            };

            // How much of this run is left from here.
            let run_remaining = (run.length - run_offset) * cluster - into_cluster;
            let mut take = run_remaining.min(buffer.len() as u64 - done);
            if initialized_size != u64::MAX {
                take = take.min(initialized_size - at);
            }
            let take_usize = math::to_usize("attribute read length", take)?;

            match run.lcn {
                // A hole reads as zeros, which the buffer already holds.
                None => {}
                Some(lcn) => {
                    let disk_offset = math::add_u64(
                        "attribute read offset",
                        math::mul_u64("attribute read offset", lcn + run_offset, cluster)?,
                        into_cluster,
                    )?;
                    if self.volume_bytes > 0 && disk_offset.saturating_add(take) > self.volume_bytes
                    {
                        return Err(unreadable(format!(
                            "a file's layout points at byte {disk_offset} of a {} byte volume",
                            self.volume_bytes
                        )));
                    }
                    let start = math::to_usize("attribute read cursor", done)?;
                    self.source
                        .read_exact_at(disk_offset, &mut buffer[start..start + take_usize])?;
                }
            }
            done += take;
        }
        Ok(())
    }

    /// Reads a whole attribute.
    ///
    /// Refuses anything larger than `limit`, because a recovery interface
    /// asking for "the contents" of a two terabyte file should be told rather
    /// than allowed to exhaust memory.
    pub fn read_attribute_fully(&mut self, attribute: &Attribute, limit: u64) -> Result<Vec<u8>> {
        if attribute.data_size > limit {
            return Err(unreadable(format!(
                "this file is {} bytes, more than the {limit} this operation reads at once",
                attribute.data_size
            )));
        }
        let mut out = vec![0u8; math::to_usize("attribute size", attribute.data_size)?];
        self.read_attribute(attribute, 0, &mut out)?;
        Ok(out)
    }
}

/// One file or directory, as the index remembers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    /// Its record number in the master file table.
    pub number: u64,
    /// The directory holding this name.
    pub parent: u64,
    /// The name, without any path.
    pub name: String,
    /// Whether it is a directory.
    pub is_directory: bool,
    /// Size of the contents.
    pub size: u64,
    /// Whether it is a junction, a symbolic link, or similar.
    pub is_reparse_point: bool,
    /// Whether the contents are compressed, which this version cannot read.
    pub is_compressed: bool,
    /// Whether the contents are encrypted, which this version cannot read.
    pub is_encrypted: bool,
    /// Whether the contents have holes, which read as zeros.
    pub is_sparse: bool,
    /// Whether the file has more than one name.
    pub is_hard_linked: bool,
    /// Named streams hanging off this file, as name and size.
    pub streams: Vec<(String, u64)>,
}

impl IndexEntry {
    /// Whether this version can hand back the contents.
    pub fn is_readable(&self) -> bool {
        !self.is_compressed && !self.is_encrypted
    }

    /// Why the contents cannot be read, when they cannot.
    pub fn why_unreadable(&self) -> Option<&'static str> {
        if self.is_encrypted {
            Some("it is encrypted with the Encrypting File System")
        } else if self.is_compressed {
            Some("it is stored compressed, which this version cannot read")
        } else {
            None
        }
    }
}

/// Every file on a volume, and the tree they form.
#[derive(Debug, Clone, Default)]
pub struct FileIndex {
    entries: BTreeMap<u64, Vec<IndexEntry>>,
    children: BTreeMap<u64, Vec<u64>>,
    /// Records that could not be read, by number, with the reason.
    pub unreadable: Vec<(u64, String)>,
    /// How many records were looked at.
    pub records_scanned: u64,
}

impl FileIndex {
    /// Walks the whole master file table and builds the tree.
    pub fn build(volume: &mut Volume<'_>, cancel: &CancelToken) -> Result<Self> {
        let count = volume.record_count().min(MAX_RECORDS);
        let mut index = FileIndex::default();

        for number in 0..count {
            if number % 512 == 0 {
                cancel.check()?;
            }
            index.records_scanned += 1;

            let record = match volume.record(number) {
                Ok(record) => record,
                Err(e) => {
                    // A record that cannot be read is one file lost, not a
                    // failed recovery. It is counted and named.
                    if index.unreadable.len() < 1000 {
                        index.unreadable.push((number, e.why().to_owned()));
                    }
                    continue;
                }
            };

            if !record.in_use {
                continue;
            }
            // A record that continues another belongs to that one's file.
            if record.base_record.is_some() {
                continue;
            }

            let data = record.data();
            let streams: Vec<(String, u64)> = record
                .alternate_streams()
                .iter()
                .map(|a| (a.name.clone(), a.data_size))
                .collect();

            for name in record.names() {
                if !name.namespace.is_long_name() {
                    continue;
                }
                let entry = IndexEntry {
                    number,
                    parent: name.parent.number,
                    name: name.name.clone(),
                    is_directory: record.is_directory,
                    size: data.map(|d| d.data_size).unwrap_or(name.real_size),
                    is_reparse_point: record.is_reparse_point(),
                    is_compressed: data.map(Attribute::is_compressed).unwrap_or(false),
                    is_encrypted: data.map(Attribute::is_encrypted).unwrap_or(false),
                    is_sparse: data.map(Attribute::is_sparse).unwrap_or(false),
                    is_hard_linked: record.hard_link_count > 1,
                    streams: streams.clone(),
                };
                index.children.entry(entry.parent).or_default().push(number);
                index.entries.entry(number).or_default().push(entry);
            }
        }

        for list in index.children.values_mut() {
            list.sort_unstable();
            list.dedup();
        }
        Ok(index)
    }

    /// How many files and directories were found.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing was found.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The names a record is known by.
    pub fn names_of(&self, number: u64) -> &[IndexEntry] {
        self.entries.get(&number).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The first entry for a record, which is the one a listing shows.
    pub fn entry(&self, number: u64) -> Option<&IndexEntry> {
        self.entries.get(&number).and_then(|list| list.first())
    }

    /// What is directly inside a directory.
    pub fn children_of(&self, number: u64) -> Vec<&IndexEntry> {
        let mut out = Vec::new();
        for child in self.children.get(&number).map(Vec::as_slice).unwrap_or(&[]) {
            // The root directory names itself, with itself as its parent. It
            // is not one of its own children, and listing it as one would make
            // a browser recurse forever.
            if *child == number {
                continue;
            }
            for entry in self.names_of(*child) {
                if entry.parent == number {
                    out.push(entry);
                }
            }
        }
        out.sort_by(|a, b| {
            b.is_directory
                .cmp(&a.is_directory)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        out
    }

    /// The path of a record, from the root of the volume.
    ///
    /// Returns `None` for a record whose chain of parents does not reach the
    /// root, which happens when a directory above it could not be read.
    pub fn path_of(&self, number: u64) -> Option<String> {
        if number == MftReference::ROOT {
            return Some("\\".to_owned());
        }
        let mut parts = Vec::new();
        let mut at = number;
        // Bounded, so a record whose parent chain forms a loop, which a damaged
        // volume can produce, stops rather than spinning.
        for _ in 0..256 {
            let entry = self.entry(at)?;
            parts.push(entry.name.clone());
            if entry.parent == MftReference::ROOT {
                parts.reverse();
                return Some(format!("\\{}", parts.join("\\")));
            }
            if entry.parent == at {
                return None;
            }
            at = entry.parent;
        }
        None
    }

    /// Finds an entry by path, case insensitively.
    ///
    /// The path is matched from the root of the volume, with either separator.
    pub fn resolve(&self, path: &str) -> Option<&IndexEntry> {
        let trimmed = path.trim_matches(|c| c == '\\' || c == '/');
        if trimmed.is_empty() {
            return self.entry(MftReference::ROOT);
        }

        let mut at = MftReference::ROOT;
        let mut found = None;
        for part in trimmed.split(['\\', '/']) {
            if part.is_empty() || part == "." {
                continue;
            }
            // A path from a backup is untrusted input; `..` is refused rather
            // than followed, because a caller turning the result into a place
            // to write would otherwise be led out of the folder it chose.
            if part == ".." {
                return None;
            }
            let next = self
                .children_of(at)
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(part))?;
            at = next.number;
            found = Some(next);
        }
        found
    }
}

fn unreadable(detail: String) -> Error {
    Error::new(
        ExitCode::CorruptBackup,
        "part of the filesystem could not be read",
        detail,
        "the rest of the backup is unaffected; try a different file, or restore the whole disk",
    )
}

fn unsupported_contents(what: &str, why: &str) -> Error {
    Error::new(
        ExitCode::Unsupported,
        format!("this file is {what} and cannot be extracted"),
        why.to_owned(),
        "restore the whole disk instead, which reproduces the file exactly as it was",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::attribute;
    use mjolnir_core::blockio::MemoryBlockDevice;

    /// Builds a tiny NTFS volume in memory: a boot sector, a master file table
    /// of a few records, and whatever file contents the records point at.
    struct VolumeBuilder {
        cluster_size: u64,
        sector_size: u16,
        record_size: u64,
        total_clusters: u64,
        mft_cluster: u64,
        records: Vec<Vec<u8>>,
        data: Vec<(u64, Vec<u8>)>,
    }

    impl VolumeBuilder {
        fn new() -> Self {
            Self {
                cluster_size: 4096,
                sector_size: 512,
                record_size: 1024,
                total_clusters: 1024,
                mft_cluster: 16,
                records: Vec::new(),
                data: Vec::new(),
            }
        }

        fn record(&mut self, bytes: Vec<u8>) -> u64 {
            let number = self.records.len() as u64;
            self.records.push(bytes);
            number
        }

        /// Puts file contents at a cluster and returns the cluster number.
        fn contents(&mut self, cluster: u64, bytes: Vec<u8>) -> u64 {
            self.data.push((cluster, bytes));
            cluster
        }

        fn build(&self) -> Vec<u8> {
            let size = (self.total_clusters * self.cluster_size) as usize;
            let mut disk = vec![0u8; size];

            // The boot sector.
            let sectors_per_cluster = self.cluster_size / u64::from(self.sector_size);
            let total_sectors = self.total_clusters * sectors_per_cluster;
            let mut boot = vec![0u8; 512];
            boot[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
            boot[3..11].copy_from_slice(crate::boot::NTFS_OEM_ID);
            boot[11..13].copy_from_slice(&self.sector_size.to_le_bytes());
            boot[13] = sectors_per_cluster as u8;
            boot[40..48].copy_from_slice(&total_sectors.to_le_bytes());
            boot[48..56].copy_from_slice(&self.mft_cluster.to_le_bytes());
            boot[56..64].copy_from_slice(&2u64.to_le_bytes());
            boot[64] = 0xF6; // 1024 byte records
            boot[510] = 0x55;
            boot[511] = 0xAA;
            disk[0..512].copy_from_slice(&boot);

            // The master file table, at its cluster.
            let mft_at = (self.mft_cluster * self.cluster_size) as usize;
            for (i, record) in self.records.iter().enumerate() {
                let at = mft_at + i * self.record_size as usize;
                disk[at..at + record.len()].copy_from_slice(record);
            }

            for (cluster, bytes) in &self.data {
                let at = (cluster * self.cluster_size) as usize;
                disk[at..at + bytes.len()].copy_from_slice(bytes);
            }
            disk
        }
    }

    /// A file record, built the way `record.rs` tests build one.
    fn make_record(number: u32, flags: u16, hard_links: u16, attributes: Vec<Vec<u8>>) -> Vec<u8> {
        let size = 1024usize;
        let sector = 512usize;
        let mut r = vec![0u8; size];
        let usa_offset = 48usize;
        let usa_count = size / sector + 1;

        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&(usa_offset as u16).to_le_bytes());
        r[6..8].copy_from_slice(&(usa_count as u16).to_le_bytes());
        r[16..18].copy_from_slice(&1u16.to_le_bytes());
        r[18..20].copy_from_slice(&hard_links.to_le_bytes());
        r[22..24].copy_from_slice(&flags.to_le_bytes());
        r[44..48].copy_from_slice(&number.to_le_bytes());

        let attrs_offset = (usa_offset + usa_count * 2).div_ceil(8) * 8;
        r[20..22].copy_from_slice(&(attrs_offset as u16).to_le_bytes());

        let mut at = attrs_offset;
        for a in &attributes {
            r[at..at + a.len()].copy_from_slice(a);
            at += a.len();
        }
        r[at..at + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        at += 4;
        r[24..28].copy_from_slice(&(at as u32).to_le_bytes());

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

    fn resident(type_code: u32, name: &str, value: &[u8]) -> Vec<u8> {
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

    fn non_resident(type_code: u32, name: &str, runs: &[u8], size: u64, flags: u16) -> Vec<u8> {
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

    fn file_name(parent: u64, name: &str, namespace: u8, size: u64) -> Vec<u8> {
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

    /// A one byte run header for "n clusters at cluster c".
    fn run(clusters: u8, at: u16) -> Vec<u8> {
        vec![0x21, clusters, (at & 0xFF) as u8, (at >> 8) as u8, 0x00]
    }

    /// A volume with a root directory, a file, a directory and a file in it.
    fn sample_volume() -> (Vec<u8>, Vec<u8>) {
        let mut builder = VolumeBuilder::new();
        let contents = b"the contents of the file".to_vec();
        let mut cluster_data = vec![0u8; 4096];
        cluster_data[..contents.len()].copy_from_slice(&contents);
        builder.contents(100, cluster_data);

        // 0: $MFT itself, eight clusters at cluster 16.
        let mft = make_record(
            0,
            0x0001,
            1,
            vec![
                resident(attribute::FILE_NAME, "", &file_name(5, "$MFT", 1, 0)),
                non_resident(attribute::DATA, "", &run(8, 16), 8 * 4096, 0),
            ],
        );
        builder.record(mft);

        for n in 1..5u32 {
            builder.record(make_record(n, 0x0000, 1, vec![]));
        }

        // 5: the root directory.
        builder.record(make_record(
            5,
            0x0003,
            1,
            vec![resident(attribute::FILE_NAME, "", &file_name(5, ".", 1, 0))],
        ));

        // 6: a file in the root.
        builder.record(make_record(
            6,
            0x0001,
            1,
            vec![
                resident(
                    attribute::FILE_NAME,
                    "",
                    &file_name(5, "readme.txt", 1, contents.len() as u64),
                ),
                non_resident(attribute::DATA, "", &run(1, 100), contents.len() as u64, 0),
            ],
        ));

        // 7: a directory in the root.
        builder.record(make_record(
            7,
            0x0003,
            1,
            vec![resident(
                attribute::FILE_NAME,
                "",
                &file_name(5, "Documents", 1, 0),
            )],
        ));

        // 8: a file inside that directory, with an alternate stream.
        builder.record(make_record(
            8,
            0x0001,
            1,
            vec![
                resident(attribute::FILE_NAME, "", &file_name(7, "notes.txt", 1, 5)),
                resident(attribute::DATA, "", b"notes"),
                resident(attribute::DATA, "hidden", b"secret"),
            ],
        ));

        (builder.build(), contents)
    }

    fn open(disk: &mut MemoryBlockDevice) -> Volume<'_> {
        Volume::open(disk).expect("the synthetic volume should open")
    }

    #[test]
    fn a_volume_opens_and_finds_its_master_file_table() {
        let (bytes, _) = sample_volume();
        let mut disk = MemoryBlockDevice::from_vec("test", bytes, 512);
        let volume = open(&mut disk);

        assert_eq!(volume.cluster_size(), 4096);
        assert_eq!(volume.boot().bytes_per_sector, 512);
        // Eight clusters of 4096 bytes, in 1024 byte records.
        assert_eq!(volume.record_count(), 32);
    }

    #[test]
    fn records_are_read_by_number() {
        let (bytes, _) = sample_volume();
        let mut disk = MemoryBlockDevice::from_vec("test", bytes, 512);
        let mut volume = open(&mut disk);

        let root = volume.record(5).unwrap();
        assert!(root.is_directory);
        assert!(root.in_use);

        let file = volume.record(6).unwrap();
        assert_eq!(file.best_name().unwrap().name, "readme.txt");
    }

    #[test]
    fn a_files_contents_are_read_through_its_runs() {
        let (bytes, contents) = sample_volume();
        let mut disk = MemoryBlockDevice::from_vec("test", bytes, 512);
        let mut volume = open(&mut disk);

        let record = volume.record(6).unwrap();
        let data = record.data().unwrap().clone();
        let read = volume.read_attribute_fully(&data, 1 << 20).unwrap();
        assert_eq!(read, contents);
    }

    #[test]
    fn resident_contents_are_read_without_touching_the_disk() {
        let (bytes, _) = sample_volume();
        let mut disk = MemoryBlockDevice::from_vec("test", bytes, 512);
        let mut volume = open(&mut disk);

        let record = volume.record(8).unwrap();
        let data = record.data().unwrap().clone();
        assert!(data.is_resident());
        assert_eq!(
            volume.read_attribute_fully(&data, 1 << 20).unwrap(),
            b"notes".to_vec()
        );
    }

    #[test]
    fn the_index_builds_a_tree() {
        let (bytes, _) = sample_volume();
        let mut disk = MemoryBlockDevice::from_vec("test", bytes, 512);
        let mut volume = open(&mut disk);
        let index = FileIndex::build(&mut volume, &CancelToken::new()).unwrap();

        assert!(index.len() >= 4);
        assert_eq!(index.path_of(6).as_deref(), Some("\\readme.txt"));
        assert_eq!(index.path_of(8).as_deref(), Some("\\Documents\\notes.txt"));
        assert_eq!(index.path_of(MftReference::ROOT).as_deref(), Some("\\"));

        let root_children: Vec<&str> = index
            .children_of(MftReference::ROOT)
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert!(root_children.contains(&"readme.txt"));
        assert!(root_children.contains(&"Documents"));
        // Directories come first.
        assert_eq!(root_children[0], "Documents");
    }

    #[test]
    fn a_path_resolves_case_insensitively_and_with_either_separator() {
        let (bytes, _) = sample_volume();
        let mut disk = MemoryBlockDevice::from_vec("test", bytes, 512);
        let mut volume = open(&mut disk);
        let index = FileIndex::build(&mut volume, &CancelToken::new()).unwrap();

        for path in [
            "\\Documents\\notes.txt",
            "Documents/notes.txt",
            "/DOCUMENTS/NOTES.TXT",
            "\\documents\\Notes.Txt",
        ] {
            let entry = index.resolve(path).unwrap_or_else(|| panic!("{path}"));
            assert_eq!(entry.number, 8, "{path}");
        }
    }

    /// A path out of a backup is untrusted. Following `..` would let a caller
    /// that turns the result into a place to write escape the folder it chose.
    #[test]
    fn a_path_containing_a_parent_reference_is_refused() {
        let (bytes, _) = sample_volume();
        let mut disk = MemoryBlockDevice::from_vec("test", bytes, 512);
        let mut volume = open(&mut disk);
        let index = FileIndex::build(&mut volume, &CancelToken::new()).unwrap();

        assert!(index.resolve("\\Documents\\..\\readme.txt").is_none());
        assert!(index.resolve("..").is_none());
        assert!(index.resolve("\\..\\..\\Windows").is_none());
    }

    #[test]
    fn an_alternate_stream_is_listed_with_its_file() {
        let (bytes, _) = sample_volume();
        let mut disk = MemoryBlockDevice::from_vec("test", bytes, 512);
        let mut volume = open(&mut disk);
        let index = FileIndex::build(&mut volume, &CancelToken::new()).unwrap();

        let entry = index.resolve("\\Documents\\notes.txt").unwrap();
        assert_eq!(entry.streams, vec![("hidden".to_owned(), 6)]);
        assert_eq!(entry.size, 5);
    }

    /// Reading a compressed or encrypted file must fail by name rather than
    /// hand back the stored clusters, which are not the file.
    #[test]
    fn compressed_and_encrypted_contents_are_refused_by_name() {
        use crate::record::attribute_flags;

        let (bytes, _) = sample_volume();
        let mut disk = MemoryBlockDevice::from_vec("test", bytes, 512);
        let mut volume = open(&mut disk);

        for (flag, word) in [
            (attribute_flags::COMPRESSED, "compressed"),
            (attribute_flags::ENCRYPTED, "encrypted"),
        ] {
            let attribute = Attribute {
                type_code: attribute::DATA,
                name: String::new(),
                flags: flag,
                resident_value: None,
                runs: Some(RunList::parse(&run(1, 100), 0).unwrap()),
                data_size: 16,
                initialized_size: 16,
                starting_vcn: 0,
            };
            let err = volume
                .read_attribute_fully(&attribute, 1 << 20)
                .unwrap_err();
            assert!(err.what().contains(word), "{}", err.what());
            assert_eq!(err.exit(), ExitCode::Unsupported);
        }
    }

    #[test]
    fn a_hole_reads_as_zeros() {
        let (bytes, _) = sample_volume();
        let mut disk = MemoryBlockDevice::from_vec("test", bytes, 512);
        let mut volume = open(&mut disk);

        // One real cluster at 100, then a hole of two clusters.
        let mut runs = run(1, 100);
        runs.pop(); // drop the terminator
        runs.extend_from_slice(&[0x01, 0x02, 0x00]); // sparse run, then the end

        let attribute = Attribute {
            type_code: attribute::DATA,
            name: String::new(),
            flags: crate::record::attribute_flags::SPARSE,
            resident_value: None,
            runs: Some(RunList::parse(&runs, 0).unwrap()),
            data_size: 3 * 4096,
            initialized_size: 3 * 4096,
            starting_vcn: 0,
        };

        let read = volume.read_attribute_fully(&attribute, 1 << 20).unwrap();
        assert_eq!(read.len(), 3 * 4096);
        assert_eq!(&read[..24], b"the contents of the file");
        assert!(
            read[4096..].iter().all(|b| *b == 0),
            "the hole did not read as zeros"
        );
    }

    /// Bytes past the initialised length are zeros even where clusters are
    /// allocated, which is how NTFS represents a file that was extended and
    /// never written to.
    #[test]
    fn uninitialised_space_reads_as_zeros() {
        let (bytes, _) = sample_volume();
        let mut disk = MemoryBlockDevice::from_vec("test", bytes, 512);
        let mut volume = open(&mut disk);

        let attribute = Attribute {
            type_code: attribute::DATA,
            name: String::new(),
            flags: 0,
            resident_value: None,
            runs: Some(RunList::parse(&run(1, 100), 0).unwrap()),
            data_size: 4096,
            initialized_size: 10,
            starting_vcn: 0,
        };
        let read = volume.read_attribute_fully(&attribute, 1 << 20).unwrap();
        assert_eq!(&read[..10], &b"the conten"[..]);
        assert!(read[10..].iter().all(|b| *b == 0));
    }

    #[test]
    fn reading_past_the_end_of_an_attribute_is_refused() {
        let (bytes, _) = sample_volume();
        let mut disk = MemoryBlockDevice::from_vec("test", bytes, 512);
        let mut volume = open(&mut disk);

        let record = volume.record(6).unwrap();
        let data = record.data().unwrap().clone();
        let mut buffer = vec![0u8; data.data_size as usize + 1];
        assert!(volume.read_attribute(&data, 0, &mut buffer).is_err());
        assert!(volume.read_attribute(&data, 1, &mut buffer[..1]).is_ok());
    }

    #[test]
    fn a_very_large_file_is_refused_rather_than_read_into_memory() {
        let (bytes, _) = sample_volume();
        let mut disk = MemoryBlockDevice::from_vec("test", bytes, 512);
        let mut volume = open(&mut disk);

        let attribute = Attribute {
            type_code: attribute::DATA,
            name: String::new(),
            flags: 0,
            resident_value: None,
            runs: Some(RunList::parse(&run(1, 100), 0).unwrap()),
            data_size: 1 << 40,
            initialized_size: 1 << 40,
            starting_vcn: 0,
        };
        let err = volume
            .read_attribute_fully(&attribute, 1 << 20)
            .unwrap_err();
        assert!(err.why().contains("more than"), "{}", err.why());
    }

    /// A record whose parent chain loops has to stop rather than spin.
    #[test]
    fn a_looping_parent_chain_does_not_hang() {
        let mut index = FileIndex::default();
        index.entries.insert(
            10,
            vec![IndexEntry {
                number: 10,
                parent: 11,
                name: "a".to_owned(),
                is_directory: true,
                size: 0,
                is_reparse_point: false,
                is_compressed: false,
                is_encrypted: false,
                is_sparse: false,
                is_hard_linked: false,
                streams: Vec::new(),
            }],
        );
        index.entries.insert(
            11,
            vec![IndexEntry {
                number: 11,
                parent: 10,
                name: "b".to_owned(),
                ..index.entries[&10][0].clone()
            }],
        );
        assert_eq!(index.path_of(10), None);
    }

    #[test]
    fn a_volume_that_is_not_ntfs_is_refused() {
        let mut disk = MemoryBlockDevice::from_vec("test", vec![0u8; 1 << 20], 512);
        assert!(Volume::open(&mut disk).is_err());
    }

    #[test]
    fn a_cancelled_scan_stops() {
        let (bytes, _) = sample_volume();
        let mut disk = MemoryBlockDevice::from_vec("test", bytes, 512);
        let mut volume = open(&mut disk);

        let cancel = CancelToken::new();
        cancel.cancel();
        let err = FileIndex::build(&mut volume, &cancel).unwrap_err();
        assert_eq!(err.exit(), ExitCode::Cancelled);
    }
}
