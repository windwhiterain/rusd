//! Headless layout regression with the **real** panels: the viewport must get exactly
//! what the panels leave, at every scale.
//!
//! The defect this pins, as a screenshot caught it: smoke17 reported a viewport rect
//! of **910×713 pt** inside a window of **823×457 pt** — a viewport *wider than the
//! window it lives in*, so the composition-tree panel's ~517 pt and the viewport's
//! 910 pt added up to 1427 against a 823-pt window, and the city (framed in the
//! viewport's centre) was pushed off the right-hand edge. Only the viewport's left
//! fringe was on screen.
//!
//! These tests build the production arrangement — status bar on top, the
//! composition-tree panel on the left, the controls bar at the bottom, the viewport in
//! what remains — with the **production widgets** (`panel::tree_panel`,
//! `panel::detail_panel`, `panel::capture_toolbar`), and assert what egui actually
//! allocated:
//!
//! 1. `left panel + viewport == window width`, and `top + bottom + viewport == window
//!    height` — the panels and the viewport are a partition of the window, not
//!    overlapping or overflowing;
//! 2. the viewport rect **fits** the budget the panels leave, and does not overflow it;
//! 3. the same holds at `pixels_per_point` 1.0, 1.75 and 2.0 — the scale 1.75 is the
//!    one smoke17 ran at.
//!
//! The red-green case: put the old rect source back (a rect derived from the window,
//! or a stale one) and `the_panels_and_the_viewport_partition_the_window` fails.

use rusd_viewer::panel::TreePanel;
use rusd_viewer::viewport::{PanelBudget, Partition, ScreenAgreement, ScreenSpace, ViewportSize};

/// What one pass laid out.
struct Laid {
    /// The rect the viewport was given (points).
    viewport: egui::Rect,
    /// The left panel's outer width and the bars' heights (points).
    left: f32,
    top: f32,
    bottom: f32,
    /// The `ViewportSize` the widget derived from the rect.
    size: ViewportSize,
}

/// Run one pass with the production panels.
fn lay_out(ppp: f32, window_points: egui::Vec2) -> Laid {
    let ctx = egui::Context::default();
    let mut raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, window_points)),
        ..Default::default()
    };
    raw.viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .native_pixels_per_point = Some(ppp);

    let panel = TreePanel::new();
    let mut vp_rect = egui::Rect::NOTHING;
    let mut size = None;
    let mut left = 0.0f32;
    let mut top = 0.0f32;
    let mut bottom = 0.0f32;

    let out = ctx.run_ui(raw, |root| {
        let r = egui::Panel::top("status").show(root, |ui| {
            ui.horizontal(|ui| {
                ui.strong("rusd-viewer");
                ui.label("已连接 · v4 · 475 prim · 未选中");
                ui.label("70 fps");
            });
        });
        top = r.response.rect.height();

        let r = egui::Panel::left("composition-tree")
            .default_size(280.0)
            .min_size(180.0)
            .show(root, |ui| {
                let _ = rusd_viewer::panel::tree_panel(ui, &panel);
                ui.separator();
                rusd_viewer::panel::detail_panel(ui, &panel);
            });
        left = r.response.rect.width();

        let r = egui::Panel::bottom("controls").show(root, |ui| {
            ui.horizontal(|ui| {
                let _ = ui.button("Undo");
                let _ = ui.button("Redo");
                let _ = ui.button("Fit");
                let mut note = String::new();
                let _ = rusd_viewer::panel::capture_toolbar(ui, false, &mut note);
                ui.label(
                    "click a prim · LMB orbit · RMB pan · wheel zoom · drag a selected prim to move it",
                );
            });
        });
        bottom = r.response.rect.height();

        // The production allocation: whatever the layout left, and nothing else.
        egui::CentralPanel::no_frame().show(root, |ui| {
            let avail = ui.available_size();
            let (rect, _resp) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
            vp_rect = rect;
            size = Some(ViewportSize::new(rect, ui.pixels_per_point()));
        });
    });
    let mut delta = out.textures_delta;
    delta.clear();
    Laid {
        viewport: vp_rect,
        left,
        top,
        bottom,
        size: size.expect("the viewport was allocated"),
    }
}

