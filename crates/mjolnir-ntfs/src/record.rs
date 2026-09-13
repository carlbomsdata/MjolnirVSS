//! Reading NTFS file records.
//!
//! Everything on an NTFS volume is a file, including the filesystem's own
//! structures, and every file is described by a record in the master file
//! table. A record is a header followed by a list of attributes: its name, its
//! timestamps, its contents, and for a directory its index.
//!
//! # Fixups
//!
//! NTFS protects multi sector structures with an update sequence. The last two
//! bytes of every sector in a record are replaced by a sequence number, and the
//! bytes they displaced are kept in an array in the header. A reader has to put
//! them back, and a reader that forgets produces data that is right except for
//! two bytes every sector, which is the worst kind of wrong.
//!
//! [`FileRecord::parse`] does it, and refuses a record whose sequence numbers
//! do not match, because that means the record was written while being read or
//! has been damaged.
//!
//! # What is not here
//!
//! Nothing writes. This module and everything above it opens a backup read
//! only, and there is no code path from here to a disk.

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;

use crate::runs::RunList;

/// Signature at the start of a file record.
pub const RECORD_SIGNATURE: &[u8; 4] = b"FILE";

/// Attribute type codes, from the ones this crate reads.
pub mod attribute {
    /// Timestamps and DOS attributes.
    pub const STANDARD_INFORMATION: u32 = 0x10;
    /// A list of the other records holding this file's attributes.
    pub const ATTRIBUTE_LIST: u32 = 0x20;
    /// A name, and the directory it is in.
    pub const FILE_NAME: u32 = 0x30;
    /// The contents.
    pub const DATA: u32 = 0x80;
    /// A directory's index, when it fits in the record.
    pub const INDEX_ROOT: u32 = 0x90;
    /// A directory's index, when it does not.
    pub const INDEX_ALLOCATION: u32 = 0xA0;
    /// A junction, a symbolic link, or something a filter driver owns.
    pub const REPARSE_POINT: u32 = 0xC0;
    /// The marker that ends the attribute list.
    pub const END: u32 = 0xFFFF_FFFF;
}

/// Attribute flags that change how the contents are stored.
pub mod attribute_flags {
    /// The contents are compressed.
    pub const COMPRESSED: u16 = 0x0001;
    /// The contents are encrypted by the Encrypting File System.
    pub const ENCRYPTED: u16 = 0x4000;
    /// The contents have holes.
    pub const SPARSE: u16 = 0x8000;
}

/// Which naming rules a name follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameSpace {
    /// Case sensitive, almost anything allowed.
    Posix,
    /// The long name.
    Win32,
    /// The short `EXAMPL~1.TXT` name.
    Dos,
    /// A name short enough to be both.
    Win32AndDos,
    /// Something this version does not recognise.
    Unknown(u8),
}

impl NameSpace {
    fn from_byte(value: u8) -> Self {
        match value {
            0 => NameSpace::Posix,
            1 => NameSpace::Win32,
            2 => NameSpace::Dos,
            3 => NameSpace::Win32AndDos,
            other => NameSpace::Unknown(other),
        }
    }

    /// Whether this is a name a person would recognise.
    ///
    /// The short name is a second name for the same file, and listing both
    /// would show every file twice.
    pub fn is_long_name(self) -> bool {
        matches!(
            self,
            NameSpace::Posix | NameSpace::Win32 | NameSpace::Win32AndDos
        )
    }
}

/// A reference to a record in the master file table.
///
/// The low 48 bits are the record number and the top 16 are a sequence number
/// that changes every time the record is reused. Checking the sequence is what
/// stops a stale reference pointing at whatever occupies that record now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MftReference {
    /// The record number.
    pub number: u64,
    /// The sequence number the reference expects.
    pub sequence: u16,
}

impl MftReference {
    /// Splits a packed reference.
    pub fn from_raw(raw: u64) -> Self {
        Self {
            number: raw & 0x0000_FFFF_FFFF_FFFF,
            sequence: (raw >> 48) as u16,
        }
    }

    /// The record number of the volume's root directory.
    pub const ROOT: u64 = 5;
}

