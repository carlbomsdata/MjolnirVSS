//! The NTFS cluster allocation bitmap, and turning it into byte ranges.
//!
//! Windows hands the bitmap over through `FSCTL_GET_VOLUME_BITMAP`, one page at
//! a time, as a `VOLUME_BITMAP_BUFFER`:
//!
//! ```text
//! offset  0  LARGE_INTEGER StartingLcn   the first cluster this page describes
//! offset  8  LARGE_INTEGER BitmapSize    clusters from StartingLcn to the end
//! offset 16  BYTE          Buffer[]      one bit per cluster, least significant bit first
//! ```
//!
//! A volume of any size needs several pages, and the call reports
//! `ERROR_MORE_DATA` for every page but the last. That is a continuation
//! signal rather than a failure, but only when the structure that came back
//! with it is valid, which is what [`BitmapPage::parse`] establishes.
//!
//! # Why this matters more than it looks like it should
//!
//! Skipping free space makes a backup smaller, which is the obvious reason. The
//! less obvious one is correctness: the snapshot driver has no reason to
//! preserve the contents of clusters nobody wrote to, so reading a free region
//! through a shadow copy does not reliably give back what was there. Capturing
//! only what the filesystem says is in use avoids depending on that, and the
//! ranges nobody captured are recorded as gaps and defined to be zero, rather
//! than being quietly filled with whatever was read.
//!
//! # What this module does not do
//!
//! It does not talk to Windows. It decodes buffers and does arithmetic, so
//! every rule below is exercised by ordinary tests against synthetic bitmaps.

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::extents::{ByteRange, ExtentList};
use mjolnir_core::math;

use crate::boot::NtfsBootSector;

/// Size of the fixed part of `VOLUME_BITMAP_BUFFER`.
pub const BITMAP_HEADER_BYTES: usize = 16;

/// `FSCTL_GET_VOLUME_BITMAP`, from `winioctl.h`.
///
/// `CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 27, METHOD_NEITHER, FILE_ANY_ACCESS)`,
/// which works out as `0x0009006F`. Spelled out rather than computed so the
/// value can be checked against the header by eye.
pub const FSCTL_GET_VOLUME_BITMAP: u32 = 0x0009_006F;

/// The largest bitmap page this crate asks for, in bytes.
///
/// Two mebibytes describes sixteen million clusters, which is sixty four
/// gibibytes at the usual cluster size, so even a very large volume needs only a
/// handful of round trips. Asking for more wastes memory on small volumes and
/// gains nothing on large ones.
pub const DEFAULT_PAGE_BYTES: usize = 2 * 1024 * 1024;

/// One page of the bitmap, as it came back from the filesystem.
#[derive(Debug, Clone, Copy)]
pub struct BitmapPage<'a> {
    starting_lcn: u64,
    bitmap_size: u64,
    bits: &'a [u8],
}

impl<'a> BitmapPage<'a> {
    /// Decodes a page from the bytes the filesystem returned.
    ///
    /// `returned` is what `DeviceIoControl` reported it wrote, which is what
    /// bounds the meaningful part of `buffer`. A page whose header does not fit
    /// is rejected: `ERROR_MORE_DATA` alongside a truncated header means the
    /// call did not do what it says it did, and continuing would invent data.
    pub fn parse(buffer: &'a [u8], returned: usize) -> Result<Self> {
        if returned > buffer.len() {
            return Err(malformed(format!(
                "the filesystem reported writing {returned} bytes into a {} byte buffer",
                buffer.len()
            )));
        }
        if returned < BITMAP_HEADER_BYTES {
            return Err(malformed(format!(
                "the allocation bitmap came back with {returned} bytes, which is too few to hold its own header"
            )));
        }

        let starting_lcn = u64::from_le_bytes(buffer[0..8].try_into().expect("8 bytes"));
        let bitmap_size = u64::from_le_bytes(buffer[8..16].try_into().expect("8 bytes"));

        // Both fields are signed on the Windows side, and a negative value
        // would arrive here as an enormous unsigned one. Neither is meaningful.
        if starting_lcn > i64::MAX as u64 {
            return Err(malformed(format!(
                "the allocation bitmap reports a negative starting cluster ({starting_lcn:#x})"
            )));
        }
        if bitmap_size > i64::MAX as u64 {
            return Err(malformed(format!(
                "the allocation bitmap reports a negative size ({bitmap_size:#x})"
            )));
        }

        Ok(Self {
            starting_lcn,
            bitmap_size,
            bits: &buffer[BITMAP_HEADER_BYTES..returned],
        })
    }

    /// The first cluster this page describes.
    pub fn starting_lcn(&self) -> u64 {
        self.starting_lcn
    }

    /// What the page claims about the volume's size, in clusters.
    ///
    /// Microsoft documents this as the number of clusters from
    /// [`starting_lcn`](Self::starting_lcn) to the end of the volume. Only the
    /// first page's value is relied on, because implementations have been known
    /// to differ about whether later pages restate the remainder or the whole.
    pub fn bitmap_size(&self) -> u64 {
        self.bitmap_size
    }

    /// How many clusters this page carries bits for.
    pub fn bits_available(&self) -> u64 {
        self.bits.len() as u64 * 8
    }

