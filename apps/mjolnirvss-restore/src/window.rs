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
const ID_PASSWORD_EDIT: i32 = 2011;

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
    /// The box a password is typed into.
    Password,
    /// The button that goes on.
    NextButton,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    FindBackup,
    SelectBackup,
    /// Shown only for an encrypted backup.
    Password,
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
            Step::Password => "Step 2 of 5:  This backup is encrypted",
            Step::SelectTarget => "Step 3 of 5:  Choose the disk to restore onto",
            Step::Review => "Step 4 of 5:  Check this carefully",
            Step::Restoring => "Step 5 of 5:  Restoring",
            Step::Completed => "Finished",
        }
    }

    /// The buttons this step shows.
    ///
    /// The one place that decides, so what is laid out, what is shown and what
    /// the overlap test checks cannot drift apart. Refresh and Back share the
    /// right hand end of the row and are never shown together.
    fn buttons(self) -> &'static [ButtonSlot] {
        match self {
            Step::FindBackup => &[ButtonSlot::Exit, ButtonSlot::Refresh, ButtonSlot::Next],
            Step::SelectBackup | Step::Password | Step::SelectTarget | Step::Review => {
                &[ButtonSlot::Exit, ButtonSlot::Back, ButtonSlot::Next]
            }
            Step::Restoring => &[ButtonSlot::Exit],
            Step::Completed => &[ButtonSlot::Exit, ButtonSlot::Next],
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
            Step::Password => Focus::Password,
            Step::Review => Focus::Confirmation,
            Step::FindBackup | Step::Restoring | Step::Completed => Focus::NextButton,
        }
    }
}

#[cfg(test)]
mod focus_tests {

    /// The text mangled itself once already, through escaping in the wrong
    /// layer, and reached a real screen looking broken. It is built from parts
    /// now, and this checks the parts arrive whole.
    #[test]
    fn the_password_step_text_is_not_mangled() {
        let t = super::password_step_text();
        assert!(t.contains("This backup is encrypted."), "{t}");
        assert!(
            t.contains("MjolnirVSS did not store the password and cannot recover it."),
            "a sentence was broken up: {t}"
        );
        assert!(
            t.contains("getting it wrong here costs nothing."),
            "a sentence was broken up: {t}"
        );
        // No run of spaces: that is what the broken version looked like.
        assert!(
            !t.contains("   "),
            "stray indentation got into the text: {t}"
        );
        // Paragraphs separated by a blank line, not by a bare newline.
        assert_eq!(t.matches(&super::paragraph_break()).count(), 2, "{t}");
    }

    /// An encrypted backup is asked about **before** a disk is chosen, so a
    /// wrong password costs nothing. Asking after the target was picked would
    /// mean asking after it had been erased.
    #[test]
    fn the_password_is_asked_for_before_a_disk_is_chosen() {
        let order = super::ALL_STEPS;
        let password = order.iter().position(|s| *s == Step::Password).unwrap();
        let target = order.iter().position(|s| *s == Step::SelectTarget).unwrap();
        let review = order.iter().position(|s| *s == Step::Review).unwrap();
        assert!(password < target, "the password comes before the disk");
        assert!(password < review, "and well before anything is erased");
    }

    /// The password box, not the erase phrase box: one has to be hidden as it
    /// is typed and the other has to be readable.
    #[test]
    fn the_password_step_focuses_the_password_box() {
        assert_eq!(Step::Password.focus(), Focus::Password);
        assert_ne!(
            Step::Password.focus(),
            Focus::Confirmation,
            "a password must not be typed into the box that shows what is typed"
        );
    }

    /// Going back from the password step has to be possible: somebody may have
    /// chosen the wrong backup.
    #[test]
    fn the_password_step_offers_a_way_back() {
        let buttons = Step::Password.buttons();
        assert!(buttons.contains(&ButtonSlot::Back), "{buttons:?}");
        assert!(buttons.contains(&ButtonSlot::Next), "{buttons:?}");
        assert!(buttons.contains(&ButtonSlot::Exit), "{buttons:?}");
    }

    use super::{button_row, ButtonSlot, Focus, Step};