/// One name a file has, and the directory holding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileName {
    /// The directory this name is in.
    pub parent: MftReference,
    /// The name itself.
    pub name: String,
    /// Which naming rules it follows.
    pub namespace: NameSpace,
    /// Size the file occupies, from the directory entry. Not authoritative:
    /// the `$DATA` attribute is.
    pub allocated_size: u64,
    /// Size of the contents, from the directory entry.
    pub real_size: u64,
    /// DOS style attribute flags.
    pub flags: u32,
}

/// One attribute of a file record.
#[derive(Debug, Clone)]
pub struct Attribute {
    /// What kind it is.
    pub type_code: u32,
    /// Its name, for a named stream. Empty for the unnamed one.
    pub name: String,
    /// Flags describing how the contents are stored.
    pub flags: u16,
    /// Contents, when they fit inside the record.
    pub resident_value: Option<Vec<u8>>,
    /// Where the contents live, when they do not.
    pub runs: Option<RunList>,
    /// Logical size of the contents.
    pub data_size: u64,
    /// How much of the contents has ever been written. Bytes past it read as
    /// zeros even where clusters are allocated.
    pub initialized_size: u64,
    /// First cluster of the file this attribute describes.
    pub starting_vcn: u64,
}

impl Attribute {
    /// Whether the contents are stored inside the record.
    pub fn is_resident(&self) -> bool {
        self.resident_value.is_some()
    }

    /// Whether the contents are compressed.
    pub fn is_compressed(&self) -> bool {
        self.flags & attribute_flags::COMPRESSED != 0
    }

    /// Whether the contents are encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.flags & attribute_flags::ENCRYPTED != 0
    }

    /// Whether the contents have holes.
    pub fn is_sparse(&self) -> bool {
        self.flags & attribute_flags::SPARSE != 0
    }

    /// Whether this is the unnamed `$DATA` attribute: a file's actual contents.
    pub fn is_main_data(&self) -> bool {
        self.type_code == attribute::DATA && self.name.is_empty()
    }

    /// Whether this is an alternate data stream.
    pub fn is_alternate_stream(&self) -> bool {
        self.type_code == attribute::DATA && !self.name.is_empty()
    }
}

/// A parsed file record.
#[derive(Debug, Clone)]
pub struct FileRecord {
    /// Which record this is, as the record itself says.
    pub record_number: u64,
    /// The sequence number, which a reference has to match.
    pub sequence: u16,
    /// Whether the record describes a file that exists.
    pub in_use: bool,
    /// Whether it describes a directory.
    pub is_directory: bool,
    /// How many names the file has across all directories.
    pub hard_link_count: u16,
    /// When this record continues another, the one it continues.
    pub base_record: Option<MftReference>,
    /// Every attribute, in the order they appear.
    pub attributes: Vec<Attribute>,
}

impl FileRecord {
    /// The names this file is known by.
    pub fn names(&self) -> Vec<FileName> {
        self.attributes
            .iter()
            .filter(|a| a.type_code == attribute::FILE_NAME)
            .filter_map(|a| a.resident_value.as_deref())
            .filter_map(|bytes| parse_file_name(bytes).ok())
            .collect()
    }

    /// The name a person would see, preferring a long one.
    pub fn best_name(&self) -> Option<FileName> {
        let names = self.names();
        names
            .iter()
            .find(|n| n.namespace.is_long_name())
            .or_else(|| names.first())
            .cloned()
    }

    /// The attribute holding the file's contents.
    pub fn data(&self) -> Option<&Attribute> {
        self.attributes.iter().find(|a| a.is_main_data())
    }

    /// The named streams, which are extra contents hanging off the same file.
    pub fn alternate_streams(&self) -> Vec<&Attribute> {
        self.attributes
            .iter()
            .filter(|a| a.is_alternate_stream())
            .collect()
    }

    /// Whether the file is a junction, a symbolic link or similar.
    pub fn is_reparse_point(&self) -> bool {
        self.attributes
            .iter()
            .any(|a| a.type_code == attribute::REPARSE_POINT)
    }

    /// Whether this record's attributes are listed in other records.
    pub fn has_attribute_list(&self) -> bool {
        self.attributes
            .iter()
            .any(|a| a.type_code == attribute::ATTRIBUTE_LIST)
    }

