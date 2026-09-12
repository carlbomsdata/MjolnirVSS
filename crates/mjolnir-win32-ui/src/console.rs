//! The console, and getting rid of it.
//!
//! Both applications are built for the **console** subsystem. That is the only
//! choice under which the command line behaves properly: a windows subsystem
//! program does not make the shell wait for it, and does not inherit the pipe a
//! shell set up to capture its output, so `MjolnirVSS.exe verify ...` would
//! return to the prompt immediately and print nothing into a script. This was
//! measured rather than assumed, and it is why the subsystem was changed.
//!
//! The price of that choice is a console window when the program is started by
//! double clicking it, which [`release_own_console`] removes before the
//! graphical interface appears.
//!
//! [`attach_to_parent`] is kept for the other direction: it binds the standard
//! handles when a console exists but nothing is attached to them, and it takes
//! care not to disturb a stream the parent has already redirected to a file or
//! a pipe.

/// Attaches to the console of the process that started this one, and makes
/// the standard handles usable.
///
/// Silent on failure, which is the normal case when the program was started
/// from Explorer: there is no parent console, output goes nowhere, and the exit
/// code still carries the result.
#[cfg(windows)]
pub fn attach_to_parent() {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ,
        FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    // SAFETY: the call takes a single integer, the documented constant meaning
    // "the parent process", and no pointers. It either succeeds, or fails
    // because there is no parent console or this process already has one. Both
    // outcomes are fine; a console attached this way is released when the
    // process exits.
    let attached = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_ok();
    if !attached {
        return;
    }

    /// Binds one standard handle to the console, unless it already points at
    /// something the parent set up.
    fn bind(which: STD_HANDLE, device: &str, writable: bool) {
        // SAFETY: GetStdHandle takes one documented constant and returns a
        // handle this process already owns, or null when nothing is bound.
        let current = unsafe { GetStdHandle(which) };
        let already_bound = match current {
            Ok(h) => !h.is_invalid() && h != HANDLE::default(),
            Err(_) => false,
        };
        if already_bound {
            // The parent redirected this stream to a file or a pipe. Leaving it
            // alone is what makes `> out.txt` and `| Select-String` work.
            return;
        }

        let name: Vec<u16> = device.encode_utf16().chain(std::iter::once(0)).collect();
        let access = if writable {
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0
        } else {
            FILE_GENERIC_READ.0
        };

        // SAFETY: `name` is a null terminated wide string in a local that
        // outlives the call. CONOUT$ and CONIN$ are the documented names for
        // the attached console's screen buffer and input buffer, and sharing
        // both ways is required because the console is shared with the parent.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(FILE_ATTRIBUTE_NORMAL.0),
                None,
            )
        };
        let Ok(handle) = handle else {
            return;
        };
        if handle.is_invalid() || handle == INVALID_HANDLE_VALUE {
            return;
        }

        // SAFETY: `handle` was just opened by this process and is valid. The
        // process keeps it for its whole life, which is what a standard handle
        // requires; it is deliberately not closed, because closing it would
        // leave the standard handle dangling.
        unsafe {
            let _ = SetStdHandle(which, handle);
        }
    }

    bind(STD_OUTPUT_HANDLE, "CONOUT$", true);
    bind(STD_ERROR_HANDLE, "CONOUT$", true);
    bind(STD_INPUT_HANDLE, "CONIN$", false);
}

/// Does nothing on platforms without a Windows console.
#[cfg(not(windows))]
pub fn attach_to_parent() {}

/// Hides and releases this process's console, if it owns one.
///
/// Both applications are built for the console subsystem, because that is the
/// only way the command line behaves correctly: a windows subsystem program
/// does not make the shell wait for it and does not inherit the pipe the shell
/// set up, so `MjolnirVSS.exe verify ...` would return instantly and print
/// nothing into a script that captured it.
///
/// The cost is a console window when the program is started by double clicking
/// it. This removes that window before the graphical interface appears.
///
/// It only releases a console this process owns. When the program was started
/// from an existing prompt, that prompt's console is shared with the shell, and
/// closing it would close the operator's window.
#[cfg(windows)]
pub fn release_own_console() {
    use windows::Win32::System::Console::{FreeConsole, GetConsoleProcessList, GetConsoleWindow};
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

    // SAFETY: takes no arguments and returns the console window handle, or a
    // null handle when this process has no console at all.
    let console = unsafe { GetConsoleWindow() };
    if console.is_invalid() {
        return;
    }

    // How many processes share this console tells us who created it. One means
    // only this process, so the console came up with it and closing it affects
    // nobody else. More than one means it was inherited from a shell.
    let mut pids = [0u32; 4];
    // SAFETY: the slice is a live local and its length is what the call is
    // told, so the call cannot write past it. A count larger than the buffer is
    // reported as the required size rather than written, which is why a result
    // above the buffer length is treated as "shared".
    let count = unsafe { GetConsoleProcessList(&mut pids) };
    if count != 1 {
        return;
    }

    // SAFETY: `console` is this process's own console window, established
    // above. Hiding it before releasing it keeps it from flickering while the
    // graphical interface is created.
    unsafe {
        let _ = ShowWindow(console, SW_HIDE);
        let _ = FreeConsole();
    }
}

/// Does nothing on platforms without a Windows console.
#[cfg(not(windows))]
pub fn release_own_console() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attaching_twice_is_harmless() {
        // The test harness owns a console already, so both calls are no-ops.
        // Neither may panic or leave the process unable to print.
        attach_to_parent();
        attach_to_parent();
        println!("still able to print after attaching");
    }
}
