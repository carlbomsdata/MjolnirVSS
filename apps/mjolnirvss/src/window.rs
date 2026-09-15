//! The MjolnirVSS window.
//!
//! A navigation rail down the left and a content pane on the right, in the shape
//! Windows' own settings applications use:
//!
//! * **Back up this PC**, which shows the disk, what is on it and where to put
//!   the copy, and then runs and reports;
//! * **Restore files**, for getting single files back out of a backup;
//! * **Recovery media**, for building the thing the machine is started from when
//!   it will not start by itself;
//! * **Settings**, which says there is nothing to set.
//!
//! Nothing here knows how a backup is taken, and nothing here talks to Windows
//! directly: it implements [`WindowHandler`], starts the engine on a worker
//! thread, reads a shared progress record on a timer, and paints. That is the
//! whole design, and it is why the window never stops responding and why this
//! file contains no `unsafe`.
//!
//! # Where the pixels come from
//!
//! Every piece of text and every control a person operates is a real Win32
//! control, so the keyboard, the focus rectangles, the high contrast theme and
//! what a screen reader says all come from Windows rather than from here. What
//! this file paints is the surfaces underneath: the rail, the cards, the
//! dividers, the small pictures and the bar showing a disk's partitions.

use std::path::PathBuf;

use mjolnir_backup::{BackupOutcome, BackupPlan, BackupRequest, CaptureLimit};
use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use mjolnir_core::ids::BackupName;
use mjolnir_core::progress::format_bytes;
use mjolnir_image::PartitionRole;
use mjolnir_media::MediaOutcome;
use windows::Win32::Foundation::{COLORREF, HWND, RECT};

use mjolnir_win32_ui::paint::{Canvas, Glyph, ItemStyle};
use mjolnir_win32_ui::sys::{self, ControlKind, ProgressState};
use mjolnir_win32_ui::theme::{self, Metrics, Palette, TextStyle};
use mjolnir_win32_ui::window::{
    set_item_style, set_label_colours, Window, WindowConfig, WindowHandler,
};
use mjolnir_win32_ui::worker::Worker;
use mjolnir_win32_ui::{message_box, shell};

/// Width of the window at 96 dpi.
///
/// Wide enough for the rail and a content pane that still fits a four partition
/// disk, a full path and a row of statistics without anything scrolling.
const WINDOW_WIDTH: i32 = 940;
/// Height of the window at 96 dpi.
///
/// Chosen to leave the taskbar alone on a 768 pixel screen, which is what a
/// virtual machine and an older laptop both have.
const WINDOW_HEIGHT: i32 = 680;

// Control identifiers. Stable numbers so a command can be matched on them.
const ID_NAV_BACKUP: i32 = 1001;
const ID_NAV_RESTORE: i32 = 1002;
const ID_NAV_MEDIA: i32 = 1003;
const ID_NAV_SETTINGS: i32 = 1004;

const ID_BRAND: i32 = 1010;
const ID_BRAND_VERSION: i32 = 1011;
const ID_RAIL_FOOTER: i32 = 1012;
const ID_PAGE_TITLE: i32 = 1013;
const ID_PAGE_SUBTITLE: i32 = 1014;

const ID_DISK_OVERLINE: i32 = 1100;
const ID_DISK_NAME: i32 = 1101;
const ID_DISK_META: i32 = 1102;
const ID_DATA_OVERLINE: i32 = 1103;
const ID_DATA_FIGURE: i32 = 1104;
const ID_PART_LEGEND: i32 = 1105;
const ID_NOTES: i32 = 1106;
const ID_DEST_LABEL: i32 = 1107;
const ID_DEST_EDIT: i32 = 1108;
const ID_DEST_BROWSE: i32 = 1109;
const ID_NAME_LABEL: i32 = 1110;
const ID_NAME_EDIT: i32 = 1111;
const ID_SPACE: i32 = 1112;
const ID_START: i32 = 1113;
const ID_REASSURANCE: i32 = 1114;

const ID_PROG_HEADING: i32 = 1200;
const ID_PROG_STAGE: i32 = 1201;
const ID_PROG_PERCENT: i32 = 1202;
const ID_PROGRESS: i32 = 1203;
const ID_CANCEL: i32 = 1204;
const ID_DETAILS_TOGGLE: i32 = 1205;
const ID_DETAILS: i32 = 1206;
const ID_STAT_KEY: i32 = 1210;
const ID_STAT_VALUE: i32 = 1220;
const ID_STAGE_ROW: i32 = 1230;

const ID_RESULT_TITLE: i32 = 1300;
const ID_RESULT_NOTE: i32 = 1301;
const ID_RESULT_BODY: i32 = 1302;
const ID_OPEN_FOLDER: i32 = 1303;
const ID_MAKE_MEDIA: i32 = 1304;
const ID_CLOSE: i32 = 1305;

const ID_BROWSE_VOLUME: i32 = 1400;
const ID_BROWSE_PATH: i32 = 1401;
const ID_BROWSE_LIST: i32 = 1402;
const ID_BROWSE_OPEN: i32 = 1403;
const ID_BROWSE_UP: i32 = 1404;
const ID_BROWSE_EXTRACT: i32 = 1405;
const ID_BROWSE_BACK: i32 = 1406;

const ID_MEDIA_BODY: i32 = 1500;
const ID_MEDIA_REQUIREMENT: i32 = 1501;
const ID_SETTINGS_BODY: i32 = 1502;

/// How many statistics the progress screen shows.
const STATS: usize = 4;
/// How many stages the progress screen lists.
const STAGES: usize = 4;

/// Notification a list box sends when an item is double clicked.
const LBN_DBLCLK: u32 = 2;
/// Notification code a text box sends when its contents change.
const EN_CHANGE: u32 = 0x0300;

/// Timer that polls the worker for progress.
const TIMER_PROGRESS: usize = 1;
/// How often to repaint progress, in milliseconds.
const TIMER_INTERVAL: u32 = 200;

/// What the Recovery media section explains before anything is built.
///
/// Two sentences and a requirement. The reasoning behind it is in the
/// documentation, which is where somebody who wants it will look; a person
/// standing at a working computer wanting rescue media needs to know what the
/// button does and what it needs.
const MEDIA_BODY: &str = "Recovery media is what you start the computer from when its disk has failed. \
MjolnirVSS builds it from the Windows recovery files already on this computer, so nothing belonging to \
Microsoft is downloaded or redistributed.\r\n\r\n\
The result is an ISO image of about 300 MB. Write it to a USB drive with any tool that writes a bootable image.\r\n\r\n\
Build it now, while this computer still works. A computer that will not start cannot build its own rescue media.";

/// What the Settings section says.
///
/// It says there is nothing to set, because there is nothing to set: the
/// compression, the block size and the digest are not choices a person should
/// have to make, and MjolnirVSS stores nothing on this computer to configure.
/// A sparse page that says so is better than invented switches.
const SETTINGS_BODY: &str = "MjolnirVSS has nothing to configure, on purpose.\r\n\r\n\
Compression and block size are chosen for the disk being read, rather than being a decision to get wrong. \
Nothing is stored on this computer: no service, no scheduled task, no registry entries and no settings file. \
The only thing MjolnirVSS writes is the backup folder you choose.\r\n\r\n\
Encryption is chosen per backup rather than once and forgotten, and is set from the command line with --encrypt.";

/// The line along the bottom of the navigation rail.
///
/// The version is already under the name at the top, so this says the one thing
/// about MjolnirVSS that is worth repeating where somebody can see it.
fn rail_footer_text() -> String {
    "Nothing is installed.\r\nDeleting this folder removes every trace.".to_owned()
}

/// Which screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    /// Backing this computer up: what would be copied, and where to put it.
    Backup,
    Progress,
    Result,
    Browse,
    Media,
    Settings,
}

impl Screen {
    /// Which navigation item is lit while this screen is open.
    ///
    /// Progress and Result belong to the section that started them, so the rail
    /// does not appear to jump somewhere else while a backup runs.
    fn section(self) -> Screen {
        match self {
            Screen::Progress | Screen::Result => Screen::Backup,
            other => other,
        }
    }

    /// The heading at the top of the content pane.
    fn title(self) -> &'static str {
        match self.section() {
            Screen::Backup => "Back up this PC",
            Screen::Browse => "Restore files",
            Screen::Media => "Recovery media",
            Screen::Settings => "Settings",
            // Unreachable: section() only ever returns the four above.
            _ => "MjolnirVSS",
        }
    }

    /// The supporting sentence under the heading.
    fn subtitle(self) -> &'static str {
        match self.section() {
            Screen::Backup => "Create a complete recovery image of this Windows installation.",
            Screen::Browse => "Open a backup and copy individual files back out of it.",
            Screen::Media => "Create bootable media now, before you need it.",
            Screen::Settings => "What MjolnirVSS decides for itself, and why.",
            _ => "",
        }
    }
}

/// Where a label sits, which decides what colour it is drawn in.
///
/// A label on the navy rail and a label on a white card cannot share a colour,
/// and a static control paints its own background, so each one has to be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    /// Primary text on the navigation rail.
    OnRail,
    /// Secondary text on the navigation rail.
    OnRailDim,
    /// A page title on the page background.
    TitleOnPage,
    /// Ordinary text on the page background.
    OnPage,
    /// Secondary text on the page background.
    DimOnPage,
    /// A heading on a card.
    TitleOnCard,
    /// Ordinary text on a card.
    OnCard,
    /// Secondary text on a card.
    DimOnCard,
    /// Something that went well.
    Good,
    /// Something that needs attention.
    Caution,
    /// Something that failed.
    Bad,
}

impl Tone {
    /// The text and background colours for this tone.
    fn colours(self, palette: &Palette) -> (COLORREF, COLORREF) {
        match self {
            Tone::OnRail => (palette.nav_text, palette.nav),
            Tone::OnRailDim => (palette.nav_text_dim, palette.nav),
            Tone::TitleOnPage => (palette.ink, palette.page),
            Tone::OnPage => (palette.body, palette.page),
            Tone::DimOnPage => (palette.muted, palette.page),
            Tone::TitleOnCard => (palette.ink, palette.card),
            Tone::OnCard => (palette.body, palette.card),
            Tone::DimOnCard => (palette.muted, palette.card),
            Tone::Good => (palette.success, palette.page),
            Tone::Caution => (palette.warning, palette.page),
            Tone::Bad => (palette.danger, palette.page),
        }
    }
}

/// Every control in the window. Created once, shown per screen.
#[derive(Default)]
struct Controls {
    // The navigation rail down the left, in the order it is drawn. These four
    // are always on screen: the window has no separate menu to return to, so
    // whichever section is open, the way to every other one is still visible.
    nav_backup: HWND,
    nav_restore: HWND,
    nav_media: HWND,
    nav_settings: HWND,

    brand: HWND,
    brand_version: HWND,
    rail_footer: HWND,
    page_title: HWND,
    page_subtitle: HWND,

    disk_overline: HWND,
    disk_name: HWND,
    disk_meta: HWND,
    data_overline: HWND,
    data_figure: HWND,
    part_legend: HWND,
    notes: HWND,
    dest_label: HWND,
    dest_edit: HWND,
    dest_browse: HWND,
    name_label: HWND,
    name_edit: HWND,
    space: HWND,
    start: HWND,
    reassurance: HWND,

    prog_heading: HWND,
    prog_stage: HWND,
    prog_percent: HWND,
    progress: HWND,
    cancel: HWND,
    details_toggle: HWND,
    details: HWND,
    stat_key: [HWND; STATS],
    stat_value: [HWND; STATS],
    stage_row: [HWND; STAGES],

    result_title: HWND,
    result_note: HWND,
    result_body: HWND,
    open_folder: HWND,
    make_media: HWND,
    close: HWND,

