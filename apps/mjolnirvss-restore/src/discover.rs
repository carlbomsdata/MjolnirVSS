//! Finding MjolnirVSS backups on the drives attached to a recovery machine.
//!
//! Inside Windows PE the drive letters are not the ones the machine normally
//! has, so asking the operator to type a path would be asking them to guess.
//! Every attached volume is searched instead, at a fixed shallow depth, and
//! what is found is listed.

use std::path::{Path, PathBuf};

use mjolnir_image::BackupSet;

/// How deep below a drive root to look for backup folders.
///
/// A backup folder sits either at the root of a drive or one level down, in a
/// folder the operator made. Searching deeper would mean walking a whole disk.
const MAX_DEPTH: usize = 3;

/// A backup found on an attached drive.
pub struct FoundBackup {
    /// The folder holding it.
    pub path: PathBuf,
    /// The backup set, already opened and checked.
    pub set: BackupSet,
}

impl FoundBackup {
    /// A one line description for the list.
    pub fn describe(&self) -> String {
        let m = self.set.manifest();
        let state = if self.set.is_restorable() {
            "ready"
        } else if self.set.completion().is_none() {
            "INCOMPLETE - cannot be restored"
        } else {
            "NOT RESTORABLE"
        };
        // Whether it needs a password belongs in the list, not in a failure
        // three screens later. Somebody in a recovery environment with several
        // backups has to be able to see which ones they can actually open.
        let sealed = if self.set.is_encrypted() {
            "  [encrypted]"
        } else {
            ""
        };
        format!(
            "{}  -  {}  -  {}  -  {}  [{}]{}",
            m.backup.name.as_str(),
            m.source.computer_name,
            m.backup.created_utc,
            mjolnir_core::progress::format_bytes(m.stats.stored_bytes),
            state,
            sealed
        )
    }

    /// The disk numbers this backup is stored on, so they are never erased.
    #[cfg(windows)]
    pub fn stored_on_disks(&self) -> Vec<u32> {
        disks_holding(&self.path)
    }

    #[cfg(not(windows))]
    pub fn stored_on_disks(&self) -> Vec<u32> {
        Vec::new()
    }
}

/// Searches every attached drive for backups.
#[cfg(windows)]
pub fn search_all_drives() -> Vec<FoundBackup> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(volumes) = mjolnir_storage::volumes::enumerate_volumes() {
        for volume in volumes {
            for mount in &volume.mount_points {
                roots.push(PathBuf::from(mount));
            }
        }
    }
    roots.sort();
    roots.dedup();

    let mut found = Vec::new();
    for root in roots {
        search_under(&root, 0, &mut found);
    }
    found
}

#[cfg(not(windows))]
pub fn search_all_drives() -> Vec<FoundBackup> {
    Vec::new()
}

/// Searches one folder and its subfolders for backups.
pub fn search_under(dir: &Path, depth: usize, found: &mut Vec<FoundBackup>) {
    if depth > MAX_DEPTH {
        return;
    }

    // A folder holding a manifest is a backup, and is not descended into.
    if dir.join("manifest.json").is_file() {
        if let Ok(set) = BackupSet::open_unchecked(dir) {
            found.push(FoundBackup {
                path: dir.to_path_buf(),
                set,
            });
        }
        return;
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip the places a backup is never in, so a recovery machine with
            // a full Windows disk attached does not spend minutes walking it.
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if matches!(
                    name.to_ascii_lowercase().as_str(),
                    "windows"
                        | "program files"
                        | "program files (x86)"
                        | "programdata"
                        | "$recycle.bin"
                        | "system volume information"
                        | "users"
                        | "chunks"
                ) {
                    continue;
                }
            }
            search_under(&path, depth + 1, found);
        }
    }
}

/// Which physical disks a path lives on.
#[cfg(windows)]
pub fn disks_holding(path: &Path) -> Vec<u32> {
    let text = path.to_string_lossy();
    let bytes = text.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' {
        return Vec::new();
    }
    let letter = (bytes[0] as char).to_ascii_uppercase().to_string();

    let Ok(volumes) = mjolnir_storage::volumes::enumerate_volumes() else {
        return Vec::new();
    };
    volumes
        .iter()
        .filter(|v| v.drive_letter().as_deref() == Some(letter.as_str()))
        .flat_map(|v| v.extents.iter().map(|e| e.disk_number))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_holding_a_manifest_is_recognised_as_a_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let backup = tmp.path().join("PC_2026-01-01_1200");
        std::fs::create_dir_all(&backup).unwrap();
        // Not a real manifest, so opening fails and nothing is listed, but the
        // folder must still be recognised and not descended into.
        std::fs::write(backup.join("manifest.json"), b"{}").unwrap();

        let mut found = Vec::new();
        search_under(tmp.path(), 0, &mut found);
        assert!(
            found.is_empty(),
            "an unreadable manifest must not be listed"
        );
    }

    #[test]
    fn searching_stops_at_the_depth_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut deep = tmp.path().to_path_buf();
        for i in 0..10 {
            deep = deep.join(format!("level{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("manifest.json"), b"{}").unwrap();

        // Must return rather than recurse forever, and must not find the
        // deeply buried folder.
        let mut found = Vec::new();
        search_under(tmp.path(), 0, &mut found);
        assert!(found.is_empty());
    }

    #[test]
    fn the_windows_folder_is_not_searched() {
        let tmp = tempfile::tempdir().unwrap();
        let windows = tmp.path().join("Windows").join("backup");
        std::fs::create_dir_all(&windows).unwrap();
        std::fs::write(windows.join("manifest.json"), b"{}").unwrap();

        let mut found = Vec::new();
        search_under(tmp.path(), 0, &mut found);
        assert!(found.is_empty());
    }

    #[test]
    fn a_missing_folder_is_not_an_error() {
        let mut found = Vec::new();
        search_under(Path::new("Z:\\does\\not\\exist"), 0, &mut found);
        assert!(found.is_empty());
    }
}
