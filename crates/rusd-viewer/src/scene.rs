//! Geometry extraction and the headless snapshot: a composed stage → owned
//! geometry → PNG, with a bbox-fitting look-at camera.
//!
//! The render path is `rusd_render`'s, with `rusd-cli render`'s default
//! [`RenderConfig`] (background, light direction, ambient/diffuse) so a
//! snapshot from a remote composite stack is pixel-comparable with one the CLI
//! makes from the same scene. Only the camera is this module's: it fits the
//! scene's world bbox instead of reading a `Camera` prim, because a cook output
//! often has none.

use openusd::usd::Stage;
use rusd_core::Diagnostic;
use rusd_render::{RenderConfig, RenderOutput};
use rusd_scene::geometry::{self, Mat, SceneGeometry};

use crate::diag;

/// A world-space axis-aligned bounding box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BBox {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl BBox {
    /// The box of `geometry` (already world space).
    pub fn of(items: &[geometry::GeoItem]) -> Self {
        let (min, max) = bounds_of(items);
        BBox { min, max }
    }

    /// The two corners as a tuple.
    pub fn corners(&self) -> ([f64; 3], [f64; 3]) {
        (self.min, self.max)
    }

    /// Center of the box.
    pub fn center(&self) -> [f64; 3] {
        [
            (self.min[0] + self.max[0]) / 2.0,
            (self.min[1] + self.max[1]) / 2.0,
            (self.min[2] + self.max[2]) / 2.0,
        ]
    }

    /// Extent along each axis.
    pub fn size(&self) -> [f64; 3] {
        [
            self.max[0] - self.min[0],
            self.max[1] - self.min[1],
            self.max[2] - self.min[2],
        ]
    }

    /// Whether every corner is within `tol` of `other`.
    pub fn approx_eq(&self, other: &BBox, tol: f64) -> bool {
        (0..3).all(|k| {
            (self.min[k] - other.min[k]).abs() <= tol && (self.max[k] - other.max[k]).abs() <= tol
        })
    }

    /// The world box of one item: its local mesh AABB under its world transform.
    pub fn of_item(item: &geometry::GeoItem) -> Self {
        let mut min = [f64::MAX; 3];
        let mut max = [f64::MIN; 3];
        for p in &item.mesh.positions {
            let w = geometry::mat_transform_point(
                item.transform,
                [p[0] as f64, p[1] as f64, p[2] as f64],
            );
            for k in 0..3 {
                min[k] = min[k].min(w[k]);
                max[k] = max[k].max(w[k]);
            }
        }
        BBox { min, max }
    }
}

/// The world bbox of a set of extracted items (same rule as
/// [`rusd_scene::geometry::extract`]'s own `bounds`: every mesh corner under its
/// item's world transform).
fn bounds_of(items: &[geometry::GeoItem]) -> ([f64; 3], [f64; 3]) {
    let mut min = [f64::MAX; 3];
    let mut max = [f64::MIN; 3];
    for item in items {
        let b = BBox::of_item(item);
        for k in 0..3 {
            min[k] = min[k].min(b.min[k]);
            max[k] = max[k].max(b.max[k]);
        }
    }
    if items.is_empty() {
        ([0.0; 3], [0.0; 3])
    } else {
        (min, max)
    }
}

/// The default camera aperture (36 × 20.25 mm, 16:9) and focal length (35 mm) —
/// USD's `Camera` defaults, so a fitted view matches a camera prim authored with
/// them.
pub const APERTURE: (f64, f64) = (36.0, 20.25);
pub const FOCAL_MM: f64 = 35.0;

/// The vertical field of view (degrees) a 35 mm lens sees through a 20.25 mm
/// aperture: `2·atan(aperture_y / (2·focal))`.
pub fn fitted_fov_deg() -> f64 {
    let (_, aperture_y) = APERTURE;
    (2.0 * (aperture_y / (2.0 * FOCAL_MM)).atan()).to_degrees()
}

/// The look-at camera that frames `bbox`: eye at `center + (0.85d, −0.85d,
/// 0.62d)` with `d` the larger of the two plan extents, looking at the center,
/// through a 35 mm / 36×20.25 lens.
pub fn fit_camera(bbox: &BBox) -> geometry::Camera {
    let center = bbox.center();
    let size = bbox.size();
    let d = size[0].max(size[1]).max(1.0);
    geometry::Camera {
        position: [
            center[0] + 0.85 * d,
            center[1] - 0.85 * d,
            center[2] + 0.62 * d,
        ],
        look_at: center,
        fov_deg: fitted_fov_deg(),
    }
}

