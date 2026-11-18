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

/// What the panels leave for the viewport: the window's point size, minus the left
/// panel's width, the top bar's height and the bottom bar's height.
///
/// This is the arithmetic a screenshot caught being violated: smoke17 reported a
/// viewport rect of 910×713 pt inside a window of 823×457 pt — a rect *bigger than
/// the window it sits in*, which pushed the city off the right-hand edge. The rect
/// must be what the layout has left after the panels, so the sizes must add up:
/// `left + viewport == window (width)` and `top + bottom + viewport == window
/// (height)`. Stated as a function so a headless test can assert it at every scale.
pub struct PanelBudget {
    /// The window's client area, in points.
    pub window: Vec2,
    /// The left panel's width, in points.
    pub left: f32,
    /// The top bar's height, in points.
    pub top: f32,
    /// The controls bar's height, in points.
    pub bottom: f32,
}

impl PanelBudget {
    /// The space the panels leave for the viewport.
    pub fn remaining(&self) -> Vec2 {
        Vec2::new(
            (self.window.x - self.left).max(1.0),
            (self.window.y - self.top - self.bottom).max(1.0),
        )
    }

    /// Whether `rect` is exactly that space — the property the viewport's allocation
    /// must have, and the one whose absence produced a rect wider than the window.
    pub fn fits(&self, rect: Rect) -> bool {
        let want = self.remaining();
        // A whole device pixel's worth of slack, for the layout's own rounding.
        (rect.width() - want.x).abs() <= 1.0 && (rect.height() - want.y).abs() <= 1.0
    }

    /// Whether `rect` could be a *stale* or *window-derived* rect: bigger than the
    /// space the panels leave, which is the defect this type exists to name.
    pub fn overflows(&self, rect: Rect) -> bool {
        rect.width() > self.remaining().x + 1.0 || rect.height() > self.remaining().y + 1.0
    }
}

/// The window's **screen space**: the three numbers that either agree or do not.
///
/// A DPI defect is never a mystery once these three are printed beside each other,
/// because the contract every egui integration relies on is one equation:
///
/// ```text
/// screen_points == inner_physical / scale_factor
/// ```
///
/// and the ways it can fail are the ways that equation can be broken:
///
/// | `screen_points` | what it means |
/// | --- | --- |
/// | `inner / scale` | correct: points are the window divided by the scale |
/// | `inner` | the scale was **never applied**: pixels used as points, so every point
///   is drawn `scale` times too large and the UI overflows the window |
/// | `inner / scale²` | the scale was applied **twice**: the layout believes the window
///   is bigger than it is, so its right and bottom edges fall outside the window |
///
/// The round that produced this type had a brief saying "the scale is counted
/// twice"; the numbers in that brief say otherwise — `2240 / 1.75 = 1280.0` and
/// `2560 / 1.75 = 1462.9` are exactly the `screen_rect` it reported, in both the
/// default and the maximized state. That is the **correct** row of the table. Only a
/// printed ratio settles it, which is why it is printed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreenSpace {
    /// `Window::inner_size()`: the client area, in **physical** pixels.
    pub inner_physical: (u32, u32),
    /// `Window::scale_factor()`: the display's scaling, per monitor.
    pub scale_factor: f32,
    /// The `RawInput::screen_rect` size egui laid this pass out in, in **points**.
    pub screen_points: Vec2,
    /// The size the egui presenter's back buffer was last told, in **physical**
    /// pixels: `inner_size()` when it was created, and on every `Resized` since.
    ///
    /// It is a separate number on purpose. The back buffer is the window's own
    /// swapchain, and `egui_wgpu::winit::Painter` configures it only in
    /// `set_window` and `on_window_resized` — a viewer that never calls the second
    /// keeps a startup-sized back buffer and asks the compositor to stretch it
    /// (`DXGI_SCALING_STRETCH`), which magnifies the whole UI by `new / old`. That is
    /// the same family of defect as a wrong point space, and it is invisible in every
    /// other number.
    pub presented_physical: (u32, u32),
    /// What the **operating system** said about the same window, when a platform probe
    /// exists: `(client area px, os scale, outer px)`.
    ///
    /// This is the second opinion round 9 lacked. Every other field here is winit's own
    /// answer, and `inner_size()` **is** `GetClientRect` — so a comparison among them
    /// proves only that winit is self-consistent. Round 10's window showed
    /// `scale count 1.000x · ok`, `present stretch 1.000x1.000 · blit` and
    /// `partition OK` while the viewport was wider than the screen, because the two
    /// numbers whose ratio it printed came from the same division. This field, and the
    /// `os` lines built from it, are the only place a wrong answer can show up.
    ///
    /// (Round 9's build printed that first row as `scale count`; it is `winit scale
    /// count` now, renamed when the second opinion arrived — the row says whose numbers
    /// it compares.)
    ///
    /// `None` on a platform without the probe (and on a failed read): no second
    /// opinion, so no agreement is claimed — see [`ScreenSpace::os_agrees`].
    pub os: Option<OsView>,
}

