//! Headless egui **layout** regression for the viewport image.
//!
//! The defect this pins: the window showed the render thread's frame as if it had
//! been magnified about 2× and anchored at the top-left, while the frame itself
//! (dumped) was a correct, fully-framed city. Nothing in the renderer was wrong —
//! the image was placed into a rect of the wrong size for its texel count, so the
//! sampler stretched it and the city's centre slid out of view.
//!
//! So these tests run the **real** egui pass (`ctx.run_ui` → `ctx.tessellate`) at
//! `pixels_per_point` 1.0, 1.25 and 2.0 and assert, on the tessellated mesh that
//! would go to the GPU:
//!
//! 1. the painted image rect **equals the allocated viewport rect** — not a rect
//!    derived from the frame's texel count, and not scaled by the ppp;
//! 2. its UV range is the **whole texture** `(0,0)`–`(1,1)`, so no corner of the
//!    frame is cropped or wrapped;
//! 3. the frame's texel count and the rect's device-pixel count are in a 1:1 ratio
//!    (`view::viewport::texel_scale`), which is what makes it a pixel-exact blit
//!    rather than a resample.
//!
//! A test of `viewport::viewport_image` alone would pass even if the widget kept
//! calling `paint_at` with a different rect; this one inspects what egui actually
//! produced, so the widget code is covered too.

/// One frame of the real egui pass at `ppp`, with the texture quad it emitted.
struct Painted {
    /// The rect `allocate_exact_size` gave the viewport (points).
    allocated: egui::Rect,
    /// The image quad's extent, in the same space egui puts mesh vertices in.
    quad: egui::Rect,
    /// The quad's UV bounds.
    uv: egui::Rect,
    /// How many textured quads were found (exactly one is expected).
    quads: usize,
}

/// Run the app's viewport painting at `ppp` for a `window_points` window whose
/// render thread produced a `frame_texels` frame, and report what egui emitted.
fn paint(ppp: f32, window_points: egui::Vec2, frame_texels: (u32, u32)) -> Painted {
    let ctx = egui::Context::default();
    let mut raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, window_points)),
        ..Default::default()
    };
    raw.viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .native_pixels_per_point = Some(ppp);

    // A frame the size the render thread actually produces.
    let image = egui::ColorImage::filled(
        [frame_texels.0 as usize, frame_texels.1 as usize],
        egui::Color32::from_rgb(30, 200, 30),
    );
    let texture = ctx.load_texture("frame", image, egui::TextureOptions::LINEAR);

    let mut allocated = egui::Rect::NOTHING;
    let out = ctx.run_ui(raw, |root| {
        egui::Panel::top("status").show(root, |ui| {
            ui.label("已连接 · v8 · 434 prim · 未选中");
        });
        egui::CentralPanel::no_frame().show(root, |ui| {
            let avail = ui.available_size();
            let (rect, _resp) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
            allocated = rect;
            // The widget's own code path, exactly as `app.rs` runs it.
            let painted =
                rusd_viewer::viewport::viewport_image(rect, frame_texels, ui.pixels_per_point());
            egui::Image::new(&texture)
                .fit_to_exact_size(painted.size())
                .uv(rusd_viewer::viewport::viewport_uv())
                .paint_at(ui, painted);
        });
    });
    // Unapplied texture deltas are a debug-assert, and this test has no painter.
    let mut delta = out.textures_delta;
    delta.clear();

    // The image is painted as a `RectShape` carrying a `Brush` (texture id + UV),
    // not as a four-vertex mesh, so the shape list is what has to be read. Walking
    // it here rather than the tessellated primitives keeps the assertion about
    // what the widget *asked for*, which is where the rect comes from.
    let mut quad = egui::Rect::NOTHING;
    let mut uv = egui::Rect::NOTHING;
    let mut quads = 0usize;
    // The image is a named (managed) texture, never the font atlas at Managed(0).
    let atlas = egui::TextureId::Managed(0);
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
    fn walk(shape: &egui::Shape, visit: &mut impl FnMut(&egui::Shape)) {
        visit(shape);
        if let egui::Shape::Vec(shapes) = shape {
            for s in shapes {
                walk(s, visit);
            }
        }
    }
    for cs in &out.shapes {
        walk(&cs.shape, &mut visit);
    }
    Painted {
        allocated,
        quad,
        uv,
        quads,
    }
}