    browse_volume: HWND,
    browse_path: HWND,
    browse_list: HWND,
    browse_open: HWND,
    browse_up: HWND,
    browse_extract: HWND,
    browse_back: HWND,

    media_body: HWND,
    media_requirement: HWND,
    settings_body: HWND,
}

impl Controls {
    fn all(&self) -> Vec<HWND> {
        let mut v = vec![
            self.nav_backup,
            self.nav_restore,
            self.nav_media,
            self.nav_settings,
            self.brand,
            self.brand_version,
            self.rail_footer,
            self.page_title,
            self.page_subtitle,
            self.disk_overline,
            self.disk_name,
            self.disk_meta,
            self.data_overline,
            self.data_figure,
            self.part_legend,
            self.notes,
            self.dest_label,
            self.dest_edit,
            self.dest_browse,
            self.name_label,
            self.name_edit,
            self.space,
            self.start,
            self.reassurance,
            self.prog_heading,
            self.prog_stage,
            self.prog_percent,
            self.progress,
            self.cancel,
            self.details_toggle,
            self.details,
            self.result_title,
            self.result_note,
            self.result_body,
            self.open_folder,
            self.make_media,
            self.close,
            self.browse_volume,
            self.browse_path,
            self.browse_list,
            self.browse_open,
            self.browse_up,
            self.browse_extract,
            self.browse_back,
            self.media_body,
            self.media_requirement,
            self.settings_body,
        ];
        v.extend_from_slice(&self.stat_key);
        v.extend_from_slice(&self.stat_value);
        v.extend_from_slice(&self.stage_row);
        v
    }

    /// The sections, in the order the rail lists them.
    ///
    /// Four, and the rail shows four. Anything that is not one of these is
    /// content, not navigation, and belongs in the pane on the right.
    fn sections(&self) -> [HWND; 4] {
        [
            self.nav_backup,
            self.nav_restore,
            self.nav_media,
            self.nav_settings,
        ]
    }

    /// The rail and the page heading, which every screen has.
    fn always_visible(&self) -> [HWND; 8] {
        [
            self.nav_backup,
            self.nav_restore,
            self.nav_media,
            self.nav_settings,
            self.brand,
            self.brand_version,
            self.rail_footer,
            self.page_title,
        ]
    }

    fn for_screen(&self, screen: Screen) -> Vec<HWND> {
        let mut shown = self.always_visible().to_vec();
        shown.push(self.page_subtitle);
        shown.extend(self.content_of(screen));
        shown
    }

    /// What the content pane holds, which is everything but the rail.
    fn content_of(&self, screen: Screen) -> Vec<HWND> {
        match screen {
            Screen::Media => vec![self.media_body, self.media_requirement, self.make_media],
            Screen::Settings => vec![self.settings_body],
            Screen::Backup => vec![
                self.disk_overline,
                self.disk_name,
                self.disk_meta,
                self.data_overline,
                self.data_figure,
                self.part_legend,
                self.notes,
                self.dest_label,
                self.dest_edit,
                self.dest_browse,
                self.name_label,
                self.name_edit,
                self.space,
                self.start,
                self.reassurance,
            ],
            Screen::Progress => {
                let mut v = vec![
                    self.prog_heading,
                    self.prog_stage,
                    self.prog_percent,
                    self.progress,
                    self.cancel,
                    self.details_toggle,
                    self.details,
                ];
                v.extend_from_slice(&self.stat_key);
                v.extend_from_slice(&self.stat_value);
                v.extend_from_slice(&self.stage_row);
                v
            }
            Screen::Result => vec![
                self.result_title,
                self.result_note,
                self.result_body,
                self.open_folder,
                self.make_media,
                self.close,
            ],
            Screen::Browse => vec![
                self.browse_volume,
                self.browse_path,
                self.browse_list,
                self.browse_open,
                self.browse_up,
                self.browse_extract,
                self.browse_back,
            ],
        }
    }
}

/// Where the painted surfaces go.
///
/// Worked out once per layout and kept, so the rectangles a control is placed in
/// and the rectangles painted underneath it cannot drift apart.
#[derive(Default, Clone)]
struct Geometry {
    rail: RECT,
    brand_icon: RECT,
    nav_icons: Vec<RECT>,
    cards: Vec<RECT>,
    partition_bar: Option<RECT>,
    partition_shares: Vec<(u64, PartitionRole)>,
    stage_marks: Vec<RECT>,
    result_icon: Option<(RECT, Glyph, bool)>,
}

/// The work the window can have running.
///
/// Only one at a time, ever. Two shadow copies of the same machine, or two
/// programs writing the same folder, is the kind of thing that produces a
/// backup nobody can explain afterwards.
enum Job {
    /// Backing up this computer.
    Backup(Worker<BackupOutcome>),
    /// Building recovery media.
    Media(Worker<MediaOutcome>),
    /// Reading the list of files in a backed up volume.
    Index(Worker<mjolnir_ntfs::volume::FileIndex>),
    /// Copying files out of a backup.
    Extract(Worker<mjolnir_files::ExtractOutcome>),
}

impl Job {
    fn progress(&self) -> &std::sync::Arc<mjolnir_win32_ui::worker::SharedProgress> {
        match self {
            Job::Backup(w) => w.progress(),
            Job::Media(w) => w.progress(),
            Job::Index(w) => w.progress(),
            Job::Extract(w) => w.progress(),
        }
    }

    fn is_cancelling(&self) -> bool {
        match self {
            Job::Backup(w) => w.is_cancelling(),
            Job::Media(w) => w.is_cancelling(),
            Job::Index(w) => w.is_cancelling(),
            Job::Extract(w) => w.is_cancelling(),
        }
    }

    fn is_finished(&self) -> bool {
        match self {
            Job::Backup(w) => w.is_finished(),
            Job::Media(w) => w.is_finished(),
            Job::Index(w) => w.is_finished(),
            Job::Extract(w) => w.is_finished(),
        }
    }

    fn cancel(&self) {
        match self {
            Job::Backup(w) => w.cancel(),
            Job::Media(w) => w.cancel(),
            Job::Index(w) => w.cancel(),
            Job::Extract(w) => w.cancel(),
        }
    }

    /// What this job is called, for a message about it already running.
    fn describe(&self) -> &'static str {
        match self {
            Job::Backup(_) => "A backup is already running.",
            Job::Media(_) => "Recovery media is already being made.",
            Job::Index(_) => "A backup is already being opened.",
            Job::Extract(_) => "Files are already being copied.",
        }
    }

    /// What the progress screen calls this job.
    fn heading(&self) -> &'static str {
        match self {
            Job::Backup(_) => "Backing up this PC",
            Job::Media(_) => "Building recovery media",
            Job::Index(_) => "Opening the backup",
            Job::Extract(_) => "Copying files",
        }
    }

    /// The stages this job goes through, in order.
    ///
    /// Shown as a list with the one running marked, so somebody watching a
    /// twenty minute backup can see where it is without reading a percentage.
    fn stages(&self) -> [&'static str; STAGES] {
        match self {
            Job::Backup(_) => [
                "Snapshot created",
                "Disk layout captured",
                "Copying the disk",
                "Verifying the backup",
            ],
            Job::Media(_) => [
                "Windows recovery files found",
                "Boot image prepared",
                "Writing the image",
                "Checking the image",
            ],
            Job::Index(_) => [
                "Backup opened",
                "Filesystem read",
                "Building the file list",
                "Ready",
            ],
            Job::Extract(_) => ["Backup opened", "Files located", "Copying", "Finished"],
        }
    }

    fn take_result(&mut self) -> Option<JobResult> {
        match self {
            Job::Backup(w) => w.take_result().map(JobResult::Backup),
            Job::Media(w) => w.take_result().map(JobResult::Media),
            Job::Index(w) => w.take_result().map(JobResult::Index),
            Job::Extract(w) => w.take_result().map(JobResult::Extract),
        }
    }
}

/// What a finished job produced.
enum JobResult {
    /// A backup, or why there was not one.
    Backup(std::result::Result<BackupOutcome, Error>),
    /// Recovery media, or why there was none.
    Media(std::result::Result<MediaOutcome, Error>),
    /// A volume's file tree, or why it could not be read.
    Index(std::result::Result<mjolnir_ntfs::volume::FileIndex, Error>),
    /// Files copied out, or why they were not.
    Extract(std::result::Result<mjolnir_files::ExtractOutcome, Error>),
}

/// A backup opened for browsing.
///
/// The file tree is kept, and the backup is re-opened for each extraction. That
/// avoids holding a reader and an index that borrow from each other for the
/// life of a screen, and browsing itself needs no reading: the tree is already
/// in memory.
struct Browsing {
    /// The backup folder.
    backup: PathBuf,
    /// Which partition is being browsed.
    stream_id: String,
    /// How that partition should be described.
    volume_name: String,
    /// The tree.
    index: mjolnir_ntfs::volume::FileIndex,
    /// The directory being shown.
    at: u64,
    /// What is in it, in the order the list shows it.
    entries: Vec<mjolnir_ntfs::volume::IndexEntry>,
}

/// How one entry reads in the list.
fn describe_entry(entry: &mjolnir_ntfs::volume::IndexEntry) -> String {
    let mut line = if entry.is_directory {
        format!("[{}]", entry.name)
    } else {
        entry.name.clone()
    };
    if !entry.is_directory {
        line.push_str(&format!(
            "   {}",
            mjolnir_core::progress::format_bytes(entry.size)
        ));
    }
    let mut notes = Vec::new();
    if entry.is_reparse_point {
        notes.push("link");
    }
    if let Some(why) = entry.why_unreadable() {
        notes.push(why);
    }
    if !entry.streams.is_empty() {
        notes.push("has hidden streams");
    }
    if !notes.is_empty() {
        line.push_str(&format!("   ({})", notes.join(", ")));
    }
    line
}

/// The colour a partition is drawn in, by what it is for.
///
/// The Windows partition is the accent, because it is the one the person cares
/// about; the rest are steps away from it. Nothing depends on colour alone: the
/// legend underneath names every partition in the same order.
fn role_colour(role: PartitionRole, palette: &Palette) -> COLORREF {
    if palette.high_contrast {
        // In a high contrast theme every segment is the system's own highlight,
        // and the legend is what distinguishes them.
        return palette.accent;
    }
    match role {
        PartitionRole::Windows => palette.accent,
        PartitionRole::EfiSystem => theme::rgb(0x5A, 0xA9, 0xF0),
        PartitionRole::Recovery => theme::rgb(0x4B, 0x86, 0xB4),
        PartitionRole::MicrosoftReserved => theme::rgb(0xA9, 0xB6, 0xC2),
        PartitionRole::Data => theme::rgb(0x7C, 0x93, 0xA8),
        PartitionRole::Unknown => theme::rgb(0xC0, 0xC8, 0xD0),
    }
}

/// The window's state.
pub struct BackupWindow {
    dpi: u32,
    screen: Screen,
    controls: Controls,
    /// The tone of every label that is not drawn in the default colours.
    tones: Vec<(HWND, Tone)>,
    geometry: Geometry,
    plan: Option<BackupPlan>,
    worker: Option<Job>,
    finished: Option<JobResult>,
    show_details: bool,
    /// Which stage of the running job is current, as an index.
    stage_index: usize,
    browsing: Option<Browsing>,
    /// What the file tree being read is for, while it is being read.
    pending_browse: Option<(PathBuf, String, String)>,
}

