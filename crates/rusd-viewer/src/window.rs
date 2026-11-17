//! Where the window opens: a fixed 1280×720 client area, centred on the primary
//! monitor.
//!
//! winit's default is "the platform picks", which on Windows means a cascade
//! position that can be anywhere. A viewer that opens off-centre (or behind the
//! taskbar, or half off a second screen) reads as broken before its first frame,
//! so the opening rectangle is computed here — in **physical** pixels, because
//! winit's monitor geometry is physical and mixing the two under a 125% scale
//! factor is how a "centred" window ends up 200 px low.
//!
//! It is one function, pure, so the arithmetic is testable without a window: a
//! monitor at (0, 0) sized 1920×1080 gives (320, 180); a monitor with a non-zero
//! origin gives that origin plus the offset; a window larger than the monitor is
//! clamped to the monitor, never placed at a negative coordinate.

use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowAttributes};

/// The client area the viewer opens at, in logical points (1280×720 at 100%).
pub const OPEN_SIZE: (f64, f64) = (1280.0, 720.0);

/// The position that centres a `size` window on a monitor at `origin` with
/// `monitor_size`.
///
/// A window bigger than the monitor is clamped to the monitor's own size, so the
/// result is the monitor's origin rather than a negative coordinate (a title bar
/// above the top of the screen is not recoverable by the user).
pub fn centred_position(
    origin: (i32, i32),
    monitor_size: (u32, u32),
    size: (u32, u32),
) -> (i32, i32) {
    let w = size.0.min(monitor_size.0);
    let h = size.1.min(monitor_size.1);
    let dx = (monitor_size.0.saturating_sub(w) / 2) as i32;
    let dy = (monitor_size.1.saturating_sub(h) / 2) as i32;
    (origin.0 + dx, origin.1 + dy)
}

/// The monitor's **work area** (monitor minus taskbar/appbars), physical px.
/// Opening against the full monitor let the controls bar land behind the
/// taskbar; the work area is the rectangle a window may actually occupy.
#[cfg(windows)]
fn work_area_px(
    origin: (i32, i32),
    size: PhysicalSize<u32>,
) -> Option<((i32, i32), PhysicalSize<u32>)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    let pt = POINT {
        x: origin.0 + (size.width / 2) as i32,
        y: origin.1 + (size.height / 2) as i32,
    };
    let hmon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    if hmon.is_null() {
        return None;
    }
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(hmon, &mut info) } == 0 {
        return None;
    }
    let r = info.rcWork;
    Some((
        (r.left, r.top),
        PhysicalSize::new((r.right - r.left) as u32, (r.bottom - r.top) as u32),
    ))
}

#[cfg(not(windows))]
fn work_area_px(
    _origin: (i32, i32),
    _size: PhysicalSize<u32>,
) -> Option<((i32, i32), PhysicalSize<u32>)> {
    None
}

/// The opening rectangle for a window of `size` (physical) on `monitor`.
pub fn place_on(
    origin: (i32, i32),
    monitor_size: PhysicalSize<u32>,
    size: PhysicalSize<u32>,
) -> (PhysicalSize<u32>, PhysicalPosition<i32>) {
    let w = size.width.min(monitor_size.width).max(1);
    let h = size.height.min(monitor_size.height).max(1);
    let (x, y) = centred_position(origin, (monitor_size.width, monitor_size.height), (w, h));
    (PhysicalSize::new(w, h), PhysicalPosition::new(x, y))
}

/// The attributes `rusd-viewer` opens with, centred on the primary monitor.
///
/// Falls back to an unpositioned window when the platform reports no primary
/// monitor (a headless session, or a very early startup): a window somewhere is
/// better than refusing to open one.
pub fn startup_window_attrs(event_loop: &ActiveEventLoop) -> Result<WindowAttributes, String> {
    let attrs = Window::default_attributes().with_title("rusd-viewer");
    let Some(monitor) = event_loop.primary_monitor() else {
        return Ok(attrs.with_inner_size(LogicalSize::new(OPEN_SIZE.0, OPEN_SIZE.1)));
    };
    // The requested client area is a logical size; the position must be physical.
    // The scale factor of the monitor the window lands on is that monitor's own,
    // so converting with it keeps the two consistent.
    let scale = monitor.scale_factor().max(0.1);
    let want = LogicalSize::new(OPEN_SIZE.0, OPEN_SIZE.1).to_physical::<u32>(scale);
    let (origin, area) = work_area_px(
        (monitor.position().x, monitor.position().y),
        monitor.size(),
    )
    .unwrap_or(((monitor.position().x, monitor.position().y), monitor.size()));
    let (size, position) = place_on(origin, area, want);
    Ok(attrs.with_inner_size(size).with_position(position))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_1080p_monitor_centres_a_1280x720_window() {
        let (size, at) = place_on(
            (0, 0),
            PhysicalSize::new(1920, 1080),
            PhysicalSize::new(1280, 720),
        );
        assert_eq!(size, PhysicalSize::new(1280, 720), "the size is unchanged");
        assert_eq!(at, PhysicalPosition::new(320, 180), "centred on 1920x1080");
    }

    #[test]
    fn a_monitor_with_an_origin_carries_it_into_the_position() {
        // A second screen to the right of the primary one.
        let (_, at) = place_on(
            (1920, 0),
            PhysicalSize::new(2560, 1440),
            PhysicalSize::new(1280, 720),
        );
        assert_eq!(at, PhysicalPosition::new(1920 + 640, 360));
    }

    #[test]
    fn a_window_larger_than_the_monitor_is_clamped_to_it() {
        let (size, at) = place_on(
            (100, 50),
            PhysicalSize::new(1024, 600),
            PhysicalSize::new(1280, 720),
        );
        assert_eq!(
            size,
            PhysicalSize::new(1024, 600),
            "no bigger than the screen"
        );
        assert_eq!(
            at,
            PhysicalPosition::new(100, 50),
            "pinned at the monitor's corner"
        );
    }

    #[test]
    fn an_odd_spare_pixel_goes_to_the_right_and_bottom() {
        // 1921 - 1280 = 641, whose half is 320 (floor): one pixel spare, not lost.
        let (_, at) = place_on(
            (0, 0),
            PhysicalSize::new(1921, 1081),
            PhysicalSize::new(1280, 720),
        );
        assert_eq!(at, PhysicalPosition::new(320, 180));
    }

    #[test]
    fn centring_is_never_negative_for_a_monitor_at_the_origin() {
        let at = centred_position((0, 0), (800, 600), (1280, 720));
        assert_eq!(at, (0, 0), "clamped, not negative");
    }
}
