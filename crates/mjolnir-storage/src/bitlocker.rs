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
//! Two independent sources are used instead, and both are recorded.
//!
//! **The documented one.** `Win32_EncryptableVolume` is the interface Microsoft
//! publishes for this question. Its `ProtectionStatus` says whether BitLocker is
//! on, and a value of 2, "unknown", is documented as being caused by a locked
//! volume. Its `ConversionStatus` says how much of the volume is encrypted, and
//! cannot be read at all for a locked volume. This is the primary source. It
//! needs administrator rights, and the namespace is absent on Windows editions
//! without BitLocker and inside Windows PE.
//!
//! **The corroborating one.** The partition's own first sector, read from the
//! physical disk below the encryption filter, carries `-FVE-FS-` where a plain
//! NTFS volume carries `NTFS`. This is the structure the open source `libbde`
//! project documented by reverse engineering; **Microsoft does not publish it**,
//! so it is used to corroborate and as a fallback where WMI cannot answer, never
//! as the sole basis for calling a volume unencrypted.
//!
//! Together they separate the three states that matter:
//!
//! | `Win32_EncryptableVolume` | On the disk | Through the filesystem | Conclusion |
//! |---|---|---|---|
//! | absent or unprotected | `NTFS` | NTFS | not encrypted |
//! | protected, readable | `-FVE-FS-` | NTFS | BitLocker, unlocked |
//! | protection status unknown | `-FVE-FS-` | unavailable or RAW | BitLocker, locked |
//!
//! # No key material, ever
//!
//! Nothing here reads, derives, stores or logs a recovery key, a volume master
//! key, or any other secret. It reads two status numbers and eight bytes of one
//! sector. None of the `Win32_EncryptableVolume` methods that handle key
//! material is called, and BitLocker's state is never changed.

use mjolnir_core::error::Result;
use mjolnir_ntfs::boot::VolumeSignature;

use crate::device::Device;
use crate::disks::{PhysicalDisk, PhysicalPartition};
use crate::volumes::VolumeInfo;
use crate::wmi::EncryptableVolume;

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
    /// What `Win32_EncryptableVolume` said, when it could be asked.
    pub reported: Option<EncryptableVolume>,
    /// The observations the conclusion was drawn from, for the log and the
    /// diagnostic report.
    pub evidence: Vec<String>,
}

/// Everything `classify` needs, gathered from the machine.
///
/// Separating this from the reading makes the decision itself a pure function,
/// so the table above can be tested exhaustively without a disk.
#[derive(Debug, Clone)]
struct Observations<'a> {
    on_disk: VolumeSignature,
    filesystem: Option<&'a str>,
    reported: Option<&'a EncryptableVolume>,
}

/// Draws the conclusion, and says why.
///
/// The documented source wins where it has an answer. The signature is the
/// fallback, and a disagreement between the two is recorded rather than
/// silently resolved.
fn classify(observed: &Observations<'_>) -> (Encryption, Vec<String>) {
    let mut evidence = Vec::new();

    let signature_says_bitlocker = observed.on_disk == VolumeSignature::BitLocker;
    evidence.push(format!(
        "the partition's first sector on the disk identifies it as {}",
        observed.on_disk.describe()
    ));

    // The filesystem layer can see inside a BitLocker volume only while it is
    // unlocked, so what Windows reports here is a lock state rather than a
    // filesystem.
    let filesystem_is_readable = observed
        .filesystem
        .is_some_and(|fs| !fs.eq_ignore_ascii_case("RAW"));

    if let Some(reported) = observed.reported {
        evidence.push(format!(
            "Windows reports this volume as {} and {}",
            if reported.is_protected() {
                "BitLocker protected"
            } else {
                "not BitLocker protected"
            },
            reported.describe_conversion()
        ));

        if reported.is_locked() {
            evidence.push(
                "a volume in that state is locked, and nothing can read what is inside it"
                    .to_owned(),
            );
            return (Encryption::BitLockerLocked, evidence);
        }

        if reported.has_encryption() {
            if !signature_says_bitlocker {
                // Mid-decryption a volume can be reported as encrypted while its
                // first sector has already been rewritten, so this is a real
                // state rather than a contradiction. It is recorded either way.
                evidence.push(
                    "the partition header does not carry a BitLocker signature, which happens while a volume is being decrypted"
                        .to_owned(),
                );
            }
            if !filesystem_is_readable {
                evidence.push(
                    "Windows reports no readable filesystem for the volume, so it is treated as locked"
                        .to_owned(),
                );
                return (Encryption::BitLockerLocked, evidence);
            }
            return (Encryption::BitLockerUnlocked, evidence);
        }

        if signature_says_bitlocker {
            // Windows says no encryption, the disk says otherwise. Believing the
            // "no" would mean capturing a volume without saying it holds
            // decrypted data, so the cautious answer is taken.
            evidence.push(
                "the partition header still carries a BitLocker signature, so the volume is treated as encrypted at rest"
                    .to_owned(),
            );
            return (
                if filesystem_is_readable {
                    Encryption::BitLockerUnlocked
                } else {
                    Encryption::BitLockerLocked
                },
                evidence,
            );
        }

        return (Encryption::None, evidence);
    }

    evidence.push(
        "Windows did not report this volume as encryptable, so only the partition header is available"
            .to_owned(),
    );

    match observed.on_disk {
        VolumeSignature::BitLocker if filesystem_is_readable => {
            evidence.push(
                "Windows reports a readable filesystem, which it can only do while the volume is unlocked"
                    .to_owned(),
            );
            (Encryption::BitLockerUnlocked, evidence)
        }
        VolumeSignature::BitLocker => {
            evidence.push(
                "Windows reports no readable filesystem, which means it cannot see inside the volume"
                    .to_owned(),
            );
            (Encryption::BitLockerLocked, evidence)
        }
        VolumeSignature::Ntfs | VolumeSignature::Fat => (Encryption::None, evidence),
        VolumeSignature::Unknown => {
            // An unrecognised header is not evidence of encryption. It is a
            // partition MjolnirVSS does not understand, which the planner
            // refuses separately.
            evidence.push(
                "the header is not one MjolnirVSS recognises, so no conclusion about encryption is drawn"
                    .to_owned(),
            );
            (Encryption::None, evidence)
        }
    }
}

