//! The `--debug-ui` overlay's **plan**: how large it may be, how much of it fits,
//! and how a long line is made to fit without being thrown away.
//!
//! The overlay started as thirteen lines of 12 pt text drawn from the viewport's
//! top-left corner, and on a 1280×670-point viewport it half-filled the screen — the
//! user's complaint. The contract it has to keep (`docs/PROTOCOL.md` v1.4) is
//! concrete, so it is enforced here as arithmetic instead of as a promise:
//!
//! | bound | value | what fails without it |
//! | --- | --- | --- |
//! | font | **≤ 11 pt** | the overlay covers the picture it is describing |
//! | width | **≤ 460 pt** | a long line grows the box past half the window |
//! | area | **≤ 25 %** of the viewport | thirteen long lines fill the viewport |
//! | collapsible | one row, by `o` or the button | no way back to a clean picture |
//!
//! # Why wrapping rather than truncation
//!
//! The lines are the instrument this viewer has been debugged with, and their exact
//! text is what tests assert (`viewport::OverlayReport::lines`, `ScreenSpace::lines`,
//! `Partition::lines` — none of which changed). Clipping a line at 460 pt would cut
//! the numbers off the end of the line that exists to show them. So a line that does
//! not fit is **wrapped** into rows of at most the box's width, and what cannot be
//! shown at all is counted on the last row — `+N more` — rather than silently lost.
//!
//! # What is pure and what is not
//!
//! Measuring text needs a font, so [`wrap`] takes a `measure` closure: the window
//! passes the egui painter's, and a test passes a synthetic one (six points a
//! character), which is why the wrap rule is testable without a screen. [`plan`] takes
//! rows that are already measured, so the size arithmetic is testable the same way.

use egui::{Rect, Vec2};

/// The overlay's font size in points. It is a **ceiling**: nothing here may draw the
/// overlay larger, and [`OverlayPlan::within_bounds`] checks the plan against it.
pub const FONT_PT: f32 = 11.0;

/// The tallest one row of overlay text may be, in points (the font plus leading).
pub const LINE_PT: f32 = 14.0;

/// The overlay's box may never be wider than this, in points.
pub const MAX_WIDTH_PT: f32 = 460.0;

/// Nor may it cover more than this fraction of the viewport's **area**.
pub const MAX_AREA_FRACTION: f32 = 0.25;

/// The gap between the box and the viewport's edges, in points.
pub const MARGIN_PT: f32 = 8.0;

/// Padding inside the box, in points.
pub const PAD_PT: f32 = 5.0;

/// One row's `y`, relative to the box's top-left, for a row index.
pub fn row_y(row: usize) -> f32 {
    PAD_PT + row as f32 * LINE_PT
}

/// The widest a **row of text** may be at `viewport`: the box's bound, the viewport's own
/// width less its margins, and the box's padding taken off.
///
/// This is where [`wrap`] is told to break, and [`plan`] uses the same number to size the
/// box — because a box narrower than its rows would draw text outside its own background,
/// which is what a fixed 460 pt wrap width does on a small viewport. One function, so the
/// wrapping and the box cannot disagree.
pub fn row_width(viewport: Rect) -> f32 {
    let room = viewport.width() - 2.0 * MARGIN_PT;
    (room.min(MAX_WIDTH_PT) - 2.0 * PAD_PT).max(1.0)
}

/// Split `line` into rows no wider than `max_width`, by measuring candidate rows with
/// `measure`.
///
/// Words are kept whole where they fit; a single word wider than the box is split by
/// character, because a box that refuses to draw it is worse than a broken word. An
/// empty `line` is one empty row, so a blank line in the input stays a blank line in
/// the overlay.
///
/// `measure` must be monotone (a longer string is never narrower), which any font's
/// layout is; the greedy loop relies on it.
pub fn wrap(line: &str, max_width: f32, measure: impl Fn(&str) -> f32) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    if max_width <= 0.0 {
        // No width to draw in: one row holding the whole line, and `plan` will hide it.
        return vec![line.to_owned()];
    }
    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in line.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_owned()
        } else {
            format!("{current} {word}")
        };
        if measure(&candidate) <= max_width {
            current = candidate;
            continue;
        }
        if !current.is_empty() {
            rows.push(std::mem::take(&mut current));
        }
        // The word alone may still be too wide: split it by character, keeping as
        // much as fits on each row.
        let mut piece = String::new();
        for ch in word.chars() {
            let mut next = piece.clone();
            next.push(ch);
            if !piece.is_empty() && measure(&next) > max_width {
                rows.push(std::mem::take(&mut piece));
            }
            piece.push(ch);
        }
        current = piece;
    }
    if !current.is_empty() {
        rows.push(current);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

/// One row of the drawn box: where it goes, and what text it holds.
///
/// `input` is the index into the rows the caller passed to [`plan`]; `None` is the
/// `+N more` marker, which is the box's own text rather than any of the caller's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrawnRow {
    /// The row's `y` index inside the box.
    pub row: usize,
    /// Which input row this draws, or `None` for the `+N more` marker.
    pub input: Option<usize>,
}

