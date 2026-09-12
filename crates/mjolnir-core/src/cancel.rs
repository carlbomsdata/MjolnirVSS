//! Cooperative cancellation.
//!
//! A backup holds a VSS snapshot and several open device handles. Killing the
//! process outright leaves the snapshot behind until the service times it out,
//! so Ctrl+C is turned into a flag that the copy loops poll. The loops unwind
//! normally, and the RAII guards that own the snapshot release it on the way
//! out.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A shared cancellation flag.
///
/// Cloning gives another handle to the same flag.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    /// A token that has not been cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. Safe to call from a signal handler.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Returns [`crate::Error::cancelled`] if cancellation was requested.
    ///
    /// Copy loops call this once per chunk, which bounds the delay between
    /// Ctrl+C and the snapshot being released to one chunk of work.
    pub fn check(&self) -> crate::Result<()> {
        if self.is_cancelled() {
            Err(crate::Error::cancelled())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_uncancelled() {
        let t = CancelToken::new();
        assert!(!t.is_cancelled());
        assert!(t.check().is_ok());
    }

    #[test]
    fn cancellation_is_visible_through_clones() {
        let a = CancelToken::new();
        let b = a.clone();
        b.cancel();
        assert!(a.is_cancelled());
        assert_eq!(a.check().unwrap_err().exit(), crate::ExitCode::Cancelled);
    }

    #[test]
    fn cancellation_crosses_threads() {
        let a = CancelToken::new();
        let b = a.clone();
        std::thread::spawn(move || b.cancel()).join().unwrap();
        assert!(a.is_cancelled());
    }
}
