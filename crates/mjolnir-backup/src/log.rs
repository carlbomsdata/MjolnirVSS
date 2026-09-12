//! The log a backup leaves next to the data it wrote.
//!
//! Enough to diagnose a failure afterwards, and nothing that would be
//! embarrassing to hand to somebody else. No file names from the machine being
//! backed up, no credentials, no recovery keys: a MjolnirVSS log describes
//! disks, partitions and its own decisions, never the contents of anything.

use std::fmt::Write as _;
use std::path::Path;

use mjolnir_core::error::{Error, Result};
use mjolnir_core::timestamp::UtcTimestamp;

/// Collects log lines during a run and writes them out at the end.
///
/// Buffered rather than streamed because the destination drive is the thing
/// most likely to fail mid run, and a log that cannot be written should not be
/// what stops a backup.
#[derive(Debug, Default)]
pub struct RunLog {
    text: String,
}

impl RunLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one timestamped line.
    pub fn line(&mut self, message: impl AsRef<str>) {
        let _ = writeln!(
            self.text,
            "{}  {}",
            UtcTimestamp::now().to_log_stamp(),
            message.as_ref()
        );
    }

    /// The log so far.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// How many lines have been recorded.
    pub fn lines(&self) -> usize {
        self.text.lines().count()
    }

    /// Writes the log to `path`, creating the folder if needed.
    pub fn write_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent.display(), e))?;
        }
        std::fs::write(path, self.text.as_bytes()).map_err(|e| Error::io(path.display(), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_timestamped() {
        let mut log = RunLog::new();
        log.line("starting");
        assert!(log.text().contains("starting"));
        assert!(log.text().starts_with("20"), "{}", log.text());
        assert_eq!(log.lines(), 1);
    }

    #[test]
    fn the_log_is_written_where_it_is_asked_for() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("logs").join("backup.log");
        let mut log = RunLog::new();
        log.line("one");
        log.line("two");
        log.write_to(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("one"));
        assert!(text.contains("two"));
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn an_empty_log_still_writes_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("backup.log");
        RunLog::new().write_to(&path).unwrap();
        assert!(path.exists());
    }
}