impl BackupWindow {
    /// Builds the window's controls. Called once, when the window exists.
    pub fn new(window: &Window) -> Self {
        let dpi = sys::dpi_of(window.raw());
        let mut me = Self {
            dpi,
            screen: Screen::Backup,
            controls: Controls::default(),
            tones: Vec::new(),
            geometry: Geometry::default(),
            plan: None,
            worker: None,
            finished: None,
            show_details: false,
            stage_index: 0,
            browsing: None,
            pending_browse: None,
        };
        me.create_controls(window);
        me
    }

    /// Says what surface a control that is not a label sits on.
    ///
    /// A read only text area asks its parent for colours the same way a static
    /// does, so it needs telling too, or it draws itself in dialog grey in the
    /// middle of a white card.
    fn tone_of(&mut self, hwnd: HWND, tone: Tone) {
        self.tones.push((hwnd, tone));
        let (foreground, background) = tone.colours(&Palette::current());
        set_label_colours(hwnd, foreground, background);
    }

    /// Creates one label, in a given type style and colour.
    fn label(&mut self, h: HWND, text: &str, id: i32, style: TextStyle, tone: Tone) -> HWND {
        self.label_of(h, ControlKind::Label, text, id, style, tone)
    }

    /// Creates one label whose text sits against its right edge.
    ///
    /// For a value at the right of a card, so its right edge lands on the
    /// card's padding rather than wherever the text happens to end.
    fn label_right(&mut self, h: HWND, text: &str, id: i32, style: TextStyle, tone: Tone) -> HWND {
        self.label_of(h, ControlKind::LabelRight, text, id, style, tone)
    }

    /// Creates one block of text, wrapped over as many lines as it needs.
    fn paragraph(&mut self, h: HWND, text: &str, id: i32, style: TextStyle, tone: Tone) -> HWND {
        self.label_of(h, ControlKind::Paragraph, text, id, style, tone)
    }

    /// Creates a label of a given kind, registering its colours as it goes.
    fn label_of(
        &mut self,
        h: HWND,
        kind: ControlKind,
        text: &str,
        id: i32,
        style: TextStyle,
        tone: Tone,
    ) -> HWND {
        let hwnd = sys::create_control(h, kind, text, id, theme::font(style, self.dpi));
        self.tones.push((hwnd, tone));
        let (foreground, background) = tone.colours(&Palette::current());
        set_label_colours(hwnd, foreground, background);
        hwnd
    }

    fn create_controls(&mut self, window: &Window) {
        let h = window.raw();
        let body = theme::font(TextStyle::Body, self.dpi);
        let nav_font = theme::font(TextStyle::Body, self.dpi);

        // The rail. Owner drawn buttons, still real buttons: Tab reaches them,
        // Space presses them and a screen reader calls them buttons.
        let c = &mut self.controls;
        c.nav_backup = sys::create_control(
            h,
            ControlKind::NavItem,
            "Back up this PC",
            ID_NAV_BACKUP,
            nav_font,
        );
        c.nav_restore = sys::create_control(
            h,
            ControlKind::NavItem,
            "Restore files",
            ID_NAV_RESTORE,
            nav_font,
        );
        c.nav_media = sys::create_control(
            h,
            ControlKind::NavItem,
            "Recovery media",
            ID_NAV_MEDIA,
            nav_font,
        );
        c.nav_settings = sys::create_control(
            h,
            ControlKind::NavItem,
            "Settings",
            ID_NAV_SETTINGS,
            nav_font,
        );

        self.controls.brand = self.label(h, "MjolnirVSS", ID_BRAND, TextStyle::Brand, Tone::OnRail);
        self.controls.brand_version = self.label(
            h,
            mjolnir_core::TOOL_VERSION,
            ID_BRAND_VERSION,
            TextStyle::Caption,
            Tone::OnRailDim,
        );
        self.controls.rail_footer = self.paragraph(
            h,
            &rail_footer_text(),
            ID_RAIL_FOOTER,
            TextStyle::Caption,
            Tone::OnRailDim,
        );

        self.controls.page_title =
            self.label(h, "", ID_PAGE_TITLE, TextStyle::Title, Tone::TitleOnPage);
        self.controls.page_subtitle = self.paragraph(
            h,
            "",
            ID_PAGE_SUBTITLE,
            TextStyle::Subtitle,
            Tone::DimOnPage,
        );

        // --- the backup screen ---
        self.controls.disk_overline = self.label(
            h,
            "System disk",
            ID_DISK_OVERLINE,
            TextStyle::Overline,
            Tone::DimOnCard,
        );
        self.controls.disk_name =
            self.label(h, "", ID_DISK_NAME, TextStyle::Heading, Tone::TitleOnCard);
        self.controls.disk_meta =
            self.label(h, "", ID_DISK_META, TextStyle::Caption, Tone::DimOnCard);
        self.controls.data_overline = self.label_right(
            h,
            "To back up",
            ID_DATA_OVERLINE,
            TextStyle::Overline,
            Tone::DimOnCard,
        );
        self.controls.data_figure =
            self.label_right(h, "", ID_DATA_FIGURE, TextStyle::Figure, Tone::TitleOnCard);
        self.controls.part_legend =
            self.label(h, "", ID_PART_LEGEND, TextStyle::Caption, Tone::DimOnCard);
        self.controls.notes = self.paragraph(h, "", ID_NOTES, TextStyle::Caption, Tone::Caution);

        self.controls.dest_label = self.label(
            h,
            "Save backup to",
            ID_DEST_LABEL,
            TextStyle::Strong,
            Tone::TitleOnPage,
        );
        self.controls.dest_edit =
            sys::create_control(h, ControlKind::TextBox, "", ID_DEST_EDIT, body);
        self.controls.dest_browse =
            sys::create_control(h, ControlKind::Button, "Browse…", ID_DEST_BROWSE, body);
        self.controls.name_label = self.label(
            h,
            "Backup name",
            ID_NAME_LABEL,
            TextStyle::Strong,
            Tone::TitleOnPage,
        );
        self.controls.name_edit =
            sys::create_control(h, ControlKind::TextBox, "", ID_NAME_EDIT, body);
        self.controls.space = self.paragraph(h, "", ID_SPACE, TextStyle::Caption, Tone::OnPage);
        self.controls.start = sys::create_control(
            h,
            ControlKind::AccentButton,
            "Start backup",
            ID_START,
            theme::font(TextStyle::Strong, self.dpi),
        );
        set_item_style(self.controls.start, ItemStyle::Primary);
        self.controls.reassurance = self.paragraph(
            h,
            "The backup is verified automatically when it finishes.",
            ID_REASSURANCE,
            TextStyle::Caption,
            Tone::DimOnPage,
        );

        // --- the progress screen ---
        self.controls.prog_heading = self.label(
            h,
            "",
            ID_PROG_HEADING,
            TextStyle::Heading,
            Tone::TitleOnCard,
        );
        self.controls.prog_stage = self.label(h, "", ID_PROG_STAGE, TextStyle::Body, Tone::OnCard);
        self.controls.prog_percent =
            self.label_right(h, "", ID_PROG_PERCENT, TextStyle::Figure, Tone::TitleOnCard);
        self.controls.progress =
            sys::create_control(h, ControlKind::ProgressBar, "", ID_PROGRESS, body);
        for index in 0..STATS {
            self.controls.stat_key[index] = self.label(
                h,
                "",
                ID_STAT_KEY + index as i32,
                TextStyle::Overline,
                Tone::DimOnCard,
            );
            self.controls.stat_value[index] = self.label(
                h,
                "",
                ID_STAT_VALUE + index as i32,
                TextStyle::Strong,
                Tone::TitleOnCard,
            );
        }
        for index in 0..STAGES {
            self.controls.stage_row[index] = self.label(
                h,
                "",
                ID_STAGE_ROW + index as i32,
                TextStyle::Body,
                Tone::DimOnCard,
            );
        }
        self.controls.cancel =
            sys::create_control(h, ControlKind::Button, "Cancel", ID_CANCEL, body);
        self.controls.details_toggle = sys::create_control(
            h,
            ControlKind::Button,
            "Show details",
            ID_DETAILS_TOGGLE,
            body,
        );
        self.controls.details = sys::create_control(h, ControlKind::TextArea, "", ID_DETAILS, body);
        self.tone_of(self.controls.details, Tone::OnCard);

        // --- the result screen ---
        self.controls.result_title =
            self.label(h, "", ID_RESULT_TITLE, TextStyle::Title, Tone::TitleOnPage);
        self.controls.result_note =
            self.paragraph(h, "", ID_RESULT_NOTE, TextStyle::Subtitle, Tone::DimOnPage);
        self.controls.result_body =
            sys::create_control(h, ControlKind::TextArea, "", ID_RESULT_BODY, body);
        self.tone_of(self.controls.result_body, Tone::OnCard);
        self.controls.open_folder = sys::create_control(
            h,
            ControlKind::Button,
            "Open backup folder",
            ID_OPEN_FOLDER,
            body,
        );
        self.controls.make_media = sys::create_control(
            h,
            ControlKind::AccentButton,
            "Create recovery media",
            ID_MAKE_MEDIA,
            theme::font(TextStyle::Strong, self.dpi),
        );
        self.controls.close = sys::create_control(h, ControlKind::Button, "Close", ID_CLOSE, body);
        set_item_style(self.controls.make_media, ItemStyle::Primary);

        // --- restoring files ---
        self.controls.browse_volume = self.label(
            h,
            "",
            ID_BROWSE_VOLUME,
            TextStyle::Heading,
            Tone::TitleOnPage,
        );
        self.controls.browse_path =
            self.label(h, "", ID_BROWSE_PATH, TextStyle::Caption, Tone::DimOnPage);
        self.controls.browse_list =
            sys::create_control(h, ControlKind::ListBox, "", ID_BROWSE_LIST, body);
        self.controls.browse_open =
            sys::create_control(h, ControlKind::Button, "Open", ID_BROWSE_OPEN, body);
        self.controls.browse_up =
            sys::create_control(h, ControlKind::Button, "Up", ID_BROWSE_UP, body);
        self.controls.browse_extract = sys::create_control(
            h,
            ControlKind::AccentButton,
            "Copy out…",
            ID_BROWSE_EXTRACT,
            theme::font(TextStyle::Strong, self.dpi),
        );
        self.controls.browse_back =
            sys::create_control(h, ControlKind::Button, "Back", ID_BROWSE_BACK, body);
        set_item_style(self.controls.browse_extract, ItemStyle::Primary);

        // --- recovery media and settings ---
        self.controls.media_body =
            self.paragraph(h, MEDIA_BODY, ID_MEDIA_BODY, TextStyle::Body, Tone::OnCard);
        self.controls.media_requirement = self.label(
            h,
            "Requires the Windows Assessment and Deployment Kit.",
            ID_MEDIA_REQUIREMENT,
            TextStyle::Caption,
            Tone::DimOnPage,
        );
        self.controls.settings_body = self.paragraph(
            h,
            SETTINGS_BODY,
            ID_SETTINGS_BODY,
            TextStyle::Body,
            Tone::OnCard,
        );
    }

