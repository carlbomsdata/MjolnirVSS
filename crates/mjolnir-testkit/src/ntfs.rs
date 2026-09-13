//! A synthetic NTFS volume, for testing used block imaging.
//!
//! Enough of NTFS to be recognised and planned against: a valid boot sector, the
//! copy of it NTFS keeps in the last sector of the partition, and a chosen set
//! of allocated clusters. There is no master file table, no directory tree and
//! no file data, because none of that is what used block imaging depends on.
//!
//! # The part that makes the test mean something
//!
//! Free clusters are filled with **garbage rather than zeros**. A used block
//! capture is supposed not to read them, and a restore is supposed to leave them
//! alone. If free space were zeros, a capture that read everything and a capture
//! that read nothing would restore to the same disk, and the test would pass
//! either way. With garbage in the free space, a restored volume can only come
//! out zero there if the free space really was skipped.

use mjolnir_ntfs::bitmap::Allocation;
use mjolnir_ntfs::boot::{NtfsBootSector, NTFS_OEM_ID};

/// Byte written into every free cluster.
///
/// Chosen so that finding it in a restored volume is unambiguous, and so that a
/// buffer that was never written stands out from one that was.
pub const FREE_SPACE_FILL: u8 = 0xEE;

/// A synthetic NTFS volume laid out inside one partition.
#[derive(Debug, Clone)]
pub struct SyntheticNtfs {
    /// Size of the partition holding the volume.
    pub partition_bytes: u64,
    /// Logical sector size.
    pub bytes_per_sector: u16,
    /// Sectors per cluster.
    pub sectors_per_cluster: u32,
    /// Sectors the filesystem claims, which is one less than the partition
    /// holds: the last one carries the spare boot sector.
    pub total_sectors: u64,
    /// First cluster of the master file table.
    pub mft_cluster: u64,
    /// First cluster of its mirror.
    pub mft_mirror_cluster: u64,
    /// Allocated cluster runs, as `(first cluster, cluster count)`.
    pub allocated: Vec<(u64, u64)>,
}

impl SyntheticNtfs {
    /// Describes a volume filling `partition_bytes`, with nothing allocated.
    pub fn new(partition_bytes: u64, bytes_per_sector: u16, sectors_per_cluster: u32) -> Self {
        let sector = u64::from(bytes_per_sector);
        assert!(
            partition_bytes % sector == 0,
            "partition must be whole sectors"
        );
        let partition_sectors = partition_bytes / sector;
        assert!(partition_sectors > 1, "partition is too small for a volume");

        // NTFS counts every sector of the partition except the last, which is
        // where it keeps the spare copy of the boot sector.
        let total_sectors = partition_sectors - 1;

        let clusters = total_sectors / u64::from(sectors_per_cluster);
        assert!(clusters > 32, "volume is too small to be interesting");

        Self {
            partition_bytes,
            bytes_per_sector,
            sectors_per_cluster,
            total_sectors,
            mft_cluster: clusters / 3,
            mft_mirror_cluster: 2,
            allocated: Vec::new(),
        }
    }

    /// A volume with the front metadata and the master file table in use, which
    /// is roughly what a freshly formatted volume looks like.
    pub fn formatted(
        partition_bytes: u64,
        bytes_per_sector: u16,
        sectors_per_cluster: u32,
    ) -> Self {
        let mut volume = Self::new(partition_bytes, bytes_per_sector, sectors_per_cluster);
        volume.allocate(0, 16);
        let mft = volume.mft_cluster;
        volume.allocate(mft, 16);
        volume
    }

    /// Bytes in one cluster.
    pub fn cluster_size(&self) -> u64 {
        u64::from(self.bytes_per_sector) * u64::from(self.sectors_per_cluster)
    }

    /// Clusters the filesystem describes.
    pub fn cluster_count(&self) -> u64 {
        self.total_sectors / u64::from(self.sectors_per_cluster)
    }

    /// Marks `count` clusters from `start` as in use.
    pub fn allocate(&mut self, start: u64, count: u64) {
        assert!(
            count > 0 && start + count <= self.cluster_count(),
            "run {start}+{count} does not fit in {} clusters",
            self.cluster_count()
        );
        self.allocated.push((start, count));
    }

    /// Whether a cluster is marked in use.
    pub fn is_allocated(&self, cluster: u64) -> bool {
        self.allocated
            .iter()
            .any(|(start, count)| cluster >= *start && cluster < start + count)
    }