/// A window in points: the 1440×800 physical the smoke test set, and two others.
fn windows(ppp: f32) -> Vec<egui::Vec2> {
    let phys = egui::vec2(1440.0, 800.0);
    vec![
        phys / ppp,
        egui::vec2(1280.0, 720.0) / ppp,
        egui::vec2(1100.0, 700.0),
    ]
}

/// The scales a real desktop runs at; 1.75 is smoke17's.
const PPPS: [f32; 3] = [1.0, 1.75, 2.0];

#[test]
fn the_panels_and_the_viewport_partition_the_window() {
    for ppp in PPPS {
        for window in windows(ppp) {
            let laid = lay_out(ppp, window);
            // Width: the left panel and the viewport together are the window.
            let width_sum = laid.left + laid.viewport.width();
            assert!(
                (width_sum - window.x).abs() <= 1.5,
                "ppp {ppp}, window {window:?}: left panel {:.1} + viewport {:.1} = \
                 {width_sum:.1}, not the window's width — the viewport was given a rect \
                 the panels had already taken",
                laid.left,
                laid.viewport.width()
            );
            // Height: top bar, controls and the viewport are the window.
            let height_sum = laid.top + laid.bottom + laid.viewport.height();
            assert!(
                (height_sum - window.y).abs() <= 1.5,
                "ppp {ppp}, window {window:?}: top {:.1} + bottom {:.1} + viewport {:.1} \
                 = {height_sum:.1}, not the window's height",
                laid.top,
                laid.bottom,
                laid.viewport.height()
            );
            // And nothing overflows on the right or the bottom.
            assert!(
                laid.viewport.max.x <= window.x + 1.0,
                "ppp {ppp}: the viewport's right edge {:.1} is outside the window {:.1}",
                laid.viewport.max.x,
                window.x
            );
            assert!(
                laid.viewport.max.y <= window.y + 1.0,
                "ppp {ppp}: the viewport's bottom edge {:.1} is outside the window {:.1}",
                laid.viewport.max.y,
                window.y
            );
        }
    }
}

#[test]
fn the_viewport_rect_is_what_the_panels_leave() {
    for ppp in PPPS {
        for window in windows(ppp) {
            let laid = lay_out(ppp, window);
            let budget = PanelBudget {
                window,
                left: laid.left,
                top: laid.top,
                bottom: laid.bottom,
            };
            assert!(
                budget.fits(laid.viewport),
                "ppp {ppp}, window {window:?}: the viewport rect {:.0}x{:.0} is not the \
                 space the panels leave ({:.0}x{:.0})",
                laid.viewport.width(),
                laid.viewport.height(),
                budget.remaining().x,
                budget.remaining().y
            );
            assert!(
                !budget.overflows(laid.viewport),
                "ppp {ppp}: the viewport rect overflowed the panels' budget"
            );
        }
    }
}

#[test]
fn a_viewport_wider_than_the_window_is_the_bug_and_is_detected() {
    // smoke17's numbers, kept as an executable record: a 910x713 pt viewport inside a
    // 823x457 pt window, with the panel's ~517 pt. `overflows` must say so — if this
    // ever stops being detected, the regression test above has gone blind.
    let window = egui::vec2(1440.0, 800.0) / 1.75; // 822.9 x 457.1
    let budget = PanelBudget {
        window,
        left: 517.0,
        top: 25.0,
        bottom: 24.0,
    };
    let reported = egui::Rect::from_min_size(egui::pos2(0.0, 25.0), egui::vec2(910.0, 713.0));
    assert!(
        budget.overflows(reported),
        "a 910-pt viewport in a {:.0}-pt window (panels take {:.0}) must be flagged: \
         remaining is {:?}",
        window.x,
        budget.left,
        budget.remaining()
    );
    assert!(!budget.fits(reported));
    // And the rect the layout should have produced fits.
    let good = egui::Rect::from_min_size(egui::pos2(517.0, 49.0), budget.remaining());
    assert!(
        budget.fits(good),
        "the correct rect is the remaining space: {good:?}"
    );
}