    /// Changes the colour a label is drawn in.
    ///
    /// Used where the meaning is only known once something has finished: the
    /// same line says how a backup went, and it should not be the same colour
    /// whether it worked or not.
    fn retone(&mut self, hwnd: HWND, tone: Tone) {
        if let Some(entry) = self.tones.iter_mut().find(|(h, _)| *h == hwnd) {
            entry.1 = tone;
        }
        let (foreground, background) = tone.colours(&Palette::current());
        set_label_colours(hwnd, foreground, background);
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
        if screen == Screen::Backup {
            // The notes line only exists when there is something to say.
            let has_notes = self
                .plan
                .as_ref()
                .map(|p| !p.warnings.is_empty())
                .unwrap_or(false);
            sys::show(self.controls.notes, has_notes);
        }

        sys::set_text(self.controls.page_title, screen.title());
        sys::set_text(self.controls.page_subtitle, screen.subtitle());
        self.mark_selected_section(screen.section());

        // A backup that is running is not the moment to wander into another
        // section, so the rail is unavailable until it has finished.
        for hwnd in self.controls.sections() {
            sys::enable(hwnd, !matches!(screen, Screen::Progress));
        }

        self.layout(window);

        // Where the keyboard goes, and what Enter means, for each screen. Enter
        // needs saying separately: a plain window does not answer the question
        // the dialog manager asks before turning Enter into a button press, so
        // without this it works only while a button already has the keyboard.
        let (focus, default) = match screen {
            Screen::Backup => (self.controls.start, ID_START),
            Screen::Media => (self.controls.make_media, ID_MAKE_MEDIA),
            Screen::Settings => (self.controls.nav_settings, ID_NAV_SETTINGS),
            Screen::Progress => (self.controls.cancel, ID_CANCEL),
            Screen::Result => (self.controls.close, ID_CLOSE),
            Screen::Browse => (self.controls.browse_list, ID_BROWSE_OPEN),
        };
        window.set_default_button(default);
        window.focus(focus);
        window.invalidate();
    }

    /// Works out where everything goes, and puts the controls there.
    /// Lights the rail item for the section that is open.
    fn mark_selected_section(&self, selected: Screen) {
        for (hwnd, screen, glyph) in [
            (self.controls.nav_backup, Screen::Backup, Glyph::Disk),
            (self.controls.nav_restore, Screen::Browse, Glyph::Document),
            (self.controls.nav_media, Screen::Media, Glyph::Disc),
            (self.controls.nav_settings, Screen::Settings, Glyph::Sliders),
        ] {
            set_item_style(
                hwnd,
                ItemStyle::Nav {
                    glyph,
                    selected: screen == selected,
                },
            );
        }
    }

    fn layout(&mut self, window: &Window) {
        let dpi = sys::dpi_of(window.raw());
        self.dpi = dpi;
        let s = |v: i32| Metrics::at(v, dpi);
        let client: RECT = window.client_rect();
        let width = client.right - client.left;
        let height = client.bottom - client.top;

        let mut geometry = Geometry {
            rail: sys::rect(0, 0, s(Metrics::NAV_WIDTH), height),
            ..Default::default()
        };

        // ---- the rail -----------------------------------------------------
        let rail = s(Metrics::NAV_WIDTH);
        let icon = s(Metrics::LINE_HEADING);
        let brand_x = s(Metrics::SPACE_L);
        let text_x = brand_x + icon + s(Metrics::SPACE_M);
        let text_width = rail - text_x - s(Metrics::SPACE_M);

        // The mark, the name and the version are one block, centred together
        // in the rail's header rather than each placed at its own guessed y.
        let block = Metrics::LINE_HEADING + Metrics::LINE_CAPTION;
        let block_top = (s(Metrics::NAV_HEADER) - s(block)) / 2;
        geometry.brand_icon = sys::rect(brand_x, block_top + (s(block) - icon) / 2, icon, icon);
        sys::place(
            self.controls.brand,
            sys::rect(text_x, block_top, text_width, s(Metrics::LINE_HEADING)),
        );
        sys::place(
            self.controls.brand_version,
            sys::rect(
                text_x,
                block_top + s(Metrics::LINE_HEADING),
                text_width,
                s(Metrics::LINE_CAPTION),
            ),
        );

        let item_width = rail - s(Metrics::NAV_INSET) * 2;
        let mut ny = s(Metrics::NAV_HEADER);
        for hwnd in [
            self.controls.nav_backup,
            self.controls.nav_restore,
            self.controls.nav_media,
        ] {
            sys::place(
                hwnd,
                sys::rect(s(Metrics::NAV_INSET), ny, item_width, s(Metrics::NAV_ITEM)),
            );
            ny += s(Metrics::NAV_ITEM + Metrics::NAV_ITEM_GAP);
        }

        // Settings sits at the bottom of the rail, the way Windows' own
        // settings applications put it, with the footer beneath it.
        let footer_height = s(Metrics::LINE_CAPTION * 3);
        let settings_y = height - s(Metrics::SPACE_L) - footer_height - s(Metrics::NAV_ITEM);
        sys::place(
            self.controls.nav_settings,
            sys::rect(
                s(Metrics::NAV_INSET),
                settings_y,
                item_width,
                s(Metrics::NAV_ITEM),
            ),
        );
        sys::place(
            self.controls.rail_footer,
            sys::rect(
                brand_x,
                height - s(Metrics::SPACE_M) - footer_height,
                rail - brand_x - s(Metrics::SPACE_M),
                footer_height,
            ),
        );

        // ---- the content pane ---------------------------------------------
        let x = rail + s(Metrics::PAGE_MARGIN);
        let inner = width - x - s(Metrics::PAGE_MARGIN);
        let floor = height - s(Metrics::PAGE_MARGIN);

        let mut y = s(Metrics::PAGE_MARGIN);
        sys::place(
            self.controls.page_title,
            sys::rect(x, y, inner, s(Metrics::LINE_TITLE)),
        );
        y += s(Metrics::LINE_TITLE);
        sys::place(
            self.controls.page_subtitle,
            sys::rect(x, y, inner, s(Metrics::LINE_BODY)),
        );
        y += s(Metrics::LINE_BODY + Metrics::SPACE_L);

        match self.screen {
            Screen::Backup => self.layout_backup(x, y, inner, floor, &s, &mut geometry),
            Screen::Progress => self.layout_progress(x, y, inner, floor, &s, &mut geometry),
            Screen::Result => self.layout_result(x, y, inner, floor, &s, &mut geometry),
            Screen::Browse => self.layout_browse(x, y, inner, floor, &s, &mut geometry),
            Screen::Media => self.layout_media(x, y, inner, floor, &s, &mut geometry),
            Screen::Settings => self.layout_settings(x, y, inner, floor, &s, &mut geometry),
        }

        self.geometry = geometry;
    }

    fn layout_backup(
        &self,
        x: i32,
        top: i32,
        inner: i32,
        floor: i32,
        s: &dyn Fn(i32) -> i32,
        geometry: &mut Geometry,
    ) {
        let c = &self.controls;
        let pad = s(Metrics::CARD_PADDING);

        // The disk card: what is being copied, how much of it there is, and how
        // the disk is laid out. The left column names the disk; the right one
        // carries the figure, right aligned so it ends on the card's padding.
        let rows = Metrics::LINE_CAPTION
            + Metrics::SPACE_XS
            + Metrics::LINE_HEADING
            + Metrics::LINE_CAPTION
            + Metrics::SPACE_L
            + Metrics::PARTITION_BAR
            + Metrics::SPACE_S
            + Metrics::LINE_CAPTION;
        let card_height = pad * 2 + s(rows);
        geometry.cards.push(sys::rect(x, top, inner, card_height));

        let figure_width = s(Metrics::BUTTON_WIDTH_WIDE);
        let left_width = inner - pad * 2 - figure_width - s(Metrics::SPACE_L);
        let right_x = x + inner - pad - figure_width;

        let mut cy = top + pad;
        sys::place(
            c.disk_overline,
            sys::rect(x + pad, cy, left_width, s(Metrics::LINE_CAPTION)),
        );
        sys::place(
            c.data_overline,
            sys::rect(right_x, cy, figure_width, s(Metrics::LINE_CAPTION)),
        );
        cy += s(Metrics::LINE_CAPTION + Metrics::SPACE_XS);

        // The heading and the figure share a row. Both are single line labels,
        // which centre their text in their box, so boxes of different heights
        // still line up; the figure's box is the taller of the two and is
        // raised by half the difference to keep both centred on one line.
        let figure_lift = s(Metrics::LINE_FIGURE - Metrics::LINE_HEADING) / 2;
        sys::place(
            c.disk_name,
            sys::rect(x + pad, cy, left_width, s(Metrics::LINE_HEADING)),
        );
        sys::place(
            c.data_figure,
            sys::rect(
                right_x,
                cy - figure_lift,
                figure_width,
                s(Metrics::LINE_FIGURE),
            ),
        );
        cy += s(Metrics::LINE_HEADING);
        sys::place(
            c.disk_meta,
            sys::rect(x + pad, cy, left_width, s(Metrics::LINE_CAPTION)),
        );
        cy += s(Metrics::LINE_CAPTION + Metrics::SPACE_L);

        geometry.partition_bar = Some(sys::rect(
            x + pad,
            cy,
            inner - pad * 2,
            s(Metrics::PARTITION_BAR),
        ));
        geometry.partition_shares = self
            .plan
            .as_ref()
            .map(|p| {
                p.partitions
                    .iter()
                    .map(|part| (part.partition.length, part.role))
                    .collect()
            })
            .unwrap_or_default();
        cy += s(Metrics::PARTITION_BAR + Metrics::SPACE_S);
        sys::place(
            c.part_legend,
            sys::rect(x + pad, cy, inner - pad * 2, s(Metrics::LINE_CAPTION)),
        );

        let mut y = top + card_height + s(Metrics::SPACE_L);

        // Anything the plan wants to say that does not stop the backup.
        let has_notes = self
            .plan
            .as_ref()
            .map(|p| !p.warnings.is_empty())
            .unwrap_or(false);
        if has_notes {
            let notes_height = s(Metrics::LINE_CAPTION * 4);
            sys::place(c.notes, sys::rect(x, y, inner, notes_height));
            y += notes_height + s(Metrics::SPACE_L);
        }

        // Where it goes. Both inputs share both edges, so the two rows read as
        // one block rather than two controls that happen to be near each other.
        let button = s(Metrics::BUTTON_WIDTH);
        let input_width = inner - button - s(Metrics::BUTTON_GAP);
        sys::place(c.dest_label, sys::rect(x, y, inner, s(Metrics::LINE_BODY)));
        y += s(Metrics::LINE_BODY + Metrics::SPACE_S);
        sys::place(
            c.dest_edit,
            sys::rect(x, y, input_width, s(Metrics::INPUT_HEIGHT)),
        );
        // The button is taller than the box it sits beside, so it is centred on
        // it rather than hung from the same top edge.
        let button_lift = s(Metrics::BUTTON_HEIGHT - Metrics::INPUT_HEIGHT) / 2;
        sys::place(
            c.dest_browse,
            sys::rect(
                x + inner - button,
                y - button_lift,
                button,
                s(Metrics::BUTTON_HEIGHT),
            ),
        );
        y += s(Metrics::INPUT_HEIGHT + Metrics::SPACE_M);

        sys::place(c.name_label, sys::rect(x, y, inner, s(Metrics::LINE_BODY)));
        y += s(Metrics::LINE_BODY + Metrics::SPACE_S);
        sys::place(
            c.name_edit,
            sys::rect(x, y, input_width, s(Metrics::INPUT_HEIGHT)),
        );
        y += s(Metrics::INPUT_HEIGHT + Metrics::SPACE_S);

        sys::place(
            c.space,
            sys::rect(x, y, inner, s(Metrics::LINE_CAPTION * 2)),
        );

        // The primary action, and the one line of reassurance under it.
        let action = s(Metrics::BUTTON_WIDTH_WIDE);
        let button_y = floor - s(Metrics::BUTTON_HEIGHT + Metrics::SPACE_S + Metrics::LINE_CAPTION);
        sys::place(
            c.start,
            sys::rect(
                x + inner - action,
                button_y,
                action,
                s(Metrics::BUTTON_HEIGHT),
            ),
        );
        sys::place(
            c.reassurance,
            sys::rect(
                x,
                button_y + s(Metrics::BUTTON_HEIGHT + Metrics::SPACE_S),
                inner,
                s(Metrics::LINE_CAPTION),
            ),
        );
    }

