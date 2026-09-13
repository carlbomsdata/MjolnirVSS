//! Decoding NTFS data runs.
//!
//! A file that does not fit inside its own record has its contents described by
//! a run list: a compact sequence saying "this many clusters, starting there",
//! where "there" is relative to where the previous run started. It is the
//! densest structure in the filesystem and the easiest to decode slightly
//! wrongly, which is why it has its own module and its own tests.
//!
//! # The encoding
//!
//! Each run begins with one header byte. Its low nibble is how many bytes the
//! length takes, and its high nibble how many bytes the offset takes. Both
//! numbers are little endian; the length is unsigned and the offset is **signed
//! and relative to the previous run's start**, so a file whose parts go
//! backwards along the disk is normal rather than a sign of damage.
//!
//! A header byte of zero ends the list.
//!
//! An offset length of zero means the run has no offset at all. That is a
//! **sparse run**: a hole in the file, which occupies clusters in the file's
//! numbering but none on the disk, and reads as zeros.

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::math;

/// One run of a file's contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRun {
    /// First cluster of this run within the file.
    pub vcn: u64,
    /// How many clusters it covers.
    pub length: u64,
    /// Where it starts on the volume, or `None` when it is a hole.
    pub lcn: Option<u64>,
}

impl DataRun {
    /// Whether this run is a hole rather than data on the disk.
    pub fn is_sparse(&self) -> bool {
        self.lcn.is_none()
    }
}

/// A decoded run list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunList {
    runs: Vec<DataRun>,
}

impl RunList {
    /// The runs, in file order.
    pub fn runs(&self) -> &[DataRun] {
        &self.runs
    }

    /// Whether the list covers nothing.
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Total clusters the list describes, holes included.
    pub fn cluster_count(&self) -> u64 {
        self.runs.iter().map(|r| r.length).sum()
    }

    /// Clusters that actually occupy space on the volume.
    pub fn allocated_clusters(&self) -> u64 {
        self.runs
            .iter()
            .filter(|r| !r.is_sparse())
            .map(|r| r.length)
            .sum()
    }

    /// Finds the run holding a cluster of the file, and how far into it.
    pub fn locate(&self, vcn: u64) -> Option<(&DataRun, u64)> {
        self.runs
            .iter()
            .find(|r| vcn >= r.vcn && vcn < r.vcn + r.length)
            .map(|r| (r, vcn - r.vcn))
    }

    /// Decodes a run list.
    ///
    /// `starting_vcn` is where this attribute's part of the file begins, which
    /// is zero except for a file whose run list is too big for one record.
    pub fn parse(bytes: &[u8], starting_vcn: u64) -> Result<Self> {
        let mut runs = Vec::new();
        let mut at = 0usize;
        let mut vcn = starting_vcn;
        let mut previous_lcn: i64 = 0;

        loop {
            let Some(&header) = bytes.get(at) else {
                // Running off the end without a terminator is how a truncated
                // or misread record shows up, and it must not be mistaken for
                // a list that simply ended.
                return Err(malformed(format!(
                    "the run list ends after {at} bytes without a terminator"
                )));
            };
            at += 1;
            if header == 0 {
                break;
            }

            let length_bytes = (header & 0x0F) as usize;
            let offset_bytes = (header >> 4) as usize;

            if length_bytes == 0 || length_bytes > 8 || offset_bytes > 8 {
                return Err(malformed(format!(
                    "a run header of {header:#04x} describes a {length_bytes} byte length and a {offset_bytes} byte offset, which is not possible"
                )));
            }
            if at + length_bytes + offset_bytes > bytes.len() {
                return Err(malformed(
                    "a run claims more bytes than the attribute holds".to_owned(),
                ));
            }

            let length = read_unsigned(&bytes[at..at + length_bytes]);
            at += length_bytes;
            if length == 0 {
                return Err(malformed("a run covers no clusters".to_owned()));
            }

            let lcn = if offset_bytes == 0 {
                // A hole. The previous run's position is deliberately left
                // alone: the next real run is relative to the last real one.
                None
            } else {
                let delta = read_signed(&bytes[at..at + offset_bytes]);
                at += offset_bytes;
                previous_lcn = previous_lcn.checked_add(delta).ok_or_else(|| {
                    malformed("a run offset moves outside what a disk can address".to_owned())
                })?;
                if previous_lcn < 0 {
                    return Err(malformed(format!(
                        "a run starts at cluster {previous_lcn}, which is before the start of the volume"
                    )));
                }
                Some(previous_lcn as u64)
            };

            runs.push(DataRun { vcn, length, lcn });
            vcn = math::add_u64("run list cursor", vcn, length)
                .map_err(|e| malformed(e.to_string()))?;
        }

        Ok(Self { runs })
    }

