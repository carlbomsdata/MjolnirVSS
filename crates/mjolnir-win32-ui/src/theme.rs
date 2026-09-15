//! The MjolnirVSS visual design system: colours, spacing and type.
//!
//! This module holds the numbers, and nothing else holds them. A screen that
//! wants a margin asks [`Metrics`] for one rather than inventing a coordinate,
//! which is what keeps two screens looking like the same program.
//!
//! # Why a palette at all
//!
//! Native controls come with the system's own colours, and for buttons, text
//! boxes and lists that is exactly right: they follow the user's theme, their
//! focus rectangles are drawn the way that user recognises, and a screen reader
//! and a high contrast theme both work without anything here being involved.
//!
//! What the system does not provide is a *surface*. A navigation rail, a card
//! and a page background are painted by the application, and painting them in
//! `COLOR_BTNFACE` is what made the window look like a utility from 1998. Those
//! surfaces are what this palette is for.
//!
//! # High contrast
//!
//! When the user has asked Windows for a high contrast theme, the brand palette
//! is abandoned entirely and every colour comes from the system. A navy rail
//! that ignored that setting would be unreadable for the person who chose it,
//! and being pretty is not worth that. [`Palette::current`] makes the decision
//! once; callers never test for it themselves.

use std::cell::RefCell;

use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, GetSysColor, COLOR_BTNFACE, COLOR_BTNTEXT, COLOR_GRAYTEXT,
    COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT, HFONT, LOGFONTW,
};
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

use crate::sys;

/// Builds a `COLORREF` from red, green and blue.
///
/// Win32 stores colours as `0x00BBGGRR`, which is the reverse of how they are
/// written down, so this exists to stop that being got wrong by hand.
pub const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

/// Every colour a screen is allowed to use.
///
/// Obtained from [`Palette::current`], never constructed by a caller: which set
/// of colours is right depends on the user's accessibility settings, and that
/// decision belongs in one place.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Whether these colours came from a high contrast theme.
    ///
    /// Screens use it to drop decoration that only makes sense in the brand
    /// palette, such as a coloured selection bar behind a navigation item.
    pub high_contrast: bool,

    /// The navigation rail.
    pub nav: COLORREF,
    /// The rail's own header, one step darker.
    pub nav_deep: COLORREF,
    /// Text on the rail.
    pub nav_text: COLORREF,
    /// Text on the rail that is secondary.
    pub nav_text_dim: COLORREF,
    /// The selected navigation item's background.
    pub nav_selected: COLORREF,
    /// A navigation item under the pointer.
    pub nav_hover: COLORREF,
    /// The accent stripe beside the selected navigation item.
    pub nav_marker: COLORREF,

    /// The page behind the cards.
    pub page: COLORREF,
    /// A card or panel.
    pub card: COLORREF,
    /// A card's border.
    pub card_border: COLORREF,
    /// A hairline divider.
    pub divider: COLORREF,

    /// Headings and primary text.
    pub ink: COLORREF,
    /// Ordinary body text.
    pub body: COLORREF,
    /// Secondary and supporting text.
    pub muted: COLORREF,

    /// The accent, used for the primary action and for emphasis.
    pub accent: COLORREF,
    /// The accent when the pointer is over it.
    pub accent_hover: COLORREF,
    /// The accent when pressed.
    pub accent_press: COLORREF,
    /// Text on top of the accent.
    pub on_accent: COLORREF,

    /// Something finished correctly.
    pub success: COLORREF,
    /// Something needs attention but is not a failure.
    pub warning: COLORREF,
    /// Something destructive or failed.
    pub danger: COLORREF,
    /// A very light wash behind a danger message.
    pub danger_wash: COLORREF,
    /// A very light wash behind a warning message.
    pub warning_wash: COLORREF,
    /// A very light wash behind an informational message.
    pub accent_wash: COLORREF,
}

