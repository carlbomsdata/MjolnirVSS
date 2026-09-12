//! Folder picking, opening Explorer, and free space.
//!
//! Three small shell operations the applications need. They live here so the
//! application crates contain no Win32 calls at all.

use std::path::{Path, PathBuf};

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, ShellExecuteW, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
    SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::sys::wide;

/// Asks the operator to choose a folder.
///
/// Returns `Ok(None)` when they cancel, which is not a failure.
pub fn pick_folder(parent: HWND, title: &str) -> Result<Option<PathBuf>> {
    // The shell dialog requires a single threaded apartment. This thread is the
    // one running the message loop and has not joined one, so it joins here and
    // leaves before returning.
    //
    // SAFETY: takes no pointers. A failure here means the thread is already in
    // an apartment of a different kind, in which case the dialog is attempted
    // anyway and reports its own error; `owns_com` records whether this call is
    // the one that must be balanced.
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let owns_com = hr.is_ok();

    let result = pick_folder_inner(parent, title);

    if owns_com {
        // SAFETY: balances the CoInitializeEx above, on the same thread, and
        // only when that call was the one that entered the apartment. Every
        // interface obtained inside has been dropped by now, because
        // pick_folder_inner returns owned values only.
        unsafe { CoUninitialize() };
    }
    result
}

fn pick_folder_inner(parent: HWND, title: &str) -> Result<Option<PathBuf>> {
    // SAFETY: creates a documented in-process shell class. No pointer is passed
    // in, and the returned interface is reference counted by the wrapper, which
    // releases it when it drops at the end of this function.
    let dialog: IFileOpenDialog =
        unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }.map_err(|e| {
            Error::new(
                ExitCode::Failure,
                "the folder picker could not be opened",
                format!("Windows reported: {e}"),
                "type the folder path into the box instead",
            )
        })?;

    let title_w = wide(title);

    // SAFETY: `dialog` is alive for this whole block. `title_w` is a null
    // terminated local that outlives the SetTitle call, which copies it. The
    // option flags are documented constants.
    unsafe {
        let options = dialog.GetOptions().unwrap_or_default();
        let _ = dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM);
        let _ = dialog.SetTitle(PCWSTR(title_w.as_ptr()));

        let owner = if parent.is_invalid() {
            None
        } else {
            Some(parent)
        };
        if dialog.Show(owner).is_err() {
            // The operator pressed Cancel. Show returns an error for that, and
            // it is not one.
            return Ok(None);
        }
    }

    // SAFETY: Show succeeded, which is the state in which GetResult is defined
    // to return the chosen item. The item is reference counted and released
    // when it drops.
    let item = unsafe { dialog.GetResult() }.map_err(|e| {
        Error::new(
            ExitCode::Failure,
            "the chosen folder could not be read",
            format!("Windows reported: {e}"),
            "type the folder path into the box instead",
        )
    })?;

    // SAFETY: `item` came from a successful GetResult. SIGDN_FILESYSPATH asks
    // for a real path, which fails for a library or a virtual folder rather
    // than returning something that is not a path.
    let path = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.map_err(|e| {
        Error::new(
            ExitCode::Failure,
            "the chosen folder is not a folder on a drive",
            format!("Windows reported: {e}"),
            "choose a folder on a drive rather than a library or a network shortcut",
        )
    })?;

    // SAFETY: GetDisplayName returns a string allocated by the shell's task
    // allocator, and the caller owns it. `to_string` copies it, and the copy is
    // what is returned, so freeing here is correct and the pointer is not used
    // afterwards.
    let text = unsafe { path.to_string() }.unwrap_or_default();
    // SAFETY: `path` was allocated by the shell and has not been freed. This is
    // the documented way to release it.
    unsafe { CoTaskMemFree(Some(path.0 as *const core::ffi::c_void)) };

    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(text)))
}

/// Opens a folder in File Explorer.
///
/// Best effort: if Explorer is not available, nothing happens, which is what
/// should happen inside a recovery environment.
pub fn open_in_explorer(path: &Path) {
    let wide_path = wide(&path.to_string_lossy());
    // SAFETY: both strings are null terminated. `w!("open")` is a static wide
    // literal and `wide_path` is a local that outlives the call, which does not
    // retain either. The remaining arguments are null, meaning no parameters
    // and no working directory.
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide_path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Free space on the drive a path is on, if it can be determined.
pub fn free_space_of(path: &str) -> Option<u64> {
    if path.trim().is_empty() {
        return None;
    }
    let wide_path = wide(path);
    let mut free = 0u64;
    // SAFETY: `wide_path` is a null terminated local that outlives the call.
    // `free` is a live local of the expected type, and is only read when the
    // call reported success.
    let ok =
        unsafe { GetDiskFreeSpaceExW(PCWSTR(wide_path.as_ptr()), Some(&mut free), None, None) };
    ok.ok().map(|_| free)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_space_of_a_missing_drive_is_unknown_rather_than_a_crash() {
        assert_eq!(free_space_of("Z:\\does\\not\\exist"), None);
        assert_eq!(free_space_of(""), None);
        assert_eq!(free_space_of("   "), None);
    }

    #[test]
    fn free_space_of_the_system_drive_is_reported() {
        // Every Windows machine has one, and it always has some free space.
        let free = free_space_of("C:\\").expect("the system drive should report free space");
        assert!(free > 0);
    }

    #[test]
    fn opening_a_path_that_does_not_exist_does_not_crash() {
        open_in_explorer(Path::new("Z:\\does\\not\\exist\\at\\all"));
    }
}
