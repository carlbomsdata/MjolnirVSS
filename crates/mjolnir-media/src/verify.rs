//! Checking that recovery media is actually going to boot and be useful.
//!
//! Recovery media is only ever used on the worst day, so it is checked on the
//! day it is made. The checks are the ones that would actually stop it working:
//! the file exists and is a plausible size, it carries the boot loaders the
//! firmware looks for, and the recovery application is inside the boot image.
//!
//! What cannot be checked without booting is said plainly rather than implied.

use std::path::{Path, PathBuf};

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;

/// Smallest believable size for recovery media, in bytes.
///
/// A Windows PE image is about a quarter of a gibibyte before anything is added
/// to it. Anything much smaller than a tenth of that did not get built.
pub const MINIMUM_PLAUSIBLE_BYTES: u64 = 32 * 1024 * 1024;

/// One thing that was checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// What was checked, in words.
    pub what: String,
    /// Whether it passed.
    pub passed: bool,
    /// What was found.
    pub detail: String,
}

/// What checking found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaReport {
    /// The media that was checked.
    pub path: PathBuf,
    /// Every check, in the order they were made.
    pub checks: Vec<Check>,
    /// What was not checked, and why.
    pub not_checked: Vec<String>,
}

impl MediaReport {
    /// Whether every check passed.
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }

    /// The checks that failed.
    pub fn failures(&self) -> Vec<&Check> {
        self.checks.iter().filter(|c| !c.passed).collect()
    }

    /// A report an operator reads.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("Recovery media: {}\n", self.path.display()));
        for check in &self.checks {
            out.push_str(&format!(
                "  [{}] {}: {}\n",
                if check.passed { "ok" } else { "FAILED" },
                check.what,
                check.detail
            ));
        }
        if !self.not_checked.is_empty() {
            out.push_str("\nNot checked:\n");
            for line in &self.not_checked {
                out.push_str(&format!("  {line}\n"));
            }
        }
        out
    }
}

/// The files a UEFI machine's firmware looks for on bootable media.
///
/// `bootmgr` is for the older BIOS path and `EFI\Boot\bootx64.efi` is the one
/// UEFI loads. Media missing the second one boots on nothing modern.
pub const REQUIRED_MEDIA_FILES: [&str; 3] = [
    r"EFI\Boot\bootx64.efi",
    r"EFI\Microsoft\Boot\bootmgfw.efi",
    r"sources\boot.wim",
];

