//! What recovery media has to contain, decided without touching a disk.
//!
//! Keeping this separate from the building means every rule about what goes
//! where, and every refusal, is exercised by an ordinary test rather than only
//! by making a real disc.

use std::path::{Path, PathBuf};

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;

/// Folder inside the boot image that MjolnirVSS puts itself in.
pub const PAYLOAD_DIR: &str = "MjolnirVSS";

/// The file Windows PE runs when it starts.
pub const STARTNET_PATH: &str = r"Windows\System32\startnet.cmd";

/// The recovery application's file name on the media.
pub const RECOVERY_EXE: &str = "MjolnirVSS.Restore.exe";

/// A file that is copied into the boot image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadFile {
    /// Where it comes from on this computer.
    pub from: PathBuf,
    /// Where it goes inside the image, relative to the image's root.
    pub to: PathBuf,
}

/// Everything that has to be put inside the boot image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    /// Files to copy in.
    pub files: Vec<PayloadFile>,
    /// The contents of `startnet.cmd`.
    pub startnet: String,
}

/// Builds the payload from a folder holding a MjolnirVSS release.
///
/// The recovery application is required. Everything else is optional, and its
/// absence is not a failure: a recovery disc that carries the program but not
/// the documentation still recovers a computer.
pub fn payload_from_release(release_dir: &Path) -> Result<Payload> {
    let exe = release_dir.join(RECOVERY_EXE);
    if !exe.is_file() {
        return Err(Error::new(
            ExitCode::Failure,
            "the recovery application was not found",
            format!(
                "{RECOVERY_EXE} is not in {}, so there would be nothing on the recovery media to run",
                release_dir.display()
            ),
            "point MjolnirVSS at the folder it was extracted to, the one holding both programs",
        ));
    }

    let mut files = vec![PayloadFile {
        from: exe,
        to: Path::new(PAYLOAD_DIR).join(RECOVERY_EXE),
    }];

    // Carried so that somebody holding only the recovery stick can still read
    // what it does and what licence it is under.
    for optional in ["LICENSE", "NOTICE", "README.md"] {
        let path = release_dir.join(optional);
        if path.is_file() {
            files.push(PayloadFile {
                from: path,
                to: Path::new(PAYLOAD_DIR).join(optional),
            });
        }
    }
    let restore_doc = release_dir.join("docs").join("bare-metal-restore.md");
    if restore_doc.is_file() {
        files.push(PayloadFile {
            from: restore_doc,
            to: Path::new(PAYLOAD_DIR)
                .join("docs")
                .join("bare-metal-restore.md"),
        });
    }

    Ok(Payload {
        files,
        startnet: startnet_script(),
    })
}

/// The script Windows PE runs when it starts.
///
/// `wpeinit` is what brings the network and the device stack up, and skipping
/// it is the usual reason a custom Windows PE image cannot see any disks. The
/// recovery application is started after it, and the console is left running
/// underneath so that a failure leaves somebody a prompt to work from rather
/// than a machine that reboots.
pub fn startnet_script() -> String {
    let mut s = String::new();
    s.push_str("@echo off\r\n");
    s.push_str("wpeinit\r\n");
    s.push_str("echo MjolnirVSS recovery environment\r\n");
    // The drive letter Windows PE gives itself is always X:, and the payload is
    // inside the boot image, so this path does not depend on which disc or
    // stick the machine started from.
    s.push_str(&format!("if exist X:\\{PAYLOAD_DIR}\\{RECOVERY_EXE} (\r\n"));
    s.push_str(&format!(
        "  start \"MjolnirVSS\" X:\\{PAYLOAD_DIR}\\{RECOVERY_EXE}\r\n"
    ));
    s.push_str(") else (\r\n");
    s.push_str("  echo The recovery application is missing from this media.\r\n");
    s.push_str(")\r\n");
    s
}

/// Where the media is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaTarget {
    /// A `.iso` file, which can be attached to a virtual machine or burned.
    Iso(PathBuf),
    /// A physical USB device, by disk number.
    ///
    /// Writing one erases it completely, so a target of this kind cannot be
    /// acted on without a [`crate::EraseAgreement`].
    UsbDisk {
        /// Windows disk number.
        number: u32,
    },
}

impl MediaTarget {
    /// Whether acting on this target destroys what is already there.
    pub fn is_destructive(&self) -> bool {
        matches!(self, MediaTarget::UsbDisk { .. })
    }

    /// How to describe it to somebody about to press the button.
    pub fn describe(&self) -> String {
        match self {
            MediaTarget::Iso(path) => format!("an ISO file at {}", path.display()),
            MediaTarget::UsbDisk { number } => format!("USB disk {number}"),
        }
    }
}

