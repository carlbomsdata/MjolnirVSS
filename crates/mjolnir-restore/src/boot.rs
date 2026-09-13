//! Making a restored disk start, and saying exactly what was changed to do it.
//!
//! # Why a restored disk might not start
//!
//! A MjolnirVSS restore reproduces every partition byte for byte, including the
//! EFI system partition and the boot configuration inside it. On a machine
//! whose firmware is happy to boot the disk it is given, that is enough, and
//! nothing here has to run.
//!
//! It is not always enough:
//!
//! * the boot configuration names the Windows volume by a disk signature and a
//!   partition identifier, and a replacement disk has different ones;
//! * the firmware may have no boot entry pointing at the new disk;
//! * the recovery environment is registered by a path that includes the
//!   partition it lives in, so it can be left pointing at nothing.
//!
//! # What this does about it
//!
//! It looks first and changes second. Everything it finds is reported, every
//! change it makes is reported, and the check is run again afterwards so the
//! report says whether the change worked rather than whether it was attempted.
//!
//! The work itself is done by `bcdboot.exe`, which is Microsoft's own tool for
//! it, is present in every Windows PE image, and is the only supported way to
//! write a boot configuration. MjolnirVSS runs it; it does not write BCD files
//! itself.
//!
//! # What it will not do
//!
//! It will not touch the computer it is running on. In Windows PE that is a
//! RAM disk and there is nothing to damage, but the rule is enforced rather
//! than assumed: every path it writes to has to be on the disk that was just
//! restored.

use std::path::{Path, PathBuf};

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;

/// The files a UEFI machine needs in order to start Windows.
///
/// Relative to the root of the EFI system partition. `bootmgfw.efi` is what a
/// Windows boot entry points at; `bootx64.efi` is the fallback path firmware
/// uses when it has no entry at all, which is the usual state of a machine with
/// a brand new disk in it.
pub const REQUIRED_EFI_FILES: [&str; 3] = [
    r"EFI\Microsoft\Boot\bootmgfw.efi",
    r"EFI\Microsoft\Boot\BCD",
    r"EFI\Boot\bootx64.efi",
];

/// What was found on a restored disk.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BootState {
    /// Whether `\Windows\System32` was found on the Windows volume.
    pub windows_found: bool,
    /// Which of [`REQUIRED_EFI_FILES`] are present.
    pub efi_files_present: Vec<String>,
    /// Which of them are missing.
    pub efi_files_missing: Vec<String>,
    /// Whether a recovery image was found in the recovery partition.
    pub recovery_image_found: bool,
}

impl BootState {
    /// Whether the EFI system partition has everything it needs.
    pub fn efi_is_complete(&self) -> bool {
        self.efi_files_missing.is_empty()
    }

    /// A description for the log and the report.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(if self.windows_found {
            "Windows was found on the restored disk.".to_owned()
        } else {
            "No Windows installation was found on the restored disk.".to_owned()
        });
        for file in &self.efi_files_present {
            lines.push(format!("The boot partition has {file}."));
        }
        for file in &self.efi_files_missing {
            lines.push(format!("The boot partition is missing {file}."));
        }
        lines.push(if self.recovery_image_found {
            "A recovery image was found.".to_owned()
        } else {
            "No recovery image was found.".to_owned()
        });
        lines
    }
}

/// What has to be done about a restored disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootDecision {
    /// Everything is there. Nothing is changed.
    NothingToDo,
    /// The boot configuration has to be written again.
    RewriteBootFiles {
        /// Why, in words, for the operator and the log.
        reason: String,
    },
    /// Nothing can be done, because there is no Windows to point at.
    CannotRepair {
        /// What is missing.
        reason: String,
    },
}

impl BootDecision {
    /// Whether this decision changes anything on the disk.
    pub fn writes_anything(&self) -> bool {
        matches!(self, BootDecision::RewriteBootFiles { .. })
    }
}

/// Decides what to do, from what was found.
///
/// Pure, so the whole table is exercised by ordinary tests. The rule is
/// deliberately cautious in one direction only: a disk that looks complete is
/// left alone, and anything else is rewritten, because writing a boot
/// configuration over a correct one costs nothing and not writing one over a
/// broken one costs the whole restore.
pub fn decide(state: &BootState) -> BootDecision {
    if !state.windows_found {
        return BootDecision::CannotRepair {
            reason: "there is no Windows installation on the restored disk to point a boot configuration at".to_owned(),
        };
    }
    if state.efi_is_complete() {
        return BootDecision::NothingToDo;
    }
    BootDecision::RewriteBootFiles {
        reason: format!(
            "the boot partition is missing {}",
            state.efi_files_missing.join(", ")
        ),
    }
}

