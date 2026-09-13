//! Asking a volume which of its clusters are in use.
//!
//! This is the Windows half of used block imaging. The decoding, the arithmetic
//! and every rule about what a valid answer looks like live in
//! [`mjolnir_ntfs::bitmap`], which has no Windows in it and is tested against
//! synthetic bitmaps. What is left here is the loop that issues
//! `FSCTL_GET_VOLUME_BITMAP` until the volume has been described once.
//!
//! The control code is issued against a **shadow copy device**, not the live
//! volume. That matters: the answer has to describe the same frozen image the
//! bytes are read from, or the backup would capture clusters according to one
//! moment and read them at another. Microsoft does not document the control
//! code as working on a shadow copy handle, so the fact that it does is
//! measured rather than assumed, and a volume where it does not work falls back
//! to being copied whole.

use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_ntfs::bitmap::{
    Allocation, AllocationScan, BitmapPage, BITMAP_HEADER_BYTES, DEFAULT_PAGE_BYTES,
    FSCTL_GET_VOLUME_BITMAP,
};

use crate::device::Device;

/// Largest number of round trips before the loop gives up.
///
/// At the default page size this is enough to describe a volume of about two
/// petabytes. It exists so that a filesystem answering oddly produces an error
/// rather than an endless loop; the scan's own progress rule catches the normal
/// forms of that, and this catches the rest.
const MAX_PAGES: u32 = 32_768;

/// Reads a volume's allocation bitmap in full.
///
/// `device` must be open on a volume or a shadow copy device. Passing a
/// physical disk gets an error from Windows, which is correct: a disk has no
/// allocation bitmap.
pub fn read_allocation(device: &Device, cancel: &CancelToken) -> Result<Allocation> {
    let mut buffer = vec![0u8; BITMAP_HEADER_BYTES + DEFAULT_PAGE_BYTES];
    let mut scan: Option<AllocationScan> = None;
    let mut starting_lcn = 0i64;

    for _ in 0..MAX_PAGES {
        cancel.check()?;

        // STARTING_LCN_INPUT_BUFFER: a single signed cluster number.
        let input = starting_lcn.to_le_bytes();
        let (returned, more) =
            device.control_paged(FSCTL_GET_VOLUME_BITMAP, &input, &mut buffer)?;

        let page = BitmapPage::parse(&buffer, returned as usize)?;
        let next = match &mut scan {
            None => {
                let started = AllocationScan::begin(&page)?;
                let next = started.next_lcn();
                scan = Some(started);
                next
            }
            Some(existing) => {
                existing.accept(&page)?;
                existing.next_lcn()
            }
        };

        let complete = scan.as_ref().is_some_and(AllocationScan::is_complete);
        if complete {
            // The filesystem may still be saying there is more, which would mean
            // it disagrees with its own reported size. The scan's own count is
            // what the rest of the backup is built on, so it wins, and the
            // disagreement is not silently absorbed.
            if more {
                return Err(Error::new(
                    ExitCode::Unsupported,
                    "the volume disagrees with itself about its own size",
                    format!(
                        "the allocation bitmap described all {} clusters it said the volume has, and then reported that more remained",
                        scan.as_ref().map(AllocationScan::total_clusters).unwrap_or(0)
                    ),
                    "capture this partition without used block imaging",
                ));
            }
            return scan.expect("set above").finish();
        }

        if !more {
            // Not complete, and the filesystem says there is nothing left. The
            // scan will refuse this, with a message naming how much of the
            // volume was never described.
            return scan.expect("set above").finish();
        }

        starting_lcn = i64::try_from(next).map_err(|_| {
            Error::new(
                ExitCode::Unsupported,
                "the volume has more clusters than Windows can address",
                format!(
                    "the scan reached cluster {next}, which does not fit a signed 64 bit value"
                ),
                "capture this partition without used block imaging",
            )
        })?;
    }

    Err(Error::new(
        ExitCode::Unsupported,
        "the volume's allocation bitmap did not finish",
        format!(
            "the filesystem was asked for it {MAX_PAGES} times and still reported more to come"
        ),
        "capture this partition without used block imaging",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The control code has to be exactly the one in `winioctl.h`, because a
    /// wrong one would be rejected by the driver in a way that looks like an
    /// unsupported filesystem and would quietly disable used block imaging
    /// everywhere.
    ///
    /// `CTL_CODE(FILE_DEVICE_FILE_SYSTEM=9, 27, METHOD_NEITHER=3, FILE_ANY_ACCESS=0)`
    /// is `(device << 16) | (access << 14) | (function << 2) | method`.
    #[test]
    fn the_control_code_matches_the_windows_header() {
        let (device, access, function, method) = (9u32, 0u32, 27u32, 3u32);
        let computed: u32 = (device << 16) | (access << 14) | (function << 2) | method;
        assert_eq!(FSCTL_GET_VOLUME_BITMAP, computed);
        assert_eq!(FSCTL_GET_VOLUME_BITMAP, 0x0009_006F);
    }

    /// A physical disk has no allocation bitmap, and asking for one must fail
    /// rather than produce something. Runs against whatever the machine has,
    /// and asserts only that the refusal happens.
    #[test]
    fn a_device_with_no_filesystem_refuses() {
        let Ok(device) = Device::query(r"\\.\PhysicalDrive0") else {
            // No disk 0, or no rights. Nothing to assert.
            return;
        };
        let result = read_allocation(&device, &CancelToken::new());
        assert!(
            result.is_err(),
            "a physical disk must not appear to have an allocation bitmap"
        );
    }
}
