//! `completion.json`: the file that makes a backup usable.
//!
//! A backup set is incomplete until this document exists. It is written last,
//! after every chunk and every other document is on stable storage, and it
//! carries the digests of those documents. That gives three things at once:
//!
//! * an explicit incomplete state, because a run that was interrupted or whose
//!   drive was unplugged simply never gets this file;
//! * an atomic transition to complete, because the file appears by a rename;
//! * tamper and corruption detection for the metadata, because the digests of
//!   `manifest.json` and `disk-layout.json` are recorded here and checked when
//!   the set is opened.

use serde::{Deserialize, Serialize};

use crate::hash::ChunkHash;
use crate::issue::Issue;
use crate::version::{DocumentKind, FormatHeader};

/// The state a backup set is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompletionState {
    /// Every chunk and document is durable and verified.
    Complete,
    /// The run finished but verification found problems. Recorded so the
    /// operator is told rather than left with a silently bad backup.
    Failed,
}

/// The outcome of verifying a backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerificationResult {
    /// Every chunk decompressed and matched its digest.
    Passed,
    /// At least one object failed.
    Failed,
    /// Verification was not run.
    NotPerformed,
}

/// What verification did and what it found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    /// The outcome.
    pub result: VerificationResult,
    /// RFC 3339 UTC time verification finished.
    #[serde(default)]
    pub completed_utc: Option<String>,
    /// How many chunks were decompressed and hashed.
    #[serde(default)]
    pub chunks_verified: u64,
    /// How many uncompressed bytes were hashed.
    #[serde(default)]
    pub bytes_verified: u64,
    /// Problems found, if any. Kept short: the full list goes in the log.
    #[serde(default)]
    pub problems: Vec<String>,
}

impl Verification {
    /// A record saying verification has not been run.
    pub fn not_performed() -> Self {
        Self {
            result: VerificationResult::NotPerformed,
            completed_utc: None,
            chunks_verified: 0,
            bytes_verified: 0,
            problems: Vec::new(),
        }
    }
}

/// The digest and size of one metadata document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentDigest {
    /// Path of the document relative to the backup folder.
    pub path: String,
    /// BLAKE3 of the file's exact bytes.
    pub blake3: ChunkHash,
    /// Size of the file in bytes.
    pub bytes: u64,
}

/// `completion.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Completion {
    /// Format identity.
    pub format: FormatHeader,
    /// The backup this belongs to, checked against `manifest.json`.
    pub backup_uuid: String,
    /// Whether the backup is usable.
    pub state: CompletionState,
    /// RFC 3339 UTC time the backup finished.
    pub completed_utc: String,
    /// What verification found.
    pub verification: Verification,
    /// Digests of every metadata document in the set.
    pub documents: Vec<DocumentDigest>,
    /// Smallest target disk, in bytes, this backup can be restored to.
    ///
    /// Duplicated from the manifest so the restore application can filter the
    /// disk list before parsing a large chunk table, and checked against the
    /// manifest when the set is opened.
    pub required_restore_bytes: u64,
}

impl Completion {
    /// Builds a completion document for this build.
    pub fn new(
        backup_uuid: impl Into<String>,
        state: CompletionState,
        completed_utc: impl Into<String>,
        verification: Verification,
        required_restore_bytes: u64,
    ) -> Self {
        Self {
            format: FormatHeader::current(DocumentKind::Completion),
            backup_uuid: backup_uuid.into(),
            state,
            completed_utc: completed_utc.into(),
            verification,
            documents: Vec::new(),
            required_restore_bytes,
        }
    }

    /// Whether the backup may be restored from.
    ///
    /// Deliberately conservative: anything other than a complete state with a
    /// passed verification is treated as not restorable.
    pub fn is_restorable(&self) -> bool {
        self.state == CompletionState::Complete
            && self.verification.result == VerificationResult::Passed
    }

    /// The digest recorded for one document.
    pub fn digest_of(&self, path: &str) -> Option<&DocumentDigest> {
        self.documents.iter().find(|d| d.path == path)
    }

    /// Checks the document on its own.
    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = self.format.check(DocumentKind::Completion);
        let object = "completion.json";