/// Checks that an ISO path is somewhere a file can be written.
pub fn check_iso_path(path: &Path) -> Result<()> {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    if extension.as_deref() != Some("iso") {
        return Err(Error::new(
            ExitCode::Failure,
            "the recovery image has to be saved as an .iso file",
            format!("{} does not end in .iso", path.display()),
            "choose a file name ending in .iso",
        ));
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    match parent {
        Some(dir) if dir.is_dir() => Ok(()),
        Some(dir) => Err(Error::new(
            ExitCode::Failure,
            "the folder for the recovery image does not exist",
            format!("{} is not a folder", dir.display()),
            "choose a folder that exists",
        )),
        None => Err(Error::new(
            ExitCode::Failure,
            "the recovery image needs a full path",
            format!("{} has no folder in it", path.display()),
            "choose where to save it",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_with(files: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        for name in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"x").unwrap();
        }
        let root = dir.path().to_path_buf();
        (dir, root)
    }

    #[test]
    fn the_recovery_application_is_required() {
        let (_guard, root) = release_with(&["LICENSE"]);
        let err = payload_from_release(&root).unwrap_err();
        assert!(err.what().contains("recovery application"));
        assert!(err.why().contains(RECOVERY_EXE));
    }

    #[test]
    fn a_release_folder_becomes_a_payload() {
        let (_guard, root) = release_with(&[
            RECOVERY_EXE,
            "LICENSE",
            "NOTICE",
            "README.md",
            "docs/bare-metal-restore.md",
        ]);
        let payload = payload_from_release(&root).unwrap();

        assert_eq!(payload.files.len(), 5);
        // Everything lands under the payload folder, never loose in the image.
        for file in &payload.files {
            assert!(
                file.to.starts_with(PAYLOAD_DIR),
                "{:?} is not inside the payload folder",
                file.to
            );
        }
        assert_eq!(
            payload.files[0].to,
            Path::new(PAYLOAD_DIR).join(RECOVERY_EXE)
        );
    }

    /// Missing extras are not a failure. A disc with the program on it works.
    #[test]
    fn the_extras_are_optional() {
        let (_guard, root) = release_with(&[RECOVERY_EXE]);
        let payload = payload_from_release(&root).unwrap();
        assert_eq!(payload.files.len(), 1);
    }

    /// The startup script is the difference between a recovery disc that sees
    /// disks and one that does not, so its contents are pinned.
    #[test]
    fn the_startup_script_initialises_windows_pe_before_starting_anything() {
        let script = startnet_script();
        let wpeinit = script.find("wpeinit").expect("wpeinit has to be in it");
        let start = script
            .find(RECOVERY_EXE)
            .expect("the program has to be started");
        assert!(
            wpeinit < start,
            "wpeinit must run before the recovery application, or no disks are visible"
        );
    }

    #[test]
    fn the_startup_script_survives_a_missing_program() {
        let script = startnet_script();
        assert!(script.contains("if exist"));
        assert!(script.contains("missing from this media"));
    }

    /// Windows PE always calls itself X:, so the path must not depend on the
    /// letter the media itself got.
    #[test]
    fn the_startup_script_runs_from_the_boot_image_not_the_media() {
        let script = startnet_script();
        assert!(script.contains(&format!("X:\\{PAYLOAD_DIR}\\{RECOVERY_EXE}")));
        assert!(!script.contains("D:\\"));
        assert!(!script.contains("%~dp0"));
    }

    #[test]
    fn the_startup_script_uses_windows_line_endings() {
        // A script with bare newlines is read by cmd.exe as one long line.
        let script = startnet_script();
        assert!(script.contains("\r\n"));
        assert_eq!(script.matches('\n').count(), script.matches("\r\n").count());
    }

    #[test]
    fn an_iso_path_has_to_look_like_one() {
        let dir = tempfile::tempdir().unwrap();
        assert!(check_iso_path(&dir.path().join("recovery.iso")).is_ok());

        let err = check_iso_path(&dir.path().join("recovery.img")).unwrap_err();
        assert!(err.what().contains(".iso"));

        let err = check_iso_path(&dir.path().join("nowhere").join("r.iso")).unwrap_err();
        assert!(err.what().contains("does not exist"));

        let err = check_iso_path(Path::new("recovery.iso")).unwrap_err();
        assert!(err.what().contains("full path"));
    }

    #[test]
    fn only_a_usb_target_is_destructive() {
        let dir = tempfile::tempdir().unwrap();
        let iso = MediaTarget::Iso(dir.path().join("r.iso"));
        assert!(!iso.is_destructive());
        assert!(iso.describe().contains("ISO"));

        let usb = MediaTarget::UsbDisk { number: 3 };
        assert!(usb.is_destructive());
        assert!(usb.describe().contains('3'));
    }
}
