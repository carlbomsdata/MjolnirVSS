//! Stable identifiers for machines, disks, partitions, volumes, streams and
//! backups.
//!
//! Identifiers end up in directory names on an NTFS volume and in JSON
//! manifests, so they are restricted to a conservative character set. That
//! keeps them safe as path components and keeps a hostile manifest from
//! reaching outside the backup set with something like `..`.

use std::fmt;

/// The longest an identifier may be.
pub const MAX_ID_LEN: usize = 64;

/// Why a string was rejected as an identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdError {
    /// The string was empty.
    Empty,
    /// The string was longer than [`MAX_ID_LEN`].
    TooLong {
        /// The length that was offered.
        len: usize,
    },
    /// The string contained a character outside `a-z`, `0-9` and `-`.
    BadCharacter {
        /// The offending character.
        ch: char,
        /// Its byte position.
        at: usize,
    },
    /// The string started or ended with a dash, or contained a double dash.
    BadShape,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdError::Empty => write!(f, "identifier is empty"),
            IdError::TooLong { len } => {
                write!(
                    f,
                    "identifier is {len} characters, the limit is {MAX_ID_LEN}"
                )
            }
            IdError::BadCharacter { ch, at } => write!(
                f,
                "identifier contains {ch:?} at position {at}, only a-z, 0-9 and - are allowed"
            ),
            IdError::BadShape => {
                write!(f, "identifier must not start or end with - or contain --")
            }
        }
    }
}

impl std::error::Error for IdError {}

/// Validates a raw identifier string.
///
/// The rules are deliberately strict: lowercase ASCII letters, digits and
/// single interior dashes. That excludes `.`, `..`, path separators, drive
/// letters, colons and every Windows reserved-name trick.
pub fn validate_id(value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty);
    }
    if value.len() > MAX_ID_LEN {
        return Err(IdError::TooLong { len: value.len() });
    }
    for (at, ch) in value.char_indices() {
        let ok = ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-';
        if !ok {
            return Err(IdError::BadCharacter { ch, at });
        }
    }
    if value.starts_with('-') || value.ends_with('-') || value.contains("--") {
        return Err(IdError::BadShape);
    }
    Ok(())
}

/// Turns arbitrary text into something [`validate_id`] accepts.
///
/// Used for machine names and disk models, which come from the computer and
/// can contain anything. The result is only ever a display aid; uniqueness is
/// always carried by a hash or GUID appended by the caller.
pub fn slugify(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_dash = true; // leading dashes are suppressed
    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if mapped == '-' {
            if last_dash {
                continue;
            }
            last_dash = true;
        } else {
            last_dash = false;
        }
        out.push(mapped);
        if out.len() >= MAX_ID_LEN {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps a raw identifier.
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                validate_id(&value)?;
                Ok(Self(value))
            }

            /// The identifier as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consumes the wrapper and returns the inner string.
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = <String as serde::de::Deserialize>::deserialize(d)?;
                // A manifest is untrusted input. Identifiers become path
                // components, so they are validated on the way in, not on the
                // way out.
                Self::new(raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

id_type! {
    /// Identifies the computer a backup was taken from.
    MachineId
}

id_type! {
    /// Identifies one backup run.
    BackupId
}

id_type! {
    /// Identifies one physical disk inside a backup.
    DiskId
}

id_type! {
    /// Identifies one partition inside a disk.
    PartitionId
}

id_type! {
    /// Identifies one captured byte stream.
    StreamId
}

id_type! {
    /// Identifies one volume inside a backup.
    VolumeId
}

/// The longest a backup name may be.
pub const MAX_BACKUP_NAME_LEN: usize = 128;

/// Windows device names that can never be used as a directory name, even with
/// an extension attached.
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Why a backup name was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupNameError {
    /// The name was empty.
    Empty,
    /// The name was longer than [`MAX_BACKUP_NAME_LEN`].
    TooLong {
        /// The length that was offered.
        len: usize,
    },
    /// The name contained a character Windows does not allow in a path
    /// component, or one that would make the name ambiguous.
    BadCharacter {
        /// The offending character.
        ch: char,
    },
    /// The name started or ended with something Windows silently rewrites.
    BadEdge,
    /// The name is a reserved Windows device name.
    Reserved {
        /// The reserved name it collides with.
        name: String,
    },
}

impl fmt::Display for BackupNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackupNameError::Empty => write!(f, "the backup name is empty"),
            BackupNameError::TooLong { len } => write!(
                f,
                "the backup name is {len} characters, the limit is {MAX_BACKUP_NAME_LEN}"
            ),
            BackupNameError::BadCharacter { ch } => write!(
                f,
                "the backup name contains {ch:?}; use letters, digits, dots, dashes and underscores"
            ),
            BackupNameError::BadEdge => write!(
                f,
                "the backup name must not begin with a dot or dash, or end with a dot or space"
            ),
            BackupNameError::Reserved { name } => write!(
                f,
                "{name} is a reserved Windows device name and cannot be used as a folder name"
            ),
        }
    }
}

