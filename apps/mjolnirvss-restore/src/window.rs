//! The recovery wizard.
//!
//! Six steps, in the order a person in trouble would think of them: find a
//! backup, choose one, choose the disk to restore onto, look at what is about
//! to be destroyed, type the disk's serial number, and watch it happen.
//!
//! Nothing is ever preselected on the target screen. The Restore button stays
//! disabled until the operator has typed the exact erase phrase for the disk
//! they picked, and that phrase contains the disk's serial number, so it cannot
//! be produced by pressing Enter twice.

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::progress::format_bytes;
use mjolnir_restore::{EraseConfirmation, RestoreOutcome, RestorePlan, TargetDisk};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::HFONT;

use mjolnir_win32_ui::message_box;
use mjolnir_win32_ui::sys::{self, ControlKind, ProgressState};
use mjolnir_win32_ui::window::{Window, WindowConfig, WindowHandler};
use mjolnir_win32_ui::worker::Worker;

use crate::discover::{self, FoundBackup};

/// Notification a list sends when the selection changes.
const LBN_SELCHANGE: u32 = 1;
/// Notification a text box sends when its contents change.
const EN_CHANGE: u32 = 0x0300;

const WINDOW_WIDTH: i32 = 720;
const WINDOW_HEIGHT: i32 = 520;
const MARGIN: i32 = 18;
const BUTTON_HEIGHT: i32 = 32;
const LINE: i32 = 20;

const ID_TITLE: i32 = 2000;
const ID_BODY: i32 = 2001;
const ID_LIST: i32 = 2002;
const ID_BACK: i32 = 2003;
const ID_NEXT: i32 = 2004;
const ID_CONFIRM_LABEL: i32 = 2005;
const ID_CONFIRM_EDIT: i32 = 2006;
const ID_PROGRESS: i32 = 2007;
const ID_STAGE: i32 = 2008;
const ID_REFRESH: i32 = 2009;
const ID_EXIT: i32 = 2010;

const TIMER_PROGRESS: usize = 1;
const TIMER_INTERVAL: u32 = 200;

/// Where a step puts the keyboard.
///
/// A small type rather than a control handle on purpose: the read only body is
/// not one of these, so no step can hand it the keyboard by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// The list, where there is something to choose.
    List,
    /// The box the erase phrase is typed into.
    Confirmation,
    /// The button that goes on.
    NextButton,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    FindBackup,
    SelectBackup,
    SelectTarget,
    Review,
    Restoring,
    Completed,
}

impl Step {
    fn title(self) -> &'static str {
        match self {
            Step::FindBackup => "Step 1 of 5:  Find a backup",
            Step::SelectBackup => "Step 2 of 5:  Choose the backup to restore",
            Step::SelectTarget => "Step 3 of 5:  Choose the disk to restore onto",
            Step::Review => "Step 4 of 5:  Check this carefully",
            Step::Restoring => "Step 5 of 5:  Restoring",
            Step::Completed => "Finished",
        }
    }

    /// Where the keyboard goes when this step appears.
    ///
    /// Every step names a control the operator acts on. Leaving it to the tab
    /// order put the keyboard in the read only body, and a multiline edit tells
    /// Windows it wants the Return key, so Return did nothing at all: the
    /// window looked operable and would not advance. That was found by running
    /// this in Windows PE, where there is no mouse to reach for.
    fn focus(self) -> Focus {
        match self {
            Step::SelectBackup | Step::SelectTarget => Focus::List,
            Step::Review => Focus::Confirmation,
            Step::FindBackup | Step::Restoring | Step::Completed => Focus::NextButton,
        }
    }
}

#[cfg(test)]
mod focus_tests {
    use super::{Focus, Step};

    /// Every step a person has to act on hands the keyboard to the thing they
    /// act on, and a step showing a list hands it to the list.
    #[test]
    fn every_step_puts_the_keyboard_somewhere_it_can_be_used() {
        for (step, expected) in [
            (Step::FindBackup, Focus::NextButton),
            (Step::SelectBackup, Focus::List),
            (Step::SelectTarget, Focus::List),
            (Step::Review, Focus::Confirmation),
            (Step::Restoring, Focus::NextButton),
            (Step::Completed, Focus::NextButton),
        ] {
            assert_eq!(step.focus(), expected, "{step:?}");
        }
    }

