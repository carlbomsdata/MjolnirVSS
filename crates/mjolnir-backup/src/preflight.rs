//! Warning the operator before a backup costs them their restore points.
//!
//! # The behaviour this exists for
//!
//! Windows keeps a volume's shadow copies in one differential area and can only
//! release space from the oldest end of it. When MjolnirVSS releases the
//! temporary shadow copy it took, the volume snapshot driver sometimes has to
//! delete older shadow copies first in order to reclaim it, and it says so:
//!
//! ```text
//! System log, Volsnap, event 95
//! The oldest shadow copy of volume C: was deleted to allow shadow copies
//! created afterward and marked for delete to be deleted.
//! ```
//!
//! This was measured on a real machine on 12 September 2026: three pre-existing
//! shadow copies of `C:` were removed at the moment a MjolnirVSS diagnostic
//! released its own. See `docs/vss-lifecycle.md`.
//!
//! # What this module promises, and what it does not
//!
//! It reports what it can see: how many persistent shadow copies exist, how much
//! room they have, and how close that is to the ceiling. From that it says
//! whether the risk is worth mentioning.
//!
//! It does **not** promise that restore points survive. Nothing available to a
//! program outside the kernel can promise that, so MjolnirVSS does not say it
//! can, and the wording below is deliberately "may" rather than "should not".
//!
//! Everything here reads. Nothing changes a shadow copy, a storage limit, or a
//! restore point.

use mjolnir_core::error::Result;
use mjolnir_storage::wmi::{ShadowCopy, ShadowStorage};

/// The one line an operator is shown before a backup.
///
/// Fixed wording, because it is the sentence that has to be right.
pub const RESTORE_POINT_WARNING: &str =
    "Windows may delete older restore points when creating the temporary backup snapshot.";

/// How likely it is that taking a snapshot costs the machine a restore point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RestorePointRisk {
    /// There are no persistent shadow copies to lose.
    None,
    /// There are, and they could be deleted. This is the ordinary case on a
    /// machine with System Restore switched on.
    Possible,
    /// There are, and the space they live in is nearly full, which is the
    /// situation the deletions were measured in.
    Likely,
}

impl RestorePointRisk {
    /// Whether the operator should be told before the backup starts.
    pub fn is_worth_warning_about(self) -> bool {
        self != RestorePointRisk::None
    }

    /// A short description for a log line.
    pub const fn describe(self) -> &'static str {
        match self {
            RestorePointRisk::None => "no existing restore points to lose",
            RestorePointRisk::Possible => "existing restore points may be deleted",
            RestorePointRisk::Likely => {
                "existing restore points are likely to be deleted, because their storage is nearly full"
            }
        }
    }
}

/// Below this much headroom, a volume's shadow storage counts as nearly full.
///
/// One gibibyte is roughly what a differential area needs for the writes a
/// machine makes during a backup of any size. It is a threshold for choosing a
/// word in a warning, not a calculation anything depends on.
pub const TIGHT_HEADROOM_BYTES: u64 = 1024 * 1024 * 1024;

/// Within this fraction of the ceiling, a volume counts as nearly full too.
///
/// Expressed as a percentage so the arithmetic stays in integers.
pub const TIGHT_HEADROOM_PERCENT: u64 = 10;

/// What was found about one volume a backup is going to snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumePreflight {
    /// The volume, as a GUID path.
    pub volume: String,
    /// Its drive letter, when it has one.
    pub drive_letter: Option<String>,
    /// Persistent shadow copies of this volume that already exist.
    pub existing_persistent: u32,
    /// What Windows says about the room its shadow copies have.
    pub storage: Option<ShadowStorage>,
    /// Free space on the volume itself.
    pub free_bytes: u64,
}

impl VolumePreflight {
    /// How the volume should be named to a person.
    pub fn name(&self) -> String {
        match &self.drive_letter {
            Some(letter) => format!("{letter}:"),
            None => self.volume.clone(),
        }
    }

    /// Whether this volume's shadow storage is close enough to its ceiling for
    /// the driver to start deleting from the oldest end.
    pub fn is_tight(&self) -> bool {
        let Some(storage) = &self.storage else {
            return false;
        };
        let Some(headroom) = storage.headroom_bytes() else {
            // No ceiling. The disk filling up is a different problem with a
            // different message.
            return false;
        };
        let Some(max) = storage.max_bytes else {
            return false;
        };
        headroom < TIGHT_HEADROOM_BYTES || headroom < max / 100 * TIGHT_HEADROOM_PERCENT
    }
}

