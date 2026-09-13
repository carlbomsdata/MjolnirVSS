//! Win32 primitives: DPI, fonts, window classes and child controls.
//!
//! A deliberately small toolkit. MjolnirVSS needs five buttons and three
//! screens, not a widget library, and native controls are what make it work
//! inside Windows PE, respond to the keyboard and read correctly to a screen
//! reader without any of that being written here.
//!
//! Every control is a real window of a standard class, so tab order, focus
//! rectangles, high contrast themes and accessibility come from Windows.

use std::cell::RefCell;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, GetSysColor, SetBkMode, SetTextColor, COLOR_BTNFACE,
    COLOR_WINDOW, COLOR_WINDOWTEXT, HBRUSH, HDC, HFONT, LOGFONTW, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemServices::SS_LEFT;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_PROGRESS_CLASS, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX,
    PBM_SETPOS, PBM_SETRANGE32, PBM_SETSTATE, PBST_ERROR, PBST_NORMAL, PBST_PAUSED,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, SystemParametersInfoForDpi,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, SendMessageW, SetWindowLongPtrW, SetWindowTextW,
    SystemParametersInfoW, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, DLGC_STATIC, ES_AUTOHSCROLL, ES_LEFT,
    ES_MULTILINE, ES_READONLY, GWLP_WNDPROC, HMENU, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WINDOW_EX_STYLE, WINDOW_STYLE, WM_GETDLGCODE, WM_SETFONT,
    WNDPROC, WS_CHILD, WS_DISABLED, WS_EX_CLIENTEDGE, WS_GROUP, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};

/// Reference device independent pixels per inch.
pub const BASE_DPI: i32 = 96;

thread_local! {
    /// The font every control uses, created once per thread at the right size.
    static UI_FONT: RefCell<Option<HFONT>> = const { RefCell::new(None) };
}

/// Tells Windows this process scales its own windows.
///
/// Without this a high DPI display shows a blurry, bitmap stretched window.
/// Called once, before any window exists.
pub fn enable_dpi_awareness() {
    // SAFETY: no pointers are involved. Failure means an older Windows that
    // does not support per monitor awareness, where the system scales for us
    // and the window is merely less crisp.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Registers the common control classes the window uses.
pub fn init_common_controls() {
    let init = INITCOMMONCONTROLSEX {
        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_STANDARD_CLASSES | ICC_PROGRESS_CLASS,
    };
    // SAFETY: the structure is fully initialised and valid for the call.
    unsafe {
        let _ = InitCommonControlsEx(&init);
    }
}

/// This module's instance handle.
pub fn instance() -> HINSTANCE {
    // SAFETY: passing null asks for the handle of the running executable, which
    // always succeeds.
    unsafe {
        GetModuleHandleW(None)
            .map(HINSTANCE::from)
            .unwrap_or_default()
    }
}

/// Scales a value designed at 96 dpi to the window's actual dpi.
pub fn scale(value: i32, dpi: u32) -> i32 {
    (value * dpi as i32) / BASE_DPI
}

/// The dpi of a window, falling back to 96 when it cannot be determined.
pub fn dpi_of(hwnd: HWND) -> u32 {
    // SAFETY: the handle is valid for the life of the call.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 {
        BASE_DPI as u32
    } else {
        dpi
    }
}

/// Builds the shell's message font at `dpi` and caches it for this thread.
///
/// Using the system font rather than a chosen one is what makes the window look
/// like the rest of Windows, and what makes it follow the user's text size
/// settings.
pub fn ui_font(dpi: u32) -> HFONT {
    UI_FONT.with(|cell| {
        let mut cell = cell.borrow_mut();
        if let Some(font) = *cell {
            return font;
        }

        let mut metrics = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        // SAFETY: the structure is sized correctly and valid for the call. The
        // per dpi variant is used so the font comes back already scaled.
        let ok = unsafe {
            SystemParametersInfoForDpi(
                SPI_GETNONCLIENTMETRICS.0,
                std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
                Some(&mut metrics as *mut _ as *mut core::ffi::c_void),
                0,
                dpi,
            )
        };
        if ok.is_err() {
            // Older Windows without the per dpi call: ask for the unscaled
            // metrics and let the system handle it.
            // SAFETY: as above.
            let _ = unsafe {
                SystemParametersInfoW(
                    SPI_GETNONCLIENTMETRICS,
                    std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
                    Some(&mut metrics as *mut _ as *mut core::ffi::c_void),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                )
            };
        }

        let logfont: LOGFONTW = metrics.lfMessageFont;
        // SAFETY: the structure describes a valid font request.
        let font = unsafe { CreateFontIndirectW(&logfont) };
        *cell = Some(font);
        font
    })
}

