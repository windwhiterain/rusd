//! The viewport's size, in **one** place.
//!
//! The window kept showing the city as a cropped corner of itself because the
//! viewport's size had four independent sources that disagreed:
//!
//! | what | where it came from | value on the machine that showed the bug |
//! | --- | --- | --- |
//! | the rect egui allocates for the viewport | the layout, in **points** | 1280 × 670 |
//! | the render thread's framebuffer | `window.inner_size()`, **physical px** | 2240 × 1260 |
//! | the aspect the camera was fitted at | `window.inner_size()` (`1.777`) | ≠ the rect's `1.910` |
//! | the image's paint rect | the frame's texel count, read as points | 2240 × 1260 "points" |
//!
//! Every pair of those is a different number, so the picture was rendered at one
//! aspect, stretched into a rect of another, and finally drawn at a texel-derived
//! size — which lands on screen as the top-left `1 / pixels_per_point` of the frame,
//! magnified (57% at the 1.75 scale factor that exposed it).
//!
//! [`ViewportSize`] is the fix: one struct that carries the rect's **point** size,
//! the `pixels_per_point` it is laid out at, and the **texel** size derived from
//! them — and nothing else may invent a size. The rules it enforces:
//!
//! - `texels == rect × pixels_per_point`, rounded once, so the renderer's framebuffer
//!   is exactly the device-pixel area the rect covers;
//! - [`ViewportSize::aspect`] is the **rect's** aspect (not the window's), so the
//!   camera is fitted to the shape it will actually be drawn into;
//! - the paint rect is [`ViewportSize::rect`], so the image covers it exactly;
//! - **`pixels_per_point` never reaches the camera.** It is a property of the
//!   display, not of the scene; the only place it is allowed to multiply anything is
//!   `texels`, here.

use egui::emath::GuiRounding as _;
use egui::{Pos2, Rect, Vec2};

/// The viewport's geometry: the single source of truth for every size the client
/// uses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewportSize {
    /// The rect egui allocated for the viewport, in **points**, on the pixel grid.
    rect: Rect,
    /// The display scale the rect is laid out at.
    pixels_per_point: f32,
    /// The framebuffer size, in **texels** (`rect · pixels_per_point`).
    texels: (u32, u32),
}

impl ViewportSize {
    /// The viewport layout for a rect allocated at `pixels_per_point`.
    ///
    /// `rect` is snapped to the pixel grid, and the texel size is derived from the
    /// snapped rect — so a fractional point position (y = 20.2 at a 1.25 scale)
    /// cannot leave the framebuffer a pixel short of the area it is drawn into.
    pub fn new(rect: Rect, pixels_per_point: f32) -> Self {
        let ppp = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
            pixels_per_point
        } else {
            1.0
        };
        let rect = rect.round_to_pixels(ppp);
        let texels = (
            (rect.width() * ppp).round().max(1.0) as u32,
            (rect.height() * ppp).round().max(1.0) as u32,
        );
        Self {
            rect,
            pixels_per_point: ppp,
            texels,
        }
    }

    /// The rect to allocate and to paint the frame into, in points.
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// The display scale the rect is laid out at.
    pub fn pixels_per_point(&self) -> f32 {
        self.pixels_per_point
    }

    /// The framebuffer size the render thread must use, in texels.
    pub fn texels(&self) -> (u32, u32) {
        self.texels
    }

    /// `width / height` of the **rect** — what the camera is fitted to, because it
    /// is the shape the picture is drawn into. Not the window's aspect: the status
    /// bar and the controls take their height out of the window first, so the two
    /// differ whenever the window is not exactly the panels' shape.
    pub fn aspect(&self) -> f64 {
        let s = self.rect.size();
        if s.y <= 0.0 {
            return 16.0 / 9.0;
        }
        f64::from(s.x) / f64::from(s.y)
    }

    /// The UV rectangle to paint with: the whole texture, `(0,0)`–`(1,1)`.
    pub fn uv(&self) -> Rect {
        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))
    }

    /// How much the sampler must scale a `frame_texels` image to cover the rect:
    /// `(1, 1)` is a pixel-exact blit, which is what this type exists to arrange.
    pub fn texel_scale(&self, frame_texels: (u32, u32)) -> (f32, f32) {
        if frame_texels.0 == 0 || frame_texels.1 == 0 {
            return (f32::INFINITY, f32::INFINITY);
        }
        (
            self.texels.0 as f32 / frame_texels.0 as f32,
            self.texels.1 as f32 / frame_texels.1 as f32,
        )
    }

    /// Whether a frame of `frame_texels` covers this rect exactly — the check a
    /// debug overlay reports and a test asserts.
    pub fn is_pixel_exact(&self, frame_texels: (u32, u32)) -> bool {
        let (sx, sy) = self.texel_scale(frame_texels);
        (sx - 1.0).abs() <= 1.0 / self.texels.0 as f32
            && (sy - 1.0).abs() <= 1.0 / self.texels.1 as f32
    }
}

