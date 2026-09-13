//! The content addressed chunk store.
//!
//! Chunks are immutable and named by the BLAKE3 digest of their uncompressed
//! contents, so writing the same bytes twice is a no-op and a later incremental
//! backup can reuse whatever is already there.
//!
//! Every write lands atomically: the bytes go to a uniquely named temporary
//! file in the destination directory, the file is flushed to stable storage,
//! and only then is it renamed onto its final name. A process killed at any
//! point leaves either nothing or a complete, correctly named chunk.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;

use crate::hash::ChunkHash;
use crate::layout::BackupLayout;
use crate::manifest::{CompressionAlgorithm, CompressionSpec};

/// Counter making temporary filenames unique within this process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    // Mixed with the process id so two MjolnirVSS processes writing to the same
    // destination never choose the same temporary name.
    (u64::from(std::process::id()) << 32) ^ counter
}

/// What happened when a chunk was offered to the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PutOutcome {
    /// Digest of the uncompressed contents.
    pub hash: ChunkHash,
    /// Size of the stored file in bytes.
    pub compressed_size: u64,
    /// Whether the chunk had to be written, as opposed to already being there.
    pub written: bool,
}

/// Reads and writes chunks under one backup directory.
#[derive(Debug)]
pub struct ChunkStore {
    layout: BackupLayout,
    compression: CompressionSpec,
    /// Present when the backup is encrypted.
    ///
    /// Shared rather than copied: key material should exist in one place, and
    /// a store is handed around by value.
    keys: Option<std::sync::Arc<mjolnir_crypto::Keys>>,
    /// The backup is sealed and no password has been given.
    ///
    /// Without this the store would try to decompress ciphertext and report
    /// "the compressed stream is damaged", sending somebody to check their
    /// drive for a fault that is really a missing password.
    locked: bool,
}

impl ChunkStore {
    /// Opens a store over `layout` using `compression` for new chunks.
    pub fn new(layout: BackupLayout, compression: CompressionSpec) -> Self {
        Self {
            layout,
            compression,
            keys: None,
            locked: false,
        }
    }

    /// The same store, marked as sealed with no password given.
    pub fn locked(mut self) -> Self {
        self.locked = true;
        self
    }

    /// The same store, sealing and opening chunks with `keys`.
    ///
    /// Chunks are compressed first and sealed second. The other order would
    /// mean compressing ciphertext, which does not compress.
    pub fn with_keys(mut self, keys: std::sync::Arc<mjolnir_crypto::Keys>) -> Self {
        self.keys = Some(keys);
        self.locked = false;
        self
    }

    /// Refuses early, and by name, when there is no password to read with.
    fn check_unlocked(&self) -> Result<()> {
        if self.locked {
            return Err(Error::new(
                ExitCode::Failure,
                "this backup is encrypted and no password has been given",
                "its contents are sealed, so nothing can be read out of it until it is unlocked"
                    .to_owned(),
                "run the command again with the password for this backup",
            ));
        }
        Ok(())
    }

    /// Whether this store seals what it writes.
    pub fn is_encrypted(&self) -> bool {
        self.keys.is_some()
    }

    /// The name `data` is stored under.
    ///
    /// An encrypted backup names chunks with a digest keyed by the password, so
    /// that the names on the drive do not say what the chunks contain.
    fn name_of(&self, data: &[u8]) -> ChunkHash {
        match &self.keys {
            Some(keys) => ChunkHash::from_bytes(keys.name_of(data)),
            None => ChunkHash::of(data),
        }
    }

    /// The layout this store writes through.
    pub fn layout(&self) -> &BackupLayout {
        &self.layout
    }

    /// The compression settings applied to new chunks.
    pub fn compression(&self) -> CompressionSpec {
        self.compression
    }

    /// Whether a chunk is already present.
    pub fn contains(&self, hash: ChunkHash) -> bool {
        self.layout.chunk_path(hash).is_file()
    }

    /// The path a chunk occupies.
    pub fn path_of(&self, hash: ChunkHash) -> PathBuf {
        self.layout.chunk_path(hash)
    }