/// The OS's own view of the window, as [`crate::platform_metrics`] reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OsView {
    /// `GetClientRect`: the client area in physical pixels.
    pub client_px: (u32, u32),
    /// `dpi / 96`: the scale the window is really on.
    pub scale: f32,
    /// `GetWindowRect`: the whole window in physical pixels.
    pub window_px: (u32, u32),
    /// The process/thread DPI awareness word (`unaware` / `system` / `per-monitor`).
    pub awareness: &'static str,
    /// The monitor's physical size, when it could be read.
    ///
    /// The datum that separates "the layout is bigger than the window" from "the window
    /// is bigger than the screen". Round 10's client area was `2496` physical pixels,
    /// which cannot fit a 1920-wide monitor — so if the city hung off the edge with the
    /// partition identity holding, this is the line that says why.
    pub monitor_px: Option<(u32, u32)>,
}

/// Which row of [`ScreenSpace`]'s table the platform's numbers are on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenAgreement {
    /// `screen_points == inner / scale`: the contract holds.
    Consistent,
    /// `screen_points == inner`: no division happened, so points are pixels.
    PixelsAsPoints,
    /// `screen_points == inner / scale²`: the scale was applied twice.
    ScaledTwice,
    /// Neither of the above, so the ratio itself is the only useful statement.
    Odd,
}

impl ScreenSpace {
    /// The point size the platform's own numbers imply: `inner / scale`.
    ///
    /// This is the arithmetic the `screen_rect` correction is made of, kept here so
    /// it is testable without a window. A non-finite or zero scale falls back to
    /// `1.0`, the same rule [`ViewportSize::new`] applies.
    ///
    /// **It is not an authority.** It is derived from [`Self::inner_physical`] and
    /// [`Self::scale_factor`], which are both winit's answers — so a caller that only
    /// reads this function is checking winit against itself. Use
    /// [`Self::os_expected_points`] when the OS's numbers are available.
    pub fn expected_points(&self) -> Vec2 {
        let s = self.scale();
        Vec2::new(
            self.inner_physical.0 as f32 / s,
            self.inner_physical.1 as f32 / s,
        )
    }

    /// The point size the **OS's** numbers imply: `os client px / os scale`.
    ///
    /// `None` when there is no probe or it could not read the window. This is the
    /// authority the layout should be judged against: it comes from `GetClientRect`
    /// and `GetDpiForWindow`, not from winit's cached `WindowState`.
    pub fn os_expected_points(&self) -> Option<Vec2> {
        let os = self.os?;
        if !os.scale.is_finite() || os.scale <= 0.0 {
            return None;
        }
        Some(Vec2::new(
            os.client_px.0 as f32 / os.scale,
            os.client_px.1 as f32 / os.scale,
        ))
    }

    /// Whether winit and the OS tell the same story, field by field.
    ///
    /// `None` with no probe. `Some(false)` is the round-10 state: winit's number and the
    /// OS's disagree, and which one is wrong has to be settled by the two lines that
    /// print both — not by this predicate.
    pub fn os_agrees(&self) -> Option<bool> {
        let os = self.os?;
        let same_size = os.client_px == self.inner_physical;
        let same_scale = (os.scale - self.scale()).abs() <= 1e-3;
        Some(same_size && same_scale)
    }