/// Releases the cached font. Called as the application exits.
pub fn release_ui_font() {
    UI_FONT.with(|cell| {
        if let Some(font) = cell.borrow_mut().take() {
            // SAFETY: the font was created by CreateFontIndirectW, is owned
            // here, and no window still uses it because this runs after every
            // window has been destroyed.
            unsafe {
                let _ = DeleteObject(font.into());
            }
        }
    });
}

/// Applies the cached font to a control.
pub fn set_font(hwnd: HWND, font: HFONT) {
    // SAFETY: both handles are valid. WM_SETFONT does not take ownership.
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    }
}

/// Converts a Rust string into a null terminated wide string.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Sets a window's text.
pub fn set_text(hwnd: HWND, text: &str) {
    let w = wide(text);
    // SAFETY: the string outlives the call and is null terminated.
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
    }
}

/// Which kind of child control to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlKind {
    /// A push button.
    Button,
    /// The default push button, activated by Enter.
    DefaultButton,
    /// A read only text label.
    Label,
    /// A single line text box.
    TextBox,
    /// A multi line, read only text box with a scroll bar.
    TextArea,
    /// A progress bar.
    ProgressBar,
    /// A list of items the operator picks one of.
    ListBox,
}

impl ControlKind {
    fn class(self) -> PCWSTR {
        match self {
            ControlKind::Button | ControlKind::DefaultButton => w!("BUTTON"),
            ControlKind::Label => w!("STATIC"),
            ControlKind::TextBox | ControlKind::TextArea => w!("EDIT"),
            ControlKind::ProgressBar => w!("msctls_progress32"),
            ControlKind::ListBox => w!("LISTBOX"),
        }
    }

    fn style(self) -> WINDOW_STYLE {
        let base = WS_CHILD | WS_VISIBLE;
        match self {
            ControlKind::Button => base | WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
            ControlKind::DefaultButton => {
                base | WS_TABSTOP | WS_GROUP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32)
            }
            ControlKind::Label => base | WINDOW_STYLE(SS_LEFT.0),
            ControlKind::TextBox => {
                base | WS_TABSTOP | WINDOW_STYLE((ES_LEFT | ES_AUTOHSCROLL) as u32)
            }
            // Read only, and deliberately not a tab stop. It is output, not a
            // control: a multiline edit tells Windows it wants the Return key,
            // so when the dialog manager parked focus here Return did nothing
            // and the window could not be advanced without a mouse. Found by
            // running the recovery application in Windows PE.
            ControlKind::TextArea => {
                base | WS_VSCROLL | WINDOW_STYLE((ES_LEFT | ES_MULTILINE | ES_READONLY) as u32)
            }
            ControlKind::ProgressBar => base,
            // LBS_NOTIFY is what makes a selection arrive as WM_COMMAND, and
            // the scroll bar matters because a recovery machine can have more
            // backups on a drive than fit on one screen.
            ControlKind::ListBox => {
                base | WS_TABSTOP
                    | WS_VSCROLL
                    | WINDOW_STYLE(windows::Win32::UI::WindowsAndMessaging::LBS_NOTIFY as u32)
            }
        }
    }

    fn ex_style(self) -> WINDOW_EX_STYLE {
        match self {
            ControlKind::TextBox | ControlKind::TextArea | ControlKind::ListBox => WS_EX_CLIENTEDGE,
            _ => WINDOW_EX_STYLE(0),
        }
    }
}

/// Creates a child control.
///
/// `id` is the control identifier reported back in `WM_COMMAND`, which is how
/// button presses are recognised.
pub fn create_control(parent: HWND, kind: ControlKind, text: &str, id: i32, font: HFONT) -> HWND {
    let text_w = wide(text);
    // SAFETY: the class name is a static wide string, the text outlives the
    // call, and the parent handle is valid. A failed creation returns an
    // invalid handle, which every caller tolerates because a control that could
    // not be created simply is not drawn.
    let hwnd = unsafe {
        CreateWindowExW(
            kind.ex_style(),
            kind.class(),
            PCWSTR(text_w.as_ptr()),
            kind.style(),
            0,
            0,
            10,
            10,
            Some(parent),
            Some(HMENU(id as *mut core::ffi::c_void)),
            Some(instance()),
            None,
        )
    }
    .unwrap_or_default();

    if !hwnd.is_invalid() {
        set_font(hwnd, font);
        if kind == ControlKind::TextArea {
            make_output_only(hwnd);
        }
    }
    hwnd
}

/// The window procedure an edit control had before it was made output only.
///
/// Every edit control on the system shares one, so one slot is enough.
static ORIGINAL_EDIT_PROC: std::sync::OnceLock<isize> = std::sync::OnceLock::new();