    /// No two buttons a step shows may share a pixel. One drawn over another
    /// leaves a control that cannot be read but can still be tabbed to and
    /// pressed, which is how the wizard used to go backwards on its own when
    /// somebody tabbed out of the list.
    #[test]
    fn the_buttons_a_step_shows_do_not_overlap() {
        for scale in [1, 2, 3] {
            let s = move |v: i32| v * scale;
            let x = super::MARGIN * scale;
            let inner = (super::WINDOW_WIDTH - super::MARGIN * 2) * scale;
            for step in super::ALL_STEPS {
                let row = button_row(x, inner, &s);
                let mut spans: Vec<(i32, i32)> =
                    step.buttons().iter().map(|b| row.span(*b)).collect();
                spans.sort_by_key(|(left, _)| *left);
                for pair in spans.windows(2) {
                    let (left, width) = pair[0];
                    let (next_left, _) = pair[1];
                    assert!(
                        left + width <= next_left,
                        "{step:?} at scale {scale}: a button ending at {} runs into one starting at {next_left}",
                        left + width
                    );
                }
            }
        }
    }

    /// Every button stays inside the window it is drawn in.
    #[test]
    fn no_button_hangs_off_the_edge() {
        let s = |v: i32| v;
        let x = 16;
        let inner = 600;
        let row = button_row(x, inner, &s);
        for (name, (left, width)) in [
            ("exit", row.exit),
            ("refresh", row.refresh),
            ("back", row.back),
            ("next", row.next),
        ] {
            assert!(left >= x, "{name} starts left of the margin");
            assert!(
                left + width <= x + inner,
                "{name} ends past the right margin"
            );
        }
    }

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
    /// Where a password is typed. Separate from `confirm_edit` because the
    /// erase phrase has to be readable as it is typed and a password must not
    /// be.
    password_edit: HWND,
    progress: HWND,
    stage: HWND,
    refresh: HWND,
    back: HWND,
    next: HWND,
    exit: HWND,
}

impl Controls {
    fn all(&self) -> [HWND; 12] {
        [
            self.title,
            self.body,
            self.list,
            self.confirm_label,
            self.confirm_edit,
            self.password_edit,
            self.progress,
            self.stage,
            self.refresh,
            self.back,
            self.next,
            self.exit,
        ]
    }

