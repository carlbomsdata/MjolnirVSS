//! Opening a backup set: reading the documents, checking them against each
//! other, and refusing anything that is not whole.
//!
//! The checks that live here are the ones no single document can make on its
//! own. The most important is that every partition recorded in
//! `disk-layout.json` is carried by a stream in `manifest.json`. A backup that
//! quietly dropped the recovery partition would validate perfectly against each
//! document separately and then produce a computer that cannot repair itself.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use mjolnir_core::error::{Error, Result};
use mjolnir_core::ids::{DiskId, PartitionId, StreamId};
use mjolnir_core::math;

use crate::completion::{Completion, DocumentDigest};
use crate::disk_layout::DiskLayout;
use crate::hash::ChunkHash;
use crate::issue::{Issue, IssueList};
use crate::layout::BackupLayout;
use crate::manifest::{Manifest, StreamKind};
use crate::store::ChunkStore;

/// A backup set that has been read off a drive.
#[derive(Debug, Clone)]
pub struct BackupSet {
    layout: BackupLayout,
    manifest: Manifest,
    disk_layout: DiskLayout,
    completion: Option<Completion>,
    issues: Vec<Issue>,
    /// Present once a password has been given for an encrypted backup.
    keys: Option<std::sync::Arc<mjolnir_crypto::Keys>>,
}

impl BackupSet {
    /// Opens a backup folder and refuses it unless it is complete and sound.
    ///
    /// This is what every destructive path calls. Browsing and diagnostics use
    /// [`BackupSet::open_unchecked`] so an operator can still look at a damaged
    /// backup and be told exactly what is wrong with it.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let set = Self::open_unchecked(dir)?;

        let Some(completion) = &set.completion else {
            return Err(Error::corrupt(
                format!(
                    "the backup at {} is incomplete",
                    set.layout.dir().display()
                ),
                "it has no completion.json, which MjolnirVSS writes only after every chunk is on the drive and verification has passed; the backup run was interrupted, cancelled, or the drive was disconnected",
                "take a new backup; an incomplete folder can be deleted, nothing else refers to it",
            ));
        };
        if !completion.is_restorable() {
            return Err(Error::corrupt(
                format!(
                    "the backup at {} did not complete successfully",
                    set.layout.dir().display()
                ),
                format!(
                    "completion.json records state {:?} and verification {:?}",
                    completion.state, completion.verification.result
                ),
                "take a new backup, and check the destination drive if this happens again",
            ));
        }

        set.issues
            .clone()
            .into_result(&format!("the backup at {}", set.layout.dir().display()))?;
        Ok(set)
    }

    /// Opens a backup folder without refusing it, collecting findings instead.
    ///
    /// Fails only when a required document cannot be read or parsed at all,
    /// because then there is nothing to report findings about.
    pub fn open_unchecked(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();

        let manifest: Manifest = read_json(&dir.join(crate::layout::MANIFEST_FILE))?;
        let layout = BackupLayout::new(
            dir.clone(),
            manifest.chunk_store.root.clone(),
            manifest.chunk_store.fanout,
        );
        let disk_layout: DiskLayout = read_json(&layout.disk_layout_path())?;

        let completion_path = layout.completion_path();
        let completion: Option<Completion> = if completion_path.exists() {
            Some(read_json(&completion_path)?)
        } else {
            None
        };

        let mut issues = manifest.validate();
        issues.extend(disk_layout.validate());
        if let Some(c) = &completion {
            issues.extend(c.validate());
        }
        issues.extend(cross_check(&manifest, &disk_layout, completion.as_ref()));
        if let Some(c) = &completion {
            issues.extend(check_document_digests(&layout, c));
        }

        Ok(Self {
            layout,
            manifest,
            disk_layout,
            completion,
            issues,
            keys: None,
        })
    }

    /// Paths inside this backup.
    pub fn layout(&self) -> &BackupLayout {
        &self.layout
    }

    /// The manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The disk layout.
    pub fn disk_layout(&self) -> &DiskLayout {
        &self.disk_layout
    }

    /// The completion document, absent when the backup is incomplete.
    pub fn completion(&self) -> Option<&Completion> {
        self.completion.as_ref()
    }

    /// Findings collected while opening.
    pub fn issues(&self) -> &[Issue] {
        &self.issues
    }

    /// Whether the backup is complete and its documents agree with each other.
    pub fn is_restorable(&self) -> bool {
        self.completion
            .as_ref()
            .is_some_and(Completion::is_restorable)
            && !self.issues.has_errors()
    }

    /// A chunk store reading from this backup.
    pub fn chunk_store(&self) -> ChunkStore {
        let store = ChunkStore::new(self.layout.clone(), self.manifest.compression);
        match &self.keys {
            Some(keys) => store.with_keys(keys.clone()),
            None if self.is_encrypted() => store.locked(),
            None => store,
        }
    }

    /// Whether the contents of this backup are sealed.
    ///
    /// Answerable without a password: it is read from the manifest, which stays
    /// readable on purpose so a backup can be identified before anybody is
    /// asked for anything.
    pub fn is_encrypted(&self) -> bool {
        self.manifest.encryption.is_some()
    }

    /// Whether a password has been supplied for this backup already.
    pub fn is_unlocked(&self) -> bool {
        self.keys.is_some() || !self.is_encrypted()
    }

    /// Supplies the password, so the contents can be read.
    ///
    /// A wrong password is reported here, at once, rather than as a failure to
    /// read some block in the middle of a restore.
    pub fn unlock(&mut self, password: &str) -> Result<()> {
        let Some(info) = &self.manifest.encryption else {
            return Err(Error::new(
                mjolnir_core::ExitCode::Failure,
                "this backup is not encrypted",
                "a password was given for a backup that does not have one".to_owned(),
                "open it without a password",
            ));
        };
        let keys = mjolnir_crypto::unlock(info, password)?;
        self.keys = Some(std::sync::Arc::new(keys));
        Ok(())
    }
}

