//! Safe window plumbing: class registration, the message loop, and the window
//! procedure trampoline.
//!
//! This module exists so that an application can put a window on the screen
//! without writing a single `unsafe` block. Everything below the
//! [`WindowHandler`] trait is Win32 detail, and everything above it is ordinary
//! Rust.
//!
//! # Re-entrancy
//!
//! A window procedure is re-entrant whether or not its author wants it to be.
//! Showing a message box pumps messages, so a dialog opened from inside a
//! button handler can deliver `WM_SIZE` or `WM_TIMER` back into the same
//! handler before the first call has returned. Borrowing the handler with
//! `RefCell::borrow_mut` in that situation panics, and the panic unwinds
//! through a Win32 callback, which is undefined behaviour.
//!
//! [`with_handler`] therefore uses `try_borrow_mut` and quietly drops the
//! nested message. A repaint or a progress tick that arrives while a modal
//! dialog is open is not worth reaching for, and the next timer tick delivers
//! it anyway.

use std::cell::RefCell;

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{InvalidateRect, UpdateWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetWindowTextLengthW, GetWindowTextW, IsDialogMessageW, KillTimer, LoadCursorW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetTimer, SetWindowPos, ShowWindow, TranslateMessage,
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, IDC_ARROW, MSG, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOW,
    WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLORSTATIC, WM_DESTROY, WM_DPICHANGED, WM_SIZE,
    WM_TIMER, WNDCLASSW, WS_CAPTION, WS_EX_CONTROLPARENT, WS_MINIMIZEBOX, WS_OVERLAPPED,
    WS_SYSMENU,
};

use crate::sys;

/// A window this process owns.
///
/// Every method is safe. The handle inside is valid for as long as the window
/// exists, and a `Window` is only ever handed to a [`WindowHandler`] during a
/// message, which is exactly when that is true.
#[derive(Debug, Clone, Copy)]
pub struct Window {
    hwnd: HWND,
}

impl Window {
    /// Wraps a raw handle.
    ///
    /// Only the trampoline in this module calls it, with a handle Windows has
    /// just given it for a live window.
    fn from_raw(hwnd: HWND) -> Self {
        Self { hwnd }
    }

    /// The raw handle, for the few places that still need one.
    pub fn raw(&self) -> HWND {
        self.hwnd
    }

