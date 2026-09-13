//! The MjolnirVSS window.
//!
//! One small window, four screens, and no navigation to learn:
//!
//! * the menu, where **Back up this PC** is the obvious thing to press;
//! * the destination screen, which shows what will be captured and asks where
//!   to put it;
//! * the progress screen, with plain language stages and a Cancel button;
//! * the result screen, which says whether it worked.
//!
//! Nothing here knows how a backup is taken, and nothing here talks to Windows
//! directly: it implements [`WindowHandler`], starts the engine on a worker
//! thread, reads a shared progress record on a timer, and paints. That is the
//! whole design, and it is why the window never stops responding and why this
//! file contains no `unsafe`.

use std::path::PathBuf;

use mjolnir_backup::{BackupOutcome, BackupPlan, BackupRequest, CaptureLimit};
use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::ids::BackupName;
use mjolnir_core::progress::format_bytes;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::HFONT;

use mjolnir_win32_ui::sys::{self, ControlKind, ProgressState};
use mjolnir_win32_ui::window::{Window, WindowConfig, WindowHandler};
use mjolnir_win32_ui::worker::Worker;
use mjolnir_win32_ui::{message_box, shell};

/// Width of the window at 96 dpi.
const WINDOW_WIDTH: i32 = 560;
/// Height of the window at 96 dpi.
const WINDOW_HEIGHT: i32 = 460;
/// Margin around the content at 96 dpi.
const MARGIN: i32 = 18;
/// Height of an ordinary button at 96 dpi.
const BUTTON_HEIGHT: i32 = 32;
/// Height of the primary action button at 96 dpi.
const PRIMARY_HEIGHT: i32 = 52;
/// Height of one line of text at 96 dpi.
const LINE: i32 = 20;

// Control identifiers. Stable numbers so a command can be matched on them.
const ID_BACKUP: i32 = 1001;
const ID_RESTORE_FILES: i32 = 1002;
const ID_RECOVERY_MEDIA: i32 = 1003;
const ID_SETTINGS: i32 = 1004;
const ID_EXIT: i32 = 1005;

const ID_SUMMARY: i32 = 1100;
const ID_DEST_LABEL: i32 = 1101;
const ID_DEST_EDIT: i32 = 1102;
const ID_DEST_BROWSE: i32 = 1103;
const ID_NAME_LABEL: i32 = 1104;
const ID_NAME_EDIT: i32 = 1105;
const ID_SPACE: i32 = 1106;
const ID_START: i32 = 1107;
const ID_DEST_BACK: i32 = 1108;

const ID_STAGE: i32 = 1200;
const ID_PROGRESS: i32 = 1201;
const ID_STATS: i32 = 1202;
const ID_CANCEL: i32 = 1203;
const ID_DETAILS_TOGGLE: i32 = 1204;
const ID_DETAILS: i32 = 1205;

const ID_RESULT_TITLE: i32 = 1300;
const ID_RESULT_BODY: i32 = 1301;
const ID_OPEN_FOLDER: i32 = 1302;
const ID_MAKE_MEDIA: i32 = 1303;
const ID_CLOSE: i32 = 1304;

/// Timer that polls the worker for progress.
const TIMER_PROGRESS: usize = 1;
/// How often to repaint progress, in milliseconds.
const TIMER_INTERVAL: u32 = 200;

/// Notification code a text box sends when its contents change.
const EN_CHANGE: u32 = 0x0300;

/// Which screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Menu,
    Destination,
    Progress,
    Result,
}

/// Every control in the window. Created once, shown per screen.
#[derive(Default)]
struct Controls {
    backup: HWND,
    restore_files: HWND,
    recovery_media: HWND,
    settings: HWND,
    exit: HWND,

    summary: HWND,
    dest_label: HWND,
    dest_edit: HWND,
    dest_browse: HWND,
    name_label: HWND,
    name_edit: HWND,
    space: HWND,
    start: HWND,
    dest_back: HWND,

    stage: HWND,
    progress: HWND,
    stats: HWND,
    cancel: HWND,
    details_toggle: HWND,
    details: HWND,

    result_title: HWND,
    result_body: HWND,
    open_folder: HWND,
    make_media: HWND,
    close: HWND,
}