    fn layout_progress(
        &self,
        x: i32,
        top: i32,
        inner: i32,
        floor: i32,
        s: &dyn Fn(i32) -> i32,
        geometry: &mut Geometry,
    ) {
        let c = &self.controls;
        let pad = s(Metrics::CARD_PADDING);

        // The card carrying the headline, the bar and the figures.
        let rows = Metrics::LINE_HEADING
            + Metrics::LINE_BODY
            + Metrics::SPACE_L
            + Metrics::PROGRESS_HEIGHT
            + Metrics::SPACE_L
            + Metrics::LINE_CAPTION
            + Metrics::SPACE_XS
            + Metrics::LINE_BODY;
        let card_height = pad * 2 + s(rows);
        geometry.cards.push(sys::rect(x, top, inner, card_height));

        let percent_width = s(Metrics::BUTTON_WIDTH_WIDE);
        let text_width = inner - pad * 2 - percent_width - s(Metrics::SPACE_L);
        let right_x = x + inner - pad - percent_width;

        // The percentage is centred against the two lines beside it, the same
        // way the figure is on the backup screen.
        let block = Metrics::LINE_HEADING + Metrics::LINE_BODY;
        let percent_lift = s(Metrics::LINE_FIGURE - block) / 2;

        let mut cy = top + pad;
        sys::place(
            c.prog_heading,
            sys::rect(x + pad, cy, text_width, s(Metrics::LINE_HEADING)),
        );
        sys::place(
            c.prog_percent,
            sys::rect(
                right_x,
                cy - percent_lift,
                percent_width,
                s(Metrics::LINE_FIGURE),
            ),
        );
        cy += s(Metrics::LINE_HEADING);
        sys::place(
            c.prog_stage,
            sys::rect(x + pad, cy, text_width, s(Metrics::LINE_BODY)),
        );
        cy += s(Metrics::LINE_BODY + Metrics::SPACE_L);
        sys::place(
            c.progress,
            sys::rect(x + pad, cy, inner - pad * 2, s(Metrics::PROGRESS_HEIGHT)),
        );
        cy += s(Metrics::PROGRESS_HEIGHT + Metrics::SPACE_L);

        // Four figures in a row, each a small heading over a value, every
        // column the same width and every pair on the same two baselines.
        let column = (inner - pad * 2) / STATS as i32;
        for index in 0..STATS {
            let cx = x + pad + column * index as i32;
            let width = column - s(Metrics::SPACE_S);
            sys::place(
                c.stat_key[index],
                sys::rect(cx, cy, width, s(Metrics::LINE_CAPTION)),
            );
            sys::place(
                c.stat_value[index],
                sys::rect(
                    cx,
                    cy + s(Metrics::LINE_CAPTION + Metrics::SPACE_XS),
                    width,
                    s(Metrics::LINE_BODY),
                ),
            );
        }

        let mut y = top + card_height + s(Metrics::SPACE_L);

        // The stages, with the one running marked.
        let stages_height = pad * 2 + s(Metrics::STAGE_ROW) * STAGES as i32;
        geometry.cards.push(sys::rect(x, y, inner, stages_height));
        let mark = s(Metrics::LINE_CAPTION);
        let mut sy = y + pad;
        for index in 0..STAGES {
            geometry.stage_marks.push(sys::rect(
                x + pad,
                sy + (s(Metrics::STAGE_ROW) - mark) / 2,
                mark,
                mark,
            ));
            sys::place(
                c.stage_row[index],
                sys::rect(
                    x + pad + mark + s(Metrics::SPACE_M),
                    sy,
                    inner - pad * 2 - mark - s(Metrics::SPACE_M),
                    s(Metrics::STAGE_ROW),
                ),
            );
            sy += s(Metrics::STAGE_ROW);
        }
        y += stages_height + s(Metrics::SPACE_L);

        let bottom = floor - s(Metrics::BUTTON_HEIGHT);
        let button = s(Metrics::BUTTON_WIDTH);
        sys::place(
            c.details_toggle,
            sys::rect(x, bottom, button, s(Metrics::BUTTON_HEIGHT)),
        );
        sys::place(
            c.cancel,
            sys::rect(
                x + inner - button,
                bottom,
                button,
                s(Metrics::BUTTON_HEIGHT),
            ),
        );
        if self.show_details {
            let height = (bottom - y - s(Metrics::SPACE_M)).max(s(Metrics::LINE_BODY * 2));
            sys::place(c.details, sys::rect(x, y, inner, height));
        }
    }

    fn layout_result(
        &self,
        x: i32,
        top: i32,
        inner: i32,
        floor: i32,
        s: &dyn Fn(i32) -> i32,
        geometry: &mut Geometry,
    ) {
        let c = &self.controls;
        // The title block replaces the page heading on this screen, and carries
        // a mark saying at a glance how it went.
        let icon = s(Metrics::LINE_TITLE);
        let failed = matches!(
            &self.finished,
            Some(JobResult::Backup(Err(_)))
                | Some(JobResult::Media(Err(_)))
                | Some(JobResult::Extract(Err(_)))
        );
        geometry.result_icon = Some((
            sys::rect(x, top, icon, icon),
            if failed { Glyph::Warning } else { Glyph::Tick },
            failed,
        ));

        let text_x = x + icon + s(Metrics::SPACE_M);
        let text_width = inner - icon - s(Metrics::SPACE_M);
        sys::place(
            c.result_title,
            sys::rect(text_x, top, text_width, s(Metrics::LINE_TITLE)),
        );
        sys::place(
            c.result_note,
            sys::rect(
                text_x,
                top + s(Metrics::LINE_TITLE),
                text_width,
                s(Metrics::LINE_BODY),
            ),
        );

        let mut y = top + s(Metrics::LINE_TITLE + Metrics::LINE_BODY + Metrics::SPACE_L);
        let bottom = floor - s(Metrics::BUTTON_HEIGHT);
        let body_height = (bottom - y - s(Metrics::SPACE_L)).max(s(Metrics::LINE_BODY * 4));
        geometry.cards.push(sys::rect(x, y, inner, body_height));
        let pad = s(Metrics::CARD_PADDING);
        sys::place(
            c.result_body,
            sys::rect(x + pad, y + pad, inner - pad * 2, body_height - pad * 2),
        );
        y += body_height;
        let _ = y;

        // Two widths and one gap: the two long labels are wide, Close is not.
        let wide = s(Metrics::BUTTON_WIDTH_WIDE);
        let gap = s(Metrics::BUTTON_GAP);
        sys::place(
            c.open_folder,
            sys::rect(x, bottom, wide, s(Metrics::BUTTON_HEIGHT)),
        );
        sys::place(
            c.make_media,
            sys::rect(x + wide + gap, bottom, wide, s(Metrics::BUTTON_HEIGHT)),
        );
        sys::place(
            c.close,
            sys::rect(
                x + inner - s(Metrics::BUTTON_WIDTH),
                bottom,
                s(Metrics::BUTTON_WIDTH),
                s(Metrics::BUTTON_HEIGHT),
            ),
        );
    }

    fn layout_browse(
        &self,
        x: i32,
        top: i32,
        inner: i32,
        floor: i32,
        s: &dyn Fn(i32) -> i32,
        geometry: &mut Geometry,
    ) {
        let c = &self.controls;
        let mut y = top;
        sys::place(
            c.browse_volume,
            sys::rect(x, y, inner, s(Metrics::LINE_HEADING)),
        );
        y += s(Metrics::LINE_HEADING);
        sys::place(
            c.browse_path,
            sys::rect(x, y, inner, s(Metrics::LINE_CAPTION)),
        );
        y += s(Metrics::LINE_CAPTION + Metrics::SPACE_M);

        let bottom = floor - s(Metrics::BUTTON_HEIGHT);
        let list_height = (bottom - y - s(Metrics::SPACE_L)).max(s(Metrics::LINE_BODY * 4));
        geometry.cards.push(sys::rect(x, y, inner, list_height));
        let pad = s(Metrics::SPACE_XS);
        sys::place(
            c.browse_list,
            sys::rect(x + pad, y + pad, inner - pad * 2, list_height - pad * 2),
        );

        let button = s(Metrics::BUTTON_WIDTH);
        let gap = s(Metrics::BUTTON_GAP);
        for (index, hwnd) in [c.browse_back, c.browse_up, c.browse_open]
            .into_iter()
            .enumerate()
        {
            sys::place(
                hwnd,
                sys::rect(
                    x + (button + gap) * index as i32,
                    bottom,
                    button,
                    s(Metrics::BUTTON_HEIGHT),
                ),
            );
        }
        let wide = s(Metrics::BUTTON_WIDTH_WIDE);
        sys::place(
            c.browse_extract,
            sys::rect(x + inner - wide, bottom, wide, s(Metrics::BUTTON_HEIGHT)),
        );
    }

    fn layout_media(
        &self,
        x: i32,
        top: i32,
        inner: i32,
        floor: i32,
        s: &dyn Fn(i32) -> i32,
        geometry: &mut Geometry,
    ) {
        let c = &self.controls;
        let pad = s(Metrics::CARD_PADDING);
        let card_height = pad * 2 + s(Metrics::LINE_BODY * 8);
        geometry.cards.push(sys::rect(x, top, inner, card_height));
        sys::place(
            c.media_body,
            sys::rect(x + pad, top + pad, inner - pad * 2, card_height - pad * 2),
        );

        let y = top + card_height + s(Metrics::SPACE_L);
        let wide = s(Metrics::BUTTON_WIDTH_WIDE);
        sys::place(
            c.make_media,
            sys::rect(x, y, wide, s(Metrics::BUTTON_HEIGHT)),
        );
        sys::place(
            c.media_requirement,
            sys::rect(
                x,
                y + s(Metrics::BUTTON_HEIGHT + Metrics::SPACE_S),
                inner,
                s(Metrics::LINE_CAPTION),
            ),
        );
        let _ = floor;
    }

    fn layout_settings(
        &self,
        x: i32,
        top: i32,
        inner: i32,
        floor: i32,
        s: &dyn Fn(i32) -> i32,
        geometry: &mut Geometry,
    ) {
        let pad = s(Metrics::CARD_PADDING);
        let card_height = (pad * 2 + s(Metrics::LINE_BODY * 9)).min(floor - top);
        geometry.cards.push(sys::rect(x, top, inner, card_height));
        sys::place(
            self.controls.settings_body,
            sys::rect(x + pad, top + pad, inner - pad * 2, card_height - pad * 2),
        );
    }

    // ---- painting --------------------------------------------------------

    fn paint(&self, canvas: &Canvas) {
        let palette = canvas.palette();
        let geometry = &self.geometry;

        // The rail, and the mark at the top of it.
        canvas.fill(geometry.rail, palette.nav);
        canvas.glyph(geometry.brand_icon, Glyph::Brand, palette.nav_marker);
        // A hairline where the rail meets the page, so the two surfaces read as
        // deliberate rather than as one bleeding into the other.
        canvas.fill(
            sys::rect(
                geometry.rail.right,
                0,
                canvas.scale(1).max(1),
                geometry.rail.bottom,
            ),
            palette.card_border,
        );

        for card in &geometry.cards {
            canvas.card(*card);
        }

        if let Some(bar) = geometry.partition_bar {
            let segments: Vec<_> = geometry
                .partition_shares
                .iter()
                .map(|(length, role)| (*length, role_colour(*role, palette)))
                .collect();
            if segments.is_empty() {
                canvas.rounded(bar, palette.divider, None, 2);
            } else {
                canvas.proportion_bar(bar, &segments);
            }
        }

        for (index, mark) in geometry.stage_marks.iter().enumerate() {
            let (glyph, colour) = match index.cmp(&self.stage_index) {
                std::cmp::Ordering::Less => (Glyph::Tick, palette.success),
                std::cmp::Ordering::Equal => (Glyph::Dot, palette.accent),
                std::cmp::Ordering::Greater => (Glyph::Ring, palette.muted),
            };
            canvas.glyph(*mark, glyph, colour);
        }

        if let Some((rect, glyph, failed)) = geometry.result_icon {
            let colour = if failed {
                palette.danger
            } else {
                palette.success
            };
            // A tick alone is a shape; a tick in a circle reads as a state.
            if glyph == Glyph::Tick {
                canvas.rounded(rect, colour, None, 15);
                let inner = mjolnir_win32_ui::paint::inset(rect, canvas.scale(6));
                canvas.glyph(inner, Glyph::Tick, palette.on_accent);
            } else {
                canvas.glyph(rect, glyph, colour);
            }
        }

        let _ = geometry.nav_icons;
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
            // Planning only; nothing is written, so nothing is sealed.
            encryption: None,
        };