    /// Whether the two agree about the **window's size**, ignoring the scale.
    ///
    /// This is the one that a real window cannot fail: both numbers are the same
    /// `GetClientRect` on the same handle. It exists to separate the two failures — a
    /// size disagreement means the probe is reading a different window (a plumbing bug
    /// here), while a scale disagreement is winit's cached `WindowState` being stale,
    /// which is the mechanism a fix has to handle.
    pub fn client_only_agrees(&self) -> Option<bool> {
        self.os.map(|os| os.client_px == self.inner_physical)
    }

    /// The fractional error of the layout's point space against the OS's, per axis.
    ///
    /// `1.0` means the layout occupies exactly the window the OS describes. `1.75` is
    /// round 10's symptom — a point space 1.75x too large, so the right and bottom of
    /// the layout fall outside the window by `1 - 1/1.75` of its width. `None` with no
    /// probe.
    pub fn os_point_ratio(&self) -> Option<(f32, f32)> {
        let want = self.os_expected_points()?;
        let got = self.screen_points;
        let per = |g: f32, w: f32| if w > 0.0 { g / w } else { 1.0 };
        Some((per(got.x, want.x), per(got.y, want.y)))
    }

    /// The scale factor, or `1.0` when the platform reported something unusable.
    pub fn scale(&self) -> f32 {
        if self.scale_factor.is_finite() && self.scale_factor > 0.0 {
            self.scale_factor
        } else {
            1.0
        }
    }

    /// `screen_points.width / expected_points.width`: `1.0` is the contract met.
    pub fn ratio(&self) -> f32 {
        let e = self.expected_points().x;
        if !e.is_finite() || e <= 0.0 {
            return 1.0;
        }
        self.screen_points.x / e
    }

    /// Which row of the table these numbers are on.
    ///
    /// Both axes must agree within a point for `Consistent`: a width that is right
    /// and a height that is not is the shape of a defect that pins the back buffer
    /// rather than the scale, and it must not pass as consistent.
    pub fn agreement(&self) -> ScreenAgreement {
        let e = self.expected_points();
        let near = |a: f32, b: f32| (a - b).abs() <= 1.0;
        if near(self.screen_points.x, e.x) && near(self.screen_points.y, e.y) {
            return ScreenAgreement::Consistent;
        }
        let s = self.scale();
        let r = self.ratio();
        // `screen = inner / scale²` means `ratio = 1 / scale`. Checked before the
        // other row because at scale 1 the two coincide, and `Consistent` already won
        // there.
        if (r - 1.0 / s).abs() < 0.02 {
            return ScreenAgreement::ScaledTwice;
        }
        if (r - s).abs() < 0.02 {
            return ScreenAgreement::PixelsAsPoints;
        }
        ScreenAgreement::Odd
    }

    /// The point size the layout must use **instead**, or `None` when the platform
    /// already has it right.
    ///
    /// This is the round's task-2 correction, narrowed to what it can honestly do:
    /// it rewrites the screen rect only when the ratio says the platform's own
    /// numbers are off by a whole factor of the scale, and never rewrites a rect the
    /// contract already agrees with. That matters — the two states this round
    /// measured were both `Consistent`, so an unconditional override would have
    /// "corrected" a correct layout, and its arithmetic would then have been blamed
    /// for whatever the *display* was really doing.
    ///
    /// The arithmetic it refuses is the one the brief forbade outright: physical
    /// pixels used as points. `expected_points` divides by the scale, so that can
    /// never come back out.
    pub fn correction(&self) -> Option<Vec2> {
        // With a probe, the OS is the authority **whether or not it agrees with winit**:
        // the layout must occupy the client area the OS describes, and round 10 is the
        // case where winit's own numbers are the ones that are wrong. Requiring the two
        // to agree here would have skipped the correction exactly when it was needed.
        if let Some(want) = self.os_expected_points() {
            let off = (self.screen_points.x - want.x).abs() > 1.0
                || (self.screen_points.y - want.y).abs() > 1.0;
            return off.then_some(want);
        }
        // No probe: no second opinion, so fall back to winit's own arithmetic — the same
        // rule round 9 applied, with the same limits.
        match self.agreement() {
            ScreenAgreement::Consistent => None,
            // A ratio that is no whole power of the scale is not something this can
            // repair without knowing which side is wrong; report it instead.
            ScreenAgreement::Odd => None,
            ScreenAgreement::PixelsAsPoints | ScreenAgreement::ScaledTwice => {
                Some(self.expected_points())
            }
        }
    }