    fn for_step(&self, step: Step) -> Vec<HWND> {
        let mut v = vec![self.title, self.body];
        // The buttons come from the step, so a button that is laid out is a
        // button that is shown, and one that is hidden is never in the way.
        for slot in step.buttons() {
            v.push(match slot {
                ButtonSlot::Exit => self.exit,
                ButtonSlot::Refresh => self.refresh,
                ButtonSlot::Back => self.back,
                ButtonSlot::Next => self.next,
            });
        }
        match step {
            Step::Password => {
                v.push(self.confirm_label);
                v.push(self.password_edit);
            }
            Step::SelectBackup | Step::SelectTarget => v.push(self.list),
            Step::Review => {
                v.push(self.confirm_label);
                v.push(self.confirm_edit);
            }
            Step::Restoring => {
                v.push(self.stage);
                v.push(self.progress);
            }
            _ => {}
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

/// A blank line between paragraphs, as the text control wants it.
///
/// Built from character codes rather than written as an escape, because these
/// strings have been mangled once already by escaping in the wrong layer, and
/// the result reached a screen before anybody noticed.
fn paragraph_break() -> String {
    let mut s = String::new();
    for code in [13u8, 10, 13, 10] {
        s.push(char::from(code));
    }
    s
}

/// What the password step says.
fn password_step_text() -> String {
    let gap = paragraph_break();
    let mut t = String::new();
    t.push_str("This backup is encrypted. Nothing can be read out of it without the password.");
    t.push_str(&gap);
    t.push_str(
        "MjolnirVSS did not store the password and cannot recover it. If it has been lost, this backup cannot be used.",
    );
    t.push_str(&gap);
    t.push_str(
        "The password is checked before anything is written, so getting it wrong here costs nothing.",
    );
    t
}

/// Every step, so a test can walk all of them and none is forgotten.
#[cfg(test)]
const ALL_STEPS: [Step; 7] = [
    Step::FindBackup,
    Step::SelectBackup,
    Step::Password,
    Step::SelectTarget,
    Step::Review,
    Step::Restoring,
    Step::Completed,
];

/// One of the four buttons along the bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ButtonSlot {
    Exit,
    Refresh,
    Back,
    Next,
}

/// Where each button in the bottom row goes, as (left, width).
///
/// Pulled out of the layout so it can be checked without a window. Back and
/// Next used to overlap: Next was drawn on top, leaving a sliver of Back
/// showing with no label on it. The sliver was still in the tab order, so a
/// keyboard user tabbing from the list landed on an invisible button and went
/// backwards on pressing Return. Found by driving the wizard in Windows PE.
struct ButtonRow {
    exit: (i32, i32),
    refresh: (i32, i32),
    back: (i32, i32),
    next: (i32, i32),
}

impl ButtonRow {
    fn span(&self, slot: ButtonSlot) -> (i32, i32) {
        match slot {
            ButtonSlot::Exit => self.exit,
            ButtonSlot::Refresh => self.refresh,
            ButtonSlot::Back => self.back,
            ButtonSlot::Next => self.next,
        }
    }
}

fn button_row(x: i32, inner: i32, s: &dyn Fn(i32) -> i32) -> ButtonRow {
    let gap = s(10);
    let next_width = s(180);
    let back_width = s(90);
    ButtonRow {
        exit: (x, s(90)),
        refresh: (x + s(100), s(130)),
        back: (x + inner - next_width - gap - back_width, back_width),
        next: (x + inner - next_width, next_width),
    }
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
        c.password_edit = sys::create_control(h, ControlKind::PasswordBox, "", ID_PASSWORD_EDIT, f);
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
            Step::Password => self.enter_password(),
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
            Focus::Password => self.controls.password_edit,
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

    fn enter_password(&mut self) {
        sys::set_text(self.controls.next, "Unlock");
        sys::set_text(self.controls.body, &password_step_text());
        sys::set_text(self.controls.confirm_label, "Password:");
        sys::set_text(self.controls.password_edit, "");
        // Nothing to unlock with until something is typed.
        sys::enable(self.controls.next, false);
    }

    /// Tries the typed password, and stays on this step if it is wrong.
    fn try_unlock(&mut self, window: &Window) {
        let Some(index) = self.chosen_backup else {
            return;
        };
        let typed = self.read_text(window, self.controls.password_edit);
        if typed.is_empty() {
            return;
        }

        match self.found[index].set.unlock(&typed) {
            Ok(()) => {
                // The typed password is wiped from the box as soon as it has
                // been used: there is no reason for it to sit on screen, or in
                // a control, for the rest of the restore.
                sys::set_text(self.controls.password_edit, "");
                self.show_step(window, Step::SelectTarget);
            }
            Err(e) => {
                message_box::error_for(window.raw(), "MjolnirVSS Recovery", &e);
                sys::set_text(self.controls.password_edit, "");
                sys::enable(self.controls.next, false);
                window.focus(self.controls.password_edit);
            }
        }
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
        // The set is taken as it stands, unlocked if a password was given.
        // Re-opening it from its path inside the worker would throw the
        // password away, and an encrypted restore would then fail partway
        // through writing the disk, which is the worst possible moment.
        let backup_set = self.found[backup_index].set.clone();
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
            let set = backup_set;
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
                // An encrypted backup needs a password before anything can be
                // read out of it, and asking now means a wrong one costs
                // nothing. Asking later would mean asking after the target disk
                // had already been erased.
                if self.found[index].set.is_encrypted() && !self.found[index].set.is_unlocked() {
                    self.show_step(window, Step::Password);
                    return;
                }
                self.show_step(window, Step::SelectTarget);
            }
            Step::Password => self.try_unlock(window),
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
            Step::Password => {
                let block = s(LINE) + s(30) + s(10);
                let body_height = (bottom - y - block - s(20)).max(s(80));
                sys::place(c.body, sys::rect(x, y, inner, body_height));
                y += body_height + s(10);
                sys::place(c.confirm_label, sys::rect(x, y, inner, s(LINE)));
                y += s(LINE) + s(4);
                sys::place(c.password_edit, sys::rect(x, y, s(300), s(26)));
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

        // Placed through the same spans the overlap test checks, so what is
        // tested is what is drawn.
        let row = button_row(x, inner, &s);
        for (slot, hwnd) in [
            (ButtonSlot::Exit, c.exit),
            (ButtonSlot::Refresh, c.refresh),
            (ButtonSlot::Back, c.back),
            (ButtonSlot::Next, c.next),
        ] {
            let (left, width) = row.span(slot);
            sys::place(hwnd, sys::rect(left, bottom, width, s(BUTTON_HEIGHT)));
        }
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
        // Enter means Next, wherever the keyboard happens to be.
        window.set_default_button(ID_NEXT);
        self.show_step(window, Step::FindBackup);
    }

    fn on_layout(&mut self, window: &Window) {
        self.layout(window);
    }

    fn on_activate(&mut self, window: &Window) {
        // Where the keyboard goes is decided by the step, not by the order the
        // controls were created in.
        self.take_focus(window, self.step);
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
            (ID_PASSWORD_EDIT, EN_CHANGE) => {
                // Something typed is enough to try; whether it is right is the
                // backup's answer to give, not this window's guess.
                let typed = self.read_text(window, self.controls.password_edit);
                sys::enable(self.controls.next, !typed.is_empty());
            }
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
        for step in ALL_STEPS {
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
            ID_PASSWORD_EDIT,
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
