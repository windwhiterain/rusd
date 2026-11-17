//! Where the render thread's frame goes on screen, as pure arithmetic.
//!
//! The viewport is the one widget whose **content is a texture of a fixed texel
//! size** rather than something egui lays out, so it is the one place where a
//! coordinate-space slip shows up as a picture that is the wrong size in the wrong
//! corner instead of as a layout that looks slightly off. The rule is:
//!
//! - **the image covers the allocated rect exactly** — one texel of the frame per
//!   one *pixel* of the window, never scaled and never anchored anywhere but the
//!   rect's top-left;
//! - **the UV rectangle is the whole texture** — `(0,0)` to `(1,1)`, so no part of
//!   the frame is cropped and no wrapping is reached.
//!
//! Both are computed here, as values, so a test can assert them without a GPU and
//! the widget code cannot quietly pass a different rect (or a different UV) than
//! the one this function describes.
//!
//! # Why the frame is drawn at the rect's *point* size
//!
//! egui positions and sizes everything in **points**, and the painter multiplies by
//! `pixels_per_point` on the way to the GPU. The frame arrives in **texels** — the
//! render thread's viewport is a real pixel buffer of `window.inner_size()`. At
//! `pixels_per_point = 1.0` the two coincide; at 1.25 or 2.0 they do not, and that
//! is the only way a "full frame" can end up looking like it was magnified from the
//! top-left: the image was painted into a rect of the wrong *point* size for its
//! texel size, so the sampler stretched it. [`viewport_image`] therefore derives the
//! paint rect from the texel size and the current `pixels_per_point`, and
//! [`texel_scale`] reports the ratio so a mismatch is visible rather than silent.

use egui::emath::GuiRounding as _;
use egui::{Pos2, Rect, Vec2};

/// The rect a frame of `frame_texels` is painted into inside `viewport`, given the
/// UI's `pixels_per_point`.
///
/// `viewport` is the rect egui allocated for the viewport in **points**. The result
/// is that same rect: the frame fills it corner to corner. `frame_texels` and
/// `pixels_per_point` are taken so the caller cannot accidentally pass a rect
/// derived from the frame's texel count (which is what a "draw the buffer at its
/// own size" mistake looks like), and so any future letterboxing decision has one
/// place to live.
pub fn viewport_image(viewport: Rect, frame_texels: (u32, u32), pixels_per_point: f32) -> Rect {
    debug_assert!(
        frame_texels.0 > 0 && frame_texels.1 > 0,
        "a zero-sized frame has no image to paint"
    );
    debug_assert!(pixels_per_point > 0.0, "pixels_per_point must be positive");
    // The frame fills the viewport exactly — and on the **pixel grid**: egui's
    // `paint_at` rounds the rect it is given to whole device pixels
    // (`Rect::round_to_pixels`), so a rect left at a fractional point position
    // (y = 20.2 at ppp 1.25) would be painted at the rounded position while the
    // layout still reports the fractional one. Rounding here makes the two agree,
    // and an image that is off by a fraction of a pixel is what leaves a seam
    // between the panels and the viewport.
    viewport.round_to_pixels(pixels_per_point)
}

/// The UV rectangle: the whole texture, `(0,0)` top-left to `(1,1)` bottom-right.
///
/// A constant, expressed as a function so every caller (and the test) asks the same
/// place rather than writing `Rect::from_min_max(pos2(0,0), pos2(1,1))` again — the
/// one form that is easy to get subtly wrong (`Rect::ZERO` even means "no texture").
pub fn viewport_uv() -> Rect {
    Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0))
}

/// The physical pixel size the render thread should render at so the frame is 1:1
/// with the window.
///
/// The UI is in points and the window's surface is in physical pixels; a frame
/// rendered at the *point* size (or at a stale size) is then resampled by the
/// sampler to the rect's physical size — which is precisely "magnified, anchored at
/// the top-left" when the frame is smaller than the window. Multiplying here keeps
/// the render size and the paint rect in the same space.
pub fn render_texel_size(window_points: Vec2, pixels_per_point: f32) -> (u32, u32) {
    let ppp = pixels_per_point.max(0.1);
    let w = (window_points.x * ppp).round().max(1.0) as u32;
    let h = (window_points.y * ppp).round().max(1.0) as u32;
    (w, h)
}

