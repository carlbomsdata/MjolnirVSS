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

mod discover;

#[cfg(windows)]
mod cli;
#[cfg(windows)]
mod window;

fn main() -> std::process::ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();

    if args.len() <= 1 {
        #[cfg(windows)]
        mjolnir_win32_ui::console::release_own_console();
        return run_gui();
    }

    #[cfg(windows)]
    {
        mjolnir_win32_ui::console::attach_to_parent();
        // Before any work starts, so that a Ctrl+C during a restore stops
        // between blocks instead of in the middle of writing one.
        mjolnir_win32_ui::console::install_cancel_handler();
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
