//! Finding the Windows recovery components already on this computer.
//!
//! # Why nothing is downloaded and nothing is shipped
//!
//! Windows PE is Microsoft's, and its licence does not let anyone else
//! redistribute it. MjolnirVSS therefore ships none of it. What it does instead
//! is use what the computer already has a licence for:
//!
//! * **The Windows ADK's Windows PE add-on**, if it is installed. This is the
//!   supported way to build recovery media, and it is what Microsoft's own
//!   `copype` and `MakeWinPEMedia` scripts use.
//! * **The recovery image this Windows installation already carries**, which is
//!   `winre.wim` in the recovery partition. Every Windows installation has one,
//!   it is the image the machine boots when you ask for advanced startup, and
//!   it is licensed to the machine it came from.
//!
//! Neither is copied into the repository, into a release, or anywhere except
//! the recovery media being made on the machine that owns it.

use std::path::{Path, PathBuf};

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;

/// Where the bootable part of the recovery media comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// The Windows Assessment and Deployment Kit's Windows PE add-on.
    ///
    /// The best source: it carries the boot files, the media layout and the
    /// tools to make an ISO, all designed for this.
    Adk,
    /// The recovery image belonging to this Windows installation.
    ///
    /// Always present, but carries no media layout and no ISO builder, so a USB
    /// stick can be made from it and an ISO cannot.
    LocalRecovery,
}

impl SourceKind {
    /// How to describe it to somebody choosing.
    pub const fn describe(self) -> &'static str {
        match self {
            SourceKind::Adk => "the Windows Assessment and Deployment Kit",
            SourceKind::LocalRecovery => "this computer's own recovery image",
        }
    }
}

/// A usable set of Windows recovery components found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaSource {
    /// Which kind it is.
    pub kind: SourceKind,
    /// The boot image to customise, a `.wim`.
    pub boot_image: PathBuf,
    /// A folder holding the bootable media layout, when the source has one.
    ///
    /// The ADK provides `Media`, holding `bootmgr`, `EFI\Boot\bootx64.efi` and
    /// the rest. A local recovery image does not, which is why it cannot
    /// produce an ISO.
    pub media_template: Option<PathBuf>,
    /// `oscdimg.exe`, when it is available.
    pub oscdimg: Option<PathBuf>,
    /// The El Torito boot sector for BIOS booting.
    pub etfsboot: Option<PathBuf>,
    /// The EFI boot image, the one that does not ask for a key press.
    pub efisys: Option<PathBuf>,
}

impl MediaSource {
    /// Whether this source can produce an ISO file.
    pub fn can_make_iso(&self) -> bool {
        self.media_template.is_some()
            && self.oscdimg.is_some()
            && self.etfsboot.is_some()
            && self.efisys.is_some()
    }

    /// Whether this source can produce a bootable USB stick.
    ///
    /// A USB stick needs the media layout too: the firmware loads `bootmgfw.efi`
    /// from the stick's own filesystem, not from inside the image.
    pub fn can_make_usb(&self) -> bool {
        self.media_template.is_some()
    }

    /// Why this source cannot do something, in words.
    pub fn explain_limits(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.media_template.is_none() {
            out.push(
                "This source has the recovery image but not the files that make a disc or a drive bootable."
                    .to_owned(),
            );
        }
        if self.oscdimg.is_none() {
            out.push(
                "Building an ISO needs oscdimg.exe, which comes with the Windows Assessment and Deployment Kit. Microsoft does not allow it to be shipped with anything else."
                    .to_owned(),
            );
        }
        out
    }
}

/// The folders the ADK installs into, newest first.
const ADK_ROOTS: [&str; 2] = [
    r"C:\Program Files (x86)\Windows Kits\10\Assessment and Deployment Kit",
    r"C:\Program Files\Windows Kits\10\Assessment and Deployment Kit",
];

