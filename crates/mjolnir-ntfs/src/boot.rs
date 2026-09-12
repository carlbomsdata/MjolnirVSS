//! The NTFS boot sector, and what can be proved from it.
//!
//! This is the first structure of an NTFS volume and the root of everything
//! else: it says how big a cluster is and where the master file table starts.
//! MjolnirVSS parses it for three separate reasons.
//!
//! The first is used block imaging, which needs the cluster size to turn the
//! allocation bitmap into byte ranges.
//!
//! The second is file recovery, which needs the master file table.
//!
//! The third is proof. A volume read through a shadow copy is supposed to come
//! back as ordinary NTFS even when the underlying disk is encrypted. Checking
//! that the first sector *says* `NTFS` is weak evidence, because eight bytes
//! can look like anything. Checking that the boot sector is internally
//! consistent, and that the master file table really is where it claims to be
//! and really does begin with a file record, is strong evidence: ciphertext
//! does not accidentally satisfy all of that at once.

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::math;

/// The signature an NTFS boot sector carries at offset 3.
pub const NTFS_OEM_ID: &[u8; 8] = b"NTFS    ";

/// The signature a BitLocker protected volume carries in the same place.
///
/// A volume showing this is encrypted at rest. Seeing it is how MjolnirVSS
/// knows the difference between a plain NTFS volume and a BitLocker one without
/// touching any key material.
pub const BITLOCKER_OEM_ID: &[u8; 8] = b"-FVE-FS-";

/// The signature every NTFS file record begins with.
pub const FILE_RECORD_SIGNATURE: &[u8; 4] = b"FILE";

/// Sector sizes an NTFS volume may declare.
const VALID_SECTOR_SIZES: [u16; 4] = [512, 1024, 2048, 4096];

/// What the first sector of a volume turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeSignature {
    /// An NTFS boot sector.
    Ntfs,
    /// A BitLocker volume header. The filesystem inside is not visible here.
    BitLocker,
    /// A FAT volume, which is what an EFI system partition holds.
    Fat,
    /// Something else, or nothing recognisable.
    Unknown,
}

impl VolumeSignature {
    /// Reads the signature from the first sector of a volume.
    pub fn of(sector: &[u8]) -> Self {
        if sector.len() < 11 {
            return VolumeSignature::Unknown;
        }
        let oem = &sector[3..11];
        if oem == NTFS_OEM_ID {
            return VolumeSignature::Ntfs;
        }
        if oem == BITLOCKER_OEM_ID {
            return VolumeSignature::BitLocker;
        }
        // FAT puts its type string in one of two places depending on whether it
        // is FAT12/16 or FAT32.
        if sector.len() >= 90 {
            let fat16 = &sector[54..62];
            let fat32 = &sector[82..90];
            if fat16.starts_with(b"FAT") || fat32.starts_with(b"FAT") {
                return VolumeSignature::Fat;
            }
        }
        VolumeSignature::Unknown
    }

    /// A short description for the operator and the log.
    pub const fn describe(self) -> &'static str {
        match self {
            VolumeSignature::Ntfs => "NTFS",
            VolumeSignature::BitLocker => "BitLocker encrypted",
            VolumeSignature::Fat => "FAT",
            VolumeSignature::Unknown => "unrecognised",
        }
    }
}

/// A parsed NTFS boot sector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NtfsBootSector {
    /// Bytes per sector, as the volume declares.
    pub bytes_per_sector: u16,
    /// Sectors per cluster.
    pub sectors_per_cluster: u32,
    /// Total sectors in the volume.
    pub total_sectors: u64,
    /// Cluster number where the master file table starts.
    pub mft_cluster: u64,
    /// Cluster number where the mirror of the master file table starts.
    pub mft_mirror_cluster: u64,
    /// Size of one file record, in bytes.
    pub bytes_per_file_record: u32,
    /// The volume serial number.
    pub serial: u64,
}