/// Extract `stage`'s renderable geometry and replace its camera with the fitted
/// one, so a cook output with no `Camera` prim still frames correctly.
pub fn extract_fitted(stage: &Stage) -> SceneGeometry {
    let mut geo = geometry::extract(stage);
    geo.camera = Some(fit_camera(&BBox {
        min: geo.bounds.0,
        max: geo.bounds.1,
    }));
    geo
}

/// The world transform of one extracted item, by prim path.
pub fn item_transform(geo: &SceneGeometry, path: &str) -> Option<Mat> {
    geo.items
        .iter()
        .find(|it| it.path == path)
        .map(|it| it.transform)
}

/// The world bbox of one extracted item, by prim path.
pub fn item_bbox(geo: &SceneGeometry, path: &str) -> Option<BBox> {
    geo.items
        .iter()
        .find(|it| it.path == path)
        .map(BBox::of_item)
}

/// Render `geo` offscreen at `width`×`height` with `rusd-cli render`'s defaults.
pub fn render_offscreen(
    geo: &SceneGeometry,
    width: u32,
    height: u32,
) -> Result<RenderOutput, Vec<Diagnostic>> {
    let cfg = RenderConfig {
        width,
        height,
        ..Default::default()
    };
    rusd_render::render_with(geo, &cfg)
}

/// Render `geo` and write the beauty pass to `out` as PNG.
pub fn write_snapshot(
    geo: &SceneGeometry,
    width: u32,
    height: u32,
    out: &str,
) -> Result<RenderOutput, Vec<Diagnostic>> {
    let r = render_offscreen(geo, width, height)?;
    rusd_render::capture::write_png(out, r.width, r.height, &r.beauty)
        .map_err(|e| vec![diag("E_PNG", e)])?;
    Ok(r)
}

/// Snapshot a single `.usdc` layer held in memory: read `usdc`, decode it as an
/// anonymous layer, compose it over a one-layer [`crate::MemResolver`], fit the
/// camera, render, write PNG. Returns the render output and the fitted geometry.
///
/// The file is read, never written, and the stage never touches it again: the
/// layer's bytes are served from memory, which is exactly what a remote client
/// holding a cooked artifact does.
pub fn snapshot_usdc_bytes(
    bytes: Vec<u8>,
    tag: &str,
    width: u32,
    height: u32,
    out: Option<&str>,
) -> Result<Snapshot, Vec<Diagnostic>> {
    let layer = openusd::sdf::Layer::from_bytes(tag, bytes.clone())
        .map_err(|e| vec![diag("E_LAYER_DECODE", format!("{tag}: {e}"))])?;
    let mut mem = crate::MemResolver::new();
    mem.insert(layer.identifier(), bytes);
    let stack = crate::compose::Stack::open(mem, layer.identifier(), None, Vec::new())?;
    let mut diags = stack.diags.clone();
    let geo = extract_fitted(&stack.stage);
    diags.extend(geo.diags.iter().cloned());
    let render = match out {
        Some(path) => write_snapshot(&geo, width, height, path)?,
        None => render_offscreen(&geo, width, height)?,
    };
    Ok(Snapshot {
        geometry: geo,
        render,
        diags,
    })
}

/// What a snapshot produced: the fitted geometry (bbox, items, camera), the
/// render output, and every diagnostic along the way.
pub struct Snapshot {
    pub geometry: SceneGeometry,
    pub render: RenderOutput,
    pub diags: Vec<Diagnostic>,
}

/// Snapshot a `.usdc` file on disk (read-only) to `out` PNG.
pub fn snapshot_png(
    usdc_path: &str,
    out: &str,
    width: u32,
    height: u32,
) -> Result<Snapshot, Vec<Diagnostic>> {
    let bytes =
        std::fs::read(usdc_path).map_err(|e| vec![diag("E_READ", format!("{usdc_path}: {e}"))])?;
    snapshot_usdc_bytes(bytes, usdc_path, width, height, Some(out))
}

/// The world-space translation of a 4×4 column-major transform.
pub fn translation(m: Mat) -> [f64; 3] {
    [m[12], m[13], m[14]]
}

/// Whether the file at `path` exists and is non-empty (a PNG with pixels has a
/// file size; a zero-byte file means the write failed silently).
pub fn png_is_nonempty(path: &str) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}