    /// Stores `data`, returning its digest.
    ///
    /// If the digest is already present the bytes are not rewritten. That makes
    /// the operation idempotent, which is what lets a backup be resumed later
    /// without redoing work, and what makes deduplication free.
    pub fn put(&self, data: &[u8]) -> Result<PutOutcome> {
        self.check_unlocked()?;
        if data.is_empty() {
            return Err(Error::new(
                ExitCode::Failure,
                "an empty chunk was offered to the chunk store",
                "a zero length chunk carries no data but would still occupy a manifest entry, so it is refused rather than written",
                "this is an internal error; please report it with the command you ran",
            ));
        }

        let hash = self.name_of(data);
        let final_path = self.layout.chunk_path(hash);

        if let Ok(meta) = fs::metadata(&final_path) {
            if meta.is_file() && meta.len() > 0 {
                return Ok(PutOutcome {
                    hash,
                    compressed_size: meta.len(),
                    written: false,
                });
            }
        }

        let dir = self.layout.chunk_dir(hash);
        fs::create_dir_all(&dir).map_err(|e| Error::io(dir.display(), e))?;

        let temp_path = self.layout.chunk_temp_path(hash, unique_suffix());
        let compressed_size = match self.write_temp(&temp_path, data) {
            Ok(size) => size,
            Err(e) => {
                // Leaving a partial temporary file behind would waste space on
                // the destination drive for no benefit.
                let _ = fs::remove_file(&temp_path);
                return Err(e);
            }
        };

        // The rename is the commit point. Everything before it is invisible to
        // a reader, because a reader only ever looks for the final name.
        if let Err(e) = fs::rename(&temp_path, &final_path) {
            let _ = fs::remove_file(&temp_path);
            return Err(Error::io(final_path.display(), e));
        }

        Ok(PutOutcome {
            hash,
            compressed_size,
            written: true,
        })
    }

    fn write_temp(&self, temp_path: &Path, data: &[u8]) -> Result<u64> {
        let file = File::create(temp_path).map_err(|e| Error::io(temp_path.display(), e))?;
        let mut writer = io::BufWriter::new(file);

        if let Some(keys) = &self.keys {
            // Compress into memory, then seal the result. Sealing first would
            // leave nothing for the compressor to work with.
            let squeezed = self.compress_to_vec(data, temp_path)?;
            let sealed = keys.seal(&squeezed)?;
            writer
                .write_all(&sealed)
                .map_err(|e| Error::io(temp_path.display(), e))?;
        } else {
            match self.compression.algorithm {
                CompressionAlgorithm::Zstd => {
                    let mut encoder =
                        zstd::stream::Encoder::new(&mut writer, self.compression.level)
                            .map_err(|e| Error::io(temp_path.display(), e))?;
                    encoder
                        .write_all(data)
                        .map_err(|e| Error::io(temp_path.display(), e))?;
                    encoder
                        .finish()
                        .map_err(|e| Error::io(temp_path.display(), e))?;
                }
                CompressionAlgorithm::None => {
                    writer
                        .write_all(data)
                        .map_err(|e| Error::io(temp_path.display(), e))?;
                }
            }
        }

        let file = writer
            .into_inner()
            .map_err(|e| Error::io(temp_path.display(), e.into_error()))?;
        // Without this the rename can be durable while the contents are not,
        // which would leave a correctly named chunk full of zeroes after a
        // power cut.
        file.sync_all()
            .map_err(|e| Error::io(temp_path.display(), e))?;
        let size = file
            .metadata()
            .map_err(|e| Error::io(temp_path.display(), e))?
            .len();
        drop(file);

        if size == 0 {
            return Err(Error::io(
                temp_path.display(),
                io::Error::new(io::ErrorKind::WriteZero, "the chunk file ended up empty"),
            ));
        }
        Ok(size)
    }

    /// Compresses into memory, for the encrypted path.
    fn compress_to_vec(&self, data: &[u8], whose: &Path) -> Result<Vec<u8>> {
        match self.compression.algorithm {
            CompressionAlgorithm::Zstd => zstd::stream::encode_all(data, self.compression.level)
                .map_err(|e| Error::io(whose.display(), e)),
            CompressionAlgorithm::None => Ok(data.to_vec()),
        }
    }