    /// Whether the cluster `starting_lcn + index` is in use.
    ///
    /// Out of range indices read as free rather than panicking, because the
    /// caller bounds the scan by the volume size and the last byte of a page
    /// can carry bits past the end of the volume.
    pub fn is_allocated(&self, index: u64) -> bool {
        let byte = (index / 8) as usize;
        match self.bits.get(byte) {
            Some(b) => b & (1u8 << (index % 8)) != 0,
            None => false,
        }
    }

    /// The raw bits, for the scanner's byte at a time fast path.
    fn bytes(&self) -> &'a [u8] {
        self.bits
    }
}

/// Accumulates allocated cluster runs from successive bitmap pages.
///
/// Pages must arrive in order and must not skip clusters. Both rules are
/// enforced rather than assumed: a paging loop that quietly stopped making
/// progress, or restarted, would otherwise produce a backup missing whole
/// regions of a volume, and would produce it silently.
#[derive(Debug)]
pub struct AllocationScan {
    total_clusters: u64,
    next_lcn: u64,
    open_run: Option<(u64, u64)>,
    runs: Vec<(u64, u64)>,
    allocated: u64,
    pages: u32,
}

impl AllocationScan {
    /// Starts a scan from the first page, which must begin at cluster zero.
    ///
    /// The volume's size in clusters is taken from that page and never revised,
    /// which is what makes the rest of the scan independent of how later pages
    /// choose to report their own size.
    pub fn begin(first: &BitmapPage<'_>) -> Result<Self> {
        if first.starting_lcn() != 0 {
            return Err(malformed(format!(
                "the allocation bitmap started at cluster {} instead of zero",
                first.starting_lcn()
            )));
        }
        let mut scan = Self {
            total_clusters: first.bitmap_size(),
            next_lcn: 0,
            open_run: None,
            runs: Vec::new(),
            allocated: 0,
            pages: 0,
        };
        scan.accept(first)?;
        Ok(scan)
    }

    /// The volume's size in clusters, as the first page reported it.
    pub fn total_clusters(&self) -> u64 {
        self.total_clusters
    }

    /// The cluster the next page has to begin at.
    pub fn next_lcn(&self) -> u64 {
        self.next_lcn
    }

    /// Whether every cluster of the volume has been accounted for.
    pub fn is_complete(&self) -> bool {
        self.next_lcn >= self.total_clusters
    }

    /// Folds one more page into the scan.
    pub fn accept(&mut self, page: &BitmapPage<'_>) -> Result<()> {
        if page.starting_lcn() != self.next_lcn {
            return Err(malformed(format!(
                "the allocation bitmap jumped to cluster {} when cluster {} was expected, so part of the volume would have been missed",
                page.starting_lcn(),
                self.next_lcn
            )));
        }
        if self.is_complete() {
            return Err(malformed(
                "the allocation bitmap carried on past the end of the volume".to_owned(),
            ));
        }

        let remaining = self.total_clusters - self.next_lcn;
        let count = page.bits_available().min(remaining);
        if count == 0 {
            return Err(malformed(format!(
                "the allocation bitmap returned no cluster data at cluster {}, so the scan could not make progress",
                self.next_lcn
            )));
        }

        self.scan_page(page, count);
        self.next_lcn = math::add_u64("bitmap cursor", self.next_lcn, count)
            .map_err(|e| malformed(e.to_string()))?;
        self.pages = self.pages.saturating_add(1);
        Ok(())
    }

    /// Walks one page's bits, extending or starting runs.
    ///
    /// Whole bytes of `0x00` and `0xFF` are the overwhelming majority on a real
    /// volume, so they are handled without touching individual bits. A sixty
    /// gibibyte volume has sixteen million clusters, and bit at a time would be
    /// the slowest part of a backup that is otherwise bounded by the disk.
    fn scan_page(&mut self, page: &BitmapPage<'_>, count: u64) {
        let base = page.starting_lcn();
        let bytes = page.bytes();
        let whole_bytes = (count / 8) as usize;

        for (i, byte) in bytes.iter().take(whole_bytes).enumerate() {
            let first = base + i as u64 * 8;
            match byte {
                0x00 => self.close_run(),
                0xFF => self.extend_run(first, 8),
                _ => {
                    for bit in 0..8 {
                        if byte & (1u8 << bit) != 0 {
                            self.extend_run(first + bit, 1);
                        } else {
                            self.close_run();
                        }
                    }
                }
            }
        }

        for index in whole_bytes as u64 * 8..count {
            if page.is_allocated(index) {
                self.extend_run(base + index, 1);
            } else {
                self.close_run();
            }
        }
    }

    fn extend_run(&mut self, lcn: u64, length: u64) {
        self.allocated += length;
        match &mut self.open_run {
            Some((start, len)) if *start + *len == lcn => *len += length,
            Some(_) => {
                self.close_run();
                self.open_run = Some((lcn, length));
            }
            None => self.open_run = Some((lcn, length)),
        }
    }

    fn close_run(&mut self) {
        if let Some(run) = self.open_run.take() {
            self.runs.push(run);
        }
    }