    /// The step that erases a disk is the one that must be operable without a
    /// mouse, because it is the one somebody reaches in a recovery environment.
    #[test]
    fn the_step_that_erases_a_disk_focuses_the_phrase_that_permits_it() {
        assert_eq!(Step::Review.focus(), Focus::Confirmation);
    }
}

#[derive(Default)]
struct Controls {
    title: HWND,
    body: HWND,
    list: HWND,
    confirm_label: HWND,
    confirm_edit: HWND,
    progress: HWND,
    stage: HWND,
    refresh: HWND,
    back: HWND,
    next: HWND,
    exit: HWND,
}

impl Controls {
    fn all(&self) -> [HWND; 11] {
        [
            self.title,
            self.body,
            self.list,
            self.confirm_label,
            self.confirm_edit,
            self.progress,
            self.stage,
            self.refresh,
            self.back,
            self.next,
            self.exit,
        ]
    }

    fn for_step(&self, step: Step) -> Vec<HWND> {
        let mut v = vec![self.title, self.body, self.exit];
        match step {
            Step::FindBackup => {
                v.push(self.refresh);
                v.push(self.next);
            }
            Step::SelectBackup | Step::SelectTarget => {
                v.push(self.list);
                v.push(self.back);
                v.push(self.next);
            }
            Step::Review => {
                v.push(self.confirm_label);
                v.push(self.confirm_edit);
                v.push(self.back);
                v.push(self.next);
            }
            Step::Restoring => {
                v.push(self.stage);
                v.push(self.progress);
            }
            Step::Completed => {
                v.push(self.next);
            }
        }
        v
    }
}

pub struct RecoveryWindow {
    font: HFONT,
    step: Step,
    controls: Controls,
    found: Vec<FoundBackup>,
    chosen_backup: Option<usize>,
    targets: Vec<TargetDisk>,
    chosen_target: Option<usize>,
    plan: Option<RestorePlan>,
    worker: Option<Worker<RestoreOutcome>>,
    finished: Option<std::result::Result<RestoreOutcome, Error>>,
}

impl RecoveryWindow {
    pub fn new(window: &Window) -> Self {
        let dpi = sys::dpi_of(window.raw());
        let mut me = Self {
            font: sys::ui_font(dpi),
            step: Step::FindBackup,
            controls: Controls::default(),
            found: Vec::new(),
            chosen_backup: None,
            targets: Vec::new(),
            chosen_target: None,
            plan: None,
            worker: None,
            finished: None,
        };
        me.create_controls(window);
        me
    }

    fn create_controls(&mut self, window: &Window) {
        let f = self.font;
        let h = window.raw();
        let c = &mut self.controls;

        c.title = sys::create_control(h, ControlKind::Label, "", ID_TITLE, f);
        c.body = sys::create_control(h, ControlKind::TextArea, "", ID_BODY, f);
        c.list = sys::create_control(h, ControlKind::ListBox, "", ID_LIST, f);
        c.confirm_label = sys::create_control(h, ControlKind::Label, "", ID_CONFIRM_LABEL, f);
        c.confirm_edit = sys::create_control(h, ControlKind::TextBox, "", ID_CONFIRM_EDIT, f);
        c.progress = sys::create_control(h, ControlKind::ProgressBar, "", ID_PROGRESS, f);
        c.stage = sys::create_control(h, ControlKind::Label, "", ID_STAGE, f);
        c.refresh = sys::create_control(h, ControlKind::Button, "Search again", ID_REFRESH, f);
        c.back = sys::create_control(h, ControlKind::Button, "Back", ID_BACK, f);
        c.next = sys::create_control(h, ControlKind::DefaultButton, "Next", ID_NEXT, f);
        c.exit = sys::create_control(h, ControlKind::Button, "Exit", ID_EXIT, f);
    }

    fn show_step(&mut self, window: &Window, step: Step) {
        self.step = step;
        sys::set_text(self.controls.title, step.title());

        let visible = self.controls.for_step(step);
        for hwnd in self.controls.all() {
            sys::show(hwnd, visible.contains(&hwnd));
        }

        match step {
            Step::FindBackup => self.enter_find(),
            Step::SelectBackup => self.enter_select_backup(),
            Step::SelectTarget => self.enter_select_target(),
            Step::Review => self.enter_review(window),
            Step::Restoring => {}
            Step::Completed => self.enter_completed(),
        }

        self.layout(window);
        self.invalidate(window);
        self.take_focus(window, step);
    }