impl NtfsBootSector {
    /// Parses and checks an NTFS boot sector.
    ///
    /// Every field is validated against the others, because a plausible looking
    /// number in isolation proves nothing. A structure that passes all of this
    /// is very unlikely to be anything but a real NTFS boot sector.
    pub fn parse(sector: &[u8]) -> Result<Self> {
        if sector.len() < 512 {
            return Err(bad("the boot sector is shorter than 512 bytes"));
        }
        if &sector[3..11] != NTFS_OEM_ID {
            return Err(bad(&format!(
                "the volume is {} rather than NTFS",
                VolumeSignature::of(sector).describe()
            )));
        }

        let bytes_per_sector = u16::from_le_bytes([sector[11], sector[12]]);
        if !VALID_SECTOR_SIZES.contains(&bytes_per_sector) {
            return Err(bad(&format!(
                "it declares {bytes_per_sector} bytes per sector, which is not a size NTFS uses"
            )));
        }

        // NTFS stores this as a signed byte: a small positive number is the
        // count directly, and a negative number is a power of two shift. The
        // second form appears on volumes with very large clusters.
        let raw_spc = sector[13] as i8;
        let sectors_per_cluster: u32 = if raw_spc > 0 {
            raw_spc as u32
        } else {
            let shift = (-raw_spc) as u32;
            if shift > 20 {
                return Err(bad("it declares an implausible cluster size"));
            }
            1u32 << shift
        };
        if !sectors_per_cluster.is_power_of_two() {
            return Err(bad(&format!(
                "it declares {sectors_per_cluster} sectors per cluster, which is not a power of two"
            )));
        }

        let total_sectors = read_u64(sector, 40);
        if total_sectors == 0 {
            return Err(bad("it declares a volume of zero sectors"));
        }

        let mft_cluster = read_u64(sector, 48);
        let mft_mirror_cluster = read_u64(sector, 56);

        // Both copies of the master file table have to be inside the volume.
        let total_clusters = total_sectors / u64::from(sectors_per_cluster);
        if mft_cluster == 0 || mft_cluster >= total_clusters {
            return Err(bad(&format!(
                "it places the master file table at cluster {mft_cluster}, outside the {total_clusters} clusters it claims to have"
            )));
        }
        if mft_mirror_cluster >= total_clusters {
            return Err(bad(
                "it places the mirror of the master file table outside the volume",
            ));
        }

        // Like the cluster count, this is a count or a negative power of two.
        let raw_record = sector[64] as i8;
        let bytes_per_file_record: u32 = if raw_record > 0 {
            math::mul_u64(
                "file record size",
                u64::from(raw_record as u32),
                u64::from(sectors_per_cluster) * u64::from(bytes_per_sector),
            )
            .and_then(|v| math::to_u32("file record size", v))
            .map_err(|_| bad("it declares an implausible file record size"))?
        } else {
            let shift = (-raw_record) as u32;
            if shift > 20 {
                return Err(bad("it declares an implausible file record size"));
            }
            1u32 << shift
        };
        if !(256..=65536).contains(&bytes_per_file_record)
            || !bytes_per_file_record.is_power_of_two()
        {
            return Err(bad(&format!(
                "it declares a file record size of {bytes_per_file_record} bytes"
            )));
        }

        // The last two bytes of a boot sector are always this.
        if sector[510] != 0x55 || sector[511] != 0xAA {
            return Err(bad("the boot sector is missing its end signature"));
        }

        Ok(Self {
            bytes_per_sector,
            sectors_per_cluster,
            total_sectors,
            mft_cluster,
            mft_mirror_cluster,
            bytes_per_file_record,
            serial: read_u64(sector, 72),
        })
    }

    /// Bytes in one cluster.
    pub fn bytes_per_cluster(&self) -> u64 {
        u64::from(self.sectors_per_cluster) * u64::from(self.bytes_per_sector)
    }

    /// Total size of the volume in bytes, as the boot sector declares.
    pub fn volume_bytes(&self) -> Result<u64> {
        Ok(math::mul_u64(
            "ntfs volume size",
            self.total_sectors,
            u64::from(self.bytes_per_sector),
        )?)
    }

    /// Byte offset of the master file table from the start of the volume.
    pub fn mft_offset(&self) -> Result<u64> {
        Ok(math::mul_u64(
            "master file table offset",
            self.mft_cluster,
            self.bytes_per_cluster(),
        )?)
    }

    /// Byte offset of the mirror of the master file table.
    pub fn mft_mirror_offset(&self) -> Result<u64> {
        Ok(math::mul_u64(
            "master file table mirror offset",
            self.mft_mirror_cluster,
            self.bytes_per_cluster(),
        )?)
    }

    /// Total clusters in the volume.
    pub fn total_clusters(&self) -> u64 {
        self.total_sectors / u64::from(self.sectors_per_cluster)
    }
}

/// Whether a buffer begins with an NTFS file record.
///
/// The master file table is a sequence of these. Finding one at the offset the
/// boot sector points at is what turns "this looks like NTFS" into "this is
/// NTFS", because it requires two independent structures to agree.
pub fn looks_like_file_record(buffer: &[u8]) -> bool {
    buffer.len() >= 4 && &buffer[0..4] == FILE_RECORD_SIGNATURE
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(b)
}

