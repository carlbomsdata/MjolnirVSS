//! Safe drawing, for the surfaces underneath the native controls.
//!
//! [`Canvas`] wraps a device context so a screen can paint a card, a divider or
//! a heading without writing an `unsafe` block or leaking a GDI object. Every
//! method creates what it needs, selects it, draws, puts back what was there and
//! deletes what it made, which is the part that is easy to get wrong by hand.
//!
//! # What is painted and what is not
//!
//! Only surfaces: page backgrounds, cards, dividers, headings, icons, the
//! navigation rail and the bars that visualise a disk. Every control a person
//! operates stays a real Win32 control, because that is what carries keyboard
//! behaviour, focus rectangles, high contrast support and accessibility.
//!
//! The one exception is the owner drawn button, and it is still a real `BUTTON`
//! window: Windows handles the keyboard, the focus and the reporting, and asks
//! this module only for the pixels. See [`crate::sys::ControlKind`].
//!
//! # Windows PE
//!
//! Everything here is plain GDI from `gdi32.dll`, which exists in every Windows
//! PE image. Nothing reaches for GDI+, Direct2D or the desktop window manager,
//! none of which can be relied on inside a recovery environment.

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    CreatePen, CreateSolidBrush, DeleteObject, DrawFocusRect, DrawTextW, FillRect, LineTo,
    MoveToEx, Polygon, RoundRect, SelectObject, SetBkMode, SetTextColor, DRAW_TEXT_FORMAT,
    DT_CALCRECT, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE,
    DT_VCENTER, DT_WORDBREAK, HDC, HFONT, HGDIOBJ, PS_SOLID, TRANSPARENT,
};

use crate::sys;
use crate::theme::{font, Metrics, Palette, TextStyle};

/// How a piece of text sits inside the rectangle it is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// Against the left edge, vertically centred.
    Left,
    /// Centred both ways.
    Centre,
    /// Against the right edge, vertically centred.
    Right,
    /// Against the left edge, wrapping onto as many lines as it needs, starting
    /// at the top.
    Wrap,
}

impl Align {
    fn flags(self) -> DRAW_TEXT_FORMAT {
        match self {
            Align::Left => DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            Align::Centre => DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            Align::Right => DT_RIGHT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            Align::Wrap => DT_LEFT | DT_WORDBREAK,
        }
    }
}

/// Which small picture to draw.
///
/// Drawn from lines and polygons rather than loaded from a font or a resource,
/// so they look the same at any scale and need nothing to be installed. A
/// recovery environment has neither an icon font nor a guarantee about which
/// typefaces are present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    /// The product's mark: a hammer head over a shaft, inside a rounded square.
    Brand,
    /// A disk, for backing this computer up.
    Disk,
    /// A document, for recovering single files.
    Document,
    /// A disc, for recovery media.
    Disc,
    /// Three sliders, for settings.
    Sliders,
    /// A tick, for a stage that is done.
    Tick,
    /// A filled dot, for the stage running now.
    Dot,
    /// An open circle, for a stage not started.
    Ring,
    /// An exclamation mark in a triangle, for a warning.
    Warning,
}

/// A device context that can be drawn on safely.
///
/// Borrowed for the duration of one paint and never stored: the handle inside is
/// only valid between `BeginPaint` and `EndPaint`, or for the length of one
/// `WM_DRAWITEM`.
pub struct Canvas {
    hdc: HDC,
    dpi: u32,
    palette: Palette,
}

impl Canvas {
    /// Wraps a device context for the length of one paint.
    ///
    /// Called by the window plumbing, which owns the context and ends the paint
    /// once the canvas has been dropped.
    pub(crate) fn new(hdc: HDC, dpi: u32, palette: Palette) -> Self {
        Self { hdc, dpi, palette }
    }

    /// The colours this paint should use.
    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    /// The dpi of the window being painted.
    pub fn dpi(&self) -> u32 {
        self.dpi
    }

    /// Scales a design value to this window's dpi.
    pub fn scale(&self, value: i32) -> i32 {
        Metrics::at(value, self.dpi)
    }