/// Stops a read only text area from taking the keyboard hostage.
///
/// A multiline edit control answers `WM_GETDLGCODE` by asking for every key,
/// including Tab and Return. That is right for something being typed into and
/// wrong for something only being read: once the keyboard reached the body of
/// the recovery wizard, Tab could not leave it and Return could not press
/// anything, so the window could not be operated at all without a mouse. In
/// Windows PE that is close to fatal.
///
/// Answering `DLGC_STATIC` instead tells the dialog manager this is text, not a
/// control: Tab moves past it and Return goes to the default button. The mouse
/// can still scroll it, which is the only reason it is an edit control and not
/// a label.
fn make_output_only(hwnd: HWND) {
    // SAFETY: the handle names a live edit control this module just created.
    // The previous procedure is kept and called for every message this one does
    // not answer, which is what subclassing requires.
    unsafe {
        let previous = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, output_only_proc as isize);
        if previous != 0 {
            let _ = ORIGINAL_EDIT_PROC.set(previous);
        }
    }
}

/// The replacement procedure installed by [`make_output_only`].
unsafe extern "system" fn output_only_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_GETDLGCODE {
        return LRESULT(DLGC_STATIC as isize);
    }
    let previous = ORIGINAL_EDIT_PROC.get().copied().unwrap_or_default();
    if previous == 0 {
        // SAFETY: falling back to the default procedure is always valid.
        return unsafe {
            windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wparam, lparam)
        };
    }
    // SAFETY: `previous` came from SetWindowLongPtrW(GWLP_WNDPROC) on a control
    // of this class, so it is a window procedure with this signature.
    let original: WNDPROC = unsafe { std::mem::transmute(previous) };
    // SAFETY: every argument is passed through unchanged to the procedure that
    // was handling these messages a moment ago.
    unsafe { CallWindowProcW(original, hwnd, msg, wparam, lparam) }
}

/// Moves and resizes a control.
pub fn place(hwnd: HWND, rect: RECT) {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};
    if hwnd.is_invalid() {
        return;
    }
    // SAFETY: the handle is valid and the rectangle is plain data.
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Shows or hides a control.
pub fn show(hwnd: HWND, visible: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOW};
    if hwnd.is_invalid() {
        return;
    }
    // SAFETY: the handle is valid.
    unsafe {
        let _ = ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
    }
}

/// Enables or disables a control.
pub fn enable(hwnd: HWND, enabled: bool) {
    use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    if hwnd.is_invalid() {
        return;
    }
    // SAFETY: the handle is valid.
    unsafe {
        let _ = EnableWindow(hwnd, enabled);
    }
}

/// Sets a progress bar's range and position, in percent.
pub fn set_progress(hwnd: HWND, percent: u32) {
    if hwnd.is_invalid() {
        return;
    }
    // SAFETY: the handle is a progress bar created by this module.
    unsafe {
        SendMessageW(hwnd, PBM_SETRANGE32, Some(WPARAM(0)), Some(LPARAM(1000)));
        SendMessageW(
            hwnd,
            PBM_SETPOS,
            Some(WPARAM(percent.min(1000) as usize)),
            None,
        );
    }
}

/// Empties a list.
pub fn list_clear(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::LB_RESETCONTENT;
    if hwnd.is_invalid() {
        return;
    }
    // SAFETY: the handle is a list box created by this module.
    unsafe {
        SendMessageW(hwnd, LB_RESETCONTENT, None, None);
    }
}

/// Appends one item to a list.
pub fn list_add(hwnd: HWND, text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::LB_ADDSTRING;
    if hwnd.is_invalid() {
        return;
    }
    let w = wide(text);
    // SAFETY: the handle is a list box and the string outlives the call.
    unsafe {
        SendMessageW(hwnd, LB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
}

/// The index of the selected item, if anything is selected.
pub fn list_selected(hwnd: HWND) -> Option<usize> {
    use windows::Win32::UI::WindowsAndMessaging::LB_GETCURSEL;
    if hwnd.is_invalid() {
        return None;
    }
    // SAFETY: the handle is a list box created by this module.
    let index = unsafe { SendMessageW(hwnd, LB_GETCURSEL, None, None) };
    // LB_ERR is -1 and means nothing is selected, which is the state a
    // destructive screen starts in on purpose.
    if index.0 < 0 {
        None
    } else {
        Some(index.0 as usize)
    }
}

/// Selects an item, or clears the selection when given `None`.
pub fn list_select(hwnd: HWND, index: Option<usize>) {
    use windows::Win32::UI::WindowsAndMessaging::LB_SETCURSEL;
    if hwnd.is_invalid() {
        return;
    }
    let value = index.map(|i| i as isize).unwrap_or(-1);
    // SAFETY: the handle is a list box created by this module.
    unsafe {
        SendMessageW(hwnd, LB_SETCURSEL, Some(WPARAM(value as usize)), None);
    }
}

/// How a progress bar should look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    /// Running normally.
    Normal,
    /// Paused or cancelling.
    Paused,
    /// Failed.
    Error,
}

/// Sets a progress bar's colour state.
pub fn set_progress_state(hwnd: HWND, state: ProgressState) {
    if hwnd.is_invalid() {
        return;
    }
    let value = match state {
        ProgressState::Normal => PBST_NORMAL,
        ProgressState::Paused => PBST_PAUSED,
        ProgressState::Error => PBST_ERROR,
    };
    // SAFETY: the handle is a progress bar created by this module.
    unsafe {
        SendMessageW(hwnd, PBM_SETSTATE, Some(WPARAM(value as usize)), None);
    }
}

/// Paints a label with the system window colours.
///
/// Without this, labels on a dialog coloured background draw their own opaque
/// white rectangle, which looks wrong in dark high contrast themes.
pub fn paint_label_background(hdc: HDC) -> HBRUSH {
    // SAFETY: the device context comes from a WM_CTLCOLORSTATIC message and is
    // valid for the duration of handling it.
    unsafe {
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(GetSysColor(COLOR_WINDOWTEXT)));
    }
    system_brush(COLOR_BTNFACE)
}

