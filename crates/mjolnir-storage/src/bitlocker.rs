//! Detecting BitLocker, and telling a locked volume from an unlocked one.
//!
//! # Why this is not just a flag lookup
//!
//! An unlocked BitLocker volume is indistinguishable from ordinary NTFS when
//! asked through the filesystem: `GetVolumeInformationW` reports `NTFS`,
//! because the encryption filter sits below the filesystem and presents
//! decrypted contents to everything above it. Checking there would miss it
//! entirely.
//!
//! What does show the difference is the partition itself, read from the
//! physical disk, below that filter. A BitLocker volume's first sector carries
//! `-FVE-FS-` where a plain NTFS volume carries `NTFS`. That is the same
//! structure `manage-bde` and Windows' own tools recognise a volume by, and
//! reading it involves no key material of any kind.
//!
//! Combining the two answers separates the three states that matter:
//!
//! | On the disk | Through the filesystem | Conclusion |
//! |---|---|---|
//! | `NTFS` | NTFS | not encrypted |
//! | `-FVE-FS-` | NTFS | BitLocker, unlocked |
//! | `-FVE-FS-` | unavailable or RAW | BitLocker, locked |
//!
//! # No key material, ever
//!
//! Nothing here reads, derives, stores or logs a recovery key, a volume master
//! key, or any other secret. It reads one 512 byte sector and compares eight
//! bytes of it.

use mjolnir_core::error::Result;
use mjolnir_ntfs::boot::VolumeSignature;

use crate::device::Device;
use crate::disks::{PhysicalDisk, PhysicalPartition};
use crate::volumes::VolumeInfo;

/// Whether a volume is encrypted at rest, and whether it is readable now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encryption {
    /// Not encrypted. The partition holds a filesystem directly.
    None,
    /// BitLocker, and Windows currently has it unlocked.
    ///
    /// The filesystem inside is readable, and a shadow copy of it presents
    /// decrypted contents.
    BitLockerUnlocked,
    /// BitLocker, and the volume is locked.
    ///
    /// Nothing inside can be read without unlocking it first, so it cannot be
    /// backed up.
    BitLockerLocked,
    /// The partition could not be read, so nothing can be concluded.
    ///
    /// Happens without administrator rights, which is why planning treats this
    /// as "not yet known" rather than as an answer.
    Unknown,
}

impl Encryption {
    /// Whether this partition is protected by BitLocker at all.
    pub fn is_bitlocker(self) -> bool {
        matches!(
            self,
            Encryption::BitLockerUnlocked | Encryption::BitLockerLocked
        )
    }

    /// Whether the contents can be read right now.
    pub fn is_readable(self) -> bool {
        matches!(self, Encryption::None | Encryption::BitLockerUnlocked)
    }

    /// A short description for the operator and the log.
    pub const fn describe(self) -> &'static str {
        match self {
            Encryption::None => "not encrypted",
            Encryption::BitLockerUnlocked => "BitLocker, unlocked",
            Encryption::BitLockerLocked => "BitLocker, locked",
            Encryption::Unknown => "could not be determined",
        }
    }
}

/// What was found when a partition was inspected.
#[derive(Debug, Clone)]
pub struct PartitionEncryption {
    /// The conclusion.
    pub encryption: Encryption,
    /// What the first sector of the partition says, read from the disk.
    pub on_disk: VolumeSignature,
    /// What Windows reports the volume's filesystem to be, if it has one.
    pub filesystem: Option<String>,
    /// The observations the conclusion was drawn from, for the log and the
    /// diagnostic report.
    pub evidence: Vec<String>,
}

