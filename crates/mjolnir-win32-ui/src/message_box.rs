//! Message boxes that explain themselves.
//!
//! A raw `HRESULT` in a dialog tells the person in front of the computer
//! nothing. Every error MjolnirVSS shows carries what happened, why it matters
//! and what to do next, and this module is what puts those three things on the
//! screen in that order.

use mjolnir_core::error::Error;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_ICONERROR, MB_ICONQUESTION, MB_ICONWARNING, MB_OK, MB_YESNO,
    MESSAGEBOX_RESULT, MESSAGEBOX_STYLE,
};

use crate::sys::wide;

/// Formats an error the way the product promises.
pub fn format_error(e: &Error) -> String {
    format!(
        "{}\n\nWhy this matters:\n{}\n\nWhat to do next:\n{}",
        capitalise(e.what()),
        capitalise(e.why()),
        capitalise(e.next_step())
    )
}

/// Shows an error, with no parent window.
pub fn error(title: &str, e: &Error) {
    show(
        HWND::default(),
        title,
        &format_error(e),
        MB_ICONERROR | MB_OK,
    );
}

/// Shows an error owned by a window.
pub fn error_for(parent: HWND, title: &str, e: &Error) {
    show(parent, title, &format_error(e), MB_ICONERROR | MB_OK);
}

/// Shows a plain message.
pub fn info(parent: HWND, title: &str, message: &str) {
    show(parent, title, message, MB_OK);
}

/// Shows a warning.
pub fn warn(parent: HWND, title: &str, message: &str) {
    show(parent, title, message, MB_ICONWARNING | MB_OK);
}

/// Asks a yes or no question. Returns true for yes.
pub fn confirm(parent: HWND, title: &str, message: &str) -> bool {
    show(parent, title, message, MB_ICONQUESTION | MB_YESNO) == IDYES
}

fn show(parent: HWND, title: &str, message: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let title_w = wide(title);
    let message_w = wide(message);
    let parent = if parent.is_invalid() {
        None
    } else {
        Some(parent)
    };
    // SAFETY: both strings are null terminated and outlive the call, and the
    // parent handle is either a live window or absent.
    unsafe {
        MessageBoxW(
            parent,
            PCWSTR(message_w.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            style,
        )
    }
}

/// Capitalises the first letter of a sentence.
///
/// The error strings are written lowercase so they read correctly when embedded
/// in a log line; on screen they start a sentence.
fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mjolnir_core::ExitCode;

    #[test]
    fn an_error_is_shown_as_three_parts_in_order() {
        let e = Error::new(
            ExitCode::Destination,
            "the external drive is full",
            "the backup cannot finish without room for the rest of the data",
            "free up space on the drive, or use a larger one, then try again",
        );
        let text = format_error(&e);

        let what = text.find("The external drive is full").expect("what");
        let why = text.find("Why this matters:").expect("why");
        let next = text.find("What to do next:").expect("next");
        assert!(what < why, "what must come before why");
        assert!(why < next, "why must come before next");

        // And never a bare error code on its own.
        assert!(!text.contains("0x8007"));
    }

    #[test]
    fn sentences_start_with_a_capital() {
        assert_eq!(capitalise("the drive is full"), "The drive is full");
        assert_eq!(capitalise(""), "");
        assert_eq!(capitalise("Already capital"), "Already capital");
        // Must not mangle a non ASCII first letter.
        assert_eq!(capitalise("ärlig"), "Ärlig");
    }

    #[test]
    fn a_vss_error_reaches_the_screen_as_advice_not_a_code() {
        let e = Error::new(
            ExitCode::VssFailure,
            "creating the shadow copy failed",
            "another program is creating a shadow copy right now, and Windows allows only one at a time",
            "wait for the other backup to finish and try again",
        );
        let text = format_error(&e);
        // The explanation is capitalised on screen, so the match is on the
        // wording rather than the exact casing of the first word.
        assert!(text.contains("program is creating a shadow copy"), "{text}");
        assert!(text.contains("try again"), "{text}");
        // And never an HRESULT on its own.
        assert!(!text.contains("0x"), "{text}");
    }
}
