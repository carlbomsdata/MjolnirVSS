//! Where the files of a backup live on the destination drive.
//!
//! Paths are built only from validated pieces: a [`BackupName`] that has
//! already been checked as a safe Windows path component, and identifiers that
//! reject separators and dot segments at parse time. Nothing in this module
//! joins a raw string that came out of a manifest.

use std::path::{Path, PathBuf};

use mjolnir_core::ids::{BackupName, VolumeId};

use crate::hash::ChunkHash;

/// The manifest filename.
pub const MANIFEST_FILE: &str = "manifest.json";

/// The disk layout filename.
pub const DISK_LAYOUT_FILE: &str = "disk-layout.json";

/// The completion marker filename, written last.
pub const COMPLETION_FILE: &str = "completion.json";

/// Directory holding the logs of the run that produced the backup.
pub const LOGS_DIR: &str = "logs";

/// Filename of the backup log.
pub const BACKUP_LOG_FILE: &str = "backup.log";

/// Filename of the verification log.
pub const VERIFY_LOG_FILE: &str = "verify.log";

/// Filename of a restore log, written next to the backup it was restored from.
pub const RESTORE_LOG_FILE: &str = "restore.log";

/// Directory holding per volume file indexes.
pub const INDEXES_DIR: &str = "indexes";

/// Extension of a zstd compressed chunk file.
pub const CHUNK_EXT: &str = "zst";

/// Suffix used while a file is still being written.
pub const TEMP_SUFFIX: &str = ".tmp";

/// Paths inside one backup folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupLayout {
    dir: PathBuf,
    chunk_root: String,
    fanout: u8,
}

impl BackupLayout {
    /// Builds a layout for an existing backup folder.
    ///
    /// `chunk_root` and `fanout` come from the manifest, which is why they are
    /// parameters rather than constants: a later format version can point a
    /// backup at a store shared by every backup of the machine.
    pub fn new(dir: impl Into<PathBuf>, chunk_root: impl Into<String>, fanout: u8) -> Self {
        Self {
            dir: dir.into(),
            chunk_root: chunk_root.into(),
            fanout,
        }
    }

    /// Builds a layout using the default chunk store settings.
    pub fn with_defaults(dir: impl Into<PathBuf>) -> Self {
        let spec = crate::manifest::ChunkStoreSpec::default();
        Self::new(dir, spec.root, spec.fanout)
    }

    /// Builds a layout for a new backup called `name` under `destination`.
    pub fn for_new_backup(destination: impl AsRef<Path>, name: &BackupName) -> Self {
        Self::with_defaults(destination.as_ref().join(name.as_str()))
    }