/// The brand palette, used whenever high contrast is off.
const BRAND: Palette = Palette {
    high_contrast: false,

    nav: rgb(0x0A, 0x1A, 0x2D),
    nav_deep: rgb(0x05, 0x10, 0x1D),
    nav_text: rgb(0xE8, 0xEE, 0xF5),
    nav_text_dim: rgb(0x8A, 0xA0, 0xB6),
    nav_selected: rgb(0x16, 0x32, 0x4F),
    nav_hover: rgb(0x10, 0x27, 0x41),
    nav_marker: rgb(0x5A, 0xA9, 0xF0),

    page: rgb(0xF5, 0xF7, 0xFA),
    card: rgb(0xFF, 0xFF, 0xFF),
    card_border: rgb(0xE2, 0xE7, 0xEC),
    divider: rgb(0xEA, 0xEE, 0xF2),

    ink: rgb(0x0E, 0x16, 0x21),
    body: rgb(0x4A, 0x55, 0x60),
    muted: rgb(0x6C, 0x77, 0x84),

    accent: rgb(0x0F, 0x5B, 0xA8),
    accent_hover: rgb(0x12, 0x69, 0xBD),
    accent_press: rgb(0x0B, 0x43, 0x80),
    on_accent: rgb(0xFF, 0xFF, 0xFF),

    success: rgb(0x0F, 0x7B, 0x3F),
    warning: rgb(0x8A, 0x54, 0x00),
    danger: rgb(0xB4, 0x26, 0x1B),
    danger_wash: rgb(0xFD, 0xF3, 0xF2),
    warning_wash: rgb(0xFD, 0xF7, 0xEC),
    accent_wash: rgb(0xEF, 0xF5, 0xFB),
};

impl Palette {
    /// The palette to paint with right now.
    ///
    /// Returns the brand colours normally and a palette built entirely from
    /// system colours when the user has a high contrast theme, because a fixed
    /// navy is exactly what that setting exists to get rid of.
    pub fn current() -> Self {
        if high_contrast() {
            Self::from_system()
        } else {
            BRAND
        }
    }

    /// A palette built from the system's own colours.
    fn from_system() -> Self {
        // SAFETY: GetSysColor takes an index and returns a value. No pointers.
        let sys_colour = |index| COLORREF(unsafe { GetSysColor(index) });
        let window = sys_colour(COLOR_WINDOW);
        let window_text = sys_colour(COLOR_WINDOWTEXT);
        let face = sys_colour(COLOR_BTNFACE);
        let face_text = sys_colour(COLOR_BTNTEXT);
        let highlight = sys_colour(COLOR_HIGHLIGHT);
        let highlight_text = sys_colour(COLOR_HIGHLIGHTTEXT);
        let grey = sys_colour(COLOR_GRAYTEXT);

        Self {
            high_contrast: true,

            nav: face,
            nav_deep: face,
            nav_text: face_text,
            nav_text_dim: face_text,
            nav_selected: highlight,
            nav_hover: highlight,
            nav_marker: highlight,

            page: window,
            card: window,
            card_border: window_text,
            divider: window_text,

            ink: window_text,
            body: window_text,
            // Grey text in a high contrast theme is the theme's own grey, which
            // the user has chosen to be legible. It is never invented here.
            muted: grey,

            accent: highlight,
            accent_hover: highlight,
            accent_press: highlight,
            on_accent: highlight_text,

            success: window_text,
            warning: window_text,
            danger: window_text,
            danger_wash: window,
            warning_wash: window,
            accent_wash: window,
        }
    }
}