/// Looks for the ADK's Windows PE add-on.
pub fn find_adk() -> Option<MediaSource> {
    for root in ADK_ROOTS {
        let root = Path::new(root);
        let pe = root.join(r"Windows Preinstallation Environment\amd64");
        let boot_image = pe.join(r"en-us\winpe.wim");
        let media = pe.join("Media");
        if !boot_image.is_file() || !media.is_dir() {
            continue;
        }

        let oscdimg_dir = root.join(r"Deployment Tools\amd64\Oscdimg");
        let oscdimg = oscdimg_dir.join("oscdimg.exe");
        let etfsboot = oscdimg_dir.join("etfsboot.com");
        // The "noprompt" variant is the one that boots without asking for a key
        // press, which a recovery disc should do.
        let efisys = oscdimg_dir.join("efisys_noprompt.bin");
        let efisys_fallback = oscdimg_dir.join("efisys.bin");

        return Some(MediaSource {
            kind: SourceKind::Adk,
            boot_image,
            media_template: Some(media),
            oscdimg: oscdimg.is_file().then_some(oscdimg),
            etfsboot: etfsboot.is_file().then_some(etfsboot),
            efisys: if efisys.is_file() {
                Some(efisys)
            } else {
                efisys_fallback.is_file().then_some(efisys_fallback)
            },
        });
    }
    None
}

/// Asks Windows where this installation keeps its recovery image.
///
/// Uses `reagentc /info`, which is the documented way to ask, and parses the
/// path it prints. The path is a device path into the recovery partition, which
/// only a process with administrator rights can open.
#[cfg(windows)]
pub fn find_local_recovery() -> Option<MediaSource> {
    let output = std::process::Command::new("reagentc.exe")
        .arg("/info")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let location = parse_recovery_location(&text)?;
    let wim = Path::new(&location).join("winre.wim");

    // The path is a GLOBALROOT device path, so `is_file` is the only way to
    // find out whether it is really there.
    if !wim.is_file() {
        return None;
    }
    Some(MediaSource {
        kind: SourceKind::LocalRecovery,
        boot_image: wim,
        media_template: None,
        oscdimg: None,
        etfsboot: None,
        efisys: None,
    })
}

/// Not available away from Windows.
#[cfg(not(windows))]
pub fn find_local_recovery() -> Option<MediaSource> {
    None
}

/// Pulls the recovery environment's location out of `reagentc /info` output.
///
/// The line looks like:
///
/// ```text
/// Windows RE location:       \\?\GLOBALROOT\device\harddisk0\partition4\Recovery\WindowsRE
/// ```
///
/// An empty value means the recovery environment is disabled, which is a real
/// answer and produces `None` rather than a path to nothing.
pub fn parse_recovery_location(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        // Matched case insensitively and by prefix, because the wording is
        // localised but the shape is not.
        let lower = line.to_ascii_lowercase();
        if !lower.starts_with("windows re location") {
            continue;
        }
        let (_, value) = line.split_once(':')?;
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        return Some(value.to_owned());
    }
    None
}

/// Every source this computer can offer, best first.
pub fn find_sources() -> Vec<MediaSource> {
    let mut out = Vec::new();
    if let Some(adk) = find_adk() {
        out.push(adk);
    }
    if let Some(local) = find_local_recovery() {
        out.push(local);
    }
    out
}

