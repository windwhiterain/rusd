//! What the **operating system** says about the window's size and scale, read
//! through win32 directly so that it does not share an answer with winit.
//!
//! Round 9 made the shell print three numbers — `inner_size()` in physical pixels,
//! `scale_factor()`, and the point size egui laid out in — and called `1.000` on their
//! ratio "the contract met". Round 10's window then showed `partition OK`, `scale count
//! 1.000x · ok` and `present stretch 1.000x1.000 · blit` while the city was off the
//! right-hand edge. Those three lines agreed because **they are all winit's own
//! answers**: `inner_size()` is `GetClientRect` (winit-0.30.13
//! `platform_impl/windows/window.rs:214`), the point size is that value divided by
//! `scale_factor()`, and `Partition::window` was built from the same division. An
//! instrument made only of one library's outputs cannot see that library being wrong —
//! it reports self-consistency and calls it health.
//!
//! This module is the second opinion. `GetClientRect` and `GetDpiForWindow` are called
//! here, on the window's own `HWND`, and the numbers are compared against what winit
//! said. The two agreeing is evidence; one number alone is not.
//!
//! Everything here is `unsafe` win32 as `windows-sys` declares it, and every call has
//! a defined answer when the OS refuses: a failed `GetClientRect` or a `GetDpiForWindow`
//! of `0` leaves the field `None`, which the overlay prints as `n/a` rather than as a
//! passing number. A probe that guesses is worse than no probe.

/// The OS's own view of a window: the client area, the window rectangle, and the DPI
/// the window is on, plus the process/thread DPI-awareness the answers depend on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlatformMetrics {
    /// `GetClientRect(hwnd)` — the client area in **physical** pixels, or `None` if the
    /// call failed. This is the same call winit makes for `inner_size()`; printing both
    /// is what proves they came from one place rather than from two agreeing sources.
    pub client_px: Option<(u32, u32)>,
    /// `GetWindowRect(hwnd)` — the whole window including the frame, in physical
    /// pixels. Its difference from `client_px` is the border and title bar, which is
    /// how a caller can tell a client-area report from an outer-rectangle one.
    pub window_px: Option<(u32, u32)>,
    /// `GetDpiForWindow(hwnd)` — the window's DPI (96 = 100%). `None` when the OS
    /// returned 0, which it does for an invalid handle.
    pub dpi: Option<u32>,
    /// `GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext())`, as a word:
    /// `unaware` / `system` / `per-monitor`. `None` when the context could not be read.
    ///
    /// This is the field that settles "is the process DPI-unaware, so the OS is
    /// silently scaling every coordinate it hands us?". A per-monitor-aware process is
    /// not virtualized: `GetClientRect` then returns true device pixels.
    pub awareness: Option<&'static str>,
    /// `GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST))`: the monitor's
    /// **physical** rectangle, as `(x, y, width, height)`.
    ///
    /// This is the datum that decides "is the window bigger than the screen?" — a
    /// different defect from "is the layout bigger than the window?", and the only one
    /// left when the partition identity holds. A client area of `2496` physical pixels
    /// cannot fit a 1920-wide monitor: then the window really does hang off the edge,
    /// and no point-space arithmetic is at fault.
    pub monitor_px: Option<(i32, i32, u32, u32)>,
}

impl PlatformMetrics {
    /// The scale the OS says this window is at: `dpi / 96`.
    ///
    /// `None` when the DPI was not readable, and never a guess of `1.0` — a fabricated
    /// scale factor is exactly the kind of number this module exists to avoid producing.
    pub fn os_scale(&self) -> Option<f32> {
        self.dpi.map(|dpi| dpi as f32 / 96.0)
    }

    /// Whether winit's `inner_size()` agrees with the OS's `GetClientRect`.
    ///
    /// This is expected to be `true` (winit calls the same function). It is printed
    /// anyway, because "expected to be true" is a claim and this is the measurement.
    pub fn client_matches(&self, winit_inner: (u32, u32)) -> Option<bool> {
        self.client_px.map(|c| c == winit_inner)
    }

    /// Whether winit's `scale_factor()` agrees with the OS's `dpi / 96`.
    ///
    /// This one is *not* expected to be trivially true: winit caches
    /// `scale_factor` in its `WindowState` and updates it only from `WM_DPICHANGED`
    /// (event_loop.rs:2216). A window resized without a DPI change keeps a stale scale
    /// — and a stale scale is a point space that is wrong by exactly that factor.
    pub fn scale_matches(&self, winit_scale: f32) -> Option<bool> {
        self.os_scale().map(|s| (s - winit_scale).abs() <= 1e-3)
    }

