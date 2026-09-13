//! Cooperative cancellation.
//!
//! A backup holds a VSS snapshot and several open device handles. Killing the
//! process outright leaves the snapshot behind until the service times it out,
//! so Ctrl+C is turned into a flag that the copy loops poll. The loops unwind
//! normally, and the RAII guards that own the snapshot release it on the way
//! out.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

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

/// The one token a Ctrl+C cancels.
///
/// A console control handler runs on a thread the operating system injects,
/// with no way to be handed anything, so the token it cancels has to be
/// reachable from nowhere in particular. Every entry point takes its token from
/// here rather than making its own, so that the handler and the copy loop are
/// certain to be looking at the same flag.
///
/// The handler itself lives in the graphical crate, which is where the console
/// is dealt with; this side is deliberately free of any platform code.
pub fn process_token() -> &'static CancelToken {
    static PROCESS: OnceLock<CancelToken> = OnceLock::new();
    PROCESS.get_or_init(CancelToken::new)
}

/// Requests cancellation of [`process_token`].
///
/// Written to be callable from a console control handler: it takes no
/// arguments, allocates nothing after the first call, and cannot fail.
pub fn cancel_process() {
    process_token().cancel();
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

    #[test]
    fn the_process_token_is_one_token() {
        // Two calls have to be the same flag, or a handler would cancel
        // something the copy loop is not watching.
        let a = process_token();
        let b = process_token();
        assert!(std::ptr::eq(a, b));
    }

    #[test]
    fn cancelling_the_process_is_seen_through_a_clone() {
        // The handler cancels the static; the copy loop holds a clone. This is
        // the path a Ctrl+C actually takes.
        let held = process_token().clone();
        cancel_process();
        assert!(held.is_cancelled(), "a clone must see the handler's flag");
        assert!(process_token().check().is_err());
    }
}