    /// Parses a record, applying the update sequence.
    ///
    /// `bytes` is one record, `sector_size` the volume's sector size. The
    /// buffer is copied rather than modified, because a caller reading a whole
    /// page of records must not have its buffer changed underneath it.
    pub fn parse(bytes: &[u8], sector_size: u32) -> Result<Self> {
        if bytes.len() < 48 {
            return Err(malformed(format!(
                "a file record of {} bytes is too small to hold its own header",
                bytes.len()
            )));
        }
        if &bytes[0..4] != RECORD_SIGNATURE {
            return Err(malformed(format!(
                "a file record begins with {:02x?} instead of FILE",
                &bytes[0..4]
            )));
        }

        let usa_offset = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
        let usa_count = u16::from_le_bytes([bytes[6], bytes[7]]) as usize;
        let attrs_offset = u16::from_le_bytes([bytes[20], bytes[21]]) as usize;
        let flags = u16::from_le_bytes([bytes[22], bytes[23]]);
        let bytes_in_use =
            u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]) as usize;

        let mut fixed = bytes.to_vec();
        apply_fixups(&mut fixed, usa_offset, usa_count, sector_size)?;

        if attrs_offset < 42 || attrs_offset >= fixed.len() {
            return Err(malformed(format!(
                "a file record says its attributes begin at {attrs_offset}, which is outside it"
            )));
        }
        // A record whose "bytes in use" is wrong is damaged; falling back to
        // the whole buffer would read attributes out of whatever follows.
        let limit = if bytes_in_use >= attrs_offset && bytes_in_use <= fixed.len() {
            bytes_in_use
        } else {
            fixed.len()
        };

        let sequence = u16::from_le_bytes([fixed[16], fixed[17]]);
        let hard_link_count = u16::from_le_bytes([fixed[18], fixed[19]]);
        let base_raw = u64::from_le_bytes(fixed[32..40].try_into().expect("8 bytes"));
        let record_number = if fixed.len() >= 48 {
            u32::from_le_bytes(fixed[44..48].try_into().expect("4 bytes")) as u64
        } else {
            0
        };

        let attributes = parse_attributes(&fixed[..limit], attrs_offset)?;

        Ok(Self {
            record_number,
            sequence,
            in_use: flags & 0x0001 != 0,
            is_directory: flags & 0x0002 != 0,
            hard_link_count,
            base_record: (base_raw != 0).then(|| MftReference::from_raw(base_raw)),
            attributes,
        })
    }
}

/// Puts back the bytes the update sequence displaced.
fn apply_fixups(
    bytes: &mut [u8],
    usa_offset: usize,
    usa_count: usize,
    sector_size: u32,
) -> Result<()> {
    if usa_count == 0 {
        return Err(malformed("a file record has no update sequence".to_owned()));
    }
    // The array is the sequence number followed by one entry per sector.
    let sectors = usa_count - 1;
    let sector_size = sector_size.max(512) as usize;

    if usa_offset + usa_count * 2 > bytes.len() {
        return Err(malformed(
            "a file record's update sequence is outside the record".to_owned(),
        ));
    }
    if sectors * sector_size > bytes.len() {
        return Err(malformed(format!(
            "a file record claims {sectors} sectors but is only {} bytes",
            bytes.len()
        )));
    }

    let sequence = [bytes[usa_offset], bytes[usa_offset + 1]];

    for sector in 0..sectors {
        let tail = (sector + 1) * sector_size - 2;
        let entry = usa_offset + 2 + sector * 2;
        let replacement = [bytes[entry], bytes[entry + 1]];

        if bytes[tail] != sequence[0] || bytes[tail + 1] != sequence[1] {
            return Err(malformed(format!(
                "the update sequence does not match at sector {sector}, so the record was damaged or is being written to"
            )));
        }
        bytes[tail] = replacement[0];
        bytes[tail + 1] = replacement[1];
    }
    Ok(())
}