    /// Whether the window's client area is bigger than the monitor it is on.
    ///
    /// This is the one defect that survives every other check passing: the point space
    /// can agree with the OS, the partition identity can hold, the swapchain can be 1:1 —
    /// and the window still hangs off the edge of the screen because its client area is
    /// wider or taller than the monitor.
    ///
    /// `None` when the monitor could not be read.
    pub fn client_exceeds_monitor(&self) -> Option<bool> {
        let (_, _, mw, mh) = self.monitor_px?;
        let (cw, ch) = self.client_px?;
        Some(cw > mw || ch > mh)
    }

    /// The overlay's lines: the OS's numbers, then the two comparisons.
    pub fn lines(&self, winit_inner: (u32, u32), winit_scale: f32) -> Vec<String> {
        let client = match self.client_px {
            Some((w, h)) => format!("{w}x{h} px"),
            None => "n/a".to_owned(),
        };
        let outer = match self.window_px {
            Some((w, h)) => format!("{w}x{h} px"),
            None => "n/a".to_owned(),
        };
        let dpi = match (self.dpi, self.os_scale()) {
            (Some(d), Some(s)) => format!("dpi {d} · scale {s:.2}"),
            _ => "dpi n/a".to_owned(),
        };
        let aware = self.awareness.unwrap_or("n/a");
        let client_cmp = match self.client_matches(winit_inner) {
            Some(true) => "agrees with winit",
            Some(false) => "NO winit disagrees",
            None => "n/a",
        };
        let scale_cmp = match self.scale_matches(winit_scale) {
            Some(true) => "agrees with winit",
            Some(false) => "NO winit disagrees",
            None => "n/a",
        };
        vec![
            format!("client rect {client} (win32 GetClientRect) · {client_cmp}"),
            format!(
                "window rect {outer} (GetWindowRect) · frame {}",
                match (self.window_px, self.client_px) {
                    (Some(o), Some(c)) =>
                        format!("{}x{} px", o.0.saturating_sub(c.0), o.1.saturating_sub(c.1)),
                    _ => "n/a".to_owned(),
                }
            ),
            format!("os {dpi} · {scale_cmp}"),
            match self.monitor_px {
                Some((_, _, mw, mh)) => format!(
                    "monitor {mw}x{mh} px · client {} · {}",
                    match self.client_px {
                        Some((w, h)) => format!("{w}x{h} px"),
                        None => "n/a".to_owned(),
                    },
                    match self.client_exceeds_monitor() {
                        Some(true) => "NO the window is bigger than its screen",
                        Some(false) => "fits the screen",
                        None => "n/a",
                    }
                ),
                None => "monitor n/a".to_owned(),
            },
            format!("dpi awareness {aware}"),
        ]
    }
}

#[cfg(target_os = "windows")]
mod win {
    use super::PlatformMetrics;
    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    };
    use windows_sys::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        GetAwarenessFromDpiAwarenessContext, GetDpiForWindow, GetThreadDpiAwarenessContext,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect};

    /// The client area from `GetClientRect`, in physical pixels.
    ///
    /// # Safety
    /// `hwnd` must be a live handle of a window owned by this process; callers get it
    /// from the window itself ([`super::probe`]), never from a constant or a guess.
    pub fn probes(hwnd: HWND) -> PlatformMetrics {
        let mut client = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let client_px = unsafe {
            if GetClientRect(hwnd, &mut client) != 0 {
                Some((
                    (client.right - client.left).max(0) as u32,
                    (client.bottom - client.top).max(0) as u32,
                ))
            } else {
                None
            }
        };

        let mut window = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let window_px = unsafe { GetWindowRect(hwnd, &mut window) != 0 }.then(|| {
            (
                (window.right - window.left).max(0) as u32,
                (window.bottom - window.top).max(0) as u32,
            )
        });

        let dpi = unsafe {
            match GetDpiForWindow(hwnd) {
                0 => None,
                d => Some(d),
            }
        };

        let awareness = unsafe {
            let ctx = GetThreadDpiAwarenessContext();
            match GetAwarenessFromDpiAwarenessContext(ctx) {
                // DPI_AWARENESS_INVALID = -1, UNAWARE = 0, SYSTEM_AWARE = 1, PER_MONITOR_AWARE = 2
                -1 => None,
                0 => Some("unaware"),
                1 => Some("system"),
                2 => Some("per-monitor"),
                _ => None,
            }
        };
        // The two named contexts are referenced so a reader can see which ones the
        // answer above distinguishes; `GetAwarenessFromDpiAwarenessContext` folds both
        // per-monitor variants into the same `2`.
        let _ = (
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );

        let monitor_px = unsafe {
            let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            if hmon.is_null() {
                None
            } else {
                let mut info: MONITORINFO = std::mem::zeroed();
                info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
                (GetMonitorInfoW(hmon, &mut info) != 0).then(|| {
                    let r = info.rcMonitor;
                    (
                        r.left,
                        r.top,
                        (r.right - r.left).max(0) as u32,
                        (r.bottom - r.top).max(0) as u32,
                    )
                })
            }
        };
        let monitor_px: Option<(i32, i32, u32, u32)> = monitor_px;

        PlatformMetrics {
            client_px,
            window_px,
            dpi,
            awareness,
            monitor_px,
        }
    }
}

