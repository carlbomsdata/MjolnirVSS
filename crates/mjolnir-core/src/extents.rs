//! Sorted, merged, non overlapping byte ranges.
//!
//! An NTFS allocation bitmap turns into tens of thousands of small ranges. They
//! arrive unsorted and adjacent, they have to be aligned outward to a copy
//! granularity, and after that they must still be provably non overlapping,
//! because every one of them becomes a write against a disk during recovery.
//!
//! [`ExtentList`] is the only way this crate produces such a set, and its
//! invariant is checked on construction rather than assumed.

use crate::math::{add_u64, range_end, round_up, ArithResult};

/// A half open byte range `[offset, offset + length)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteRange {
    /// First byte of the range.
    pub offset: u64,
    /// Number of bytes. May be zero, in which case the range covers nothing.
    pub length: u64,
}

impl ByteRange {
    /// Builds a range.
    pub const fn new(offset: u64, length: u64) -> Self {
        Self { offset, length }
    }

    /// One past the last byte, refusing to wrap.
    pub fn end(&self) -> ArithResult<u64> {
        range_end("byte range end", self.offset, self.length)
    }

    /// Whether the range covers no bytes.
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }
}

/// A set of byte ranges kept sorted, merged and non overlapping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtentList {
    ranges: Vec<ByteRange>,
}

impl ExtentList {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a set from ranges in any order.
    ///
    /// Empty ranges are dropped. Overlapping and touching ranges are merged, so
    /// the result is always minimal: no two ranges in it are adjacent.
    pub fn from_unsorted(mut input: Vec<ByteRange>) -> ArithResult<Self> {
        input.retain(|r| !r.is_empty());
        // Validate before sorting so an overflowing range is reported as such
        // rather than silently ordering oddly.
        for r in &input {
            r.end()?;
        }
        input.sort_unstable_by_key(|r| (r.offset, r.length));

        let mut ranges: Vec<ByteRange> = Vec::with_capacity(input.len());
        for r in input {
            match ranges.last_mut() {
                Some(last) => {
                    let last_end = last.end()?;
                    if r.offset <= last_end {
                        // Overlapping or touching: extend, never shrink.
                        let r_end = r.end()?;
                        if r_end > last_end {
                            last.length = r_end - last.offset;
                        }
                    } else {
                        ranges.push(r);
                    }
                }
                None => ranges.push(r),
            }
        }
        Ok(Self { ranges })
    }

    /// A set covering a single range.
    pub fn single(offset: u64, length: u64) -> ArithResult<Self> {
        Self::from_unsorted(vec![ByteRange::new(offset, length)])
    }

    /// The ranges, sorted ascending.
    pub fn ranges(&self) -> &[ByteRange] {
        &self.ranges
    }

    /// Number of ranges.
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    /// Whether the set covers no bytes.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Total number of bytes covered.
    pub fn total_bytes(&self) -> ArithResult<u64> {
        let mut total = 0u64;
        for r in &self.ranges {
            total = add_u64("extent total", total, r.length)?;
        }
        Ok(total)
    }

    /// Drops everything at or beyond `limit`, truncating a straddling range.
    ///
    /// A volume can report allocation past the end of its own partition when a
    /// layout is inconsistent. Clamping keeps that from becoming a read beyond
    /// the device.
    pub fn clamp_to(&self, limit: u64) -> ArithResult<Self> {
        let mut out = Vec::with_capacity(self.ranges.len());
        for r in &self.ranges {
            if r.offset >= limit {
                break;
            }
            let end = r.end()?.min(limit);
            out.push(ByteRange::new(r.offset, end - r.offset));
        }
        Ok(Self { ranges: out })
    }

    /// Expands every range outward to a multiple of `granularity`, then merges.
    ///
    /// Device reads happen in whole sectors, so a range that starts mid sector
    /// has to grow down to the sector boundary and up to the next one. Growing
    /// can make neighbours touch, which is why the result is re merged.
    pub fn align_outward(&self, granularity: u64) -> ArithResult<Self> {
        if granularity <= 1 {
            return Ok(self.clone());
        }
        let mut grown = Vec::with_capacity(self.ranges.len());
        for r in &self.ranges {
            let start = r.offset - (r.offset % granularity);
            let end = round_up("extent alignment", r.end()?, granularity)?;
            grown.push(ByteRange::new(start, end - start));
        }
        Self::from_unsorted(grown)
    }

    /// Splits the set into pieces of at most `chunk_size` bytes.
    ///
    /// Each piece stays inside one range, so a piece never spans a gap. Pieces
    /// are cut on multiples of `chunk_size` measured from zero, which makes the
    /// cut points stable: the same byte lands in the same piece boundary in
    /// every backup, which is what lets a later incremental reuse chunks.
    pub fn split_chunks(&self, chunk_size: u32) -> ArithResult<Vec<ByteRange>> {
        assert!(chunk_size > 0, "chunk size must be non zero");
        let size = u64::from(chunk_size);
        let mut out = Vec::new();
        for r in &self.ranges {
            let end = r.end()?;
            let mut at = r.offset;
            while at < end {
                // Distance to the next global chunk boundary.
                let to_boundary = size - (at % size);
                let take = to_boundary.min(end - at);
                out.push(ByteRange::new(at, take));
                at = add_u64("chunk split cursor", at, take)?;
            }
        }
        Ok(out)
    }

