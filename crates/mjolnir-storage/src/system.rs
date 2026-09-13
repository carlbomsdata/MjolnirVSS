//! What this computer is, and which disk Windows boots from.
//!
//! Finding the system disk is the first thing a backup does, and getting it
//! wrong would mean backing up the wrong thing, so it is derived rather than
//! guessed: the Windows directory gives a volume, the volume gives its extents,
//! and the extents give a disk number.
//!
//! Everything read here is read only. MjolnirVSS never writes to the registry,
//! which is part of what makes it possible to run it from a USB stick and leave
//! nothing behind.

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::ids::{slugify, MachineId};
use mjolnir_image::manifest::{FirmwareMode, WindowsInfo};
use windows::core::PCWSTR;
use windows::Win32::Foundation::MAX_PATH;
use windows::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ,
    REG_VALUE_TYPE,
};
use windows::Win32::System::SystemInformation::{
    ComputerNamePhysicalDnsHostname, FirmwareTypeBios, FirmwareTypeUefi, GetComputerNameExW,
    GetFirmwareType, GetWindowsDirectoryW, FIRMWARE_TYPE,
};

use crate::device::{wide, wide_to_string};
use crate::volumes::{self, VolumeInfo};

/// A summary of the computer being backed up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemSummary {
    /// Computer name.
    pub computer_name: String,
    /// A stable identifier derived from the computer name and the system disk.
    pub machine_id: MachineId,
    /// What Windows reports about itself.
    pub windows: WindowsInfo,
    /// Whether the machine booted from UEFI firmware.
    pub firmware: FirmwareMode,
    /// The physical disk number Windows is installed on.
    pub system_disk_number: u32,
    /// The volume Windows is installed on.
    pub windows_volume: VolumeInfo,
}

/// The computer's name.
pub fn computer_name() -> String {
    let mut buffer = [0u16; 256];
    let mut size = buffer.len() as u32;
    // SAFETY: the buffer is valid for `size` units for the duration of the
    // call, and `size` is a valid writable u32.
    let ok = unsafe {
        GetComputerNameExW(
            ComputerNamePhysicalDnsHostname,
            Some(windows::core::PWSTR(buffer.as_mut_ptr())),
            &mut size,
        )
    };
    if ok.is_err() {
        return "PC".to_owned();
    }
    let name = wide_to_string(&buffer);
    if name.is_empty() {
        "PC".to_owned()
    } else {
        name
    }
}

/// Whether the machine booted from UEFI firmware.
pub fn firmware_mode() -> FirmwareMode {
    let mut kind = FIRMWARE_TYPE::default();
    // SAFETY: `kind` is a live local of exactly the type the call writes,
    // and it is only read when the call reported success; on failure it keeps
    // the default it was initialised with, which maps to Unknown.
    let ok = unsafe { GetFirmwareType(&mut kind) };
    if ok.is_err() {
        return FirmwareMode::Unknown;
    }
    if kind == FirmwareTypeUefi {
        FirmwareMode::Uefi
    } else if kind == FirmwareTypeBios {
        FirmwareMode::Bios
    } else {
        FirmwareMode::Unknown
    }
}

/// Reads what Windows says about itself.
///
/// Read only registry access. Every field is optional, because a missing value
/// is not a reason to refuse a backup.
pub fn windows_info() -> WindowsInfo {
    let key = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let build = read_registry_string(key, "CurrentBuild");
    WindowsInfo {
        product_name: corrected_product_name(
            read_registry_string(key, "ProductName"),
            build.as_deref(),
        ),
        build,
        edition: read_registry_string(key, "EditionID"),
        architecture: Some(
            if cfg!(target_arch = "x86_64") {
                "x64"
            } else {
                "unknown"
            }
            .to_owned(),
        ),
    }
}

/// The build number at which Windows 11 begins.
const FIRST_WINDOWS_11_BUILD: u32 = 22000;

