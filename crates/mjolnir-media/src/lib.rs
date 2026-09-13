//! Making bootable recovery media.
//!
//! The one thing a backup tool must not need in order to recover a computer is
//! that computer. This crate builds the media you start the broken machine
//! from: a Windows PE environment with `MjolnirVSS.Restore.exe` inside it, made
//! from Windows components the machine already has a licence for.
//!
//! # What is not here
//!
//! **No Microsoft files.** Windows PE cannot be redistributed, so MjolnirVSS
//! ships none of it and downloads none of it. It uses the Windows ADK's Windows
//! PE add-on if that is installed, or the recovery image the computer already
//! carries. See [`source`] for how both are found.
//!
//! **No physical disk writing without being told twice.** Making a USB stick
//! erases it, so it needs an [`EraseAgreement`], which can only be made by
//! typing the device's own serial number. The same shape of refusal the restore
//! side uses, for the same reason.
//!
//! Copyright (C) the MjolnirVSS contributors.
//! Licensed under the GNU General Public License, version 3 or later.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod build;
pub mod layout;
pub mod source;
pub mod verify;

pub use build::{build_iso, MediaOutcome};
pub use layout::{payload_from_release, MediaTarget, Payload, PayloadFile};
pub use source::{best_source, find_sources, MediaSource, SourceKind};
pub use verify::{check_iso, check_media_folder, MediaReport};

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;

/// Proof that somebody agreed to erase a particular USB device.
///
/// The only way to make one is to type the device's serial number, so it cannot
/// be produced by a mistyped command or a stray click. Writing to a device
/// without one is not possible, because the function that writes takes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EraseAgreement {
    disk_number: u32,
    serial: String,
}

impl EraseAgreement {
    /// Checks what the operator typed against what the device is.
    ///
    /// The phrase has to be `ERASE <serial>`, matched exactly apart from
    /// surrounding space and letter case. A device with no serial number cannot
    /// be confirmed at all, which is deliberate: there would be nothing
    /// specific to type, and `ERASE` on its own is too easy to type by accident
    /// about the wrong device.
    pub fn check(disk_number: u32, serial: Option<&str>, typed: &str) -> Result<Self> {
        let Some(serial) = serial.map(str::trim).filter(|s| !s.is_empty()) else {
            return Err(Error::new(
                ExitCode::Unsupported,
                "this device cannot be confirmed",
                format!(
                    "disk {disk_number} does not report a serial number, so there is nothing specific to type to confirm erasing it"
                ),
                "use a different USB device, or build an ISO file instead",
            ));
        };

        let expected = format!("ERASE {serial}");
        if typed.trim().eq_ignore_ascii_case(&expected) {
            return Ok(Self {
                disk_number,
                serial: serial.to_owned(),
            });
        }
        Err(Error::new(
            ExitCode::Failure,
            "that is not the confirmation phrase",
            format!("to erase disk {disk_number}, type exactly: {expected}"),
            "check you have the right device, then type the phrase exactly",
        ))
    }

    /// The phrase that would confirm this device.
    pub fn phrase_for(serial: &str) -> String {
        format!("ERASE {serial}")
    }

    /// The disk this agreement is about.
    pub fn disk_number(&self) -> u32 {
        self.disk_number
    }

    /// The serial that was typed.
    pub fn serial(&self) -> &str {
        &self.serial
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exact_phrase_confirms_the_device() {
        let agreement = EraseAgreement::check(3, Some("USB123456"), "ERASE USB123456").unwrap();
        assert_eq!(agreement.disk_number(), 3);
        assert_eq!(agreement.serial(), "USB123456");
    }

    #[test]
    fn surrounding_space_and_case_are_forgiven() {
        assert!(EraseAgreement::check(3, Some("USB123456"), "  erase usb123456  ").is_ok());
    }

    /// Everything short of the phrase has to be refused, because the cost of a
    /// mistake here is somebody's only copy of something.
    #[test]
    fn anything_else_is_refused() {
        for typed in [
            "y",
            "yes",
            "ERASE",
            "ERASE ",
            "ERASE USB12345",
            "ERASE USB1234567",
            "ERASEUSB123456",
            "USB123456",
            "",
        ] {
            assert!(
                EraseAgreement::check(3, Some("USB123456"), typed).is_err(),
                "{typed:?} should not have confirmed anything"
            );
        }
    }

    /// An agreement for one device must never satisfy another.
    #[test]
    fn an_agreement_names_one_device() {
        let agreement = EraseAgreement::check(3, Some("USB123456"), "ERASE USB123456").unwrap();
        assert_eq!(agreement.disk_number(), 3);

        // The phrase for a different device does not match this one.
        assert!(EraseAgreement::check(4, Some("OTHER9999"), "ERASE USB123456").is_err());
    }

    #[test]
    fn a_device_without_a_serial_number_cannot_be_confirmed() {
        for serial in [None, Some(""), Some("   ")] {
            let err = EraseAgreement::check(3, serial, "ERASE ").unwrap_err();
            assert!(err.what().contains("cannot be confirmed"));
            assert!(err.next_step().contains("ISO"));
        }
    }

    #[test]
    fn the_phrase_is_shown_the_way_it_has_to_be_typed() {
        assert_eq!(EraseAgreement::phrase_for("ABC"), "ERASE ABC");
        let err = EraseAgreement::check(1, Some("ABC"), "no").unwrap_err();
        assert!(err.why().contains("ERASE ABC"));
    }
}