impl Controls {
    fn all(&self) -> [HWND; 25] {
        [
            self.backup,
            self.restore_files,
            self.recovery_media,
            self.settings,
            self.exit,
            self.summary,
            self.dest_label,
            self.dest_edit,
            self.dest_browse,
            self.name_label,
            self.name_edit,
            self.space,
            self.start,
            self.dest_back,
            self.stage,
            self.progress,
            self.stats,
            self.cancel,
            self.details_toggle,
            self.details,
            self.result_title,
            self.result_body,
            self.open_folder,
            self.make_media,
            self.close,
        ]
    }

    fn for_screen(&self, screen: Screen) -> Vec<HWND> {
        match screen {
            Screen::Menu => vec![
                self.backup,
                self.restore_files,
                self.recovery_media,
                self.settings,
                self.exit,
            ],
            Screen::Destination => vec![
                self.summary,
                self.dest_label,
                self.dest_edit,
                self.dest_browse,
                self.name_label,
                self.name_edit,
                self.space,
                self.start,
                self.dest_back,
            ],
            Screen::Progress => vec![
                self.stage,
                self.progress,
                self.stats,
                self.cancel,
                self.details_toggle,
                self.details,
            ],
            Screen::Result => vec![
                self.result_title,
                self.result_body,
                self.open_folder,
                self.make_media,
                self.close,
            ],
        }
    }
}

/// The window's state.
pub struct BackupWindow {
    font: HFONT,
    screen: Screen,
    controls: Controls,
    plan: Option<BackupPlan>,
    worker: Option<Worker<BackupOutcome>>,
    finished: Option<std::result::Result<BackupOutcome, Error>>,
    show_details: bool,
}

impl BackupWindow {
    /// Builds the window's controls. Called once, when the window exists.
    pub fn new(window: &Window) -> Self {
        let font = sys::ui_font(sys::dpi_of(window.raw()));
        let mut me = Self {
            font,
            screen: Screen::Menu,
            controls: Controls::default(),
            plan: None,
            worker: None,
            finished: None,
            show_details: false,
        };
        me.create_controls(window);
        me
    }

    fn create_controls(&mut self, window: &Window) {
        let f = self.font;
        let h = window.raw();
        let c = &mut self.controls;

        c.backup = sys::create_control(
            h,
            ControlKind::DefaultButton,
            "Back up this PC",
            ID_BACKUP,
            f,
        );
        c.restore_files =
            sys::create_control(h, ControlKind::Button, "Restore files", ID_RESTORE_FILES, f);
        c.recovery_media = sys::create_control(
            h,
            ControlKind::Button,
            "Recovery media",
            ID_RECOVERY_MEDIA,
            f,
        );
        c.settings = sys::create_control(h, ControlKind::Button, "Settings", ID_SETTINGS, f);
        c.exit = sys::create_control(h, ControlKind::Button, "Exit", ID_EXIT, f);

        c.summary = sys::create_control(h, ControlKind::TextArea, "", ID_SUMMARY, f);
        c.dest_label = sys::create_control(
            h,
            ControlKind::Label,
            "Save the backup to:",
            ID_DEST_LABEL,
            f,
        );
        c.dest_edit = sys::create_control(h, ControlKind::TextBox, "", ID_DEST_EDIT, f);
        c.dest_browse = sys::create_control(h, ControlKind::Button, "Browse...", ID_DEST_BROWSE, f);
        c.name_label = sys::create_control(h, ControlKind::Label, "Backup name:", ID_NAME_LABEL, f);
        c.name_edit = sys::create_control(h, ControlKind::TextBox, "", ID_NAME_EDIT, f);
        c.space = sys::create_control(h, ControlKind::Label, "", ID_SPACE, f);
        c.start = sys::create_control(h, ControlKind::DefaultButton, "Start backup", ID_START, f);
        c.dest_back = sys::create_control(h, ControlKind::Button, "Back", ID_DEST_BACK, f);

        c.stage = sys::create_control(h, ControlKind::Label, "", ID_STAGE, f);
        c.progress = sys::create_control(h, ControlKind::ProgressBar, "", ID_PROGRESS, f);
        c.stats = sys::create_control(h, ControlKind::Label, "", ID_STATS, f);
        c.cancel = sys::create_control(h, ControlKind::Button, "Cancel", ID_CANCEL, f);
        c.details_toggle =
            sys::create_control(h, ControlKind::Button, "Show details", ID_DETAILS_TOGGLE, f);
        c.details = sys::create_control(h, ControlKind::TextArea, "", ID_DETAILS, f);

        c.result_title = sys::create_control(h, ControlKind::Label, "", ID_RESULT_TITLE, f);
        c.result_body = sys::create_control(h, ControlKind::TextArea, "", ID_RESULT_BODY, f);
        c.open_folder =
            sys::create_control(h, ControlKind::Button, "Open folder", ID_OPEN_FOLDER, f);
        c.make_media = sys::create_control(
            h,
            ControlKind::Button,
            "Create recovery media",
            ID_MAKE_MEDIA,
            f,
        );
        c.close = sys::create_control(h, ControlKind::DefaultButton, "Close", ID_CLOSE, f);
    }