    /// The backup folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The manifest path.
    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join(MANIFEST_FILE)
    }

    /// The disk layout path.
    pub fn disk_layout_path(&self) -> PathBuf {
        self.dir.join(DISK_LAYOUT_FILE)
    }

    /// The completion marker path.
    pub fn completion_path(&self) -> PathBuf {
        self.dir.join(COMPLETION_FILE)
    }

    /// The chunk store root.
    pub fn chunk_root(&self) -> PathBuf {
        self.dir.join(&self.chunk_root)
    }

    /// How many leading digest characters name a chunk subdirectory.
    pub fn fanout(&self) -> u8 {
        self.fanout
    }

    /// The directory a chunk with this digest belongs in.
    pub fn chunk_dir(&self, hash: ChunkHash) -> PathBuf {
        if self.fanout == 0 {
            self.chunk_root()
        } else {
            self.chunk_root()
                .join(hash.hex_prefix(self.fanout as usize))
        }
    }

    /// The final path of a chunk.
    pub fn chunk_path(&self, hash: ChunkHash) -> PathBuf {
        self.chunk_dir(hash)
            .join(format!("{}.{CHUNK_EXT}", hash.to_hex()))
    }

    /// A temporary path for a chunk still being written.
    ///
    /// The unique suffix keeps two MjolnirVSS processes writing the same chunk
    /// from clobbering each other's partial file. Whichever finishes first
    /// renames into place; the other rename replaces identical content.
    pub fn chunk_temp_path(&self, hash: ChunkHash, unique: u64) -> PathBuf {
        self.chunk_dir(hash).join(format!(
            "{}.{CHUNK_EXT}.{unique:016x}{TEMP_SUFFIX}",
            hash.to_hex()
        ))
    }

    /// The logs directory.
    pub fn logs_dir(&self) -> PathBuf {
        self.dir.join(LOGS_DIR)
    }

    /// The backup log path.
    pub fn backup_log_path(&self) -> PathBuf {
        self.logs_dir().join(BACKUP_LOG_FILE)
    }

    /// The verification log path.
    pub fn verify_log_path(&self) -> PathBuf {
        self.logs_dir().join(VERIFY_LOG_FILE)
    }

    /// The restore log path.
    pub fn restore_log_path(&self) -> PathBuf {
        self.logs_dir().join(RESTORE_LOG_FILE)
    }

    /// The indexes directory.
    pub fn indexes_dir(&self) -> PathBuf {
        self.dir.join(INDEXES_DIR)
    }

    /// The path of one volume's file index.
    pub fn volume_index_path(&self, volume: &VolumeId) -> PathBuf {
        self.indexes_dir()
            .join(format!("volume-{}.json", volume.as_str()))
    }

    /// The relative path of one volume's file index, as recorded in the
    /// manifest. Always uses forward slashes so the format stays portable.
    pub fn volume_index_relative(volume: &VolumeId) -> String {
        format!("{INDEXES_DIR}/volume-{}.json", volume.as_str())
    }

    /// Resolves a relative path recorded in a document to an absolute path.
    ///
    /// Refuses anything that is not a safe relative path, so a hostile manifest
    /// cannot make a reader open a file outside the backup folder.
    pub fn resolve_relative(&self, relative: &str) -> Option<PathBuf> {
        if !crate::manifest::is_safe_relative_path(relative) {
            return None;
        }
        let mut path = self.dir.clone();
        for segment in relative.split('/') {
            path.push(segment);
        }
        Some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name() -> BackupName {
        BackupName::new("DESKTOP-1A2B_2026-09-12_1015").unwrap()
    }

    #[test]
    fn a_new_backup_goes_in_its_own_folder_under_the_destination() {
        let l = BackupLayout::for_new_backup(Path::new("E:\\Backups"), &name());
        assert_eq!(
            l.dir(),
            Path::new("E:\\Backups\\DESKTOP-1A2B_2026-09-12_1015")
        );
        assert_eq!(
            l.manifest_path(),
            Path::new("E:\\Backups\\DESKTOP-1A2B_2026-09-12_1015\\manifest.json")
        );
    }

    #[test]
    fn the_three_documents_sit_at_the_top_of_the_folder() {
        let l = BackupLayout::with_defaults("B");
        assert_eq!(l.manifest_path(), Path::new("B").join("manifest.json"));
        assert_eq!(
            l.disk_layout_path(),
            Path::new("B").join("disk-layout.json")
        );
        assert_eq!(l.completion_path(), Path::new("B").join("completion.json"));
    }

    #[test]
    fn chunks_are_flat_by_default_and_nested_when_asked() {
        let hash = ChunkHash::of(b"");
        let hex = hash.to_hex();

        let flat = BackupLayout::with_defaults("B");
        assert_eq!(flat.fanout(), 0);
        assert_eq!(
            flat.chunk_path(hash),
            Path::new("B").join("chunks").join(format!("{hex}.zst"))
        );

        let nested = BackupLayout::new("B", "chunks", 2);
        assert_eq!(
            nested.chunk_path(hash),
            Path::new("B")
                .join("chunks")
                .join("af")
                .join(format!("{hex}.zst"))
        );
    }

    #[test]
    fn temp_chunk_paths_are_unique_and_distinguishable() {
        let l = BackupLayout::with_defaults("B");
        let hash = ChunkHash::of(b"x");
        let a = l.chunk_temp_path(hash, 1);
        let b = l.chunk_temp_path(hash, 2);
        assert_ne!(a, b);
        assert!(a.to_string_lossy().ends_with(".tmp"));
        assert_ne!(a, l.chunk_path(hash));
    }

    #[test]
    fn volume_index_paths_agree_between_absolute_and_relative_forms() {
        let l = BackupLayout::with_defaults("B");
        let v = VolumeId::new("volume-1").unwrap();
        let relative = BackupLayout::volume_index_relative(&v);
        assert_eq!(relative, "indexes/volume-volume-1.json");
        assert_eq!(
            l.resolve_relative(&relative).unwrap(),
            l.volume_index_path(&v)
        );
    }

    #[test]
    fn resolving_a_hostile_relative_path_is_refused() {
        let l = BackupLayout::with_defaults("B");
        for hostile in [
            "../escape",
            "/absolute",
            "C:/windows",
            "indexes\\back",
            "indexes/../../x",
            "",
        ] {
            assert!(
                l.resolve_relative(hostile).is_none(),
                "{hostile:?} was resolved"
            );
        }
    }

    #[test]
    fn every_path_stays_under_the_backup_folder() {
        let l = BackupLayout::with_defaults("B");
        let v = VolumeId::new("volume-1").unwrap();
        for p in [
            l.manifest_path(),
            l.disk_layout_path(),
            l.completion_path(),
            l.chunk_path(ChunkHash::of(b"x")),
            l.chunk_temp_path(ChunkHash::of(b"x"), 7),
            l.backup_log_path(),
            l.verify_log_path(),
            l.restore_log_path(),
            l.volume_index_path(&v),
        ] {
            assert!(p.starts_with("B"), "{p:?} escaped the backup folder");
            assert!(
                !p.to_string_lossy().contains(".."),
                "{p:?} contains a parent segment"
            );
        }
    }
}