    /// Reads a chunk and checks it against `hash` and `expected_size`.
    ///
    /// The decompressor is bounded by `expected_size`, so a manifest that
    /// understates a chunk's size cannot make the reader allocate without
    /// limit. The digest check happens after, which is what catches silent
    /// corruption on the destination drive.
    pub fn get(&self, hash: ChunkHash, expected_size: u32) -> Result<Vec<u8>> {
        self.check_unlocked()?;
        let path = self.layout.chunk_path(hash);
        let file = File::open(&path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                Error::corrupt(
                    format!("chunk {hash} is missing from the backup"),
                    "the manifest lists this chunk, so part of the captured data is simply not present and the backup cannot be restored in full",
                    "use a different backup; if the drive was disconnected during the backup the set was never marked complete and should be taken again",
                )
            } else {
                Error::io(path.display(), e)
            }
        })?;

        let limit = u64::from(expected_size);
        let mut out = Vec::with_capacity(expected_size as usize);
        let mut reader = io::BufReader::new(file);

        let read_result = if let Some(keys) = &self.keys {
            // A sealed chunk has to be whole before any of it can be trusted,
            // so it is read and opened before anything is decompressed. The
            // seal is checked first: nothing that failed authentication ever
            // reaches the decompressor.
            let mut sealed = Vec::new();
            reader
                .read_to_end(&mut sealed)
                .map_err(|e| Error::io(path.display(), e))?;
            let squeezed = keys.open(&sealed)?;
            match self.compression.algorithm {
                CompressionAlgorithm::Zstd => zstd::stream::Decoder::new(squeezed.as_slice())
                    .map_err(|e| Error::io(path.display(), e))?
                    .take(limit + 1)
                    .read_to_end(&mut out),
                CompressionAlgorithm::None => {
                    squeezed.as_slice().take(limit + 1).read_to_end(&mut out)
                }
            }
        } else {
            match self.compression.algorithm {
                CompressionAlgorithm::Zstd => {
                    let decoder = zstd::stream::Decoder::new(reader)
                        .map_err(|e| Error::io(path.display(), e))?;
                    // Reading one byte past the limit is what detects a chunk
                    // that decompresses to more than the manifest claims.
                    decoder.take(limit + 1).read_to_end(&mut out)
                }
                CompressionAlgorithm::None => (&mut reader).take(limit + 1).read_to_end(&mut out),
            }
        };
        read_result.map_err(|e| {
            Error::corrupt(
                format!("chunk {hash} could not be decompressed"),
                format!("the compressed stream is damaged or truncated: {e}"),
                "run `MjolnirVSS.exe verify` against the whole backup to see how much of it is affected",
            )
        })?;

        if out.len() as u64 != limit {
            return Err(Error::corrupt(
                format!("chunk {hash} is the wrong size"),
                format!(
                    "the manifest says {expected_size} bytes but the stored chunk holds {}; the file is truncated or the manifest does not belong to this chunk store",
                    out.len()
                ),
                "run `MjolnirVSS.exe verify` against the whole backup, and do not restore from it until it passes",
            ));
        }

        let actual = self.name_of(&out);
        if actual != hash {
            return Err(Error::corrupt(
                format!("chunk {hash} failed its digest check"),
                format!("the stored bytes hash to {actual} instead, so the chunk has been altered or the drive returned bad data"),
                "run `MjolnirVSS.exe verify` against the whole backup; if more than one chunk fails, check the destination drive's health before trusting any backup on it",
            ));
        }

        Ok(out)
    }

    /// Checks one chunk without keeping its contents.
    pub fn verify(&self, hash: ChunkHash, expected_size: u32) -> Result<u64> {
        let stored = fs::metadata(self.layout.chunk_path(hash))
            .map(|m| m.len())
            .unwrap_or(0);
        self.get(hash, expected_size)?;
        Ok(stored)
    }

    /// Removes temporary files left behind by an interrupted run.
    ///
    /// Only files matching the temporary naming scheme are touched, and only
    /// inside this backup's own chunk store.
    pub fn sweep_temp_files(&self) -> Result<usize> {
        let root = self.layout.chunk_root();
        if !root.is_dir() {
            return Ok(0);
        }
        let mut removed = 0usize;
        let entries = fs::read_dir(&root).map_err(|e| Error::io(root.display(), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| Error::io(root.display(), e))?;
            let path = entry.path();
            if path.is_dir() {
                removed += sweep_dir(&path)?;
            } else if is_temp_chunk(&path) {
                fs::remove_file(&path).map_err(|e| Error::io(path.display(), e))?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

fn is_temp_chunk(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.ends_with(crate::layout::TEMP_SUFFIX))
        .unwrap_or(false)
}

fn sweep_dir(dir: &Path) -> Result<usize> {
    let mut removed = 0usize;
    let entries = fs::read_dir(dir).map_err(|e| Error::io(dir.display(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(dir.display(), e))?;
        let path = entry.path();
        if path.is_file() && is_temp_chunk(&path) {
            fs::remove_file(&path).map_err(|e| Error::io(path.display(), e))?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::CompressionAlgorithm;

    fn store_in(dir: &Path) -> ChunkStore {
        ChunkStore::new(
            BackupLayout::with_defaults(dir),
            CompressionSpec {
                algorithm: CompressionAlgorithm::Zstd,
                level: 3,
            },
        )
    }

    /// Fast key settings. What these tests check is the wiring, not Argon2.
    fn sealed_store_in(dir: &Path, password: &str) -> (ChunkStore, mjolnir_crypto::EncryptionInfo) {
        let params = mjolnir_crypto::KdfParams {
            memory_kib: 8 * 1024,
            passes: 1,
            lanes: 1,
        };
        let started = mjolnir_crypto::begin(password, params).expect("keys");
        let info = started.info.clone();
        let store = store_in(dir).with_keys(std::sync::Arc::new(started.keys));
        (store, info)
    }

    #[test]
    fn a_sealed_chunk_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _) = sealed_store_in(tmp.path(), "a password");
        assert!(store.is_encrypted());

        let data = vec![7u8; 100_000];
        let put = store.put(&data).unwrap();
        assert!(put.written);
        assert_eq!(store.get(put.hash, data.len() as u32).unwrap(), data);
    }

    /// The point of the whole thing: what is on the drive must not be the data.
    #[test]
    fn what_lands_on_the_drive_is_not_the_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _) = sealed_store_in(tmp.path(), "a password");

        // Something that would survive compression and be recognisable.
        let secret = b"the quick brown fox jumps over the lazy dog".repeat(400);
        let put = store.put(&secret).unwrap();

        let stored = std::fs::read(store.path_of(put.hash)).unwrap();
        assert!(
            !contains(&stored, b"quick brown fox"),
            "the contents must not be readable on the drive"
        );
    }

    /// And the name on the drive must not give the contents away either. Anyone
    /// holding the drive could otherwise test whether a file they already have
    /// is in the backup, just by hashing it.
    #[test]
    fn the_name_on_the_drive_does_not_reveal_the_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _) = sealed_store_in(tmp.path(), "a password");

        let data = vec![3u8; 5_000];
        let put = store.put(&data).unwrap();
        assert_ne!(
            put.hash,
            ChunkHash::of(&data),
            "an encrypted chunk must not be named by the plain digest of its contents"
        );
    }

    /// A different password must not open somebody else's chunk.
    #[test]
    fn another_password_does_not_open_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _) = sealed_store_in(tmp.path(), "the right password");
        let data = vec![9u8; 4_096];
        let put = store.put(&data).unwrap();

        let (other, _) = sealed_store_in(tmp.path(), "a different password");
        assert!(
            other.get(put.hash, data.len() as u32).is_err(),
            "the wrong password must not read the chunk"
        );
    }

    /// A flipped bit in a sealed chunk is caught by the seal, before anything
    /// is decompressed.
    #[test]
    fn a_changed_sealed_chunk_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _) = sealed_store_in(tmp.path(), "a password");
        let data = vec![5u8; 10_000];
        let put = store.put(&data).unwrap();

        let path = store.path_of(put.hash);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();

        let err = store.get(put.hash, data.len() as u32).unwrap_err();
        assert_eq!(err.exit(), mjolnir_core::exit::ExitCode::CorruptBackup);
    }

    /// An unencrypted store must be completely unaffected by any of this.
    #[test]
    fn an_unencrypted_store_is_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        assert!(!store.is_encrypted());

        let data = vec![1u8; 8_192];
        let put = store.put(&data).unwrap();
        assert_eq!(
            put.hash,
            ChunkHash::of(&data),
            "an unencrypted chunk is still named by the digest of its contents"
        );
        assert_eq!(store.get(put.hash, data.len() as u32).unwrap(), data);
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn round_trips_a_chunk() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let data = vec![7u8; 100_000];

        let put = store.put(&data).unwrap();
        assert!(put.written);
        assert!(put.compressed_size > 0);
        // Highly compressible input must actually get smaller, otherwise the
        // compression path is not wired up.
        assert!(put.compressed_size < data.len() as u64);

        let got = store.get(put.hash, data.len() as u32).unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn writing_the_same_chunk_twice_is_free() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let data = b"the same bytes".repeat(1000);

        let first = store.put(&data).unwrap();
        let second = store.put(&data).unwrap();
        assert!(first.written);
        assert!(!second.written);
        assert_eq!(first.hash, second.hash);
        assert_eq!(first.compressed_size, second.compressed_size);
    }

    #[test]
    fn an_empty_chunk_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(store_in(tmp.path()).put(b"").is_err());
    }

    #[test]
    fn a_missing_chunk_is_reported_as_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let hash = ChunkHash::of(b"never stored");
        let err = store.get(hash, 12).unwrap_err();
        assert_eq!(err.exit(), ExitCode::CorruptBackup);
        assert!(err.what().contains("missing"));
    }

    #[test]
    fn a_corrupted_chunk_fails_its_digest_check() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let data = vec![1u8; 50_000];
        let put = store.put(&data).unwrap();

        // Flip bits in the middle of the stored file. zstd may or may not
        // notice; the digest check is what must catch it either way.
        let path = store.path_of(put.hash);
        let mut bytes = fs::read(&path).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        fs::write(&path, &bytes).unwrap();

        let err = store.get(put.hash, data.len() as u32).unwrap_err();
        assert_eq!(err.exit(), ExitCode::CorruptBackup);
        assert!(
            err.what().contains("digest")
                || err.what().contains("decompress")
                || err.what().contains("wrong size"),
            "unexpected message: {}",
            err.what()
        );
    }

    #[test]
    fn a_truncated_chunk_is_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let data = vec![2u8; 80_000];
        let put = store.put(&data).unwrap();

        let path = store.path_of(put.hash);
        let bytes = fs::read(&path).unwrap();
        fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();

        let err = store.get(put.hash, data.len() as u32).unwrap_err();
        assert_eq!(err.exit(), ExitCode::CorruptBackup);
    }

    #[test]
    fn a_chunk_claiming_to_be_smaller_than_it_is_gets_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let data = vec![3u8; 60_000];
        let put = store.put(&data).unwrap();

        // Ask for fewer bytes than the chunk really holds. The bounded reader
        // must notice rather than silently returning a prefix.
        let err = store.get(put.hash, 1000).unwrap_err();
        assert_eq!(err.exit(), ExitCode::CorruptBackup);
        assert!(err.what().contains("wrong size"));
    }

    #[test]
    fn uncompressed_storage_round_trips_too() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ChunkStore::new(
            BackupLayout::with_defaults(tmp.path()),
            CompressionSpec {
                algorithm: CompressionAlgorithm::None,
                level: 0,
            },
        );
        let data = b"plain bytes".repeat(500);
        let put = store.put(&data).unwrap();
        assert_eq!(put.compressed_size, data.len() as u64);
        assert_eq!(store.get(put.hash, data.len() as u32).unwrap(), data);
    }

    #[test]
    fn no_temporary_files_survive_a_successful_write() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        store.put(&vec![9u8; 10_000]).unwrap();

        let mut leftovers = Vec::new();
        for entry in walk(&store.layout().chunk_root()) {
            if entry.to_string_lossy().ends_with(".tmp") {
                leftovers.push(entry);
            }
        }
        assert!(
            leftovers.is_empty(),
            "temporary files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn sweeping_removes_only_temporary_files() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let put = store.put(&vec![4u8; 20_000]).unwrap();

        let temp = store.layout().chunk_temp_path(put.hash, 12345);
        fs::write(&temp, b"partial").unwrap();
        assert!(temp.exists());

        assert_eq!(store.sweep_temp_files().unwrap(), 1);
        assert!(!temp.exists());
        // The real chunk is untouched.
        assert!(store.path_of(put.hash).exists());
        assert_eq!(store.get(put.hash, 20_000).unwrap().len(), 20_000);
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
        out
    }
}