/// Whether Windows is running a high contrast theme.
///
/// Checked on every paint rather than cached, because the user can turn it on
/// with Left Alt + Left Shift + Print Screen while the window is open, and a
/// cached answer would leave them looking at an unreadable window.
pub fn high_contrast() -> bool {
    let mut info = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    // SAFETY: the structure is sized correctly and is a live local for the
    // duration of the call, which is the contract SystemParametersInfoW states
    // for SPI_GETHIGHCONTRAST. A failure leaves the zeroed flags, which read as
    // high contrast being off, and that is the right thing to assume when the
    // question could not be answered.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            std::mem::size_of::<HIGHCONTRASTW>() as u32,
            Some(&mut info as *mut _ as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    ok.is_ok() && (info.dwFlags.0 & HCF_HIGHCONTRASTON.0) != 0
}

/// Spacing, sizes and other measurements, in device independent pixels.
///
/// Every value is designed at 96 dpi and scaled through [`Metrics::at`]. The
/// point of gathering them is that a screen reads as a sequence of named steps
/// rather than a list of coordinates nobody can check.
pub struct Metrics;

impl Metrics {
    /// The unit every measurement is a multiple of.
    ///
    /// Four, because Windows' own scaling plateaus (125%, 150%, 175% and so on)
    /// turn a multiple of four into a whole number of pixels at every one of
    /// them. Anything else lands on a half pixel and looks soft on one machine
    /// and crisp on another.
    pub const UNIT: i32 = 4;

    /// Inside a pair that reads as one thing: a value under its own heading.
    pub const SPACE_XS: i32 = 4;
    /// Between a label and the control it names.
    pub const SPACE_S: i32 = 8;
    /// Between two related controls.
    pub const SPACE_M: i32 = 12;
    /// Between two groups of controls.
    pub const SPACE_L: i32 = 20;
    /// Between two sections of a page.
    pub const SPACE_XL: i32 = 28;

    /// The width of a command button.
    ///
    /// There are two button widths in the whole application and no others.
    /// Windows' own guidance is to use one or two on a surface, and the reason
    /// is visible the moment there are nine: a row of buttons with nothing in
    /// common looks like a row of accidents.
    pub const BUTTON_WIDTH: i32 = 120;
    /// The width of a primary action, or of a button whose label will not fit
    /// in [`Self::BUTTON_WIDTH`].
    pub const BUTTON_WIDTH_WIDE: i32 = 184;
    /// Between two buttons side by side.
    pub const BUTTON_GAP: i32 = 8;

    /// The box one line of caption text occupies.
    pub const LINE_CAPTION: i32 = 16;
    /// The box one line of body text occupies.
    pub const LINE_BODY: i32 = 20;
    /// The box one line of heading text occupies.
    pub const LINE_HEADING: i32 = 24;
    /// The box a page title occupies.
    pub const LINE_TITLE: i32 = 32;
    /// The box a large figure occupies.
    pub const LINE_FIGURE: i32 = 40;

    /// Width of the navigation rail.
    pub const NAV_WIDTH: i32 = 224;
    /// Height of one navigation item.
    pub const NAV_ITEM: i32 = 40;
    /// Gap between navigation items.
    pub const NAV_ITEM_GAP: i32 = 2;
    /// Height of the branding block at the top of the rail.
    pub const NAV_HEADER: i32 = 72;
    /// Inset of a navigation item from the rail's edges.
    pub const NAV_INSET: i32 = 8;
    /// Width of the accent stripe marking the selected navigation item.
    pub const NAV_MARKER: i32 = 3;

    /// Margin between the content and the edge of its pane.
    pub const PAGE_MARGIN: i32 = 28;
    /// Padding inside a card.
    pub const CARD_PADDING: i32 = 20;
    /// Vertical gap between two sections of a page.
    pub const SECTION_GAP: i32 = Self::SPACE_L;
    /// Vertical gap between a label and the control it names.
    pub const LABEL_GAP: i32 = Self::SPACE_S;
    /// Gap between two controls on the same row.
    pub const CONTROL_GAP: i32 = Self::SPACE_S;
    /// Corner radius of a card or a painted button.
    pub const RADIUS: i32 = 6;

    /// Height of a push button.
    pub const BUTTON_HEIGHT: i32 = 32;
    /// Height of a text box or other single line input.
    ///
    /// Four short of a button on purpose: an input is not a command, and
    /// Windows' own dialogs make the same distinction.
    pub const INPUT_HEIGHT: i32 = 28;
    /// Height of a progress bar.
    pub const PROGRESS_HEIGHT: i32 = 8;
    /// Height of one line of body text.
    pub const LINE: i32 = Self::LINE_BODY;
    /// Height of the title block at the top of a page.
    pub const PAGE_TITLE: i32 = Self::LINE_TITLE;
    /// Height of the supporting line under a page title.
    pub const PAGE_SUBTITLE: i32 = Self::LINE_BODY;
    /// Height of the bar showing a disk's partitions.
    pub const PARTITION_BAR: i32 = 28;
    /// Height of one entry in a list of stages.
    pub const STAGE_ROW: i32 = 28;

    /// Scales a design value to a window's actual dpi.
    pub fn at(value: i32, dpi: u32) -> i32 {
        sys::scale(value, dpi)
    }
}

// Anything a person has to hit and read has a floor under its size. Windows'
// own guidance puts a button at 32 device independent pixels, and these are
// checked when the file is compiled rather than when a test is run, so shrinking
// one to make a screen fit fails the build instead of the layout.
const _: () = assert!(
    Metrics::BUTTON_HEIGHT >= 32,
    "a button smaller than 32 units is hard to hit and hard to read"
);
const _: () = assert!(
    Metrics::INPUT_HEIGHT >= 28,
    "a text box smaller than 28 units crowds the text inside it"
);
const _: () = assert!(
    Metrics::NAV_ITEM >= 32,
    "a navigation item smaller than 32 units is hard to hit"
);
const _: () = assert!(
    Metrics::NAV_WIDTH > Metrics::NAV_ITEM,
    "the rail has to be wider than one of its items is tall"
);
const _: () = assert!(
    Metrics::BUTTON_WIDTH_WIDE > Metrics::BUTTON_WIDTH,
    "the wide button is the wider of the two"
);

// The spacing scale has to be a scale: every step bigger than the last, and
// every one a whole number of units, or it is just eight arbitrary numbers.
const _: () = assert!(Metrics::SPACE_XS < Metrics::SPACE_S);
const _: () = assert!(Metrics::SPACE_S < Metrics::SPACE_M);
const _: () = assert!(Metrics::SPACE_M < Metrics::SPACE_L);
const _: () = assert!(Metrics::SPACE_L < Metrics::SPACE_XL);
const _: () = assert!(Metrics::SPACE_XS % Metrics::UNIT == 0);
const _: () = assert!(Metrics::SPACE_S % Metrics::UNIT == 0);
const _: () = assert!(Metrics::SPACE_M % Metrics::UNIT == 0);
const _: () = assert!(Metrics::SPACE_L % Metrics::UNIT == 0);
const _: () = assert!(Metrics::SPACE_XL % Metrics::UNIT == 0);
const _: () = assert!(Metrics::BUTTON_WIDTH % Metrics::UNIT == 0);
const _: () = assert!(Metrics::BUTTON_WIDTH_WIDE % Metrics::UNIT == 0);
const _: () = assert!(Metrics::BUTTON_HEIGHT % Metrics::UNIT == 0);
const _: () = assert!(Metrics::INPUT_HEIGHT % Metrics::UNIT == 0);
const _: () = assert!(Metrics::NAV_WIDTH % Metrics::UNIT == 0);
const _: () = assert!(Metrics::NAV_ITEM % Metrics::UNIT == 0);
const _: () = assert!(Metrics::PAGE_MARGIN % Metrics::UNIT == 0);

// Every line box is a whole number of units too, so two of them stacked land on
// the grid rather than half a unit off it.
const _: () = assert!(Metrics::LINE_CAPTION % Metrics::UNIT == 0);
const _: () = assert!(Metrics::LINE_BODY % Metrics::UNIT == 0);
const _: () = assert!(Metrics::LINE_HEADING % Metrics::UNIT == 0);
const _: () = assert!(Metrics::LINE_TITLE % Metrics::UNIT == 0);
const _: () = assert!(Metrics::LINE_FIGURE % Metrics::UNIT == 0);
const _: () = assert!(Metrics::LINE_CAPTION < Metrics::LINE_BODY);
const _: () = assert!(Metrics::LINE_BODY < Metrics::LINE_HEADING);
const _: () = assert!(Metrics::LINE_HEADING < Metrics::LINE_TITLE);
const _: () = assert!(Metrics::LINE_TITLE < Metrics::LINE_FIGURE);

/// Which of the application's type styles a piece of text is.
///
/// Every one is derived from the shell's own message font, so the user's chosen
/// font and text size are respected. Nothing here names a typeface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextStyle {
    /// The product's name, in the navigation rail.
    Brand,
    /// A page title, at the top of the content pane.
    Title,
    /// The supporting sentence under a page title.
    Subtitle,
    /// A heading inside a card.
    Heading,
    /// Ordinary text.
    Body,
    /// Text that carries more weight than its neighbours.
    Strong,
    /// A large figure, such as a percentage.
    Figure,
    /// Small supporting text.
    Caption,
    /// A small heading over a value, in capitals.
    Overline,
}