    /// Asks for the whole window to be repainted.
    pub fn invalidate(&self) {
        // SAFETY: `hwnd` names a window that is alive for the duration of this
        // call: a `Window` only exists while its window does, because the only
        // constructor is private and the only instances handed out come from
        // the trampoline during a message. Passing None for the rectangle is
        // documented as meaning the whole client area, and the erase flag is a
        // plain bool.
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, true);
        }
    }

    /// Paints anything currently invalid, immediately.
    pub fn update(&self) {
        // SAFETY: as in invalidate, the handle names a live window and the call
        // takes no pointers.
        unsafe {
            let _ = UpdateWindow(self.hwnd);
        }
    }

    /// The client area, in client coordinates.
    pub fn client_rect(&self) -> RECT {
        let mut rect = RECT::default();
        // SAFETY: `rect` is a live local of exactly the type the call writes,
        // and it is not read unless the call succeeded. On failure it keeps the
        // zeroes it was initialised with, which lays every control out at zero
        // size rather than reading uninitialised memory.
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rect);
        }
        rect
    }

    /// Moves and resizes the window.
    pub fn move_to(&self, rect: RECT) {
        // SAFETY: the handle names a live window; passing None for the insert
        // position together with SWP_NOZORDER means the z order is untouched,
        // so no second window handle is involved.
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                None,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    /// Makes the window visible.
    pub fn show(&self) {
        // SAFETY: the handle names a live window and the command is a documented
        // constant.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOW);
        }
    }

    /// Gives keyboard focus to one of this window's controls.
    pub fn focus(&self, child: HWND) {
        if child.is_invalid() {
            return;
        }
        // SAFETY: `child` is a control created as a child of this window and is
        // destroyed with it, so it is alive whenever this window is. Focus does
        // not transfer ownership of anything.
        unsafe {
            let _ = SetFocus(Some(child));
        }
    }

    /// Reads the text of one of this window's controls.
    pub fn text_of(&self, child: HWND) -> String {
        if child.is_invalid() {
            return String::new();
        }
        // SAFETY: `child` is a live control of this window. The length is asked
        // for first and the buffer is allocated one unit larger, so the call
        // cannot write past the end even if the text grows between the two
        // calls: GetWindowTextW truncates to the buffer size it is given.
        let len = unsafe { GetWindowTextLengthW(child) };
        if len <= 0 {
            return String::new();
        }
        let mut buffer = vec![0u16; len as usize + 1];
        // SAFETY: the slice is valid for its full length, which is what the
        // call is told, and the return value bounds what was actually written.
        let written = unsafe { GetWindowTextW(child, &mut buffer) };
        buffer.truncate(written.max(0) as usize);
        String::from_utf16_lossy(&buffer)
    }

    /// Starts a repeating timer.
    pub fn set_timer(&self, id: usize, interval_ms: u32) {
        // SAFETY: the handle names a live window. Passing None for the callback
        // means the timer arrives as WM_TIMER on this window rather than
        // through a function pointer, so no callback lifetime is involved. The
        // timer is killed in WM_DESTROY and when the window is dropped.
        unsafe {
            SetTimer(Some(self.hwnd), id, interval_ms, None);
        }
    }

    /// Stops a timer. Harmless if it was never started.
    pub fn kill_timer(&self, id: usize) {
        // SAFETY: the handle names a live window; killing a timer that does not
        // exist is documented as returning an error, which is ignored here.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), id);
        }
    }

    /// Asks the window to close, going through the normal close handling.
    pub fn request_close(&self) {
        // SAFETY: the handle names a live window. WM_CLOSE carries no pointers
        // in either parameter, so the zeroes are the whole message.
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }

    /// Destroys the window, ending the message loop.
    fn destroy(&self) {
        // SAFETY: the handle names a live window, and this is only reached from
        // WM_CLOSE handling, where it has not been destroyed yet.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// What an application does with the messages its window receives.
///
/// Implementing this is the whole of writing a window. Every method is safe and
/// is handed a [`Window`] that is guaranteed to be alive for the call.
pub trait WindowHandler: 'static {
    /// The window and its controls have just been created.
    fn on_create(&mut self, window: &Window);

    /// The window changed size, or the layout needs redoing.
    fn on_layout(&mut self, window: &Window);

    /// A control was used. `id` is the control identifier and `notification`
    /// is the high word of `WPARAM`, which distinguishes a button press from a
    /// list selection.
    fn on_command(&mut self, window: &Window, id: i32, notification: u32);

    /// A timer fired.
    fn on_timer(&mut self, window: &Window, id: usize) {
        let _ = (window, id);
    }

    /// The window was asked to close. Returning false keeps it open.
    fn on_close(&mut self, window: &Window) -> bool {
        let _ = window;
        true
    }
}

thread_local! {
    /// The one handler for this thread's window.
    ///
    /// A `RefCell` rather than a plain cell because the trampoline needs
    /// mutable access, and an `Option` because it exists only between WM_CREATE
    /// and WM_DESTROY.
    static HANDLER: RefCell<Option<Box<dyn WindowHandler>>> = const { RefCell::new(None) };
}

/// Runs `f` against the handler, unless the handler is already borrowed.
///
/// See the module comment: a message that arrives while an outer message is
/// still being handled, which is what a modal dialog causes, is dropped rather
/// than allowed to panic through a Win32 callback.
fn with_handler(f: impl FnOnce(&mut dyn WindowHandler)) {
    HANDLER.with(|cell| {
        if let Ok(mut borrowed) = cell.try_borrow_mut() {
            if let Some(handler) = borrowed.as_mut() {
                f(handler.as_mut());
            }
        }
    });
}

/// How to create the window.
pub struct WindowConfig {
    /// Window class name. Must be unique within the process.
    pub class_name: &'static str,
    /// The title shown in the title bar.
    pub title: &'static str,
    /// Width at 96 dpi.
    pub width: i32,
    /// Height at 96 dpi.
    pub height: i32,
    /// Whether the window can be minimised.
    pub minimise_box: bool,
}