/// Reads and parses one JSON document, turning every failure into an operator
/// facing error rather than a serde message.
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::corrupt(
                format!("{} is missing", path.display()),
                "a MjolnirVSS backup folder must contain manifest.json and disk-layout.json; without them there is no way to know what the chunks mean",
                "check that the whole backup folder was copied, and that you selected the backup folder itself rather than the drive",
            )
        } else {
            Error::io(path.display(), e)
        }
    })?;
    serde_json::from_str(&text).map_err(|e| {
        Error::corrupt(
            format!("{} could not be read", path.display()),
            format!("the file is not valid MjolnirVSS JSON: {e}"),
            "the file has been damaged or edited; use a different backup, and do not restore from this one",
        )
    })
}

/// Writes a JSON document atomically and returns its digest and size.
///
/// The bytes go to a temporary file in the same directory, are flushed to
/// stable storage, and are then renamed into place. A reader never sees a
/// half written document.
pub fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<DocumentDigest> {
    let text = serde_json::to_vec_pretty(value).map_err(|e| {
        Error::new(
            mjolnir_core::ExitCode::Failure,
            format!("{} could not be encoded", path.display()),
            format!("serialising the document failed: {e}"),
            "this is an internal error; please report it with the command you ran",
        )
    })?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io(parent.display(), e))?;
    }

    let temp = temp_sibling(path);
    {
        let mut file = fs::File::create(&temp).map_err(|e| Error::io(temp.display(), e))?;
        file.write_all(&text)
            .map_err(|e| Error::io(temp.display(), e))?;
        // Without this the rename can be durable while the contents are not,
        // leaving a correctly named but empty document after a power cut.
        file.sync_all().map_err(|e| Error::io(temp.display(), e))?;
    }
    if let Err(e) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(Error::io(path.display(), e));
    }

    Ok(DocumentDigest {
        path: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        blake3: ChunkHash::of(&text),
        bytes: text.len() as u64,
    })
}

fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document".to_owned());
    name.push_str(&format!(
        ".{:08x}{}",
        std::process::id(),
        crate::layout::TEMP_SUFFIX
    ));
    path.with_file_name(name)
}

/// Checks the recorded metadata digests against the files on the drive.
fn check_document_digests(layout: &BackupLayout, completion: &Completion) -> Vec<Issue> {
    let mut issues = Vec::new();
    for doc in &completion.documents {
        let Some(path) = layout.resolve_relative(&doc.path) else {
            // Already reported by Completion::validate; skip rather than
            // double report.
            continue;
        };
        match fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => issues.push(Issue::error(
                format!("document {:?}", doc.path),
                "is listed in completion.json but is not in the backup folder",
            )),
            Err(e) => issues.push(Issue::error(
                format!("document {:?}", doc.path),
                format!("could not be read: {e}"),
            )),
            Ok(bytes) => {
                if bytes.len() as u64 != doc.bytes {
                    issues.push(Issue::error(
                        format!("document {:?}", doc.path),
                        format!(
                            "is {} bytes but completion.json records {}; the file has been changed since the backup was taken",
                            bytes.len(),
                            doc.bytes
                        ),
                    ));
                }
                let actual = ChunkHash::of(&bytes);
                if actual != doc.blake3 {
                    issues.push(Issue::error(
                        format!("document {:?}", doc.path),
                        format!(
                            "hashes to {actual} but completion.json records {}; the file has been damaged or edited",
                            doc.blake3
                        ),
                    ));
                }
            }
        }
    }
    issues
}