/// A viewport whose size is known only as a **window** size, for the frames before
/// the first layout and for tests: the rect is taken as the whole window.
///
/// This is the one place a window size is allowed to stand in for a rect, and it
/// exists only because the render thread must be started before egui has laid out
/// anything. The first real layout replaces it (`Shell::sync_viewport`).
pub fn whole_window(window_points: Vec2, pixels_per_point: f32) -> ViewportSize {
    ViewportSize::new(
        Rect::from_min_size(Pos2::ZERO, window_points),
        pixels_per_point,
    )
}

/// What the `--debug-ui` overlay reports, as data so a test can assert it and a
/// screenshot can be read without a second probe.
///
/// Three rounds of guessing about sizes and frame freshness ended in screenshots; this
/// is the answer to the questions those rounds kept posing: what scale is the UI at,
/// how big is the rect, how big is the frame, are they the same size, and is the frame
/// on screen the latest one. If `fresh` is false the window is showing an older frame,
/// and `texels_match` says whether the picture is being resampled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayReport {
    pub pixels_per_point: f32,
    pub rect_points: (f32, f32),
    pub viewport_texels: (u32, u32),
    pub frame_texels: (u32, u32),
    /// Whether the frame on screen has the viewport's texel size (a 1:1 blit).
    pub texels_match: bool,
    /// Frames the render thread has delivered, and how many have been shown.
    pub frames_received: u64,
    pub frames_shown: u64,
    /// The camera the viewport is drawn with.
    pub orbit_distance: f64,
    pub orbit_target: [f64; 3],
}

impl OverlayReport {
    /// Whether the frame on screen is the latest one delivered.
    pub fn fresh(&self) -> bool {
        self.frames_shown >= self.frames_received
    }