impl TextStyle {
    /// How much larger or smaller than the message font this style is.
    fn scale(self) -> (i32, i32) {
        match self {
            TextStyle::Brand => (23, 20),
            TextStyle::Title => (29, 20),
            TextStyle::Subtitle => (23, 20),
            TextStyle::Heading => (22, 20),
            TextStyle::Body => (20, 20),
            TextStyle::Strong => (20, 20),
            TextStyle::Figure => (48, 20),
            TextStyle::Caption => (19, 20),
            TextStyle::Overline => (18, 20),
        }
    }

    /// The stroke weight, in the units `LOGFONTW` uses.
    fn weight(self) -> i32 {
        match self {
            // 600 is semibold. Windows picks the nearest weight the family has,
            // so on a font without one this lands on bold, which is still the
            // right relationship to the text around it.
            TextStyle::Brand | TextStyle::Title | TextStyle::Figure => 600,
            TextStyle::Heading | TextStyle::Strong | TextStyle::Overline => 600,
            TextStyle::Subtitle | TextStyle::Body | TextStyle::Caption => 400,
        }
    }

    /// Whether the text is drawn in capitals with the letters spaced apart.
    pub fn is_overline(self) -> bool {
        self == TextStyle::Overline
    }
}

thread_local! {
    /// Fonts built so far, keyed by style and dpi.
    ///
    /// A small vector rather than a map: there are nine styles and rarely more
    /// than two dpi values in one session, so a linear scan is the cheaper of
    /// the two and needs no hashing.
    static FONTS: RefCell<Vec<(TextStyle, u32, HFONT)>> = const { RefCell::new(Vec::new()) };
}