    /// The point size the layout **should** have used: the OS's, else winit's.
    ///
    /// This is what a pass sets `screen_rect` to when the measured one is wrong, and it
    /// is deliberately one function rather than two call sites choosing — so the
    /// authority is a property of the data, not of the caller's memory of which round it
    /// is in.
    pub fn authoritative_points(&self) -> Vec2 {
        self.os_expected_points()
            .unwrap_or_else(|| self.expected_points())
    }

    /// Whether the presenter's back buffer is the size of the window it presents.
    ///
    /// False means the swapchain is stale: the frame is stretched to the window
    /// instead of covering it texel for texel.
    pub fn back_buffer_matches(&self) -> bool {
        self.presented_physical.0 == self.inner_physical.0
            && self.presented_physical.1 == self.inner_physical.1
    }

    /// How far the stale back buffer is magnified to fill the window, per axis.
    ///
    /// `(1, 1)` is no magnification. Anything else is the compositor stretching the
    /// buffer (`DXGI_SCALING_STRETCH`), and the two axes being **different** is the
    /// signature a screenshot shows and no printed point-space number does: the UI
    /// is not merely too large, it is the wrong shape.
    pub fn stretch(&self) -> (f32, f32) {
        let f = |buf: u32, win: u32| {
            if buf == 0 {
                1.0
            } else {
                win as f32 / buf as f32
            }
        };
        (
            f(self.presented_physical.0, self.inner_physical.0),
            f(self.presented_physical.1, self.inner_physical.1),
        )
    }

    /// The overlay's lines for this: what the platform said, what the contract
    /// expects, and the ratio that names the row.
    ///
    /// The first row's verdict is scoped to what it can actually check. It compares
    /// `screen_rect` with `inner / scale` — arithmetic that **winit itself performed**,
    /// so it reads `ok` even when winit's scale is stale, which is the one failure a
    /// stale scale produces. A reader who stops at the first `ok` would therefore stop
    /// before the row that matters; so when the OS disagrees, that is said on this row
    /// too, and the `NO` highlights it the way every other failure is highlighted.
    pub fn lines(&self) -> Vec<String> {
        let e = self.expected_points();
        let label = match (self.agreement(), self.os, self.os_agrees()) {
            (ScreenAgreement::Consistent, Some(_), Some(false)) => {
                "NO winit agrees with itself; its scale is stale (see the os row)"
            }
            (ScreenAgreement::Consistent, _, _) => "ok",
            (ScreenAgreement::PixelsAsPoints, _, _) => "NO pixels as points",
            (ScreenAgreement::ScaledTwice, _, _) => "NO scaled twice",
            (ScreenAgreement::Odd, _, _) => "NO not a whole scale",
        };
        let mut out = vec![
            format!(
                "screen {:.1}x{:.1} pt · inner {}x{} px · scale {:.2} · expect {:.1}x{:.1} pt",
                self.screen_points.x,
                self.screen_points.y,
                self.inner_physical.0,
                self.inner_physical.1,
                self.scale(),
                e.x,
                e.y
            ),
            format!(
                "winit scale count {:.3}x (1.000 = ok) · {label}",
                self.ratio()
            ),
        ];
        // The **independent** opinion. Without a probe there is nothing to say, and
        // saying nothing is the point: a build that cannot second-guess winit must not
        // print a line that reads as if it had.
        match (self.os, self.os_agrees(), self.os_point_ratio()) {
            (Some(os), Some(agree), Some((rx, ry))) => {
                let want = self.authoritative_points();
                out.push(format!(
                    "os client {}x{} px · os scale {:.2} · window {}x{} px · {}",
                    os.client_px.0,
                    os.client_px.1,
                    os.scale,
                    os.window_px.0,
                    os.window_px.1,
                    if agree {
                        "agrees with winit"
                    } else {
                        "NO winit disagrees"
                    }
                ));
                out.push(format!(
                    "os expect {:.1}x{:.1} pt · layout is {:.3}x{:.3} of it · {}",
                    want.x,
                    want.y,
                    rx,
                    ry,
                    if (rx - 1.0).abs() <= 0.01 && (ry - 1.0).abs() <= 0.01 {
                        "point space fits the os window"
                    } else {
                        "NO point space overflows the os window"
                    }
                ));
                out.push(format!("dpi awareness {}", os.awareness));
                if let Some((mw, mh)) = os.monitor_px {
                    let (cw, ch) = os.client_px;
                    out.push(format!(
                        "monitor {mw}x{mh} px · client {cw}x{ch} px · {}",
                        if cw > mw || ch > mh {
                            "NO the window is bigger than its screen"
                        } else {
                            "fits the screen"
                        }
                    ));
                }
            }
            _ => out.push("os probe n/a (no second opinion on this platform)".to_owned()),
        }
        out.extend(vec![
            match self.correction() {
                // The layout on this screen used the corrected size, so a `NO` above is
                // the *platform's* fault and is already repaired: saying so keeps a
                // screenshot from reading as a live defect.
                Some(want) => format!(
                    "layout used {:.1}x{:.1} pt: the screen rect was corrected this pass",
                    want.x, want.y
                ),
                None => "layout used the platform's own screen rect (no correction)".to_owned(),
            },
            format!(
                "back buffer {}x{} px · window inner {}x{} px · matches {}",
                self.presented_physical.0,
                self.presented_physical.1,
                self.inner_physical.0,
                self.inner_physical.1,
                if self.back_buffer_matches() {
                    "yes"
                } else {
                    "NO"
                }
            ),
            format!(
                "present stretch {:.3}x{:.3} (1.000x1.000 = ok) · {}",
                self.stretch().0,
                self.stretch().1,
                if self.back_buffer_matches() {
                    "blit"
                } else if (self.stretch().0 - self.stretch().1).abs() > 1e-3 {
                    "NO anisotropic — the UI is the wrong shape"
                } else {
                    "NO magnified"
                }
            ),
        ]);
        out
    }
}

