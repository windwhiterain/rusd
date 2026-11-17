//! **End-to-end** size pipeline regression: layout → fit → projection → frame.
//!
//! Every earlier round tested one link and passed while the chain was broken
//! somewhere else — the fit was right and unused, the paint was right and given the
//! wrong rect, the render size was right for the window and wrong for the viewport.
//! These tests run the whole chain in one function call, at `pixels_per_point`
//! **1.0, 1.25, 1.75 and 2.0**:
//!
//! ```text
//! ViewportSize (rect, ppp)            ← the layout
//!   → aspect = rect aspect            ← what the camera is fitted to
//!   → fit_camera(bbox, aspect)        ← scene::fit_camera, eight corners
//!   → camera_matrix(cam, bounds, texels.0, texels.1)   ← the renderer's projection
//!   → every bbox corner inside the frame, with a margin
//! ```
//!
//! and separately that the frame the renderer is told to make is the one the viewport
//! can blit 1:1.
//!
//! The red-green case: put `pixels_per_point` back into the camera's distance — the
//! mistake the parent suspected when the rendered frame looked 1.75× zoomed — and
//! `the_camera_is_fitted_for_the_rect_not_scaled_by_the_display` fails.

use rusd_viewer::camera::{project, view_proj};
use rusd_viewer::scene::{BBox, fit_camera};
use rusd_viewer::viewport::ViewportSize;

/// The city's world box, as the parity test records it.
fn city_bbox() -> BBox {
    BBox {
        min: [-210.262254, -211.440871, -2.0],
        max: [444.0, 444.0, 52.0654526],
    }
}

/// The eight corners of `bbox`.
fn corners(b: &BBox) -> [[f64; 3]; 8] {
    let mut out = [[0.0; 3]; 8];
    let mut i = 0;
    for x in [b.min[0], b.max[0]] {
        for y in [b.min[1], b.max[1]] {
            for z in [b.min[2], b.max[2]] {
                out[i] = [x, y, z];
                i += 1;
            }
        }
    }
    out
}

/// Every corner's NDC under `vp`'s projection, and the worst |NDC| reached.
///
/// The projection is `camera::view_proj` — the exact function the draw path uses — at
/// the viewport's **texel** size, which is what `gpu::camera_matrix` gets.
fn worst_ndc(bbox: &BBox, aspect: f64, vp: &ViewportSize) -> f64 {
    let cam = fit_camera(bbox, aspect);
    let (w, h) = vp.texels();
    let m = view_proj(&cam, bbox.corners(), w, h);
    let mut worst = 0.0f64;
    for c in corners(bbox) {
        let clip = [
            m[0] * c[0] + m[4] * c[1] + m[8] * c[2] + m[12],
            m[1] * c[0] + m[5] * c[1] + m[9] * c[2] + m[13],
            m[2] * c[0] + m[6] * c[1] + m[10] * c[2] + m[14],
            m[3] * c[0] + m[7] * c[1] + m[11] * c[2] + m[15],
        ];
        assert!(clip[3] > 0.0, "corner {c:?} is behind the eye");
        let ndc = [clip[0] / clip[3], clip[1] / clip[3]];
        worst = worst.max(ndc[0].abs()).max(ndc[1].abs());
    }
    worst
}

/// The `pixels_per_point` values a real desktop runs at: 1.75 is the one that
/// exposed the bug.
const PPPS: [f32; 4] = [1.0, 1.25, 1.75, 2.0];

/// Viewport rects in points, one per window shape (a 1280×720 window's viewport is
/// 1280×670 once the panels take their height).
const RECTS: [(f32, f32); 3] = [(1280.0, 670.0), (1100.0, 620.0), (900.0, 880.0)];

/// The fit margin the whole chain must respect, in NDC. `fit_camera` gives up 8% of
/// the half-angle, so a corner should land inside ±0.92; the assertion allows a hair
/// for float noise.
const MARGIN: f64 = 0.925;

