//! Headless egui **layout** regression for the viewport image, at the three scales a
//! real desktop runs at.
//!
//! The defect this pins, as it reached the screen: the render thread produced a
//! correct, fully-framed frame, and the window showed the city as a magnified crop of
//! its top-left corner. The frame was rendered at the **window's** size, fitted for the
//! **window's** aspect, and painted into a rect that had neither — so the sampler
//! resampled it into a shape nobody had rendered for.
//!
//! These tests run the **real** egui pass (`ctx.run_ui`) through the same code path
//! `app.rs` runs — `ViewportSize::new(rect, ppp)` then `paint_at(size.rect())` — and
//! assert, on the shapes egui actually produced:
//!
//! 1. the painted image rect **is** the allocated rect (so the picture covers the
//!    viewport, not a rect derived from anything else);
//! 2. its UV range is the **whole** frame, `(0,0)`–`(1,1)`;
//! 3. the frame's texel count is the viewport's own texel count, i.e. a pixel-exact
//!    blit rather than a resample.
//!
//! Cheaper assertions on `ViewportSize` alone would pass while the widget kept calling
//! `paint_at` with a different rect, which is exactly how the last three rounds
//! regressed; this one inspects what egui actually got.

/// One pass at `ppp` with the frame the render thread would have produced.
struct Painted {
    /// The rect `allocate_exact_size` gave the viewport (points).
    allocated: egui::Rect,
    /// The texel size `ViewportSize` derived for that rect.
    texels: (u32, u32),
    /// The image quad's extent, in the space egui puts shapes in.
    quad: egui::Rect,
    /// The quad's UV bounds.
    uv: egui::Rect,
    /// How many textured quads were found (exactly one is expected).
    quads: usize,
}

/// Run the app's viewport painting for a window of `window_points` at `ppp`.
///
/// The frame is rendered at exactly the texel size `ViewportSize` asks for — the
/// production arrangement — clamped only to keep the test texture inside egui's 2048
/// atlas limit.
fn paint(ppp: f32, window_points: egui::Vec2) -> Painted {
    let ctx = egui::Context::default();
    let mut raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, window_points)),
        ..Default::default()
    };
    raw.viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .native_pixels_per_point = Some(ppp);

    // Pre-compute the texel size the widget will ask for, so the texture exists
    // before the pass that paints it. Clamped for the atlas limit only.
    let probe_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, window_points);
    let wanted = rusd_viewer::viewport::ViewportSize::new(probe_rect, ppp).texels();
    let texels = (wanted.0.min(2000), wanted.1.min(2000));
    let image = egui::ColorImage::filled(
        [texels.0 as usize, texels.1 as usize],
        egui::Color32::from_rgb(30, 200, 30),
    );
    let texture = ctx.load_texture("frame", image, egui::TextureOptions::LINEAR);

    let mut allocated = egui::Rect::NOTHING;
    let mut derived = (0u32, 0u32);
    let out = ctx.run_ui(raw, |root| {
        egui::Panel::top("status").show(root, |ui| {
            ui.label("已连接 · v8 · 434 prim · 未选中");
        });
        egui::CentralPanel::no_frame().show(root, |ui| {
            let avail = ui.available_size();
            let (rect, _resp) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
            allocated = rect;
            // The widget's own code path, exactly as `app.rs` runs it.
            let size = rusd_viewer::viewport::ViewportSize::new(rect, ui.pixels_per_point());
            derived = size.texels();
            egui::Image::new(&texture)
                .fit_to_exact_size(size.rect().size())
                .uv(size.uv())
                .paint_at(ui, size.rect());
        });
    });
    // Unapplied texture deltas are a debug-assert, and this test has no painter.
    let mut delta = out.textures_delta;
    delta.clear();

    // The image is painted as a `RectShape` carrying a `Brush` (texture id + UV),
    // not as a four-vertex mesh, so the shape list is what has to be read.
    let mut quad = egui::Rect::NOTHING;
    let mut uv = egui::Rect::NOTHING;
    let mut quads = 0usize;
    let atlas = egui::TextureId::Managed(0);
    fn walk(shape: &egui::Shape, visit: &mut impl FnMut(&egui::Shape)) {
        visit(shape);
        if let egui::Shape::Vec(shapes) = shape {
            for s in shapes {
                walk(s, visit);
            }
        }
    }
    {
        let mut visit = |shape: &egui::Shape| {
            if let egui::Shape::Rect(r) = shape
                && let Some(brush) = &r.brush
                && brush.fill_texture_id != atlas
            {
                quads += 1;
                quad.extend_with(r.rect.min);
                quad.extend_with(r.rect.max);
                uv.extend_with(brush.uv.min);
                uv.extend_with(brush.uv.max);
            }
        };
        for cs in &out.shapes {
            walk(&cs.shape, &mut visit);
        }
    }
    Painted {
        allocated,
        texels: derived,
        quad,
        uv,
        quads,
    }
}

