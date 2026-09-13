//! Assembling recovery media from the parts this computer already has.
//!
//! The work is done by Microsoft's own tools, `dism.exe` and `oscdimg.exe`,
//! because they are the supported way to modify a Windows PE image and the only
//! way that produces something Microsoft will stand behind booting. MjolnirVSS
//! runs them; it does not reimplement them, and it does not ship them.
//!
//! Every temporary file lives in one work folder, and that folder is removed
//! afterwards whether the build succeeded or not. An image left mounted would
//! otherwise sit in the machine's DISM state until somebody found it.

use std::path::{Path, PathBuf};
use std::process::Command;

use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::progress::Progress;

use crate::layout::{Payload, STARTNET_PATH};
use crate::source::MediaSource;

/// What a finished build produced.
#[derive(Debug, Clone)]
pub struct MediaOutcome {
    /// The file that was written.
    pub path: PathBuf,
    /// Its size in bytes.
    pub size_bytes: u64,
    /// Which source the bootable part came from.
    pub source: crate::source::SourceKind,
    /// Things worth telling the operator.
    pub notes: Vec<String>,
}

/// Runs a tool, and turns a non zero exit into an error that explains itself.
fn run(tool: &Path, args: &[String], what: &str) -> Result<String> {
    let output = Command::new(tool).args(args).output().map_err(|e| {
        Error::new(
            ExitCode::Failure,
            format!("{what} could not be started"),
            format!("running {} failed: {e}", tool.display()),
            "check that the Windows Assessment and Deployment Kit is still installed",
        )
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() {
        // The last few lines are where these tools put the reason.
        let detail: Vec<&str> = stdout
            .lines()
            .chain(stderr.lines())
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(6)
            .collect();
        let detail: Vec<&str> = detail.into_iter().rev().collect();
        return Err(Error::new(
            ExitCode::Failure,
            format!("{what} failed"),
            format!(
                "{} exited with {}: {}",
                tool.display(),
                output.status.code().unwrap_or(-1),
                detail.join(" / ")
            ),
            "run MjolnirVSS as administrator, and check there is room on the drive holding the temporary folder",
        ));
    }
    Ok(stdout)
}

/// Copies a folder tree.
fn copy_tree(from: &Path, to: &Path, cancel: &CancelToken) -> Result<u64> {
    let mut copied = 0u64;
    std::fs::create_dir_all(to).map_err(|e| io_error("creating a folder", to, e))?;

    for entry in std::fs::read_dir(from).map_err(|e| io_error("reading a folder", from, e))? {
        cancel.check()?;
        let entry = entry.map_err(|e| io_error("reading a folder", from, e))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|e| io_error("inspecting", &source, e))?;

        if kind.is_dir() {
            copied += copy_tree(&source, &target, cancel)?;
        } else {
            copied +=
                std::fs::copy(&source, &target).map_err(|e| io_error("copying", &source, e))?;
            // The ADK's files are read only where they sit, and a read only
            // boot.wim cannot be mounted for writing.
            clear_read_only(&target)?;
        }
    }
    Ok(copied)
}

fn clear_read_only(path: &Path) -> Result<()> {
    let mut perms = std::fs::metadata(path)
        .map_err(|e| io_error("inspecting", path, e))?
        .permissions();
    if perms.readonly() {
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(path, perms).map_err(|e| io_error("changing", path, e))?;
    }
    Ok(())
}

fn io_error(doing: &str, path: &Path, e: std::io::Error) -> Error {
    Error::new(
        ExitCode::Io,
        format!("{doing} {} failed", path.display()),
        e.to_string(),
        "check the drive is still connected and there is room on it",
    )
}

/// A work folder that cleans itself up, including any mounted image.
struct WorkFolder {
    root: PathBuf,
    mount: PathBuf,
    mounted: bool,
}

impl WorkFolder {
    fn create(parent: &Path) -> Result<Self> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let root = parent.join(format!("mjolnir-media-{stamp}-{}", std::process::id()));
        std::fs::create_dir_all(&root).map_err(|e| io_error("creating a folder", &root, e))?;
        let mount = root.join("mount");
        std::fs::create_dir_all(&mount).map_err(|e| io_error("creating a folder", &mount, e))?;
        Ok(Self {
            root,
            mount,
            mounted: false,
        })
    }

    fn media(&self) -> PathBuf {
        self.root.join("media")
    }
}