/// How the firmware starts the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Firmware {
    /// UEFI, which is the only mode MjolnirVSS supports.
    Uefi,
}

impl Firmware {
    /// The value `bcdboot` wants after `/f`.
    pub const fn bcdboot_name(self) -> &'static str {
        match self {
            Firmware::Uefi => "UEFI",
        }
    }
}

/// Builds the `bcdboot` command line for a restored disk.
///
/// `windows_dir` is the Windows folder on the restored volume, for example
/// `C:\Windows`. `esp_letter` is the drive letter the EFI system partition was
/// temporarily given.
///
/// `/f UEFI` is spelled out rather than left to `bcdboot` to guess, because the
/// machine doing the repair may have started in a different mode from the one
/// the restored machine will.
pub fn bcdboot_args(windows_dir: &Path, esp_letter: char, firmware: Firmware) -> Vec<String> {
    vec![
        windows_dir.display().to_string(),
        "/s".to_owned(),
        format!("{esp_letter}:"),
        "/f".to_owned(),
        firmware.bcdboot_name().to_owned(),
    ]
}

/// One thing that was looked at or changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// What it was.
    pub what: String,
    /// Whether it worked.
    pub ok: bool,
    /// What happened.
    pub detail: String,
}

/// What a repair looked at, changed, and found afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BootRepairReport {
    /// What was found before anything was changed.
    pub before: Vec<String>,
    /// Every change that was made, in order. Empty when nothing was changed.
    pub changed: Vec<String>,
    /// What was checked afterwards.
    pub after: Vec<Step>,
    /// Whether the disk is expected to start.
    pub succeeded: bool,
    /// Anything the operator should know.
    pub notes: Vec<String>,
}

impl BootRepairReport {
    /// Whether anything on the disk was modified.
    pub fn changed_anything(&self) -> bool {
        !self.changed.is_empty()
    }

    /// The report an operator reads.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        out.push_str("Boot configuration\n");
        out.push_str("==================\n\n");

        out.push_str("What was found:\n");
        for line in &self.before {
            out.push_str(&format!("  {line}\n"));
        }

        out.push_str("\nWhat was changed:\n");
        if self.changed.is_empty() {
            out.push_str("  Nothing. The restored disk already had what it needs.\n");
        } else {
            for line in &self.changed {
                out.push_str(&format!("  {line}\n"));
            }
        }

        if !self.after.is_empty() {
            out.push_str("\nChecked afterwards:\n");
            for step in &self.after {
                out.push_str(&format!(
                    "  [{}] {}: {}\n",
                    if step.ok { "ok" } else { "FAILED" },
                    step.what,
                    step.detail
                ));
            }
        }

        out.push_str(&format!(
            "\nResult: {}\n",
            if self.succeeded {
                "the restored disk has a complete boot configuration"
            } else {
                "the restored disk does NOT have a complete boot configuration"
            }
        ));
        for note in &self.notes {
            out.push_str(&format!("  {note}\n"));
        }
        out
    }
}

/// Looks at a restored disk through the folders its partitions are mounted at.
///
/// `windows_root` is the root of the restored Windows volume, `esp_root` the
/// root of its EFI system partition, and `recovery_root` the recovery partition
/// when there is one.
pub fn inspect(windows_root: &Path, esp_root: &Path, recovery_root: Option<&Path>) -> BootState {
    let mut present = Vec::new();
    let mut missing = Vec::new();
    for relative in REQUIRED_EFI_FILES {
        if esp_root.join(relative).is_file() {
            present.push(relative.to_owned());
        } else {
            missing.push(relative.to_owned());
        }
    }

    let recovery_image_found = recovery_root
        .map(|root| {
            root.join(r"Recovery\WindowsRE\winre.wim").is_file()
                || root.join(r"Recovery\WindowsRE\Winre.wim").is_file()
        })
        .unwrap_or(false)
        || windows_root
            .join(r"Windows\System32\Recovery\Winre.wim")
            .is_file();

    BootState {
        windows_found: windows_root.join(r"Windows\System32").is_dir(),
        efi_files_present: present,
        efi_files_missing: missing,
        recovery_image_found,
    }
}