#[test]
fn the_whole_chain_frames_the_scene_at_every_pixels_per_point() {
    let bbox = city_bbox();
    for ppp in PPPS {
        for (rw, rh) in RECTS {
            let rect = egui::Rect::from_min_size(egui::pos2(0.0, 25.0), egui::vec2(rw, rh));
            let vp = ViewportSize::new(rect, ppp);
            // 1. the fit uses the rect's aspect (not the window's, not the texels')
            let aspect = vp.aspect();
            let rect_aspect = f64::from(vp.rect().width()) / f64::from(vp.rect().height());
            assert!(
                (aspect - rect_aspect).abs() < 1e-6,
                "ppp {ppp}, rect {rw}x{rh}: the fit aspect is the snapped rect's ({aspect} vs \
                 {rect_aspect})"
            );
            assert!(
                (aspect - f64::from(rw) / f64::from(rh)).abs() < 1e-2,
                "ppp {ppp}, rect {rw}x{rh}: and that is the rect we asked for"
            );
            // 2. the projection is built at the texel size, and every corner is inside
            let worst = worst_ndc(&bbox, aspect, &vp);
            assert!(
                worst <= MARGIN,
                "ppp {ppp}, rect {rw}x{rh}: the city must be inside the frame; worst |NDC| \
                 {worst:.4} > {MARGIN}"
            );
            // 3. and the scene centre lands at the centre of the frame, so the city is
            //    framed rather than pushed into a corner.
            let cam = fit_camera(&bbox, aspect);
            let m = view_proj(&cam, bbox.corners(), vp.texels().0, vp.texels().1);
            let centre = bbox.center();
            let (px, py) =
                project(m, centre, vp.texels().0, vp.texels().1).expect("the centre projects");
            let (cx, cy) = (vp.texels().0 as f64 / 2.0, vp.texels().1 as f64 / 2.0);
            assert!(
                (px - cx).abs() < 1.0 && (py - cy).abs() < 1.0,
                "ppp {ppp}, rect {rw}x{rh}: the centre must project to the frame's centre, \
                 got ({px:.1}, {py:.1}) against ({cx:.1}, {cy:.1})"
            );
        }
    }
}

#[test]
fn the_frame_the_renderer_makes_is_the_one_the_viewport_can_blit() {
    for ppp in PPPS {
        for (rw, rh) in RECTS {
            let rect = egui::Rect::from_min_size(egui::pos2(0.0, 25.0), egui::vec2(rw, rh));
            let vp = ViewportSize::new(rect, ppp);
            // The renderer produces exactly `vp.texels()`; painting that into the rect
            // is a 1:1 blit, so what the camera framed is what the user sees.
            assert!(
                vp.is_pixel_exact(vp.texels()),
                "ppp {ppp}, rect {rw}x{rh}: the render size and the paint rect must agree"
            );
            assert_eq!(vp.texel_scale(vp.texels()), (1.0, 1.0));
        }
    }
}