/// Windows 11 still calls itself Windows 10 in the registry.
///
/// `ProductName` under `CurrentVersion` was never updated when Windows 11
/// shipped: a Windows 11 machine reports `Windows 10 Pro`. Microsoft's guidance
/// is to go by the build number instead, and 22000 is where Windows 11 starts.
///
/// This matters more here than it would in a status line. The name is written
/// into every manifest, so without it a backup of a Windows 11 machine would
/// say for ever that it came from Windows 10, and somebody reading that backup
/// years later has no way to tell it was wrong.
///
/// The `Windows 10` test is what keeps Server out of it. Windows Server 2025 is
/// build 26100, the same build as Windows 11 24H2, but calls itself
/// `Windows Server 2025 Standard` and so is left exactly as it is.
fn corrected_product_name(product_name: Option<String>, build: Option<&str>) -> Option<String> {
    let name = product_name?;
    let build_number: u32 = build
        .and_then(|b| b.trim().parse().ok())
        .unwrap_or_default();
    if build_number >= FIRST_WINDOWS_11_BUILD && name.contains("Windows 10") {
        Some(name.replace("Windows 10", "Windows 11"))
    } else {
        Some(name)
    }
}

fn read_registry_string(subkey: &str, value: &str) -> Option<String> {
    let subkey_w = wide(subkey);
    let value_w = wide(value);
    let mut key = HKEY::default();

    // SAFETY: the subkey is a null terminated wide string that outlives the
    // call, and `key` is a valid writable HKEY. The key is closed below on
    // every path.
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey_w.as_ptr()),
            Some(0),
            KEY_READ,
            &mut key,
        )
    };
    if opened.is_err() {
        return None;
    }

    let mut kind = REG_VALUE_TYPE::default();
    let mut buffer = [0u8; 512];
    let mut size = buffer.len() as u32;

    // SAFETY: the value name outlives the call, the buffer is valid for `size`
    // bytes, and both out pointers are valid.
    let queried = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(value_w.as_ptr()),
            None,
            Some(&mut kind),
            Some(buffer.as_mut_ptr()),
            Some(&mut size),
        )
    };
    // SAFETY: `key` came from a RegOpenKeyExW that returned success, and is
    // closed here on both the success and failure paths of the query above,
    // exactly once, with no use afterwards.
    unsafe {
        let _ = RegCloseKey(key);
    }

    if queried.is_err() || kind != REG_SZ {
        return None;
    }

    // The value is a wide string, so the byte count has to be halved and any
    // odd trailing byte ignored rather than trusted.
    let units = (size as usize) / 2;
    let mut wide_buffer = Vec::with_capacity(units);
    for i in 0..units {
        wide_buffer.push(u16::from_le_bytes([buffer[i * 2], buffer[i * 2 + 1]]));
    }
    let text = wide_to_string(&wide_buffer);
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// The directory Windows is installed in, for example `C:\WINDOWS`.
pub fn windows_directory() -> Result<String> {
    let mut buffer = [0u16; MAX_PATH as usize + 1];
    // SAFETY: the buffer is valid for its full length for the duration of the
    // call.
    let len = unsafe { GetWindowsDirectoryW(Some(&mut buffer)) };
    if len == 0 {
        return Err(Error::new(
            ExitCode::Failure,
            "the Windows folder could not be located",
            "Windows did not report where it is installed, which MjolnirVSS needs in order to know which disk to back up",
            "restart the computer and try again",
        ));
    }
    Ok(wide_to_string(&buffer))
}

/// The volume GUID path a drive letter or path resolves to.
pub fn volume_for_path(path: &str) -> Result<String> {
    // The call needs a trailing backslash, and a path like `C:\WINDOWS` has to
    // be reduced to its root first.
    let root = if path.len() >= 2 && path.as_bytes()[1] == b':' {
        format!("{}:\\", &path[..1])
    } else {
        format!("{}\\", path.trim_end_matches('\\'))
    };
    let root_w = wide(&root);
    let mut buffer = [0u16; MAX_PATH as usize + 1];

    // SAFETY: the root path outlives the call and the buffer is valid for its
    // full length.
    unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(root_w.as_ptr()), &mut buffer) }.map_err(
        |e| {
            Error::new(
                ExitCode::Failure,
                format!("the volume holding {root} could not be identified"),
                format!("Windows reported: {e}"),
                "restart the computer and try again",
            )
        },
    )?;

    Ok(wide_to_string(&buffer))
}

