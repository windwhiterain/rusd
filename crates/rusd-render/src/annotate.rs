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
