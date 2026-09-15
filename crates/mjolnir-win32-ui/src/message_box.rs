//! Message boxes that explain themselves.
//!
//! A raw `HRESULT` in a dialog tells the person in front of the computer
//! nothing. Every error MjolnirVSS shows carries what happened, why it matters
//! and what to do next, and this module is what puts those three things on the
//! screen in that order.

use mjolnir_core::error::Error;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Controls::{
    TaskDialogIndirect, TASKDIALOGCONFIG, TASKDIALOG_BUTTON, TDCBF_CANCEL_BUTTON,
    TDF_EXPAND_FOOTER_AREA, TDF_POSITION_RELATIVE_TO_WINDOW, TDF_USE_COMMAND_LINKS,
    TD_WARNING_ICON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_ICONERROR, MB_ICONQUESTION, MB_ICONWARNING, MB_OK, MB_YESNO,
    MESSAGEBOX_RESULT, MESSAGEBOX_STYLE,
};

use crate::sys::wide;

/// Formats an error the way the product promises.
pub fn format_error(e: &Error) -> String {
    format!(
        // Carriage returns as well as newlines. A message box renders either,
        // but a plain edit control renders only this pair, and the same text
        // goes to both: a failed restore put its three paragraphs onto the
        // wizard's page as one run-on sentence because a lone newline is not a
        // line break to an EDIT.
        "{}\r\n\r\nWhy this matters:\r\n{}\r\n\r\nWhat to do next:\r\n{}",
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

/// A question with the technical part folded away.
///
/// The simple interface shows one sentence and two buttons. Everything an
/// operator might want to check is behind "Show details", which is Windows'
/// own expandable section rather than a second dialog.
///
/// Returns true when the operator chose to go ahead.
pub fn confirm_with_details(
    parent: HWND,
    title: &str,
    instruction: &str,
    content: &str,
    details: &str,
    go_ahead: &str,
) -> bool {
    const GO_AHEAD_ID: i32 = 1000;

    let title_w = wide(title);
    let instruction_w = wide(instruction);
    let content_w = wide(content);
    let details_w = wide(details);
    let go_ahead_w = wide(go_ahead);
    let expand_w = wide("Hide details");
    let collapse_w = wide("Show details");

    let buttons = [TASKDIALOG_BUTTON {
        nButtonID: GO_AHEAD_ID,
        pszButtonText: PCWSTR(go_ahead_w.as_ptr()),
    }];

    let mut config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: parent,
        dwFlags: TDF_EXPAND_FOOTER_AREA | TDF_POSITION_RELATIVE_TO_WINDOW,
        dwCommonButtons: TDCBF_CANCEL_BUTTON,
        pszWindowTitle: PCWSTR(title_w.as_ptr()),
        pszMainInstruction: PCWSTR(instruction_w.as_ptr()),
        pszContent: PCWSTR(content_w.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: GO_AHEAD_ID,
        pszExpandedInformation: PCWSTR(details_w.as_ptr()),
        pszExpandedControlText: PCWSTR(expand_w.as_ptr()),
        pszCollapsedControlText: PCWSTR(collapse_w.as_ptr()),
        ..Default::default()
    };
    config.Anonymous1.pszMainIcon = TD_WARNING_ICON;

    let mut pressed = 0i32;
    // SAFETY: every string and the button array are locals that outlive the
    // call, and `config` points only at them. `pressed` is a live local the
    // call writes the chosen button into. The dialog is modal, so nothing here
    // is freed while it is on screen.
    let shown = unsafe { TaskDialogIndirect(&config, Some(&mut pressed), None, None) };

    match shown {
        Ok(()) => pressed == GO_AHEAD_ID,
        Err(_) => {
            // Older or cut down Windows builds may not have the task dialog.
            // The question still has to be asked, so it falls back to a plain
            // one carrying the same text.
            confirm(
                parent,
                title,
                &format!(
                    "{instruction}

{content}

{details}"
                ),
            )
        }
    }
}

/// Offers two named choices, with the explanation folded away.
///
/// Returns `Some(true)` for the first, `Some(false)` for the second, and `None`
/// when the operator cancels. Two plainly named buttons beat a yes and a no
/// that the reader has to map back onto the question.
pub fn choose(
    parent: HWND,
    title: &str,
    instruction: &str,
    content: &str,
    details: &str,
    first: &str,
    second: &str,
) -> Option<bool> {
    const FIRST_ID: i32 = 1001;
    const SECOND_ID: i32 = 1002;

    let title_w = wide(title);
    let instruction_w = wide(instruction);
    let content_w = wide(content);
    let details_w = wide(details);
    let first_w = wide(first);
    let second_w = wide(second);
    let expand_w = wide("Hide details");
    let collapse_w = wide("Show details");

    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: FIRST_ID,
            pszButtonText: PCWSTR(first_w.as_ptr()),
        },
        TASKDIALOG_BUTTON {
            nButtonID: SECOND_ID,
            pszButtonText: PCWSTR(second_w.as_ptr()),
        },
    ];

    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: parent,
        dwFlags: TDF_USE_COMMAND_LINKS | TDF_EXPAND_FOOTER_AREA | TDF_POSITION_RELATIVE_TO_WINDOW,
        dwCommonButtons: TDCBF_CANCEL_BUTTON,
        pszWindowTitle: PCWSTR(title_w.as_ptr()),
        pszMainInstruction: PCWSTR(instruction_w.as_ptr()),
        pszContent: PCWSTR(content_w.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: FIRST_ID,
        pszExpandedInformation: PCWSTR(details_w.as_ptr()),
        pszExpandedControlText: PCWSTR(expand_w.as_ptr()),
        pszCollapsedControlText: PCWSTR(collapse_w.as_ptr()),
        ..Default::default()
    };

    let mut pressed = 0i32;
    // SAFETY: every string and the button array are locals that outlive the
    // call, and `config` points only at them. `pressed` is a live local the
    // call writes into. The dialog is modal, so nothing is freed while it is
    // on screen.
    let shown = unsafe { TaskDialogIndirect(&config, Some(&mut pressed), None, None) };

    match shown {
        Ok(()) => match pressed {
            FIRST_ID => Some(true),
            SECOND_ID => Some(false),
            _ => None,
        },
        Err(_) => {
            // Without the task dialog, the question still has to be asked.
            let text =
                format!("{instruction}\n\n{content}\n\n{details}\n\nYes: {first}\nNo: {second}");
            if confirm(parent, title, &text) {
                Some(true)
            } else {
                Some(false)
            }
        }
    }
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
    /// The same error text is shown in a message box and written into an edit
    /// control on a results page. An edit control breaks lines only on a
    /// carriage return and newline together, so a lone newline turns three
    /// paragraphs into one run-on sentence. That reached a real screen.
    #[test]
    fn an_error_breaks_its_lines_the_way_an_edit_control_needs() {
        let e = Error::new(
            ExitCode::Failure,
            "writing to the disk failed",
            "the target disk may be failing",
            "try a different disk",
        );
        let text = super::format_error(&e);
        assert!(text.contains("\r\n"), "{text:?}");
        assert!(
            !text.replace("\r\n", "").contains('\n'),
            "a bare newline is left in the text: {text:?}"
        );
        // And the three parts are still separated by a blank line.
        assert_eq!(text.matches("\r\n\r\n").count(), 2, "{text:?}");
    }

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