    /// Joins another attribute's runs onto the end of this list.
    ///
    /// A file with very many fragments has its run list split across several
    /// attributes, each starting where the last ended. They have to line up.
    pub fn extend(&mut self, other: RunList) -> Result<()> {
        if let (Some(last), Some(first)) = (self.runs.last(), other.runs.first()) {
            let expected = last.vcn + last.length;
            if first.vcn != expected {
                return Err(malformed(format!(
                    "a continued run list starts at cluster {} where {} was expected, so part of the file is unaccounted for",
                    first.vcn, expected
                )));
            }
        }
        self.runs.extend(other.runs);
        Ok(())
    }
}

/// Reads a little endian unsigned number of up to eight bytes.
fn read_unsigned(bytes: &[u8]) -> u64 {
    let mut value = 0u64;
    for (i, b) in bytes.iter().enumerate() {
        value |= u64::from(*b) << (8 * i);
    }
    value
}

/// Reads a little endian signed number of up to eight bytes.
///
/// The top bit of the last byte is the sign, so a shorter number has to be
/// extended into the bits above it.
fn read_signed(bytes: &[u8]) -> i64 {
    if bytes.is_empty() {
        return 0;
    }
    let mut value = read_unsigned(bytes);
    let bits = bytes.len() * 8;
    if bits < 64 && bytes[bytes.len() - 1] & 0x80 != 0 {
        value |= u64::MAX << bits;
    }
    value as i64
}

