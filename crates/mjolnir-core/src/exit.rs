//! Stable, machine-readable process exit codes.
//!
//! These values are part of the MjolnirVSS command line contract. Scripts may
//! depend on them, so a code is never reused for a different meaning. New
//! conditions get new numbers.

/// Exit code returned by `MjolnirVSS.exe` and `MjolnirVSS.Restore.exe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ExitCode {
    /// The requested operation completed and its result is trustworthy.
    Success = 0,
    /// An error that does not fit any other category.
    Failure = 1,
    /// The command line could not be parsed, or arguments contradict.
    Usage = 2,
    /// The process is not elevated, or Windows denied access to a device.
    AccessDenied = 3,
    /// The machine, disk or volume layout is outside the supported set.
    Unsupported = 4,
    /// The Volume Shadow Copy Service refused or failed the request.
    VssFailure = 5,
    /// A backup set is corrupt, incomplete or fails verification.
    CorruptBackup = 6,
    /// The backup destination is unusable, full or unsafe to write to.
    Destination = 7,
    /// The chosen restore target was refused for safety reasons.
    UnsafeTarget = 8,
    /// The operator cancelled the operation.
    Cancelled = 9,
    /// A read or write against a device or file failed.
    Io = 10,
}

impl ExitCode {
    /// The numeric value handed back to the operating system.
    pub const fn code(self) -> i32 {
        self as i32
    }

    /// Short stable identifier, printed in logs next to the number.
    pub const fn name(self) -> &'static str {
        match self {
            ExitCode::Success => "success",
            ExitCode::Failure => "failure",
            ExitCode::Usage => "usage",
            ExitCode::AccessDenied => "access-denied",
            ExitCode::Unsupported => "unsupported",
            ExitCode::VssFailure => "vss-failure",
            ExitCode::CorruptBackup => "corrupt-backup",
            ExitCode::Destination => "destination",
            ExitCode::UnsafeTarget => "unsafe-target",
            ExitCode::Cancelled => "cancelled",
            ExitCode::Io => "io",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable() {
        // Locking the contract down: changing any of these breaks callers.
        assert_eq!(ExitCode::Success.code(), 0);
        assert_eq!(ExitCode::Failure.code(), 1);
        assert_eq!(ExitCode::Usage.code(), 2);
        assert_eq!(ExitCode::AccessDenied.code(), 3);
        assert_eq!(ExitCode::Unsupported.code(), 4);
        assert_eq!(ExitCode::VssFailure.code(), 5);
        assert_eq!(ExitCode::CorruptBackup.code(), 6);
        assert_eq!(ExitCode::Destination.code(), 7);
        assert_eq!(ExitCode::UnsafeTarget.code(), 8);
        assert_eq!(ExitCode::Cancelled.code(), 9);
        assert_eq!(ExitCode::Io.code(), 10);
    }

    #[test]
    fn names_are_unique() {
        let all = [
            ExitCode::Success,
            ExitCode::Failure,
            ExitCode::Usage,
            ExitCode::AccessDenied,
            ExitCode::Unsupported,
            ExitCode::VssFailure,
            ExitCode::CorruptBackup,
            ExitCode::Destination,
            ExitCode::UnsafeTarget,
            ExitCode::Cancelled,
            ExitCode::Io,
        ];
        let mut names: Vec<&str> = all.iter().map(|e| e.name()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len());
    }
}