#[test]
fn the_viewport_takes_back_the_width_when_the_panel_is_collapsed() {
    // Task: with the panel collapsible and drag-resizable, the viewport must re-derive
    // its rect from what is left. The budget arithmetic says exactly what that means:
    // a narrower panel leaves a wider viewport, and the two still partition the window.
    let window = egui::vec2(822.857, 457.143);
    let open = PanelBudget {
        window,
        left: 280.0,
        top: 25.0,
        bottom: 24.0,
    };
    let collapsed = PanelBudget {
        window,
        left: 24.0,
        top: 25.0,
        bottom: 24.0,
    };
    assert!(
        collapsed.remaining().x > open.remaining().x + 200.0,
        "collapsing the panel must hand its width to the viewport: {:.0} → {:.0}",
        open.remaining().x,
        collapsed.remaining().x
    );
    // Both are still partitions of the same window: nothing was lost or doubled.
    for b in [&open, &collapsed] {
        let sum = b.left + b.remaining().x;
        assert!(
            (sum - window.x).abs() <= 1.0,
            "left + viewport = window: {sum} vs {}",
            window.x
        );
        let vsum = b.top + b.bottom + b.remaining().y;
        assert!(
            (vsum - window.y).abs() <= 1.0,
            "top + bottom + viewport = window: {vsum}"
        );
    }
}

#[test]
fn a_dragged_panel_width_still_partitions_the_window() {
    // A user drags the panel to any width in its range; the arithmetic has to hold at
    // each, because the viewport reads it every pass rather than remembering one.
    let window = egui::vec2(822.857, 457.143);
    for left in [180.0f32, 280.0, 340.0, 517.0] {
        let b = PanelBudget {
            window,
            left,
            top: 25.0,
            bottom: 24.0,
        };
        assert!(
            (b.left + b.remaining().x - window.x).abs() <= 1.0,
            "panel at {left}: the viewport is whatever is left"
        );
        assert!(b.remaining().x > 0.0, "the viewport never disappears");
    }
}

#[test]
fn the_partition_line_the_overlay_prints_is_true_at_every_scale() {
    // The same property as above, asserted through the type the `--debug-ui`
    // overlay prints — so the line a screenshot shows and the line a test checks
    // are one function. If `ok()` ever disagrees with the assertions above, the
    // overlay is lying to the person reading the screenshot.
    for ppp in PPPS {
        for window in windows(ppp) {
            let laid = lay_out(ppp, window);
            let p = Partition {
                window,
                left: laid.left,
                top: laid.top,
                bottom: laid.bottom,
                rect: laid.viewport,
            };
            assert!(
                p.ok(),
                "ppp {ppp}, window {window:?}: {}",
                p.lines().join(" | ")
            );
            // And the bars are really there: a zero-height controls bar would keep
            // both identities true and still be the defect a screenshot shows.
            assert!(
                p.bars_present(),
                "ppp {ppp}: left {:.1}, top {:.1}, bottom {:.1}",
                laid.left,
                laid.top,
                laid.bottom
            );
            let lines = p.lines().join(" | ");
            assert!(lines.contains("partition OK"), "{lines}");
        }
    }
}

#[test]
fn a_window_resized_the_way_the_round_did_keeps_its_point_space() {
    // The DPI round's arithmetic, executable: the window is resized from its startup
    // client area to a maximized one, at the machine's 1.75 scale, and the point
    // space the layout is given must stay `inner / scale` at **both** sizes. The
    // failure rows are the ones a wrong fix produces: physical pixels as points
    // (never allowed) and the scale counted twice.
    let scale = 1.75f32;
    for inner in [(2240u32, 1216u32), (2560, 1272), (1920, 1080), (1463, 784)] {
        let mut space = ScreenSpace {
            inner_physical: inner,
            scale_factor: scale,
            screen_points: egui::Vec2::ZERO,
            presented_physical: inner,
            os: None,
        };
        let points = space.expected_points();
        assert!(
            (points.x - inner.0 as f32 / scale).abs() < 1e-3
                && (points.y - inner.1 as f32 / scale).abs() < 1e-3,
            "inner {inner:?}: the point size is the physical size divided by the scale"
        );
        // What egui-winit hands the pass, when the platform is not broken.
        space.screen_points = points;
        assert_eq!(space.agreement(), ScreenAgreement::Consistent);
        assert_eq!(space.correction(), None, "a correct space is left alone");

        // And the layout run in that space partitions it: every window size, one
        // scale, the production panels.
        let laid = lay_out(scale, points);
        let p = Partition {
            window: points,
            left: laid.left,
            top: laid.top,
            bottom: laid.bottom,
            rect: laid.viewport,
        };
        assert!(p.ok(), "inner {inner:?}: {}", p.lines().join(" | "));
    }
}

