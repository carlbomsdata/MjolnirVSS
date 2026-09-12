//! Progress reporting.
//!
//! Backups run for tens of minutes against an external USB disk, so the
//! operator needs to see that something is happening and roughly how far along
//! it is. Progress goes to stderr so that stdout stays clean for machine
//! readable output.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

/// A sink for progress updates.
///
/// Implementations must tolerate being called very often; throttling is their
/// responsibility, not the caller's.
pub trait Progress: Send {
    /// Starts a named phase with an optional total in bytes.
    fn begin(&mut self, phase: &str, total_bytes: Option<u64>);
    /// Reports that `bytes` more have been processed in the current phase.
    fn advance(&mut self, bytes: u64);
    /// Ends the current phase.
    fn end(&mut self);
    /// Prints a line that is not part of the progress display.
    fn note(&mut self, message: &str);
}

/// Discards everything. Used by tests and by machine readable commands.
#[derive(Debug, Default)]
pub struct SilentProgress;

impl Progress for SilentProgress {
    fn begin(&mut self, _phase: &str, _total_bytes: Option<u64>) {}
    fn advance(&mut self, _bytes: u64) {}
    fn end(&mut self) {}
    fn note(&mut self, _message: &str) {}
}

/// Writes human readable progress to stderr.
#[derive(Debug)]
pub struct StderrProgress {
    phase: String,
    total: Option<u64>,
    done: u64,
    started: Instant,
    last_paint: Instant,
    interactive: bool,
    painted: bool,
}

impl Default for StderrProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl StderrProgress {
    /// Creates a reporter, detecting whether stderr is a terminal.
    ///
    /// When it is not, updates are emitted as plain lines at a slow cadence so
    /// a redirected log does not fill up with carriage returns.
    pub fn new() -> Self {
        Self {
            phase: String::new(),
            total: None,
            done: 0,
            started: Instant::now(),
            last_paint: Instant::now() - Duration::from_secs(60),
            interactive: std::io::stderr().is_terminal(),
            painted: false,
        }
    }

    fn interval(&self) -> Duration {
        if self.interactive {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(10)
        }
    }

    fn paint(&mut self, force: bool) {
        if !force && self.last_paint.elapsed() < self.interval() {
            return;
        }
        self.last_paint = Instant::now();

        let elapsed = self.started.elapsed().as_secs_f64();
        let rate = if elapsed > 0.0 {
            self.done as f64 / elapsed
        } else {
            0.0
        };

        let body = match self.total {
            Some(total) if total > 0 => {
                let pct = (self.done as f64 / total as f64 * 100.0).min(100.0);
                format!(
                    "{}: {:.1}% ({} of {}) at {}/s",
                    self.phase,
                    pct,
                    format_bytes(self.done),
                    format_bytes(total),
                    format_bytes(rate as u64)
                )
            }
            _ => format!(
                "{}: {} at {}/s",
                self.phase,
                format_bytes(self.done),
                format_bytes(rate as u64)
            ),
        };

        let mut err = std::io::stderr().lock();
        if self.interactive {
            // Pad to overwrite a previously longer line.
            let _ = write!(err, "\r{body:<78}");
        } else {
            let _ = writeln!(err, "{body}");
        }
        let _ = err.flush();
        self.painted = true;
    }
}

impl Progress for StderrProgress {
    fn begin(&mut self, phase: &str, total_bytes: Option<u64>) {
        self.end();
        self.phase = phase.to_owned();
        self.total = total_bytes;
        self.done = 0;
        self.started = Instant::now();
        self.last_paint = Instant::now() - Duration::from_secs(60);
        self.paint(true);
    }

    fn advance(&mut self, bytes: u64) {
        self.done = self.done.saturating_add(bytes);
        self.paint(false);
    }

    fn end(&mut self) {
        if self.phase.is_empty() {
            return;
        }
        self.paint(true);
        if self.interactive && self.painted {
            let _ = writeln!(std::io::stderr());
        }
        self.phase.clear();
        self.painted = false;
    }

    fn note(&mut self, message: &str) {
        let mut err = std::io::stderr().lock();
        if self.interactive && self.painted {
            let _ = write!(err, "\r{:<78}\r", "");
        }
        let _ = writeln!(err, "{message}");
        let _ = err.flush();
        self.painted = false;
    }
}

/// Formats a byte count for humans, using binary multiples.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{:.1} {}", value, UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_binary_multiples() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
        // Must not panic or produce a bogus unit at the top of the range.
        assert!(format_bytes(u64::MAX).ends_with("PiB"));
    }

    #[test]
    fn silent_progress_accepts_everything() {
        let mut p = SilentProgress;
        p.begin("copy", Some(10));
        p.advance(5);
        p.note("hello");
        p.end();
    }

    #[test]
    fn advance_saturates_instead_of_wrapping() {
        let mut p = StderrProgress::new();
        p.begin("copy", None);
        p.advance(u64::MAX);
        p.advance(u64::MAX);
        assert_eq!(p.done, u64::MAX);
    }
}