/// Read the OS's metrics for `window`, or `None` when this platform has no such probe.
///
/// On a non-Windows peer this is `None`: the overlay's two lines then say `n/a`, which is
/// the honest answer — there is no second opinion to print, so no agreement is claimed.
pub fn probe(window: &winit::window::Window) -> Option<PlatformMetrics> {
    #[cfg(target_os = "windows")]
    {
        use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
        let handle = window.window_handle().ok()?;
        let RawWindowHandle::Win32(w) = handle.as_raw() else {
            return None;
        };
        Some(win::probes(w.hwnd.get() as _))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = window;
        None
    }
}

/// A measured disagreement between the OS and winit, as one sentence.
///
/// `None` when both comparisons hold (or neither could be made), so a caller has one
/// thing to report rather than a set of booleans to interpret.
pub fn disagreement(
    m: &PlatformMetrics,
    winit_inner: (u32, u32),
    winit_scale: f32,
) -> Option<String> {
    if m.client_matches(winit_inner) == Some(false) {
        let c = m.client_px?;
        let ratio = c.0 as f32 / winit_inner.0.max(1) as f32;
        return Some(format!(
            "winit inner {}x{} vs OS client {}x{} ({ratio:.3}x)",
            winit_inner.0, winit_inner.1, c.0, c.1
        ));
    }
    if m.scale_matches(winit_scale) == Some(false) {
        let os = m.os_scale()?;
        return Some(format!(
            "winit scale {winit_scale:.3} vs OS dpi/96 {os:.3} ({:.3}x)",
            os / winit_scale
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The machine round 10 ran on: 175% Windows scaling.
    fn at_175(client: (u32, u32), dpi: u32) -> PlatformMetrics {
        PlatformMetrics {
            client_px: Some(client),
            window_px: Some((client.0 + 16, client.1 + 39)),
            dpi: Some(dpi),
            awareness: Some("per-monitor"),
            monitor_px: Some((0, 0, 1920, 1080)),
        }
    }

    #[test]
    fn the_os_scale_is_dpi_over_96_and_never_a_guess() {
        let m = at_175((1424, 761), 168);
        assert_eq!(m.os_scale(), Some(1.75));

        let unknown = PlatformMetrics { dpi: None, ..m };
        assert_eq!(unknown.os_scale(), None, "no reading means no number");
        assert!(
            unknown
                .lines((1424, 761), 1.75)
                .join(" | ")
                .contains("dpi n/a"),
            "the dpi line must say n/a rather than a number"
        );
    }

    #[test]
    fn a_real_client_rect_agrees_with_winit_because_it_is_the_same_call() {
        // winit's `inner_size()` is `GetClientRect`, so this comparison is a check on
        // the instrument's plumbing, not on the OS: if it ever fails, the handle passed
        // in is the wrong window, and every other line on the overlay is suspect.
        let m = at_175((1424, 761), 168);
        assert_eq!(m.client_matches((1424, 761)), Some(true));
        assert_eq!(m.client_matches((2496, 1336)), Some(false));
        assert!(
            m.lines((2496, 1336), 1.75)
                .join(" | ")
                .contains("NO winit disagrees")
        );
    }

    #[test]
    fn a_stale_winit_scale_is_what_the_comparison_exists_to_catch() {
        // The round-10 hypothesis, made checkable: winit reports 1.75 (its cached
        // `WindowState` value) while the window is really on 300% (dpi 288). Then the
        // point space is `client / 1.75` instead of `client / 3.0` — the UI is drawn
        // 1.714x too large and its right-hand side leaves the window. This is the one
        // mechanism that produces "screen 1.75x too big" without winit's size being
        // wrong, and it is invisible to every line round 9 printed.
        let m = at_175((1424, 761), 288);
        assert_eq!(m.os_scale(), Some(3.0));
        assert_eq!(m.scale_matches(1.75), Some(false));
        let lines = m.lines((1424, 761), 1.75).join(" | ");
        assert!(lines.contains("NO winit disagrees"), "{lines}");
        assert!(lines.contains("os dpi 288 · scale 3.00"), "{lines}");
    }

    #[test]
    fn the_disagreement_sentence_names_the_ratio_and_the_culprit() {
        // Size: the OS's client rect is what wins.
        let m = at_175((1424, 761), 168);
        let s = disagreement(&m, (2496, 1336), 1.75).expect("a size disagreement");
        assert!(
            s.contains("winit inner 2496x1336 vs OS client 1424x761"),
            "{s}"
        );
        assert!(
            s.contains("0.571x"),
            "2496 / 1424 = 1.753, so the OS is 0.571 of what winit reported: {s}"
        );

        // At dpi 168 = 1.75 the two agree, so there is nothing to report: a
        // disagreement that is not there must not be invented.
        assert_eq!(disagreement(&m, (1424, 761), 1.75), None);

        // Scale: and when the sizes agree, the scale is the only thing left.
        let stale = at_175((1424, 761), 288);
        let s = disagreement(&stale, (1424, 761), 1.75).expect("a scale disagreement");
        assert!(s.contains("winit scale 1.750 vs OS dpi/96 3.000"), "{s}");

        // And agreement says nothing rather than inventing a defect.
        let clean = at_175((1424, 761), 168);
        assert_eq!(disagreement(&clean, (1424, 761), 1.75), None);
    }

    #[test]
    fn an_unreadable_probe_claims_no_agreement() {
        // Every field `None`: the lines must say `n/a` and the comparisons must be
        // `None`, so a screenshot of this build cannot be read as "verified".
        let blind = PlatformMetrics {
            client_px: None,
            window_px: None,
            dpi: None,
            awareness: None,
            monitor_px: None,
        };
        assert_eq!(blind.client_matches((1424, 761)), None);
        assert_eq!(blind.scale_matches(1.75), None);
        assert_eq!(disagreement(&blind, (1424, 761), 1.75), None);
        let lines = blind.lines((1424, 761), 1.75).join(" | ");
        assert!(lines.contains("client rect n/a"), "{lines}");
        assert!(lines.contains("dpi awareness n/a"), "{lines}");
        assert!(
            !lines.contains("agrees"),
            "no reading, no agreement: {lines}"
        );
    }

    #[test]
    fn the_frame_size_is_the_difference_between_the_two_rects() {
        // This is how a reader tells a client-area report from a whole-window one:
        // 1424x761 client inside a 1440x800 outer is a 16x39 px frame.
        let m = at_175((1424, 761), 168);
        let lines = m.lines((1424, 761), 1.75).join(" | ");
        assert!(lines.contains("window rect 1440x800 px"), "{lines}");
        assert!(lines.contains("frame 16x39 px"), "{lines}");
    }

    #[test]
    fn a_window_wider_than_its_monitor_is_the_defect_no_other_line_can_show() {
        // The case the arithmetic points at. A 2496-px client area cannot fit a
        // 1920-px monitor, so the window hangs off the edge — and every other check can
        // pass while it does: the point space can agree with the OS, the partition
        // identity can hold, the swapchain can be 1:1. Only this line says so.
        let m = PlatformMetrics {
            client_px: Some((2496, 1336)),
            window_px: Some((2520, 1400)),
            dpi: Some(168),
            awareness: Some("per-monitor"),
            monitor_px: Some((0, 0, 1920, 1080)),
        };
        assert_eq!(m.client_exceeds_monitor(), Some(true));
        let lines = m.lines((2496, 1336), 1.75).join(" | ");
        assert!(lines.contains("monitor 1920x1080 px"), "{lines}");
        assert!(
            lines.contains("NO the window is bigger than its screen"),
            "{lines}"
        );
        // The point space is "right" here: that is exactly why the check is needed.
        assert_eq!(m.client_matches((2496, 1336)), Some(true));
        assert_eq!(m.scale_matches(1.75), Some(true));

        // A window that fits says so, and a monitor-sized 2560 screen clears the same
        // numbers — 2496 fits 2560, so the same client area is fine on that display.
        let on_2560 = PlatformMetrics {
            monitor_px: Some((0, 0, 2560, 1440)),
            ..m
        };
        assert_eq!(on_2560.client_exceeds_monitor(), Some(false));
        assert!(
            on_2560
                .lines((2496, 1336), 1.75)
                .join(" | ")
                .contains("fits the screen")
        );
    }
}