    /// Puts the keyboard where the operator's next action is.
    ///
    /// Without this the first control in the tab order takes focus, which is
    /// the read only body. A multiline edit control tells Windows it wants the
    /// Return key, so Return would land there and do nothing: the window would
    /// look operable and refuse to advance. Found by running this in Windows
    /// PE, where there is no mouse to fall back on.
    fn take_focus(&mut self, window: &Window, step: Step) {
        let target = match step.focus() {
            Focus::List => self.controls.list,
            Focus::Confirmation => self.controls.confirm_edit,
            Focus::NextButton => self.controls.next,
        };
        window.focus(target);
    }

    fn enter_find(&mut self) {
        sys::set_text(self.controls.next, "Search for backups");
        sys::set_text(
            self.controls.body,
            "This will restore a whole Windows disk from a MjolnirVSS backup.\r\n\r\n\
             Everything on the disk you choose in step 3 will be erased.\r\n\r\n\
             Make sure the drive holding the backup is connected, then press \
             Search for backups. Every attached drive is searched; nothing is \
             changed.",
        );
    }

    pub fn search(&mut self, window: &Window) {
        sys::set_text(self.controls.body, "Searching the attached drives...");
        self.invalidate(window);

        self.found = discover::search_all_drives();
        self.chosen_backup = None;

        if self.found.is_empty() {
            sys::set_text(
                self.controls.body,
                "No MjolnirVSS backups were found on the drives attached to this \
                 computer.\r\n\r\n\
                 Check that the drive holding the backup is plugged in, then press \
                 Search for backups again.\r\n\r\n\
                 If the drive is connected but nothing is listed, the backup folder may be \
                 deeper than three folders from the top of the drive. Move it nearer the \
                 top of the drive and search again.",
            );
            return;
        }
        self.show_step(window, Step::SelectBackup);
    }

    fn enter_select_backup(&mut self) {
        sys::set_text(self.controls.next, "Next");
        sys::list_clear(self.controls.list);
        for f in &self.found {
            sys::list_add(self.controls.list, &f.describe());
        }
        sys::list_select(self.controls.list, None);
        sys::set_text(
            self.controls.body,
            "Choose the backup to restore. A backup marked INCOMPLETE was interrupted \
             when it was taken and cannot be used.",
        );
        sys::enable(self.controls.next, false);
    }

    fn enter_select_target(&mut self) {
        sys::set_text(self.controls.next, "Next");
        let backup_disks = self
            .chosen_backup
            .and_then(|i| self.found.get(i))
            .map(|f| f.stored_on_disks())
            .unwrap_or_default();

        self.targets = mjolnir_restore::enumerate_targets(&backup_disks);
        self.chosen_target = None;

        sys::list_clear(self.controls.list);
        for t in &self.targets {
            let marker = if t.holds_the_backup {
                "  <-- HOLDS THE BACKUP, cannot be used"
            } else {
                ""
            };
            sys::list_add(self.controls.list, &format!("{}{marker}", t.describe()));
        }
        // Nothing is selected. Choosing the disk to destroy is the operator's
        // decision, and a preselected row invites pressing Next without reading.
        sys::list_select(self.controls.list, None);
        sys::enable(self.controls.next, false);

        sys::set_text(
            self.controls.body,
            "Choose the disk to restore onto. EVERYTHING ON IT WILL BE ERASED.\r\n\r\n\
             This is normally the new, blank disk you have just fitted. The drive holding \
             the backup is marked and cannot be chosen.",
        );
    }

    fn enter_review(&mut self, _window: &Window) {
        sys::set_text(self.controls.next, "Restore");
        sys::enable(self.controls.next, false);
        sys::set_text(self.controls.confirm_edit, "");

        let Some(backup) = self.chosen_backup.and_then(|i| self.found.get(i)) else {
            return;
        };
        let Some(target) = self.chosen_target.and_then(|i| self.targets.get(i)) else {
            return;
        };

        match mjolnir_restore::plan(&backup.set, target) {
            Ok(plan) => {
                let mut text = String::new();
                text.push_str("WILL BE RESTORED\r\n");
                for line in plan.summary_lines() {
                    text.push_str(&format!("  {line}\r\n"));
                }
                text.push_str("\r\nONTO THIS DISK, ERASING IT COMPLETELY\r\n");
                text.push_str(&format!("  {}\r\n", target.describe()));
                if target.existing_partitions.is_empty() {
                    text.push_str("  The disk has no partitions; it is blank.\r\n");
                } else {
                    text.push_str("  These will be destroyed:\r\n");
                    for p in &target.existing_partitions {
                        text.push_str(&format!("    {p}\r\n"));
                    }
                }
                for w in &plan.warnings {
                    text.push_str(&format!("\r\nNote: {w}\r\n"));
                }
                sys::set_text(self.controls.body, &text);
                sys::set_text(
                    self.controls.confirm_label,
                    &format!("To continue, type:   {}", target.erase_phrase()),
                );
                self.plan = Some(plan);
            }
            Err(e) => {
                sys::set_text(self.controls.body, &message_box::format_error(&e));
                sys::set_text(self.controls.confirm_label, "This disk cannot be used.");
                self.plan = None;
            }
        }
    }