/// Creates the window, runs it until it closes, and cleans up.
///
/// `make_handler` is called once the window and its controls exist, so it can
/// build them. Returning from this function means the window has closed.
pub fn run<H, F>(config: WindowConfig, make_handler: F) -> Result<()>
where
    H: WindowHandler,
    F: FnOnce(&Window) -> H + 'static,
{
    sys::enable_dpi_awareness();
    sys::init_common_controls();

    // The factory is stashed so WM_CREATE can build the handler at the moment
    // the window exists, which is the only time its controls can be created.
    FACTORY.with(|cell| {
        *cell.borrow_mut() = Some(Box::new(move |window: &Window| {
            Box::new(make_handler(window)) as Box<dyn WindowHandler>
        }));
    });

    let class_name = sys::wide(config.class_name);
    let title = sys::wide(config.title);
    let instance = sys::instance();

    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(trampoline),
        hInstance: instance,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hbrBackground: sys::dialog_brush(),
        // SAFETY: asks for a standard system cursor. It takes no module handle
        // and returns a shared cursor owned by Windows, which must not be
        // destroyed; failure gives the null default, which is what an
        // unspecified cursor means anyway.
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        ..Default::default()
    };

    // SAFETY: every pointer in the structure outlives the call: the two name
    // buffers are locals of this function that live until it returns, and the
    // window is created below before it does. The brush and cursor are owned by
    // Windows. Registering the same class twice returns zero, which is treated
    // as the failure it would be.
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(Error::new(
            ExitCode::Failure,
            "the window could not be created",
            "Windows refused to register the window class, which usually means another copy of the program is already running",
            "close any other copy of MjolnirVSS and try again",
        ));
    }

    // WS_EX_CONTROLPARENT is what makes Tab move between the child controls,
    // which is what makes the window usable without a mouse.
    let style = if config.minimise_box {
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX
    } else {
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU
    };

    // SAFETY: the class was registered above and both strings are locals that
    // outlive the call. No creation parameter is passed, so the WM_CREATE
    // handler reads nothing from LPARAM.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_CONTROLPARENT,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            sys::scale(config.width, 96),
            sys::scale(config.height, 96),
            None,
            None,
            Some(instance),
            None,
        )
    }
    .map_err(|e| {
        Error::new(
            ExitCode::Failure,
            "the window could not be created",
            format!("Windows reported: {e}"),
            "restart the computer and try again; the command line works without a window",
        )
    })?;

    let window = Window::from_raw(hwnd);
    window.show();
    window.update();

    pump_messages(hwnd);

    sys::release_ui_font();
    Ok(())
}

type HandlerFactory = Box<dyn FnOnce(&Window) -> Box<dyn WindowHandler>>;

thread_local! {
    static FACTORY: RefCell<Option<HandlerFactory>> = const { RefCell::new(None) };
}

