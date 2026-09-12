//! Damaging a backup on purpose.
//!
//! A verifier is only worth having if it has been shown to fail. These helpers
//! produce the specific kinds of damage a backup actually suffers: a chunk file
//! that a failing drive returned different bytes for, a copy that was
//! interrupted halfway, a file that never made it, and a manifest somebody
//! edited.

use std::fs;
use std::path::{Path, PathBuf};

use mjolnir_core::error::{Error, Result};

/// Lists the chunk files in a backup folder.
pub fn chunk_files(backup_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect(&backup_dir.join("chunks"), &mut out)?;
    out.sort();
    Ok(out)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).map_err(|e| Error::io(dir.display(), e))? {
        let entry = entry.map_err(|e| Error::io(dir.display(), e))?;
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("zst") {
            out.push(path);
        }
    }
    Ok(())
}

/// Flips bits in the middle of one chunk file.
///
/// Models a drive that returned different bytes than were written. The file
/// keeps its name and its size, so only the digest can catch it.
pub fn corrupt_chunk(path: &Path) -> Result<()> {
    let mut bytes = fs::read(path).map_err(|e| Error::io(path.display(), e))?;
    if bytes.is_empty() {
        return Err(Error::io(
            path.display(),
            std::io::Error::new(std::io::ErrorKind::InvalidData, "the chunk file is empty"),
        ));
    }
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xFF;
    fs::write(path, &bytes).map_err(|e| Error::io(path.display(), e))
}

/// Cuts a chunk file in half.
///
/// Models a copy that stopped partway, or a drive that filled up.
pub fn truncate_chunk(path: &Path) -> Result<()> {
    let bytes = fs::read(path).map_err(|e| Error::io(path.display(), e))?;
    fs::write(path, &bytes[..bytes.len() / 2]).map_err(|e| Error::io(path.display(), e))
}

/// Deletes a chunk file.
///
/// Models a backup folder that was copied incompletely.
pub fn remove_chunk(path: &Path) -> Result<()> {
    fs::remove_file(path).map_err(|e| Error::io(path.display(), e))
}

/// Removes the completion marker, making a finished backup look interrupted.
pub fn make_incomplete(backup_dir: &Path) -> Result<()> {
    let path = backup_dir.join("completion.json");
    if path.exists() {
        fs::remove_file(&path).map_err(|e| Error::io(path.display(), e))?;
    }
    Ok(())
}

/// Replaces some text in a document, the way a person editing it would.
pub fn edit_document(path: &Path, from: &str, to: &str) -> Result<()> {
    let text = fs::read_to_string(path).map_err(|e| Error::io(path.display(), e))?;
    if !text.contains(from) {
        return Err(Error::io(
            path.display(),
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("the document does not contain {from:?}"),
            ),
        ));
    }
    fs::write(path, text.replace(from, to)).map_err(|e| Error::io(path.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_at(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let chunks = dir.join("chunks");
        fs::create_dir_all(&chunks).unwrap();
        let path = chunks.join(format!("{name}.zst"));
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn chunk_files_are_found_including_in_subdirectories() {
        let tmp = tempfile::tempdir().unwrap();
        chunk_at(tmp.path(), "aaaa", b"one");
        let nested = tmp.path().join("chunks").join("ab");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("bbbb.zst"), b"two").unwrap();
        // Something that is not a chunk must be ignored.
        fs::write(tmp.path().join("chunks").join("notes.txt"), b"x").unwrap();

        let found = chunk_files(tmp.path()).unwrap();
        assert_eq!(found.len(), 2, "{found:?}");
    }

    #[test]
    fn corrupting_a_chunk_keeps_its_size_but_changes_its_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let path = chunk_at(tmp.path(), "aaaa", b"0123456789");
        corrupt_chunk(&path).unwrap();

        let after = fs::read(&path).unwrap();
        assert_eq!(after.len(), 10, "size must not change");
        assert_ne!(after, b"0123456789", "contents must change");
    }

    #[test]
    fn truncating_a_chunk_halves_it() {
        let tmp = tempfile::tempdir().unwrap();
        let path = chunk_at(tmp.path(), "aaaa", b"0123456789");
        truncate_chunk(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"01234");
    }

    #[test]
    fn removing_a_chunk_removes_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = chunk_at(tmp.path(), "aaaa", b"x");
        remove_chunk(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn making_a_backup_incomplete_is_safe_to_repeat() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("completion.json"), b"{}").unwrap();
        make_incomplete(tmp.path()).unwrap();
        assert!(!tmp.path().join("completion.json").exists());
        // A second call must not fail.
        make_incomplete(tmp.path()).unwrap();
    }

    #[test]
    fn editing_a_document_that_does_not_contain_the_text_fails_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("manifest.json");
        fs::write(&path, "{\"a\": 1}").unwrap();
        assert!(edit_document(&path, "missing", "x").is_err());
        edit_document(&path, "\"a\"", "\"b\"").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"b\": 1}");
    }
}