    /// Enables Restore only when the typed phrase is exactly right.
    fn check_confirmation(&mut self, window: &Window) {
        let typed = self.read_text(window, self.controls.confirm_edit);
        let ready = self
            .chosen_target
            .and_then(|i| self.targets.get(i))
            .map(|t| EraseConfirmation::check(t, &typed).is_ok())
            .unwrap_or(false)
            && self.plan.is_some();
        sys::enable(self.controls.next, ready);
    }

    fn start_restore(&mut self, window: &Window) {
        let (Some(backup_index), Some(target_index), Some(plan)) =
            (self.chosen_backup, self.chosen_target, self.plan.clone())
        else {
            return;
        };
        let backup_path = self.found[backup_index].path.clone();
        let target = self.targets[target_index].clone();
        let typed = self.read_text(window, self.controls.confirm_edit);

        let confirmation = match EraseConfirmation::check(&target, &typed) {
            Ok(c) => c,
            Err(e) => {
                message_box::error_for(window.raw(), "MjolnirVSS Recovery", &e);
                return;
            }
        };

        // The last chance to stop, in the operator's own words.
        if !message_box::confirm(
            window.raw(),
            "MjolnirVSS Recovery",
            &format!(
                "This will erase {} completely and cannot be undone.\n\nContinue?",
                target.describe()
            ),
        ) {
            return;
        }

        sys::set_text(self.controls.stage, "Starting...");
        sys::set_progress(self.controls.progress, 0);
        sys::set_progress_state(self.controls.progress, ProgressState::Normal);

        let worker_target = target.clone();
        self.worker = Some(Worker::start(move |progress, cancel| {
            let set = mjolnir_image::BackupSet::open(&backup_path)?;
            let mut disk = mjolnir_restore::WritableDisk::open(&worker_target)?;
            let mut outcome = mjolnir_restore::restore(
                &set,
                &plan,
                &worker_target,
                &confirmation,
                &mut disk,
                progress,
                cancel,
            )?;
            disk.refresh_partition_table()?;

            // Closed before the repair, because Windows will not show the new
            // partitions while the disk is open for writing.
            drop(disk);

            progress.begin(mjolnir_restore::stages::REPAIRING_BOOT, None);
            outcome.boot_repair = mjolnir_restore::repair_disk(worker_target.number).ok();
            progress.end();

            Ok(outcome)
        }));

        window.set_timer(TIMER_PROGRESS, TIMER_INTERVAL);
        self.show_step(window, Step::Restoring);
    }

    fn tick(&mut self, window: &Window) {
        let Some(worker) = &mut self.worker else {
            return;
        };
        let snapshot = worker.progress().read();

        sys::set_text(
            self.controls.stage,
            if snapshot.stage.is_empty() {
                "Starting..."
            } else {
                &snapshot.stage
            },
        );
        if let Some(fraction) = snapshot.fraction() {
            sys::set_progress(self.controls.progress, (fraction * 1000.0) as u32);
        }

        let mut text = format!(
            "{}\r\n\r\nWritten: {}",
            snapshot.stage,
            format_bytes(snapshot.done)
        );
        if let Some(total) = snapshot.total {
            text.push_str(&format!(" of {}", format_bytes(total)));
        }
        if let Some(remaining) = snapshot.seconds_remaining() {
            text.push_str(&format!("\r\nTime remaining: about {remaining:.0} seconds"));
        }
        text.push_str("\r\n\r\nDo not turn the computer off.");
        sys::set_text(self.controls.body, &text);

        if let Some(result) = worker.take_result() {
            window.kill_timer(TIMER_PROGRESS);
            self.worker = None;
            self.finished = Some(result);
            self.show_step(window, Step::Completed);
        }
    }

