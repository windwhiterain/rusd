//! Visual selection feedback (SPEC §7 "可反馈"): make the agent's point-and-select
//! loop verifiable. Two CPU post-processes over an existing [`RenderOutput`]
//! (beauty + prim-id readback) — no GPU changes, deterministic:
//!
//! - [`draw_markers`]: reticles at the pixels the agent reported, so it can see
//!   where it pointed.
//! - [`apply_selection`]: highlight selected prims by id — recolor (preserving
//!   shading) and/or a silhouette outline. Multi-select, one color per prim.
//!
//! Selection is addressed by stable prim path (mapped to the render's id table),
//! never by pixel, so it survives re-renders.
//!
//! All visual knobs live in [`Style`] (outline width, marker radius, recolor
//! shading) so nothing is hardcoded. The outline expands **only inward into its
//! own id's pixels**, so two selected prims' outlines never overwrite each other.

use crate::render::RenderOutput;

/// Tunable annotation parameters (all configurable; sensible defaults).
#[derive(Clone, Copy, Debug)]
pub struct Style {
    /// Outline thickness in pixels, expanding inward into the prim's own region.
    /// 0 disables the outline.
    pub outline_width: u32,
    /// Reticle ring radius in pixels (arms extend to ~2× radius).
    pub marker_radius: u32,
    /// How much of the original shading a recolor keeps: 0 = flat selection color,
    /// 1 = full original luminance. Default 0.7.
    pub recolor_shading: f32,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            outline_width: 2,
            marker_radius: 6,
            recolor_shading: 0.7,
        }
    }
}

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
pub fn apply_selection(r: &mut RenderOutput, selections: &[Selection], style: &Style) {
    for sel in selections {
        let Some(idx) = r.paths.iter().position(|p| p == &sel.path) else {
            continue;
        };
        let id = (idx + 1) as u32;
        if matches!(sel.mode, SelectMode::Recolor | SelectMode::Both) {
            recolor_id(r, id, sel.color, style.recolor_shading);
        }
        if matches!(sel.mode, SelectMode::Outline | SelectMode::Both) {
            outline_id(r, id, sel.color, style.outline_width);
        }
    }
}

fn recolor_id(r: &mut RenderOutput, id: u32, color: [u8; 3], shading: f32) {
    let keep = shading.clamp(0.0, 1.0);
    for (i, &pid) in r.ids.iter().enumerate() {
        if pid != id {
            continue;
        }
        let px = &mut r.beauty[i * 4..i * 4 + 4];
        let luma = 0.299 * px[0] as f32 + 0.587 * px[1] as f32 + 0.114 * px[2] as f32;
        let k = (1.0 - keep) + keep * (luma / 255.0);
        px[0] = (color[0] as f32 * k) as u8;
        px[1] = (color[1] as f32 * k) as u8;
        px[2] = (color[2] as f32 * k) as u8;
        px[3] = 255;
    }
}