    /// The boot sector this volume declares.
    pub fn boot_sector_bytes(&self) -> Vec<u8> {
        let mut s = vec![0u8; self.bytes_per_sector as usize];
        s[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
        s[3..11].copy_from_slice(NTFS_OEM_ID);
        s[11..13].copy_from_slice(&self.bytes_per_sector.to_le_bytes());
        s[13] = u8::try_from(self.sectors_per_cluster).expect("small cluster factor");
        s[40..48].copy_from_slice(&self.total_sectors.to_le_bytes());
        s[48..56].copy_from_slice(&self.mft_cluster.to_le_bytes());
        s[56..64].copy_from_slice(&self.mft_mirror_cluster.to_le_bytes());
        s[64] = 0xF6; // -10, meaning 1024 byte file records
        s[72..80].copy_from_slice(&0x0BAD_C0DE_DEAD_BEEFu64.to_le_bytes());
        // The signature sits at 510 and 511 whatever the sector size is, which
        // is where every reader looks for it.
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    /// The parsed boot sector, as the capture path would read it.
    pub fn boot(&self) -> NtfsBootSector {
        NtfsBootSector::parse(&self.boot_sector_bytes()).expect("a valid synthetic boot sector")
    }

    /// The allocated runs, merged so that none overlaps or touches another.
    ///
    /// A real bitmap cannot produce an overlap, so the merge happens here
    /// rather than being something the code under test has to tolerate. It also
    /// means a test can mark the same region twice without having to track what
    /// it has already marked.
    pub fn merged_runs(&self) -> Vec<(u64, u64)> {
        let mut sorted = self.allocated.clone();
        sorted.sort_unstable();
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(sorted.len());
        for (start, count) in sorted {
            match out.last_mut() {
                Some((last_start, last_count)) if start <= *last_start + *last_count => {
                    let end = (start + count).max(*last_start + *last_count);
                    *last_count = end - *last_start;
                }
                _ => out.push((start, count)),
            }
        }
        out
    }

    /// The allocation, as `FSCTL_GET_VOLUME_BITMAP` would report it.
    pub fn allocation(&self) -> Allocation {
        Allocation::from_runs(self.cluster_count(), self.merged_runs())
            .expect("valid synthetic cluster runs")
    }

    /// The bitmap pages the volume would return, ready to be fed to a scan.
    ///
    /// `clusters_per_page` controls how many pages there are, so pagination can
    /// be exercised without needing a volume large enough to force it.
    pub fn bitmap_pages(&self, clusters_per_page: u64) -> Vec<Vec<u8>> {
        assert!(clusters_per_page % 8 == 0, "a page must be whole bytes");
        let total = self.cluster_count();
        let mut pages = Vec::new();
        let mut lcn = 0u64;
        while lcn < total {
            let count = clusters_per_page.min(total - lcn);
            let mut bits = vec![0u8; (count as usize).div_ceil(8)];
            for i in 0..count {
                if self.is_allocated(lcn + i) {
                    bits[(i / 8) as usize] |= 1u8 << (i % 8);
                }
            }
            let mut page = Vec::with_capacity(16 + bits.len());
            page.extend_from_slice(&lcn.to_le_bytes());
            page.extend_from_slice(&(total - lcn).to_le_bytes());
            page.extend_from_slice(&bits);
            pages.push(page);
            lcn += count;
        }
        pages
    }

    /// The byte a given offset inside an allocated cluster should hold.
    ///
    /// Derived from the offset, so a restored volume can be checked against what
    /// it should contain without keeping a copy of it.
    pub fn expected_byte(offset: u64) -> u8 {
        // An odd multiplier so neighbouring bytes differ and compression cannot
        // collapse the whole volume into one chunk.
        ((offset.wrapping_mul(31).wrapping_add(offset / 997)) % 251) as u8
    }

    /// Writes the volume into a buffer the size of its partition.
    ///
    /// Allocated clusters get a recognisable pattern, free ones get
    /// [`FREE_SPACE_FILL`], and both copies of the boot sector are written.
    pub fn write_into(&self, partition: &mut [u8]) {
        assert_eq!(
            partition.len() as u64,
            self.partition_bytes,
            "buffer is not the size of the partition"
        );

        partition.fill(FREE_SPACE_FILL);

        let cluster_size = self.cluster_size();
        for (start, count) in &self.merged_runs() {
            let from = (start * cluster_size) as usize;
            let to = ((start + count) * cluster_size) as usize;
            for (i, b) in partition[from..to].iter_mut().enumerate() {
                *b = Self::expected_byte(from as u64 + i as u64);
            }
        }

        let boot = self.boot_sector_bytes();
        partition[..boot.len()].copy_from_slice(&boot);

        // NTFS keeps a copy in the last sector of the partition, outside the
        // space the filesystem counts as its own.
        let spare_at = partition.len() - boot.len();
        partition[spare_at..].copy_from_slice(&boot);
    }

    /// The bytes of the whole partition, laid out.
    pub fn to_partition_bytes(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.partition_bytes as usize];
        self.write_into(&mut out);
        out
    }
}