    fn enter_completed(&mut self) {
        sys::set_text(self.controls.next, "Exit");
        match &self.finished {
            Some(Ok(outcome)) => {
                sys::set_text(
                    self.controls.body,
                    &format!(
                        "The restore finished.\r\n\r\n\
                         Written: {}\r\n\
                         Partitions restored: {}\r\n\
                         Left unallocated at the end of the disk: {}\r\n\r\n\
                         Next: close this, remove the recovery media and restart the computer.\r\n\r\n\
                         If Windows does not start, boot the recovery media again and run \
                         Startup Repair. The data is on the disk; only the boot configuration \
                         would need fixing.",
                        format_bytes(outcome.written_bytes),
                        outcome.partitions_restored,
                        format_bytes(outcome.unallocated_bytes)
                    ),
                );
            }
            Some(Err(e)) => {
                sys::set_text(
                    self.controls.title,
                    if e.exit() == ExitCode::Cancelled {
                        "Restore cancelled"
                    } else {
                        "Restore failed"
                    },
                );
                sys::set_text(self.controls.body, &message_box::format_error(e));
            }
            None => {}
        }
    }

    fn on_next(&mut self, window: &Window) {
        match self.step {
            Step::FindBackup => self.search(window),
            Step::SelectBackup => {
                let Some(index) = self.chosen_backup else {
                    return;
                };
                if !self.found[index].set.is_restorable() {
                    message_box::warn(
                        window.raw(),
                        "MjolnirVSS Recovery",
                        "That backup cannot be restored. It was interrupted when it was taken, \
                         or it did not pass its check.\n\nChoose a different one.",
                    );
                    return;
                }
                self.show_step(window, Step::SelectTarget);
            }
            Step::SelectTarget => {
                let Some(index) = self.chosen_target else {
                    return;
                };
                if self.targets[index].holds_the_backup {
                    message_box::warn(
                        window.raw(),
                        "MjolnirVSS Recovery",
                        "That disk holds the backup you are restoring from.\n\nErasing it would \
                         destroy the backup partway through. Choose the replacement disk instead.",
                    );
                    return;
                }
                self.show_step(window, Step::Review);
            }
            Step::Review => self.start_restore(window),
            Step::Restoring => {}
            Step::Completed => window.request_close(),
        }
    }

    fn on_back(&mut self, window: &Window) {
        match self.step {
            Step::SelectBackup => self.show_step(window, Step::FindBackup),
            Step::SelectTarget => self.show_step(window, Step::SelectBackup),
            Step::Review => self.show_step(window, Step::SelectTarget),
            _ => {}
        }
    }

    fn on_list_changed(&mut self) {
        let selected = sys::list_selected(self.controls.list);
        match self.step {
            Step::SelectBackup => {
                self.chosen_backup = selected;
                sys::enable(self.controls.next, selected.is_some());
            }
            Step::SelectTarget => {
                self.chosen_target = selected;
                let usable = selected
                    .and_then(|i| self.targets.get(i))
                    .map(|t| !t.holds_the_backup)
                    .unwrap_or(false);
                sys::enable(self.controls.next, usable);
            }
            _ => {}
        }
    }

    fn read_text(&self, window: &Window, hwnd: HWND) -> String {
        window.text_of(hwnd)
    }

    fn invalidate(&self, window: &Window) {
        window.invalidate();
    }

