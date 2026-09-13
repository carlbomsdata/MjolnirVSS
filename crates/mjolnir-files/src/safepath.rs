//! Turning a path out of a backup into a path it is safe to write to.
//!
//! # Why this is its own module
//!
//! Every name extraction writes comes out of somebody else's filesystem. It has
//! not been checked by this program, it may have been written by something
//! hostile, and it may simply be old and strange. Joining such a name onto a
//! folder the operator chose is the single place where "get my files back" can
//! turn into "overwrite something else", so the rules for doing it live in one
//! module with one set of tests.
//!
//! # What is refused
//!
//! * anything that would climb out of the chosen folder: `..`, an absolute
//!   path, a drive letter, a leading separator;
//! * a name Windows reserves for a device, like `CON` or `LPT1`, in any case
//!   and with any extension, because opening one writes to a device;
//! * a name ending in a dot or a space, which Windows strips, so that
//!   `secret.txt.` cannot be used to land on `secret.txt`;
//! * a character Windows forbids in a name;
//! * an alternate data stream separator, so a name cannot be used to write into
//!   a stream of an existing file.
//!
//! Nothing here silently repairs a name. A refused name is reported with the
//! reason, because a file whose name could not be reproduced is something the
//! operator should know about rather than something to guess at.

use std::path::{Component, Path, PathBuf};

/// Why a name from a backup cannot be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathRefusal {
    /// The path is empty, or every part of it was.
    Empty,
    /// The path tries to climb out of the folder it would be written into.
    Escapes {
        /// The offending part.
        part: String,
    },
    /// The name is one Windows treats as a device.
    ReservedDeviceName {
        /// The offending part.
        part: String,
    },
    /// The name holds a character Windows does not allow.
    ForbiddenCharacter {
        /// The offending part.
        part: String,
        /// The character.
        character: char,
    },
    /// The name ends in something Windows would strip.
    TrailingDotOrSpace {
        /// The offending part.
        part: String,
    },
}

impl PathRefusal {
    /// The reason, in words.
    pub fn describe(&self) -> String {
        match self {
            PathRefusal::Empty => "the path is empty".to_owned(),
            PathRefusal::Escapes { part } => format!(
                "{part:?} would put the file outside the folder you chose"
            ),
            PathRefusal::ReservedDeviceName { part } => format!(
                "{part:?} is a name Windows reserves for a device, so writing it would write to the device"
            ),
            PathRefusal::ForbiddenCharacter { part, character } => format!(
                "{part:?} contains {character:?}, which Windows does not allow in a file name"
            ),
            PathRefusal::TrailingDotOrSpace { part } => format!(
                "{part:?} ends in a dot or a space, which Windows strips, so the file would land somewhere else"
            ),
        }
    }
}

/// Names Windows treats as devices, whatever the extension.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Characters Windows does not allow in a file name.
///
/// The colon is here twice over: it separates a drive letter, and it separates
/// an alternate data stream, and a name carrying one could be used for either.
const FORBIDDEN: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Checks one part of a path.
pub fn sanitise_component(part: &str) -> Result<&str, PathRefusal> {
    if part.is_empty() {
        return Err(PathRefusal::Empty);
    }
    if part == "." || part == ".." {
        return Err(PathRefusal::Escapes {
            part: part.to_owned(),
        });
    }
    for character in part.chars() {
        if FORBIDDEN.contains(&character) || (character as u32) < 0x20 {
            return Err(PathRefusal::ForbiddenCharacter {
                part: part.to_owned(),
                character,
            });
        }
    }
    if part.ends_with('.') || part.ends_with(' ') {
        return Err(PathRefusal::TrailingDotOrSpace {
            part: part.to_owned(),
        });
    }

    // A reserved name is reserved whatever follows the first dot, so `CON.txt`
    // is refused as well as `CON`.
    let stem = part.split('.').next().unwrap_or(part);
    if RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
        return Err(PathRefusal::ReservedDeviceName {
            part: part.to_owned(),
        });
    }
    Ok(part)
}

