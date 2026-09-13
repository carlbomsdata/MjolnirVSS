//! Getting a password from somebody without leaving it lying about.
//!
//! Three ways in, in the order they are tried: a file named on the command
//! line, an environment variable, or asking. Asking is the normal one; the
//! other two exist because a scheduled backup has nobody to ask.
//!
//! **Not** a command line argument. A password passed as an argument is visible
//! in Task Manager, in the process list, and in the shell's history, which
//! makes it a worse secret than no secret at all.
//!
//! Copyright (C) the MjolnirVSS contributors.
//! Licensed under the GNU General Public License, version 3 or later.

use std::io::{self, BufRead, Write};
use std::path::Path;

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use zeroize::Zeroizing;

/// The environment variable a scheduled run can use.
pub const PASSWORD_VARIABLE: &str = "MJOLNIRVSS_PASSWORD";

/// Where a password may come from.
#[derive(Debug, Clone, Default)]
pub struct PasswordSource {
    /// A file holding the password and nothing else.
    pub file: Option<std::path::PathBuf>,
}

impl PasswordSource {
    /// Reads a password for opening an existing backup.
    ///
    /// The result wipes itself when dropped.
    pub fn read(&self, prompt: &str) -> Result<Zeroizing<String>> {
        if let Some(path) = &self.file {
            return read_from_file(path);
        }
        if let Ok(value) = std::env::var(PASSWORD_VARIABLE) {
            if !value.is_empty() {
                return Ok(Zeroizing::new(value));
            }
        }
        ask_once(prompt)
    }

    /// Reads a password for a **new** backup, asking twice.
    ///
    /// A password mistyped once is a backup that cannot be opened, and nobody
    /// finds out until the day they need it. Asking twice costs a few seconds
    /// now against losing everything later.
    pub fn read_new(&self) -> Result<Zeroizing<String>> {
        if self.file.is_some() || std::env::var(PASSWORD_VARIABLE).is_ok() {
            // Supplied rather than typed, so there is no typing to get wrong.
            return self.read("Password: ");
        }

        eprintln!("This backup will be encrypted. There is no way to open it without");
        eprintln!("the password, and MjolnirVSS does not store it anywhere. If you lose");
        eprintln!("it, the backup is lost.");
        eprintln!();

        let first = ask_once("Password: ")?;
        if first.trim().is_empty() {
            return Err(refuse("an empty password was given"));
        }
        let again = ask_once("Password again: ")?;
        if *first != *again {
            return Err(refuse("the two passwords were not the same"));
        }
        Ok(first)
    }
}

/// Reads a password from a file, trimming the trailing newline a text editor
/// leaves behind.
fn read_from_file(path: &Path) -> Result<Zeroizing<String>> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path.display(), e))?;
    let trimmed = text.trim_end_matches(['\r', '\n']).to_owned();
    if trimmed.is_empty() {
        return Err(refuse(&format!("{} is empty", path.display())));
    }
    Ok(Zeroizing::new(trimmed))
}

/// Asks at the console, without showing what is typed.
fn ask_once(prompt: &str) -> Result<Zeroizing<String>> {
    eprint!("{prompt}");
    io::stderr().flush().ok();

    let hidden = EchoOff::start();
    let mut line = String::new();
    let read = io::stdin().lock().read_line(&mut line);
    drop(hidden);
    eprintln!();

    read.map_err(|e| Error::io("the console", e))?;
    let value = line.trim_end_matches(['\r', '\n']).to_owned();
    line.clear();

    if value.is_empty() {
        return Err(refuse("no password was typed"));
    }
    Ok(Zeroizing::new(value))
}

fn refuse(why: &str) -> Error {
    Error::new(
        ExitCode::Failure,
        "a password is needed and was not given",
        why.to_owned(),
        "run the command again, or use --password-file for an unattended run",
    )
}

/// Turns console echo off while it exists, and back on when dropped.
///
/// On the way out of every path, including a failure and a Ctrl+C that unwinds,
/// because a console left with echo off is a console that looks broken.
struct EchoOff {
    #[cfg(windows)]
    previous: Option<(windows::Win32::Foundation::HANDLE, u32)>,
}

#[cfg(windows)]
impl EchoOff {
    fn start() -> Self {
        use windows::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE, ENABLE_ECHO_INPUT,
            STD_INPUT_HANDLE,
        };

        // SAFETY: the standard input handle is owned by the process and is not
        // closed here. Both calls are reads or writes of a mode word, and a
        // failure simply means the password is typed visibly, which is worse
        // but not unsafe.
        unsafe {
            let Ok(handle) = GetStdHandle(STD_INPUT_HANDLE) else {
                return Self { previous: None };
            };
            let mut mode = CONSOLE_MODE(0);
            if GetConsoleMode(handle, &mut mode).is_err() {
                // Not a console: input is redirected from a file or a pipe, and
                // there is no echo to turn off.
                return Self { previous: None };
            }
            let quiet = CONSOLE_MODE(mode.0 & !ENABLE_ECHO_INPUT.0);
            if SetConsoleMode(handle, quiet).is_err() {
                return Self { previous: None };
            }
            Self {
                previous: Some((handle, mode.0)),
            }
        }
    }
}

#[cfg(not(windows))]
impl EchoOff {
    fn start() -> Self {
        Self {}
    }
}

#[cfg(windows)]
impl Drop for EchoOff {
    fn drop(&mut self) {
        use windows::Win32::System::Console::{SetConsoleMode, CONSOLE_MODE};
        if let Some((handle, mode)) = self.previous.take() {
            // SAFETY: the handle is the one read in `start` and is still owned
            // by the process; the mode is the value it had before.
            unsafe {
                let _ = SetConsoleMode(handle, CONSOLE_MODE(mode));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_file_is_read_without_its_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        std::fs::write(&path, "hunter2\r\n").unwrap();

        let source = PasswordSource {
            file: Some(path.clone()),
        };
        assert_eq!(&*source.read("Password: ").unwrap(), "hunter2");
    }

    /// A file somebody made but never typed into would otherwise become an
    /// empty password, which is worse than no encryption at all.
    #[test]
    fn an_empty_password_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "\n").unwrap();

        let source = PasswordSource { file: Some(path) };
        assert!(source.read("Password: ").is_err());
    }

    #[test]
    fn a_missing_password_file_is_reported_as_a_missing_file() {
        let source = PasswordSource {
            file: Some(std::path::PathBuf::from("no-such-file-anywhere.txt")),
        };
        let err = source.read("Password: ").unwrap_err();
        assert!(
            err.what().contains("no-such-file-anywhere"),
            "the message should name the file: {}",
            err.what()
        );
    }

    /// Spaces can be part of a password and must survive. Only the line ending
    /// a text editor adds is removed.
    #[test]
    fn spaces_inside_a_password_survive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spaces.txt");
        std::fs::write(&path, "correct horse battery staple\n").unwrap();

        let source = PasswordSource { file: Some(path) };
        assert_eq!(
            &*source.read("Password: ").unwrap(),
            "correct horse battery staple"
        );
    }
}