    fn show_screen(&mut self, window: &Window, screen: Screen) {
        self.screen = screen;
        let visible = self.controls.for_screen(screen);
        for hwnd in self.controls.all() {
            sys::show(hwnd, visible.contains(&hwnd));
        }
        if screen == Screen::Progress {
            sys::show(self.controls.details, self.show_details);
        }
        self.layout(window);

        let focus = match screen {
            Screen::Menu => self.controls.backup,
            Screen::Destination => self.controls.start,
            Screen::Progress => self.controls.cancel,
            Screen::Result => self.controls.close,
        };
        window.focus(focus);
        window.invalidate();
    }

    fn layout(&self, window: &Window) {
        let dpi = sys::dpi_of(window.raw());
        let s = |v: i32| sys::scale(v, dpi);
        let client: RECT = window.client_rect();
        let width = client.right - client.left;
        let inner = width - s(MARGIN) * 2;
        let x = s(MARGIN);
        let c = &self.controls;

        match self.screen {
            Screen::Menu => {
                let mut y = s(MARGIN);
                // The primary action is taller and sits alone at the top, so
                // there is never a question about what to press.
                sys::place(c.backup, sys::rect(x, y, inner, s(PRIMARY_HEIGHT)));
                y += s(PRIMARY_HEIGHT) + s(MARGIN);
                for hwnd in [c.restore_files, c.recovery_media, c.settings, c.exit] {
                    sys::place(hwnd, sys::rect(x, y, inner, s(BUTTON_HEIGHT)));
                    y += s(BUTTON_HEIGHT) + s(8);
                }
            }
            Screen::Destination => {
                let mut y = s(MARGIN);
                let summary_height = s(LINE) * 7;
                sys::place(c.summary, sys::rect(x, y, inner, summary_height));
                y += summary_height + s(MARGIN);

                sys::place(c.dest_label, sys::rect(x, y, inner, s(LINE)));
                y += s(LINE) + s(4);
                let browse_width = s(90);
                sys::place(
                    c.dest_edit,
                    sys::rect(x, y, inner - browse_width - s(8), s(26)),
                );
                sys::place(
                    c.dest_browse,
                    sys::rect(x + inner - browse_width, y, browse_width, s(26)),
                );
                y += s(26) + s(10);

                sys::place(c.name_label, sys::rect(x, y, inner, s(LINE)));
                y += s(LINE) + s(4);
                sys::place(c.name_edit, sys::rect(x, y, inner, s(26)));
                y += s(26) + s(10);

                sys::place(c.space, sys::rect(x, y, inner, s(LINE) * 3));

                let bottom = client.bottom - s(MARGIN) - s(BUTTON_HEIGHT);
                sys::place(c.dest_back, sys::rect(x, bottom, s(90), s(BUTTON_HEIGHT)));
                let start_width = s(150);
                sys::place(
                    c.start,
                    sys::rect(
                        x + inner - start_width,
                        bottom,
                        start_width,
                        s(BUTTON_HEIGHT),
                    ),
                );
            }
            Screen::Progress => {
                let mut y = s(MARGIN);
                sys::place(c.stage, sys::rect(x, y, inner, s(LINE) + s(6)));
                y += s(LINE) + s(12);
                sys::place(c.progress, sys::rect(x, y, inner, s(24)));
                y += s(24) + s(12);
                sys::place(c.stats, sys::rect(x, y, inner, s(LINE) * 4));
                y += s(LINE) * 4 + s(8);

                sys::place(c.details_toggle, sys::rect(x, y, s(120), s(BUTTON_HEIGHT)));
                y += s(BUTTON_HEIGHT) + s(8);

                let bottom = client.bottom - s(MARGIN) - s(BUTTON_HEIGHT);
                if self.show_details {
                    let height = (bottom - y - s(12)).max(s(40));
                    sys::place(c.details, sys::rect(x, y, inner, height));
                }
                let cancel_width = s(120);
                sys::place(
                    c.cancel,
                    sys::rect(
                        x + inner - cancel_width,
                        bottom,
                        cancel_width,
                        s(BUTTON_HEIGHT),
                    ),
                );
            }
            Screen::Result => {
                let mut y = s(MARGIN);
                sys::place(c.result_title, sys::rect(x, y, inner, s(LINE) + s(8)));
                y += s(LINE) + s(16);

                let bottom = client.bottom - s(MARGIN) - s(BUTTON_HEIGHT);
                let body_height = (bottom - y - s(12)).max(s(60));
                sys::place(c.result_body, sys::rect(x, y, inner, body_height));

                sys::place(
                    c.open_folder,
                    sys::rect(x, bottom, s(110), s(BUTTON_HEIGHT)),
                );
                sys::place(
                    c.make_media,
                    sys::rect(x + s(118), bottom, s(160), s(BUTTON_HEIGHT)),
                );
                sys::place(
                    c.close,
                    sys::rect(x + inner - s(90), bottom, s(90), s(BUTTON_HEIGHT)),
                );
            }
        }
    }

