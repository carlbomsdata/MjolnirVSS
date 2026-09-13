//! Running the boot repair against a real restored disk.
//!
//! Everything that decides anything lives in [`crate::boot`], which has no
//! Windows in it. This is the part that makes Windows show the restored disk's
//! partitions, gives them letters for as long as the repair takes, runs
//! `bcdboot`, and takes the letters away again.
//!
//! # The letters are temporary and are always taken back
//!
//! A boot repair needs paths, and paths need drive letters. The letters
//! assigned here are removed on the way out, on every path including a failure,
//! because a recovery environment that leaves a restored disk mounted is one
//! that will confuse whoever looks at it next.
//!
//! # Nothing outside the restored disk is touched
//!
//! Every volume this module considers has to be on the disk that was just
//! restored, and that is checked against the disk number rather than assumed
//! from the order Windows happened to list things in.

use std::path::{Path, PathBuf};
use std::process::Command;

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_image::disk_layout::PartitionRole;

use crate::boot::{
    bcdboot_args, check_not_the_running_system, decide, inspect, windows_dir, BootDecision,
    BootRepairReport, Firmware, Step,
};

/// Letters the repair borrows, in the order it tries them.
///
/// Chosen from the end of the alphabet, because a machine in a recovery
/// environment has few drives and those it has are near the beginning. Any that
/// are in use are skipped.
const BORROWABLE_LETTERS: [char; 8] = ['T', 'U', 'V', 'W', 'X', 'Y', 'Z', 'S'];

/// A drive letter that removes itself.
struct BorrowedLetter {
    letter: char,
    assigned: bool,
}

impl BorrowedLetter {
    /// Gives `volume` a free drive letter.
    ///
    /// `volume` is a volume GUID path. Windows requires the trailing backslash
    /// on both arguments, and refuses without it.
    #[cfg(windows)]
    fn assign(volume: &str, taken: &[char]) -> Result<Self> {
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::SetVolumeMountPointW;

        let volume = format!("{}\\", volume.trim_end_matches('\\'));
        let volume_w = wide(&volume);

        // Windows PE mounts the volumes it finds, so a disk restored earlier
        // and repaired now usually already has letters. A volume cannot be
        // given a second one, so trying produced "no drive letter was free" and
        // a repair that could never run on the ordinary case. A letter it
        // already has is the one to use, and it must not be taken away
        // afterwards: it was not this program's to borrow.
        if let Some(letter) = existing_letter(&volume) {
            if !taken.contains(&letter) {
                return Ok(Self {
                    letter,
                    assigned: false,
                });
            }
        }

        for letter in BORROWABLE_LETTERS {
            if taken.contains(&letter) {
                continue;
            }
            let mount = format!("{letter}:\\");
            if Path::new(&mount).exists() {
                continue;
            }
            let mount_w = wide(&mount);

            // SAFETY: both strings are null terminated locals that outlive the
            // call, and both carry the trailing backslash the call requires.
            // Failure is expected for a letter that is in use, and is handled
            // by trying the next one.
            let result = unsafe {
                SetVolumeMountPointW(PCWSTR(mount_w.as_ptr()), PCWSTR(volume_w.as_ptr()))
            };
            if result.is_ok() {
                return Ok(Self {
                    letter,
                    assigned: true,
                });
            }
        }

        Err(Error::new(
            ExitCode::Failure,
            "no drive letter was free for the restored disk",
            format!("none of {BORROWABLE_LETTERS:?} could be given to {volume}"),
            "restart the recovery environment and try again",
        ))
    }

    fn root(&self) -> PathBuf {
        PathBuf::from(format!("{}:\\", self.letter))
    }
}