/// The Windows folder on a restored volume.
pub fn windows_dir(windows_root: &Path) -> PathBuf {
    windows_root.join("Windows")
}

/// Refuses to repair a disk that is the one this program is running from.
///
/// In Windows PE the running system is a RAM disk, so this never fires there.
/// It exists because the same code can be run from a full Windows for testing,
/// and a boot repair aimed at the running machine would be a very bad way to
/// find that out.
pub fn check_not_the_running_system(windows_root: &Path) -> Result<()> {
    let Ok(running) = std::env::var("SystemDrive") else {
        return Ok(());
    };
    let running = running.trim_end_matches('\\').to_ascii_uppercase();

    let target = windows_root
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_ascii_uppercase();

    if !running.is_empty() && target == running {
        return Err(Error::new(
            ExitCode::UnsafeTarget,
            "refusing to change the boot configuration of the running system",
            format!(
                "the restored volume was given {target}, which is the drive this program is running from"
            ),
            "this is an internal error; please report it with the command you ran",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete() -> BootState {
        BootState {
            windows_found: true,
            efi_files_present: REQUIRED_EFI_FILES.iter().map(|s| s.to_string()).collect(),
            efi_files_missing: Vec::new(),
            recovery_image_found: true,
        }
    }

    #[test]
    fn a_complete_disk_is_left_alone() {
        assert_eq!(decide(&complete()), BootDecision::NothingToDo);
        assert!(!decide(&complete()).writes_anything());
    }

    /// The case the whole module exists for: the boot files did not survive, or
    /// never existed, and the disk will not start without them.
    #[test]
    fn a_missing_boot_loader_is_rewritten() {
        let state = BootState {
            efi_files_present: vec![r"EFI\Microsoft\Boot\BCD".to_owned()],
            efi_files_missing: vec![
                r"EFI\Microsoft\Boot\bootmgfw.efi".to_owned(),
                r"EFI\Boot\bootx64.efi".to_owned(),
            ],
            ..complete()
        };
        let decision = decide(&state);
        assert!(decision.writes_anything());
        match decision {
            BootDecision::RewriteBootFiles { reason } => {
                assert!(reason.contains("bootmgfw.efi"));
                assert!(reason.contains("bootx64.efi"));
            }
            other => panic!("expected a rewrite, got {other:?}"),
        }
    }

    /// A missing boot configuration is as fatal as a missing loader, and is the
    /// one a restore onto a different disk is most likely to produce.
    #[test]
    fn a_missing_boot_configuration_is_rewritten() {
        let state = BootState {
            efi_files_present: vec![
                r"EFI\Microsoft\Boot\bootmgfw.efi".to_owned(),
                r"EFI\Boot\bootx64.efi".to_owned(),
            ],
            efi_files_missing: vec![r"EFI\Microsoft\Boot\BCD".to_owned()],
            ..complete()
        };
        assert!(decide(&state).writes_anything());
    }

    /// Without Windows there is nothing to point at, and writing a boot
    /// configuration anyway would produce a machine that fails later and less
    /// clearly.
    #[test]
    fn a_disk_without_windows_cannot_be_repaired() {
        let state = BootState {
            windows_found: false,
            ..complete()
        };
        match decide(&state) {
            BootDecision::CannotRepair { reason } => {
                assert!(reason.contains("no Windows installation"));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(!decide(&state).writes_anything());
    }

    /// A missing recovery environment is worth reporting and is not a reason to
    /// rewrite anything: the machine still starts without it.
    #[test]
    fn a_missing_recovery_image_does_not_by_itself_cause_a_rewrite() {
        let state = BootState {
            recovery_image_found: false,
            ..complete()
        };
        assert_eq!(decide(&state), BootDecision::NothingToDo);
    }

    #[test]
    fn the_bcdboot_command_names_the_firmware_explicitly() {
        let args = bcdboot_args(Path::new(r"C:\Windows"), 'S', Firmware::Uefi);
        assert_eq!(
            args,
            vec![
                r"C:\Windows".to_owned(),
                "/s".to_owned(),
                "S:".to_owned(),
                "/f".to_owned(),
                "UEFI".to_owned(),
            ]
        );
    }

    #[test]
    fn inspecting_a_complete_layout_finds_everything() {
        let dir = tempfile::tempdir().unwrap();
        let windows = dir.path().join("win");
        let esp = dir.path().join("esp");
        let recovery = dir.path().join("rec");

        std::fs::create_dir_all(windows.join(r"Windows\System32")).unwrap();
        for relative in REQUIRED_EFI_FILES {
            let path = esp.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"x").unwrap();
        }
        std::fs::create_dir_all(recovery.join(r"Recovery\WindowsRE")).unwrap();
        std::fs::write(recovery.join(r"Recovery\WindowsRE\winre.wim"), b"x").unwrap();

        let state = inspect(&windows, &esp, Some(&recovery));
        assert!(state.windows_found);
        assert!(state.efi_is_complete());
        assert!(state.recovery_image_found);
        assert_eq!(decide(&state), BootDecision::NothingToDo);
    }

    #[test]
    fn inspecting_an_empty_layout_finds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let state = inspect(&dir.path().join("win"), &dir.path().join("esp"), None);

        assert!(!state.windows_found);
        assert!(!state.efi_is_complete());
        assert_eq!(state.efi_files_missing.len(), REQUIRED_EFI_FILES.len());
        assert!(!state.recovery_image_found);
    }

    /// A recovery image kept inside Windows rather than in its own partition is
    /// a real layout and counts.
    #[test]
    fn a_recovery_image_inside_windows_counts() {
        let dir = tempfile::tempdir().unwrap();
        let windows = dir.path().join("win");
        std::fs::create_dir_all(windows.join(r"Windows\System32\Recovery")).unwrap();
        std::fs::write(windows.join(r"Windows\System32\Recovery\Winre.wim"), b"x").unwrap();

        let state = inspect(&windows, &dir.path().join("esp"), None);
        assert!(state.recovery_image_found);
    }

    #[test]
    fn the_report_says_what_changed_and_what_did_not() {
        let report = BootRepairReport {
            before: vec!["The boot partition is missing something.".to_owned()],
            changed: vec!["Wrote the boot configuration.".to_owned()],
            after: vec![Step {
                what: "the boot loader is present".to_owned(),
                ok: true,
                detail: "found".to_owned(),
            }],
            succeeded: true,
            notes: Vec::new(),
        };
        let text = report.describe();
        assert!(report.changed_anything());
        assert!(text.contains("What was changed"));
        assert!(text.contains("Wrote the boot configuration"));
        assert!(text.contains("[ok]"));
        assert!(text.contains("has a complete boot configuration"));
    }

    /// A report that changed nothing has to say so in words, not by leaving the
    /// section empty.
    #[test]
    fn a_report_that_changed_nothing_says_so() {
        let report = BootRepairReport {
            before: vec!["Everything was there.".to_owned()],
            succeeded: true,
            ..Default::default()
        };
        assert!(!report.changed_anything());
        assert!(report
            .describe()
            .contains("Nothing. The restored disk already had"));
    }

    #[test]
    fn a_failed_repair_says_so_plainly() {
        let report = BootRepairReport {
            succeeded: false,
            ..Default::default()
        };
        assert!(report.describe().contains("does NOT have a complete"));
    }

    /// The safety rule: never repair the machine doing the repairing.
    #[test]
    fn the_running_system_is_refused() {
        let Ok(system_drive) = std::env::var("SystemDrive") else {
            return;
        };
        let err = check_not_the_running_system(Path::new(&system_drive)).unwrap_err();
        assert_eq!(err.exit(), ExitCode::UnsafeTarget);
        assert!(err.what().contains("running system"));

        // A trailing separator is the same drive and must be caught too.
        assert!(check_not_the_running_system(Path::new(&format!("{system_drive}\\"))).is_err());
    }

    #[test]
    fn another_drive_is_allowed() {
        // A letter no machine in this test uses for its system drive.
        assert!(check_not_the_running_system(Path::new("W:")).is_ok());
    }

    #[test]
    fn every_state_describes_itself() {
        for state in [complete(), BootState::default()] {
            let lines = state.describe();
            assert!(!lines.is_empty());
            assert!(lines.iter().all(|l| !l.is_empty()));
        }
    }
}