    // ---- actions ---------------------------------------------------------

    fn begin_backup_setup(&mut self, window: &Window) {
        let name = default_name();

        // Planning talks to the disks and can refuse, so it happens before the
        // operator is asked to choose anything.
        let probe = BackupRequest {
            destination: PathBuf::from("."),
            name: name.clone(),
            scope: mjolnir_backup::BackupScope::SystemDisk,
            limit: CaptureLimit::Everything,
        };

        match mjolnir_backup::plan(&probe) {
            Ok(plan) => {
                let mut text = plan.summary_lines().join("\r\n");
                text.push_str(&format!(
                    "\r\n\r\nThere is {} to read.",
                    format_bytes(plan.source_bytes)
                ));
                for w in &plan.warnings {
                    text.push_str(&format!("\r\n\r\nNote: {w}"));
                }
                sys::set_text(self.controls.summary, &text);
                sys::set_text(self.controls.name_edit, name.as_str());
                self.plan = Some(plan);
                self.update_space_label(window);
                self.show_screen(window, Screen::Destination);
            }
            Err(e) => message_box::error_for(window.raw(), "MjolnirVSS", &e),
        }
    }

    fn update_space_label(&self, window: &Window) {
        let destination = window.text_of(self.controls.dest_edit);
        let Some(plan) = &self.plan else {
            return;
        };

        if destination.trim().is_empty() {
            sys::set_text(
                self.controls.space,
                "Choose a folder on an external drive. It must not be on the disk being backed up.",
            );
            sys::enable(self.controls.start, false);
            return;
        }

        let needed = plan.source_bytes;
        let mut text = match shell::free_space_of(&destination) {
            Some(free) => format!(
                "This drive has {} free. The backup will read {} and is normally smaller once compressed.",
                format_bytes(free),
                format_bytes(needed)
            ),
            None => format!(
                "The backup will read {}. It is normally smaller once compressed.",
                format_bytes(needed)
            ),
        };

        // A backup holds everything on the disk, and MjolnirVSS does not
        // encrypt it. Saying so where the destination is chosen is the only
        // place it is actually useful.
        if plan.contains_decrypted_data {
            text.push_str(
                "\r\nWarning: this backup will contain readable copies of your files, including from the encrypted drive. The backup itself is not encrypted.",
            );
        }

        sys::set_text(self.controls.space, &text);
        sys::enable(self.controls.start, true);
    }

