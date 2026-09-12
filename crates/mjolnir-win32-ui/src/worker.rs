//! Running the engine on a background thread while the window stays alive.
//!
//! A backup takes tens of minutes. Doing it on the thread that pumps messages
//! would give the operator a frozen, grey rectangle that Windows offers to
//! close for them, so the work runs elsewhere and communicates through a small
//! shared record.
//!
//! The record is a mutex rather than a channel because the window does not want
//! every update, only the latest one: repainting a progress bar with a value
//! that is already stale is wasted work.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use mjolnir_core::cancel::CancelToken;
use mjolnir_core::error::Error;
use mjolnir_core::progress::Progress;

/// What the worker is doing, as the window understands it.
#[derive(Debug, Clone)]
pub struct ProgressSnapshot {
    /// The current stage, in plain language.
    pub stage: String,
    /// Bytes processed in this stage.
    pub done: u64,
    /// Total bytes in this stage, when it is known.
    pub total: Option<u64>,
    /// Lines worth showing behind "Show details".
    pub notes: Vec<String>,
    /// When the work started.
    pub started: Instant,
    /// When this stage started.
    pub stage_started: Instant,
}

impl ProgressSnapshot {
    /// How far through the current stage, as a fraction.
    pub fn fraction(&self) -> Option<f64> {
        match self.total {
            Some(total) if total > 0 => Some((self.done as f64 / total as f64).min(1.0)),
            _ => None,
        }
    }

    /// Bytes per second across the current stage.
    pub fn rate(&self) -> f64 {
        let elapsed = self.stage_started.elapsed().as_secs_f64();
        if elapsed <= 0.0 {
            return 0.0;
        }
        self.done as f64 / elapsed
    }

    /// Seconds remaining, when there is enough information to guess.
    ///
    /// Deliberately absent rather than wrong: an estimate from the first second
    /// of a copy is noise, and showing it makes the whole display look
    /// untrustworthy.
    pub fn seconds_remaining(&self) -> Option<f64> {
        let total = self.total?;
        if self.done == 0 || self.stage_started.elapsed().as_secs_f64() < 2.0 {
            return None;
        }
        let rate = self.rate();
        if rate <= 0.0 {
            return None;
        }
        Some((total.saturating_sub(self.done)) as f64 / rate)
    }

    /// The elapsed time across the whole run.
    pub fn elapsed_seconds(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
}

impl Default for ProgressSnapshot {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            stage: String::new(),
            done: 0,
            total: None,
            notes: Vec::new(),
            started: now,
            stage_started: now,
        }
    }
}

/// The record the worker writes and the window reads.
#[derive(Debug, Default)]
pub struct SharedProgress {
    snapshot: Mutex<ProgressSnapshot>,
    changed: AtomicBool,
}

impl SharedProgress {
    /// A new, empty record.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Reads the latest state.
    pub fn read(&self) -> ProgressSnapshot {
        self.snapshot.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Whether anything has changed since the last time this was asked.
    pub fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::Relaxed)
    }

    fn update(&self, f: impl FnOnce(&mut ProgressSnapshot)) {
        if let Ok(mut snapshot) = self.snapshot.lock() {
            f(&mut snapshot);
        }
        self.changed.store(true, Ordering::Relaxed);
    }
}

/// A [`Progress`] sink that writes into a [`SharedProgress`].
pub struct SharedProgressSink {
    shared: Arc<SharedProgress>,
}

impl SharedProgressSink {
    /// Wraps a shared record.
    pub fn new(shared: Arc<SharedProgress>) -> Self {
        Self { shared }
    }
}

impl Progress for SharedProgressSink {
    fn begin(&mut self, phase: &str, total_bytes: Option<u64>) {
        self.shared.update(|s| {
            s.stage = phase.to_owned();
            s.done = 0;
            s.total = total_bytes;
            s.stage_started = Instant::now();
        });
    }

    fn advance(&mut self, bytes: u64) {
        self.shared.update(|s| {
            s.done = s.done.saturating_add(bytes);
        });
    }

    fn end(&mut self) {}

    fn note(&mut self, message: &str) {
        self.shared.update(|s| {
            s.notes.push(message.to_owned());
            // The details pane is a diagnostic aid, not a log file; the log on
            // the destination drive is the complete record.
            if s.notes.len() > 500 {
                s.notes.remove(0);
            }
        });
    }
}

/// A job running on a background thread.
pub struct Worker<T> {
    handle: Option<std::thread::JoinHandle<Result<T, Error>>>,
    cancel: CancelToken,
    shared: Arc<SharedProgress>,
}