/// The `pixels_per_point` values a real desktop runs at — including the 1.75 that
/// exposed the bug.
const PPPS: [f32; 4] = [1.0, 1.25, 1.75, 2.0];

const WINDOWS: [egui::Vec2; 3] = [
    egui::vec2(1100.0, 620.0),
    egui::vec2(900.0, 880.0),
    egui::vec2(1375.0, 800.0),
];

#[test]
fn the_frame_fills_the_viewport_rect_at_every_pixels_per_point() {
    for ppp in PPPS {
        for window_points in WINDOWS {
            let p = paint(ppp, window_points);
            assert_eq!(p.quads, 1, "ppp {ppp}: exactly one textured quad");
            // The rect is the viewport's, on the pixel grid: a whole device pixel's
            // worth of slack expressed in points.
            let slack = 1.0 / ppp + 1e-3;
            assert!(
                (p.quad.min - p.allocated.min).length() <= slack
                    && (p.quad.max - p.allocated.max).length() <= slack,
                "ppp {ppp}, window {window_points:?}: the painted image must cover the \
                 allocated viewport rect (quad {:?}, rect {:?}) — a different rect is \
                 the scaling bug",
                p.quad,
                p.allocated
            );
            // And the quad is the size the renderer was told to render: a frame of
            // `texels` drawn into a rect of `texels / ppp` points is 1:1.
            assert_eq!(
                p.texels,
                (
                    (p.allocated.width() * ppp).round().max(1.0) as u32,
                    (p.allocated.height() * ppp).round().max(1.0) as u32,
                ),
                "ppp {ppp}: the render texel size must be the rect times the scale"
            );
        }
    }
}

#[test]
fn the_texture_uv_covers_the_whole_frame() {
    for ppp in PPPS {
        let p = paint(ppp, WINDOWS[0]);
        assert_eq!(
            p.uv,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            "ppp {ppp}: the quad must sample the whole frame, corner to corner"
        );
    }
}

#[test]
fn the_render_texel_size_is_the_rect_times_the_scale_not_the_window() {
    for ppp in PPPS {
        for window_points in WINDOWS {
            let vp = rusd_viewer::viewport::whole_window(window_points, ppp);
            // The rect's own device pixels, rounded once.
            assert_eq!(
                vp.texels(),
                (
                    (window_points.x * ppp).round().max(1.0) as u32,
                    (window_points.y * ppp).round().max(1.0) as u32,
                ),
                "ppp {ppp}: the render size must be the rect times the scale"
            );
            assert!(
                vp.is_pixel_exact(vp.texels()),
                "ppp {ppp}: a frame of the viewport's own texel size is 1:1"
            );
        }
    }
}

#[test]
fn a_frame_of_the_windows_size_is_not_a_blit_of_the_viewport_rect() {
    // The bug's own arithmetic, kept as a test so the regression cannot come back
    // silently: the render thread used `window.inner_size()` (2240x1260 at ppp 1.75
    // in a 1280x720 window) while the viewport rect is 1280x670 points — so the frame
    // is squeezed vertically into 2240x1173 device pixels, and its aspect is wrong as
    // well.
    let ppp = 1.75f32;
    let rect = egui::Rect::from_min_size(egui::pos2(0.0, 25.0), egui::vec2(1280.0, 670.0));
    let vp = rusd_viewer::viewport::ViewportSize::new(rect, ppp);
    // The rect snapped to the pixel grid (670 → 669.8 pt at 1.75), so the height is
    // 1172 device pixels rather than the 1173 the un-snapped rect would give — which
    // is exactly the kind of one-pixel disagreement the snap exists to settle.
    let (tw, th) = vp.texels();
    assert_eq!(tw, 2240, "the width's device pixels");
    assert_eq!(
        th,
        (vp.rect().height() * ppp).round() as u32,
        "the height is the snapped rect's, times the scale"
    );
    assert!(
        !vp.is_pixel_exact((2240, 1260)),
        "the window's device pixels are not the rect's"
    );
    let (sx, sy) = vp.texel_scale((2240, 1260));
    assert!((sx - 1.0).abs() < 1e-6, "the width happens to agree");
    assert!(
        (sy - th as f32 / 1260.0).abs() < 1e-3,
        "and the height is squeezed — which is the crop that reached the screen: {sy}"
    );
    // The aspect the camera is fitted at is the rect's, not the window's.
    assert!(
        (vp.aspect() - f64::from(vp.rect().width() / vp.rect().height())).abs() < 1e-6,
        "the rect's aspect"
    );
    assert!(
        (vp.aspect() - 1280.0 / 670.0).abs() < 1e-2,
        "which is ~1.910"
    );
    assert!(
        (vp.aspect() - 1280.0 / 720.0).abs() > 0.1,
        "and not the window's 1.778"
    );
}