/// What a backup is about to do to this machine's shadow copies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPreflight {
    /// One entry per volume that will be snapshotted.
    pub volumes: Vec<VolumePreflight>,
    /// Whether the query could be answered at all.
    ///
    /// False without administrator rights, and inside Windows PE. The backup
    /// still runs; the operator is simply not told something that could not be
    /// established.
    pub answered: bool,
}

impl SnapshotPreflight {
    /// Nothing known, which is what a machine that cannot be asked produces.
    pub fn unknown() -> Self {
        Self {
            volumes: Vec::new(),
            answered: false,
        }
    }

    /// Persistent shadow copies across every volume that will be snapshotted.
    pub fn existing_persistent(&self) -> u32 {
        self.volumes.iter().map(|v| v.existing_persistent).sum()
    }

    /// The conclusion.
    pub fn risk(&self) -> RestorePointRisk {
        if !self.answered || self.existing_persistent() == 0 {
            return RestorePointRisk::None;
        }
        if self
            .volumes
            .iter()
            .any(|v| v.existing_persistent > 0 && v.is_tight())
        {
            return RestorePointRisk::Likely;
        }
        RestorePointRisk::Possible
    }

    /// The single sentence the simple interface shows, when there is one.
    pub fn warning(&self) -> Option<&'static str> {
        self.risk()
            .is_worth_warning_about()
            .then_some(RESTORE_POINT_WARNING)
    }

    /// The lines behind "Show details", and the ones written to the log.
    ///
    /// Deliberately factual: the figures, and what MjolnirVSS can and cannot
    /// say about them.
    pub fn details(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.answered {
            lines.push(
                "Windows did not report its shadow copy settings, so nothing is known about existing restore points."
                    .to_owned(),
            );
            return lines;
        }

        for volume in &self.volumes {
            let name = volume.name();
            match volume.existing_persistent {
                0 => lines.push(format!("{name} has no existing restore points.")),
                1 => lines.push(format!("{name} has 1 existing restore point.")),
                n => lines.push(format!("{name} has {n} existing restore points.")),
            }

            match &volume.storage {
                Some(storage) => {
                    let used = mjolnir_core::progress::format_bytes(storage.used_bytes);
                    let allocated = mjolnir_core::progress::format_bytes(storage.allocated_bytes);
                    match storage.max_bytes {
                        Some(max) => lines.push(format!(
                            "  Shadow copy storage: {used} in use, {allocated} reserved, out of a limit of {}.",
                            mjolnir_core::progress::format_bytes(max)
                        )),
                        None => lines.push(format!(
                            "  Shadow copy storage: {used} in use, {allocated} reserved, with no limit set."
                        )),
                    }
                    if volume.is_tight() {
                        lines.push(
                            "  That is close enough to the limit that Windows is likely to delete the oldest restore points."
                                .to_owned(),
                        );
                    }
                }
                None => lines.push("  Windows reports no shadow copy storage for it.".to_owned()),
            }

            lines.push(format!(
                "  Free space on the volume: {}.",
                mjolnir_core::progress::format_bytes(volume.free_bytes)
            ));
        }

        if self.risk().is_worth_warning_about() {
            lines.push(
                "MjolnirVSS deletes only the temporary shadow copy it creates, by its own identifier."
                    .to_owned(),
            );
            lines.push(
                "Windows may still remove older ones to reclaim the space, which it records in the System log as Volsnap event 95."
                    .to_owned(),
            );
            lines.push(
                "MjolnirVSS cannot prevent that and does not claim existing restore points will survive."
                    .to_owned(),
            );
        }

        lines
    }
}

/// What one volume looks like to the preflight check.
///
/// Separate from [`VolumePreflight`] so the gathering can be driven from a plan
/// on Windows and from a literal in a test.
#[derive(Debug, Clone)]
pub struct VolumeToSnapshot {
    /// Volume GUID path, with a trailing backslash.
    pub guid_path: String,
    /// Drive letter without a colon, when it has one.
    pub drive_letter: Option<String>,
    /// Free space on the volume.
    pub free_bytes: u64,
}