    /// Finishes the scan, refusing an incomplete one.
    ///
    /// Stopping early would produce a backup whose missing regions look exactly
    /// like free space, so an incomplete scan is an error rather than a partial
    /// result.
    pub fn finish(mut self) -> Result<Allocation> {
        if !self.is_complete() {
            return Err(malformed(format!(
                "the allocation bitmap stopped at cluster {} of {}, so {} clusters were never described",
                self.next_lcn,
                self.total_clusters,
                self.total_clusters - self.next_lcn
            )));
        }
        self.close_run();
        Ok(Allocation {
            total_clusters: self.total_clusters,
            allocated_clusters: self.allocated,
            runs: self.runs,
            pages: self.pages,
        })
    }
}

/// Which clusters of a volume are in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    total_clusters: u64,
    allocated_clusters: u64,
    runs: Vec<(u64, u64)>,
    pages: u32,
}

impl Allocation {
    /// Builds an allocation directly from cluster runs, for tests and for
    /// callers that obtained the information some other way.
    pub fn from_runs(total_clusters: u64, runs: Vec<(u64, u64)>) -> Result<Self> {
        let mut sorted = runs;
        sorted.sort_unstable();
        let mut allocated = 0u64;
        let mut previous_end = 0u64;
        for (start, length) in &sorted {
            if *length == 0 {
                return Err(malformed("a cluster run covers no clusters".to_owned()));
            }
            if *start < previous_end {
                return Err(malformed(format!(
                    "cluster runs overlap at cluster {start}"
                )));
            }
            let end = math::add_u64("cluster run end", *start, *length)
                .map_err(|e| malformed(e.to_string()))?;
            if end > total_clusters {
                return Err(malformed(format!(
                    "a cluster run ends at cluster {end}, past the volume's {total_clusters} clusters"
                )));
            }
            allocated = math::add_u64("allocated clusters", allocated, *length)
                .map_err(|e| malformed(e.to_string()))?;
            previous_end = end;
        }
        Ok(Self {
            total_clusters,
            allocated_clusters: allocated,
            runs: sorted,
            pages: 1,
        })
    }

    /// Total clusters on the volume.
    pub fn total_clusters(&self) -> u64 {
        self.total_clusters
    }

    /// Clusters in use.
    pub fn allocated_clusters(&self) -> u64 {
        self.allocated_clusters
    }

    /// Number of contiguous runs of allocated clusters.
    pub fn run_count(&self) -> usize {
        self.runs.len()
    }

    /// How many bitmap pages the scan consumed.
    pub fn pages(&self) -> u32 {
        self.pages
    }

    /// The runs, as `(first cluster, cluster count)`, ascending.
    pub fn runs(&self) -> &[(u64, u64)] {
        &self.runs
    }

    /// Converts the runs into byte ranges.
    pub fn to_byte_ranges(&self, cluster_size: u32) -> Result<Vec<ByteRange>> {
        if cluster_size == 0 {
            return Err(malformed(
                "the volume reports a cluster size of zero".to_owned(),
            ));
        }
        let size = u64::from(cluster_size);
        let mut out = Vec::with_capacity(self.runs.len());
        for (start, length) in &self.runs {
            let offset = math::mul_u64("cluster offset", *start, size)
                .map_err(|e| malformed(e.to_string()))?;
            let bytes = math::mul_u64("cluster run bytes", *length, size)
                .map_err(|e| malformed(e.to_string()))?;
            // Proves the range does not wrap before it reaches the extent list.
            math::add_u64("cluster run end", offset, bytes)
                .map_err(|e| malformed(e.to_string()))?;
            out.push(ByteRange::new(offset, bytes));
        }
        Ok(out)
    }
}

/// What a used block capture will read, and what it deliberately will not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsedBlockPlan {
    /// The byte ranges to capture, sorted, merged and non overlapping.
    pub extents: ExtentList,
    /// Cluster size the plan was built with.
    pub cluster_size: u32,
    /// Clusters the bitmap described.
    pub clusters_total: u64,
    /// Clusters the bitmap said were in use.
    pub clusters_allocated: u64,
    /// Bytes of the partition the bitmap describes.
    pub described_bytes: u64,
    /// Bytes at the end of the partition the filesystem says nothing about.
    ///
    /// Normally one sector, holding the copy of the boot sector NTFS keeps
    /// there. A larger value means the partition was grown without the
    /// filesystem being grown to match, and every byte of it is captured,
    /// because nothing available says it is free.
    pub undescribed_tail_bytes: u64,
    /// Bytes captured because the format requires them, over and above the
    /// clusters the bitmap marked in use.
    pub reserved_bytes: u64,
}

impl UsedBlockPlan {
    /// Total bytes the capture will read.
    pub fn captured_bytes(&self) -> Result<u64> {
        self.extents
            .total_bytes()
            .map_err(|e| malformed(e.to_string()))
    }

    /// Number of extents, which is what the manifest records.
    pub fn extent_count(&self) -> usize {
        self.extents.len()
    }
}