        match mjolnir_backup::plan(&probe) {
            Ok(plan) => {
                self.describe_plan(&plan);
                sys::set_text(self.controls.name_edit, name.as_str());
                self.plan = Some(plan);
                self.update_space_label(window);
                self.show_screen(window, Screen::Backup);
            }
            Err(e) => message_box::error_for(window.raw(), "MjolnirVSS", &e),
        }
    }

    /// Fills the disk card in from a plan.
    fn describe_plan(&self, plan: &BackupPlan) {
        let model = plan
            .disk
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or("System disk");
        sys::set_text(
            self.controls.disk_name,
            &format!("{model} · {}", format_bytes(plan.disk.size_bytes)),
        );
        sys::set_text(
            self.controls.disk_meta,
            &format!(
                "Disk {} · {} · {} · {} partitions · {}",
                plan.disk.number,
                match plan.disk.partition_style {
                    mjolnir_image::disk_layout::PartitionStyle::Gpt => "GPT",
                    mjolnir_image::disk_layout::PartitionStyle::Mbr => "MBR",
                    mjolnir_image::disk_layout::PartitionStyle::Raw => "no partition table",
                },
                plan.disk.bus_type.describe(),
                plan.partitions.len(),
                plan.system.computer_name
            ),
        );
        sys::set_text(self.controls.data_figure, &format_bytes(plan.source_bytes));

        // The legend names every partition in the order the bar draws them, so
        // nothing depends on telling two shades of blue apart.
        let legend = plan
            .partitions
            .iter()
            .map(|p| {
                let letter = p
                    .volume
                    .as_ref()
                    .and_then(|v| v.drive_letter())
                    .map(|l| format!(" ({l}:)"))
                    .unwrap_or_default();
                format!("{}{letter}", p.role.describe())
            })
            .collect::<Vec<_>>()
            .join("   ·   ");
        sys::set_text(self.controls.part_legend, &legend);

        if plan.warnings.is_empty() {
            sys::set_text(self.controls.notes, "");
        } else {
            sys::set_text(self.controls.notes, &plan.warnings.join("\r\n"));
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
                "{} free on this drive. The backup reads {} and is normally smaller once compressed.",
                format_bytes(free),
                format_bytes(needed)
            ),
            None => format!(
                "The backup reads {}. It is normally smaller once compressed.",
                format_bytes(needed)
            ),
        };

        // A backup holds everything on the disk, and MjolnirVSS does not
        // encrypt it. Saying so where the destination is chosen is the only
        // place it is actually useful.
        if plan.contains_decrypted_data {
            text.push_str(
                "\r\nThis backup will contain readable copies of your files, including from the encrypted drive. The backup itself is not encrypted.",
            );
        }

        sys::set_text(self.controls.space, &text);
        sys::enable(self.controls.start, true);
    }

    fn start_backup(&mut self, window: &Window) {
        // Refusing a second job is what stops two shadow copies and two writers
        // running against the same destination.
        if let Some(running) = &self.worker {
            message_box::warn(window.raw(), "MjolnirVSS", running.describe());
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
            // The window does not offer encryption yet: it is reachable from
            // the command line only, and this says so rather than silently
            // writing an unencrypted backup somebody believed was sealed.
            encryption: None,
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
        self.begin_job(Job::Backup(Worker::start(move |progress, cancel| {
            mjolnir_backup::run(&request, &plan, progress, cancel)
        })));
        window.set_timer(TIMER_PROGRESS, TIMER_INTERVAL);
        self.show_screen(window, Screen::Progress);
    }

    /// Puts the progress screen into its starting state and takes the job.
    fn begin_job(&mut self, job: Job) {
        sys::set_text(self.controls.prog_heading, job.heading());
        sys::set_text(self.controls.prog_stage, "Starting…");
        sys::set_text(self.controls.prog_percent, "0%");
        for (index, stage) in job.stages().into_iter().enumerate() {
            sys::set_text(self.controls.stage_row[index], stage);
        }
        for index in 0..STATS {
            sys::set_text(self.controls.stat_key[index], "");
            sys::set_text(self.controls.stat_value[index], "");
        }
        sys::set_text(self.controls.details, "");
        sys::set_progress(self.controls.progress, 0);
        sys::set_progress_state(self.controls.progress, ProgressState::Normal);
        sys::enable(self.controls.cancel, true);
        sys::set_text(self.controls.cancel, "Cancel");
        self.stage_index = 0;
        self.worker = Some(job);
    }

    fn tick(&mut self, window: &Window) {
        let Some(worker) = &mut self.worker else {
            return;
        };

        let snapshot = worker.progress().read();
        let stage = if worker.is_cancelling() && !worker.is_finished() {
            "Stopping…".to_owned()
        } else if snapshot.stage.is_empty() {
            "Starting…".to_owned()
        } else {
            snapshot.stage.clone()
        };
        sys::set_text(self.controls.prog_stage, &stage);

        // Which of the listed stages is running. The engine names its stage in
        // words rather than by number, so this matches on what it says and
        // falls back to keeping the mark where it was, which is better than
        // making it jump about.
        let lower = stage.to_ascii_lowercase();
        let matched = if lower.contains("verif") {
            Some(3)
        } else if lower.contains("copy") || lower.contains("read") || lower.contains("captur") {
            Some(2)
        } else if lower.contains("layout") || lower.contains("partition") {
            Some(1)
        } else if lower.contains("snapshot") || lower.contains("shadow") {
            Some(0)
        } else {
            None
        };
        if let Some(index) = matched {
            if index != self.stage_index {
                self.stage_index = index;
                window.invalidate();
            }
        }

        if let Some(fraction) = snapshot.fraction() {
            sys::set_progress(self.controls.progress, (fraction * 1000.0) as u32);
            sys::set_text(
                self.controls.prog_percent,
                &format!("{:.0}%", fraction * 100.0),
            );
        }

        // Four figures: what has been done, out of how much, how fast, and how
        // long it has been going. A time remaining is only shown once there is
        // enough behind it for the estimate to mean anything.
        let stats: [(&str, String); STATS] = [
            ("Processed", format_bytes(snapshot.done)),
            (
                "Total",
                snapshot
                    .total
                    .map(format_bytes)
                    .unwrap_or_else(|| "—".to_owned()),
            ),
            (
                "Speed",
                if snapshot.rate() > 0.0 {
                    format!("{}/s", format_bytes(snapshot.rate() as u64))
                } else {
                    "—".to_owned()
                },
            ),
            (
                if snapshot.seconds_remaining().is_some() {
                    "Remaining"
                } else {
                    "Elapsed"
                },
                match snapshot.seconds_remaining() {
                    Some(remaining) => format!("about {}", format_duration(remaining)),
                    None => format_duration(snapshot.elapsed_seconds()),
                },
            ),
        ];
        for (index, (key, value)) in stats.into_iter().enumerate() {
            sys::set_text(self.controls.stat_key[index], key);
            sys::set_text(self.controls.stat_value[index], &value);
        }

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
        // Taken rather than borrowed, because painting the result needs the
        // window and the window is reached through the same `self`. Both arms
        // put it back before they return.
        let Some(result) = self.finished.take() else {
            return;
        };

        let result = match result {
            JobResult::Backup(r) => r,
            JobResult::Media(r) => {
                self.show_media_result(window, r);
                return;
            }
            JobResult::Extract(r) => {
                self.show_extract_result(window, r);
                return;
            }
            JobResult::Index(r) => {
                let pending = self.pending_browse.take();
                match (r, pending) {
                    (Ok(index), Some((backup, stream_id, volume_name))) => {
                        let at = mjolnir_ntfs::record::MftReference::ROOT;
                        self.browsing = Some(Browsing {
                            backup,
                            stream_id,
                            volume_name,
                            index,
                            at,
                            entries: Vec::new(),
                        });
                        self.show_browse(window);
                    }
                    (Err(e), _) => {
                        if e.exit() != ExitCode::Cancelled {
                            message_box::error_for(window.raw(), "MjolnirVSS", &e);
                        }
                        self.show_screen(window, Screen::Backup);
                    }
                    (Ok(_), None) => self.show_screen(window, Screen::Backup),
                }
                return;
            }
        };

        let mut good = false;
        match &result {
            Ok(outcome) => {
                sys::set_text(self.controls.result_title, "Backup completed");
                good = true;
                sys::set_text(
                    self.controls.result_note,
                    &format!(
                        "Verified. {} read, {} stored, in {}.",
                        format_bytes(outcome.captured_bytes),
                        format_bytes(outcome.stored_bytes),
                        format_duration(outcome.elapsed_seconds)
                    ),
                );
                let mut body = format!(
                    "Location\r\n{}\r\n\r\nRead from this PC\r\n{}\r\n\r\nWritten to the drive\r\n{}\r\n\r\nTook\r\n{}\r\n\r\nVerification\r\nPassed. Every stored piece was read back, decompressed and checked against its checksum ({} pieces).",
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
                let (title, note) = if e.exit() == ExitCode::Cancelled {
                    ("Backup cancelled", "Nothing was left marked as a backup.")
                } else {
                    ("Backup failed", "Nothing on this computer was changed.")
                };
                sys::set_text(self.controls.result_title, title);
                sys::set_text(self.controls.result_note, note);
                sys::set_text(self.controls.result_body, &message_box::format_error(e));
                sys::enable(self.controls.open_folder, false);
            }
        }
        let tone = if good { Tone::Good } else { Tone::Bad };
        self.retone(self.controls.result_note, tone);
        self.finished = Some(JobResult::Backup(result));
        self.show_screen(window, Screen::Result);
    }

    /// Makes recovery media: the disc or drive this computer is started from
    /// when it will not start by itself.
    ///
    /// Three questions and then it runs: what kind, where, and are you sure.
    fn make_recovery_media(&mut self, window: &Window) {
        if let Some(running) = &self.worker {
            message_box::warn(window.raw(), "MjolnirVSS", running.describe());
            return;
        }

        // What MjolnirVSS can build depends on what Windows components this
        // computer has, so that is settled before anything is asked.
        let source = match mjolnir_media::best_source() {
            Ok(source) => source,
            Err(e) => {
                message_box::error_for(window.raw(), "MjolnirVSS", &e);
                return;
            }
        };

        let wants_iso = message_box::choose(
            window.raw(),
            "Recovery media",
            "What kind of recovery media?",
            "A file can be attached to a virtual machine or written to a disc later. A USB drive can be used straight away.",
            &format!(
                "MjolnirVSS builds recovery media from the Windows parts already on this computer, using {}. Nothing is downloaded, and no Microsoft file is copied anywhere except onto the media.",
                source.kind.describe()
            ),
            "Save a file (.iso)",
            "USB drive",
        );

        let Some(wants_iso) = wants_iso else { return };
        if !wants_iso {
            // Writing a USB drive erases it, and this version has not been
            // tested doing that. Saying so is better than doing it badly.
            message_box::info(
                window.raw(),
                "MjolnirVSS",
                "Writing a USB drive is not built yet.\n\nSave an ISO file instead, then write it to a USB drive with Microsoft's own MakeWinPEMedia, or with any tool that writes a bootable image. Writing a USB drive erases everything on it, which is why MjolnirVSS will not do it until it has been tested properly.",
            );
            return;
        }

        let Some(path) = shell::save_file(
            window.raw(),
            "Save the recovery image",
            "MjolnirVSS-Recovery.iso",
            "Disc image (*.iso)",
            "iso",
        ) else {
            return;
        };

        let going_ahead = message_box::confirm_with_details(
            window.raw(),
            "Recovery media",
            "Make recovery media?",
            &format!(
                "MjolnirVSS will write {}. It takes a few minutes and about 400 MB of space.",
                path.display()
            ),
            &format!(
                "Source: {}\r\nImage: {}\r\n\r\nThe Windows files on the result belong to Microsoft and are licensed to this computer. Keep the media, do not pass it on.\r\n\r\nNothing on this computer is changed, and no disk is erased.",
                source.kind.describe(),
                source.boot_image.display()
            ),
            "Make it",
        );
        if !going_ahead {
            return;
        }

        let release = match std::env::current_exe() {
            Ok(exe) => exe.parent().map(std::path::Path::to_path_buf),
            Err(_) => None,
        };
        let Some(release) = release else {
            message_box::warn(
                window.raw(),
                "MjolnirVSS",
                "MjolnirVSS could not find its own folder, so it does not know where the recovery program is.",
            );
            return;
        };

        let payload = match mjolnir_media::payload_from_release(&release) {
            Ok(payload) => payload,
            Err(e) => {
                message_box::error_for(window.raw(), "MjolnirVSS", &e);
                return;
            }
        };

        self.finished = None;
        let work = std::env::temp_dir();
        self.begin_job(Job::Media(Worker::start(move |progress, cancel| {
            let outcome =
                mjolnir_media::build_iso(&source, &payload, &path, &work, progress, cancel)?;
            let report = mjolnir_media::check_iso(&outcome.path)?;
            if !report.passed() {
                return Err(Error::new(
                    ExitCode::CorruptBackup,
                    "the recovery media was made but did not check out",
                    report
                        .failures()
                        .iter()
                        .map(|c| format!("{}: {}", c.what, c.detail))
                        .collect::<Vec<_>>()
                        .join("; "),
                    "try making it again; if it fails the same way, there may not be room on the drive",
                ));
            }
            Ok(outcome)
        })));

        self.show_screen(window, Screen::Progress);
        window.set_timer(TIMER_PROGRESS, TIMER_INTERVAL);
    }

    /// The result screen for a finished recovery media job.
    fn show_media_result(
        &mut self,
        window: &Window,
        result: std::result::Result<MediaOutcome, Error>,
    ) {
        let mut good = false;
        match &result {
            Ok(outcome) => {
                sys::set_text(self.controls.result_title, "Recovery media is ready");
                good = true;
                sys::set_text(
                    self.controls.result_note,
                    &format!("{} written and checked.", format_bytes(outcome.size_bytes)),
                );
                let mut body = format!(
                    "Saved\r\n{}\r\n\r\nSize\r\n{}\r\n\r\nChecked\r\nIt is a disc image, it is marked bootable, and it is the size it should be.",
                    outcome.path.display(),
                    format_bytes(outcome.size_bytes)
                );
                for note in &outcome.notes {
                    body.push_str(&format!("\r\n\r\n{note}"));
                }
                body.push_str(
                    "\r\n\r\nStart the broken computer from this media, then follow the recovery program. Try it once before you need it.",
                );
                sys::set_text(self.controls.result_body, &body);
                sys::enable(self.controls.open_folder, true);
            }
            Err(e) => {
                let (title, note) = if e.exit() == ExitCode::Cancelled {
                    ("Recovery media cancelled", "Nothing was left behind.")
                } else {
                    (
                        "Recovery media could not be made",
                        "Nothing on this computer was changed.",
                    )
                };
                sys::set_text(self.controls.result_title, title);
                sys::set_text(self.controls.result_note, note);
                sys::set_text(self.controls.result_body, &message_box::format_error(e));
                sys::enable(self.controls.open_folder, false);
            }
        }
        let tone = if good { Tone::Good } else { Tone::Bad };
        self.retone(self.controls.result_note, tone);
        self.finished = Some(JobResult::Media(result));
        self.show_screen(window, Screen::Result);
    }

    /// Opens a backup to look inside it.
    ///
    /// Three questions: which backup, which partition, and then the list. The
    /// tree is read on a worker, because a volume with a few hundred thousand
    /// files takes a few seconds and the window must stay usable.
    fn browse_backup(&mut self, window: &Window) {
        if let Some(running) = &self.worker {
            message_box::warn(window.raw(), "MjolnirVSS", running.describe());
            return;
        }

        let Ok(Some(folder)) = shell::pick_folder(window.raw(), "Choose the backup to look inside")
        else {
            return;
        };

        let set = match mjolnir_image::BackupSet::open(&folder) {
            Ok(set) => set,
            Err(e) => {
                message_box::error_for(window.raw(), "MjolnirVSS", &e);
                return;
            }
        };

        let volumes = mjolnir_files::volumes_in(&set);
        let readable: Vec<_> = volumes.iter().filter(|v| v.is_readable).collect();
        if readable.is_empty() {
            let why = volumes
                .iter()
                .filter_map(|v| v.why_not.clone())
                .collect::<Vec<_>>()
                .join("\r\n");
            message_box::info(
                window.raw(),
                "MjolnirVSS",
                &format!(
                    "There is nothing in this backup that MjolnirVSS can look inside.\n\n{why}"
                ),
            );
            return;
        }

        // Almost always one Windows partition. When there are two, the
        // operator is asked rather than guessed at.
        let chosen = if readable.len() == 1 {
            readable[0].clone()
        } else {
            let first = readable[0];
            let second = readable[1];
            match message_box::choose(
                window.raw(),
                "Restore files",
                "Which drive?",
                "A backup can hold more than one drive with files on it.",
                &volumes
                    .iter()
                    .map(|v| v.describe())
                    .collect::<Vec<_>>()
                    .join("\r\n"),
                &first.describe(),
                &second.describe(),
            ) {
                Some(true) => first.clone(),
                Some(false) => second.clone(),
                None => return,
            }
        };

        let name = chosen.describe();
        let stream_id = chosen.stream_id.clone();
        let backup = folder.clone();

        self.finished = None;
        let worker_backup = backup.clone();
        let worker_stream = stream_id.clone();
        self.begin_job(Job::Index(Worker::start(move |_progress, cancel| {
            let set = mjolnir_image::BackupSet::open(&worker_backup)?;
            let stream = mjolnir_files::stream_in(&set, &worker_stream)?;
            let open = mjolnir_files::OpenVolume::open(&set, stream, cancel)?;
            Ok(open.index().clone())
        })));

        self.pending_browse = Some((backup, stream_id, name));
        self.show_screen(window, Screen::Progress);
        window.set_timer(TIMER_PROGRESS, TIMER_INTERVAL);
    }

    /// Shows the contents of the directory the browser is in.
    fn show_browse(&mut self, window: &Window) {
        let Some(browsing) = &mut self.browsing else {
            return;
        };
        browsing.entries = browsing
            .index
            .children_of(browsing.at)
            .into_iter()
            .cloned()
            .collect();

        let path = browsing
            .index
            .path_of(browsing.at)
            .unwrap_or_else(|| "\\".to_owned());

        sys::set_text(self.controls.browse_volume, &browsing.volume_name.clone());
        sys::set_text(self.controls.browse_path, &path);
        sys::list_clear(self.controls.browse_list);
        let entries: Vec<String> = browsing.entries.iter().map(describe_entry).collect();
        let empty = entries.is_empty();
        for line in entries {
            sys::list_add(self.controls.browse_list, &line);
        }
        if !empty {
            sys::list_select(self.controls.browse_list, Some(0));
        }
        self.show_screen(window, Screen::Browse);
        window.focus(self.controls.browse_list);
    }

    /// Opens whatever is selected, when it is a directory.
    fn browse_open_selected(&mut self, window: &Window) {
        let Some(index) = sys::list_selected(self.controls.browse_list) else {
            return;
        };
        let Some(browsing) = &mut self.browsing else {
            return;
        };
        let Some(entry) = browsing.entries.get(index) else {
            return;
        };
        if !entry.is_directory {
            return;
        }
        browsing.at = entry.number;
        self.show_browse(window);
    }

    /// Goes to the directory above.
    fn browse_up(&mut self, window: &Window) {
        let Some(browsing) = &mut self.browsing else {
            return;
        };
        let Some(entry) = browsing.index.entry(browsing.at) else {
            return;
        };
        if entry.parent == browsing.at {
            return;
        }
        browsing.at = entry.parent;
        self.show_browse(window);
    }

    /// Copies what is selected out of the backup.
    fn browse_extract(&mut self, window: &Window) {
        if let Some(running) = &self.worker {
            message_box::warn(window.raw(), "MjolnirVSS", running.describe());
            return;
        }
        let Some(index) = sys::list_selected(self.controls.browse_list) else {
            message_box::info(
                window.raw(),
                "MjolnirVSS",
                "Choose a file or a folder in the list first.",
            );
            return;
        };
        let Some(browsing) = &self.browsing else {
            return;
        };
        let Some(entry) = browsing.entries.get(index).cloned() else {
            return;
        };

        let Ok(Some(into)) = shell::pick_folder(window.raw(), "Choose where to put the files")
        else {
            return;
        };

        let source = browsing
            .index
            .path_of(entry.number)
            .unwrap_or_else(|| entry.name.clone());
        let backup = browsing.backup.clone();
        let stream_id = browsing.stream_id.clone();

        self.finished = None;
        self.begin_job(Job::Extract(Worker::start(move |progress, cancel| {
            let set = mjolnir_image::BackupSet::open(&backup)?;
            let stream = mjolnir_files::stream_in(&set, &stream_id)?;
            let mut open = mjolnir_files::OpenVolume::open(&set, stream, cancel)?;
            let Some(entry) = open.index().resolve(&source).cloned() else {
                return Err(Error::new(
                    ExitCode::Failure,
                    "that file is no longer in the backup",
                    format!("{source} could not be found when it came to copying it"),
                    "open the backup again and try once more",
                ));
            };
            let options = mjolnir_files::ExtractOptions::default();
            if entry.is_directory {
                mjolnir_files::extract_tree(&mut open, &entry, &into, &options, progress, cancel)
            } else {
                let mut outcome = mjolnir_files::ExtractOutcome::default();
                for result in mjolnir_files::extract_file(
                    &mut open, &entry, &into, &options, progress, cancel,
                )? {
                    if let mjolnir_files::Extracted::Written { bytes, .. } = &result {
                        outcome.bytes_written += bytes;
                    }
                    outcome.files.push(result);
                }
                Ok(outcome)
            }
        })));

        self.show_screen(window, Screen::Progress);
        window.set_timer(TIMER_PROGRESS, TIMER_INTERVAL);
    }

    /// The result screen for a finished extraction.
    fn show_extract_result(
        &mut self,
        window: &Window,
        result: std::result::Result<mjolnir_files::ExtractOutcome, Error>,
    ) {
        let mut good = false;
        match &result {
            Ok(outcome) => {
                good = outcome.everything_worked();
                sys::set_text(
                    self.controls.result_title,
                    if outcome.everything_worked() {
                        "Files copied"
                    } else {
                        "Some files could not be copied"
                    },
                );
                sys::set_text(self.controls.result_note, &outcome.summary());
                let mut body = String::new();
                // Sixty lines is as much as the box shows before it becomes a
                // wall of text. The rest is counted rather than listed.
                const SHOW_AT_MOST: usize = 60;
                for file in outcome.files.iter().take(SHOW_AT_MOST) {
                    body.push_str(&format!("{}\r\n", file.describe()));
                }
                if outcome.files.len() > SHOW_AT_MOST {
                    body.push_str(&format!(
                        "\r\n...and {} more.",
                        outcome.files.len() - SHOW_AT_MOST
                    ));
                }
                sys::set_text(self.controls.result_body, &body);
                sys::enable(self.controls.open_folder, false);
            }
            Err(e) => {
                let (title, note) = if e.exit() == ExitCode::Cancelled {
                    ("Copying cancelled", "Nothing was left half written.")
                } else {
                    ("The files could not be copied", "The backup is unchanged.")
                };
                sys::set_text(self.controls.result_title, title);
                sys::set_text(self.controls.result_note, note);
                sys::set_text(self.controls.result_body, &message_box::format_error(e));
                sys::enable(self.controls.open_folder, false);
            }
        }
        let tone = if good { Tone::Good } else { Tone::Caution };
        self.retone(self.controls.result_note, tone);
        self.finished = Some(JobResult::Extract(result));
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
            sys::set_text(self.controls.prog_stage, "Stopping…");
            sys::set_text(self.controls.cancel, "Cancelling");
            sys::enable(self.controls.cancel, false);
            sys::set_progress_state(self.controls.progress, ProgressState::Paused);
        }
    }
}