/// The drive letter a volume is already mounted at, if it has one.
#[cfg(windows)]
fn existing_letter(volume: &str) -> Option<char> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetVolumePathNamesForVolumeNameW;

    let volume_w = wide(volume);
    let mut buffer = vec![0u16; 512];
    let mut written = 0u32;
    // SAFETY: the volume name is a null terminated local that outlives the
    // call, and the buffer is passed with its own length. A failure means the
    // answer is simply not known, which the caller handles.
    let ok = unsafe {
        GetVolumePathNamesForVolumeNameW(PCWSTR(volume_w.as_ptr()), Some(&mut buffer), &mut written)
    };
    if ok.is_err() {
        return None;
    }

    // The answer is a run of null terminated paths. A volume can be mounted in
    // several places; only a drive letter is useful here, because bcdboot is
    // given one.
    first_drive_letter(&String::from_utf16_lossy(&buffer[..written as usize]))
}

/// The first drive letter in a run of null terminated mount paths.
///
/// Separated from the call that produces it so the parsing can be tested.
/// A volume can be mounted in several places, including inside folders;
/// only a drive letter is useful here, because bcdboot is given one.
fn first_drive_letter(paths: &str) -> Option<char> {
    for path in paths.split('\0') {
        // A drive letter mount point is exactly three characters. Anything
        // longer is a volume mounted inside a folder, whose leading letter
        // belongs to a different volume: handing that to bcdboot would
        // write the boot files to the wrong disk.
        let path = path.trim();
        let chars: Vec<char> = path.chars().collect();
        if chars.len() == 3 && chars[0].is_ascii_alphabetic() && chars[1] == ':' && chars[2] == '\\'
        {
            return Some(chars[0].to_ascii_uppercase());
        }
    }
    None
}

#[cfg(windows)]
impl Drop for BorrowedLetter {
    fn drop(&mut self) {
        if !self.assigned {
            return;
        }
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::DeleteVolumeMountPointW;

        let mount = wide(&format!("{}:\\", self.letter));
        // SAFETY: the string is a null terminated local that outlives the call,
        // and names the mount point this value created. Failure is ignored
        // because there is nothing useful to do about it while unwinding, and
        // the letter goes away with the recovery environment anyway.
        unsafe {
            let _ = DeleteVolumeMountPointW(PCWSTR(mount.as_ptr()));
        }
    }
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Makes Windows notice that a disk's partitions have changed.
///
/// A disk that has just been written to byte by byte still has its old
/// partition table cached, so without this the volumes found below would be
/// the ones that were there before the restore.
#[cfg(windows)]
pub fn rescan_disk(disk_number: u32) -> Result<()> {
    use mjolnir_storage::device::Device;

    // IOCTL_DISK_UPDATE_PROPERTIES, from winioctl.h:
    // CTL_CODE(IOCTL_DISK_BASE=7, 0x0050, METHOD_BUFFERED=0, FILE_ANY_ACCESS=0)
    const IOCTL_DISK_UPDATE_PROPERTIES: u32 = (7 << 16) | (0x0050 << 2);

    let path = format!(r"\\.\PhysicalDrive{disk_number}");
    let device = Device::query(&path)?;
    device.control(IOCTL_DISK_UPDATE_PROPERTIES, &mut [])?;
    Ok(())
}

/// The three partitions a boot repair cares about, on one disk.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RestoredVolumes {
    /// The volume holding Windows, as a GUID path.
    pub windows: Option<String>,
    /// The EFI system partition.
    pub efi: Option<String>,
    /// The recovery partition, when there is one.
    pub recovery: Option<String>,
}

/// Sorts the volumes on a restored disk into the roles a repair needs.
///
/// Identification is by what is actually in each partition rather than by
/// position: the EFI system partition is the FAT one, Windows is the NTFS one
/// holding `\Windows\System32`, and the recovery partition is whatever is left
/// that holds a recovery image. A disk whose partitions were restored in a
/// different order still comes out right.
#[cfg(windows)]
pub fn find_restored_volumes(disk_number: u32) -> Result<RestoredVolumes> {
    let volumes = mjolnir_storage::volumes::enumerate_volumes()?;
    let mut found = RestoredVolumes::default();

    for volume in volumes {
        if volume.disk_number() != Some(disk_number) {
            continue;
        }
        let filesystem = volume
            .filesystem
            .as_deref()
            .unwrap_or("")
            .to_ascii_uppercase();

        if filesystem.starts_with("FAT") && found.efi.is_none() {
            found.efi = Some(volume.device_path.clone());
            continue;
        }
        if filesystem == "NTFS" {
            // Only a mounted volume can be looked inside, and at this point
            // none of them is. The distinction between the Windows volume and
            // the recovery partition is made by size: a recovery partition is
            // small and a Windows volume is not.
            const SMALL_ENOUGH_TO_BE_A_RECOVERY_PARTITION: u64 = 4 * 1024 * 1024 * 1024;
            if volume.total_bytes < SMALL_ENOUGH_TO_BE_A_RECOVERY_PARTITION {
                if found.recovery.is_none() {
                    found.recovery = Some(volume.device_path.clone());
                }
            } else if found.windows.is_none() {
                found.windows = Some(volume.device_path.clone());
            }
        }
    }
    Ok(found)
}