/// Works out what to capture from a volume's allocation and its boot sector.
///
/// Beyond the allocated clusters, three things are always included, because a
/// volume missing any of them cannot be reconstructed:
///
/// * the first sixteen clusters, holding `$Boot` and the start of the metadata
///   files, which are allocated in every healthy volume and are included anyway
///   so that a volume whose bitmap is wrong about them still restores;
/// * the master file table and its mirror, at the offsets the boot sector
///   itself gives;
/// * everything between the end of the filesystem and the end of the partition,
///   which is where NTFS keeps its copy of the boot sector and which the bitmap
///   does not cover.
///
/// Geometry that does not add up is refused rather than clamped. A bitmap
/// claiming more clusters than the partition holds means one of the two is
/// being misread, and guessing which would put every offset in the backup in
/// doubt.
pub fn plan_used_blocks(
    boot: &NtfsBootSector,
    allocation: &Allocation,
    partition_length: u64,
) -> Result<UsedBlockPlan> {
    let cluster_size = u32::try_from(boot.bytes_per_cluster()).map_err(|_| {
        Error::new(
            ExitCode::Unsupported,
            "the volume uses a cluster size MjolnirVSS does not handle",
            format!(
                "the boot sector describes {} byte clusters",
                boot.bytes_per_cluster()
            ),
            "this volume cannot be captured by used block imaging; capture it whole instead",
        )
    })?;
    let sector_size = u64::from(boot.bytes_per_sector);

    let described_bytes = math::mul_u64(
        "bitmap coverage",
        allocation.total_clusters(),
        u64::from(cluster_size),
    )?;

    // The filesystem's own idea of its size, which should agree with the bitmap
    // to within one cluster: NTFS counts sectors, and the bitmap counts whole
    // clusters, so the last partial cluster is not described.
    let volume_bytes = boot.volume_bytes()?;
    if volume_bytes > partition_length {
        return Err(Error::new(
            ExitCode::Unsupported,
            "the filesystem claims to be larger than its partition",
            format!(
                "the boot sector describes {volume_bytes} bytes inside a {partition_length} byte partition"
            ),
            "this volume cannot be captured safely; check the disk with chkdsk",
        ));
    }

    if described_bytes > partition_length {
        return Err(Error::new(
            ExitCode::Unsupported,
            "the filesystem describes more space than the partition holds",
            format!(
                "the allocation bitmap covers {described_bytes} bytes but the partition is {partition_length} bytes, so the two disagree about the size of the volume"
            ),
            "MjolnirVSS will not guess which is right; capture this partition without used block imaging, or check the disk with chkdsk",
        ));
    }

    let mut allocated = allocation.to_byte_ranges(cluster_size)?;
    let from_bitmap = ExtentList::from_unsorted(allocated.clone())?.total_bytes()?;

    // $Boot and the metadata files that follow it. Sixteen clusters is what
    // NTFS reserves at the front of every volume.
    let front = math::mul_u64("reserved front", u64::from(cluster_size), 16)?.min(partition_length);
    allocated.push(ByteRange::new(0, front));

    // The master file table and its mirror, at the offsets the boot sector
    // gives, one record each. Their full extents are allocated and already
    // covered; this is here so a volume whose bitmap has been damaged still
    // yields something a reader can orient itself in.
    let record = u64::from(boot.bytes_per_file_record).max(sector_size);
    for offset in [boot.mft_offset()?, boot.mft_mirror_offset()?] {
        if let Some(available) = partition_length.checked_sub(offset) {
            allocated.push(ByteRange::new(offset, record.min(available)));
        }
    }

    // Everything the filesystem does not describe, at the end of the partition.
    // On a normal volume this is the single sector holding NTFS's copy of the
    // boot sector.
    let uncovered_from = described_bytes.min(volume_bytes);
    let undescribed_tail_bytes = partition_length - uncovered_from;
    if undescribed_tail_bytes > 0 {
        allocated.push(ByteRange::new(uncovered_from, undescribed_tail_bytes));
    }

    let extents = ExtentList::from_unsorted(allocated)?
        .clamp_to(partition_length)?
        .align_outward(sector_size)?
        .clamp_to(partition_length)?;

    let total = extents.total_bytes()?;
    let reserved_bytes = total.saturating_sub(from_bitmap);

    Ok(UsedBlockPlan {
        extents,
        cluster_size,
        clusters_total: allocation.total_clusters(),
        clusters_allocated: allocation.allocated_clusters(),
        described_bytes,
        undescribed_tail_bytes,
        reserved_bytes,
    })
}