    /// Fills a rectangle with a flat colour.
    pub fn fill(&self, rect: RECT, colour: COLORREF) {
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return;
        }
        // SAFETY: the brush is created here, used only by this call and deleted
        // before returning, so nothing outlives it. `rect` is a live local.
        unsafe {
            let brush = CreateSolidBrush(colour);
            if !brush.is_invalid() {
                FillRect(self.hdc, &rect, brush);
                let _ = DeleteObject(brush.into());
            }
        }
    }

    /// Fills a rectangle with rounded corners, optionally outlining it.
    ///
    /// Passing the same colour for fill and border draws a plain rounded block.
    pub fn rounded(&self, rect: RECT, fill: COLORREF, border: Option<COLORREF>, radius: i32) {
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return;
        }
        let r = self.scale(radius).max(0) * 2;
        // SAFETY: both objects are created here and deleted below, and what was
        // selected before is put back first, which is what stops a selected
        // object being deleted. The rectangle is plain data.
        unsafe {
            let brush = CreateSolidBrush(fill);
            let pen_colour = border.unwrap_or(fill);
            let pen = CreatePen(PS_SOLID, self.scale(1).max(1), pen_colour);
            let old_brush = SelectObject(self.hdc, brush.into());
            let old_pen = SelectObject(self.hdc, pen.into());

            let _ = RoundRect(self.hdc, rect.left, rect.top, rect.right, rect.bottom, r, r);

            SelectObject(self.hdc, old_brush);
            SelectObject(self.hdc, old_pen);
            let _ = DeleteObject(brush.into());
            let _ = DeleteObject(pen.into());
        }
    }

    /// Draws a card: a light panel with a hairline border.
    pub fn card(&self, rect: RECT) {
        self.rounded(
            rect,
            self.palette.card,
            Some(self.palette.card_border),
            Metrics::RADIUS,
        );
    }

    /// Draws a horizontal hairline across a rectangle's top edge.
    pub fn divider(&self, left: i32, right: i32, y: i32) {
        self.fill(
            sys::rect(left, y, right - left, self.scale(1).max(1)),
            self.palette.divider,
        );
    }

    /// Draws a straight line.
    pub fn line(&self, from: (i32, i32), to: (i32, i32), colour: COLORREF, width: i32) {
        // SAFETY: the pen is created, selected, used and deleted here, with the
        // previous pen restored before the deletion.
        unsafe {
            let pen = CreatePen(PS_SOLID, self.scale(width).max(1), colour);
            let old = SelectObject(self.hdc, pen.into());
            let _ = MoveToEx(self.hdc, from.0, from.1, None);
            let _ = LineTo(self.hdc, to.0, to.1);
            SelectObject(self.hdc, old);
            let _ = DeleteObject(pen.into());
        }
    }

    /// Draws text in one of the application's styles.
    ///
    /// Returns the height the text actually occupied, which is what a wrapping
    /// block needs in order to place whatever comes after it.
    pub fn text(
        &self,
        rect: RECT,
        text: &str,
        style: TextStyle,
        colour: COLORREF,
        align: Align,
    ) -> i32 {
        if text.is_empty() {
            return 0;
        }
        let shown = if style.is_overline() {
            spaced_capitals(text)
        } else {
            text.to_owned()
        };
        let mut wide = sys::wide(&shown);
        // DrawTextW counts characters rather than reading to a terminator when
        // it is given a length, so the trailing zero is not part of it.
        let len = wide.len().saturating_sub(1) as i32;
        let mut area = rect;

        // SAFETY: the font is owned by the theme's cache and outlives this call.
        // `wide` is a live buffer of at least `len` characters and `area` is a
        // live local the call may adjust. The previous font is restored before
        // returning, and nothing created here is left selected.
        unsafe {
            let old_font = self.select_font(font(style, self.dpi));
            SetBkMode(self.hdc, TRANSPARENT);
            SetTextColor(self.hdc, colour);
            // DT_NOPREFIX: an ampersand in a path or a file name is a character,
            // not an accelerator, and without this one would silently vanish.
            let height = DrawTextW(
                self.hdc,
                &mut wide[..len.max(0) as usize],
                &mut area,
                align.flags() | DT_NOPREFIX,
            );
            if !old_font.is_invalid() {
                SelectObject(self.hdc, old_font);
            }
            height
        }
    }

    /// How tall a piece of text would be if it were wrapped to `width`.
    ///
    /// Measured rather than guessed, because the user's text size setting and
    /// their chosen font both change the answer.
    pub fn measure(&self, text: &str, style: TextStyle, width: i32) -> i32 {
        if text.is_empty() {
            return 0;
        }
        let shown = if style.is_overline() {
            spaced_capitals(text)
        } else {
            text.to_owned()
        };
        let mut wide = sys::wide(&shown);
        let len = wide.len().saturating_sub(1) as i32;
        let mut area = sys::rect(0, 0, width, 0);
        // SAFETY: as in `text`. DT_CALCRECT measures without drawing anything.
        unsafe {
            let old_font = self.select_font(font(style, self.dpi));
            let height = DrawTextW(
                self.hdc,
                &mut wide[..len.max(0) as usize],
                &mut area,
                DT_WORDBREAK | DT_CALCRECT | DT_NOPREFIX,
            );
            if !old_font.is_invalid() {
                SelectObject(self.hdc, old_font);
            }
            height
        }
    }

    /// Selects a font, returning whatever was selected before.
    ///
    /// # Safety
    ///
    /// The caller must restore the returned object before the canvas is
    /// finished with, and must not delete the font while it is selected.
    unsafe fn select_font(&self, f: HFONT) -> HGDIOBJ {
        if f.is_invalid() {
            return HGDIOBJ::default();
        }
        // SAFETY: the font is valid and owned by the theme cache.
        unsafe { SelectObject(self.hdc, f.into()) }
    }

    /// Draws the dotted rectangle Windows uses to show keyboard focus.
    ///
    /// Owner drawn buttons have to do this themselves. Leaving it out is what
    /// makes a window impossible to use from the keyboard: the focus is there,
    /// but nothing on screen says where.
    pub fn focus_rect(&self, rect: RECT) {
        // SAFETY: `rect` is a live local for the call. DrawFocusRect draws with
        // an inverting raster operation and changes no state that has to be
        // restored afterwards.
        unsafe {
            let _ = DrawFocusRect(self.hdc, &rect);
        }
    }

    /// Draws a filled polygon from points already in client coordinates.
    fn polygon(&self, points: &[(i32, i32)], colour: COLORREF) {
        if points.len() < 3 {
            return;
        }
        let converted: Vec<windows::Win32::Foundation::POINT> = points
            .iter()
            .map(|(x, y)| windows::Win32::Foundation::POINT { x: *x, y: *y })
            .collect();
        // SAFETY: the slice is a live local of the right type and length, and
        // both objects created here are deleted after the previous ones are
        // put back.
        unsafe {
            let brush = CreateSolidBrush(colour);
            let pen = CreatePen(PS_SOLID, 1, colour);
            let old_brush = SelectObject(self.hdc, brush.into());
            let old_pen = SelectObject(self.hdc, pen.into());
            let _ = Polygon(self.hdc, &converted);
            SelectObject(self.hdc, old_brush);
            SelectObject(self.hdc, old_pen);
            let _ = DeleteObject(brush.into());
            let _ = DeleteObject(pen.into());
        }
    }

    /// Draws an ellipse, filled or outlined.
    fn ellipse(&self, rect: RECT, fill: Option<COLORREF>, border: Option<COLORREF>, width: i32) {
        use windows::Win32::Graphics::Gdi::{Ellipse, GetStockObject, NULL_BRUSH};
        // SAFETY: the stock brush is owned by Windows and must not be deleted,
        // which is why it is never passed to DeleteObject below. The pen and any
        // solid brush created here are deleted after the previous objects are
        // restored.
        unsafe {
            let brush_obj: HGDIOBJ = match fill {
                Some(colour) => CreateSolidBrush(colour).into(),
                None => GetStockObject(NULL_BRUSH),
            };
            let pen = CreatePen(
                PS_SOLID,
                self.scale(width).max(1),
                border.unwrap_or(fill.unwrap_or(self.palette.ink)),
            );
            let old_brush = SelectObject(self.hdc, brush_obj);
            let old_pen = SelectObject(self.hdc, pen.into());
            let _ = Ellipse(self.hdc, rect.left, rect.top, rect.right, rect.bottom);
            SelectObject(self.hdc, old_brush);
            SelectObject(self.hdc, old_pen);
            if fill.is_some() {
                let _ = DeleteObject(brush_obj);
            }
            let _ = DeleteObject(pen.into());
        }
    }

    /// Draws one of the application's small pictures inside a square.
    ///
    /// Every glyph is built from the same proportions, so they sit together at
    /// any size. A glyph is decoration: whatever it marks always has a text
    /// label beside it, because a picture on its own says nothing to a screen
    /// reader.
    pub fn glyph(&self, rect: RECT, glyph: Glyph, colour: COLORREF) {
        let size = (rect.right - rect.left).min(rect.bottom - rect.top);
        if size < 6 {
            return;
        }
        // Everything below is expressed in sixteenths of the box, which is what
        // lets one description serve every scale.
        let unit = |n: i32| size * n / 16;
        let x = rect.left + ((rect.right - rect.left) - size) / 2;
        let y = rect.top + ((rect.bottom - rect.top) - size) / 2;
        let at = |a: i32, b: i32| (x + unit(a), y + unit(b));
        let stroke = (size / 10).max(1);

        match glyph {
            Glyph::Brand => {
                // A hammer, reduced to two rectangles: a head across the top and
                // a shaft down the middle. Anything more detailed turns to mush
                // at the sixteen pixels the rail draws it at.
                self.rounded(
                    sys::rect(x + unit(3), y + unit(2), unit(10), unit(5)),
                    colour,
                    None,
                    2,
                );
                self.rounded(
                    sys::rect(x + unit(7), y + unit(7), unit(2), unit(7)),
                    colour,
                    None,
                    1,
                );
            }
            Glyph::Disk => {
                // A drive seen from the front: a rounded body and an indicator.
                self.rounded(
                    sys::rect(x + unit(2), y + unit(4), unit(12), unit(8)),
                    colour,
                    None,
                    2,
                );
                // The indicator is punched out of the body in the background
                // colour, so the glyph reads as one object rather than two.
                self.ellipse(
                    sys::rect(x + unit(10), y + unit(7), unit(2), unit(2)),
                    Some(self.palette.nav),
                    Some(self.palette.nav),
                    1,
                );
            }
            Glyph::Document => {
                // A page with its top right corner turned over.
                self.polygon(
                    &[at(3, 2), at(10, 2), at(13, 5), at(13, 14), at(3, 14)],
                    colour,
                );
                self.polygon(&[at(10, 2), at(13, 5), at(10, 5)], self.palette.nav);
            }
            Glyph::Disc => {
                self.ellipse(
                    sys::rect(x + unit(2), y + unit(2), unit(12), unit(12)),
                    None,
                    Some(colour),
                    2,
                );
                self.ellipse(
                    sys::rect(x + unit(7), y + unit(7), unit(2), unit(2)),
                    Some(colour),
                    Some(colour),
                    1,
                );
            }
            Glyph::Sliders => {
                // Three rails with a handle on each, at different positions.
                for (row, handle) in [(4, 11), (8, 5), (12, 9)] {
                    self.fill(
                        sys::rect(x + unit(2), y + unit(row) - stroke / 2, unit(12), stroke),
                        colour,
                    );
                    self.ellipse(
                        sys::rect(
                            x + unit(handle) - unit(2),
                            y + unit(row) - unit(2),
                            unit(4),
                            unit(4),
                        ),
                        Some(colour),
                        Some(colour),
                        1,
                    );
                }
            }
            Glyph::Tick => {
                self.line(at(3, 8), at(6, 12), colour, 2);
                self.line(at(6, 12), at(13, 4), colour, 2);
            }
            Glyph::Dot => {
                self.ellipse(
                    sys::rect(x + unit(4), y + unit(4), unit(8), unit(8)),
                    Some(colour),
                    Some(colour),
                    1,
                );
            }
            Glyph::Ring => {
                self.ellipse(
                    sys::rect(x + unit(4), y + unit(4), unit(8), unit(8)),
                    None,
                    Some(colour),
                    1,
                );
            }
            Glyph::Warning => {
                self.polygon(&[at(8, 2), at(15, 14), at(1, 14)], colour);
                // The bar and dot are punched through in the card colour, so the
                // mark reads at small sizes.
                self.fill(
                    sys::rect(x + unit(7), y + unit(6), unit(2), unit(4)),
                    self.palette.card,
                );
                self.fill(
                    sys::rect(x + unit(7), y + unit(11), unit(2), unit(2)),
                    self.palette.card,
                );
            }
        }
    }

    /// Draws a bar showing how a disk's space is divided between partitions.
    ///
    /// `segments` is a list of (share, colour), where the shares are relative to
    /// each other. A partition too small to see is still drawn as a thin sliver
    /// rather than dropped, because a disk layout with a piece missing is
    /// exactly the wrong thing to show on a backup screen.
    pub fn proportion_bar(&self, rect: RECT, segments: &[(u64, COLORREF)]) {
        let width = rect.right - rect.left;
        if width <= 0 || segments.is_empty() {
            return;
        }
        let total: u64 = segments.iter().map(|(share, _)| *share).sum();
        if total == 0 {
            return;
        }

        let minimum = self.scale(6).max(2);
        let gap = self.scale(2).max(1);
        let count = segments.len() as i32;
        let available = width - gap * (count - 1).max(0);

        // Each segment gets its share of what is left after every segment has
        // been guaranteed its minimum, so a 16 MiB reserved partition beside a
        // 500 GB one is still visible.
        let guaranteed = minimum * count;
        let flexible = (available - guaranteed).max(0);

        let mut x = rect.left;
        for (index, (share, colour)) in segments.iter().enumerate() {
            let extra = ((*share as u128) * (flexible as u128) / (total as u128)) as i32;
            let mut segment_width = minimum + extra;
            if index + 1 == segments.len() {
                // The last one takes whatever rounding left behind, so the bar
                // always ends exactly where it should.
                segment_width = (rect.right - x).max(minimum);
            }
            self.rounded(
                sys::rect(x, rect.top, segment_width, rect.bottom - rect.top),
                *colour,
                None,
                2,
            );
            x += segment_width + gap;
            if x >= rect.right {
                break;
            }
        }
    }
}