/// Whether the panels and the viewport **partition** the window, printed as the
/// numbers that add up.
///
/// [`PanelBudget`] states the same property for a test; this type states it for a
/// screenshot, which is the only instrument that sees the running window. Two
/// independent anomalies are visible here and nowhere else:
///
/// - a viewport rect wider or taller than what the panels leave (the smoke17
///   defect: `910 × 713 pt` inside an `823 × 457 pt` window);
/// - a bar that laid out at **zero height** — the numbers then still add up
///   (`top + 0 + rect == window`), so the identity alone cannot catch it, and the
///   probe that produced this type reported exactly that: `25 + 702 = 727` with the
///   controls bar contributing nothing. `bottom` is therefore printed on its own.
///
/// **`window` must be the OS's client area in points, not `inner / scale`.** Round 10
/// printed `partition OK` for a layout whose viewport reached past the screen: the
/// window it was checked against was derived from the very numbers being suspected, so
/// the identity held between two conspirators. The caller passes
/// [`ScreenSpace::authoritative_points`], which prefers the OS's `GetClientRect` divided
/// by the OS's DPI and falls back to winit's only when no probe exists.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Partition {
    /// The window's client area in points, `inner / scale` — not the rect's size.
    pub window: Vec2,
    /// The left panel's width, in points.
    pub left: f32,
    /// The status bar's height, in points.
    pub top: f32,
    /// The controls bar's height, in points.
    pub bottom: f32,
    /// The rect the viewport was allocated, in points.
    pub rect: Rect,
}

impl Partition {
    /// `left + viewport` — must equal the window's width.
    pub fn width_total(&self) -> f32 {
        self.left + self.rect.width()
    }

    /// `top + bottom + viewport` — must equal the window's height.
    pub fn height_total(&self) -> f32 {
        self.top + self.bottom + self.rect.height()
    }

    /// The width identity holds to within a point (the layout's own rounding).
    pub fn fits_width(&self) -> bool {
        (self.width_total() - self.window.x).abs() <= 1.0
    }

    /// The height identity holds to within a point.
    pub fn fits_height(&self) -> bool {
        (self.height_total() - self.window.y).abs() <= 1.0
    }