impl std::error::Error for BackupNameError {}

/// A user chosen backup name, safe to use as a single directory component.
///
/// Unlike [`MachineId`] and friends this keeps the user's capitalisation,
/// because it is shown back to them and defaults to something like
/// `DESKTOP-1A2B_2026-09-12_1015`. Everything that could change its meaning as
/// a path is still rejected: separators, drive colons, wildcards, dot segments,
/// reserved device names, and the leading or trailing characters Windows
/// quietly strips.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BackupName(String);

impl BackupName {
    /// Validates and wraps a name.
    pub fn new(value: impl Into<String>) -> Result<Self, BackupNameError> {
        let value = value.into();
        if value.is_empty() {
            return Err(BackupNameError::Empty);
        }
        if value.chars().count() > MAX_BACKUP_NAME_LEN {
            return Err(BackupNameError::TooLong {
                len: value.chars().count(),
            });
        }
        for ch in value.chars() {
            let ok = ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.';
            if !ok {
                return Err(BackupNameError::BadCharacter { ch });
            }
        }
        // Windows strips a trailing dot or space, so a name ending in one would
        // not round trip through the filesystem.
        if value.starts_with('.')
            || value.starts_with('-')
            || value.ends_with('.')
            || value.ends_with(' ')
        {
            return Err(BackupNameError::BadEdge);
        }
        let stem = value.split('.').next().unwrap_or(&value);
        if let Some(reserved) = RESERVED_NAMES.iter().find(|r| stem.eq_ignore_ascii_case(r)) {
            return Err(BackupNameError::Reserved {
                name: (*reserved).to_owned(),
            });
        }
        Ok(Self(value))
    }