/// Checks the documents against each other.
fn cross_check(
    manifest: &Manifest,
    disk_layout: &DiskLayout,
    completion: Option<&Completion>,
) -> Vec<Issue> {
    let mut issues = Vec::new();

    if disk_layout.backup_uuid != manifest.backup.uuid {
        issues.push(Issue::error(
            "disk-layout.json",
            format!(
                "belongs to backup {} but manifest.json belongs to {}; these documents are from different backups",
                disk_layout.backup_uuid, manifest.backup.uuid
            ),
        ));
    }
    if let Some(c) = completion {
        if c.backup_uuid != manifest.backup.uuid {
            issues.push(Issue::error(
                "completion.json",
                format!(
                    "belongs to backup {} but manifest.json belongs to {}",
                    c.backup_uuid, manifest.backup.uuid
                ),
            ));
        }
        if c.required_restore_bytes != manifest.required_restore_bytes {
            issues.push(Issue::error(
                "completion.json",
                format!(
                    "records a required restore size of {} but manifest.json records {}",
                    c.required_restore_bytes, manifest.required_restore_bytes
                ),
            ));
        }
    }

    let disk_ids: BTreeSet<&DiskId> = disk_layout.disks.iter().map(|d| &d.id).collect();
    let mut referenced_streams: BTreeSet<&StreamId> = BTreeSet::new();
    let mut partition_streams: BTreeMap<&PartitionId, Vec<&StreamId>> = BTreeMap::new();

    for stream in &manifest.streams {
        let object = format!("stream {:?}", stream.id.as_str());
        if !disk_ids.contains(&stream.disk_id) {
            issues.push(Issue::error(
                &object,
                format!(
                    "belongs to disk {:?}, which is not in disk-layout.json",
                    stream.disk_id.as_str()
                ),
            ));
            continue;
        }
        if let Some(pid) = &stream.partition_id {
            partition_streams.entry(pid).or_default().push(&stream.id);
        }
        let disk = disk_layout
            .disk(&stream.disk_id)
            .expect("membership checked above");

        if let Err(e) = math::ensure_within(
            "stream placement",
            stream.target_offset,
            stream.length,
            disk.size_bytes,
        ) {
            issues.push(Issue::error(
                &object,
                format!("would be restored outside its disk: {e}"),
            ));
        }
    }

    for disk in &disk_layout.disks {
        let object = format!("disk {:?}", disk.id.as_str());

        let heads: Vec<&crate::manifest::Stream> = manifest
            .streams
            .iter()
            .filter(|s| s.disk_id == disk.id && s.kind == StreamKind::DiskHead)
            .collect();
        let tails: Vec<&crate::manifest::Stream> = manifest
            .streams
            .iter()
            .filter(|s| s.disk_id == disk.id && s.kind == StreamKind::DiskTail)
            .collect();

        if heads.len() != 1 {
            issues.push(Issue::error(
                &object,
                format!(
                    "has {} disk head streams, expected exactly one holding the protective MBR and primary GPT",
                    heads.len()
                ),
            ));
        }
        if tails.len() != 1 {
            issues.push(Issue::error(
                &object,
                format!(
                    "has {} disk tail streams, expected exactly one holding the secondary GPT",
                    tails.len()
                ),
            ));
        }
        for head in &heads {
            referenced_streams.insert(&head.id);
            if head.target_offset != 0 {
                issues.push(Issue::error(
                    format!("stream {:?}", head.id.as_str()),
                    format!(
                        "is a disk head stream but starts at offset {} instead of 0",
                        head.target_offset
                    ),
                ));
            }
        }
        for tail in &tails {
            referenced_streams.insert(&tail.id);
            match math::range_end("disk tail end", tail.target_offset, tail.length) {
                Ok(end) if end != disk.size_bytes => issues.push(Issue::error(
                    format!("stream {:?}", tail.id.as_str()),
                    format!(
                        "is a disk tail stream but ends at {end} instead of the end of the disk at {}",
                        disk.size_bytes
                    ),
                )),
                Ok(_) => {}
                Err(e) => issues.push(Issue::error(
                    format!("stream {:?}", tail.id.as_str()),
                    format!("{e}"),
                )),
            }
        }

        // The rule that matters most: nothing is silently left out.
        for part in &disk.partitions {
            let p_object = format!("{object} partition {:?}", part.id.as_str());
            let carriers = partition_streams.get(&part.id);
            match carriers.map(Vec::as_slice) {
                None | Some([]) => {
                    issues.push(Issue::error(
                        &p_object,
                        format!(
                            "is recorded on the disk but no stream carries its contents; MjolnirVSS never omits a partition, so this backup is incomplete ({}, {} bytes)",
                            part.role.describe(),
                            part.length
                        ),
                    ));
                }
                Some([one]) => {
                    referenced_streams.insert(one);
                    let stream = manifest
                        .stream(one)
                        .expect("stream id came from the manifest");
                    if stream.kind != StreamKind::Partition {
                        issues.push(Issue::error(
                            &p_object,
                            "is carried by a stream that is not a partition stream",
                        ));
                    }
                    if stream.disk_id != disk.id {
                        issues.push(Issue::error(
                            &p_object,
                            format!(
                                "is carried by a stream belonging to disk {:?}",
                                stream.disk_id.as_str()
                            ),
                        ));
                    }
                    if stream.length != part.length {
                        issues.push(Issue::error(
                            &p_object,
                            format!(
                                "is {} bytes but its stream is {} bytes",
                                part.length, stream.length
                            ),
                        ));
                    }
                    if stream.target_offset != part.starting_offset {
                        issues.push(Issue::error(
                            &p_object,
                            format!(
                                "starts at {} but its stream would be restored to {}",
                                part.starting_offset, stream.target_offset
                            ),
                        ));
                    }
                }
                Some(many) => {
                    let names: Vec<&str> = many.iter().map(|s| s.as_str()).collect();
                    issues.push(Issue::error(
                        &p_object,
                        format!(
                            "is carried by {} streams ({}); exactly one must carry it or a restore would write the same range twice",
                            many.len(),
                            names.join(", ")
                        ),
                    ));
                    for s in many {
                        referenced_streams.insert(s);
                    }
                }
            }
        }

        match disk.required_target_bytes() {
            Ok(required) if required > manifest.required_restore_bytes => {
                issues.push(Issue::error(
                    &object,
                    format!(
                        "needs a target of at least {required} bytes but the manifest records {}; a restore could be offered a disk that is too small",
                        manifest.required_restore_bytes
                    ),
                ));
            }
            Ok(_) => {}
            Err(e) => issues.push(Issue::error(&object, format!("{e}"))),
        }
    }

    // A stream nothing restores means the two documents disagree about what was
    // captured, which is worth refusing even though it is not itself dangerous.
    for stream in &manifest.streams {
        if !referenced_streams.contains(&stream.id) {
            issues.push(Issue::error(
                format!("stream {:?}", stream.id.as_str()),
                "is not referenced by any partition, disk head or disk tail; manifest.json and disk-layout.json disagree about what was captured",
            ));
        }
    }

    for volume in &manifest.volumes {
        if disk_layout.partition(&volume.partition_id).is_none() {
            issues.push(Issue::error(
                format!("volume {:?}", volume.id.as_str()),
                format!(
                    "sits on partition {:?}, which is not in disk-layout.json",
                    volume.partition_id.as_str()
                ),
            ));
        }
    }

    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_json_write_leaves_no_temporary_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("doc.json");
        let digest = write_json_atomic(&path, &serde_json::json!({"a": 1})).unwrap();

        assert!(path.exists());
        assert_eq!(digest.path, "doc.json");
        assert_eq!(digest.bytes, fs::metadata(&path).unwrap().len());
        assert_eq!(digest.blake3, ChunkHash::of(&fs::read(&path).unwrap()));

        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn atomic_json_write_replaces_an_existing_document() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("doc.json");
        write_json_atomic(&path, &serde_json::json!({"v": 1})).unwrap();
        write_json_atomic(&path, &serde_json::json!({"v": 2})).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"v\": 2"), "{text}");
    }

    #[test]
    fn opening_a_folder_without_a_manifest_says_so_plainly() {
        let tmp = tempfile::tempdir().unwrap();
        let err = BackupSet::open(tmp.path()).unwrap_err();
        assert_eq!(err.exit(), mjolnir_core::ExitCode::CorruptBackup);
        assert!(err.what().contains("manifest.json"));
        assert!(err.next_step().contains("backup folder"));
    }

    #[test]
    fn opening_a_folder_with_damaged_json_says_so_plainly() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("manifest.json"), b"{ not json").unwrap();
        let err = BackupSet::open(tmp.path()).unwrap_err();
        assert_eq!(err.exit(), mjolnir_core::ExitCode::CorruptBackup);
        assert!(err.what().contains("could not be read"));
    }
}