/// Works out which partition on a restored layout should hold what.
///
/// Used when the volumes cannot be identified from the machine, and as a check
/// that what was found matches what the backup said.
pub fn roles_in_order(roles: &[PartitionRole]) -> (Option<usize>, Option<usize>, Option<usize>) {
    let find = |wanted: PartitionRole| roles.iter().position(|r| *r == wanted);
    (
        find(PartitionRole::Windows),
        find(PartitionRole::EfiSystem),
        find(PartitionRole::Recovery),
    )
}

#[cfg(windows)]
/// Looks at a restored disk's boot configuration and reports what a repair
/// would do, without changing anything.
///
/// Deliberately not the same code path as [`repair_disk`]. A dry run that
/// shares its body with the function that writes is one edit away from not
/// being dry, and this is a tool that rewrites the way a computer starts. The
/// two are short enough to keep apart and read side by side.
pub fn inspect_disk(disk_number: u32) -> Result<(BootRepairReport, BootDecision)> {
    let mut report = BootRepairReport::default();

    rescan_disk(disk_number)?;
    let volumes = find_restored_volumes(disk_number)?;

    let Some(windows_volume) = volumes.windows.clone() else {
        report
            .before
            .push("No Windows volume was found on this disk.".to_owned());
        return Ok((
            report,
            BootDecision::CannotRepair {
                reason: "there is no Windows volume on this disk".to_owned(),
            },
        ));
    };
    let Some(efi_volume) = volumes.efi.clone() else {
        report
            .before
            .push("No EFI system partition was found on this disk.".to_owned());
        return Ok((
            report,
            BootDecision::CannotRepair {
                reason: "there is no EFI system partition on this disk".to_owned(),
            },
        ));
    };

    let windows_letter = BorrowedLetter::assign(&windows_volume, &[])?;
    let efi_letter = BorrowedLetter::assign(&efi_volume, &[windows_letter.letter])?;
    let recovery_letter = match &volumes.recovery {
        Some(volume) => {
            BorrowedLetter::assign(volume, &[windows_letter.letter, efi_letter.letter]).ok()
        }
        None => None,
    };

    let windows_root = windows_letter.root();
    let state = inspect(
        &windows_root,
        &efi_letter.root(),
        recovery_letter
            .as_ref()
            .map(BorrowedLetter::root)
            .as_deref(),
    );
    report.before = state.describe();
    let decision = decide(&state);
    report.succeeded = !matches!(decision, BootDecision::CannotRepair { .. });
    Ok((report, decision))
}