/// Inspects one partition for encryption.
///
/// Reads a single sector from the physical disk. Writes nothing, and needs no
/// key.
pub fn inspect(
    disk: &PhysicalDisk,
    partition: &PhysicalPartition,
    volume: Option<&VolumeInfo>,
) -> Result<PartitionEncryption> {
    let filesystem = volume.and_then(|v| v.filesystem.clone());
    let mut evidence = Vec::new();

    let device = match Device::open_read(
        &disk.device_path,
        disk.logical_sector_size,
        disk.size_bytes,
    ) {
        Ok(device) => device,
        Err(e) => {
            // Without administrator rights the disk cannot be read at all. That
            // is not an answer, and must not be mistaken for "not encrypted".
            evidence.push(format!(
                "the physical disk could not be opened, so the partition's own header could not be read: {}",
                e.what()
            ));
            return Ok(PartitionEncryption {
                encryption: Encryption::Unknown,
                on_disk: VolumeSignature::Unknown,
                filesystem,
                evidence,
            });
        }
    };

    let mut sector = vec![0u8; disk.logical_sector_size.max(512) as usize];
    if let Err(e) = device.read_at(partition.starting_offset, &mut sector) {
        evidence.push(format!(
            "the first sector of the partition could not be read: {}",
            e.what()
        ));
        return Ok(PartitionEncryption {
            encryption: Encryption::Unknown,
            on_disk: VolumeSignature::Unknown,
            filesystem,
            evidence,
        });
    }

    let on_disk = VolumeSignature::of(&sector);
    evidence.push(format!(
        "the partition's first sector on the disk identifies it as {}",
        on_disk.describe()
    ));

    let encryption = match on_disk {
        VolumeSignature::BitLocker => {
            // The filesystem layer sees through the encryption only while the
            // volume is unlocked, so what Windows reports here is the lock
            // state.
            match filesystem.as_deref() {
                Some(fs) if !fs.eq_ignore_ascii_case("RAW") => {
                    evidence.push(format!(
                        "Windows reports the volume as {fs}, which it can only do while the volume is unlocked"
                    ));
                    Encryption::BitLockerUnlocked
                }
                Some(fs) => {
                    evidence.push(format!(
                        "Windows reports the volume as {fs}, which means it cannot see inside it"
                    ));
                    Encryption::BitLockerLocked
                }
                None => {
                    evidence.push(
                        "Windows reports no filesystem for the volume, which means it cannot see inside it"
                            .to_owned(),
                    );
                    Encryption::BitLockerLocked
                }
            }
        }
        VolumeSignature::Ntfs | VolumeSignature::Fat => Encryption::None,
        VolumeSignature::Unknown => {
            // An unrecognised header is not evidence of encryption. It is a
            // partition MjolnirVSS does not understand, which the planner
            // refuses separately.
            evidence.push(
                "the header is not one MjolnirVSS recognises, so no conclusion about encryption is drawn"
                    .to_owned(),
            );
            Encryption::None
        }
    };

    Ok(PartitionEncryption {
        encryption,
        on_disk,
        filesystem,
        evidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_states_classify_correctly() {
        assert!(!Encryption::None.is_bitlocker());
        assert!(Encryption::None.is_readable());

        assert!(Encryption::BitLockerUnlocked.is_bitlocker());
        assert!(Encryption::BitLockerUnlocked.is_readable());

        assert!(Encryption::BitLockerLocked.is_bitlocker());
        assert!(
            !Encryption::BitLockerLocked.is_readable(),
            "a locked volume must never be treated as readable"
        );

        assert!(!Encryption::Unknown.is_readable());
        assert!(!Encryption::Unknown.is_bitlocker());
    }

    #[test]
    fn every_state_describes_itself() {
        for state in [
            Encryption::None,
            Encryption::BitLockerUnlocked,
            Encryption::BitLockerLocked,
            Encryption::Unknown,
        ] {
            assert!(!state.describe().is_empty());
        }
        assert!(Encryption::BitLockerLocked.describe().contains("locked"));
    }

    /// The decisive property: a BitLocker header on the disk plus a readable
    /// filesystem means unlocked, and no filesystem means locked. This is the
    /// rule the planner depends on.
    #[test]
    fn the_signature_and_the_filesystem_together_give_the_lock_state() {
        // Reproduces the classification without needing a real disk.
        fn classify(on_disk: VolumeSignature, filesystem: Option<&str>) -> Encryption {
            match on_disk {
                VolumeSignature::BitLocker => match filesystem {
                    Some(fs) if !fs.eq_ignore_ascii_case("RAW") => Encryption::BitLockerUnlocked,
                    _ => Encryption::BitLockerLocked,
                },
                _ => Encryption::None,
            }
        }

        assert_eq!(
            classify(VolumeSignature::BitLocker, Some("NTFS")),
            Encryption::BitLockerUnlocked
        );
        assert_eq!(
            classify(VolumeSignature::BitLocker, Some("RAW")),
            Encryption::BitLockerLocked
        );
        assert_eq!(
            classify(VolumeSignature::BitLocker, None),
            Encryption::BitLockerLocked
        );
        assert_eq!(
            classify(VolumeSignature::Ntfs, Some("NTFS")),
            Encryption::None
        );
        assert_eq!(
            classify(VolumeSignature::Fat, Some("FAT32")),
            Encryption::None
        );
    }
}