#[test]
fn the_scale_counted_twice_would_push_the_viewport_out_of_the_window() {
    // Red-green for the correction: the same production layout run in a point space
    // that is `inner / scale²` produces a rect that does not partition the window,
    // and `Partition` says so. Then the correction hands the layout the right size
    // and the same layout partitions it again.
    let scale = 1.75f32;
    let inner = (2240u32, 1216u32);
    let right = inner.0 as f32 / scale;
    let mut space = ScreenSpace {
        inner_physical: inner,
        scale_factor: scale,
        screen_points: egui::vec2(right / scale, inner.1 as f32 / scale / scale),
        presented_physical: inner,
        os: None,
    };
    assert_eq!(space.agreement(), ScreenAgreement::ScaledTwice);
    let want = space
        .correction()
        .expect("the correction is the divided size");

    // The wrong space: the layout believes the window is 731 pt wide, so its panels
    // and its viewport are laid out inside that — while the window really is
    // 1280 pt. Checked against the **real** window, the identity fails on the width,
    // and it fails by the whole of the scale's error: that is exactly how a
    // double-counted scale pushes the viewport's right-hand half off the window.
    let laid = lay_out(scale, space.screen_points);
    let wrong = Partition {
        window: space.expected_points(),
        left: laid.left,
        top: laid.top,
        bottom: laid.bottom,
        rect: laid.viewport,
    };
    assert!(
        !wrong.fits_width(),
        "red: laid out in `inner / scale²` against the real window, the partition \
         must fail — but it read {} ({:.1} + {:.1} vs {:.1})",
        "fits",
        wrong.left,
        wrong.rect.width(),
        wrong.window.x
    );
    assert!(
        (wrong.window.x - wrong.width_total()) > 500.0,
        "and the shortfall is the scale's error, not a pixel: {:.1}",
        wrong.window.x - wrong.width_total()
    );
    assert!(
        laid.viewport.width() < want.x - 500.0,
        "the too-small window leaves a viewport short by the same amount: {:.1} vs {:.1}",
        laid.viewport.width(),
        want.x
    );

    // After the correction the layout is handed the right space, and it partitions.
    space.screen_points = want;
    assert_eq!(space.agreement(), ScreenAgreement::Consistent);
    let laid = lay_out(scale, want);
    let ok = Partition {
        window: want,
        left: laid.left,
        top: laid.top,
        bottom: laid.bottom,
        rect: laid.viewport,
    };
    assert!(ok.ok(), "{}", ok.lines().join(" | "));
}

#[test]
fn the_render_texel_size_follows_the_viewport_rect_at_every_scale() {
    // The panels' width change must flow through to the renderer: the frame is the
    // rect's device pixels, not the window's.
    for ppp in PPPS {
        for window in windows(ppp) {
            let laid = lay_out(ppp, window);
            let (tw, th) = laid.size.texels();
            // `ViewportSize` snaps the rect to the pixel grid, and derives the texels
            // from the *snapped* rect — so the comparison is against `size.rect()`,
            // not the pre-snap rect the layout handed over.
            let snapped = laid.size.rect();
            assert_eq!(
                tw,
                (snapped.width() * ppp).round().max(1.0) as u32,
                "ppp {ppp}: render width is the viewport rect's, not the window's"
            );
            assert_eq!(
                th,
                (snapped.height() * ppp).round().max(1.0) as u32,
                "ppp {ppp}: render height is the viewport rect's"
            );
            assert!(
                laid.size.is_pixel_exact((tw, th)),
                "ppp {ppp}: the frame the renderer makes must be 1:1 with the viewport"
            );
            // And the window's device pixels are not what it renders at.
            let window_texels = (
                (window.x * ppp).round() as u32,
                (window.y * ppp).round() as u32,
            );
            assert_ne!(
                (tw, th),
                window_texels,
                "ppp {ppp}: the render size equals the window's, which is the old bug"
            );
        }
    }
}