fn bad(why: &str) -> Error {
    Error::new(
        ExitCode::Unsupported,
        "the volume does not have a usable NTFS boot sector",
        why.to_owned(),
        "this volume cannot be captured by used block imaging; MjolnirVSS falls back to copying every byte",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a boot sector describing a plausible NTFS volume.
    fn boot_sector(sectors_per_cluster: i8, total_sectors: u64, mft_cluster: u64) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]); // jump instruction
        s[3..11].copy_from_slice(NTFS_OEM_ID);
        s[11..13].copy_from_slice(&512u16.to_le_bytes());
        s[13] = sectors_per_cluster as u8;
        s[40..48].copy_from_slice(&total_sectors.to_le_bytes());
        s[48..56].copy_from_slice(&mft_cluster.to_le_bytes());
        s[56..64].copy_from_slice(&(mft_cluster / 2).to_le_bytes());
        s[64] = 0xF6; // -10, meaning 1024 byte records
        s[72..80].copy_from_slice(&0x1234_5678_9ABC_DEF0u64.to_le_bytes());
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    #[test]
    fn a_normal_boot_sector_parses() {
        // 16 million sectors at 8 per cluster is 2 million clusters, which is
        // room for the master file table to sit where Windows usually puts it.
        let s = boot_sector(8, 16_000_000, 786_432);
        let boot = NtfsBootSector::parse(&s).expect("should parse");

        assert_eq!(boot.bytes_per_sector, 512);
        assert_eq!(boot.sectors_per_cluster, 8);
        assert_eq!(boot.bytes_per_cluster(), 4096);
        assert_eq!(boot.total_sectors, 16_000_000);
        assert_eq!(boot.bytes_per_file_record, 1024);
        assert_eq!(boot.mft_offset().unwrap(), 786_432 * 4096);
        assert_eq!(boot.volume_bytes().unwrap(), 16_000_000 * 512);
    }

    #[test]
    fn the_negative_form_of_the_cluster_size_is_understood() {
        // -4 means 2^4 = 16 sectors per cluster.
        let s = boot_sector(-4, 4_000_000, 100_000);
        let boot = NtfsBootSector::parse(&s).unwrap();
        assert_eq!(boot.sectors_per_cluster, 16);
        assert_eq!(boot.bytes_per_cluster(), 8192);
    }

    #[test]
    fn a_bitlocker_volume_is_recognised_and_refused_as_ntfs() {
        let mut s = boot_sector(8, 4_000_000, 100_000);
        s[3..11].copy_from_slice(BITLOCKER_OEM_ID);

        assert_eq!(VolumeSignature::of(&s), VolumeSignature::BitLocker);
        let err = NtfsBootSector::parse(&s).unwrap_err();
        assert!(err.why().contains("BitLocker"), "{}", err.why());
    }

    #[test]
    fn a_fat_volume_is_recognised() {
        let mut s = vec![0u8; 512];
        s[3..11].copy_from_slice(b"MSDOS5.0");
        s[82..90].copy_from_slice(b"FAT32   ");
        assert_eq!(VolumeSignature::of(&s), VolumeSignature::Fat);
    }

    #[test]
    fn ciphertext_does_not_parse_as_a_boot_sector() {
        // A sector of pseudo random bytes, which is what an encrypted volume
        // looks like from outside. It must be refused, not misread.
        let mut s = vec![0u8; 512];
        for (i, b) in s.iter_mut().enumerate() {
            *b = ((i * 37 + 11) % 251) as u8;
        }
        assert!(NtfsBootSector::parse(&s).is_err());
        assert_eq!(VolumeSignature::of(&s), VolumeSignature::Unknown);
    }

    #[test]
    fn an_implausible_sector_size_is_refused() {
        let mut s = boot_sector(8, 4_000_000, 100_000);
        s[11..13].copy_from_slice(&777u16.to_le_bytes());
        assert!(NtfsBootSector::parse(&s).unwrap_err().why().contains("777"));
    }

    #[test]
    fn a_master_file_table_outside_the_volume_is_refused() {
        // 4_000_000 sectors at 8 per cluster is 500_000 clusters; asking for
        // the table at cluster 900_000 is outside it.
        let s = boot_sector(8, 4_000_000, 900_000);
        let err = NtfsBootSector::parse(&s).unwrap_err();
        assert!(err.why().contains("outside"), "{}", err.why());
    }

    #[test]
    fn a_zero_length_volume_is_refused() {
        let s = boot_sector(8, 0, 100);
        assert!(NtfsBootSector::parse(&s).is_err());
    }

    #[test]
    fn a_missing_end_signature_is_refused() {
        let mut s = boot_sector(8, 4_000_000, 100_000);
        s[510] = 0;
        s[511] = 0;
        let err = NtfsBootSector::parse(&s).unwrap_err();
        assert!(err.why().contains("end signature"));
    }

    #[test]
    fn a_truncated_sector_is_refused_rather_than_indexing_out_of_bounds() {
        for len in [0usize, 3, 11, 100, 511] {
            let s = vec![0u8; len];
            assert!(NtfsBootSector::parse(&s).is_err(), "length {len}");
            // The signature check must also survive a short buffer.
            let _ = VolumeSignature::of(&s);
        }
    }

    #[test]
    fn a_file_record_is_recognised() {
        let mut record = vec![0u8; 1024];
        record[0..4].copy_from_slice(FILE_RECORD_SIGNATURE);
        assert!(looks_like_file_record(&record));

        assert!(!looks_like_file_record(&[0u8; 1024]));
        assert!(!looks_like_file_record(b"FIL"));
        assert!(!looks_like_file_record(b""));
    }

    #[test]
    fn cluster_counts_are_consistent() {
        let s = boot_sector(8, 4_000_000, 100_000);
        let boot = NtfsBootSector::parse(&s).unwrap();
        assert_eq!(boot.total_clusters(), 500_000);
        assert_eq!(
            boot.total_clusters() * boot.bytes_per_cluster(),
            boot.volume_bytes().unwrap()
        );
    }
}