    fn start_backup(&mut self, window: &Window) {
        // Refusing a second job is what stops two shadow copies and two writers
        // running against the same destination.
        if self.worker.is_some() {
            message_box::warn(window.raw(), "MjolnirVSS", "A backup is already running.");
            return;
        }

        let destination = PathBuf::from(window.text_of(self.controls.dest_edit).trim().to_owned());
        let name_text = window.text_of(self.controls.name_edit).trim().to_owned();

        if destination.as_os_str().is_empty() {
            message_box::warn(
                window.raw(),
                "MjolnirVSS",
                "Choose a folder to save the backup in first.",
            );
            return;
        }
        let name = match BackupName::new(name_text) {
            Ok(name) => name,
            Err(e) => {
                message_box::warn(
                    window.raw(),
                    "MjolnirVSS",
                    &format!(
                        "That backup name cannot be used as a folder name.\n\n{e}\n\nUse letters, digits, dashes and underscores."
                    ),
                );
                return;
            }
        };

        let request = BackupRequest {
            destination,
            name,
            scope: mjolnir_backup::BackupScope::SystemDisk,
            limit: CaptureLimit::Everything,
        };

        // Re-planned against the real destination, so the "not on the source
        // disk" check runs against what was actually chosen.
        let plan = match mjolnir_backup::plan(&request) {
            Ok(plan) => plan,
            Err(e) => {
                message_box::error_for(window.raw(), "MjolnirVSS", &e);
                return;
            }
        };

        // Taking a shadow copy can cost the machine its restore points. One
        // sentence, two buttons, and the figures folded away behind Show
        // details.
        if let Some(warning) = plan.snapshot_preflight.warning() {
            let went_ahead = message_box::confirm_with_details(
                window.raw(),
                "MjolnirVSS",
                warning,
                "MjolnirVSS removes only the snapshot it creates, but Windows manages the space they share and may remove older ones to reclaim it. Your files are not affected.",
                &plan.snapshot_preflight.details().join("\r\n"),
                "Continue",
            );
            if !went_ahead {
                return;
            }
        }

        self.plan = Some(plan.clone());
        self.finished = None;
        sys::set_text(self.controls.stage, "Starting...");
        sys::set_text(self.controls.stats, "");
        sys::set_text(self.controls.details, "");
        sys::set_progress(self.controls.progress, 0);
        sys::set_progress_state(self.controls.progress, ProgressState::Normal);
        sys::enable(self.controls.cancel, true);
        sys::set_text(self.controls.cancel, "Cancel");

        self.worker = Some(Worker::start(move |progress, cancel| {
            mjolnir_backup::run(&request, &plan, progress, cancel)
        }));

        window.set_timer(TIMER_PROGRESS, TIMER_INTERVAL);
        self.show_screen(window, Screen::Progress);
    }

    fn tick(&mut self, window: &Window) {
        let Some(worker) = &mut self.worker else {
            return;
        };

        let snapshot = worker.progress().read();
        let stage = if worker.is_cancelling() && !worker.is_finished() {
            "Cancelling...".to_owned()
        } else if snapshot.stage.is_empty() {
            "Starting...".to_owned()
        } else {
            snapshot.stage.clone()
        };
        sys::set_text(self.controls.stage, &stage);

        if let Some(fraction) = snapshot.fraction() {
            sys::set_progress(self.controls.progress, (fraction * 1000.0) as u32);
        }

        let mut stats = match snapshot.total {
            Some(total) => format!(
                "Processed: {} of {}\r\nElapsed: {}",
                format_bytes(snapshot.done),
                format_bytes(total),
                format_duration(snapshot.elapsed_seconds())
            ),
            None => format!(
                "Processed: {}\r\nElapsed: {}",
                format_bytes(snapshot.done),
                format_duration(snapshot.elapsed_seconds())
            ),
        };
        let rate = snapshot.rate();
        if rate > 0.0 {
            stats.push_str(&format!("\r\nSpeed: {}/s", format_bytes(rate as u64)));
        }
        if let Some(remaining) = snapshot.seconds_remaining() {
            stats.push_str(&format!(
                "\r\nTime remaining: about {}",
                format_duration(remaining)
            ));
        }
        sys::set_text(self.controls.stats, &stats);

        if self.show_details && !snapshot.notes.is_empty() {
            sys::set_text(self.controls.details, &snapshot.notes.join("\r\n"));
        }

        if let Some(result) = worker.take_result() {
            window.kill_timer(TIMER_PROGRESS);
            self.worker = None;
            self.finished = Some(result);
            self.show_result(window);
        }
    }