impl<T: Send + 'static> Worker<T> {
    /// Starts `job` on a background thread.
    ///
    /// The job is handed a progress sink and a cancellation token, exactly the
    /// same pair the command line passes, so the engine cannot tell the
    /// difference between being driven by a window and by a script.
    pub fn start<F>(job: F) -> Self
    where
        F: FnOnce(&mut dyn Progress, &CancelToken) -> Result<T, Error> + Send + 'static,
    {
        let shared = SharedProgress::new();
        let cancel = CancelToken::new();

        let thread_shared = Arc::clone(&shared);
        let thread_cancel = cancel.clone();
        let handle = std::thread::Builder::new()
            .name("mjolnir-worker".to_owned())
            .spawn(move || {
                let mut sink = SharedProgressSink::new(thread_shared);
                job(&mut sink, &thread_cancel)
            })
            .expect("a worker thread should always start");

        Self {
            handle: Some(handle),
            cancel,
            shared,
        }
    }

    /// The record the window reads to paint progress.
    pub fn progress(&self) -> &Arc<SharedProgress> {
        &self.shared
    }

    /// Asks the job to stop at its next checkpoint.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelling(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Whether the job has finished.
    pub fn is_finished(&self) -> bool {
        self.handle
            .as_ref()
            .map(|h| h.is_finished())
            .unwrap_or(true)
    }

    /// Collects the result, once the job has finished.
    ///
    /// Returns `None` while it is still running, so the window can poll without
    /// blocking its message loop.
    pub fn take_result(&mut self) -> Option<Result<T, Error>> {
        if !self.is_finished() {
            return None;
        }
        let handle = self.handle.take()?;
        Some(match handle.join() {
            Ok(result) => result,
            Err(_) => Err(Error::new(
                mjolnir_core::ExitCode::Failure,
                "the backup stopped unexpectedly",
                "the part of MjolnirVSS doing the work ran into an internal error and stopped; anything it had written is incomplete and is not marked as a usable backup",
                "try again; if it keeps happening, please report it with the log from the backup folder",
            )),
        })
    }
}

impl<T> Drop for Worker<T> {
    fn drop(&mut self) {
        // A worker outliving its window would keep a shadow copy alive with
        // nothing watching it, so the job is asked to stop and then waited for.
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_runs_and_reports_its_result() {
        let mut worker = Worker::start(|progress, _cancel| {
            progress.begin("Reading system", Some(100));
            progress.advance(100);
            Ok(42u32)
        });

        let result = loop {
            if let Some(r) = worker.take_result() {
                break r;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn progress_is_visible_to_the_window_while_the_job_runs() {
        let gate = Arc::new(AtomicBool::new(false));
        let thread_gate = Arc::clone(&gate);

        let mut worker = Worker::start(move |progress, _cancel| {
            progress.begin("Reading system", Some(1000));
            progress.advance(500);
            while !thread_gate.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok(())
        });

        // Wait for the worker to have reported something.
        let snapshot = loop {
            let s = worker.progress().read();
            if s.done == 500 {
                break s;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert_eq!(snapshot.stage, "Reading system");
        assert_eq!(snapshot.fraction(), Some(0.5));

        gate.store(true, Ordering::Relaxed);
        while worker.take_result().is_none() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn cancelling_stops_the_job() {
        let mut worker: Worker<()> = Worker::start(|progress, cancel| {
            progress.begin("Reading system", Some(u64::MAX));
            loop {
                cancel.check()?;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });

        worker.cancel();
        assert!(worker.is_cancelling());

        let result = loop {
            if let Some(r) = worker.take_result() {
                break r;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let err = result.unwrap_err();
        assert_eq!(err.exit(), mjolnir_core::ExitCode::Cancelled);
    }

    #[test]
    fn a_panicking_job_becomes_an_explained_error_not_a_crash() {
        let mut worker: Worker<()> = Worker::start(|_progress, _cancel| {
            panic!("something went badly wrong");
        });

        let result = loop {
            if let Some(r) = worker.take_result() {
                break r;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let err = result.unwrap_err();
        assert!(err.what().contains("stopped unexpectedly"));
        assert!(!err.next_step().is_empty());
    }

    #[test]
    fn an_estimate_is_withheld_until_it_would_mean_something() {
        let mut snapshot = ProgressSnapshot {
            total: Some(1000),
            done: 10,
            ..Default::default()
        };
        // Too early to say anything useful.
        assert_eq!(snapshot.seconds_remaining(), None);

        // No total means no estimate, however long it has been running.
        snapshot.total = None;
        snapshot.stage_started = Instant::now() - std::time::Duration::from_secs(60);
        assert_eq!(snapshot.seconds_remaining(), None);

        // Nothing done yet means no estimate either.
        snapshot.total = Some(1000);
        snapshot.done = 0;
        assert_eq!(snapshot.seconds_remaining(), None);
    }

    #[test]
    fn fraction_is_clamped_and_safe_at_zero() {
        let mut snapshot = ProgressSnapshot {
            total: Some(0),
            done: 5,
            ..Default::default()
        };
        assert_eq!(snapshot.fraction(), None);

        snapshot.total = Some(100);
        snapshot.done = 500;
        assert_eq!(snapshot.fraction(), Some(1.0));
    }

    #[test]
    fn notes_do_not_grow_without_limit() {
        let shared = SharedProgress::new();
        let mut sink = SharedProgressSink::new(Arc::clone(&shared));
        for i in 0..600 {
            sink.note(&format!("line {i}"));
        }
        assert!(shared.read().notes.len() <= 500);
        // The most recent line is the one kept.
        assert_eq!(shared.read().notes.last().unwrap(), "line 599");
    }
}