        if !crate::is_guid(&self.backup_uuid) {
            issues.push(Issue::error(
                object,
                format!("backup_uuid {:?} is not a GUID", self.backup_uuid),
            ));
        }
        if self.completed_utc.is_empty() {
            issues.push(Issue::error(object, "completed_utc is empty"));
        }

        for required in [
            crate::layout::MANIFEST_FILE,
            crate::layout::DISK_LAYOUT_FILE,
        ] {
            if self.digest_of(required).is_none() {
                issues.push(Issue::error(
                    object,
                    format!(
                        "records no digest for {required}, so the document cannot be checked for corruption"
                    ),
                ));
            }
        }

        for doc in &self.documents {
            if !crate::manifest::is_safe_relative_path(&doc.path) {
                issues.push(Issue::error(
                    object,
                    format!("document path {:?} is not a safe relative path", doc.path),
                ));
            }
            if doc.bytes == 0 {
                issues.push(Issue::error(
                    object,
                    format!("document {:?} is recorded as zero bytes", doc.path),
                ));
            }
        }

        match self.state {
            CompletionState::Complete if self.verification.result != VerificationResult::Passed => {
                issues.push(Issue::error(
                    object,
                    format!(
                        "is marked complete but verification is recorded as {:?}; MjolnirVSS only marks a backup complete after verification passes",
                        self.verification.result
                    ),
                ));
            }
            CompletionState::Failed => {
                issues.push(Issue::error(
                    object,
                    "records that this backup failed; it must not be restored from",
                ));
            }
            CompletionState::Complete => {}
        }

        issues
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passed() -> Verification {
        Verification {
            result: VerificationResult::Passed,
            completed_utc: Some("2026-09-12T10:20:00Z".to_owned()),
            chunks_verified: 12,
            bytes_verified: 4096,
            problems: Vec::new(),
        }
    }

    fn complete() -> Completion {
        let mut c = Completion::new(
            "99999999-8888-7777-6666-555555555555",
            CompletionState::Complete,
            "2026-09-12T10:20:00Z",
            passed(),
            1024,
        );
        c.documents.push(DocumentDigest {
            path: "manifest.json".to_owned(),
            blake3: ChunkHash::of(b"m"),
            bytes: 10,
        });
        c.documents.push(DocumentDigest {
            path: "disk-layout.json".to_owned(),
            blake3: ChunkHash::of(b"d"),
            bytes: 10,
        });
        c
    }

    #[test]
    fn a_complete_verified_backup_is_restorable() {
        let c = complete();
        assert!(c.validate().is_empty(), "{:?}", c.validate());
        assert!(c.is_restorable());
    }

    #[test]
    fn complete_without_passing_verification_is_refused() {
        let mut c = complete();
        c.verification = Verification::not_performed();
        assert!(!c.is_restorable());
        assert!(c.validate().iter().any(|i| i
            .problem
            .contains("only marks a backup complete after verification passes")));
    }

    #[test]
    fn a_failed_backup_is_never_restorable() {
        let mut c = complete();
        c.state = CompletionState::Failed;
        c.verification.result = VerificationResult::Failed;
        assert!(!c.is_restorable());
        assert!(c.validate().iter().any(|i| i.is_error()));
    }

    #[test]
    fn missing_document_digests_are_caught() {
        let mut c = complete();
        c.documents.retain(|d| d.path != "disk-layout.json");
        assert!(c
            .validate()
            .iter()
            .any(|i| i.problem.contains("disk-layout.json")));
    }

    #[test]
    fn a_hostile_document_path_is_refused() {
        let mut c = complete();
        c.documents.push(DocumentDigest {
            path: "../../windows/system32/config/sam".to_owned(),
            blake3: ChunkHash::of(b"x"),
            bytes: 1,
        });
        assert!(c
            .validate()
            .iter()
            .any(|i| i.problem.contains("not a safe relative path")));
    }

    #[test]
    fn round_trips_through_json() {
        let c = complete();
        let json = serde_json::to_string_pretty(&c).unwrap();
        let back: Completion = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }
}