    fn show_result(&mut self, window: &Window) {
        let Some(result) = &self.finished else {
            return;
        };

        match result {
            Ok(outcome) => {
                sys::set_text(self.controls.result_title, "Backup completed and verified");
                let mut body = format!(
                    "Location:\r\n{}\r\n\r\nRead from this PC: {}\r\nWritten to the drive: {}\r\nTook: {}\r\n\r\nVerification: passed. Every stored piece was read back, decompressed and checked against its checksum ({} pieces).",
                    outcome.backup_dir.display(),
                    format_bytes(outcome.captured_bytes),
                    format_bytes(outcome.stored_bytes),
                    format_duration(outcome.elapsed_seconds),
                    outcome.verification.chunks_verified
                );
                for w in &outcome.warnings {
                    body.push_str(&format!("\r\n\r\nNote: {w}"));
                }
                if !outcome.restorable {
                    body.push_str("\r\n\r\nThis backup CANNOT be restored: it is a preview run.");
                }
                body.push_str(
                    "\r\n\r\nTest your recovery before you rely on this backup. A backup that has never been restored is a guess.",
                );
                sys::set_text(self.controls.result_body, &body);
                sys::enable(self.controls.open_folder, true);
            }
            Err(e) => {
                let title = if e.exit() == ExitCode::Cancelled {
                    "Backup cancelled"
                } else {
                    "Backup failed"
                };
                sys::set_text(self.controls.result_title, title);
                sys::set_text(self.controls.result_body, &message_box::format_error(e));
                sys::enable(self.controls.open_folder, false);
            }
        }
        self.show_screen(window, Screen::Result);
    }

    fn toggle_details(&mut self, window: &Window) {
        self.show_details = !self.show_details;
        sys::set_text(
            self.controls.details_toggle,
            if self.show_details {
                "Hide details"
            } else {
                "Show details"
            },
        );
        sys::show(self.controls.details, self.show_details);
        self.layout(window);
        window.invalidate();
    }

    fn browse_for_folder(&mut self, window: &Window) {
        match shell::pick_folder(window.raw(), "Choose where to save the backup") {
            Ok(Some(path)) => {
                sys::set_text(self.controls.dest_edit, &path.to_string_lossy());
                self.update_space_label(window);
            }
            Ok(None) => {}
            Err(e) => message_box::error_for(window.raw(), "MjolnirVSS", &e),
        }
    }

    fn request_cancel(&mut self) {
        if let Some(worker) = &self.worker {
            worker.cancel();
            sys::set_text(self.controls.stage, "Cancelling...");
            sys::set_text(self.controls.cancel, "Cancelling");
            sys::enable(self.controls.cancel, false);
            sys::set_progress_state(self.controls.progress, ProgressState::Paused);
        }
    }
}

impl WindowHandler for BackupWindow {
    fn on_create(&mut self, window: &Window) {
        self.show_screen(window, Screen::Menu);
    }

    fn on_layout(&mut self, window: &Window) {
        self.layout(window);
    }

    fn on_timer(&mut self, window: &Window, id: usize) {
        if id == TIMER_PROGRESS {
            self.tick(window);
        }
    }