/// How much the sampler must scale a `frame_texels` image to fill a `viewport` of
/// `viewport_points` at `pixels_per_point`: `(1,1)` means one texel per device
/// pixel, i.e. a pixel-exact blit.
///
/// The test asserts this is `(1,1)`, which is what "1:1 铺满" means in numbers.
pub fn texel_scale(
    viewport_points: Vec2,
    frame_texels: (u32, u32),
    pixels_per_point: f32,
) -> (f32, f32) {
    if frame_texels.0 == 0 || frame_texels.1 == 0 {
        return (f32::INFINITY, f32::INFINITY);
    }
    // Device pixels the image is asked to cover, over its own texel count.
    let device_w = viewport_points.x * pixels_per_point;
    let device_h = viewport_points.y * pixels_per_point;
    (
        device_w / frame_texels.0 as f32,
        device_h / frame_texels.1 as f32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZES: [(f32, f32); 3] = [(1100.0, 620.0), (900.0, 900.0), (1375.0, 800.0)];

    #[test]
    fn the_image_rect_is_the_allocated_rect_at_every_pixels_per_point() {
        for (w, h) in SIZES {
            for ppp in [1.0f32, 1.25, 1.5, 2.0] {
                let viewport = Rect::from_min_size(Pos2::new(0.0, 22.0), Vec2::new(w, h));
                // The frame is the window's physical pixels — the case that must be
                // 1:1.
                let texels = render_texel_size(Vec2::new(w, h), ppp);
                let image = viewport_image(viewport, texels, ppp);
                // The rect is the viewport's own rect, snapped to the pixel grid (a
                // whole device pixel's worth of slack, expressed in points).
                let slack = 1.0 / ppp + 1e-3;
                assert!(
                    (image.min - viewport.min).length() <= slack
                        && (image.max - viewport.max).length() <= slack,
                    "ppp {ppp}: the image must cover the viewport, not a rect derived \
                     from the frame's texel count (image {image:?}, viewport {viewport:?})"
                );
                assert_eq!(
                    viewport_uv(),
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))
                );
            }
        }
    }

    #[test]
    fn the_texture_is_whole_and_not_wrapped() {
        let uv = viewport_uv();
        assert_eq!(uv.min, Pos2::ZERO, "UV starts at the texture's top-left");
        assert_eq!(uv.max, Pos2::new(1.0, 1.0), "and ends at its bottom-right");
        assert_ne!(uv, Rect::ZERO, "Rect::ZERO would mean 'no texture at all'");
    }

    #[test]
    fn the_render_size_is_the_window_in_physical_pixels() {
        // The render thread's viewport must be the window's *device* pixels, or the
        // sampler stretches the frame — the "2× from the top-left" picture.
        assert_eq!(
            render_texel_size(Vec2::new(1100.0, 620.0), 1.0),
            (1100, 620)
        );
        assert_eq!(
            render_texel_size(Vec2::new(1100.0, 620.0), 1.25),
            (1375, 775)
        );
        assert_eq!(
            render_texel_size(Vec2::new(1100.0, 620.0), 2.0),
            (2200, 1240)
        );
        // Never zero, whatever the window does.
        assert_eq!(render_texel_size(Vec2::ZERO, 1.0), (1, 1));
    }

    #[test]
    fn a_frame_at_the_windows_physical_size_is_a_pixel_exact_blit() {
        for (w, h) in SIZES {
            for ppp in [1.0f32, 1.25, 1.5, 2.0] {
                let points = Vec2::new(w, h);
                let texels = render_texel_size(points, ppp);
                let (sx, sy) = texel_scale(points, texels, ppp);
                // The render size is rounded to whole pixels (`round` in
                // `render_texel_size`), so the ratio is 1 up to that rounding — a
                // sub-pixel of slack on a 4-digit width, never a visible stretch.
                let slack = 1.0 / (texels.0 as f32);
                assert!(
                    (sx - 1.0).abs() <= slack && (sy - 1.0).abs() <= slack,
                    "ppp {ppp}: one texel must be one device pixel ({sx}, {sy})"
                );
            }
        }
    }

    #[test]
    fn a_frame_rendered_at_the_point_size_is_detectably_stretched() {
        // The mistake this module exists to prevent: render at the window's *point*
        // size while the surface is `ppp` times bigger. The sampler then blows the
        // image up, and `texel_scale` names the factor instead of hiding it.
        let points = Vec2::new(1100.0, 620.0);
        let wrong = (1100, 620);
        let (sx, _sy) = texel_scale(points, wrong, 2.0);
        assert!(
            (sx - 2.0).abs() < 1e-6,
            "a point-sized frame on a 2x surface is stretched 2x (got {sx})"
        );
    }
}