fn malformed(detail: String) -> Error {
    Error::new(
        ExitCode::Unsupported,
        "the volume's allocation bitmap could not be read",
        detail,
        "MjolnirVSS will fall back to copying the whole volume, which is slower but does not depend on the bitmap",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `VOLUME_BITMAP_BUFFER` the way the filesystem would.
    fn page(starting_lcn: u64, bitmap_size: u64, bits: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(BITMAP_HEADER_BYTES + bits.len());
        out.extend_from_slice(&starting_lcn.to_le_bytes());
        out.extend_from_slice(&bitmap_size.to_le_bytes());
        out.extend_from_slice(bits);
        out
    }

    /// Packs a cluster allocation pattern into bitmap bytes, least significant
    /// bit first, which is the order Windows documents.
    fn bits_from(pattern: &[bool]) -> Vec<u8> {
        let mut out = vec![0u8; pattern.len().div_ceil(8)];
        for (i, allocated) in pattern.iter().enumerate() {
            if *allocated {
                out[i / 8] |= 1u8 << (i % 8);
            }
        }
        out
    }

    fn scan_one(pattern: &[bool]) -> Allocation {
        let bytes = bits_from(pattern);
        let buffer = page(0, pattern.len() as u64, &bytes);
        let parsed = BitmapPage::parse(&buffer, buffer.len()).unwrap();
        AllocationScan::begin(&parsed).unwrap().finish().unwrap()
    }

    #[test]
    fn the_least_significant_bit_of_the_first_byte_is_cluster_zero() {
        let allocation = scan_one(&[true, false, false, false, false, false, false, false]);
        assert_eq!(allocation.runs(), &[(0, 1)]);

        let allocation = scan_one(&[false, false, false, false, false, false, false, true]);
        assert_eq!(allocation.runs(), &[(7, 1)]);
    }

    #[test]
    fn adjacent_allocated_clusters_become_one_run() {
        let allocation = scan_one(&[true, true, true, false, true, true, false, false]);
        assert_eq!(allocation.runs(), &[(0, 3), (4, 2)]);
        assert_eq!(allocation.allocated_clusters(), 5);
    }

    /// Fragmentation is the case used block imaging exists for, and the case
    /// most likely to expose an off by one in run tracking.
    #[test]
    fn heavy_fragmentation_is_tracked_exactly() {
        let pattern: Vec<bool> = (0..1024).map(|i| i % 3 == 0).collect();
        let allocation = scan_one(&pattern);

        assert_eq!(allocation.run_count(), 342);
        assert_eq!(allocation.allocated_clusters(), 342);
        for (start, length) in allocation.runs() {
            assert_eq!(*length, 1);
            assert_eq!(start % 3, 0);
        }
    }

    /// A run crossing a byte boundary must not be split, and a run crossing the
    /// fast path boundary between whole bytes and the ragged tail must not
    /// either.
    #[test]
    fn runs_survive_byte_and_page_tail_boundaries() {
        let mut pattern = vec![false; 30];
        for slot in pattern.iter_mut().take(20).skip(6) {
            *slot = true;
        }
        let allocation = scan_one(&pattern);
        assert_eq!(allocation.runs(), &[(6, 14)]);
    }

    #[test]
    fn a_fully_allocated_volume_is_one_run() {
        let allocation = scan_one(&vec![true; 4096]);
        assert_eq!(allocation.runs(), &[(0, 4096)]);
        assert_eq!(allocation.allocated_clusters(), 4096);
    }

    #[test]
    fn an_empty_volume_has_no_runs() {
        let allocation = scan_one(&vec![false; 4096]);
        assert!(allocation.runs().is_empty());
        assert_eq!(allocation.allocated_clusters(), 0);
    }

    /// Pagination is where a wrong loop silently loses whole regions, so the
    /// scan is fed the same volume in one page and in many and the answers are
    /// required to be identical.
    #[test]
    fn paging_produces_the_same_answer_as_one_page() {
        let pattern: Vec<bool> = (0..4096).map(|i| (i / 7) % 2 == 0).collect();
        let whole = scan_one(&pattern);

        let all_bits = bits_from(&pattern);
        let mut scan: Option<AllocationScan> = None;
        // 64 bytes is 512 clusters a page, so the volume needs eight pages.
        for (i, slice) in all_bits.chunks(64).enumerate() {
            let starting_lcn = i as u64 * 512;
            let remaining = pattern.len() as u64 - starting_lcn;
            let buffer = page(starting_lcn, remaining, slice);
            let parsed = BitmapPage::parse(&buffer, buffer.len()).unwrap();
            match &mut scan {
                None => scan = Some(AllocationScan::begin(&parsed).unwrap()),
                Some(s) => s.accept(&parsed).unwrap(),
            }
        }
        let paged = scan.unwrap().finish().unwrap();

        assert_eq!(paged.runs(), whole.runs());
        assert_eq!(paged.allocated_clusters(), whole.allocated_clusters());
        assert_eq!(paged.pages(), 8);
    }

    /// A run spanning a page boundary must come back as one run, not two.
    #[test]
    fn a_run_spanning_two_pages_stays_one_run() {
        let pattern = vec![true; 1024];
        let all_bits = bits_from(&pattern);

        let first = page(0, 1024, &all_bits[..64]);
        let second = page(512, 512, &all_bits[64..]);

        let mut scan =
            AllocationScan::begin(&BitmapPage::parse(&first, first.len()).unwrap()).unwrap();
        scan.accept(&BitmapPage::parse(&second, second.len()).unwrap())
            .unwrap();
        let allocation = scan.finish().unwrap();

        assert_eq!(allocation.runs(), &[(0, 1024)]);
    }

    #[test]
    fn bits_past_the_end_of_the_volume_are_ignored() {
        // Ten clusters, but the last byte carries bits for sixteen.
        let buffer = page(0, 10, &[0xFF, 0xFF]);
        let parsed = BitmapPage::parse(&buffer, buffer.len()).unwrap();
        let allocation = AllocationScan::begin(&parsed).unwrap().finish().unwrap();
        assert_eq!(allocation.runs(), &[(0, 10)]);
        assert_eq!(allocation.allocated_clusters(), 10);
    }

    // ---- what must be refused ---------------------------------------------

    #[test]
    fn a_truncated_header_is_refused() {
        let buffer = page(0, 64, &[0xFF]);
        let err = BitmapPage::parse(&buffer, 12).unwrap_err();
        assert!(err.why().contains("too few"), "{}", err.why());
    }

    #[test]
    fn a_report_longer_than_the_buffer_is_refused() {
        let buffer = page(0, 64, &[0xFF]);
        let err = BitmapPage::parse(&buffer, buffer.len() + 1).unwrap_err();
        assert!(err.why().contains("byte buffer"), "{}", err.why());
    }

    #[test]
    fn negative_header_values_are_refused() {
        let buffer = page(u64::MAX, 64, &[0xFF]);
        assert!(BitmapPage::parse(&buffer, buffer.len()).is_err());

        let buffer = page(0, u64::MAX, &[0xFF]);
        assert!(BitmapPage::parse(&buffer, buffer.len()).is_err());
    }

    #[test]
    fn a_scan_not_starting_at_cluster_zero_is_refused() {
        let buffer = page(8, 64, &[0xFF]);
        let parsed = BitmapPage::parse(&buffer, buffer.len()).unwrap();
        let err = AllocationScan::begin(&parsed).unwrap_err();
        assert!(err.why().contains("instead of zero"), "{}", err.why());
    }

    /// The dangerous failure: a paging loop that restarts, or jumps, would
    /// produce a backup missing a region that looks exactly like free space.
    #[test]
    fn a_page_that_skips_or_repeats_clusters_is_refused() {
        let bits = bits_from(&vec![true; 1024]);
        let first = page(0, 1024, &bits[..64]);
        let mut scan =
            AllocationScan::begin(&BitmapPage::parse(&first, first.len()).unwrap()).unwrap();

        // Jumping forward would silently skip clusters 512..768.
        let skipped = page(768, 256, &bits[96..]);
        assert!(scan
            .accept(&BitmapPage::parse(&skipped, skipped.len()).unwrap())
            .is_err());

        // Repeating would double count them.
        let repeated = page(0, 1024, &bits[..64]);
        assert!(scan
            .accept(&BitmapPage::parse(&repeated, repeated.len()).unwrap())
            .is_err());
    }

    /// A page carrying no bits cannot advance the scan, so accepting it would
    /// spin forever. This is the loop guard the continuation rule needs.
    #[test]
    fn a_page_with_no_data_is_refused_rather_than_looped_on() {
        let first = page(0, 1024, &[0xFF; 64]);
        let mut scan =
            AllocationScan::begin(&BitmapPage::parse(&first, first.len()).unwrap()).unwrap();

        let empty = page(512, 512, &[]);
        let err = scan
            .accept(&BitmapPage::parse(&empty, BITMAP_HEADER_BYTES).unwrap())
            .unwrap_err();
        assert!(
            err.why().contains("could not make progress"),
            "{}",
            err.why()
        );
    }

    #[test]
    fn an_incomplete_scan_is_refused() {
        let bits = bits_from(&vec![true; 1024]);
        let first = page(0, 1024, &bits[..64]);
        let scan = AllocationScan::begin(&BitmapPage::parse(&first, first.len()).unwrap()).unwrap();
        assert!(!scan.is_complete());

        let err = scan.finish().unwrap_err();
        assert!(err.why().contains("never described"), "{}", err.why());
    }

    #[test]
    fn carrying_on_past_the_end_of_the_volume_is_refused() {
        let bits = bits_from(&vec![true; 512]);
        let first = page(0, 512, &bits);
        let mut scan =
            AllocationScan::begin(&BitmapPage::parse(&first, first.len()).unwrap()).unwrap();
        assert!(scan.is_complete());

        let extra = page(512, 0, &bits);
        assert!(scan
            .accept(&BitmapPage::parse(&extra, extra.len()).unwrap())
            .is_err());
    }

    // ---- runs to bytes ----------------------------------------------------

    #[test]
    fn runs_convert_to_byte_ranges_at_the_cluster_size() {
        let allocation = Allocation::from_runs(100, vec![(0, 2), (10, 3)]).unwrap();
        let ranges = allocation.to_byte_ranges(4096).unwrap();
        assert_eq!(
            ranges,
            vec![ByteRange::new(0, 8192), ByteRange::new(40960, 12288)]
        );
    }

    #[test]
    fn overlapping_or_out_of_range_runs_are_refused() {
        assert!(Allocation::from_runs(100, vec![(0, 10), (5, 10)]).is_err());
        assert!(Allocation::from_runs(100, vec![(95, 10)]).is_err());
        assert!(Allocation::from_runs(100, vec![(0, 0)]).is_err());
    }

    #[test]
    fn a_run_that_would_overflow_is_refused() {
        let allocation = Allocation {
            total_clusters: u64::MAX,
            allocated_clusters: 1,
            runs: vec![(u64::MAX / 2, 4)],
            pages: 1,
        };
        assert!(allocation.to_byte_ranges(4096).is_err());
    }

    // ---- planning a capture ----------------------------------------------

    /// A boot sector describing a volume of `total_sectors` sectors, with the
    /// given geometry. The master file table sits a third of the way in, which
    /// is roughly where Windows puts it.
    fn boot(bytes_per_sector: u16, sectors_per_cluster: u32, total_sectors: u64) -> NtfsBootSector {
        let clusters = total_sectors / u64::from(sectors_per_cluster);
        NtfsBootSector {
            bytes_per_sector,
            sectors_per_cluster,
            total_sectors,
            mft_cluster: clusters / 3,
            mft_mirror_cluster: 2,
            bytes_per_file_record: 1024,
            serial: 0x1234_5678_9ABC_DEF0,
        }
    }

    /// The partition a volume of this shape lives in: the filesystem plus the
    /// one sector NTFS keeps its spare boot sector in, which the filesystem
    /// does not count as its own.
    fn partition_for(boot: &NtfsBootSector) -> u64 {
        boot.volume_bytes().unwrap() + u64::from(boot.bytes_per_sector)
    }

    /// A volume where only the front metadata and the master file table are in
    /// use, which is what a freshly formatted volume looks like.
    fn nearly_empty(clusters: u64, mft_cluster: u64) -> Allocation {
        Allocation::from_runs(clusters, vec![(0, 16), (mft_cluster, 64)]).unwrap()
    }

    #[test]
    fn a_plan_covers_the_allocated_clusters_and_nothing_else_in_the_middle() {
        let b = boot(512, 8, 1024 * 1024);
        let clusters = 1024 * 1024 / 8;
        let allocation = nearly_empty(clusters, b.mft_cluster);
        let plan = plan_used_blocks(&b, &allocation, partition_for(&b)).unwrap();

        assert_eq!(plan.cluster_size, 4096);
        assert_eq!(plan.clusters_total, clusters);
        assert_eq!(plan.clusters_allocated, 80);

        // The middle of the volume is free, and must not be read.
        let middle = b.volume_bytes().unwrap() / 2;
        assert!(!plan.extents.contains(middle));

        // What is allocated is covered.
        assert!(plan.extents.contains(0));
        assert!(plan.extents.contains(b.mft_offset().unwrap()));
    }

    /// The copy of the boot sector NTFS keeps in the last sector of the
    /// partition is outside the bitmap entirely. A volume restored without it
    /// still mounts, but chkdsk and every recovery tool lose their fallback,
    /// so it is captured deliberately.
    #[test]
    fn the_spare_boot_sector_at_the_end_of_the_partition_is_captured() {
        let b = boot(512, 8, 1024 * 1024);
        let partition = partition_for(&b);
        let allocation = nearly_empty(1024 * 1024 / 8, b.mft_cluster);
        let plan = plan_used_blocks(&b, &allocation, partition).unwrap();

        assert_eq!(plan.undescribed_tail_bytes, 512);
        assert!(plan.extents.contains(partition - 1));
        assert!(plan.extents.contains(partition - 512));

        // And nothing past the end of the partition.
        let last = plan.extents.ranges().last().unwrap();
        assert_eq!(last.end().unwrap(), partition);
    }

    /// Both copies of the master file table are captured at the offsets the
    /// boot sector gives, even if the bitmap somehow failed to mark them.
    #[test]
    fn both_copies_of_the_master_file_table_are_captured() {
        let b = boot(512, 8, 1024 * 1024);
        // A bitmap that says nothing at all is in use.
        let allocation = Allocation::from_runs(1024 * 1024 / 8, vec![]).unwrap();
        let plan = plan_used_blocks(&b, &allocation, partition_for(&b)).unwrap();

        assert_eq!(plan.clusters_allocated, 0);
        assert!(plan.extents.contains(b.mft_offset().unwrap()));
        assert!(plan.extents.contains(b.mft_mirror_offset().unwrap()));
        assert!(plan.extents.contains(0), "the boot sector itself");
        assert!(plan.reserved_bytes > 0);
    }

    /// Metadata sitting near the very end of the volume is the case where an
    /// off by one in the tail handling would silently drop it.
    #[test]
    fn allocation_at_the_very_end_of_the_volume_is_captured() {
        let b = boot(512, 8, 1024 * 1024);
        let clusters = 1024 * 1024 / 8;
        let allocation = Allocation::from_runs(clusters, vec![(0, 16), (clusters - 4, 4)]).unwrap();
        let partition = partition_for(&b);
        let plan = plan_used_blocks(&b, &allocation, partition).unwrap();

        let last_cluster_start = (clusters - 1) * 4096;
        assert!(plan.extents.contains(last_cluster_start));
        assert!(plan.extents.contains(last_cluster_start + 4095));
    }

    #[test]
    fn a_full_volume_is_captured_whole() {
        let b = boot(512, 8, 1024 * 1024);
        let clusters = 1024 * 1024 / 8;
        let allocation = Allocation::from_runs(clusters, vec![(0, clusters)]).unwrap();
        let partition = partition_for(&b);
        let plan = plan_used_blocks(&b, &allocation, partition).unwrap();

        assert_eq!(plan.extent_count(), 1);
        assert_eq!(plan.captured_bytes().unwrap(), partition);
        assert_eq!(plan.extents.gaps_within(partition).unwrap(), Vec::new());
    }

    /// The point of the whole exercise: a mostly empty volume produces a
    /// capture much smaller than the partition.
    #[test]
    fn an_empty_volume_captures_almost_nothing() {
        let b = boot(512, 8, 16 * 1024 * 1024); // 8 GiB
        let clusters = 16 * 1024 * 1024 / 8;
        let allocation = nearly_empty(clusters, b.mft_cluster);
        let partition = partition_for(&b);
        let plan = plan_used_blocks(&b, &allocation, partition).unwrap();

        let captured = plan.captured_bytes().unwrap();
        assert!(
            captured < partition / 1000,
            "captured {captured} of {partition}"
        );
    }

    /// Fragmentation must not cost coverage: every allocated cluster is inside
    /// some extent, and the extents plus their gaps account for the partition
    /// exactly.
    #[test]
    fn a_fragmented_volume_is_covered_completely_and_exactly() {
        let b = boot(512, 8, 65_536);
        let clusters = 65_536 / 8;
        let runs: Vec<(u64, u64)> = (0..clusters / 4).map(|i| (i * 4, 2)).collect();
        let allocation = Allocation::from_runs(clusters, runs.clone()).unwrap();
        let partition = partition_for(&b);
        let plan = plan_used_blocks(&b, &allocation, partition).unwrap();

        for (start, length) in &runs {
            for c in *start..*start + *length {
                assert!(
                    plan.extents.contains(c * 4096),
                    "cluster {c} was not captured"
                );
            }
        }

        let covered = plan.captured_bytes().unwrap();
        let gaps: u64 = plan
            .extents
            .gaps_within(partition)
            .unwrap()
            .iter()
            .map(|g| g.length)
            .sum();
        assert_eq!(covered + gaps, partition);
    }

    /// Every extent must be sorted, non overlapping and inside the partition,
    /// because each one becomes a write against a disk during recovery.
    #[test]
    fn extents_are_ordered_disjoint_and_inside_the_partition() {
        let b = boot(512, 8, 262_144);
        let clusters = 262_144 / 8;
        let runs: Vec<(u64, u64)> = (0..50).map(|i| (i * 500, 37)).collect();
        let allocation = Allocation::from_runs(clusters, runs).unwrap();
        let partition = partition_for(&b);
        let plan = plan_used_blocks(&b, &allocation, partition).unwrap();

        let mut previous_end = 0u64;
        for range in plan.extents.ranges() {
            assert!(
                range.offset >= previous_end,
                "extent at {} overlaps the one ending at {previous_end}",
                range.offset
            );
            let end = range.end().unwrap();
            assert!(end <= partition, "extent ends at {end}, past {partition}");
            previous_end = end;
        }
    }

    /// 512 native, 512e and 4Kn all have to produce a plan, and the 4Kn one has
    /// to align to its larger sector.
    #[test]
    fn every_supported_sector_layout_plans_correctly() {
        // 512 native and 512e are identical as far as anything here can see:
        // the logical sector is 512 in both.
        for (sector, per_cluster) in [(512u16, 8u32), (512, 1), (4096, 1), (4096, 2)] {
            let total_sectors = 1024 * 1024 / u64::from(sector) * 64;
            let b = boot(sector, per_cluster, total_sectors);
            let clusters = total_sectors / u64::from(per_cluster);
            let allocation = nearly_empty(clusters, b.mft_cluster);
            let partition = partition_for(&b);
            let plan = plan_used_blocks(&b, &allocation, partition).unwrap();

            assert_eq!(
                plan.cluster_size,
                u32::from(sector) * per_cluster,
                "{sector}/{per_cluster}"
            );
            for range in plan.extents.ranges() {
                assert_eq!(
                    range.offset % u64::from(sector),
                    0,
                    "extent at {} is not sector aligned for {sector} byte sectors",
                    range.offset
                );
            }
            assert!(plan.extents.contains(partition - 1));
        }
    }

    /// A partition grown without the filesystem being grown leaves a region
    /// nothing describes. It cannot be assumed free, so it is captured, and the
    /// manifest records how much of it there was.
    #[test]
    fn a_partition_larger_than_its_filesystem_captures_the_undescribed_tail() {
        let b = boot(512, 8, 65_536);
        let clusters = 65_536 / 8;
        let allocation = nearly_empty(clusters, b.mft_cluster);
        let filesystem = b.volume_bytes().unwrap();
        let partition = filesystem + 1_048_576;

        let plan = plan_used_blocks(&b, &allocation, partition).unwrap();
        assert_eq!(plan.undescribed_tail_bytes, 1_048_576);
        assert!(plan.extents.contains(filesystem));
        assert!(plan.extents.contains(partition - 1));
    }

    // ---- geometry that does not add up ------------------------------------

    #[test]
    fn a_bitmap_claiming_more_space_than_the_partition_is_refused() {
        let b = boot(512, 8, 65_536);
        let clusters = 65_536 / 8;
        let allocation = Allocation::from_runs(clusters * 2, vec![(0, 16)]).unwrap();
        let err = plan_used_blocks(&b, &allocation, partition_for(&b)).unwrap_err();
        assert!(err.what().contains("more space than the partition holds"));
    }

    #[test]
    fn a_filesystem_claiming_more_space_than_the_partition_is_refused() {
        let b = boot(512, 8, 65_536);
        let allocation = nearly_empty(65_536 / 8, b.mft_cluster);
        let err = plan_used_blocks(&b, &allocation, 4096).unwrap_err();
        assert!(
            err.what().contains("larger than its partition"),
            "{}",
            err.what()
        );
    }

    /// Nothing in a plan may depend on reading past the end of the device, so
    /// a volume occupying the whole partition with no spare sector still works.
    #[test]
    fn a_partition_exactly_the_size_of_its_filesystem_is_handled() {
        let b = boot(512, 8, 65_536);
        let clusters = 65_536 / 8;
        let allocation = nearly_empty(clusters, b.mft_cluster);
        let partition = b.volume_bytes().unwrap();

        let plan = plan_used_blocks(&b, &allocation, partition).unwrap();
        assert_eq!(plan.undescribed_tail_bytes, 0);
        for range in plan.extents.ranges() {
            assert!(range.end().unwrap() <= partition);
        }
    }
}
