//! Visual selection feedback (SPEC §7 "可反馈"): make the agent's point-and-select
//! loop verifiable. Two CPU post-processes over an existing [`RenderOutput`]
//! (beauty + prim-id readback) — no GPU changes, deterministic:
//!
//! - [`draw_markers`]: reticles at the pixels the agent reported, so it can see
//!   where it pointed.
//! - [`apply_selection`]: highlight selected prims by id — recolor (preserving
//!   shading) and/or a screen-space silhouette outline. Multi-select, one color
//!   per prim.
//!
//! Selection is addressed by stable prim path (mapped to the render's id table),
//! never by pixel, so it survives re-renders.

use crate::render::RenderOutput;

/// A point the agent reported, drawn as a reticle.
#[derive(Clone, Copy, Debug)]
pub struct Marker {
    pub x: u32,
    pub y: u32,
    pub color: [u8; 3],
}

/// How a selected prim is highlighted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectMode {
    Outline,
    Recolor,
    Both,
}

/// One selected prim: path + highlight color + mode.
#[derive(Clone, Debug)]
pub struct Selection {
    pub path: String,
    pub color: [u8; 3],
    pub mode: SelectMode,
}

/// Well-separated colors for auto-assigning multi-select / multi-point.
const PALETTE: [[u8; 3]; 10] = [
    [255, 60, 60],
    [60, 200, 80],
    [70, 130, 255],
    [255, 210, 60],
    [255, 80, 220],
    [60, 220, 230],
    [255, 140, 40],
    [180, 120, 255],
    [160, 220, 90],
    [255, 255, 255],
];

pub fn palette(i: usize) -> [u8; 3] {
    PALETTE[i % PALETTE.len()]
}

/// Parse a color: a name (`red`, `green`, …) or `#RRGGBB` / `RRGGBB`.
pub fn parse_color(s: &str) -> Option<[u8; 3]> {
    let named = match s.to_ascii_lowercase().as_str() {
        "red" => [255, 60, 60],
        "green" => [60, 200, 80],
        "blue" => [70, 130, 255],
        "yellow" => [255, 210, 60],
        "magenta" => [255, 80, 220],
        "cyan" => [60, 220, 230],
        "orange" => [255, 140, 40],
        "purple" => [180, 120, 255],
        "lime" => [160, 220, 90],
        "white" => [255, 255, 255],
        "black" => [0, 0, 0],
        _ => {
            let hex = s.strip_prefix('#').unwrap_or(s);
            if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some([r, g, b]);
        }
    };
    Some(named)
}

/// Highlight selected prims in the beauty buffer (ids/paths unchanged, so picking
/// still works). Unknown paths are skipped.
pub fn apply_selection(r: &mut RenderOutput, selections: &[Selection]) {
    for sel in selections {
        let Some(idx) = r.paths.iter().position(|p| p == &sel.path) else {
            continue;
        };
        let id = (idx + 1) as u32;
        if matches!(sel.mode, SelectMode::Recolor | SelectMode::Both) {
            recolor_id(r, id, sel.color);
        }
        if matches!(sel.mode, SelectMode::Outline | SelectMode::Both) {
            outline_id(r, id, sel.color);
        }
    }
}

fn recolor_id(r: &mut RenderOutput, id: u32, color: [u8; 3]) {
    for (i, &pid) in r.ids.iter().enumerate() {
        if pid != id {
            continue;
        }
        let px = &mut r.beauty[i * 4..i * 4 + 4];
        let luma = 0.299 * px[0] as f32 + 0.587 * px[1] as f32 + 0.114 * px[2] as f32;
        let k = 0.3 + 0.7 * (luma / 255.0);
        px[0] = (color[0] as f32 * k) as u8;
        px[1] = (color[1] as f32 * k) as u8;
        px[2] = (color[2] as f32 * k) as u8;
        px[3] = 255;
    }
}

fn outline_id(r: &mut RenderOutput, id: u32, color: [u8; 3]) {
    let w = r.width as i32;
    let h = r.height as i32;
    // A pixel is on the silhouette if its 3x3 neighborhood mixes this id with
    // something else (background or another prim). Collect first, then paint, so
    // the boundary test reads the original ids, not freshly painted colors.
    let mut edge: Vec<usize> = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let mut has_id = false;
            let mut has_other = false;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let nx = x + dx;
                    let ny = y + dy;
                    if nx < 0 || ny < 0 || nx >= w || ny >= h {
                        has_other = true; // image border counts as outside
                        continue;
                    }
                    let nid = r.ids[(ny * w + nx) as usize];
                    if nid == id {
                        has_id = true;
                    } else {
                        has_other = true;
                    }
                }
            }
            if has_id && has_other {
                edge.push((y * w + x) as usize);
            }
        }
    }
    for i in edge {
        let px = &mut r.beauty[i * 4..i * 4 + 4];
        px[0] = color[0];
        px[1] = color[1];
        px[2] = color[2];
        px[3] = 255;
    }
}

