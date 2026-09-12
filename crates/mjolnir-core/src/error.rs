//! The error type shared by every MjolnirVSS crate.
//!
//! An operator hitting an error in the middle of a bare metal recovery needs
//! three things, and a bare message string gives only the first: what failed,
//! why that matters, and what can safely be done next. The type below forces
//! all three to be supplied at the point where the failure is understood, and
//! carries the process exit code so the top level never has to guess.

use std::fmt;

use crate::exit::ExitCode;
use crate::math::ArithError;

/// A MjolnirVSS failure, with operator facing explanation and exit code.
#[derive(Debug)]
pub struct Error {
    exit: ExitCode,
    what: String,
    why: String,
    next: String,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl Error {
    /// Builds an error. Prefer the helpers below where one fits.
    pub fn new(
        exit: ExitCode,
        what: impl Into<String>,
        why: impl Into<String>,
        next: impl Into<String>,
    ) -> Self {
        Self {
            exit,
            what: what.into(),
            why: why.into(),
            next: next.into(),
            source: None,
        }
    }

    /// Attaches the underlying error that caused this one.
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// The process exit code this failure maps to.
    pub fn exit(&self) -> ExitCode {
        self.exit
    }

    /// What failed.
    pub fn what(&self) -> &str {
        &self.what
    }

    /// Why it matters.
    pub fn why(&self) -> &str {
        &self.why
    }

    /// What the operator can safely do next.
    pub fn next_step(&self) -> &str {
        &self.next
    }

    /// The configuration is outside the supported set.
    pub fn unsupported(
        what: impl Into<String>,
        why: impl Into<String>,
        next: impl Into<String>,
    ) -> Self {
        Self::new(ExitCode::Unsupported, what, why, next)
    }

    /// A backup set is corrupt, truncated or incomplete.
    pub fn corrupt(
        what: impl Into<String>,
        why: impl Into<String>,
        next: impl Into<String>,
    ) -> Self {
        Self::new(ExitCode::CorruptBackup, what, why, next)
    }

    /// The chosen restore target was refused.
    pub fn unsafe_target(
        what: impl Into<String>,
        why: impl Into<String>,
        next: impl Into<String>,
    ) -> Self {
        Self::new(ExitCode::UnsafeTarget, what, why, next)
    }

    /// The operator cancelled.
    pub fn cancelled() -> Self {
        Self::new(
            ExitCode::Cancelled,
            "the operation was cancelled",
            "no further data was written, and any snapshot MjolnirVSS created has been released",
            "rerun the command when ready; an interrupted backup is never marked complete",
        )
    }

    /// Wraps a `std::io::Error` against a named path or device.
    pub fn io(subject: impl fmt::Display, err: std::io::Error) -> Self {
        let denied = err.kind() == std::io::ErrorKind::PermissionDenied;
        let exit = if denied {
            ExitCode::AccessDenied
        } else {
            ExitCode::Io
        };
        let next = if denied {
            "run the command from an elevated command prompt, and check that no other program holds the device open"
        } else {
            "check that the drive is still connected and has free space, then rerun the command"
        };
        Self::new(
            exit,
            format!("input or output failed on {subject}"),
            format!("the operating system reported: {err}"),
            next,
        )
        .with_source(err)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}\n  why:  {}\n  next: {}",
            self.what, self.why, self.next
        )
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|b| b.as_ref() as &(dyn std::error::Error + 'static))
    }
}

impl From<ArithError> for Error {
    fn from(err: ArithError) -> Self {
        // Reaching here means a number from a manifest, a partition table or a
        // device did not survive a bounds check. That is either corruption or a
        // layout MjolnirVSS does not understand, and in both cases continuing
        // could mean writing outside the intended range.
        Error::new(
            ExitCode::CorruptBackup,
            format!("a size or offset failed its bounds check: {err}"),
            "MjolnirVSS refuses to compute a range it cannot prove is inside the target, because the write would land somewhere unintended",
            "run `MjolnirVSS.exe verify <backup-path>` to locate the damaged object, and use a different backup if it reports corruption",
        )
        .with_source(err)
    }
}

/// Result alias used throughout MjolnirVSS.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn display_contains_all_three_parts() {
        let e = Error::unsupported("disk 0 is a dynamic disk", "why", "next");
        let text = e.to_string();
        assert!(text.contains("disk 0 is a dynamic disk"));
        assert!(text.contains("why:"));
        assert!(text.contains("next:"));
        assert_eq!(e.exit(), ExitCode::Unsupported);
    }

    #[test]
    fn permission_denied_maps_to_access_denied() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let e = Error::io("\\\\.\\PhysicalDrive0", io);
        assert_eq!(e.exit(), ExitCode::AccessDenied);
        assert!(e.next_step().contains("elevated"));
    }

    #[test]
    fn other_io_errors_map_to_io() {
        let io = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short read");
        assert_eq!(Error::io("chunk", io).exit(), ExitCode::Io);
    }

    #[test]
    fn arithmetic_failure_is_treated_as_corruption() {
        let err = crate::math::add_u64("segment end", u64::MAX, 1).unwrap_err();
        let e: Error = err.into();
        assert_eq!(e.exit(), ExitCode::CorruptBackup);
        assert!(e.source().is_some());
    }
}