    /// The overlay's lines, one per fact. Kept as text here so the same strings a
    /// screenshot shows are what a test asserts.
    pub fn lines(&self) -> Vec<String> {
        vec![
            format!("ppp {:.2}", self.pixels_per_point),
            format!(
                "rect {:.0}x{:.0} pt (aspect {:.3})",
                self.rect_points.0,
                self.rect_points.1,
                if self.rect_points.1 > 0.0 {
                    self.rect_points.0 / self.rect_points.1
                } else {
                    0.0
                }
            ),
            format!(
                "viewport tex {}x{} · frame tex {}x{} · 1:1 {}",
                self.viewport_texels.0,
                self.viewport_texels.1,
                self.frame_texels.0,
                self.frame_texels.1,
                if self.texels_match { "yes" } else { "NO" }
            ),
            format!(
                "frames {} received / {} shown · fresh {}",
                self.frames_received,
                self.frames_shown,
                if self.fresh() { "yes" } else { "NO" }
            ),
            format!(
                "orbit dist {:.1} target ({:.1}, {:.1}, {:.1})",
                self.orbit_distance,
                self.orbit_target[0],
                self.orbit_target[1],
                self.orbit_target[2]
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The machine that showed the bug: 1.75 scale, a 1280×720 window, and the
    /// viewport rect 1280×670 points (the status bar and controls take 50).
    const BUG_CASE: (f32, Vec2) = (1.75, egui::vec2(1280.0, 670.0));

    #[test]
    fn the_overlay_says_whether_the_frame_on_screen_is_fresh() {
        let mut r = OverlayReport {
            pixels_per_point: 1.75,
            rect_points: (1280.0, 670.0),
            viewport_texels: (2240, 1173),
            frame_texels: (2240, 1173),
            texels_match: true,
            frames_received: 7,
            frames_shown: 7,
            orbit_distance: 1287.0,
            orbit_target: [116.9, 116.3, 25.0],
        };
        assert!(r.fresh(), "shown == received is fresh");
        let lines = r.lines().join(" | ");
        assert!(lines.contains("ppp 1.75"), "{lines}");
        assert!(lines.contains("rect 1280x670 pt"), "{lines}");
        assert!(lines.contains("1:1 yes"), "{lines}");
        assert!(lines.contains("fresh yes"), "{lines}");

        // A frame that has not reached the screen yet: the overlay must say "NO"
        // in both places that would let a screenshot be misread.
        r.frames_received = 8;
        r.frame_texels = (2240, 1260);
        r.texels_match = false;
        assert!(!r.fresh());
        let lines = r.lines().join(" | ");
        assert!(lines.contains("1:1 NO"), "{lines}");
        assert!(lines.contains("fresh NO"), "{lines}");
        assert!(lines.contains("8 received / 7 shown"), "{lines}");
    }

    #[test]
    fn the_overlay_reports_the_bug_case_as_both_wrong() {
        // The numbers the parent's probe logged: a 2240x1260 frame in a 1280x670 pt
        // rect at 1.75 — not 1:1, and (before the fix) not fresh either.
        let vp = whole_window(egui::vec2(1280.0, 670.0), 1.75);
        let report = OverlayReport {
            pixels_per_point: vp.pixels_per_point(),
            rect_points: (vp.rect().width(), vp.rect().height()),
            viewport_texels: vp.texels(),
            frame_texels: (2240, 1260),
            texels_match: vp.is_pixel_exact((2240, 1260)),
            frames_received: 2,
            frames_shown: 1,
            orbit_distance: 1287.0,
            orbit_target: [0.0; 3],
        };
        assert!(!report.texels_match, "the window-sized frame is not 1:1");
        assert!(!report.fresh(), "and it was not the latest frame");
        let lines = report.lines().join(" | ");
        assert!(lines.contains("viewport tex 2240x1173"), "{lines}");
        assert!(lines.contains("frame tex 2240x1260"), "{lines}");
    }

    #[test]
    fn texels_are_the_rect_times_the_scale_never_the_window() {
        let (ppp, rect_pts) = BUG_CASE;
        let vp = whole_window(rect_pts, ppp);
        // The rect's own device pixels, which is what the renderer must produce. The
        // height is the *snapped* rect's (the grid rounds a fractional point edge),
        // so it is derived rather than written down.
        let (tw, th) = vp.texels();
        assert_eq!(tw, 2240, "1280 pt at 1.75");
        assert_eq!(
            th,
            (vp.rect().height() * ppp).round() as u32,
            "670 pt, snapped, at 1.75"
        );
        // The window's inner size would have given 2240x1260 — the bug.
        let window = whole_window(egui::vec2(1280.0, 720.0), ppp);
        assert_eq!(window.texels(), (2240, 1260));
        assert_ne!(
            vp.texels(),
            window.texels(),
            "the rect and the window are different sizes; the rect must win"
        );
    }

    #[test]
    fn the_fit_aspect_is_the_rects_not_the_windows() {
        let (ppp, rect_pts) = BUG_CASE;
        let vp = whole_window(rect_pts, ppp);
        // The stored rect is snapped to the pixel grid, so the aspect is that rect's
        // — not the un-snapped input's, which is off by a fraction of a device pixel.
        let snapped = vp.rect().width() / vp.rect().height();
        // `aspect()` divides in f64 from the rect's f32 sizes, so the two agree to
        // f32 precision — not to the last bit of an f64.
        assert!(
            (vp.aspect() - f64::from(snapped)).abs() < 1e-6,
            "aspect {} vs rect's {snapped}",
            vp.aspect()
        );
        assert!(
            (vp.aspect() - 1280.0 / 670.0).abs() < 1e-2,
            "the rect's aspect, 1.910 up to the pixel grid: {}",
            vp.aspect()
        );
        let window = whole_window(egui::vec2(1280.0, 720.0), ppp);
        assert!(
            (window.aspect() - 1280.0 / 720.0).abs() < 1e-2,
            "window aspect 1.778"
        );
        assert!(
            (vp.aspect() - window.aspect()).abs() > 0.1,
            "the two aspects are far apart: fitting to the window is the bug"
        );
    }

    #[test]
    fn a_frame_of_the_viewport_size_is_a_pixel_exact_blit() {
        for ppp in [1.0f32, 1.25, 1.75, 2.0] {
            for rect_pts in [
                egui::vec2(1280.0, 670.0),
                egui::vec2(1100.0, 620.0),
                egui::vec2(900.0, 880.0),
            ] {
                let vp = whole_window(rect_pts, ppp);
                assert!(
                    vp.is_pixel_exact(vp.texels()),
                    "ppp {ppp}, rect {rect_pts:?}: a frame of the viewport's own texel \
                     size must be 1:1, scale {:?}",
                    vp.texel_scale(vp.texels())
                );
                assert_eq!(vp.texel_scale(vp.texels()), (1.0, 1.0));
            }
        }
    }

    #[test]
    fn a_frame_rendered_from_the_window_size_is_detectably_stretched() {
        // The bug's arithmetic: a 2240x1260 frame (window-sized) into a 1280x670 pt
        // rect at 1.75 covers 2240x1173 device pixels, so the sampler squeezes it
        // vertically — and the aspect is wrong so the picture is not even the right
        // shape.
        let vp = whole_window(egui::vec2(1280.0, 670.0), 1.75);
        let (_, th) = vp.texels();
        let (sx, sy) = vp.texel_scale((2240, 1260));
        assert!(
            (sx - 1.0).abs() < 1e-6,
            "same width, so no horizontal stretch"
        );
        assert!(
            (sy - th as f32 / 1260.0).abs() < 1e-3,
            "the frame is squeezed vertically instead of blitted: {sy}"
        );
        assert!(!vp.is_pixel_exact((2240, 1260)));
    }

    #[test]
    fn the_paint_rect_is_the_layout_rect_and_the_uv_is_whole() {
        for ppp in [1.0f32, 1.25, 1.75, 2.0] {
            let layout = Rect::from_min_size(egui::pos2(0.0, 25.0), egui::vec2(1280.0, 670.0));
            let vp = ViewportSize::new(layout, ppp);
            // The rect the widget allocates and the rect it paints must be one rect.
            assert_eq!(vp.rect(), layout.round_to_pixels(ppp));
            // And no tolerance here: small differences at the corners are the seam.
            assert_eq!(vp.uv(), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)));
        }
    }

    #[test]
    fn a_zero_or_nonsense_scale_falls_back_to_one() {
        for bad in [0.0f32, -1.0, f32::NAN, f32::INFINITY] {
            let vp = ViewportSize::new(
                Rect::from_min_size(Pos2::ZERO, egui::vec2(100.0, 50.0)),
                bad,
            );
            assert_eq!(vp.pixels_per_point(), 1.0, "scale {bad} is not usable");
            assert_eq!(vp.texels(), (100, 50));
        }
    }
}