/// Joins a path from a backup onto a folder, safely.
///
/// The result is always inside `into`. A path that could not be made safe is
/// refused with the reason rather than repaired.
pub fn safe_join(into: &Path, relative: &str) -> Result<PathBuf, PathRefusal> {
    let trimmed = relative.trim_matches(|c| c == '\\' || c == '/');
    if trimmed.is_empty() {
        return Err(PathRefusal::Empty);
    }

    let mut out = into.to_path_buf();
    let mut any = false;
    for part in trimmed.split(['\\', '/']) {
        if part.is_empty() {
            continue;
        }
        out.push(sanitise_component(part)?);
        any = true;
    }
    if !any {
        return Err(PathRefusal::Empty);
    }

    // Belt and braces. Nothing above can produce a component that climbs, but
    // this is the check that would catch a change to the rules above, and it
    // costs nothing.
    for component in out.strip_prefix(into).unwrap_or(&out).components() {
        match component {
            Component::Normal(_) => {}
            other => {
                return Err(PathRefusal::Escapes {
                    part: other.as_os_str().to_string_lossy().into_owned(),
                })
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_path_is_joined() {
        let into = Path::new(r"C:\Recovered");
        assert_eq!(
            safe_join(into, r"Users\tobias\notes.txt").unwrap(),
            into.join("Users").join("tobias").join("notes.txt")
        );
        assert_eq!(
            safe_join(into, "Users/tobias/notes.txt").unwrap(),
            into.join("Users").join("tobias").join("notes.txt")
        );
        assert_eq!(
            safe_join(into, r"\readme.txt").unwrap(),
            into.join("readme.txt")
        );
    }

    /// The one that matters: nothing from a backup may reach outside the folder
    /// the operator chose.
    #[test]
    fn nothing_climbs_out_of_the_chosen_folder() {
        let into = Path::new(r"C:\Recovered");
        for attempt in [
            r"..\outside.txt",
            r"Users\..\..\outside.txt",
            r"..",
            r"a\..\..\b",
            r"./../x",
            "../../../../../../Windows/System32/drivers/etc/hosts",
        ] {
            let result = safe_join(into, attempt);
            assert!(
                matches!(result, Err(PathRefusal::Escapes { .. })),
                "{attempt:?} was not refused: {result:?}"
            );
        }
    }

    /// A path with a drive letter in it is an absolute path wearing a disguise,
    /// and the colon is what gives it away.
    #[test]
    fn an_absolute_path_is_refused() {
        let into = Path::new(r"C:\Recovered");
        for attempt in [r"C:\Windows\System32\x.dll", "D:/x", r"C:x"] {
            assert!(
                safe_join(into, attempt).is_err(),
                "{attempt:?} was not refused"
            );
        }
    }

    /// Opening `CON` writes to the console, and `LPT1` to a port. A backup
    /// holding such a name is not necessarily hostile, but writing it is
    /// always wrong.
    #[test]
    fn a_reserved_device_name_is_refused() {
        for name in [
            "CON",
            "con",
            "Con",
            "NUL",
            "AUX",
            "PRN",
            "COM1",
            "LPT9",
            "CON.txt",
            "nul.dat",
            "COM1.anything.at.all",
        ] {
            let result = sanitise_component(name);
            assert!(
                matches!(result, Err(PathRefusal::ReservedDeviceName { .. })),
                "{name:?} was not refused: {result:?}"
            );
        }
    }

    /// A name that merely starts with a reserved word is an ordinary name.
    #[test]
    fn a_name_that_only_looks_reserved_is_allowed() {
        for name in [
            "CONFIG",
            "console.log",
            "AUXILIARY",
            "COM10",
            "LPT10",
            "PRNT",
        ] {
            assert!(sanitise_component(name).is_ok(), "{name:?} was refused");
        }
    }

    /// Windows strips a trailing dot, so `secret.txt.` would land on
    /// `secret.txt` and overwrite something that was already there.
    #[test]
    fn a_trailing_dot_or_space_is_refused() {
        for name in ["secret.txt.", "file ", "x..", "name. "] {
            let result = sanitise_component(name);
            assert!(
                matches!(
                    result,
                    Err(PathRefusal::TrailingDotOrSpace { .. }) | Err(PathRefusal::Escapes { .. })
                ),
                "{name:?} was not refused: {result:?}"
            );
        }
    }

    /// A colon would open an alternate data stream of an existing file.
    #[test]
    fn a_stream_separator_is_refused() {
        let result = sanitise_component("notes.txt:hidden");
        assert!(
            matches!(
                result,
                Err(PathRefusal::ForbiddenCharacter { character: ':', .. })
            ),
            "{result:?}"
        );
    }

    #[test]
    fn forbidden_characters_are_refused() {
        for (name, character) in [
            ("a<b", '<'),
            ("a>b", '>'),
            ("a\"b", '"'),
            ("a|b", '|'),
            ("a?b", '?'),
            ("a*b", '*'),
        ] {
            match sanitise_component(name) {
                Err(PathRefusal::ForbiddenCharacter { character: c, .. }) => {
                    assert_eq!(c, character, "{name:?}")
                }
                other => panic!("{name:?} was not refused: {other:?}"),
            }
        }
    }

    #[test]
    fn a_control_character_is_refused() {
        assert!(sanitise_component("a\u{0007}b").is_err());
        assert!(sanitise_component("a\nb").is_err());
    }

    /// Unicode is ordinary and must survive. A recovery tool that cannot get
    /// back a file with an accent in its name is not a recovery tool.
    #[test]
    fn unicode_names_are_kept_exactly() {
        let into = Path::new(r"C:\Recovered");
        for name in [
            "\u{00e4}\u{00f6}\u{00fc}.txt",
            "\u{6587}\u{4ef6}.bin",
            "\u{03a9}mega",
            "emoji \u{1f600} here.txt",
            "\u{0440}\u{0443}\u{0441}\u{0441}\u{043a}\u{0438}\u{0439}.doc",
        ] {
            let joined = safe_join(into, name).unwrap_or_else(|e| panic!("{name:?}: {e:?}"));
            assert_eq!(joined, into.join(name));
        }
    }

    #[test]
    fn an_empty_path_is_refused() {
        let into = Path::new(r"C:\Recovered");
        for attempt in ["", "\\", "/", "///", "\\\\"] {
            assert_eq!(safe_join(into, attempt), Err(PathRefusal::Empty));
        }
    }

    #[test]
    fn every_refusal_explains_itself() {
        let refusals = [
            PathRefusal::Empty,
            PathRefusal::Escapes {
                part: "..".to_owned(),
            },
            PathRefusal::ReservedDeviceName {
                part: "CON".to_owned(),
            },
            PathRefusal::ForbiddenCharacter {
                part: "a|b".to_owned(),
                character: '|',
            },
            PathRefusal::TrailingDotOrSpace {
                part: "x.".to_owned(),
            },
        ];
        for refusal in refusals {
            let text = refusal.describe();
            assert!(!text.is_empty());
            assert!(text.len() > 10, "{text:?} is not an explanation");
        }
    }

    /// A long path is not by itself dangerous and must not be refused: a
    /// backup of a real machine has plenty of them.
    #[test]
    fn a_deep_path_is_allowed() {
        let into = Path::new(r"C:\R");
        let deep: Vec<String> = (0..40).map(|i| format!("level-{i:02}")).collect();
        let joined = safe_join(into, &deep.join("\\")).unwrap();
        assert!(joined.starts_with(into));
        assert_eq!(
            joined.strip_prefix(into).unwrap().components().count(),
            40,
            "every level should have survived"
        );
    }
}