/// Builds the report from things already gathered.
///
/// Pure, so the whole decision table can be tested without Windows.
pub fn assess(
    volumes: &[VolumeToSnapshot],
    copies: &[ShadowCopy],
    storage: &[ShadowStorage],
) -> SnapshotPreflight {
    let matches = |a: &str, b: &str| {
        a.trim_end_matches('\\')
            .eq_ignore_ascii_case(b.trim_end_matches('\\'))
    };

    let entries = volumes
        .iter()
        .map(|v| VolumePreflight {
            existing_persistent: copies
                .iter()
                .filter(|c| c.persistent && matches(&c.volume, &v.guid_path))
                .count() as u32,
            storage: storage
                .iter()
                .find(|s| matches(&s.volume, &v.guid_path))
                .cloned(),
            volume: v.guid_path.clone(),
            drive_letter: v.drive_letter.clone(),
            free_bytes: v.free_bytes,
        })
        .collect();

    SnapshotPreflight {
        volumes: entries,
        answered: true,
    }
}

/// Asks Windows, and builds the report.
///
/// A machine that will not answer produces [`SnapshotPreflight::unknown`]
/// rather than an error: not being able to warn about something is not a reason
/// to refuse to take a backup.
#[cfg(windows)]
pub fn inspect(volumes: &[VolumeToSnapshot]) -> Result<SnapshotPreflight> {
    let copies = mjolnir_storage::wmi::shadow_copies().unwrap_or_default();
    let storage = mjolnir_storage::wmi::shadow_storage().unwrap_or_default();
    if copies.is_empty() && storage.is_empty() {
        return Ok(SnapshotPreflight::unknown());
    }
    Ok(assess(volumes, &copies, &storage))
}

/// Not available away from Windows, where there are no shadow copies.
#[cfg(not(windows))]
pub fn inspect(_volumes: &[VolumeToSnapshot]) -> Result<SnapshotPreflight> {
    Ok(SnapshotPreflight::unknown())
}

#[cfg(test)]
mod tests {
    use super::*;

    const C_DRIVE: &str = "\\\\?\\Volume{aaaaaaaa-0000-0000-0000-000000000001}\\";
    const D_DRIVE: &str = "\\\\?\\Volume{bbbbbbbb-0000-0000-0000-000000000002}\\";

    fn volume(guid: &str, letter: &str) -> VolumeToSnapshot {
        VolumeToSnapshot {
            guid_path: guid.to_owned(),
            drive_letter: Some(letter.to_owned()),
            free_bytes: 100 << 30,
        }
    }

    fn copy(guid: &str, persistent: bool) -> ShadowCopy {
        ShadowCopy {
            id: format!("{{{persistent}-{guid}}}"),
            volume: guid.to_owned(),
            persistent,
        }
    }

    fn storage(guid: &str, allocated: u64, max: Option<u64>) -> ShadowStorage {
        ShadowStorage {
            volume: guid.to_owned(),
            diff_volume: guid.to_owned(),
            used_bytes: allocated / 2,
            allocated_bytes: allocated,
            max_bytes: max,
        }
    }

    #[test]
    fn a_machine_with_no_restore_points_is_not_warned() {
        let report = assess(&[volume(C_DRIVE, "C")], &[], &[storage(C_DRIVE, 0, None)]);
        assert_eq!(report.risk(), RestorePointRisk::None);
        assert_eq!(report.warning(), None);
        assert_eq!(report.existing_persistent(), 0);
    }

    /// A non persistent shadow copy is somebody else's backup in progress, not
    /// a restore point, and is not something to warn about losing.
    #[test]
    fn a_temporary_shadow_copy_is_not_a_restore_point() {
        let report = assess(
            &[volume(C_DRIVE, "C")],
            &[copy(C_DRIVE, false)],
            &[storage(C_DRIVE, 1 << 30, None)],
        );
        assert_eq!(report.risk(), RestorePointRisk::None);
    }

    #[test]
    fn restore_points_with_plenty_of_room_are_a_possible_loss() {
        let report = assess(
            &[volume(C_DRIVE, "C")],
            &[copy(C_DRIVE, true), copy(C_DRIVE, true)],
            &[storage(C_DRIVE, 3 << 30, Some(60 << 30))],
        );
        assert_eq!(report.risk(), RestorePointRisk::Possible);
        assert_eq!(report.existing_persistent(), 2);
        assert_eq!(report.warning(), Some(RESTORE_POINT_WARNING));
    }