/// Walks the attribute list in a record.
fn parse_attributes(bytes: &[u8], mut at: usize) -> Result<Vec<Attribute>> {
    let mut out = Vec::new();

    while at + 4 <= bytes.len() {
        let type_code = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
        if type_code == attribute::END {
            break;
        }
        if at + 16 > bytes.len() {
            return Err(malformed(
                "an attribute header runs past the end of its record".to_owned(),
            ));
        }

        let length =
            u32::from_le_bytes(bytes[at + 4..at + 8].try_into().expect("4 bytes")) as usize;
        if length < 16 || at + length > bytes.len() {
            return Err(malformed(format!(
                "an attribute says it is {length} bytes, which does not fit in its record"
            )));
        }
        let attribute = &bytes[at..at + length];

        let non_resident = attribute[8] != 0;
        let name_length = attribute[9] as usize;
        let name_offset = u16::from_le_bytes([attribute[10], attribute[11]]) as usize;
        let flags = u16::from_le_bytes([attribute[12], attribute[13]]);

        let name = if name_length == 0 {
            String::new()
        } else {
            read_utf16(attribute, name_offset, name_length)?
        };

        let parsed = if non_resident {
            if attribute.len() < 64 {
                return Err(malformed(
                    "a non resident attribute is too short to describe where its contents are"
                        .to_owned(),
                ));
            }
            let starting_vcn = u64::from_le_bytes(attribute[16..24].try_into().expect("8 bytes"));
            let runs_offset = u16::from_le_bytes([attribute[32], attribute[33]]) as usize;
            let data_size = u64::from_le_bytes(attribute[48..56].try_into().expect("8 bytes"));
            let initialized_size =
                u64::from_le_bytes(attribute[56..64].try_into().expect("8 bytes"));

            if runs_offset >= attribute.len() {
                return Err(malformed(
                    "a non resident attribute's run list begins outside it".to_owned(),
                ));
            }
            let runs = RunList::parse(&attribute[runs_offset..], starting_vcn)?;

            Attribute {
                type_code,
                name,
                flags,
                resident_value: None,
                runs: Some(runs),
                data_size,
                initialized_size,
                starting_vcn,
            }
        } else {
            let value_length =
                u32::from_le_bytes(attribute[16..20].try_into().expect("4 bytes")) as usize;
            let value_offset = u16::from_le_bytes([attribute[20], attribute[21]]) as usize;

            if value_offset + value_length > attribute.len() {
                return Err(malformed(format!(
                    "a resident attribute's {value_length} bytes of contents do not fit in its {} byte attribute",
                    attribute.len()
                )));
            }
            let value = attribute[value_offset..value_offset + value_length].to_vec();

            Attribute {
                type_code,
                name,
                flags,
                data_size: value.len() as u64,
                initialized_size: value.len() as u64,
                resident_value: Some(value),
                runs: None,
                starting_vcn: 0,
            }
        };

        out.push(parsed);
        at += length;
    }

    Ok(out)
}

/// Reads a `$FILE_NAME` attribute's contents.
pub fn parse_file_name(bytes: &[u8]) -> Result<FileName> {
    if bytes.len() < 66 {
        return Err(malformed(format!(
            "a file name attribute of {} bytes is too small",
            bytes.len()
        )));
    }
    let parent =
        MftReference::from_raw(u64::from_le_bytes(bytes[0..8].try_into().expect("8 bytes")));
    let allocated_size = u64::from_le_bytes(bytes[40..48].try_into().expect("8 bytes"));
    let real_size = u64::from_le_bytes(bytes[48..56].try_into().expect("8 bytes"));
    let flags = u32::from_le_bytes(bytes[56..60].try_into().expect("4 bytes"));
    let name_length = bytes[64] as usize;
    let namespace = NameSpace::from_byte(bytes[65]);

    let name = read_utf16(bytes, 66, name_length)?;

    Ok(FileName {
        parent,
        name,
        namespace,
        allocated_size,
        real_size,
        flags,
    })
}

/// Reads `characters` UTF-16 code units starting at `offset`.
///
/// Unpaired surrogates are replaced rather than refused: a name NTFS allows but
/// Unicode does not still has to be listed, and refusing would hide the file.
fn read_utf16(bytes: &[u8], offset: usize, characters: usize) -> Result<String> {
    let end = offset
        .checked_add(characters * 2)
        .ok_or_else(|| malformed("a name's length overflows".to_owned()))?;
    if end > bytes.len() {
        return Err(malformed(format!(
            "a name of {characters} characters at offset {offset} runs past the end of its attribute"
        )));
    }
    let units: Vec<u16> = bytes[offset..end]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Ok(String::from_utf16_lossy(&units))
}

