//! `MjolnirVSS.Restore.exe`: the recovery application.
//!
//! Runs from Windows installation or recovery media, on a computer whose disk
//! has failed. It has no installer, no runtime and no dependency on the machine
//! it is restoring.
//!
//! It deliberately does not link the shadow copy code. `vssapi.dll` is not part
//! of a base Windows PE image, and a missing import would stop this program
//! from starting at exactly the moment somebody needs it. `scripts/package.ps1`
//! checks the built binary's imports to make sure that stays true.
//!
//! Copyright (C) the MjolnirVSS contributors.
//! Licensed under the GNU General Public License, version 3 or later.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod discover;

#[cfg(windows)]
mod cli;
#[cfg(windows)]
mod window;

fn main() -> std::process::ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();

    if args.len() <= 1 {
        return run_gui();
    }

    #[cfg(windows)]
    {
        mjolnir_win32_ui::console::attach_to_parent();
        let code = cli::run_from_args(args);
        std::process::ExitCode::from(code.code() as u8)
    }

    #[cfg(not(windows))]
    {
        eprintln!("MjolnirVSS.Restore only runs on Windows.");
        std::process::ExitCode::from(mjolnir_core::ExitCode::Unsupported.code() as u8)
    }
}

#[cfg(windows)]
fn run_gui() -> std::process::ExitCode {
    match window::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            mjolnir_win32_ui::message_box::error("MjolnirVSS Recovery", &e);
            std::process::ExitCode::from(e.exit().code() as u8)
        }
    }
}

#[cfg(not(windows))]
fn run_gui() -> std::process::ExitCode {
    eprintln!("MjolnirVSS.Restore only runs on Windows.");
    std::process::ExitCode::from(mjolnir_core::ExitCode::Unsupported.code() as u8)
}
