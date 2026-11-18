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

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic font: every character is six points wide.
    fn measure(s: &str) -> f32 {
        s.chars().count() as f32 * 6.0
    }

    fn viewport(w: f32, h: f32) -> Rect {
        Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(w, h))
    }

    /// The thirteen lines a real window draws, at their real lengths, measured at
    /// [`FONT_PT`] by the synthetic font.
    fn real_rows() -> Vec<Vec2> {
        let lines = [
            "screen 2496.0x1336.0 pt · inner 2496x1336 px · scale 1.75 · expect 1426.3x763.4 pt",
            "winit scale count 1.000x (1.000 = ok) · ok",
            "os client 2496x1336 px · os scale 1.75 · window 2520x1400 px · agrees with winit",
            "os expect 1426.3x763.4 pt · layout is 1.000x1.000 of it · point space fits the os window",
            "dpi awareness per-monitor-aware",
            "monitor 2560x1440 px · client 2496x1336 px · fits the screen",
            "layout used 1426.3x763.4 pt: the platform's own screen rect (no correction)",
            "back buffer 2496x1336 px · window inner 2496x1336 px · matches yes",
            "present stretch 1.000x1.000 (1.000x1.000 = ok) · blit",
            "partition OK",
            "width 280.0 + 1146.3 = 1426.3 vs window 1426.3 (Δ 0.0) · fits yes",
            "height 26.0 + 34.0 + 703.4 = 763.4 vs window 763.4 (Δ 0.0) · fits yes · bars yes",
            "frames 7 produced / 7 shown · fresh yes",
        ];
        lines
            .iter()
            .flat_map(|l| wrap(l, MAX_WIDTH_PT - 2.0 * PAD_PT, measure))
            .map(|r| Vec2::new(measure(&r), LINE_PT))
            .collect()
    }

    #[test]
    fn the_box_never_exceeds_the_three_bounds_at_any_window_size() {
        // The window sizes the viewer has actually run at, and a few smaller ones.
        for (w, h) in [
            (1426.3, 763.4), // 1440x800 physical at 1.75
            (1280.0, 670.0), // the size smoke17 measured
            (823.0, 457.0),  // the size the defect was found at
            (640.0, 400.0),
            (320.0, 200.0),
        ] {
            let vp = viewport(w, h);
            let p = plan(vp, &real_rows(), false);
            assert!(
                p.within_bounds(vp),
                "{w}x{h}: font {} width {:.1} area {:.1}%",
                p.font_pt,
                p.rect.width(),
                p.area_fraction(vp) * 100.0
            );
            assert_eq!(
                p.font_pt, FONT_PT,
                "the font is the ceiling, never above it"
            );
            assert!(
                p.rect.width() <= MAX_WIDTH_PT,
                "{w}x{h}: {:.1} pt wide",
                p.rect.width()
            );
        }
    }

    #[test]
    fn at_the_real_window_the_whole_overlay_is_visible_or_says_what_is_not() {
        let vp = viewport(1426.3, 763.4);
        let rows = real_rows();
        let p = plan(vp, &rows, false);
        assert!(p.within_bounds(vp));
        // Nothing is drawn silently: every row is either drawn or counted.
        assert_eq!(p.shown + p.hidden, rows.len(), "{p:?}");
        // And the marker exists exactly when rows were dropped.
        assert_eq!(p.marker, p.hidden > 0, "{p:?}");
        assert!(
            p.shown > 0,
            "a 1426x763 viewport must show some of the overlay"
        );
    }

    #[test]
    fn a_long_line_wraps_instead_of_being_cut() {
        let line = "os expect 1426.3x763.4 pt · layout is 1.000x1.000 of it · point space fits the os window";
        let rows = wrap(line, MAX_WIDTH_PT - 2.0 * PAD_PT, measure);
        assert!(rows.len() > 1, "the line is wider than the box: {rows:?}");
        for r in &rows {
            assert!(
                measure(r) <= MAX_WIDTH_PT - 2.0 * PAD_PT,
                "{r:?} is wider than the box"
            );
        }
        // The text survives: joining the rows gives the line back, spaces normalised.
        assert_eq!(rows.join(" "), line, "no characters were dropped");
    }

    #[test]
    fn a_single_word_wider_than_the_box_is_split_by_character() {
        let rows = wrap("abcdefghijklmnopqrstuvwxyz0123456789", 60.0, measure);
        assert!(rows.len() > 1, "{rows:?}");
        for r in &rows {
            assert!(measure(r) <= 60.0, "{r:?}");
        }
        assert_eq!(rows.concat(), "abcdefghijklmnopqrstuvwxyz0123456789");
    }

    #[test]
    fn collapsed_is_one_row_and_says_how_to_get_the_rest_back() {
        let vp = viewport(1426.3, 763.4);
        let rows = real_rows();
        let expanded = plan(vp, &rows, false);
        let collapsed = plan(vp, &rows, true);
        assert!(collapsed.collapsed);
        assert_eq!(collapsed.shown, 1, "one row when collapsed");
        assert!(
            collapsed.rect.height() < expanded.rect.height(),
            "collapsing must actually shrink the box: {collapsed:?} vs {expanded:?}"
        );
        assert!(collapsed.within_bounds(vp));
        assert!(
            marker_text(collapsed.hidden, true).contains("o to expand"),
            "{}",
            marker_text(collapsed.hidden, true)
        );
        assert!(
            !expanded.marker,
            "the full box at this window fits every row: {expanded:?}"
        );
    }

    #[test]
    fn a_viewport_too_small_for_even_the_padding_draws_nothing_rather_than_overflowing() {
        let vp = viewport(24.0, 24.0);
        let p = plan(vp, &real_rows(), false);
        assert_eq!(p.rect, Rect::NOTHING, "no box");
        assert_eq!(p.area(), 0.0);
        assert!(p.within_bounds(vp));
        assert_eq!(p.shown, 0);
        assert_eq!(p.hidden, real_rows().len(), "and it says so");
    }

    #[test]
    fn a_box_that_must_drop_rows_reserves_one_for_the_marker() {
        // A viewport whose area leaves room for a handful of rows only.
        let vp = viewport(300.0, 120.0);
        let rows = real_rows();
        let p = plan(vp, &rows, false);
        assert!(p.within_bounds(vp), "{p:?}");
        assert!(p.hidden > 0, "this viewport cannot hold them all: {p:?}");
        assert!(p.marker, "and then a marker row is drawn: {p:?}");
        assert_eq!(p.shown + p.hidden, rows.len());
        assert_eq!(p.rows().len(), p.shown + 1, "the marker is a drawn row");
    }

    #[test]
    fn a_row_is_never_wider_than_the_box_the_viewport_allows() {
        // The box's width is derived from `row_width`, so wrapped text can never draw
        // outside its own background — including at a viewport narrower than 460 pt,
        // where a fixed wrap width would.
        for (w, h) in [
            (1426.3, 763.4),
            (900.0, 500.0),
            (400.0, 300.0),
            (200.0, 150.0),
        ] {
            let vp = viewport(w, h);
            let rw = row_width(vp);
            assert!(
                rw <= MAX_WIDTH_PT - 2.0 * PAD_PT + 1e-3,
                "{w}x{h}: the row width {rw} is over the box's bound"
            );
            let rows: Vec<Vec2> = wrap(
                "os expect 1426.3x763.4 pt · layout is 1.000x1.000 of it · point space fits the os window",
                rw,
                measure,
            )
            .into_iter()
            .map(|r| Vec2::new(measure(&r), LINE_PT))
            .collect();
            let p = plan(vp, &rows, false);
            let inner = p.rect.width() - 2.0 * PAD_PT;
            assert!(
                inner + 1e-3 >= rows.iter().map(|r| r.x).fold(0.0f32, f32::max),
                "{w}x{h}: a row is wider than the box's interior ({inner:.1})"
            );
        }
    }

    #[test]
    fn an_empty_overlay_plans_nothing() {
        let vp = viewport(1280.0, 670.0);
        let p = plan(vp, &[], false);
        assert_eq!(p.rect, Rect::NOTHING);
        assert!(p.within_bounds(vp));
    }

    #[test]
    fn the_row_offset_of_the_text_is_the_boxes_padding_plus_whole_lines() {
        // The painter and the plan must agree, or the last row hangs out of the box.
        assert_eq!(row_y(0), PAD_PT);
        assert_eq!(row_y(1), PAD_PT + LINE_PT);
        let rows = vec![Vec2::new(10.0, LINE_PT); 3];
        let p = plan(viewport(1280.0, 670.0), &rows, false);
        let last_bottom = p.rect.min.y + row_y(2) + LINE_PT - LINE_PT * 0.0;
        assert!(
            last_bottom <= p.rect.max.y + 1e-3,
            "the third row's bottom {last_bottom} hangs below the box {}",
            p.rect.max.y
        );
    }
}