fn malformed(detail: String) -> Error {
    Error::new(
        ExitCode::CorruptBackup,
        "a file record could not be read",
        detail,
        "this file cannot be listed or extracted; the rest of the backup is unaffected",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECTOR: u32 = 512;

    /// Builds a file record the way NTFS writes one, update sequence included.
    struct RecordBuilder {
        size: usize,
        flags: u16,
        sequence: u16,
        record_number: u32,
        hard_links: u16,
        base: u64,
        attributes: Vec<Vec<u8>>,
    }

    impl RecordBuilder {
        fn new() -> Self {
            Self {
                size: 1024,
                flags: 0x0001,
                sequence: 7,
                record_number: 42,
                hard_links: 1,
                base: 0,
                attributes: Vec::new(),
            }
        }

        fn directory(mut self) -> Self {
            self.flags |= 0x0002;
            self
        }

        fn deleted(mut self) -> Self {
            self.flags &= !0x0001;
            self
        }

        fn attribute(mut self, bytes: Vec<u8>) -> Self {
            self.attributes.push(bytes);
            self
        }

        fn build(self) -> Vec<u8> {
            let mut r = vec![0u8; self.size];
            let sectors = self.size / SECTOR as usize;
            let usa_offset = 48usize;
            let usa_count = sectors + 1;

            r[0..4].copy_from_slice(RECORD_SIGNATURE);
            r[4..6].copy_from_slice(&(usa_offset as u16).to_le_bytes());
            r[6..8].copy_from_slice(&(usa_count as u16).to_le_bytes());
            r[16..18].copy_from_slice(&self.sequence.to_le_bytes());
            r[18..20].copy_from_slice(&self.hard_links.to_le_bytes());
            r[22..24].copy_from_slice(&self.flags.to_le_bytes());
            r[32..40].copy_from_slice(&self.base.to_le_bytes());
            r[44..48].copy_from_slice(&self.record_number.to_le_bytes());

            let attrs_offset = usa_offset + usa_count * 2;
            // Round up to eight, which is what NTFS does.
            let attrs_offset = attrs_offset.div_ceil(8) * 8;
            r[20..22].copy_from_slice(&(attrs_offset as u16).to_le_bytes());

            let mut at = attrs_offset;
            for attribute in &self.attributes {
                r[at..at + attribute.len()].copy_from_slice(attribute);
                at += attribute.len();
            }
            r[at..at + 4].copy_from_slice(&attribute::END.to_le_bytes());
            at += 4;
            r[24..28].copy_from_slice(&(at as u32).to_le_bytes());

            // The update sequence: a number, then the bytes it displaces.
            let usn: u16 = 0xABCD;
            r[usa_offset..usa_offset + 2].copy_from_slice(&usn.to_le_bytes());
            for sector in 0..sectors {
                let tail = (sector + 1) * SECTOR as usize - 2;
                let entry = usa_offset + 2 + sector * 2;
                // The real bytes go into the array, the sequence into the tail.
                r[entry] = r[tail];
                r[entry + 1] = r[tail + 1];
                r[tail..tail + 2].copy_from_slice(&usn.to_le_bytes());
            }
            r
        }
    }

    fn resident_attribute(type_code: u32, name: &str, value: &[u8]) -> Vec<u8> {
        let name_units: Vec<u16> = name.encode_utf16().collect();
        let name_bytes = name_units.len() * 2;
        let name_offset = 24usize;
        let value_offset = (name_offset + name_bytes).div_ceil(8) * 8;
        let length = (value_offset + value.len()).div_ceil(8) * 8;

        let mut a = vec![0u8; length];
        a[0..4].copy_from_slice(&type_code.to_le_bytes());
        a[4..8].copy_from_slice(&(length as u32).to_le_bytes());
        a[8] = 0; // resident
        a[9] = name_units.len() as u8;
        a[10..12].copy_from_slice(&(name_offset as u16).to_le_bytes());
        a[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[20..22].copy_from_slice(&(value_offset as u16).to_le_bytes());
        for (i, unit) in name_units.iter().enumerate() {
            a[name_offset + i * 2..name_offset + i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        a[value_offset..value_offset + value.len()].copy_from_slice(value);
        a
    }

    fn non_resident_attribute(
        type_code: u32,
        name: &str,
        runs: &[u8],
        data_size: u64,
        flags: u16,
    ) -> Vec<u8> {
        let name_units: Vec<u16> = name.encode_utf16().collect();
        let name_offset = 64usize;
        let runs_offset = (name_offset + name_units.len() * 2).div_ceil(8) * 8;
        let length = (runs_offset + runs.len()).div_ceil(8) * 8;

        let mut a = vec![0u8; length];
        a[0..4].copy_from_slice(&type_code.to_le_bytes());
        a[4..8].copy_from_slice(&(length as u32).to_le_bytes());
        a[8] = 1; // non resident
        a[9] = name_units.len() as u8;
        a[10..12].copy_from_slice(&(name_offset as u16).to_le_bytes());
        a[12..14].copy_from_slice(&flags.to_le_bytes());
        a[16..24].copy_from_slice(&0u64.to_le_bytes()); // starting vcn
        a[32..34].copy_from_slice(&(runs_offset as u16).to_le_bytes());
        a[48..56].copy_from_slice(&data_size.to_le_bytes());
        a[56..64].copy_from_slice(&data_size.to_le_bytes());
        for (i, unit) in name_units.iter().enumerate() {
            a[name_offset + i * 2..name_offset + i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        a[runs_offset..runs_offset + runs.len()].copy_from_slice(runs);
        a
    }

    fn file_name_attribute(parent: u64, name: &str, namespace: u8, size: u64) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 66 + units.len() * 2];
        v[0..8].copy_from_slice(&(parent | (1u64 << 48)).to_le_bytes());
        v[40..48].copy_from_slice(&size.to_le_bytes());
        v[48..56].copy_from_slice(&size.to_le_bytes());
        v[64] = units.len() as u8;
        v[65] = namespace;
        for (i, unit) in units.iter().enumerate() {
            v[66 + i * 2..66 + i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        v
    }

    #[test]
    fn a_plain_file_record_parses() {
        let bytes = RecordBuilder::new()
            .attribute(resident_attribute(
                attribute::FILE_NAME,
                "",
                &file_name_attribute(MftReference::ROOT, "readme.txt", 1, 1234),
            ))
            .attribute(resident_attribute(attribute::DATA, "", b"hello"))
            .build();

        let record = FileRecord::parse(&bytes, SECTOR).unwrap();
        assert!(record.in_use);
        assert!(!record.is_directory);
        assert_eq!(record.record_number, 42);
        assert_eq!(record.sequence, 7);

        let name = record.best_name().unwrap();
        assert_eq!(name.name, "readme.txt");
        assert_eq!(name.parent.number, MftReference::ROOT);
        assert_eq!(name.real_size, 1234);

        let data = record.data().unwrap();
        assert!(data.is_resident());
        assert_eq!(data.resident_value.as_deref(), Some(&b"hello"[..]));
        assert_eq!(data.data_size, 5);
    }

    /// The fixups are the part a reader most easily gets wrong, and getting
    /// them wrong corrupts exactly two bytes of every sector, silently.
    ///
    /// This checks the two bytes by position rather than by looking for them
    /// anywhere, because "somewhere in the value" would pass even if they were
    /// put back in the wrong place.
    #[test]
    fn the_update_sequence_is_put_back_in_the_right_place() {
        let mut builder = RecordBuilder::new();
        builder
            .attributes
            .push(resident_attribute(attribute::DATA, "", &vec![0xAA; 600]));
        let mut bytes = builder.build();

        // The tail of sector zero holds the sequence number, not data.
        assert_eq!(&bytes[510..512], &0xABCDu16.to_le_bytes());

        // Put known bytes in the array. These are what belong at 510 and 511.
        let usa_offset = 48;
        bytes[usa_offset + 2] = 0x11;
        bytes[usa_offset + 3] = 0x22;

        let record = FileRecord::parse(&bytes, SECTOR).unwrap();
        let value = record.data().unwrap().resident_value.clone().unwrap();

        // Where the value starts inside the record: the attributes begin after
        // the update sequence array, rounded up to eight, and an unnamed
        // resident attribute puts its value 24 bytes into itself.
        let attrs_offset = u16::from_le_bytes([bytes[20], bytes[21]]) as usize;
        let value_start = attrs_offset + 24;
        let at = 510 - value_start;

        assert_eq!(value[at], 0x11, "the first displaced byte was not put back");
        assert_eq!(
            value[at + 1],
            0x22,
            "the second displaced byte was not put back"
        );
        // Everything else is untouched.
        assert_eq!(value[at - 1], 0xAA);
        assert_eq!(value[at + 2], 0xAA);
    }

    #[test]
    fn a_record_whose_sequence_does_not_match_is_refused() {
        let mut bytes = RecordBuilder::new()
            .attribute(resident_attribute(attribute::DATA, "", b"hi"))
            .build();
        // Damage the tail of the first sector.
        bytes[510] = 0x00;
        bytes[511] = 0x00;

        let err = FileRecord::parse(&bytes, SECTOR).unwrap_err();
        assert!(err.why().contains("does not match"), "{}", err.why());
    }

    #[test]
    fn something_that_is_not_a_record_is_refused() {
        assert!(FileRecord::parse(&[0u8; 1024], SECTOR).is_err());
        assert!(FileRecord::parse(b"FILE", SECTOR).is_err());
        assert!(FileRecord::parse(&[], SECTOR).is_err());
    }

    #[test]
    fn a_directory_is_recognised() {
        let bytes = RecordBuilder::new()
            .directory()
            .attribute(resident_attribute(
                attribute::FILE_NAME,
                "",
                &file_name_attribute(MftReference::ROOT, "Documents", 1, 0),
            ))
            .attribute(resident_attribute(
                attribute::INDEX_ROOT,
                "$I30",
                &[0u8; 32],
            ))
            .build();

        let record = FileRecord::parse(&bytes, SECTOR).unwrap();
        assert!(record.is_directory);
        assert!(record.data().is_none());
    }

    #[test]
    fn a_deleted_record_says_so() {
        let bytes = RecordBuilder::new().deleted().build();
        let record = FileRecord::parse(&bytes, SECTOR).unwrap();
        assert!(!record.in_use);
    }

    /// A file keeps both a long name and a short one. Listing both would show
    /// every file twice, so the long one wins.
    #[test]
    fn the_long_name_is_preferred_over_the_short_one() {
        let bytes = RecordBuilder::new()
            .attribute(resident_attribute(
                attribute::FILE_NAME,
                "",
                &file_name_attribute(MftReference::ROOT, "LONGNA~1.TXT", 2, 10),
            ))
            .attribute(resident_attribute(
                attribute::FILE_NAME,
                "",
                &file_name_attribute(MftReference::ROOT, "long name with spaces.txt", 1, 10),
            ))
            .build();

        let record = FileRecord::parse(&bytes, SECTOR).unwrap();
        assert_eq!(record.names().len(), 2);
        assert_eq!(
            record.best_name().unwrap().name,
            "long name with spaces.txt"
        );
    }

    #[test]
    fn a_unicode_name_survives() {
        for name in [
            "\u{00e4}\u{00f6}\u{00fc}.txt",
            "\u{6587}\u{4ef6}.bin",
            "\u{03a9}mega",
            "emoji \u{1f600} here",
        ] {
            let bytes = RecordBuilder::new()
                .attribute(resident_attribute(
                    attribute::FILE_NAME,
                    "",
                    &file_name_attribute(MftReference::ROOT, name, 1, 0),
                ))
                .build();
            let record = FileRecord::parse(&bytes, SECTOR).unwrap();
            assert_eq!(record.best_name().unwrap().name, name);
        }
    }

    #[test]
    fn an_alternate_data_stream_is_found_and_kept_apart_from_the_contents() {
        let bytes = RecordBuilder::new()
            .attribute(resident_attribute(attribute::DATA, "", b"the visible part"))
            .attribute(resident_attribute(
                attribute::DATA,
                "hidden",
                b"the other part",
            ))
            .build();

        let record = FileRecord::parse(&bytes, SECTOR).unwrap();
        assert_eq!(
            record.data().unwrap().resident_value.as_deref(),
            Some(&b"the visible part"[..])
        );
        let streams = record.alternate_streams();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].name, "hidden");
        assert_eq!(
            streams[0].resident_value.as_deref(),
            Some(&b"the other part"[..])
        );
    }

    #[test]
    fn a_non_resident_attribute_carries_its_runs() {
        // One run: eight clusters at cluster 100.
        let runs = [0x11u8, 0x08, 0x64, 0x00];
        let bytes = RecordBuilder::new()
            .attribute(non_resident_attribute(
                attribute::DATA,
                "",
                &runs,
                8 * 4096,
                0,
            ))
            .build();

        let record = FileRecord::parse(&bytes, SECTOR).unwrap();
        let data = record.data().unwrap();
        assert!(!data.is_resident());
        assert_eq!(data.data_size, 8 * 4096);
        let list = data.runs.as_ref().unwrap();
        assert_eq!(list.runs().len(), 1);
        assert_eq!(list.runs()[0].lcn, Some(100));
    }

    #[test]
    fn the_storage_flags_are_read() {
        for (flag, check) in [
            (attribute_flags::COMPRESSED, "compressed"),
            (attribute_flags::ENCRYPTED, "encrypted"),
            (attribute_flags::SPARSE, "sparse"),
        ] {
            let bytes = RecordBuilder::new()
                .attribute(non_resident_attribute(
                    attribute::DATA,
                    "",
                    &[0x11, 0x01, 0x05, 0x00],
                    4096,
                    flag,
                ))
                .build();
            let record = FileRecord::parse(&bytes, SECTOR).unwrap();
            let data = record.data().unwrap();
            match check {
                "compressed" => assert!(data.is_compressed()),
                "encrypted" => assert!(data.is_encrypted()),
                _ => assert!(data.is_sparse()),
            }
        }
    }

    #[test]
    fn a_reparse_point_is_recognised() {
        let bytes = RecordBuilder::new()
            .attribute(resident_attribute(attribute::REPARSE_POINT, "", &[0u8; 24]))
            .build();
        assert!(FileRecord::parse(&bytes, SECTOR)
            .unwrap()
            .is_reparse_point());
    }

    #[test]
    fn hard_links_are_counted() {
        let mut builder = RecordBuilder::new();
        builder.hard_links = 3;
        let record = FileRecord::parse(&builder.build(), SECTOR).unwrap();
        assert_eq!(record.hard_link_count, 3);
    }

    #[test]
    fn a_continuation_record_names_the_one_it_continues() {
        let mut builder = RecordBuilder::new();
        builder.base = 5 | (2u64 << 48);
        let record = FileRecord::parse(&builder.build(), SECTOR).unwrap();
        let base = record.base_record.unwrap();
        assert_eq!(base.number, 5);
        assert_eq!(base.sequence, 2);
    }

    #[test]
    fn a_reference_splits_into_a_number_and_a_sequence() {
        let reference = MftReference::from_raw(0x0003_0000_0000_002A);
        assert_eq!(reference.number, 42);
        assert_eq!(reference.sequence, 3);
    }

    /// An attribute claiming to be longer than the record is how a damaged or
    /// deliberately malformed record would try to make a reader read past it.
    #[test]
    fn an_attribute_longer_than_its_record_is_refused() {
        let mut bytes = RecordBuilder::new()
            .attribute(resident_attribute(attribute::DATA, "", b"hi"))
            .build();

        let attrs_offset = u16::from_le_bytes([bytes[20], bytes[21]]) as usize;
        bytes[attrs_offset + 4..attrs_offset + 8].copy_from_slice(&999_999u32.to_le_bytes());
        // Fix the sequence back up so the fixup check is not what fails.
        let usa_offset = 48;
        let usn = u16::from_le_bytes([bytes[usa_offset], bytes[usa_offset + 1]]);
        for sector in 0..2 {
            let tail = (sector + 1) * 512 - 2;
            bytes[tail..tail + 2].copy_from_slice(&usn.to_le_bytes());
        }

        let err = FileRecord::parse(&bytes, SECTOR).unwrap_err();
        assert!(err.why().contains("does not fit"), "{}", err.why());
    }

    #[test]
    fn a_resident_value_reaching_past_its_attribute_is_refused() {
        let mut attribute = resident_attribute(attribute::DATA, "", b"hi");
        attribute[16..20].copy_from_slice(&9999u32.to_le_bytes());
        let mut builder = RecordBuilder::new();
        builder.attributes.push(attribute);
        let bytes = builder.build();

        let err = FileRecord::parse(&bytes, SECTOR).unwrap_err();
        assert!(err.why().contains("do not fit"), "{}", err.why());
    }

    #[test]
    fn a_name_reaching_past_its_attribute_is_refused() {
        let mut value = file_name_attribute(MftReference::ROOT, "short", 1, 0);
        value[64] = 200; // claim a 200 character name
        assert!(parse_file_name(&value).is_err());
    }

    #[test]
    fn namespaces_are_classified() {
        assert!(NameSpace::from_byte(0).is_long_name());
        assert!(NameSpace::from_byte(1).is_long_name());
        assert!(!NameSpace::from_byte(2).is_long_name());
        assert!(NameSpace::from_byte(3).is_long_name());
        assert_eq!(NameSpace::from_byte(9), NameSpace::Unknown(9));
        assert!(!NameSpace::from_byte(9).is_long_name());
    }
}