    fn on_command(&mut self, window: &Window, id: i32, notification: u32) {
        match (id, notification) {
            (ID_BACKUP, _) => self.begin_backup_setup(window),
            (ID_RESTORE_FILES, _) | (ID_RECOVERY_MEDIA, _) | (ID_MAKE_MEDIA, _) => {
                let what = if id == ID_RESTORE_FILES {
                    "Restoring individual files from a backup"
                } else {
                    "Creating recovery media"
                };
                message_box::info(
                    window.raw(),
                    "MjolnirVSS",
                    &format!(
                        "{what} is not built yet.\n\nThis version takes a verified backup of the system disk. Recovery is done with MjolnirVSS.Restore.exe from Windows installation media; see docs/bare-metal-restore.md."
                    ),
                );
            }
            (ID_SETTINGS, _) => message_box::info(
                window.raw(),
                "MjolnirVSS",
                "There is nothing to configure yet.\n\nMjolnirVSS chooses its own compression and block size, and stores nothing on this computer.",
            ),
            (ID_EXIT, _) | (ID_CLOSE, _) => window.request_close(),
            (ID_DEST_BACK, _) => self.show_screen(window, Screen::Menu),
            (ID_DEST_BROWSE, _) => self.browse_for_folder(window),
            (ID_DEST_EDIT, EN_CHANGE) => self.update_space_label(window),
            (ID_START, _) => self.start_backup(window),
            (ID_CANCEL, _) => self.request_cancel(),
            (ID_DETAILS_TOGGLE, _) => self.toggle_details(window),
            (ID_OPEN_FOLDER, _) => {
                if let Some(Ok(outcome)) = &self.finished {
                    shell::open_in_explorer(&outcome.backup_dir);
                }
            }
            _ => {}
        }
    }

    fn on_close(&mut self, window: &Window) -> bool {
        if self.worker.is_none() {
            return true;
        }
        let confirmed = message_box::confirm(
            window.raw(),
            "MjolnirVSS",
            "A backup is still running.\n\nStop it and close? The partly written backup will not be usable, and will not be marked as one.",
        );
        if confirmed {
            self.request_cancel();
        }
        // Never close while the worker is alive: the shadow copy has to be
        // released first, and that happens when the job unwinds. The window
        // closes itself once the worker reports back.
        false
    }
}

/// The default backup name, `COMPUTERNAME_YYYY-MM-DD_HHMM`.
fn default_name() -> BackupName {
    #[cfg(windows)]
    let computer = mjolnir_storage::system::computer_name();
    #[cfg(not(windows))]
    let computer = "PC".to_owned();
    BackupName::default_for(
        &computer,
        &mjolnir_core::timestamp::UtcTimestamp::now().to_backup_name_stamp(),
    )
}

/// Formats a duration for a person.
fn format_duration(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let secs = total % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

/// Opens the window and runs until it closes.
pub fn run() -> Result<()> {
    mjolnir_win32_ui::window::run(
        WindowConfig {
            class_name: "MjolnirVSSMainWindow",
            title: "MjolnirVSS",
            width: WINDOW_WIDTH,
            height: WINDOW_HEIGHT,
            minimise_box: true,
        },
        BackupWindow::new,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_like_a_person_wrote_them() {
        assert_eq!(format_duration(0.0), "0s");
        assert_eq!(format_duration(45.0), "45s");
        assert_eq!(format_duration(90.0), "1m 30s");
        assert_eq!(format_duration(3661.0), "1h 1m");
        assert_eq!(format_duration(-5.0), "0s");
    }

    #[test]
    fn every_control_id_is_distinct() {
        let ids = [
            ID_BACKUP,
            ID_RESTORE_FILES,
            ID_RECOVERY_MEDIA,
            ID_SETTINGS,
            ID_EXIT,
            ID_SUMMARY,
            ID_DEST_LABEL,
            ID_DEST_EDIT,
            ID_DEST_BROWSE,
            ID_NAME_LABEL,
            ID_NAME_EDIT,
            ID_SPACE,
            ID_START,
            ID_DEST_BACK,
            ID_STAGE,
            ID_PROGRESS,
            ID_STATS,
            ID_CANCEL,
            ID_DETAILS_TOGGLE,
            ID_DETAILS,
            ID_RESULT_TITLE,
            ID_RESULT_BODY,
            ID_OPEN_FOLDER,
            ID_MAKE_MEDIA,
            ID_CLOSE,
        ];
        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "two controls share an identifier");
    }

    #[test]
    fn the_main_menu_has_exactly_the_five_documented_items() {
        let controls = Controls::default();
        assert_eq!(controls.for_screen(Screen::Menu).len(), 5);
    }

    #[test]
    fn the_default_backup_name_is_usable_as_a_folder() {
        let name = default_name();
        assert!(BackupName::new(name.as_str()).is_ok(), "{name}");
    }
}