/// The font for a style at a given dpi, built once and reused.
pub fn font(style: TextStyle, dpi: u32) -> HFONT {
    FONTS.with(|cell| {
        let mut cache = cell.borrow_mut();
        if let Some((_, _, font)) = cache
            .iter()
            .find(|(s, d, _)| *s == style && *d == dpi)
            .copied()
        {
            return font;
        }

        let mut logfont: LOGFONTW = sys::message_logfont(dpi);
        let (numerator, denominator) = style.scale();
        // lfHeight is negative for a character height rather than a cell
        // height, which is the form the shell's own font arrives in. Scaling it
        // keeps the sign, so the arithmetic is the same either way.
        logfont.lfHeight = (logfont.lfHeight * numerator) / denominator;
        logfont.lfWeight = style.weight();

        // SAFETY: the structure describes a valid font request and is a live
        // local for the call. A failure returns a null handle, which Windows
        // treats as "use the system font", so the text is still drawn.
        let font = unsafe { CreateFontIndirectW(&logfont) };
        cache.push((style, dpi, font));
        font
    })
}

/// Releases every cached font. Called once, as the application exits.
pub fn release_fonts() {
    FONTS.with(|cell| {
        for (_, _, font) in cell.borrow_mut().drain(..) {
            if font.is_invalid() {
                continue;
            }
            // SAFETY: each font was created by CreateFontIndirectW above and is
            // owned here. This runs after every window has been destroyed, so
            // no control still has one selected.
            unsafe {
                let _ = DeleteObject(font.into());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_are_stored_the_way_win32_wants_them() {
        // 0x00BBGGRR, not the 0xRRGGBB a person writes down.
        assert_eq!(rgb(0x12, 0x34, 0x56).0, 0x00563412);
        assert_eq!(rgb(0, 0, 0).0, 0);
        assert_eq!(rgb(255, 255, 255).0, 0x00FFFFFF);
    }

    /// The rail is dark and its text is light. Getting this the wrong way round
    /// produces a window nobody can read, and it is exactly the kind of thing
    /// that survives a screenshot taken on the developer's own machine.
    #[test]
    fn the_navigation_rail_is_dark_with_light_text() {
        let luminance = |c: COLORREF| {
            let r = (c.0 & 0xFF) as f64;
            let g = ((c.0 >> 8) & 0xFF) as f64;
            let b = ((c.0 >> 16) & 0xFF) as f64;
            0.2126 * r + 0.7152 * g + 0.0722 * b
        };
        assert!(luminance(BRAND.nav) < 60.0, "the rail should be dark");
        assert!(
            luminance(BRAND.nav_text) > 180.0,
            "text on the rail should be light"
        );
        assert!(
            luminance(BRAND.card) > 200.0,
            "the content surface should be light"
        );
        assert!(luminance(BRAND.ink) < 60.0, "text on a card should be dark");
    }

    /// Nothing may draw the accent's own colour as text on top of itself.
    #[test]
    fn text_on_the_accent_contrasts_with_it() {
        assert_ne!(BRAND.accent.0, BRAND.on_accent.0);
    }

    #[test]
    fn every_text_style_has_a_sane_size_and_weight() {
        for style in [
            TextStyle::Brand,
            TextStyle::Title,
            TextStyle::Subtitle,
            TextStyle::Heading,
            TextStyle::Body,
            TextStyle::Strong,
            TextStyle::Figure,
            TextStyle::Caption,
            TextStyle::Overline,
        ] {
            let (numerator, denominator) = style.scale();
            assert!(numerator > 0 && denominator > 0, "{style:?}");
            // Nothing smaller than 90% of the body size: below that it stops
            // being readable for the people most likely to be using this.
            assert!(
                numerator * 100 / denominator >= 90,
                "{style:?} is too small"
            );
            assert!((100..=900).contains(&style.weight()), "{style:?}");
        }
    }

    /// Body text is the reference every other size is relative to.
    #[test]
    fn body_text_is_exactly_the_system_size() {
        assert_eq!(TextStyle::Body.scale(), (20, 20));
    }

    /// A title has to be visibly larger than the text under it, or the hierarchy
    /// the design depends on is not there at all.
    #[test]
    fn a_title_is_larger_than_body_text() {
        let size = |s: TextStyle| {
            let (n, d) = s.scale();
            n * 1000 / d
        };
        assert!(size(TextStyle::Title) > size(TextStyle::Subtitle));
        assert!(size(TextStyle::Subtitle) >= size(TextStyle::Body));
        assert!(size(TextStyle::Body) > size(TextStyle::Caption));
        assert!(size(TextStyle::Figure) > size(TextStyle::Title));
    }

    #[test]
    fn metrics_scale_with_the_display() {
        assert_eq!(Metrics::at(Metrics::NAV_WIDTH, 96), Metrics::NAV_WIDTH);
        assert_eq!(Metrics::at(Metrics::NAV_WIDTH, 192), Metrics::NAV_WIDTH * 2);
    }

    /// The palette a high contrast theme is not in use returns is the brand one,
    /// and it must not claim otherwise: everything that drops decoration keys
    /// off that flag.
    #[test]
    fn the_brand_palette_does_not_claim_to_be_a_high_contrast_one() {
        assert!(!Palette::current().high_contrast || high_contrast());
    }
}