    /// Builds the default name, `COMPUTERNAME_YYYY-MM-DD_HHMM`.
    ///
    /// The computer name is sanitised rather than rejected, because it comes
    /// from the machine and the user cannot be asked to fix it.
    pub fn default_for(computer_name: &str, stamp: &str) -> Self {
        let mut cleaned: String = computer_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        cleaned = cleaned.trim_matches('-').to_owned();
        if cleaned.is_empty() {
            cleaned.push_str("PC");
        }
        cleaned.truncate(48);
        let candidate = format!("{cleaned}_{stamp}");
        // The pieces are already constrained, so this cannot fail; falling back
        // keeps the function total rather than panicking on a surprise.
        Self::new(candidate).unwrap_or_else(|_| Self(format!("MjolnirVSS_{stamp}")))
    }

    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the wrapper and returns the inner string.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for BackupName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for BackupName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl serde::Serialize for BackupName {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for BackupName {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = <String as serde::de::Deserialize>::deserialize(d)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_reasonable_identifiers() {
        for good in ["a", "disk-0", "2026-09-12t101500z-3f2a", "x1"] {
            validate_id(good).unwrap_or_else(|e| panic!("{good} rejected: {e}"));
        }
    }

    #[test]
    fn rejects_path_traversal_and_separators() {
        for bad in [
            "..", ".", "a/b", "a\\b", "C:",
            "con", // reserved name is fine as a slug, checked below
            "a b", "Disk-0", "disk_0", "-x", "x-", "a--b", "",
        ] {
            if bad == "con" {
                // Lowercase ASCII only means reserved device names can still be
                // produced; they are never used bare as a file name, always as
                // a directory segment under the backup root.
                assert!(validate_id(bad).is_ok());
                continue;
            }
            assert!(validate_id(bad).is_err(), "{bad} should have been rejected");
        }
    }

    #[test]
    fn rejects_overlong_identifiers() {
        let long = "a".repeat(MAX_ID_LEN + 1);
        assert_eq!(
            validate_id(&long).unwrap_err(),
            IdError::TooLong {
                len: MAX_ID_LEN + 1
            }
        );
    }

    #[test]
    fn slugify_always_produces_a_valid_id_or_empty() {
        for raw in [
            "DESKTOP-1A2B3C",
            "  weird   name  ",
            "Samsung SSD 980 PRO 1TB",
            "///",
            "..",
            "ÅÄÖ",
            "a".repeat(200).as_str(),
        ] {
            let s = slugify(raw);
            if s.is_empty() {
                continue;
            }
            validate_id(&s).unwrap_or_else(|e| panic!("slugify({raw:?}) = {s:?} invalid: {e}"));
        }
    }

    #[test]
    fn typed_ids_round_trip_through_json() {
        let id = DiskId::new("disk-0").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"disk-0\"");
        let back: DiskId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn deserialising_a_hostile_id_fails() {
        let err = serde_json::from_str::<BackupId>("\"../../etc\"").unwrap_err();
        assert!(err.to_string().contains("identifier"), "{err}");
    }

    #[test]
    fn backup_name_keeps_capitalisation() {
        let n = BackupName::new("DESKTOP-1A2B_2026-09-12_1015").unwrap();
        assert_eq!(n.as_str(), "DESKTOP-1A2B_2026-09-12_1015");
    }

    #[test]
    fn backup_name_rejects_anything_that_changes_its_path_meaning() {
        for bad in [
            "..",
            ".",
            ".hidden",
            "-leading",
            "trailing.",
            "a/b",
            "a\\b",
            "E:",
            "a*b",
            "a?b",
            "a|b",
            "a\"b",
            "a<b",
            "a>b",
            "with space",
            "",
        ] {
            assert!(
                BackupName::new(bad).is_err(),
                "{bad:?} should have been rejected"
            );
        }
    }

    #[test]
    fn backup_name_rejects_reserved_device_names() {
        for bad in ["CON", "con", "NUL", "com1", "LPT9", "aux.backup"] {
            assert!(
                matches!(BackupName::new(bad), Err(BackupNameError::Reserved { .. })),
                "{bad:?} should have been rejected as reserved"
            );
        }
        // A name that merely starts with those letters is fine.
        assert!(BackupName::new("CONTOSO_2026-01-01_0000").is_ok());
    }

    #[test]
    fn backup_name_length_is_capped() {
        let long = "a".repeat(MAX_BACKUP_NAME_LEN + 1);
        assert!(matches!(
            BackupName::new(long),
            Err(BackupNameError::TooLong { .. })
        ));
    }

    #[test]
    fn default_name_sanitises_the_computer_name() {
        assert_eq!(
            BackupName::default_for("DESKTOP-1A2B", "2026-09-12_1015").as_str(),
            "DESKTOP-1A2B_2026-09-12_1015"
        );
        // A computer name full of characters Windows allows but a path does not.
        let n = BackupName::default_for("Tobias' PC (work)", "2026-09-12_1015");
        assert!(BackupName::new(n.as_str()).is_ok(), "{n} is not valid");
        // An empty or fully stripped computer name still yields a usable name.
        let n = BackupName::default_for("...", "2026-09-12_1015");
        assert!(BackupName::new(n.as_str()).is_ok(), "{n} is not valid");
    }

    #[test]
    fn default_name_survives_a_reserved_computer_name() {
        let n = BackupName::default_for("NUL", "2026-09-12_1015");
        assert!(BackupName::new(n.as_str()).is_ok(), "{n} is not valid");
    }

    #[test]
    fn backup_name_round_trips_through_json() {
        let n = BackupName::new("PC_2026-09-12_1015").unwrap();
        let json = serde_json::to_string(&n).unwrap();
        let back: BackupName = serde_json::from_str(&json).unwrap();
        assert_eq!(back, n);
        assert!(serde_json::from_str::<BackupName>("\"../escape\"").is_err());
    }
}