/// Spaces out a string's capitals, for a small heading over a value.
///
/// `SYSTEM DISK` reads better than `SYSTEMDISK` at the size these are drawn,
/// and letter spacing is not something `DrawTextW` offers.
fn spaced_capitals(text: &str) -> String {
    let upper = text.to_uppercase();
    let mut out = String::with_capacity(upper.len() * 2);
    for (index, ch) in upper.chars().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

/// How an owner drawn control should look.
///
/// Registered against a control with [`crate::window::set_item_style`] and drawn
/// by the window plumbing, not by the application. That is deliberate: the
/// message asking for these pixels arrives *inside* whatever handler changed the
/// control, and a handler cannot be asked to draw while it is already running.
/// Keeping the description here rather than the drawing code there is what makes
/// two applications look like one product.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemStyle {
    /// An item in the navigation rail.
    Nav {
        /// The small picture beside the label.
        glyph: Glyph,
        /// Whether this is the section currently open.
        selected: bool,
    },
    /// The primary action on a screen.
    Primary,
    /// The primary action on a screen that destroys something.
    Destructive,
}

/// Draws one owner drawn control in the application's style.
pub fn draw_item(canvas: &Canvas, item: &crate::window::DrawItem, style: ItemStyle) {
    let palette = canvas.palette();
    match style {
        ItemStyle::Nav { glyph, selected } => {
            if selected {
                canvas.rounded(item.rect, palette.nav_selected, None, Metrics::RADIUS);
                // The accent stripe. In a high contrast theme the selected
                // background is already the system highlight, so a stripe on top
                // of it would only muddy the one colour the user chose.
                if !palette.high_contrast {
                    let marker = canvas.scale(Metrics::NAV_MARKER).max(2);
                    let height = (item.rect.bottom - item.rect.top) / 2;
                    let top = item.rect.top + ((item.rect.bottom - item.rect.top) - height) / 2;
                    canvas.rounded(
                        sys::rect(item.rect.left, top, marker, height),
                        palette.nav_marker,
                        None,
                        1,
                    );
                }
            } else if item.pressed {
                canvas.rounded(item.rect, palette.nav_hover, None, Metrics::RADIUS);
            } else {
                canvas.fill(item.rect, palette.nav);
            }

            let colour = if selected && !item.disabled {
                palette.nav_text
            } else {
                palette.nav_text_dim
            };
            let size = canvas.scale(18);
            let icon = sys::rect(
                item.rect.left + canvas.scale(14),
                item.rect.top + ((item.rect.bottom - item.rect.top) - size) / 2,
                size,
                size,
            );
            canvas.glyph(icon, glyph, colour);
            let text = RECT {
                left: icon.right + canvas.scale(12),
                top: item.rect.top,
                right: item.rect.right - canvas.scale(8),
                bottom: item.rect.bottom,
            };
            canvas.text(text, &item.text, TextStyle::Body, colour, Align::Left);
            if item.focused {
                canvas.focus_rect(inset(item.rect, canvas.scale(2)));
            }
        }
        ItemStyle::Primary | ItemStyle::Destructive => {
            let accent = if style == ItemStyle::Destructive {
                palette.danger
            } else {
                palette.accent
            };
            let background = if item.disabled {
                palette.divider
            } else if item.pressed {
                palette.accent_press
            } else {
                accent
            };
            let text_colour = if item.disabled {
                palette.muted
            } else {
                palette.on_accent
            };
            canvas.rounded(item.rect, background, None, Metrics::RADIUS);
            canvas.text(
                item.rect,
                &item.text,
                TextStyle::Strong,
                text_colour,
                Align::Centre,
            );
            if item.focused {
                canvas.focus_rect(inset(item.rect, canvas.scale(3)));
            }
        }
    }
}