/// Checks a folder that is about to be turned into media, or that was written
/// to a USB stick.
///
/// This is the deep check: everything is visible as a file, so everything can
/// be looked at.
pub fn check_media_folder(root: &Path) -> MediaReport {
    let mut checks = Vec::new();

    for relative in REQUIRED_MEDIA_FILES {
        let path = root.join(relative);
        let found = path.is_file();
        let size = if found {
            std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
        checks.push(Check {
            what: format!("{relative} is present"),
            passed: found && size > 0,
            detail: if found {
                format!("{size} bytes")
            } else {
                "not found".to_owned()
            },
        });
    }

    let boot_wim = root.join(r"sources\boot.wim");
    if let Ok(meta) = std::fs::metadata(&boot_wim) {
        checks.push(Check {
            what: "the recovery image is a believable size".to_owned(),
            passed: meta.len() >= MINIMUM_PLAUSIBLE_BYTES,
            detail: format!("{} bytes", meta.len()),
        });
    }

    MediaReport {
        path: root.to_path_buf(),
        checks,
        not_checked: vec![
            "Whether the computer's firmware will boot it. Only booting it proves that.".to_owned(),
        ],
    }
}

/// Checks a finished ISO file.
///
/// An ISO cannot be looked inside without mounting it, which needs rights and
/// changes the machine's drive letters, so this checks what can be checked from
/// the outside and says what it did not check.
pub fn check_iso(path: &Path) -> Result<MediaReport> {
    let meta = std::fs::metadata(path).map_err(|e| {
        Error::new(
            ExitCode::Io,
            "the recovery image could not be read back",
            format!("{}: {e}", path.display()),
            "check the drive is still connected",
        )
    })?;

    let mut checks = vec![Check {
        what: "the file exists".to_owned(),
        passed: meta.is_file(),
        detail: format!("{} bytes", meta.len()),
    }];

    checks.push(Check {
        what: "the file is a believable size".to_owned(),
        passed: meta.len() >= MINIMUM_PLAUSIBLE_BYTES,
        detail: format!(
            "{} (at least {} expected)",
            mjolnir_core::progress::format_bytes(meta.len()),
            mjolnir_core::progress::format_bytes(MINIMUM_PLAUSIBLE_BYTES)
        ),
    });

    // The ISO 9660 primary volume descriptor sits at 0x8000 and begins with the
    // type byte 1 and the string CD001. Finding it proves a filesystem was
    // written rather than an empty file of the right size.
    let descriptor = read_at(path, 0x8000, 6)?;
    let looks_like_iso =
        descriptor.len() == 6 && descriptor[0] == 1 && &descriptor[1..6] == b"CD001";
    checks.push(Check {
        what: "the file is an ISO 9660 image".to_owned(),
        passed: looks_like_iso,
        detail: if looks_like_iso {
            "the volume descriptor is where it should be".to_owned()
        } else {
            format!("expected 01 'CD001' at offset 0x8000, found {descriptor:02x?}")
        },
    });

    // The El Torito boot record is the next descriptor, and it is what makes
    // the image bootable rather than merely readable.
    let boot_record = read_at(path, 0x8800, 39)?;
    let is_bootable = boot_record.len() == 39
        && boot_record[0] == 0
        && &boot_record[1..6] == b"CD001"
        && &boot_record[7..30] == b"EL TORITO SPECIFICATION";
    checks.push(Check {
        what: "the image is marked bootable".to_owned(),
        passed: is_bootable,
        detail: if is_bootable {
            "the El Torito boot record is present".to_owned()
        } else {
            "no El Torito boot record was found at offset 0x8800".to_owned()
        },
    });

    Ok(MediaReport {
        path: path.to_path_buf(),
        checks,
        not_checked: vec![
            "What is inside the image. Looking would mean mounting it, which changes this computer's drive letters.".to_owned(),
            "Whether a computer's firmware will boot it. Only booting it proves that.".to_owned(),
        ],
    })
}

/// Reads `length` bytes at `offset`, returning fewer if the file ends first.
fn read_at(path: &Path, offset: u64, length: usize) -> Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).map_err(|e| {
        Error::new(
            ExitCode::Io,
            "the recovery image could not be read back",
            format!("{}: {e}", path.display()),
            "check the drive is still connected",
        )
    })?;
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return Ok(Vec::new());
    }
    let mut buffer = vec![0u8; length];
    let mut filled = 0usize;
    while filled < length {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    buffer.truncate(filled);
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a file that looks like a bootable ISO from the outside.
    fn fake_iso(dir: &Path, bootable: bool, size: u64) -> PathBuf {
        let path = dir.join("fake.iso");
        let mut bytes = vec![0u8; size as usize];

        if size as usize > 0x8006 {
            bytes[0x8000] = 1;
            bytes[0x8001..0x8006].copy_from_slice(b"CD001");
        }
        if bootable && size as usize > 0x8800 + 39 {
            bytes[0x8800] = 0;
            bytes[0x8801..0x8806].copy_from_slice(b"CD001");
            bytes[0x8807..0x8807 + 23].copy_from_slice(b"EL TORITO SPECIFICATION");
        }
        std::fs::write(&path, &bytes).unwrap();
        path
    }

    #[test]
    fn a_well_formed_bootable_image_passes() {
        let dir = tempfile::tempdir().unwrap();
        let iso = fake_iso(dir.path(), true, MINIMUM_PLAUSIBLE_BYTES + 1024);
        let report = check_iso(&iso).unwrap();
        assert!(report.passed(), "{}", report.describe());
        assert_eq!(report.checks.len(), 4);
    }

    /// An image that is not bootable is the failure that matters most: it looks
    /// perfectly fine until the day somebody needs it.
    #[test]
    fn an_image_without_a_boot_record_fails() {
        let dir = tempfile::tempdir().unwrap();
        let iso = fake_iso(dir.path(), false, MINIMUM_PLAUSIBLE_BYTES + 1024);
        let report = check_iso(&iso).unwrap();

        assert!(!report.passed());
        let failures = report.failures();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].what.contains("bootable"));
    }

    #[test]
    fn a_file_that_is_not_an_iso_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not.iso");
        std::fs::write(&path, vec![0u8; MINIMUM_PLAUSIBLE_BYTES as usize + 16]).unwrap();

        let report = check_iso(&path).unwrap();
        assert!(!report.passed());
        assert!(report
            .failures()
            .iter()
            .any(|c| c.what.contains("ISO 9660")));
    }

    #[test]
    fn an_implausibly_small_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let iso = fake_iso(dir.path(), true, 1024 * 1024);
        let report = check_iso(&iso).unwrap();
        assert!(!report.passed());
        assert!(report
            .failures()
            .iter()
            .any(|c| c.what.contains("believable size")));
    }

    #[test]
    fn a_missing_file_is_an_error_rather_than_a_failed_check() {
        let dir = tempfile::tempdir().unwrap();
        let err = check_iso(&dir.path().join("absent.iso")).unwrap_err();
        assert_eq!(err.exit(), ExitCode::Io);
    }

    #[test]
    fn a_media_folder_is_checked_for_the_files_firmware_looks_for() {
        let dir = tempfile::tempdir().unwrap();
        for relative in REQUIRED_MEDIA_FILES {
            let path = dir.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, vec![0u8; 64]).unwrap();
        }
        // Make the boot image a believable size.
        std::fs::write(
            dir.path().join(r"sources\boot.wim"),
            vec![0u8; MINIMUM_PLAUSIBLE_BYTES as usize + 1],
        )
        .unwrap();

        let report = check_media_folder(dir.path());
        assert!(report.passed(), "{}", report.describe());
    }

    #[test]
    fn a_media_folder_missing_the_uefi_loader_fails() {
        let dir = tempfile::tempdir().unwrap();
        // Everything except the file UEFI firmware actually loads.
        for relative in REQUIRED_MEDIA_FILES.iter().skip(1) {
            let path = dir.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, vec![0u8; MINIMUM_PLAUSIBLE_BYTES as usize + 1]).unwrap();
        }

        let report = check_media_folder(dir.path());
        assert!(!report.passed());
        assert!(report
            .failures()
            .iter()
            .any(|c| c.what.contains("bootx64.efi")));
    }

    /// A report has to say what it did not check, so nobody reads a pass as a
    /// promise that the media boots.
    #[test]
    fn the_report_says_what_it_did_not_check() {
        let dir = tempfile::tempdir().unwrap();
        let iso = fake_iso(dir.path(), true, MINIMUM_PLAUSIBLE_BYTES + 1024);
        let report = check_iso(&iso).unwrap();

        assert!(!report.not_checked.is_empty());
        assert!(report
            .not_checked
            .iter()
            .any(|l| l.contains("Only booting it proves that")));
        assert!(report.describe().contains("Not checked"));
    }
}
