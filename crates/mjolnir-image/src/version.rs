//! Format identity and version handling.
//!
//! Every document in a backup set carries the same version block, so a reader
//! can identify a file before trusting anything else in it, and so a set whose
//! documents came from different tool versions is caught rather than mixed.

use serde::{Deserialize, Serialize};

/// Human readable name of the format, written into every document.
pub const FORMAT_NAME: &str = "MjolnirVSS Image";

/// Machine readable identifier of the format.
pub const FORMAT_MAGIC: &str = "MJOLNIRVSS";

/// Major version. A reader refuses a document with a different major.
pub const FORMAT_MAJOR: u32 = 1;

/// Minor version understood by this build.
pub const FORMAT_MINOR: u32 = 0;

/// The version block at the head of every MjolnirVSS document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatHeader {
    /// Always [`FORMAT_NAME`]. Present so a human opening the file knows what
    /// it is without consulting documentation.
    pub name: String,
    /// Always [`FORMAT_MAGIC`].
    pub magic: String,
    /// Incompatible revision.
    pub major: u32,
    /// Compatible revision, incremented when optional fields are added.
    pub minor: u32,
    /// The lowest minor version that can read this document correctly.
    ///
    /// A writer that adds a field an older reader may safely ignore leaves this
    /// at its current value. A writer that adds one an older reader would have
    /// to act on raises it, and older readers then refuse the document instead
    /// of quietly restoring something incomplete.
    #[serde(default)]
    pub min_reader_minor: u32,
    /// Which document this is, so a file cannot be read as the wrong kind.
    pub document: DocumentKind,
}

/// Which document of a backup set a file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DocumentKind {
    /// `manifest.json`.
    Manifest,
    /// `disk-layout.json`.
    DiskLayout,
    /// `completion.json`.
    Completion,
    /// `indexes/volume-<id>.json`.
    VolumeIndex,
}

impl DocumentKind {
    /// The filename this kind normally occupies, for error messages.
    pub const fn filename(self) -> &'static str {
        match self {
            DocumentKind::Manifest => "manifest.json",
            DocumentKind::DiskLayout => "disk-layout.json",
            DocumentKind::Completion => "completion.json",
            DocumentKind::VolumeIndex => "indexes/volume-<id>.json",
        }
    }
}

impl FormatHeader {
    /// Builds a header for a document written by this build.
    pub fn current(document: DocumentKind) -> Self {
        Self {
            name: FORMAT_NAME.to_owned(),
            magic: FORMAT_MAGIC.to_owned(),
            major: FORMAT_MAJOR,
            minor: FORMAT_MINOR,
            min_reader_minor: 0,
            document,
        }
    }

    /// Checks the header against this build and the expected document kind.
    ///
    /// Returns a list of problems rather than the first one, so an operator
    /// looking at a damaged backup sees everything at once.
    pub fn check(&self, expected: DocumentKind) -> Vec<crate::issue::Issue> {
        use crate::issue::Issue;
        let object = expected.filename();
        let mut issues = Vec::new();

        if self.magic != FORMAT_MAGIC {
            issues.push(Issue::error(
                object,
                format!(
                    "magic is {:?}, expected {FORMAT_MAGIC:?}; this is not a MjolnirVSS document",
                    self.magic
                ),
            ));
        }
        if self.document != expected {
            issues.push(Issue::error(
                object,
                format!(
                    "this file says it is a {:?} document but it was read as {:?}; the backup set is mixed up",
                    self.document, expected
                ),
            ));
        }
        if self.major != FORMAT_MAJOR {
            issues.push(Issue::error(
                object,
                format!(
                    "format version {}.x cannot be read by this build, which understands version {FORMAT_MAJOR}.x",
                    self.major
                ),
            ));
        }
        if self.min_reader_minor > FORMAT_MINOR {
            issues.push(Issue::error(
                object,
                format!(
                    "this backup needs MjolnirVSS understanding format {FORMAT_MAJOR}.{} or later; this build understands {FORMAT_MAJOR}.{FORMAT_MINOR}",
                    self.min_reader_minor
                ),
            ));
        } else if self.minor > FORMAT_MINOR {
            issues.push(Issue::warning(
                object,
                format!(
                    "written by a newer MjolnirVSS using format {FORMAT_MAJOR}.{}; fields this build does not know about are ignored",
                    self.minor
                ),
            ));
        }
        issues
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issue::Severity;

    #[test]
    fn a_current_header_passes() {
        let h = FormatHeader::current(DocumentKind::Manifest);
        assert!(h.check(DocumentKind::Manifest).is_empty());
    }

    #[test]
    fn the_wrong_document_kind_is_caught() {
        let h = FormatHeader::current(DocumentKind::Manifest);
        let issues = h.check(DocumentKind::Completion);
        assert!(issues.iter().any(|i| i.problem.contains("mixed up")));
    }

    #[test]
    fn a_future_major_is_refused() {
        let mut h = FormatHeader::current(DocumentKind::Manifest);
        h.major = FORMAT_MAJOR + 1;
        let issues = h.check(DocumentKind::Manifest);
        assert!(issues.iter().any(|i| i.severity == Severity::Error));
    }

    #[test]
    fn a_newer_minor_is_only_a_warning_when_it_stays_readable() {
        let mut h = FormatHeader::current(DocumentKind::Manifest);
        h.minor = FORMAT_MINOR + 5;
        let issues = h.check(DocumentKind::Manifest);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Warning);
    }

    #[test]
    fn a_newer_minor_that_demands_a_newer_reader_is_refused() {
        let mut h = FormatHeader::current(DocumentKind::Manifest);
        h.minor = FORMAT_MINOR + 5;
        h.min_reader_minor = FORMAT_MINOR + 5;
        let issues = h.check(DocumentKind::Manifest);
        assert!(issues.iter().any(|i| i.severity == Severity::Error));
    }

    #[test]
    fn foreign_magic_is_refused() {
        let mut h = FormatHeader::current(DocumentKind::Manifest);
        h.magic = "SOMETHINGELSE".to_owned();
        assert!(h
            .check(DocumentKind::Manifest)
            .iter()
            .any(|i| i.severity == Severity::Error));
    }
}