/// Builds a rectangle inset on every side.
pub fn inset(rect: RECT, by: i32) -> RECT {
    RECT {
        left: rect.left + by,
        top: rect.top + by,
        right: rect.right - by,
        bottom: rect.bottom - by,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capitals_are_spaced_out_for_a_small_heading() {
        assert_eq!(spaced_capitals("Disk"), "D I S K");
        assert_eq!(spaced_capitals(""), "");
        assert_eq!(spaced_capitals("a"), "A");
    }

    #[test]
    fn inset_shrinks_on_every_side() {
        let r = inset(sys::rect(10, 20, 100, 50), 5);
        assert_eq!(r.left, 15);
        assert_eq!(r.top, 25);
        assert_eq!(r.right, 105);
        assert_eq!(r.bottom, 65);
    }

    #[test]
    fn wrapping_text_starts_at_the_top_and_does_not_truncate() {
        // A wrapped block must not be given DT_END_ELLIPSIS: the point of it is
        // to show every line, and an ellipsis would hide the last one.
        let flags = Align::Wrap.flags();
        assert_ne!(flags.0 & DT_WORDBREAK.0, 0);
        assert_eq!(flags.0 & DT_END_ELLIPSIS.0, 0);
        assert_eq!(flags.0 & DT_SINGLELINE.0, 0);
    }

    /// A single line that is too long is cut with an ellipsis rather than drawn
    /// past the edge of its card. A disk model or a path is exactly the kind of
    /// text that does this.
    #[test]
    fn single_lines_are_cut_rather_than_overflowing() {
        for align in [Align::Left, Align::Centre, Align::Right] {
            let flags = align.flags();
            assert_ne!(flags.0 & DT_SINGLELINE.0, 0, "{align:?}");
            assert_ne!(flags.0 & DT_END_ELLIPSIS.0, 0, "{align:?}");
            assert_ne!(flags.0 & DT_VCENTER.0, 0, "{align:?}");
        }
    }
}