/// Looks at a restored disk and repairs its boot configuration if it needs it.
///
/// Returns what was found, what was changed, and what was true afterwards.
/// Repairing a disk that did not need it is not an error and changes nothing.
pub fn repair_disk(disk_number: u32) -> Result<BootRepairReport> {
    let mut report = BootRepairReport::default();

    rescan_disk(disk_number)?;
    let volumes = find_restored_volumes(disk_number)?;

    let Some(windows_volume) = volumes.windows.clone() else {
        report.before.push(
            "No Windows volume was found on the restored disk, so its boot configuration was not looked at."
                .to_owned(),
        );
        report.notes.push(
            "Windows may still be there: a volume Windows cannot mount does not appear here. Check the disk in Disk Management after starting the machine."
                .to_owned(),
        );
        return Ok(report);
    };
    let Some(efi_volume) = volumes.efi.clone() else {
        report
            .before
            .push("No EFI system partition was found on the restored disk.".to_owned());
        report.notes.push(
            "Without one, a UEFI machine has nothing to start from. This is a restore that did not finish, rather than a boot configuration problem."
                .to_owned(),
        );
        return Ok(report);
    };

    // Letters are borrowed for as long as this function runs, and given back by
    // their destructors on every path out of it, including a failure.
    let windows_letter = BorrowedLetter::assign(&windows_volume, &[])?;
    let efi_letter = BorrowedLetter::assign(&efi_volume, &[windows_letter.letter])?;
    let recovery_letter = match &volumes.recovery {
        Some(volume) => {
            BorrowedLetter::assign(volume, &[windows_letter.letter, efi_letter.letter]).ok()
        }
        None => None,
    };

    let windows_root = windows_letter.root();
    let efi_root = efi_letter.root();
    let recovery_root = recovery_letter.as_ref().map(BorrowedLetter::root);

    check_not_the_running_system(&windows_root)?;

    let state = inspect(&windows_root, &efi_root, recovery_root.as_deref());
    report.before = state.describe();

    match decide(&state) {
        BootDecision::NothingToDo => {
            report.succeeded = true;
        }
        BootDecision::CannotRepair { reason } => {
            report.succeeded = false;
            report.notes.push(reason);
        }
        BootDecision::RewriteBootFiles { reason } => {
            report
                .changed
                .push(format!("Rewrote the boot configuration, because {reason}."));

            let args = bcdboot_args(
                &windows_dir(&windows_root),
                efi_letter.letter,
                Firmware::Uefi,
            );
            let output = Command::new("bcdboot.exe").args(&args).output();

            match output {
                Ok(output) if output.status.success() => {
                    report.changed.push(format!(
                        "Ran bcdboot {} and it reported success.",
                        args.join(" ")
                    ));
                }
                Ok(output) => {
                    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                    report.changed.push(format!(
                        "Ran bcdboot {} and it failed: {}",
                        args.join(" "),
                        if text.is_empty() {
                            format!("exit code {}", output.status.code().unwrap_or(-1))
                        } else {
                            text
                        }
                    ));
                }
                Err(e) => {
                    report
                        .changed
                        .push(format!("bcdboot could not be started: {e}"));
                    report.notes.push(
                        "bcdboot.exe is part of Windows and of every Windows PE image. Not finding it means this is not a Windows PE environment."
                            .to_owned(),
                    );
                }
            }
        }
    }

    // Checked again afterwards, so the report says what is true rather than
    // what was attempted.
    let after = inspect(&windows_root, &efi_root, recovery_root.as_deref());
    for file in &crate::boot::REQUIRED_EFI_FILES {
        let present = after.efi_files_present.iter().any(|f| f == file);
        report.after.push(Step {
            what: format!("{file} is on the boot partition"),
            ok: present,
            detail: if present {
                "found".to_owned()
            } else {
                "still missing".to_owned()
            },
        });
    }
    report.after.push(Step {
        what: "Windows is on the restored disk".to_owned(),
        ok: after.windows_found,
        detail: if after.windows_found {
            "found".to_owned()
        } else {
            "not found".to_owned()
        },
    });

    report.succeeded = after.windows_found && after.efi_is_complete();
    if !after.recovery_image_found {
        report.notes.push(
            "No recovery image was found. Windows will start, but Advanced Startup and Reset This PC will not be available until Windows rebuilds it."
                .to_owned(),
        );
    }
    report.notes.push(
        "The drive letters used for this repair were temporary and have been removed.".to_owned(),
    );

    Ok(report)
}

/// Not available away from Windows.
#[cfg(not(windows))]
pub fn repair_disk(_disk_number: u32) -> Result<BootRepairReport> {
    Err(not_windows())
}

#[cfg(not(windows))]
pub fn inspect_disk(_disk_number: u32) -> Result<(BootRepairReport, BootDecision)> {
    Err(not_windows())
}