fn malformed(detail: String) -> Error {
    Error::new(
        ExitCode::Unsupported,
        "a file's layout could not be read",
        detail,
        "this file cannot be extracted from the backup; the rest of the backup is unaffected",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a run list the way NTFS writes one.
    fn encode(runs: &[(u64, Option<i64>)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (length, delta) in runs {
            let length_bytes = minimal_unsigned(*length);
            let offset_bytes = match delta {
                Some(d) => minimal_signed(*d),
                None => Vec::new(),
            };
            out.push(((offset_bytes.len() as u8) << 4) | length_bytes.len() as u8);
            out.extend_from_slice(&length_bytes);
            out.extend_from_slice(&offset_bytes);
        }
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
            let redundant =
                (last == 0x00 && next & 0x80 == 0) || (last == 0xFF && next & 0x80 != 0);
            if !redundant {
                break;
            }
            bytes.pop();
        }
        bytes
    }

    #[test]
    fn a_single_run_decodes() {
        let list = RunList::parse(&encode(&[(8, Some(0x1234))]), 0).unwrap();
        assert_eq!(
            list.runs(),
            &[DataRun {
                vcn: 0,
                length: 8,
                lcn: Some(0x1234)
            }]
        );
        assert_eq!(list.cluster_count(), 8);
        assert_eq!(list.allocated_clusters(), 8);
    }

    /// Offsets are relative to the previous run, which is the part most easily
    /// got wrong and the part that decides whether a file reads as itself or as
    /// somebody else's data.
    #[test]
    fn offsets_accumulate() {
        let list =
            RunList::parse(&encode(&[(4, Some(100)), (4, Some(50)), (4, Some(25))]), 0).unwrap();
        let lcns: Vec<_> = list.runs().iter().map(|r| r.lcn).collect();
        assert_eq!(lcns, vec![Some(100), Some(150), Some(175)]);
    }

    /// A fragmented file can have a part that lives earlier on the disk than
    /// the part before it. That is ordinary, and the offset is negative.
    #[test]
    fn a_run_can_go_backwards() {
        let list = RunList::parse(&encode(&[(4, Some(1000)), (4, Some(-500))]), 0).unwrap();
        let lcns: Vec<_> = list.runs().iter().map(|r| r.lcn).collect();
        assert_eq!(lcns, vec![Some(1000), Some(500)]);
    }

    /// A hole occupies clusters in the file but none on the disk, and the run
    /// after it is still relative to the last run that had a position.
    #[test]
    fn a_sparse_run_is_a_hole_and_does_not_move_the_cursor() {
        let list = RunList::parse(&encode(&[(2, Some(100)), (10, None), (2, Some(8))]), 0).unwrap();

        assert_eq!(list.runs()[0].lcn, Some(100));
        assert!(list.runs()[1].is_sparse());
        assert_eq!(list.runs()[1].length, 10);
        // 100 + 8, not 110 + 8: the hole did not move the position.
        assert_eq!(list.runs()[2].lcn, Some(108));

        assert_eq!(list.cluster_count(), 14);
        assert_eq!(list.allocated_clusters(), 4);
    }

    #[test]
    fn cluster_numbers_within_the_file_run_on() {
        let list =
            RunList::parse(&encode(&[(3, Some(10)), (5, Some(10)), (7, Some(10))]), 0).unwrap();
        let vcns: Vec<_> = list.runs().iter().map(|r| r.vcn).collect();
        assert_eq!(vcns, vec![0, 3, 8]);
    }

    #[test]
    fn a_continued_list_starts_where_it_is_told() {
        let list = RunList::parse(&encode(&[(4, Some(10))]), 1000).unwrap();
        assert_eq!(list.runs()[0].vcn, 1000);
    }

    #[test]
    fn locating_a_cluster_finds_its_run_and_the_offset_into_it() {
        let list = RunList::parse(&encode(&[(4, Some(100)), (4, Some(100))]), 0).unwrap();

        let (run, into) = list.locate(0).unwrap();
        assert_eq!((run.lcn, into), (Some(100), 0));

        let (run, into) = list.locate(3).unwrap();
        assert_eq!((run.lcn, into), (Some(100), 3));

        let (run, into) = list.locate(5).unwrap();
        assert_eq!((run.lcn, into), (Some(200), 1));

        assert!(list.locate(8).is_none());
    }

    #[test]
    fn an_empty_list_is_just_a_terminator() {
        let list = RunList::parse(&[0], 0).unwrap();
        assert!(list.is_empty());
        assert_eq!(list.cluster_count(), 0);
    }

    // ---- what must be refused --------------------------------------------

    /// The dangerous one: a list that runs off the end of the attribute without
    /// saying it has finished. Treating that as the end would silently truncate
    /// a file.
    #[test]
    fn a_list_without_a_terminator_is_refused() {
        let mut bytes = encode(&[(4, Some(100))]);
        bytes.pop(); // remove the terminator
        let err = RunList::parse(&bytes, 0).unwrap_err();
        assert!(err.why().contains("without a terminator"), "{}", err.why());
    }

    #[test]
    fn a_run_claiming_more_bytes_than_it_has_is_refused() {
        // Header says four length bytes and four offset bytes, then stops.
        let err = RunList::parse(&[0x44, 0x01, 0x02], 0).unwrap_err();
        assert!(err.why().contains("more bytes than"), "{}", err.why());
    }

    #[test]
    fn an_impossible_header_is_refused() {
        // Zero length bytes cannot describe a run.
        assert!(RunList::parse(&[0x10, 0x01, 0x00], 0).is_err());
        // Nine length bytes do not fit a 64 bit number.
        assert!(RunList::parse(&[0x09, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0], 0).is_err());
    }

    #[test]
    fn a_run_of_no_clusters_is_refused() {
        let err = RunList::parse(&[0x11, 0x00, 0x05, 0x00], 0).unwrap_err();
        assert!(err.why().contains("no clusters"), "{}", err.why());
    }

    /// A file whose first run points before the start of the volume is damaged,
    /// and reading from there would read somebody else's data.
    #[test]
    fn a_run_before_the_start_of_the_volume_is_refused() {
        let err = RunList::parse(&encode(&[(4, Some(-10))]), 0).unwrap_err();
        assert!(err.why().contains("before the start"), "{}", err.why());
    }

    #[test]
    fn a_continued_list_that_does_not_line_up_is_refused() {
        let mut first = RunList::parse(&encode(&[(4, Some(10))]), 0).unwrap();
        let gap = RunList::parse(&encode(&[(4, Some(10))]), 100).unwrap();
        let err = first.extend(gap).unwrap_err();
        assert!(err.why().contains("unaccounted for"), "{}", err.why());
    }

    #[test]
    fn a_continued_list_that_lines_up_is_joined() {
        let mut first = RunList::parse(&encode(&[(4, Some(10))]), 0).unwrap();
        let next = RunList::parse(&encode(&[(6, Some(10))]), 4).unwrap();
        first.extend(next).unwrap();
        assert_eq!(first.cluster_count(), 10);
        assert_eq!(first.runs().len(), 2);
    }

    // ---- the number readers ----------------------------------------------

    #[test]
    fn signed_values_are_extended_from_their_own_width() {
        assert_eq!(read_signed(&[0xFF]), -1);
        assert_eq!(read_signed(&[0x80]), -128);
        assert_eq!(read_signed(&[0x7F]), 127);
        assert_eq!(read_signed(&[0x00, 0x80]), -32768);
        assert_eq!(read_signed(&[0xFF, 0xFF, 0xFF]), -1);
        assert_eq!(read_signed(&[]), 0);
    }

    #[test]
    fn unsigned_values_read_little_endian() {
        assert_eq!(read_unsigned(&[0x34, 0x12]), 0x1234);
        assert_eq!(read_unsigned(&[0xFF]), 255);
        assert_eq!(read_unsigned(&[]), 0);
    }

    /// Every run list this module can encode, it can decode back.
    #[test]
    fn encoding_and_decoding_agree() {
        let cases: Vec<Vec<(u64, Option<i64>)>> = vec![
            vec![(1, Some(1))],
            vec![(0xFFFF, Some(0x7FFF))],
            // Each run a little earlier than the last, which is legal.
            vec![(1, Some(10)), (1, Some(-1)), (1, Some(-1))],
            vec![(100, Some(1_000_000)), (200, None), (300, Some(-999_999))],
            vec![(1 << 20, Some(1 << 30))],
        ];
        for case in cases {
            let list = RunList::parse(&encode(&case), 0).unwrap();
            assert_eq!(list.runs().len(), case.len(), "{case:?}");
            let total: u64 = case.iter().map(|(l, _)| *l).sum();
            assert_eq!(list.cluster_count(), total, "{case:?}");
        }
    }
}