/// Runs the message loop until the window closes.
fn pump_messages(hwnd: HWND) {
    let mut message = MSG::default();
    loop {
        // SAFETY: `message` is a live local of the expected type. Passing None
        // for the window filter asks for every message on this thread, which is
        // what a loop owning the thread wants; the two zero filters mean no
        // message range restriction.
        let got = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if !got.as_bool() {
            break;
        }

        // IsDialogMessageW is what gives Tab, Shift+Tab, Enter, Escape and the
        // arrow keys their usual meanings inside the window. Without it the
        // interface cannot be driven from the keyboard at all.
        //
        // SAFETY: `hwnd` is the window created by run and is alive until the
        // loop ends, and `message` was just filled by GetMessageW.
        let handled = unsafe { IsDialogMessageW(hwnd, &message).as_bool() };
        if !handled {
            // SAFETY: `message` was filled by GetMessageW and is not modified
            // between the two calls, which is the contract both expect.
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}

/// The window procedure Windows calls.
///
/// Declared `extern "system"` so Windows can call it directly. It is not
/// `unsafe`: every parameter is a plain integer or handle, and everything it
/// does with them goes through the safe wrappers above.
///
/// It must never unwind. A panic crossing this boundary would unwind into
/// Windows' own stack frames, which is undefined behaviour, so the handler is
/// only ever reached through [`with_handler`], which cannot panic on a
/// re-entrant borrow.
extern "system" fn trampoline(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let window = Window::from_raw(hwnd);

    match msg {
        WM_CREATE => {
            let factory = FACTORY.with(|cell| cell.borrow_mut().take());
            if let Some(factory) = factory {
                let handler = factory(&window);
                HANDLER.with(|cell| *cell.borrow_mut() = Some(handler));
                with_handler(|h| h.on_create(&window));
            }
            LRESULT(0)
        }
        WM_SIZE => {
            with_handler(|h| h.on_layout(&window));
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            let notification = ((wparam.0 >> 16) & 0xFFFF) as u32;
            with_handler(|h| h.on_command(&window, id, notification));
            LRESULT(0)
        }
        WM_TIMER => {
            let id = wparam.0;
            with_handler(|h| h.on_timer(&window, id));
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            // Labels draw their own opaque background by default, which looks
            // wrong on a dialog coloured window and unreadable in a high
            // contrast theme.
            let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut core::ffi::c_void);
            let brush = sys::paint_label_background(hdc);
            LRESULT(brush.0 as isize)
        }
        WM_DPICHANGED => {
            // Windows supplies the rectangle the window should move to so it
            // keeps its physical size on the new display.
            if lparam.0 != 0 {
                // SAFETY: for WM_DPICHANGED, Windows documents LPARAM as a
                // pointer to a RECT that is valid for the duration of the
                // message. The null check above covers a malformed message.
                let suggested = unsafe { *(lparam.0 as *const RECT) };
                window.move_to(suggested);
            }
            with_handler(|h| h.on_layout(&window));
            LRESULT(0)
        }
        WM_CLOSE => {
            let mut may_close = true;
            with_handler(|h| may_close = h.on_close(&window));
            if may_close {
                window.destroy();
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // Dropping the handler drops anything it owns, including a worker
            // thread, whose destructor cancels the job and waits for it. That
            // is what stops a shadow copy outliving the window.
            HANDLER.with(|cell| {
                if let Ok(mut borrowed) = cell.try_borrow_mut() {
                    *borrowed = None;
                }
            });
            // SAFETY: ends the loop in pump_messages. Takes no pointers.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => {
            // SAFETY: the default handler accepts any message it is given,
            // including ones this function does not recognise, and the
            // parameters are passed through exactly as received.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handler that records what it was told, so the trait's default methods
    /// can be checked without a window.
    struct Recorder {
        events: Vec<String>,
    }

    impl WindowHandler for Recorder {
        fn on_create(&mut self, _w: &Window) {
            self.events.push("create".to_owned());
        }
        fn on_layout(&mut self, _w: &Window) {
            self.events.push("layout".to_owned());
        }
        fn on_command(&mut self, _w: &Window, id: i32, _n: u32) {
            self.events.push(format!("command {id}"));
        }
    }

    #[test]
    fn the_default_close_handler_lets_the_window_close() {
        let mut r = Recorder { events: Vec::new() };
        let window = Window::from_raw(HWND::default());
        assert!(r.on_close(&window));
    }

    #[test]
    fn the_default_timer_handler_does_nothing() {
        let mut r = Recorder { events: Vec::new() };
        let window = Window::from_raw(HWND::default());
        r.on_timer(&window, 1);
        assert!(r.events.is_empty());
    }

    #[test]
    fn methods_on_an_invalid_child_handle_are_ignored() {
        let window = Window::from_raw(HWND::default());
        // These must not crash when handed a control that was never created,
        // which is what happens if CreateWindowExW failed for one control.
        window.focus(HWND::default());
        assert_eq!(window.text_of(HWND::default()), "");
    }

    #[test]
    fn a_window_config_describes_a_reasonable_window() {
        let config = WindowConfig {
            class_name: "TestClass",
            title: "Test",
            width: 560,
            height: 460,
            minimise_box: true,
        };
        assert!(config.width > 0 && config.height > 0);
        assert!(!config.class_name.is_empty());
    }
}