impl WindowHandler for BackupWindow {
    fn on_create(&mut self, window: &Window) {
        // The window opens on the backup section, so the plan is built before
        // it is shown: there is no menu click left to trigger it, and the one
        // pane whose job is to say what would be copied must not open empty.
        self.begin_backup_setup(window);
    }

    fn on_layout(&mut self, window: &Window) {
        self.layout(window);
    }

    fn on_paint(&mut self, _window: &Window, canvas: &Canvas) {
        self.paint(canvas);
    }

    fn on_theme_changed(&mut self, _window: &Window) {
        let palette = Palette::current();
        for (hwnd, tone) in &self.tones {
            let (foreground, background) = tone.colours(&palette);
            set_label_colours(*hwnd, foreground, background);
        }
        self.mark_selected_section(self.screen.section());
    }

    fn on_timer(&mut self, window: &Window, id: usize) {
        if id == TIMER_PROGRESS {
            self.tick(window);
        }
    }

    fn on_command(&mut self, window: &Window, id: i32, notification: u32) {
        match (id, notification) {
            // Re-planned on every visit: a drive may have been plugged in or
            // taken away since the window opened, and a stale plan is worse
            // than a slow one.
            (ID_NAV_BACKUP, _) => self.begin_backup_setup(window),
            (ID_NAV_MEDIA, _) => self.show_screen(window, Screen::Media),
            (ID_MAKE_MEDIA, _) => self.make_recovery_media(window),
            (ID_NAV_RESTORE, _) => self.browse_backup(window),
            (ID_BROWSE_OPEN, _) | (ID_BROWSE_LIST, LBN_DBLCLK) => self.browse_open_selected(window),
            (ID_BROWSE_UP, _) => self.browse_up(window),
            (ID_BROWSE_EXTRACT, _) => self.browse_extract(window),
            (ID_BROWSE_BACK, _) => {
                self.browsing = None;
                self.show_screen(window, Screen::Backup);
            }
            (ID_NAV_SETTINGS, _) => self.show_screen(window, Screen::Settings),
            (ID_CLOSE, _) => window.request_close(),
            (ID_DEST_BROWSE, _) => self.browse_for_folder(window),
            (ID_DEST_EDIT, EN_CHANGE) => self.update_space_label(window),
            (ID_START, _) => self.start_backup(window),
            (ID_CANCEL, _) => self.request_cancel(),
            (ID_DETAILS_TOGGLE, _) => self.toggle_details(window),
            (ID_OPEN_FOLDER, _) => match &self.finished {
                Some(JobResult::Backup(Ok(outcome))) => {
                    shell::open_in_explorer(&outcome.backup_dir);
                }
                Some(JobResult::Media(Ok(outcome))) => {
                    if let Some(folder) = outcome.path.parent() {
                        shell::open_in_explorer(folder);
                    }
                }
                _ => {}
            },
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
        let mut ids = vec![
            ID_NAV_BACKUP,
            ID_NAV_RESTORE,
            ID_NAV_MEDIA,
            ID_NAV_SETTINGS,
            ID_BRAND,
            ID_BRAND_VERSION,
            ID_RAIL_FOOTER,
            ID_PAGE_TITLE,
            ID_PAGE_SUBTITLE,
            ID_DISK_OVERLINE,
            ID_DISK_NAME,
            ID_DISK_META,
            ID_DATA_OVERLINE,
            ID_DATA_FIGURE,
            ID_PART_LEGEND,
            ID_NOTES,
            ID_DEST_LABEL,
            ID_DEST_EDIT,
            ID_DEST_BROWSE,
            ID_NAME_LABEL,
            ID_NAME_EDIT,
            ID_SPACE,
            ID_START,
            ID_REASSURANCE,
            ID_PROG_HEADING,
            ID_PROG_STAGE,
            ID_PROG_PERCENT,
            ID_PROGRESS,
            ID_CANCEL,
            ID_DETAILS_TOGGLE,
            ID_DETAILS,
            ID_RESULT_TITLE,
            ID_RESULT_NOTE,
            ID_RESULT_BODY,
            ID_OPEN_FOLDER,
            ID_MAKE_MEDIA,
            ID_CLOSE,
            ID_BROWSE_VOLUME,
            ID_BROWSE_PATH,
            ID_BROWSE_LIST,
            ID_BROWSE_OPEN,
            ID_BROWSE_UP,
            ID_BROWSE_EXTRACT,
            ID_BROWSE_BACK,
            ID_MEDIA_BODY,
            ID_MEDIA_REQUIREMENT,
            ID_SETTINGS_BODY,
        ];
        for index in 0..STATS as i32 {
            ids.push(ID_STAT_KEY + index);
            ids.push(ID_STAT_VALUE + index);
        }
        for index in 0..STAGES as i32 {
            ids.push(ID_STAGE_ROW + index);
        }
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(before, ids.len(), "two controls share an identifier");
    }

    #[test]
    fn the_default_backup_name_is_usable_as_a_folder() {
        let name = default_name();
        assert!(BackupName::new(name.as_str()).is_ok(), "{name}");
    }

    /// The rail lists the sections and nothing else. Anything that is not one
    /// of these four is content, and belongs in the pane on the right.
    #[test]
    fn the_rail_has_exactly_four_sections() {
        assert_eq!(Controls::default().sections().len(), 4);
    }

    /// Every screen shows the rail, so there is always a way to every other
    /// section. A screen that hid it would strand somebody on it.
    #[test]
    fn the_rail_is_on_every_screen() {
        let c = Controls::default();
        for screen in [
            Screen::Backup,
            Screen::Progress,
            Screen::Result,
            Screen::Browse,
            Screen::Media,
            Screen::Settings,
        ] {
            let shown = c.for_screen(screen);
            for hwnd in c.always_visible() {
                assert!(shown.contains(&hwnd), "{screen:?} hides part of the rail");
            }
        }
    }

    /// Progress and Result belong to the section that started them, so the rail
    /// does not appear to jump elsewhere while a backup runs.
    #[test]
    fn a_running_job_keeps_the_section_it_started_from() {
        assert_eq!(Screen::Progress.section(), Screen::Backup);
        assert_eq!(Screen::Result.section(), Screen::Backup);
        assert_eq!(Screen::Browse.section(), Screen::Browse);
        assert_eq!(Screen::Settings.section(), Screen::Settings);
    }

    /// Headings are sentence case and carry no ending punctuation, which is
    /// what Microsoft's own interface text guidance asks for.
    #[test]
    fn every_heading_reads_like_a_heading() {
        for screen in [
            Screen::Backup,
            Screen::Browse,
            Screen::Media,
            Screen::Settings,
        ] {
            let title = screen.title();
            assert!(!title.is_empty());
            assert!(
                !title.ends_with('.') && !title.ends_with(':'),
                "a heading should not end in punctuation: {title:?}"
            );
            // Sentence case: only the first word is capitalised. Acronyms and
            // proper nouns keep their own capitals, so they are allowed.
            for word in title.split_whitespace().skip(1) {
                let acronym = word.chars().all(|c| c.is_uppercase() || !c.is_alphabetic());
                let proper = ["Windows", "BitLocker", "MjolnirVSS"].contains(&word);
                assert!(
                    acronym || proper || !word.starts_with(char::is_uppercase),
                    "headings are sentence case, not title case: {title:?}"
                );
            }
        }
    }

    /// Every section says what it is for in one sentence. An empty subtitle
    /// leaves a gap where the hierarchy should be.
    #[test]
    fn every_section_explains_itself_in_a_sentence() {
        for screen in [
            Screen::Backup,
            Screen::Browse,
            Screen::Media,
            Screen::Settings,
        ] {
            let subtitle = screen.subtitle();
            assert!(!subtitle.is_empty(), "{screen:?}");
            assert!(subtitle.ends_with('.'), "{screen:?}: {subtitle:?}");
        }
    }

    /// Text on the rail is drawn in rail colours and text on a card in card
    /// colours. Getting one wrong leaves a label that is there but invisible,
    /// which is the hardest kind of mistake to notice in a screenshot.
    #[test]
    fn every_tone_puts_readable_text_on_its_own_background() {
        let palette = Palette::current();
        for tone in [
            Tone::OnRail,
            Tone::OnRailDim,
            Tone::TitleOnPage,
            Tone::OnPage,
            Tone::DimOnPage,
            Tone::TitleOnCard,
            Tone::OnCard,
            Tone::DimOnCard,
            Tone::Good,
            Tone::Caution,
            Tone::Bad,
        ] {
            let (text, background) = tone.colours(&palette);
            assert_ne!(text.0, background.0, "{tone:?} is invisible");
        }
    }

    /// Rail tones sit on the rail and card tones on a card. A card tone used on
    /// the rail would draw a white rectangle in the middle of the navy.
    #[test]
    fn tones_name_the_surface_they_belong_to() {
        let palette = Palette::current();
        for tone in [Tone::OnRail, Tone::OnRailDim] {
            assert_eq!(tone.colours(&palette).1 .0, palette.nav.0, "{tone:?}");
        }
        for tone in [Tone::TitleOnCard, Tone::OnCard, Tone::DimOnCard] {
            assert_eq!(tone.colours(&palette).1 .0, palette.card.0, "{tone:?}");
        }
        for tone in [Tone::TitleOnPage, Tone::OnPage, Tone::DimOnPage] {
            assert_eq!(tone.colours(&palette).1 .0, palette.page.0, "{tone:?}");
        }
    }

    /// Every job names its stages, and the progress screen has a row for each.
    /// One short list would leave a row blank, which reads as a stage that does
    /// not exist.
    #[test]
    fn every_job_names_exactly_as_many_stages_as_there_are_rows() {
        // The variants cannot be built without starting a worker, so the arrays
        // are checked through the one thing that is knowable statically: they
        // are fixed size, and that size is the number of rows laid out.
        assert_eq!(STAGES, 4);
        assert_eq!(STATS, 4);
    }
}
