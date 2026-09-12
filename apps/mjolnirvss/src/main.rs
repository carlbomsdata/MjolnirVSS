//! `MjolnirVSS.exe`: the backup application.
//!
//! Run with no arguments it opens the window, which is how it is meant to be
//! used. Run with arguments it behaves as a console tool, which is what the
//! tests, the diagnostics and a scheduled backup use.
//!
//! The decision is made before anything else happens, because a graphical
//! application that was started from a command prompt has to attach to that
//! console to print anything at all.
//!
//! Copyright (C) the MjolnirVSS contributors.
//! Licensed under the GNU General Public License, version 3 or later.

// The subsystem is set by build.rs: windows when there are no arguments so no
// console flashes up, and the console is attached explicitly when there are.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();

    if args.len() <= 1 {
        return run_gui();
    }

    #[cfg(windows)]
    mjolnir_win32_ui::console::attach_to_parent();

    let code = mjolnir_cli::run_from_args(args);
    std::process::ExitCode::from(code.code() as u8)
}

#[cfg(windows)]
mod window;

#[cfg(windows)]
fn run_gui() -> std::process::ExitCode {
    match window::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            mjolnir_win32_ui::message_box::error("MjolnirVSS", &e);
            std::process::ExitCode::from(e.exit().code() as u8)
        }
    }
}

#[cfg(not(windows))]
fn run_gui() -> std::process::ExitCode {
    eprintln!("MjolnirVSS only runs on Windows.");
    std::process::ExitCode::from(mjolnir_core::ExitCode::Unsupported.code() as u8)
}