    /// The measured case: restore points present and the storage nearly full.
    #[test]
    fn restore_points_in_nearly_full_storage_are_a_likely_loss() {
        let report = assess(
            &[volume(C_DRIVE, "C")],
            &[copy(C_DRIVE, true)],
            &[storage(C_DRIVE, 59 << 30, Some(60 << 30))],
        );
        assert_eq!(report.risk(), RestorePointRisk::Likely);
        assert_eq!(report.warning(), Some(RESTORE_POINT_WARNING));
        assert!(report
            .details()
            .iter()
            .any(|l| l.contains("likely to delete")));
    }

    /// An unlimited storage setting must never read as nearly full, or every
    /// machine with System Restore off would get the strongest warning.
    #[test]
    fn unlimited_storage_is_never_tight() {
        let report = assess(
            &[volume(C_DRIVE, "C")],
            &[copy(C_DRIVE, true)],
            &[storage(C_DRIVE, 900 << 30, None)],
        );
        assert_eq!(report.risk(), RestorePointRisk::Possible);
    }

    /// Restore points on a volume that is not being snapshotted are nobody's
    /// business here.
    #[test]
    fn restore_points_on_other_volumes_are_ignored() {
        let report = assess(
            &[volume(C_DRIVE, "C")],
            &[copy(D_DRIVE, true), copy(D_DRIVE, true)],
            &[storage(D_DRIVE, 59 << 30, Some(60 << 30))],
        );
        assert_eq!(report.risk(), RestorePointRisk::None);
    }

    /// Volume paths are compared without caring about a trailing backslash or
    /// letter case, because the two interfaces that produce them disagree.
    #[test]
    fn volume_paths_match_regardless_of_spelling() {
        let without_slash = C_DRIVE.trim_end_matches('\\').to_uppercase();
        let report = assess(
            &[VolumeToSnapshot {
                guid_path: without_slash,
                drive_letter: Some("C".to_owned()),
                free_bytes: 0,
            }],
            &[copy(C_DRIVE, true)],
            &[storage(C_DRIVE, 59 << 30, Some(60 << 30))],
        );
        assert_eq!(report.risk(), RestorePointRisk::Likely);
    }

    /// Not being able to ask must never read as "there is nothing to lose".
    #[test]
    fn an_unanswered_query_warns_about_nothing_and_says_why() {
        let report = SnapshotPreflight::unknown();
        assert_eq!(report.risk(), RestorePointRisk::None);
        assert_eq!(report.warning(), None);
        assert!(report
            .details()
            .iter()
            .any(|l| l.contains("nothing is known")));
    }

    /// The wording is the product. It has to say "may", and must never promise
    /// that restore points survive.
    #[test]
    fn the_warning_never_promises_anything() {
        assert!(RESTORE_POINT_WARNING.contains("may delete"));

        let report = assess(
            &[volume(C_DRIVE, "C")],
            &[copy(C_DRIVE, true)],
            &[storage(C_DRIVE, 59 << 30, Some(60 << 30))],
        );
        let text = report.details().join(" ");
        assert!(text.contains("does not claim existing restore points will survive"));
        for forbidden in ["will not be deleted", "are safe", "guarantee", "preserved"] {
            assert!(
                !text.contains(forbidden),
                "the details must not say {forbidden:?}: {text}"
            );
        }
    }

    #[test]
    fn the_details_name_the_volume_and_its_figures() {
        let report = assess(
            &[volume(C_DRIVE, "C")],
            &[copy(C_DRIVE, true)],
            &[storage(C_DRIVE, 3 << 30, Some(60 << 30))],
        );
        let lines = report.details();
        assert!(lines.iter().any(|l| l.starts_with("C: has 1 existing")));
        assert!(lines.iter().any(|l| l.contains("Shadow copy storage")));
        assert!(lines.iter().any(|l| l.contains("Free space")));
    }

    #[test]
    fn every_risk_describes_itself() {
        for risk in [
            RestorePointRisk::None,
            RestorePointRisk::Possible,
            RestorePointRisk::Likely,
        ] {
            assert!(!risk.describe().is_empty());
        }
        assert!(!RestorePointRisk::None.is_worth_warning_about());
        assert!(RestorePointRisk::Possible.is_worth_warning_about());
        assert!(RestorePointRisk::Likely.is_worth_warning_about());
    }
}