/// Draw reticles (ring + four arms) at the reported pixels, on top of everything.
pub fn draw_markers(r: &mut RenderOutput, markers: &[Marker]) {
    let w = r.width as i32;
    let h = r.height as i32;
    for m in markers {
        let cx = m.x as i32;
        let cy = m.y as i32;
        for dy in -13i32..=13 {
            for dx in -13i32..=13 {
                let d2 = dx * dx + dy * dy;
                let ring = (25..=49).contains(&d2); // radius ~5..7
                let arm = (dx.abs() <= 1 && (64..=144).contains(&d2) && dy.abs() >= 8)
                    || (dy.abs() <= 1 && (64..=144).contains(&d2) && dx.abs() >= 8);
                if !(ring || arm) {
                    continue;
                }
                let x = cx + dx;
                let y = cy + dy;
                if x < 0 || y < 0 || x >= w || y >= h {
                    continue;
                }
                let px = &mut r.beauty[(y * w + x) as usize * 4..][..4];
                px[0] = m.color[0];
                px[1] = m.color[1];
                px[2] = m.color[2];
                px[3] = 255;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny synthetic render: 8x8, prim id 1 fills a 4x4 block at (2,2)-(5,5),
    /// rest background (id 0). Beauty is mid-gray everywhere.
    fn fake() -> RenderOutput {
        let (w, h) = (8u32, 8u32);
        let mut ids = vec![0u32; (w * h) as usize];
        for y in 2..6 {
            for x in 2..6 {
                ids[(y * w + x) as usize] = 1;
            }
        }
        let beauty = vec![128u8; (w * h * 4) as usize];
        RenderOutput {
            width: w,
            height: h,
            beauty,
            ids,
            paths: vec!["/World/Box".to_owned()],
            diags: vec![],
        }
    }

    #[test]
    fn parse_color_named_and_hex() {
        assert_eq!(parse_color("red"), Some([255, 60, 60]));
        assert_eq!(parse_color("#00ff00"), Some([0, 255, 0]));
        assert_eq!(parse_color("0000ff"), Some([0, 0, 255]));
        assert_eq!(parse_color("nope"), None);
    }

    #[test]
    fn recolor_changes_only_selected_pixels() {
        let mut r = fake();
        apply_selection(
            &mut r,
            &[Selection {
                path: "/World/Box".into(),
                color: [255, 0, 0],
                mode: SelectMode::Recolor,
            }],
        );
        // inside the block: recolored red-ish, green/blue zero
        let inside = (3 * 8 + 3) * 4;
        assert!(r.beauty[inside] > 100);
        assert_eq!(r.beauty[inside + 1], 0);
        assert_eq!(r.beauty[inside + 2], 0);
        // background untouched
        let outside = 0;
        assert_eq!(&r.beauty[outside..outside + 3], &[128, 128, 128]);
        // ids unchanged
        assert_eq!(r.ids[3 * 8 + 3], 1);
    }

    #[test]
    fn outline_paints_boundary_not_interior() {
        let mut r = fake();
        apply_selection(
            &mut r,
            &[Selection {
                path: "/World/Box".into(),
                color: [0, 255, 0],
                mode: SelectMode::Outline,
            }],
        );
        // deep interior pixel (3,3) has all neighbors id 1 -> not painted
        let interior = (3 * 8 + 3) * 4;
        assert_eq!(&r.beauty[interior..interior + 3], &[128, 128, 128]);
        // a boundary pixel (2,2) neighbors background -> painted green
        let boundary = (2 * 8 + 2) * 4;
        assert_eq!(&r.beauty[boundary..boundary + 3], &[0, 255, 0]);
    }

    #[test]
    fn unknown_path_is_skipped() {
        let mut r = fake();
        let before = r.beauty.clone();
        apply_selection(
            &mut r,
            &[Selection {
                path: "/World/Nope".into(),
                color: [255, 0, 0],
                mode: SelectMode::Both,
            }],
        );
        assert_eq!(r.beauty, before);
    }

    #[test]
    fn marker_paints_ring_not_center() {
        let (w, h) = (32u32, 32u32);
        let mut r = RenderOutput {
            width: w,
            height: h,
            beauty: vec![128u8; (w * h * 4) as usize],
            ids: vec![0u32; (w * h) as usize],
            paths: vec![],
            diags: vec![],
        };
        draw_markers(
            &mut r,
            &[Marker {
                x: 16,
                y: 16,
                color: [255, 255, 0],
            }],
        );
        // ring pixel at (16+6,16): d2=36 in 25..=49 -> yellow
        let ring = (16 * w + 22) as usize * 4;
        assert_eq!(&r.beauty[ring..ring + 3], &[255, 255, 0]);
        // hollow center stays gray
        let center = (16 * w + 16) as usize * 4;
        assert_eq!(&r.beauty[center..center + 3], &[128, 128, 128]);
    }
}