/// Inward-only silhouette outline: paint only pixels that belong to `id` and lie
/// within `width` px of a pixel that does not (or the image border). Because it
/// never touches another id's pixels, overlapping selections cannot cover each
/// other's outlines.
fn outline_id(r: &mut RenderOutput, id: u32, color: [u8; 3], width: u32) {
    if width == 0 {
        return;
    }
    let w = r.width as i32;
    let h = r.height as i32;
    let t = width as i32;
    // Collect first, then paint, so the boundary test reads original ids.
    let mut edge: Vec<usize> = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let p = (y * w + x) as usize;
            if r.ids[p] != id {
                continue; // only expand into our own region
            }
            let mut boundary = false;
            'outer: for dy in -t..=t {
                for dx in -t..=t {
                    let nx = x + dx;
                    let ny = y + dy;
                    if nx < 0 || ny < 0 || nx >= w || ny >= h {
                        boundary = true;
                        break 'outer;
                    }
                    if r.ids[(ny * w + nx) as usize] != id {
                        boundary = true;
                        break 'outer;
                    }
                }
            }
            if boundary {
                edge.push(p);
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
pub fn draw_markers(r: &mut RenderOutput, markers: &[Marker], style: &Style) {
    let w = r.width as i32;
    let h = r.height as i32;
    let rad = style.marker_radius.max(1) as i32;
    let ring_lo = (rad - 1) * (rad - 1);
    let ring_hi = (rad + 1) * (rad + 1);
    let arm_lo = rad + 2;
    let arm_hi = rad * 2;
    for m in markers {
        let cx = m.x as i32;
        let cy = m.y as i32;
        for dy in -arm_hi..=arm_hi {
            for dx in -arm_hi..=arm_hi {
                let d2 = dx * dx + dy * dy;
                let ring = d2 >= ring_lo && d2 <= ring_hi;
                let arm = (dx.abs() <= 1 && dy.abs() >= arm_lo && dy.abs() <= arm_hi)
                    || (dy.abs() <= 1 && dx.abs() >= arm_lo && dx.abs() <= arm_hi);
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

    /// 16x16, prim id 1 fills a 10x10 block (3,3)-(12,12), rest background.
    fn fake() -> RenderOutput {
        let (w, h) = (16u32, 16u32);
        let mut ids = vec![0u32; (w * h) as usize];
        for y in 3..13 {
            for x in 3..13 {
                ids[(y * w + x) as usize] = 1;
            }
        }
        RenderOutput {
            width: w,
            height: h,
            beauty: vec![128u8; (w * h * 4) as usize],
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
            &Style::default(),
        );
        let inside = (7 * 16 + 7) * 4;
        assert!(r.beauty[inside] > 100);
        assert_eq!(r.beauty[inside + 1], 0);
        assert_eq!(r.beauty[inside + 2], 0);
        assert_eq!(&r.beauty[0..3], &[128, 128, 128]); // background untouched
        assert_eq!(r.ids[7 * 16 + 7], 1); // ids unchanged
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
            &Style {
                outline_width: 2,
                ..Style::default()
            },
        );
        // deep interior (7,7) is >2px from any non-id -> not painted
        let interior = (7 * 16 + 7) * 4;
        assert_eq!(&r.beauty[interior..interior + 3], &[128, 128, 128]);
        // boundary (3,3) neighbors background -> painted green
        let boundary = (3 * 16 + 3) * 4;
        assert_eq!(&r.beauty[boundary..boundary + 3], &[0, 255, 0]);
    }

    #[test]
    fn outline_width_is_configurable() {
        let mut r1 = fake();
        apply_selection(
            &mut r1,
            &[Selection {
                path: "/World/Box".into(),
                color: [0, 255, 0],
                mode: SelectMode::Outline,
            }],
            &Style {
                outline_width: 1,
                ..Style::default()
            },
        );
        // (4,4) is 1px inside the edge: painted at width 1? its nearest non-id is
        // (2,4) at distance 2 -> NOT within 1, so stays gray at width 1.
        let p = (4 * 16 + 4) * 4;
        assert_eq!(&r1.beauty[p..p + 3], &[128, 128, 128]);
        let mut r2 = fake();
        apply_selection(
            &mut r2,
            &[Selection {
                path: "/World/Box".into(),
                color: [0, 255, 0],
                mode: SelectMode::Outline,
            }],
            &Style {
                outline_width: 2,
                ..Style::default()
            },
        );
        // at width 2 the same pixel is within 2 of the edge -> painted
        assert_eq!(&r2.beauty[p..p + 3], &[0, 255, 0]);
    }

    #[test]
    fn outline_stays_within_own_id_no_overwrite() {
        // Two adjacent blocks: id 1 x[2,6), id 2 x[6,10), y[2,10) in a 12x12.
        let (w, h) = (12u32, 12u32);
        let mut ids = vec![0u32; (w * h) as usize];
        for y in 2..10 {
            for x in 2..6 {
                ids[(y * w + x) as usize] = 1;
            }
            for x in 6..10 {
                ids[(y * w + x) as usize] = 2;
            }
        }
        let mut r = RenderOutput {
            width: w,
            height: h,
            beauty: vec![128u8; (w * h * 4) as usize],
            ids,
            paths: vec!["/A".to_owned(), "/B".to_owned()],
            diags: vec![],
        };
        apply_selection(
            &mut r,
            &[Selection {
                path: "/A".into(),
                color: [255, 0, 0],
                mode: SelectMode::Outline,
            }],
            &Style {
                outline_width: 2,
                ..Style::default()
            },
        );
        // every id-2 pixel must be untouched (inward-only outline never crosses ids)
        for (i, &pid) in r.ids.iter().enumerate() {
            if pid == 2 {
                assert_eq!(
                    &r.beauty[i * 4..i * 4 + 3],
                    &[128, 128, 128],
                    "id2 pixel {i} overwritten"
                );
            }
        }
        // and id 1's shared edge (5,5) is painted red
        let edge = (5 * 12 + 5) * 4;
        assert_eq!(&r.beauty[edge..edge + 3], &[255, 0, 0]);
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
            &Style::default(),
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
            &Style::default(),
        );
        let ring = (16 * w + 22) as usize * 4; // radius 6
        assert_eq!(&r.beauty[ring..ring + 3], &[255, 255, 0]);
        let center = (16 * w + 16) as usize * 4;
        assert_eq!(&r.beauty[center..center + 3], &[128, 128, 128]);
    }
}