/// A system colour brush. Not owned, so it must not be deleted.
pub fn system_brush(index: windows::Win32::Graphics::Gdi::SYS_COLOR_INDEX) -> HBRUSH {
    use windows::Win32::Graphics::Gdi::GetSysColorBrush;
    // SAFETY: system brushes are owned by Windows and live for the process.
    unsafe { GetSysColorBrush(index) }
}

/// The default window background brush.
pub fn window_brush() -> HBRUSH {
    system_brush(COLOR_WINDOW)
}

/// The dialog background brush.
pub fn dialog_brush() -> HBRUSH {
    system_brush(COLOR_BTNFACE)
}

/// Builds a rectangle from a position and size.
pub const fn rect(x: i32, y: i32, width: i32, height: i32) -> RECT {
    RECT {
        left: x,
        top: y,
        right: x + width,
        bottom: y + height,
    }
}

/// Marks a control as not participating in the tab order.
pub const NO_TABSTOP: WINDOW_STYLE = WINDOW_STYLE(0);

/// Convenience alias so callers do not import the Win32 types.
pub type WindowHandle = HWND;

/// Result type for a window procedure.
pub type ProcResult = LRESULT;

/// Marks a window as disabled at creation time.
pub const INITIALLY_DISABLED: WINDOW_STYLE = WS_DISABLED;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_matches_the_reference_dpi() {
        assert_eq!(scale(100, 96), 100);
        assert_eq!(scale(100, 192), 200);
        assert_eq!(scale(100, 144), 150);
        assert_eq!(scale(0, 144), 0);
    }

    #[test]
    fn wide_strings_are_null_terminated() {
        assert_eq!(wide("Hi"), vec![72, 105, 0]);
        assert_eq!(wide(""), vec![0]);
    }

    #[test]
    fn rect_has_the_requested_size() {
        let r = rect(10, 20, 100, 40);
        assert_eq!(r.left, 10);
        assert_eq!(r.top, 20);
        assert_eq!(r.right - r.left, 100);
        assert_eq!(r.bottom - r.top, 40);
    }

    #[test]
    fn every_control_kind_has_a_class_and_a_style() {
        for kind in [
            ControlKind::Button,
            ControlKind::DefaultButton,
            ControlKind::Label,
            ControlKind::TextBox,
            ControlKind::TextArea,
            ControlKind::ProgressBar,
            ControlKind::ListBox,
        ] {
            assert!(!kind.class().is_null());
            // Every control is a visible child; that is the minimum.
            let style = kind.style();
            assert_ne!(style.0 & WS_CHILD.0, 0, "{kind:?}");
            assert_ne!(style.0 & WS_VISIBLE.0, 0, "{kind:?}");
        }
    }

    #[test]
    fn interactive_controls_are_reachable_by_keyboard() {
        // Anything a user has to operate must be in the tab order, otherwise
        // the window cannot be driven without a mouse.
        for kind in [
            ControlKind::Button,
            ControlKind::DefaultButton,
            ControlKind::TextBox,
            ControlKind::ListBox,
        ] {
            assert_ne!(
                kind.style().0 & WS_TABSTOP.0,
                0,
                "{kind:?} is not reachable by keyboard"
            );
        }
        // Nothing that only shows things may take focus. A read only text area
        // is the one that is easy to get wrong: it looks like a control and
        // behaves like one, and while it was in the tab order the dialog
        // manager gave it the keyboard and it swallowed Return.
        for kind in [
            ControlKind::Label,
            ControlKind::ProgressBar,
            ControlKind::TextArea,
        ] {
            assert_eq!(kind.style().0 & WS_TABSTOP.0, 0, "{kind:?} steals focus");
        }
    }
}