    /// Whether every panel that is always drawn got a size at all.
    ///
    /// The status bar, the composition-tree strip and the controls bar are drawn on
    /// every pass, so a zero is not a legitimate layout: it is a panel the pass did
    /// not lay out, and the identity would still hold without it.
    pub fn bars_present(&self) -> bool {
        self.left > 0.0 && self.top > 0.0 && self.bottom > 0.0
    }

    /// The whole property: both identities hold **and** the bars are there.
    pub fn ok(&self) -> bool {
        self.fits_width() && self.fits_height() && self.bars_present()
    }

    /// The overlay's lines: one per identity, with every term printed.
    pub fn lines(&self) -> Vec<String> {
        let verdict = if self.ok() {
            "partition OK"
        } else {
            "partition NO"
        };
        vec![
            verdict.to_owned(),
            format!(
                "width {:.1} + {:.1} = {:.1} vs window {:.1} (Δ {:.1}) · fits {}",
                self.left,
                self.rect.width(),
                self.width_total(),
                self.window.x,
                self.width_total() - self.window.x,
                if self.fits_width() { "yes" } else { "NO" }
            ),
            format!(
                "height {:.1} + {:.1} + {:.1} = {:.1} vs window {:.1} (Δ {:.1}) · fits {} · bars {}",
                self.top,
                self.bottom,
                self.rect.height(),
                self.height_total(),
                self.window.y,
                self.height_total() - self.window.y,
                if self.fits_height() { "yes" } else { "NO" },
                if self.bars_present() { "yes" } else { "NO" }
            ),
        ]
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
///
/// `drawn_*` is the decisive pair the earlier version was missing: the orbit the UI
/// holds is the one it **sent**, and the picture on screen is whatever the render
/// thread drew *last*. Two numbers that can disagree — a window whose overlay read
/// `dist 1287 / target 116.9` (the fit) while the image was a pre-fit corner is
/// exactly that disagreement, and it cannot be seen by reporting one of them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayReport {
    pub pixels_per_point: f32,
    pub rect_points: (f32, f32),
    pub viewport_texels: (u32, u32),
    pub frame_texels: (u32, u32),
    /// Whether the frame on screen has the viewport's texel size (a 1:1 blit).
    pub texels_match: bool,
    /// Frames the render thread has **produced**, and how many have been shown.
    pub frames_produced: u64,
    pub frames_shown: u64,
    /// The camera the viewport is drawn with.
    pub orbit_distance: f64,
    pub orbit_target: [f64; 3],
    /// The camera the frame **on screen** was drawn with; `None` before a frame.
    pub drawn_distance: Option<f64>,
    pub drawn_target: Option<[f64; 3]>,
}

impl OverlayReport {
    /// Whether the frame on screen is the latest the thread produced. Counted over
    /// *productions*, not takes: a frame that reached the thread and not the screen
    /// is the state a stale picture is stuck in.
    pub fn fresh(&self) -> bool {
        self.frames_shown >= self.frames_produced
    }

    /// Whether the camera the picture was drawn with is the one the UI is showing
    /// in the overlay — within a centimetre of target and a millimetre of distance,
    /// which is far tighter than any real camera move.
    pub fn camera_matches(&self) -> bool {
        let (Some(d), Some(t)) = (self.drawn_distance, self.drawn_target) else {
            return false;
        };
        (d - self.orbit_distance).abs() < 1e-3
            && (0..3).all(|k| (t[k] - self.orbit_target[k]).abs() < 1e-2)
    }

    /// The overlay's lines, one per fact. Kept as text here so the same strings a
    /// screenshot shows are what a test asserts.
    pub fn lines(&self) -> Vec<String> {
        let drawn = match (self.drawn_distance, self.drawn_target) {
            (Some(d), Some(t)) => format!(
                "drawn {d:.1} target ({:.1}, {:.1}, {:.1})",
                t[0], t[1], t[2]
            ),
            _ => "drawn (no frame)".to_owned(),
        };
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
                "frames {} produced / {} shown · fresh {}",
                self.frames_produced,
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
            format!(
                "{drawn} · same camera {}",
                if self.camera_matches() { "yes" } else { "NO" }
            ),
        ]
    }
}