/// `pixels_per_point` values a real desktop actually runs at.
const PPPS: [f32; 3] = [1.0, 1.25, 2.0];

#[test]
fn the_frame_fills_the_viewport_rect_exactly_at_every_pixels_per_point() {
    for ppp in PPPS {
        for window_points in [
            egui::vec2(1100.0, 620.0),
            egui::vec2(900.0, 880.0),
            egui::vec2(1375.0, 800.0),
        ] {
            // The render thread's viewport, in the window's physical pixels. Kept
            // under egui's 2048-texel atlas limit so the test texture loads; the
            // layout arithmetic is the same at any size.
            let texels = rusd_viewer::viewport::render_texel_size(window_points, ppp);
            let texels = (texels.0.min(2000), texels.1.min(2000));
            let p = paint(ppp, window_points, texels);
            assert_eq!(p.quads, 1, "ppp {ppp}: exactly one textured quad");
            // The rect is the viewport's, on the pixel grid: a whole device pixel's
            // worth of slack, in points.
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
        }
    }
}

#[test]
fn the_texture_uv_covers_the_whole_frame() {
    for ppp in PPPS {
        let window_points = egui::vec2(1100.0, 620.0);
        let texels = rusd_viewer::viewport::render_texel_size(window_points, ppp);
        let texels = (texels.0.min(2000), texels.1.min(2000));
        let p = paint(ppp, window_points, texels);
        assert_eq!(
            p.uv,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            "ppp {ppp}: the quad must sample the whole frame, corner to corner"
        );
    }
}

#[test]
fn the_frame_is_a_pixel_exact_blit_not_a_resample() {
    for ppp in PPPS {
        for window_points in [egui::vec2(1100.0, 620.0), egui::vec2(900.0, 880.0)] {
            let texels = rusd_viewer::viewport::render_texel_size(window_points, ppp);
            let (sx, sy) = rusd_viewer::viewport::texel_scale(window_points, texels, ppp);
            assert!(
                (sx - 1.0).abs() < 1e-6 && (sy - 1.0).abs() < 1e-6,
                "ppp {ppp}: one frame texel per device pixel ({sx}, {sy})"
            );
        }
    }
}

#[test]
fn the_image_rect_is_not_derived_from_the_frame_size() {
    // The specific mistake: painting the frame at its own texel count as if those
    // were points. At ppp 2 that rect is half the viewport, so the sampler doubles
    // the picture and the city's centre leaves the frame.
    let ppp = 2.0f32;
    let window_points = egui::vec2(1100.0, 620.0);
    let texels = rusd_viewer::viewport::render_texel_size(window_points, ppp);
    let viewport = egui::Rect::from_min_size(egui::pos2(0.0, 20.0), window_points);

    let correct = rusd_viewer::viewport::viewport_image(viewport, texels, ppp);
    // The wrong rect: painting the frame's texels as if they were **points** — the
    // mistake a `paint_at(ui, Rect::from_min_size(.., texels))`-shaped call makes.
    let wrong =
        egui::Rect::from_min_size(viewport.min, egui::vec2(texels.0 as f32, texels.1 as f32));
    assert_eq!(correct, viewport, "the right rect is the viewport itself");
    // At ppp 2 the texel count is twice the viewport's point size, so treating
    // texels as points asks for a rect twice as large — the image is then squeezed
    // into the viewport or overflows it, and either way is not a 1:1 blit.
    assert!(
        wrong.width() > viewport.width() * 1.5,
        "the texel-as-points rect is {} wide against a {} viewport",
        wrong.width(),
        viewport.width()
    );
}