impl Drop for WorkFolder {
    fn drop(&mut self) {
        if self.mounted {
            // Discarding rather than committing: reaching here means the build
            // did not finish, so whatever is in the image is half done.
            let _ = Command::new("dism.exe")
                .args([
                    "/Unmount-Wim".to_owned(),
                    format!("/MountDir:{}", self.mount.display()),
                    "/Discard".to_owned(),
                ])
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Builds a bootable ISO.
///
/// Needs administrator rights, because mounting a Windows image does.
pub fn build_iso(
    source: &MediaSource,
    payload: &Payload,
    iso_path: &Path,
    work_parent: &Path,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
) -> Result<MediaOutcome> {
    if !source.can_make_iso() {
        return Err(Error::new(
            ExitCode::Unsupported,
            "this computer cannot build a recovery ISO",
            source.explain_limits().join(" "),
            "install the Windows Assessment and Deployment Kit with the Windows PE add-on, or make a USB stick instead",
        ));
    }
    crate::layout::check_iso_path(iso_path)?;

    let oscdimg = source.oscdimg.as_ref().expect("checked above").clone();
    let etfsboot = source.etfsboot.as_ref().expect("checked above").clone();
    let efisys = source.efisys.as_ref().expect("checked above").clone();
    let template = source
        .media_template
        .as_ref()
        .expect("checked above")
        .clone();

    let mut work = WorkFolder::create(work_parent)?;
    let media = work.media();

    // ---- 1. the media layout ------------------------------------------
    progress.begin("Copying the Windows boot files", None);
    copy_tree(&template, &media, cancel)?;
    progress.end();
    cancel.check()?;

    // ---- 2. the boot image --------------------------------------------
    progress.begin("Copying the recovery image", None);
    let sources_dir = media.join("sources");
    std::fs::create_dir_all(&sources_dir)
        .map_err(|e| io_error("creating a folder", &sources_dir, e))?;
    let boot_wim = sources_dir.join("boot.wim");
    std::fs::copy(&source.boot_image, &boot_wim)
        .map_err(|e| io_error("copying", &source.boot_image, e))?;
    clear_read_only(&boot_wim)?;
    progress.end();
    cancel.check()?;

    // ---- 3. put MjolnirVSS inside it ----------------------------------
    progress.begin("Adding MjolnirVSS to the recovery image", None);
    let dism = PathBuf::from("dism.exe");
    run(
        &dism,
        &[
            "/Mount-Wim".to_owned(),
            format!("/WimFile:{}", boot_wim.display()),
            "/Index:1".to_owned(),
            format!("/MountDir:{}", work.mount.display()),
        ],
        "opening the recovery image",
    )?;
    work.mounted = true;

    for file in &payload.files {
        cancel.check()?;
        let target = work.mount.join(&file.to);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| io_error("creating a folder", parent, e))?;
        }
        std::fs::copy(&file.from, &target).map_err(|e| io_error("copying", &file.from, e))?;
    }

    let startnet = work.mount.join(STARTNET_PATH);
    if let Some(parent) = startnet.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io_error("creating a folder", parent, e))?;
    }
    std::fs::write(&startnet, payload.startnet.as_bytes())
        .map_err(|e| io_error("writing", &startnet, e))?;

    run(
        &dism,
        &[
            "/Unmount-Wim".to_owned(),
            format!("/MountDir:{}", work.mount.display()),
            "/Commit".to_owned(),
        ],
        "saving the recovery image",
    )?;
    work.mounted = false;
    progress.end();
    cancel.check()?;

    // ---- 4. make it into a disc ---------------------------------------
    progress.begin("Building the recovery disc", None);
    if iso_path.exists() {
        std::fs::remove_file(iso_path).map_err(|e| io_error("replacing", iso_path, e))?;
    }
    // The boot data string is the one Microsoft documents for media that boots
    // both the old way and through UEFI: entry 1 is the El Torito boot sector,
    // entry 2 is the EFI boot image.
    let bootdata = format!("2#p0,e,b{}#pEF,e,b{}", etfsboot.display(), efisys.display());
    run(
        &oscdimg,
        &[
            "-m".to_owned(),
            "-o".to_owned(),
            "-u2".to_owned(),
            "-udfver102".to_owned(),
            format!("-bootdata:{bootdata}"),
            media.display().to_string(),
            iso_path.display().to_string(),
        ],
        "building the recovery disc",
    )?;
    progress.end();

    let size_bytes = std::fs::metadata(iso_path)
        .map_err(|e| io_error("inspecting", iso_path, e))?
        .len();

    let mut notes = vec![format!(
        "The bootable part came from {}.",
        source.kind.describe()
    )];
    notes.push(
        "The Windows files on this disc belong to Microsoft and are licensed to this computer. Do not pass the disc on."
            .to_owned(),
    );

    Ok(MediaOutcome {
        path: iso_path.to_path_buf(),
        size_bytes,
        source: source.kind,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mjolnir_core::progress::SilentProgress;

    #[test]
    fn a_source_that_cannot_make_an_iso_says_so_before_doing_anything() {
        let dir = tempfile::tempdir().unwrap();
        let source = MediaSource {
            kind: crate::source::SourceKind::LocalRecovery,
            boot_image: dir.path().join("winre.wim"),
            media_template: None,
            oscdimg: None,
            etfsboot: None,
            efisys: None,
        };
        let payload = Payload {
            files: Vec::new(),
            startnet: String::new(),
        };
        let err = build_iso(
            &source,
            &payload,
            &dir.path().join("out.iso"),
            dir.path(),
            &mut SilentProgress,
            &CancelToken::new(),
        )
        .unwrap_err();

        assert!(err.what().contains("cannot build a recovery ISO"));
        assert!(err.next_step().contains("Deployment Kit"));
        // And nothing was created.
        assert!(!dir.path().join("out.iso").exists());
    }

    #[test]
    fn copying_a_tree_reproduces_it_and_clears_read_only() {
        let from = tempfile::tempdir().unwrap();
        let to = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(from.path().join("a/b")).unwrap();
        std::fs::write(from.path().join("a/b/c.txt"), b"hello").unwrap();
        std::fs::write(from.path().join("top.txt"), b"hi").unwrap();

        let readonly = from.path().join("top.txt");
        let mut perms = std::fs::metadata(&readonly).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&readonly, perms).unwrap();

        let target = to.path().join("copy");
        let bytes = copy_tree(from.path(), &target, &CancelToken::new()).unwrap();

        assert_eq!(bytes, 7);
        assert_eq!(
            std::fs::read(target.join("a/b/c.txt")).unwrap(),
            b"hello".to_vec()
        );
        assert!(
            !std::fs::metadata(target.join("top.txt"))
                .unwrap()
                .permissions()
                .readonly(),
            "a read only copy cannot be modified later"
        );
    }

    #[test]
    fn a_cancelled_copy_stops() {
        let from = tempfile::tempdir().unwrap();
        let to = tempfile::tempdir().unwrap();
        for i in 0..20 {
            std::fs::write(from.path().join(format!("{i}.txt")), b"x").unwrap();
        }
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = copy_tree(from.path(), &to.path().join("c"), &cancel).unwrap_err();
        assert_eq!(err.exit(), ExitCode::Cancelled);
    }

    /// The work folder has to remove itself, or every failed attempt leaves a
    /// few hundred megabytes behind.
    #[test]
    fn the_work_folder_removes_itself() {
        let parent = tempfile::tempdir().unwrap();
        let path;
        {
            let work = WorkFolder::create(parent.path()).unwrap();
            path = work.root.clone();
            assert!(path.is_dir());
            assert!(work.mount.is_dir());
        }
        assert!(!path.exists(), "the work folder was left behind");
    }
}