/// Works out what this computer is and which disk it boots from.
pub fn describe_system() -> Result<SystemSummary> {
    let windows_dir = windows_directory()?;
    let volume_path = volume_for_path(&windows_dir)?;

    let windows_volume = volumes::describe_volume(&volume_path).ok_or_else(|| {
        Error::new(
            ExitCode::Failure,
            "the volume Windows is installed on could not be inspected",
            format!("Windows reports it is installed on {volume_path}, but that volume did not answer any queries"),
            "restart the computer and try again",
        )
    })?;

    if windows_volume.extents.is_empty() {
        return Err(Error::unsupported(
            "the volume Windows is installed on does not map onto a physical disk",
            "Windows did not report which disk the volume occupies, which happens with network locations and some virtual storage",
            "this version of MjolnirVSS supports a Windows installation on an ordinary local disk only",
        ));
    }

    if !windows_volume.is_simple() {
        let disks: Vec<String> = windows_volume
            .extents
            .iter()
            .map(|e| e.disk_number.to_string())
            .collect();
        return Err(Error::unsupported(
            "the volume Windows is installed on spans more than one region",
            format!(
                "it occupies {} separate runs across disk(s) {}, which means it is a spanned, striped, mirrored or Storage Spaces volume",
                windows_volume.extents.len(),
                disks.join(", ")
            ),
            "this version of MjolnirVSS supports a Windows installation on a single ordinary disk; dynamic disks and Storage Spaces are not supported yet",
        ));
    }

    let system_disk_number = windows_volume.extents[0].disk_number;
    let computer_name = computer_name();
    let machine_id = machine_id_for(&computer_name, system_disk_number)?;

    Ok(SystemSummary {
        computer_name,
        machine_id,
        windows: windows_info(),
        firmware: firmware_mode(),
        system_disk_number,
        windows_volume,
    })
}