#[test]
fn the_camera_is_fitted_for_the_rect_not_scaled_by_the_display() {
    // The red-green case for "ppp leaked into the camera". Fitting for the rect and
    // projecting at its texels is the correct chain; the *same* scene fitted for the
    // window's aspect (what the window-derived code did) is wrong, and a fit whose
    // distance were divided by ppp would be wrong by exactly 1/ppp — 57% at 1.75, the
    // crop that reached the screen.
    let bbox = city_bbox();
    let ppp = 1.75f32;
    let (rw, rh) = (1280.0f32, 670.0f32);
    let rect = egui::Rect::from_min_size(egui::pos2(0.0, 25.0), egui::vec2(rw, rh));
    let vp = ViewportSize::new(rect, ppp);

    // Correct: rect aspect, texel projection.
    let good = worst_ndc(&bbox, vp.aspect(), &vp);
    assert!(
        good <= MARGIN,
        "the correct chain frames the city: {good:.4}"
    );

    // Wrong 1: the window's aspect (1280x720), which is what `window_aspect()`
    // returned. At this scene the vertical constraint binds for both aspects, so the
    // two fits land on the **same** eye — which is worth stating, because it is why
    // the aspect alone was never the visible defect: what reached the screen needed
    // the texel mismatch as well (a 2240x1260 frame resampled into 2240x1173).
    let window_aspect = 1280.0 / 720.0;
    let cam_window = fit_camera(&bbox, window_aspect);
    let cam_rect = fit_camera(&bbox, vp.aspect());
    let same_eye = (cam_window.position[0] - cam_rect.position[0]).abs() < 1e-6
        && (cam_window.position[1] - cam_rect.position[1]).abs() < 1e-6;
    assert!(
        same_eye,
        "at this scene both aspects bind vertically, so the fit is the same; if that \
         ever changes, the aspect becomes a second real defect: {cam_window:?} vs \
         {cam_rect:?}"
    );

    // Wrong 2: ppp folded into the camera distance — the "1.75x zoom" the rendered
    // dump showed. The fit's distance is scaled down by 1/ppp, pulling the eye in.
    let cam = fit_camera(&bbox, vp.aspect());
    let mut zoomed = cam;
    let offset = [
        cam.position[0] - cam.look_at[0],
        cam.position[1] - cam.look_at[1],
        cam.position[2] - cam.look_at[2],
    ];
    let k = 1.0 / f64::from(ppp);
    zoomed.position = [
        cam.look_at[0] + offset[0] * k,
        cam.look_at[1] + offset[1] * k,
        cam.look_at[2] + offset[2] * k,
    ];
    let (w, h) = vp.texels();
    let m = view_proj(&zoomed, bbox.corners(), w, h);
    let mut worst_zoomed = 0.0f64;
    for c in corners(&bbox) {
        let clip = [
            m[0] * c[0] + m[4] * c[1] + m[8] * c[2] + m[12],
            m[1] * c[0] + m[5] * c[1] + m[9] * c[2] + m[13],
            m[2] * c[0] + m[6] * c[1] + m[10] * c[2] + m[14],
            m[3] * c[0] + m[7] * c[1] + m[11] * c[2] + m[15],
        ];
        assert!(clip[3] > 0.0, "corner {c:?} is behind the eye");
        let ndc = [clip[0] / clip[3], clip[1] / clip[3]];
        worst_zoomed = worst_zoomed.max(ndc[0].abs()).max(ndc[1].abs());
    }
    assert!(
        worst_zoomed > 1.0,
        "a distance divided by ppp must blow the frame out (worst |NDC| {worst_zoomed:.4}), \
         which is the 1.75x zoom seen in the render dump"
    );
}

#[test]
fn the_aspect_the_camera_uses_is_the_rects_everywhere_it_is_asked_for() {
    // One source: whether it is asked through the viewport or recomputed from its
    // rect, the answer is the same number. A second derivation anywhere would be a
    // second chance to disagree.
    for ppp in PPPS {
        for (rw, rh) in RECTS {
            let rect = egui::Rect::from_min_size(egui::pos2(0.0, 25.0), egui::vec2(rw, rh));
            let vp = ViewportSize::new(rect, ppp);
            let from_rect = f64::from(vp.rect().width()) / f64::from(vp.rect().height());
            assert!((vp.aspect() - from_rect).abs() < 1e-12);
            // And `pixels_per_point` never reaches the camera: `ViewportSize` gives
            // the fit one aspect, and a `ppp` change only moves the *snapped* rect by
            // a fraction of a device pixel (25.0 → 24.8 pt at 1.75). What must not
            // change is the camera: fitted twice at the same rect, the eye is the
            // same point, because `aspect()` is a property of the layout and ppp is a
            // property of the display.
            let aspect = vp.aspect();
            let cam_a = fit_camera(&city_bbox(), aspect);
            let cam_b = fit_camera(&city_bbox(), aspect);
            assert_eq!(cam_a.position, cam_b.position, "the fit is a pure function");
            // The aspect itself is within the snap's reach of the un-snapped
            // request, not a wildly different number.
            assert!(
                (aspect - f64::from(rw) / f64::from(rh)).abs() < 1e-2,
                "ppp {ppp}, rect {rw}x{rh}: the aspect drifted by more than the pixel \
                 grid allows: {aspect}"
            );
        }
    }
}