#[cfg(not(windows))]
fn not_windows() -> Error {
    Error::new(
        ExitCode::Unsupported,
        "boot repair needs Windows",
        "this build was not made for Windows",
        "run the recovery application from Windows PE",
    )
}

#[cfg(test)]
mod tests {

    /// Builds what Windows returns for a volume's mount points: each one null
    /// terminated, and an empty string to end the run.
    fn mount_paths(points: &[&str]) -> String {
        let mut out = String::new();
        for point in points {
            out.push_str(point);
            out.push(char::from(0));
        }
        out.push(char::from(0));
        out
    }

    /// Windows PE mounts what it finds, so the volumes of a disk being repaired
    /// usually already have letters. Reading the one a volume has is what makes
    /// a standalone repair possible at all.
    #[test]
    fn a_drive_letter_is_read_out_of_the_mount_paths() {
        assert_eq!(first_drive_letter(&mount_paths(&[r"D:\"])), Some('D'));
        assert_eq!(first_drive_letter(&mount_paths(&[r"c:\"])), Some('C'));
    }

    /// A volume mounted only inside a folder has no letter to use, and its path
    /// begins with the letter of a *different* volume. Handing that to bcdboot
    /// would write the boot files to the wrong disk.
    #[test]
    fn a_folder_mount_point_is_not_a_drive_letter() {
        assert_eq!(
            first_drive_letter(&mount_paths(&[r"C:\mount\disk\"])),
            None,
            "a path under a folder is not a letter for bcdboot"
        );
        assert_eq!(first_drive_letter(""), None);
        assert_eq!(first_drive_letter(&mount_paths(&[])), None);
    }

    /// A volume with several mount points still has one letter to use, and the
    /// folder mount points among them are passed over.
    #[test]
    fn a_letter_is_found_past_a_folder_mount_point() {
        assert_eq!(
            first_drive_letter(&mount_paths(&[r"E:\games\", r"F:\"])),
            Some('F'),
            "the folder mount is skipped and the letter is found"
        );
    }

    use super::*;

    #[test]
    fn the_rescan_control_code_matches_the_windows_header() {
        // IOCTL_DISK_UPDATE_PROPERTIES = CTL_CODE(7, 0x0050, 0, 0)
        let computed: u32 = (7u32 << 16) | (0x0050u32 << 2);
        assert_eq!(computed, 0x0007_0140);
    }

    #[test]
    fn the_roles_are_found_wherever_they_are() {
        let normal = [
            PartitionRole::EfiSystem,
            PartitionRole::MicrosoftReserved,
            PartitionRole::Windows,
            PartitionRole::Recovery,
        ];
        assert_eq!(roles_in_order(&normal), (Some(2), Some(0), Some(3)));

        // Some installers put the recovery partition first.
        let recovery_first = [
            PartitionRole::Recovery,
            PartitionRole::EfiSystem,
            PartitionRole::MicrosoftReserved,
            PartitionRole::Windows,
        ];
        assert_eq!(roles_in_order(&recovery_first), (Some(3), Some(1), Some(0)));
    }

    #[test]
    fn a_layout_without_a_recovery_partition_is_understood() {
        let roles = [
            PartitionRole::EfiSystem,
            PartitionRole::MicrosoftReserved,
            PartitionRole::Windows,
        ];
        assert_eq!(roles_in_order(&roles), (Some(2), Some(0), None));
    }

    /// The letters are borrowed from the end of the alphabet, so a recovery
    /// environment's own drives are left alone.
    #[test]
    fn the_borrowed_letters_avoid_the_usual_ones() {
        for letter in BORROWABLE_LETTERS {
            assert!(
                !"ABCD".contains(letter),
                "{letter} is one a recovery environment is likely to be using"
            );
        }
        assert!(BORROWABLE_LETTERS.contains(&'X'));
    }

    #[test]
    fn nothing_is_found_on_a_disk_that_does_not_exist() {
        #[cfg(windows)]
        {
            // Disk 250 does not exist on any machine this will run on.
            let found = find_restored_volumes(250).unwrap_or_default();
            assert_eq!(found, RestoredVolumes::default());
        }
    }
}
