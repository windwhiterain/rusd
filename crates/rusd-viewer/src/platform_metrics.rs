//! What the **operating system** says about the window's size and scale, read
//! through win32 directly so that it does not share an answer with winit.
//!
//! Round 9 made the shell print three numbers — `inner_size()` in physical pixels,
//! `scale_factor()`, and the point size egui laid out in — and called `1.000` on their
//! ratio "the contract met". Round 10's window then showed `partition OK`, `winit scale
//! count 1.000x · ok` and `present stretch 1.000x1.000 · blit` while the city was off the
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
