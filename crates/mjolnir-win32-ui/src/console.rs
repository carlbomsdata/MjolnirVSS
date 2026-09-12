//! Attaching to the console that started the process.
//!
//! Both applications are built for the windows subsystem, so they have no
//! console of their own. Without this, running `MjolnirVSS.exe inspect` from a
//! prompt would print nothing at all and look like it had crashed.

/// Attaches to the console of the process that started this one, if there is
/// one.
///
/// Silent on failure, which is the normal case when the program was started
/// from Explorer rather than a prompt: there is no parent console to attach to,
/// output goes nowhere, and the exit code still carries the result.
#[cfg(windows)]
pub fn attach_to_parent() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};

    // SAFETY: the call takes a single integer, the documented constant meaning
    // "the parent process", and no pointers at all. It either succeeds, giving
    // this process the parent's standard handles, or fails because there is no
    // parent console or this process already has one. Both outcomes are fine
    // and neither leaves anything to clean up: a console attached this way is
    // released when the process exits.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Does nothing on platforms without a Windows console.
#[cfg(not(windows))]
pub fn attach_to_parent() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attaching_twice_is_harmless() {
        // The test harness owns a console already, so both calls fail. Neither
        // may panic or leave the process without working output.
        attach_to_parent();
        attach_to_parent();
        println!("still able to print after attaching");
    }
}