/// Builds a stable identifier for this machine.
///
/// Derived from the computer name and the system disk's GPT disk GUID, so it
/// stays the same across reboots and Windows updates but changes if the backup
/// is taken from a different computer. Nothing is written anywhere to remember
/// it, which is what keeps the tool free of installed state.
pub fn machine_id_for(computer_name: &str, disk_number: u32) -> Result<MachineId> {
    let disk = crate::disks::describe_disk(disk_number)?;
    let fingerprint = disk
        .disk_guid
        .clone()
        .or_else(|| disk.serial.clone())
        .unwrap_or_else(|| format!("disk-{disk_number}"));

    // A short digest keeps the identifier readable while still separating two
    // machines that happen to share a name.
    let digest = mjolnir_image::ChunkHash::of(fingerprint.as_bytes()).to_hex();
    let short = &digest[..12];

    let mut slug = slugify(computer_name);
    if slug.is_empty() {
        slug.push_str("pc");
    }
    slug.truncate(40);
    let slug = slug.trim_end_matches('-').to_owned();

    MachineId::new(format!("{slug}-{short}")).map_err(|e| {
        Error::new(
            ExitCode::Failure,
            "a stable identifier for this computer could not be built",
            format!("the computer name {computer_name:?} produced an invalid identifier: {e}"),
            "this is an internal error; please report it with the computer name",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_computer_name_is_never_empty() {
        assert!(!computer_name().is_empty());
    }

    #[test]
    fn the_firmware_mode_is_reported() {
        // Cannot assert which mode, since a virtual machine may be either, but
        // the call must not fail or panic.
        let mode = firmware_mode();
        assert!(matches!(
            mode,
            FirmwareMode::Uefi | FirmwareMode::Bios | FirmwareMode::Unknown
        ));
    }

    #[test]
    fn windows_reports_something_about_itself() {
        let info = windows_info();
        // Every field is optional by design, but on a real Windows machine the
        // build number is always there.
        assert!(
            info.build.is_some(),
            "no build number in the registry: {info:?}"
        );
        assert_eq!(info.architecture.as_deref(), Some("x64"));
    }

    #[test]
    fn the_windows_directory_looks_like_a_path() {
        let dir = windows_directory().unwrap();
        assert!(dir.len() > 3, "{dir}");
        assert_eq!(&dir[1..2], ":", "{dir}");
    }

    #[test]
    fn the_windows_volume_resolves_to_a_guid_path() {
        let dir = windows_directory().unwrap();
        let volume = volume_for_path(&dir).unwrap();
        assert!(volume.starts_with("\\\\?\\Volume{"), "{volume}");
        assert!(volume.ends_with('\\'), "{volume}");
    }

    #[test]
    fn machine_ids_are_stable_and_valid() {
        let a = machine_id_for("DESKTOP-1A2B", 0).unwrap();
        let b = machine_id_for("DESKTOP-1A2B", 0).unwrap();
        assert_eq!(a, b, "the identifier must not change between calls");
        assert!(a.as_str().starts_with("desktop-1a2b-"), "{a}");
        // A different computer name must produce a different identifier.
        let c = machine_id_for("OTHER-PC", 0).unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn machine_ids_survive_an_awkward_computer_name() {
        for name in [
            "",
            "...",
            "a-very-long-computer-name-that-goes-on-and-on-and-on-forever",
        ] {
            let id = machine_id_for(name, 0).unwrap();
            assert!(mjolnir_core::ids::validate_id(id.as_str()).is_ok(), "{id}");
        }
    }

    /// The whole discovery path, run against this machine.
    #[test]
    fn the_system_disk_is_identified() {
        let summary = describe_system().expect("this machine should be describable");
        assert!(!summary.computer_name.is_empty());
        assert!(summary.windows_volume.is_simple());
        assert_eq!(
            summary.windows_volume.disk_number(),
            Some(summary.system_disk_number)
        );
        assert_eq!(
            summary.windows_volume.filesystem.as_deref(),
            Some("NTFS"),
            "the Windows volume should be NTFS"
        );
    }
    /// The bug this exists for: a real Windows 11 24H2 test machine reported
    /// itself as "Windows 10 Pro (build 26100)" in every backup it took.
    #[test]
    fn windows_11_is_not_called_windows_10() {
        assert_eq!(
            corrected_product_name(Some("Windows 10 Pro".to_owned()), Some("26100")),
            Some("Windows 11 Pro".to_owned())
        );
        assert_eq!(
            corrected_product_name(Some("Windows 10 Home".to_owned()), Some("22000")),
            Some("Windows 11 Home".to_owned())
        );
    }

    /// A real Windows 10 is left alone. 19045 is 22H2, the last of them.
    #[test]
    fn windows_10_is_still_called_windows_10() {
        assert_eq!(
            corrected_product_name(Some("Windows 10 Pro".to_owned()), Some("19045")),
            Some("Windows 10 Pro".to_owned())
        );
    }

    /// Server 2025 shares build 26100 with Windows 11 24H2 and must not be
    /// renamed. This is why the correction looks at the name and not only the
    /// number.
    #[test]
    fn windows_server_is_never_renamed() {
        for (name, build) in [
            ("Windows Server 2025 Standard", "26100"),
            ("Windows Server 2022 Datacenter", "20348"),
            ("Windows Server 2019 Standard", "17763"),
        ] {
            assert_eq!(
                corrected_product_name(Some(name.to_owned()), Some(build)),
                Some(name.to_owned()),
                "{name} must be left alone"
            );
        }
    }

    /// A missing or unreadable build number must not invent a version.
    #[test]
    fn an_unknown_build_changes_nothing() {
        for build in [None, Some(""), Some("not a number")] {
            assert_eq!(
                corrected_product_name(Some("Windows 10 Pro".to_owned()), build),
                Some("Windows 10 Pro".to_owned())
            );
        }
        assert_eq!(corrected_product_name(None, Some("26100")), None);
    }
}