    /// The gaps between the ranges inside `[0, limit)`.
    ///
    /// Verification reports these so a sparse stream is explicit about what it
    /// does not carry, instead of leaving the operator to assume.
    pub fn gaps_within(&self, limit: u64) -> ArithResult<Vec<ByteRange>> {
        let mut gaps = Vec::new();
        let mut cursor = 0u64;
        for r in &self.ranges {
            if r.offset >= limit {
                break;
            }
            if r.offset > cursor {
                gaps.push(ByteRange::new(cursor, r.offset - cursor));
            }
            cursor = cursor.max(r.end()?);
        }
        if cursor < limit {
            gaps.push(ByteRange::new(cursor, limit - cursor));
        }
        Ok(gaps)
    }

    /// Whether `offset` is covered by some range.
    pub fn contains(&self, offset: u64) -> bool {
        self.ranges
            .binary_search_by(|r| {
                if offset < r.offset {
                    std::cmp::Ordering::Greater
                } else if r.end().map(|e| offset >= e).unwrap_or(true) {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(offset: u64, length: u64) -> ByteRange {
        ByteRange::new(offset, length)
    }

    #[test]
    fn merges_overlapping_and_touching() {
        let list = ExtentList::from_unsorted(vec![
            r(10, 5),
            r(0, 5),
            r(5, 5), // touches both neighbours
            r(100, 1),
            r(50, 10),
            r(55, 10), // overlaps the previous
        ])
        .unwrap();
        assert_eq!(list.ranges(), &[r(0, 15), r(50, 15), r(100, 1)]);
        assert_eq!(list.total_bytes().unwrap(), 31);
    }

    #[test]
    fn drops_empty_ranges() {
        let list = ExtentList::from_unsorted(vec![r(0, 0), r(10, 0), r(5, 2)]).unwrap();
        assert_eq!(list.ranges(), &[r(5, 2)]);
    }

    #[test]
    fn a_range_contained_in_another_is_absorbed() {
        let list = ExtentList::from_unsorted(vec![r(0, 100), r(10, 5)]).unwrap();
        assert_eq!(list.ranges(), &[r(0, 100)]);
    }

    #[test]
    fn rejects_overflowing_range() {
        assert!(ExtentList::from_unsorted(vec![r(u64::MAX, 2)]).is_err());
    }

    #[test]
    fn clamp_truncates_a_straddling_range() {
        let list = ExtentList::from_unsorted(vec![r(0, 100)]).unwrap();
        assert_eq!(list.clamp_to(40).unwrap().ranges(), &[r(0, 40)]);
        assert!(list.clamp_to(0).unwrap().is_empty());
        assert_eq!(list.clamp_to(1000).unwrap().ranges(), &[r(0, 100)]);
    }

    #[test]
    fn alignment_grows_outward_and_remerges() {
        let list = ExtentList::from_unsorted(vec![r(1, 1), r(600, 1)]).unwrap();
        let aligned = list.align_outward(512).unwrap();
        // 1..2 grows to 0..512, 600..601 grows to 512..1024, and the two touch.
        assert_eq!(aligned.ranges(), &[r(0, 1024)]);
    }

    #[test]
    fn alignment_of_one_is_a_no_op() {
        let list = ExtentList::from_unsorted(vec![r(3, 7)]).unwrap();
        assert_eq!(list.align_outward(1).unwrap(), list);
    }

    #[test]
    fn split_cuts_on_global_boundaries() {
        let list = ExtentList::from_unsorted(vec![r(1000, 3000)]).unwrap();
        let pieces = list.split_chunks(1024).unwrap();
        // Boundaries are multiples of 1024 measured from zero, not from the
        // start of the range.
        assert_eq!(
            pieces,
            vec![r(1000, 24), r(1024, 1024), r(2048, 1024), r(3072, 928)]
        );
        let covered: u64 = pieces.iter().map(|p| p.length).sum();
        assert_eq!(covered, 3000);
    }

    #[test]
    fn split_never_spans_a_gap() {
        let list = ExtentList::from_unsorted(vec![r(0, 10), r(2000, 10)]).unwrap();
        for piece in list.split_chunks(4096).unwrap() {
            assert!(piece.length <= 10);
        }
    }

    #[test]
    fn gaps_describe_what_is_not_covered() {
        let list = ExtentList::from_unsorted(vec![r(10, 10), r(50, 10)]).unwrap();
        assert_eq!(
            list.gaps_within(100).unwrap(),
            vec![r(0, 10), r(20, 30), r(60, 40)]
        );
        assert_eq!(ExtentList::new().gaps_within(5).unwrap(), vec![r(0, 5)]);
        assert!(ExtentList::new().gaps_within(0).unwrap().is_empty());
    }

    #[test]
    fn contains_uses_half_open_bounds() {
        let list = ExtentList::from_unsorted(vec![r(10, 10)]).unwrap();
        assert!(!list.contains(9));
        assert!(list.contains(10));
        assert!(list.contains(19));
        assert!(!list.contains(20));
    }
}