/// Where the overlay's box goes and how much of it can be drawn.
///
/// `rect` is in **viewport-relative** points (its `min` is [`MARGIN_PT`] in from the
/// viewport's top-left), so a caller positions it by adding the viewport's `min` and a
/// test can assert the arithmetic without a screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayPlan {
    /// The font the rows were measured at — [`FONT_PT`] unless a caller asks smaller.
    pub font_pt: f32,
    /// The box. `Rect::NOTHING` when the viewport leaves no room at all.
    pub rect: Rect,
    /// Input rows drawn.
    pub shown: usize,
    /// Input rows the box was too small to draw.
    pub hidden: usize,
    /// Whether one of the drawn rows is the `+N more` marker rather than an input row.
    pub marker: bool,
    /// Whether the user has collapsed the overlay: one row, and how to get it back.
    pub collapsed: bool,
}

impl OverlayPlan {
    /// The box's area, in square points.
    pub fn area(&self) -> f32 {
        if self.rect == Rect::NOTHING {
            return 0.0;
        }
        self.rect.width() * self.rect.height()
    }

    /// The share of `viewport`'s area the box covers; `0.0` for an empty box.
    pub fn area_fraction(&self, viewport: Rect) -> f32 {
        let total = viewport.width() * viewport.height();
        if total <= 0.0 {
            return 0.0;
        }
        self.area() / total
    }

    /// Whether the plan obeys all three of the contract's size bounds. The resolution
    /// tests are lifted from the real window's numbers, but the bound is checked here
    /// so the window cannot drift away from it.
    pub fn within_bounds(&self, viewport: Rect) -> bool {
        self.font_pt <= FONT_PT + 1e-4
            && self.rect.width() <= MAX_WIDTH_PT + 1e-4
            && self.area_fraction(viewport) <= MAX_AREA_FRACTION + 1e-6
    }

    /// The rows to draw, in the order they are drawn.
    pub fn rows(&self) -> Vec<DrawnRow> {
        let mut out: Vec<DrawnRow> = (0..self.shown)
            .map(|i| DrawnRow {
                row: i,
                input: Some(i),
            })
            .collect();
        if self.marker {
            out.push(DrawnRow {
                row: self.shown,
                input: None,
            });
        }
        out
    }
}

/// Plan the box for `rows` (each already measured at [`FONT_PT`]) inside `viewport`.
///
/// The width is the widest row plus padding, clamped to [`MAX_WIDTH_PT`] **and** to the
/// viewport less its margins. The height is then whatever [`MAX_AREA_FRACTION`] of the
/// viewport's area allows at that width, and the rows are cut to fit it — reserving the
/// last one for the `+N more` marker when anything had to be dropped.
pub fn plan(viewport: Rect, rows: &[Vec2], collapsed: bool) -> OverlayPlan {
    let room = Vec2::new(
        viewport.width() - 2.0 * MARGIN_PT,
        viewport.height() - 2.0 * MARGIN_PT,
    );
    if room.x <= 0.0 || room.y <= 0.0 || rows.is_empty() {
        return OverlayPlan {
            font_pt: FONT_PT,
            rect: Rect::NOTHING,
            shown: 0,
            hidden: rows.len(),
            marker: false,
            collapsed,
        };
    }
    let widest = rows.iter().map(|r| r.x).fold(0.0f32, f32::max);
    let width = (widest + 2.0 * PAD_PT)
        .min(MAX_WIDTH_PT)
        .min(room.x)
        .max(row_width(viewport) + 2.0 * PAD_PT);

    // The area bound, solved for height: `width * height <= 0.25 * viewport area`.
    let area_cap = MAX_AREA_FRACTION * viewport.width() * viewport.height() / width.max(1e-3);
    let height_cap = area_cap.min(room.y);

    if height_cap < 2.0 * PAD_PT {
        // Not even the padding fits: an empty box is the only honest answer, and its
        // area is zero, so the bound still holds.
        return OverlayPlan {
            font_pt: FONT_PT,
            rect: Rect::NOTHING,
            shown: 0,
            hidden: rows.len(),
            marker: false,
            collapsed,
        };
    }

    let fit = (((height_cap - 2.0 * PAD_PT) / LINE_PT) + 1e-6).floor() as usize;
    let wanted = if collapsed { 1 } else { rows.len() };
    let (shown, marker) = if wanted <= fit {
        (wanted, false)
    } else if fit == 0 {
        (0, false)
    } else {
        // One slot goes to the marker that says how much is not shown.
        (fit - 1, true)
    };
    let grouped = shown + usize::from(marker);
    let rect = Rect::from_min_size(
        egui::pos2(MARGIN_PT, MARGIN_PT),
        Vec2::new(width, 2.0 * PAD_PT + grouped as f32 * LINE_PT),
    );
    OverlayPlan {
        font_pt: FONT_PT,
        rect,
        shown,
        hidden: rows.len().saturating_sub(shown),
        marker,
        collapsed,
    }
}

/// The marker row's text: how many rows are not drawn, and how to see them.
pub fn marker_text(hidden: usize, collapsed: bool) -> String {
    let how = if collapsed {
        "o to expand"
    } else {
        "o to collapse"
    };
    format!("+{hidden} more · {how}")
}
