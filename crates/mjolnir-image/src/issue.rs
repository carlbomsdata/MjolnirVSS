//! Validation findings.
//!
//! Validation collects findings instead of stopping at the first, because an
//! operator staring at a damaged backup during a recovery needs the whole
//! picture in one go, and because "which object failed" is more useful than
//! "something failed".

use std::fmt;

/// How serious a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Worth telling the operator, but the backup remains usable.
    Warning,
    /// The backup must not be restored.
    Error,
}

impl Severity {
    /// Lowercase label used in reports and logs.
    pub const fn label(self) -> &'static str {
        match self {
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }
}

/// One problem found while validating a backup set, naming the object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// How serious this is.
    pub severity: Severity,
    /// Which object failed, for example `stream "windows" segment 17`.
    pub object: String,
    /// What is wrong with it.
    pub problem: String,
}

impl Issue {
    /// A finding that blocks restoration.
    pub fn error(object: impl Into<String>, problem: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            object: object.into(),
            problem: problem.into(),
        }
    }

    /// A finding that does not block restoration.
    pub fn warning(object: impl Into<String>, problem: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            object: object.into(),
            problem: problem.into(),
        }
    }

    /// Whether this finding blocks restoration.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {}: {}",
            self.severity.label(),
            self.object,
            self.problem
        )
    }
}

/// Helpers over a collection of findings.
pub trait IssueList {
    /// Whether any finding blocks restoration.
    fn has_errors(&self) -> bool;
    /// How many findings block restoration.
    fn error_count(&self) -> usize;
    /// Turns the findings into a MjolnirVSS error if any of them is fatal.
    ///
    /// `subject` names what was being validated, for example the backup path.
    fn into_result(self, subject: &str) -> mjolnir_core::Result<Vec<Issue>>;
}

impl IssueList for Vec<Issue> {
    fn has_errors(&self) -> bool {
        self.iter().any(Issue::is_error)
    }

    fn error_count(&self) -> usize {
        self.iter().filter(|i| i.is_error()).count()
    }

    fn into_result(self, subject: &str) -> mjolnir_core::Result<Vec<Issue>> {
        if !self.has_errors() {
            return Ok(self);
        }
        let errors: Vec<String> = self
            .iter()
            .filter(|i| i.is_error())
            .map(|i| format!("  {}: {}", i.object, i.problem))
            .collect();
        let shown = errors.len().min(20);
        let mut detail = errors[..shown].join("\n");
        if errors.len() > shown {
            detail.push_str(&format!("\n  and {} more", errors.len() - shown));
        }
        Err(mjolnir_core::Error::corrupt(
            format!(
                "{subject} failed validation with {} problem{}",
                errors.len(),
                if errors.len() == 1 { "" } else { "s" }
            ),
            format!("restoring from it could produce a computer that does not boot, or overwrite a disk with incomplete data:\n{detail}"),
            "do not restore from this backup; take a fresh backup, and if the same problems appear again check the destination drive's health",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warnings_alone_do_not_fail() {
        let issues = vec![Issue::warning("manifest", "written by a newer build")];
        assert!(!issues.has_errors());
        assert_eq!(issues.error_count(), 0);
        assert!(issues.into_result("the backup").is_ok());
    }

    #[test]
    fn errors_become_a_corrupt_backup_failure() {
        let issues = vec![
            Issue::warning("manifest", "minor thing"),
            Issue::error("chunk 4", "missing"),
        ];
        assert!(issues.has_errors());
        let err = issues.into_result("E:\\Backups\\PC_2026").unwrap_err();
        assert_eq!(err.exit(), mjolnir_core::ExitCode::CorruptBackup);
        assert!(err.what().contains("1 problem"));
        assert!(err.why().contains("chunk 4"));
    }

    #[test]
    fn a_long_list_of_errors_is_truncated_rather_than_flooding_the_screen() {
        let issues: Vec<Issue> = (0..100)
            .map(|i| Issue::error(format!("chunk {i}"), "missing"))
            .collect();
        let err = issues.into_result("the backup").unwrap_err();
        assert!(err.what().contains("100 problems"));
        assert!(err.why().contains("and 80 more"));
    }

    #[test]
    fn display_names_the_object() {
        let i = Issue::error("stream \"windows\" segment 17", "overlaps the previous");
        assert_eq!(
            i.to_string(),
            "error: stream \"windows\" segment 17: overlaps the previous"
        );
    }
}