    fn layout(&self, window: &Window) {
        let dpi = sys::dpi_of(window.raw());
        let s = |v: i32| sys::scale(v, dpi);
        let client: RECT = window.client_rect();

        let x = s(MARGIN);
        let inner = (client.right - client.left) - x * 2;
        let bottom = client.bottom - s(MARGIN) - s(BUTTON_HEIGHT);
        let c = &self.controls;

        let mut y = s(MARGIN);
        sys::place(c.title, sys::rect(x, y, inner, s(LINE) + s(6)));
        y += s(LINE) + s(16);

        match self.step {
            Step::SelectBackup | Step::SelectTarget => {
                let body_height = s(LINE) * 3;
                sys::place(c.body, sys::rect(x, y, inner, body_height));
                y += body_height + s(10);
                let list_height = (bottom - y - s(12)).max(s(80));
                sys::place(c.list, sys::rect(x, y, inner, list_height));
            }
            Step::Review => {
                let confirm_block = s(LINE) + s(30) + s(10);
                let body_height = (bottom - y - confirm_block - s(20)).max(s(80));
                sys::place(c.body, sys::rect(x, y, inner, body_height));
                y += body_height + s(10);
                sys::place(c.confirm_label, sys::rect(x, y, inner, s(LINE)));
                y += s(LINE) + s(4);
                sys::place(c.confirm_edit, sys::rect(x, y, s(300), s(26)));
            }
            Step::Restoring => {
                sys::place(c.stage, sys::rect(x, y, inner, s(LINE) + s(4)));
                y += s(LINE) + s(10);
                sys::place(c.progress, sys::rect(x, y, inner, s(24)));
                y += s(24) + s(12);
                let body_height = (client.bottom - s(MARGIN) - y).max(s(60));
                sys::place(c.body, sys::rect(x, y, inner, body_height));
            }
            _ => {
                let body_height = (bottom - y - s(12)).max(s(80));
                sys::place(c.body, sys::rect(x, y, inner, body_height));
            }
        }

        sys::place(c.exit, sys::rect(x, bottom, s(90), s(BUTTON_HEIGHT)));
        sys::place(
            c.refresh,
            sys::rect(x + s(100), bottom, s(130), s(BUTTON_HEIGHT)),
        );
        sys::place(
            c.back,
            sys::rect(x + inner - s(200), bottom, s(90), s(BUTTON_HEIGHT)),
        );
        sys::place(
            c.next,
            sys::rect(x + inner - s(180), bottom, s(180), s(BUTTON_HEIGHT)),
        );
    }

    /// Whether the window may close right now.
    fn may_close(&mut self, window: &Window) -> bool {
        if self.worker.is_none() {
            return true;
        }
        // Stopping a restore leaves a half written disk. Saying so is the whole
        // point of asking.
        let confirmed = message_box::confirm(
            window.raw(),
            "MjolnirVSS Recovery",
            "A restore is in progress.\n\nStopping now leaves the disk partly written and \
             unable to start Windows. You would have to run the restore again from the \
             beginning.\n\nStop anyway?",
        );
        if confirmed {
            if let Some(worker) = &self.worker {
                worker.cancel();
            }
        }
        false
    }
}

impl WindowHandler for RecoveryWindow {
    fn on_create(&mut self, window: &Window) {
        self.show_step(window, Step::FindBackup);
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
            (ID_NEXT, _) => self.on_next(window),
            (ID_BACK, _) => self.on_back(window),
            (ID_REFRESH, _) => self.search(window),
            (ID_EXIT, _) => window.request_close(),
            (ID_LIST, LBN_SELCHANGE) => self.on_list_changed(),
            (ID_CONFIRM_EDIT, EN_CHANGE) => self.check_confirmation(window),
            _ => {}
        }
    }

    fn on_close(&mut self, window: &Window) -> bool {
        self.may_close(window)
    }
}

/// Opens the recovery wizard and runs until it closes.
pub fn run() -> Result<()> {
    mjolnir_win32_ui::window::run(
        WindowConfig {
            class_name: "MjolnirVSSRecoveryWindow",
            title: "MjolnirVSS Recovery",
            width: WINDOW_WIDTH,
            height: WINDOW_HEIGHT,
            // No minimise box: inside a recovery environment there is nothing
            // to minimise to.
            minimise_box: false,
        },
        RecoveryWindow::new,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_step_has_a_title_that_says_where_the_operator_is() {
        for step in [
            Step::FindBackup,
            Step::SelectBackup,
            Step::SelectTarget,
            Step::Review,
            Step::Restoring,
            Step::Completed,
        ] {
            assert!(!step.title().is_empty());
        }
        assert!(Step::FindBackup.title().starts_with("Step 1 of 5"));
        assert!(Step::Restoring.title().starts_with("Step 5 of 5"));
    }

    #[test]
    fn every_control_id_is_distinct() {
        let ids = [
            ID_TITLE,
            ID_BODY,
            ID_LIST,
            ID_BACK,
            ID_NEXT,
            ID_CONFIRM_LABEL,
            ID_CONFIRM_EDIT,
            ID_PROGRESS,
            ID_STAGE,
            ID_REFRESH,
            ID_EXIT,
        ];
        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len());
    }

    #[test]
    fn the_restoring_step_offers_no_way_to_press_on_by_accident() {
        let controls = Controls::default();
        let visible = controls.for_step(Step::Restoring);
        // No Next, no Back, no Exit button while a disk is being written.
        assert!(!visible.contains(&controls.next) || controls.next.is_invalid());
        assert!(!visible.contains(&controls.back) || controls.back.is_invalid());
    }
}