/// The best available source, or an error explaining what is missing.
pub fn best_source() -> Result<MediaSource> {
    find_sources().into_iter().next().ok_or_else(|| {
        Error::new(
            ExitCode::Unsupported,
            "no Windows recovery components were found on this computer",
            "MjolnirVSS builds recovery media from parts Windows already has, because Microsoft does not allow its recovery environment to be redistributed. Neither the Windows Assessment and Deployment Kit nor this computer's own recovery image could be found",
            "either install the Windows ADK with the Windows PE add-on, or turn the Windows Recovery Environment back on with 'reagentc /enable' and try again",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recovery_location_is_read_from_reagentc_output() {
        let text = "\
Windows Recovery Environment (Windows RE) and system reset configuration
Information:

    Windows RE status:         Enabled
    Windows RE location:       \\\\?\\GLOBALROOT\\device\\harddisk0\\partition4\\Recovery\\WindowsRE
    Boot Configuration Data (BCD) identifier: f436d6a6-b967-11ef-9028-e9dc073b8773
    Recovery image location:
    Recovery image index:      0

REAGENTC.EXE: Operation Successful.
";
        assert_eq!(
            parse_recovery_location(text).as_deref(),
            Some("\\\\?\\GLOBALROOT\\device\\harddisk0\\partition4\\Recovery\\WindowsRE")
        );
    }

    /// A disabled recovery environment prints the heading with nothing after
    /// it. That is an answer, and it must not become a path to a folder that
    /// does not exist.
    #[test]
    fn a_disabled_recovery_environment_yields_nothing() {
        let text = "\
    Windows RE status:         Disabled
    Windows RE location:
    Recovery image location:
";
        assert_eq!(parse_recovery_location(text), None);
    }

    #[test]
    fn output_without_the_line_yields_nothing() {
        assert_eq!(parse_recovery_location(""), None);
        assert_eq!(
            parse_recovery_location("REAGENTC.EXE: Operation failed: 5"),
            None
        );
    }

    /// The recovery image location line also contains a colon inside the path
    /// on some systems. Only the first one separates the label from the value.
    #[test]
    fn a_path_containing_a_colon_survives() {
        let text = "    Windows RE location:       D:\\Recovery\\WindowsRE";
        assert_eq!(
            parse_recovery_location(text).as_deref(),
            Some("D:\\Recovery\\WindowsRE")
        );
    }

    fn adk_like() -> MediaSource {
        MediaSource {
            kind: SourceKind::Adk,
            boot_image: PathBuf::from("winpe.wim"),
            media_template: Some(PathBuf::from("Media")),
            oscdimg: Some(PathBuf::from("oscdimg.exe")),
            etfsboot: Some(PathBuf::from("etfsboot.com")),
            efisys: Some(PathBuf::from("efisys_noprompt.bin")),
        }
    }

    #[test]
    fn a_complete_adk_source_can_do_both() {
        let source = adk_like();
        assert!(source.can_make_iso());
        assert!(source.can_make_usb());
        assert!(source.explain_limits().is_empty());
    }

    /// The local recovery image is enough for a USB stick only if the media
    /// files are found too, and never enough for an ISO.
    #[test]
    fn the_local_recovery_image_alone_cannot_make_an_iso() {
        let source = MediaSource {
            kind: SourceKind::LocalRecovery,
            boot_image: PathBuf::from("winre.wim"),
            media_template: None,
            oscdimg: None,
            etfsboot: None,
            efisys: None,
        };
        assert!(!source.can_make_iso());
        assert!(!source.can_make_usb());

        let limits = source.explain_limits();
        assert_eq!(limits.len(), 2);
        assert!(limits.iter().any(|l| l.contains("oscdimg")));
        // The reason has to say why rather than just refusing.
        assert!(limits
            .iter()
            .any(|l| l.contains("does not allow it to be shipped")));
    }

    #[test]
    fn an_adk_without_oscdimg_can_still_make_a_usb_stick() {
        let source = MediaSource {
            oscdimg: None,
            ..adk_like()
        };
        assert!(!source.can_make_iso());
        assert!(source.can_make_usb());
    }

    #[test]
    fn every_source_kind_describes_itself() {
        for kind in [SourceKind::Adk, SourceKind::LocalRecovery] {
            assert!(!kind.describe().is_empty());
        }
    }

    /// Runs against whatever the machine has. Asserts only that whatever is
    /// found is internally consistent, because a developer machine and a user's
    /// machine have different things installed.
    #[test]
    fn whatever_this_machine_has_is_consistent() {
        for source in find_sources() {
            assert!(
                source.boot_image.is_file(),
                "{:?} was offered but its boot image is not there",
                source.kind
            );
            if source.can_make_iso() {
                assert!(source.oscdimg.as_ref().unwrap().is_file());
                assert!(source.etfsboot.as_ref().unwrap().is_file());
                assert!(source.efisys.as_ref().unwrap().is_file());
            }
        }
    }
}