/// Inspects one partition for encryption.
///
/// Queries `Win32_EncryptableVolume` and reads a single sector from the physical
/// disk. Writes nothing, changes nothing, and needs no key.
///
/// Callers inspecting several partitions should ask
/// [`crate::wmi::encryptable_volumes`] once and use [`inspect_with`], because
/// each WMI query connects to the service afresh.
pub fn inspect(
    disk: &PhysicalDisk,
    partition: &PhysicalPartition,
    volume: Option<&VolumeInfo>,
) -> Result<PartitionEncryption> {
    let reported = crate::wmi::encryptable_volumes().unwrap_or_default();
    inspect_with(disk, partition, volume, &reported)
}

/// Inspects one partition against an already gathered list of volumes.
pub fn inspect_with(
    disk: &PhysicalDisk,
    partition: &PhysicalPartition,
    volume: Option<&VolumeInfo>,
    reported: &[EncryptableVolume],
) -> Result<PartitionEncryption> {
    let filesystem = volume.and_then(|v| v.filesystem.clone());

    let reported = volume.and_then(|v| {
        let wanted = v.guid_path.trim_end_matches('\\').to_ascii_lowercase();
        reported
            .iter()
            .find(|r| r.device_id.trim_end_matches('\\').to_ascii_lowercase() == wanted)
            .cloned()
    });

    let device = match Device::open_read(
        &disk.device_path,
        disk.logical_sector_size,
        disk.size_bytes,
    ) {
        Ok(device) => device,
        Err(e) => {
            // Without administrator rights the disk cannot be read at all. That
            // is not an answer, and must not be mistaken for "not encrypted".
            // The WMI query needs the same rights, so it will not have answered
            // either.
            return Ok(PartitionEncryption {
                encryption: Encryption::Unknown,
                on_disk: VolumeSignature::Unknown,
                filesystem,
                reported,
                evidence: vec![format!(
                    "the physical disk could not be opened, so the partition's own header could not be read: {}",
                    e.what()
                )],
            });
        }
    };

    let mut sector = vec![0u8; disk.logical_sector_size.max(512) as usize];
    if let Err(e) = device.read_at(partition.starting_offset, &mut sector) {
        return Ok(PartitionEncryption {
            encryption: Encryption::Unknown,
            on_disk: VolumeSignature::Unknown,
            filesystem,
            reported,
            evidence: vec![format!(
                "the first sector of the partition could not be read: {}",
                e.what()
            )],
        });
    }

    let on_disk = VolumeSignature::of(&sector);
    let (encryption, evidence) = classify(&Observations {
        on_disk,
        filesystem: filesystem.as_deref(),
        reported: reported.as_ref(),
    });

    Ok(PartitionEncryption {
        encryption,
        on_disk,
        filesystem,
        reported,
        evidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reported(protection: u32, conversion: Option<u32>) -> EncryptableVolume {
        EncryptableVolume {
            device_id: "\\\\?\\Volume{test}\\".to_owned(),
            protection_status: protection,
            conversion_status: conversion,
        }
    }

    fn conclude(
        on_disk: VolumeSignature,
        filesystem: Option<&str>,
        reported: Option<&EncryptableVolume>,
    ) -> Encryption {
        classify(&Observations {
            on_disk,
            filesystem,
            reported,
        })
        .0
    }

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

    /// The documented source decides where it has an answer. This is the state
    /// this machine is actually in, and the one measured in `docs/bitlocker.md`.
    #[test]
    fn a_protected_readable_volume_is_unlocked() {
        assert_eq!(
            conclude(
                VolumeSignature::BitLocker,
                Some("NTFS"),
                Some(&reported(1, Some(1)))
            ),
            Encryption::BitLockerUnlocked
        );
    }

    /// Protection status 2 is documented as "unknown", and as being caused by a
    /// locked volume. So is a conversion status that could not be read.
    #[test]
    fn the_documented_locked_signals_are_honoured() {
        for volume in [reported(2, None), reported(2, Some(1)), reported(1, None)] {
            assert_eq!(
                conclude(VolumeSignature::BitLocker, Some("NTFS"), Some(&volume)),
                Encryption::BitLockerLocked,
                "{volume:?} should be locked even though the filesystem looks readable"
            );
        }
    }

    /// Suspended protection leaves the volume encrypted on disk but readable.
    /// It must not be reported as plain, because the backup still ends up
    /// holding decrypted data.
    #[test]
    fn suspended_protection_still_counts_as_encrypted() {
        assert_eq!(
            conclude(
                VolumeSignature::BitLocker,
                Some("NTFS"),
                Some(&reported(0, Some(1)))
            ),
            Encryption::BitLockerUnlocked
        );
    }

    /// A volume part way through encryption or decryption is encrypted at rest.
    #[test]
    fn a_conversion_in_progress_counts_as_encrypted() {
        for status in 1..=5 {
            assert_eq!(
                conclude(
                    VolumeSignature::BitLocker,
                    Some("NTFS"),
                    Some(&reported(1, Some(status)))
                ),
                Encryption::BitLockerUnlocked,
                "conversion status {status}"
            );
        }
    }

    #[test]
    fn a_fully_decrypted_volume_with_no_signature_is_plain() {
        assert_eq!(
            conclude(
                VolumeSignature::Ntfs,
                Some("NTFS"),
                Some(&reported(0, Some(0)))
            ),
            Encryption::None
        );
    }

    /// If the two sources disagree, the cautious answer is taken: a backup of a
    /// volume whose header says BitLocker must be described as holding
    /// decrypted data, whatever the status numbers say.
    #[test]
    fn a_disagreement_resolves_towards_encrypted() {
        let conclusion = classify(&Observations {
            on_disk: VolumeSignature::BitLocker,
            filesystem: Some("NTFS"),
            reported: Some(&reported(0, Some(0))),
        });
        assert_eq!(conclusion.0, Encryption::BitLockerUnlocked);
        assert!(
            conclusion.1.iter().any(|e| e.contains("still carries")),
            "the disagreement must be recorded: {:?}",
            conclusion.1
        );
    }

    /// Without WMI, which is the situation in Windows PE and on editions with no
    /// BitLocker provider, the header and the filesystem still separate the
    /// three states.
    #[test]
    fn the_signature_alone_still_gives_the_lock_state() {
        assert_eq!(
            conclude(VolumeSignature::BitLocker, Some("NTFS"), None),
            Encryption::BitLockerUnlocked
        );
        assert_eq!(
            conclude(VolumeSignature::BitLocker, Some("RAW"), None),
            Encryption::BitLockerLocked
        );
        assert_eq!(
            conclude(VolumeSignature::BitLocker, None, None),
            Encryption::BitLockerLocked
        );
        assert_eq!(
            conclude(VolumeSignature::Ntfs, Some("NTFS"), None),
            Encryption::None
        );
        assert_eq!(
            conclude(VolumeSignature::Fat, Some("FAT32"), None),
            Encryption::None
        );
    }

    /// Every conclusion has to be explainable, because the reason is shown to
    /// the operator and written to the log.
    #[test]
    fn every_conclusion_carries_its_evidence() {
        for on_disk in [
            VolumeSignature::Ntfs,
            VolumeSignature::Fat,
            VolumeSignature::BitLocker,
            VolumeSignature::Unknown,
        ] {
            for filesystem in [Some("NTFS"), Some("RAW"), None] {
                for volume in [None, Some(reported(1, Some(1))), Some(reported(0, Some(0)))] {
                    let (_, evidence) = classify(&Observations {
                        on_disk,
                        filesystem,
                        reported: volume.as_ref(),
                    });
                    assert!(
                        !evidence.is_empty(),
                        "{on_disk:?} {filesystem:?} {volume:?} produced no evidence"
                    );
                    assert!(evidence.iter().all(|e| !e.is_empty()));
                }
            }
        }
    }
}
