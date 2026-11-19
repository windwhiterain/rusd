//! Geometry extraction (SPEC §7): composed openusd Stage → renderable geometry.
//!
//! This is the adapter-side half of the renderer's input ("经 Rusd 适配层提取几何"):
//! it walks the composed stage, composes each prim's `xformOpOrder` into a world
//! transform, tessellates builtin gprims (Cube/Sphere/Cylinder) or reads `Mesh`
//! topology, and collects a flat, path-sorted list of [`GeoItem`]s plus a camera.
//! It emits plain data (no wgpu); `rusd-render` bakes/uploads it.
//!
//! Conventions:
//! - Matrices are column-major `[f64; 16]` (matches wgpu/WGSL). A point transforms
//!   as `world * local`. `xformOpOrder = [op0, op1, ...]` composes as
//!   `op0 * op1 * ...` (the last op is applied to the geometry first), so
//!   `[translate, scale]` scales then translates — the USD convention.
//! - Z-up right-handed (D7); no axis conversion here (that happens at USD export).
//! - Deterministic: items are sorted by prim path; tessellation is fixed-loop.
//! - Error-tolerant: an unreadable `Mesh` becomes a magenta placeholder + a
//!   diagnostic, so a render always returns something (SPEC §7).

use std::collections::BTreeMap;

use openusd::sdf::{self, Value as SdfValue};
use openusd::usd::{Prim, PrimPredicate, Stage};
use rusd_core::Diagnostic;

/// Column-major 4x4 matrix.
pub type Mat = [f64; 16];

pub const IDENTITY: Mat = [
    1.0, 0.0, 0.0, 0.0, // col 0
    0.0, 1.0, 0.0, 0.0, // col 1
    0.0, 0.0, 1.0, 0.0, // col 2
    0.0, 0.0, 0.0, 1.0, // col 3
];

pub fn mat_translation(t: [f64; 3]) -> Mat {
    let mut m = IDENTITY;
    m[12] = t[0];
    m[13] = t[1];
    m[14] = t[2];
    m
}

pub fn mat_scale(s: [f64; 3]) -> Mat {
    let mut m = IDENTITY;
    m[0] = s[0];
    m[5] = s[1];
    m[10] = s[2];
    m
}

pub fn mat_rotation_x(rad: f64) -> Mat {
    let (s, c) = rad.sin_cos();
    let mut m = IDENTITY;
    m[5] = c;
    m[6] = s;
    m[9] = -s;
    m[10] = c;
    m
}

pub fn mat_rotation_y(rad: f64) -> Mat {
    let (s, c) = rad.sin_cos();
    let mut m = IDENTITY;
    m[0] = c;
    m[2] = -s;
    m[8] = s;
    m[10] = c;
    m
}

pub fn mat_rotation_z(rad: f64) -> Mat {
    let (s, c) = rad.sin_cos();
    let mut m = IDENTITY;
    m[0] = c;
    m[1] = s;
    m[4] = -s;
    m[5] = c;
    m
}

/// `a * b` (column-major).
pub fn mat_mul(a: Mat, b: Mat) -> Mat {
    let mut out = [0.0; 16];
    for col in 0..4 {
        for row in 0..4 {
            let mut sum = 0.0;
            for k in 0..4 {
                sum += a[k * 4 + row] * b[col * 4 + k];
            }
            out[col * 4 + row] = sum;
        }
    }
    out
}

/// Transform a point (w = 1).
pub fn mat_transform_point(m: Mat, p: [f64; 3]) -> [f64; 3] {
    let mut out = [0.0; 3];
    for row in 0..3 {
        out[row] = m[row] * p[0] + m[4 + row] * p[1] + m[8 + row] * p[2] + m[12 + row];
    }
    out
}

/// Transform a direction (upper 3x3, no translation).
pub fn mat_transform_vec(m: Mat, v: [f64; 3]) -> [f64; 3] {
    let mut out = [0.0; 3];
    for row in 0..3 {
        out[row] = m[row] * v[0] + m[4 + row] * v[1] + m[8 + row] * v[2];
    }
    out
}

/// The attribute name a point cloud is read from (USD: `UsdGeom.Points.points`,
/// `point3f[]`). D14 reads this on an *input* prim and writes instances onto an
/// output layer.
pub const POINTS_ATTR: &str = "points";

/// One prim's authored `points`, in the prim's **local** space: `point3f[]` or
/// `point3d[]`. `None` when the prim authors no such attribute (a `Mesh` does too,
/// and its vertices are a point cloud as good as any).
pub fn local_points(prim: &Prim) -> Option<Vec<[f64; 3]>> {
    match attr(prim, POINTS_ATTR)? {
        SdfValue::Vec3fVec(v) => Some(
            v.iter()
                .map(|p| [p.x as f64, p.y as f64, p.z as f64])
                .collect(),
        ),
        SdfValue::Vec3dVec(v) => Some(v.iter().map(|p| [p.x, p.y, p.z]).collect()),
        _ => None,
    }
}

/// The point cloud of the prim at `path`, in **world** space: the authored points
/// under the prim's composed world transform (its own xform ops and every
/// ancestor's — the same matrix the renderer draws it with).
///
/// `None` when the prim does not exist or authors no points. D14's geometry-in
/// contract: a scatter reads these and writes instances in its own output layer.
pub fn world_points(stage: &Stage, path: &str) -> Option<Vec<[f64; 3]>> {
    let spath = sdf::path(path).ok()?;
    let prim = stage.prim(spath).ok()?;
    let local = local_points(&prim)?;
    let xform = world_xform_of(stage, path)?;
    Some(
        local
            .into_iter()
            .map(|p| mat_transform_point(xform, p))
            .collect(),
    )
}

/// One triangle of a mesh, in **world** space.
pub type Triangle = [[f64; 3]; 3];

/// The prim's `points` tessellated by its `faceVertexCounts` /
/// `faceVertexIndices`, in world space — the input geometry a scatter samples to
/// place instances **on a surface** (D14.1).
///
/// `None` when the prim does not exist or authors no readable mesh topology (a
/// `Cube` has none authored, and its proc shape is not a surface a generator
/// should invent). Degenerate triangles are dropped: they have no area, so they
/// can never be hit by an area-weighted sample.
pub fn world_triangles(stage: &Stage, path: &str) -> Option<Vec<Triangle>> {
    let spath = sdf::path(path).ok()?;
    let prim = stage.prim(spath).ok()?;
    let local = read_mesh(&prim)?;
    let xform = world_xform_of(stage, path)?;
    let mut out = Vec::with_capacity(local.indices.len() / 3);
    let (triangles, _) = local.indices.as_chunks::<3>();
    for tri in triangles {
        let mut w = [[0.0f64; 3]; 3];
        for (k, i) in tri.iter().enumerate() {
            let p = local.positions.get(*i as usize)?;
            w[k] = mat_transform_point(xform, [p[0] as f64, p[1] as f64, p[2] as f64]);
        }
        if triangle_area(w) > 0.0 {
            out.push(w);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// A triangle's area (`|ab × ac| / 2`).
pub fn triangle_area(t: Triangle) -> f64 {
    let ab = [t[1][0] - t[0][0], t[1][1] - t[0][1], t[1][2] - t[0][2]];
    let ac = [t[2][0] - t[0][0], t[2][1] - t[0][1], t[2][2] - t[0][2]];
    let cross = [
        ab[1] * ac[2] - ab[2] * ac[1],
        ab[2] * ac[0] - ab[0] * ac[2],
        ab[0] * ac[1] - ab[1] * ac[0],
    ];
    0.5 * (cross[0] * cross[0] + cross[1] * cross[1] + cross[2] * cross[2]).sqrt()
}

/// A triangle's centroid — the point a degenerate `[u, v]` lands on.
pub fn triangle_centroid(t: Triangle) -> [f64; 3] {
    [
        (t[0][0] + t[1][0] + t[2][0]) / 3.0,
        (t[0][1] + t[1][1] + t[2][1]) / 3.0,
        (t[0][2] + t[1][2] + t[2][2]) / 3.0,
    ]
}

/// The point at barycentric `(u, v)` on a triangle: `a + u·(b − a) + v·(c − a)`,
/// which lies on the triangle (and inside it when `u, v ≥ 0` and `u + v ≤ 1`).
pub fn triangle_point(t: Triangle, u: f64, v: f64) -> [f64; 3] {
    let mut p = [0.0; 3];
    for k in 0..3 {
        p[k] = t[0][k] + u * (t[1][k] - t[0][k]) + v * (t[2][k] - t[0][k]);
    }
    p
}

/// A world-space point cloud drawn as markers: one small box per point, merged
/// into a single mesh so a `Points` prim is one `GeoItem` (one prim id in the
/// pick pass, one draw call).
///
/// The markers are what makes a **pipeline prefix** legible (D14 B): stopping the
/// stack before the scatter shows the input points, which the pipeline is about to
/// turn into instances — a cloud of nothing renders as an empty picture and says
/// nothing.
///
/// The mesh is in **local** space, like every other gprim's: callers pass the
/// points already untransformed (`world_points` gives world space; a caller that
/// wants the prim's own transform to apply instead reverses it or passes local
/// points).
pub fn points_mesh(points: &[[f64; 3]], size: f64) -> Mesh {
    // A marker is a **flat square** in the cloud's plane with one constant normal:
    // it shades uniformly (a dot of colour) instead of reading as a shaded solid.
    // v1.5 shipped a little cube per point and a user rightly mistook the cloud for
    // a pile of instances; a point must never look like geometry.
    let h = (size.max(1e-6) / 2.0) as f32;
    let mut positions = Vec::with_capacity(points.len() * 4);
    let mut normals = Vec::with_capacity(points.len() * 4);
    let mut indices = Vec::with_capacity(points.len() * 6);
    for (i, p) in points.iter().enumerate() {
        let c = [p[0] as f32, p[1] as f32, p[2] as f32];
        for (dx, dy) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            positions.push([c[0] + dx * h, c[1] + dy * h, c[2]]);
            normals.push([0.0, 0.0, 1.0]);
        }
        let base = i as u32 * 4;
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    Mesh {
        positions,
        normals,
        indices,
    }
}

/// The side of one point's marker box: a twentieth of the cloud's own diagonal,
/// clamped so a single-point cloud still gets a visible cube and a huge one does
/// not swallow its neighbours.
pub fn marker_size(points: &[[f64; 3]]) -> f64 {
    let mut min = [f64::MAX; 3];
    let mut max = [f64::MIN; 3];
    for p in points {
        for k in 0..3 {
            min[k] = min[k].min(p[k]);
            max[k] = max[k].max(p[k]);
        }
    }
    let diag =
        ((max[0] - min[0]).powi(2) + (max[1] - min[1]).powi(2) + (max[2] - min[2]).powi(2)).sqrt();
    let raw = if diag > 0.0 { diag / 20.0 } else { 1.0 };
    raw.clamp(1e-3, 1e4)
}

/// A triangulated mesh in local space.
#[derive(Clone, Debug, PartialEq)]
pub struct Mesh {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub indices: Vec<u32>,
}

/// One renderable prim: local mesh + world transform + flat beauty color.
#[derive(Clone, Debug)]
pub struct GeoItem {
    pub path: String,
    pub mesh: Mesh,
    pub transform: Mat,
    pub color: [f32; 3],
    /// Local-space centres of a `Points` prim's markers. The viewer draws them as
    /// **screen-space dots**; a point must never look like geometry.
    pub markers: Option<Vec<[f32; 3]>>,
}

/// A simple look-at perspective camera (Rusd surface `Camera` schema).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub position: [f64; 3],
    pub look_at: [f64; 3],
    pub fov_deg: f64,
}

/// The extracted, render-ready scene. `items` is sorted by path; a prim's stable
/// id for the prim-id pass is its index + 1 (0 is reserved for background).
#[derive(Clone, Debug)]
pub struct SceneGeometry {
    pub items: Vec<GeoItem>,
    pub camera: Option<Camera>,
    pub bounds: ([f64; 3], [f64; 3]),
    pub diags: Vec<Diagnostic>,
}

/// Extract renderable geometry from a composed stage.
pub fn extract(stage: &Stage) -> SceneGeometry {
    let mut paths: Vec<String> = Vec::new();
    let _ = stage.traverse(PrimPredicate::ALL, |p| paths.push(p.as_str().to_owned()));

    let mut items: Vec<GeoItem> = Vec::new();
    let mut camera: Option<Camera> = None;
    let mut diags: Vec<Diagnostic> = Vec::new();
    let mut cache: BTreeMap<String, Mat> = BTreeMap::new();

    for path in paths {
        if path == "/" || path.is_empty() {
            continue;
        }
        let Ok(spath) = sdf::path(&path) else {
            continue;
        };
        let Ok(prim) = stage.prim(spath) else {
            continue;
        };
        if !prim.is_active().unwrap_or(true) {
            continue;
        }
        // `visibility = "invisible"` (D14.1 replacement semantics): a stronger
        // layer hid this prim without deleting it — the scatter that replaced a point
        // cloud or a mesh authors exactly this over its input. Composed visibility
        // is what decides, so a prefix of the pipeline that stops before the scatter
        // sees the input again.
        if is_invisible(&prim) {
            continue;
        }
        let type_name = prim
            .type_name()
            .ok()
            .flatten()
            .map(|t| t.to_string())
            .unwrap_or_default();
        let world = world_xform(&prim, &mut cache);
        match type_name.as_str() {
            "Camera" => camera = Some(read_camera(&prim)),
            "Cube" | "Sphere" | "Cylinder" | "Mesh" | "Points" => {
                let mut color = read_color(&prim);
                let mut markers: Option<Vec<[f32; 3]>> = None;
                let mesh = match type_name.as_str() {
                    "Cube" => cube_mesh(read_f64(&prim, "size").unwrap_or(2.0)),
                    "Sphere" => sphere_mesh(read_f64(&prim, "radius").unwrap_or(1.0), 24, 16),
                    "Cylinder" => cylinder_mesh(
                        read_f64(&prim, "radius").unwrap_or(1.0),
                        read_f64(&prim, "height").unwrap_or(2.0),
                        24,
                    ),
                    "Mesh" => match read_mesh(&prim) {
                        Some(m) => m,
                        None => {
                            diags.push(
                                Diagnostic::warning(
                                    "W_RENDER_PLACEHOLDER",
                                    "mesh topology missing/invalid; using magenta placeholder",
                                )
                                .at_prim(path.clone()),
                            );
                            placeholder_mesh()
                        }
                    },
                    // D14: a point cloud draws as markers, in the prim's **local**
                    // space (the world transform is already applied to the item).
                    // The marker's size is derived from the cloud's own spread so an
                    // unscaled scene and a kilometre-wide one both read.
                    "Points" => match local_points(&prim) {
                        Some(pts) if !pts.is_empty() => {
                            color = [0.95, 0.72, 0.15];
                            markers = Some(
                                pts.iter()
                                    .map(|p| [p[0] as f32, p[1] as f32, p[2] as f32])
                                    .collect(),
                            );
                            points_mesh(&pts, marker_size(&pts))
                        }
                        _ => {
                            diags.push(
                                Diagnostic::warning(
                                    "W_RENDER_PLACEHOLDER",
                                    "Points prim authors no points; using magenta placeholder",
                                )
                                .at_prim(path.clone()),
                            );
                            placeholder_mesh()
                        }
                    },
                    _ => unreachable!(),
                };
                items.push(GeoItem {
                    path,
                    mesh,
                    transform: world,
                    color,
                    markers,
                });
            }
            _ => {} // Xform / generators / lights / materials: no direct geometry
        }
    }

    items.sort_by(|a, b| a.path.cmp(&b.path));
    let bounds = compute_bounds(&items);
    if camera.is_none() {
        camera = Some(default_camera(bounds));
    }
    SceneGeometry {
        items,
        camera,
        bounds,
        diags,
    }
}

// --- transforms -----------------------------------------------------------

/// The **world** transform of the prim at `path`: its own `xformOpOrder` and
/// every ancestor's, composed the way the renderer draws it.
///
/// `None` when there is no such prim. This is the reader a generator uses to turn
/// an input's authored points (local space) into world space (D14: geometry in).
pub fn world_xform_of(stage: &Stage, path: &str) -> Option<Mat> {
    let spath = sdf::path(path).ok()?;
    let prim = stage.prim(spath).ok()?;
    let mut cache = BTreeMap::new();
    Some(world_xform(&prim, &mut cache))
}

/// The inverse of an **affine** column-major matrix, `None` when its linear part
/// is singular.
///
/// Affine is what an xform op stack produces (rotation/scale/translate, or one
/// `xformOp:transform`): the last row is `0 0 0 1`. A full 4×4 inverse would also
/// work and would also silently "invert" a projective matrix into something that
/// is not a transform, so this one refuses what is not affine.
pub fn mat_inverse_affine(m: Mat) -> Option<Mat> {
    if (m[3]).abs() > 1e-12
        || (m[7]).abs() > 1e-12
        || (m[11]).abs() > 1e-12
        || (m[15] - 1.0).abs() > 1e-12
    {
        return None;
    }
    // Upper 3×3, column-major: column j is (m[4j], m[4j+1], m[4j+2]).
    let a = [[m[0], m[4], m[8]], [m[1], m[5], m[9]], [m[2], m[6], m[10]]];
    let det = a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1])
        - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
        + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0]);
    if det.abs() < 1e-12 {
        return None;
    }
    let inv_det = 1.0 / det;
    // Cofactor matrix, then transpose: the inverse of the linear part.
    let mut inv = IDENTITY;
    inv[0] = (a[1][1] * a[2][2] - a[1][2] * a[2][1]) * inv_det;
    inv[1] = (a[1][2] * a[2][0] - a[1][0] * a[2][2]) * inv_det;
    inv[2] = (a[1][0] * a[2][1] - a[1][1] * a[2][0]) * inv_det;
    inv[4] = (a[0][2] * a[2][1] - a[0][1] * a[2][2]) * inv_det;
    inv[5] = (a[0][0] * a[2][2] - a[0][2] * a[2][0]) * inv_det;
    inv[6] = (a[0][1] * a[2][0] - a[0][0] * a[2][1]) * inv_det;
    inv[8] = (a[0][1] * a[1][2] - a[0][2] * a[1][1]) * inv_det;
    inv[9] = (a[0][2] * a[1][0] - a[0][0] * a[1][2]) * inv_det;
    inv[10] = (a[0][0] * a[1][1] - a[0][1] * a[1][0]) * inv_det;
    // Translation: -A⁻¹ · t (t is the matrix's last column, m[12..15]). Element
    // (row, col) of a column-major matrix is `m[col * 4 + row]`, so `(A⁻¹ · t)[row]`
    // reads A⁻¹'s row `row`, i.e. indices `row`, `4 + row`, `8 + row`.
    let t = [m[12], m[13], m[14]];
    for row in 0..3 {
        inv[12 + row] = -(inv[row] * t[0] + inv[4 + row] * t[1] + inv[8 + row] * t[2]);
    }
    Some(inv)
}

fn world_xform(prim: &Prim, cache: &mut BTreeMap<String, Mat>) -> Mat {
    let key = prim.path().as_str().to_owned();
    if let Some(&w) = cache.get(&key) {
        return w;
    }
    let local = local_xform(prim);
    let parent_world = match prim.parent() {
        Some(p) if p.path().as_str() != "/" => world_xform(&p, cache),
        _ => IDENTITY,
    };
    let w = mat_mul(parent_world, local);
    cache.insert(key, w);
    w
}

fn local_xform(prim: &Prim) -> Mat {
    let order = read_token_vec(prim, "xformOpOrder").unwrap_or_default();
    let mut m = IDENTITY;
    for op in order {
        let om = match op.as_str() {
            "xformOp:translate" => mat_translation(read_vec3(prim, &op).unwrap_or([0.0; 3])),
            "xformOp:scale" => mat_scale(read_vec3(prim, &op).unwrap_or([1.0; 3])),
            "xformOp:rotateXYZ" => {
                let r = read_vec3(prim, &op).unwrap_or([0.0; 3]);
                let d = std::f64::consts::PI / 180.0;
                mat_mul(
                    mat_mul(mat_rotation_x(r[0] * d), mat_rotation_y(r[1] * d)),
                    mat_rotation_z(r[2] * d),
                )
            }
            "xformOp:transform" => read_matrix(prim, &op).unwrap_or(IDENTITY),
            _ => IDENTITY,
        };
        m = mat_mul(m, om);
    }
    m
}

// --- attribute readers ----------------------------------------------------

fn attr(prim: &Prim, name: &str) -> Option<SdfValue> {
    prim.attribute(name).get::<SdfValue>().ok().flatten()
}

fn read_f64(prim: &Prim, name: &str) -> Option<f64> {
    match attr(prim, name)? {
        SdfValue::Double(f) => Some(f),
        SdfValue::Float(f) => Some(f as f64),
        SdfValue::Int(i) => Some(i as f64),
        SdfValue::Int64(i) => Some(i as f64),
        SdfValue::Uint(u) => Some(u as f64),
        SdfValue::Uint64(u) => Some(u as f64),
        _ => None,
    }
}

fn read_vec3(prim: &Prim, name: &str) -> Option<[f64; 3]> {
    match attr(prim, name)? {
        SdfValue::Vec3f(v) => Some([v.x as f64, v.y as f64, v.z as f64]),
        SdfValue::Vec3d(v) => Some([v.x, v.y, v.z]),
        SdfValue::FloatVec(v) if v.len() == 3 => Some([v[0] as f64, v[1] as f64, v[2] as f64]),
        SdfValue::DoubleVec(v) if v.len() == 3 => Some([v[0], v[1], v[2]]),
        _ => None,
    }
}

fn read_token_vec(prim: &Prim, name: &str) -> Option<Vec<String>> {
    match attr(prim, name)? {
        SdfValue::TokenVec(v) => Some(v.iter().map(|t| t.as_str().to_owned()).collect()),
        _ => None,
    }
}

fn read_matrix(prim: &Prim, name: &str) -> Option<Mat> {
    match attr(prim, name)? {
        // gf::Matrix4d is row-major; transpose into our column-major layout.
        SdfValue::Matrix4d(m) => {
            let mut out = [0.0; 16];
            for row in 0..4 {
                for col in 0..4 {
                    out[col * 4 + row] = m.0[row * 4 + col];
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// Whether the prim's **composed** `visibility` is `invisible` (`UsdGeomImageable`'s
/// token, which any prim may carry whether or not it has the schema).
///
/// `inherited` — USD's default — and an unauthored value both mean visible.
pub fn is_invisible(prim: &Prim) -> bool {
    matches!(attr(prim, "visibility"), Some(SdfValue::Token(t)) if t.as_str() == "invisible")
}

fn read_color(prim: &Prim) -> [f32; 3] {
    match attr(prim, "primvars:displayColor") {
        Some(SdfValue::Vec3fVec(v)) => {
            if let Some(c) = v.first() {
                return [c.x, c.y, c.z];
            }
            [0.7, 0.7, 0.7]
        }
        Some(SdfValue::Vec3f(c)) => [c.x, c.y, c.z],
        _ => [0.7, 0.7, 0.7],
    }
}

fn read_camera(prim: &Prim) -> Camera {
    Camera {
        position: read_vec3(prim, "position").unwrap_or([0.0, -10.0, 5.0]),
        look_at: read_vec3(prim, "look_at").unwrap_or([0.0; 3]),
        fov_deg: read_f64(prim, "fov").unwrap_or(50.0),
    }
}

fn read_mesh(prim: &Prim) -> Option<Mesh> {
    let points = match attr(prim, "points")? {
        SdfValue::Vec3fVec(v) => v.iter().map(|p| [p.x, p.y, p.z]).collect::<Vec<_>>(),
        SdfValue::Vec3dVec(v) => v
            .iter()
            .map(|p| [p.x as f32, p.y as f32, p.z as f32])
            .collect::<Vec<_>>(),
        _ => return None,
    };
    let fvi = match attr(prim, "faceVertexIndices")? {
        SdfValue::IntVec(v) => v,
        _ => return None,
    };
    let fvc = match attr(prim, "faceVertexCounts")? {
        SdfValue::IntVec(v) => v,
        _ => return None,
    };
    triangulate_mesh(&points, &fvi, &fvc)
}

// --- tessellation ---------------------------------------------------------

/// Axis-aligned box centered at origin, full edge length `size` (USD Cube default 2).
pub fn cube_mesh(size: f64) -> Mesh {
    let h = (size / 2.0) as f32;
    let faces: [([f32; 3], [[f32; 3]; 4]); 6] = [
        (
            [1.0, 0.0, 0.0],
            [[h, -h, -h], [h, h, -h], [h, h, h], [h, -h, h]],
        ),
        (
            [-1.0, 0.0, 0.0],
            [[-h, -h, -h], [-h, -h, h], [-h, h, h], [-h, h, -h]],
        ),
        (
            [0.0, 1.0, 0.0],
            [[-h, h, -h], [-h, h, h], [h, h, h], [h, h, -h]],
        ),
        (
            [0.0, -1.0, 0.0],
            [[-h, -h, -h], [h, -h, -h], [h, -h, h], [-h, -h, h]],
        ),
        (
            [0.0, 0.0, 1.0],
            [[-h, -h, h], [h, -h, h], [h, h, h], [-h, h, h]],
        ),
        (
            [0.0, 0.0, -1.0],
            [[-h, -h, -h], [-h, h, -h], [h, h, -h], [h, -h, -h]],
        ),
    ];
    let mut positions = Vec::new();
    let mut normals = Vec::new();
    let mut indices = Vec::new();
    for (n, quad) in faces {
        let base = positions.len() as u32;
        for v in quad {
            positions.push(v);
            normals.push(n);
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    Mesh {
        positions,
        normals,
        indices,
    }
}

/// UV sphere, Z-up. Deterministic fixed segmentation.
pub fn sphere_mesh(radius: f64, segments: u32, rings: u32) -> Mesh {
    let r = radius as f32;
    let pi = std::f32::consts::PI;
    let mut positions = Vec::new();
    let mut normals = Vec::new();
    for i in 0..=rings {
        let phi = pi * (i as f32) / (rings as f32);
        let (sp, cp) = phi.sin_cos();
        for j in 0..=segments {
            let theta = 2.0 * pi * (j as f32) / (segments as f32);
            let (st, ct) = theta.sin_cos();
            let n = [sp * ct, sp * st, cp];
            positions.push([n[0] * r, n[1] * r, n[2] * r]);
            normals.push(n);
        }
    }
    let stride = segments + 1;
    let mut indices = Vec::new();
    for i in 0..rings {
        for j in 0..segments {
            let a = i * stride + j;
            let b = a + stride;
            indices.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
        }
    }
    Mesh {
        positions,
        normals,
        indices,
    }
}

/// Z-up cylinder centered at origin, from -height/2 to +height/2, with caps.
pub fn cylinder_mesh(radius: f64, height: f64, segments: u32) -> Mesh {
    let r = radius as f32;
    let h = (height / 2.0) as f32;
    let pi = std::f32::consts::PI;
    let mut positions = Vec::new();
    let mut normals = Vec::new();
    let mut indices = Vec::new();
    // side: two rings
    for &z in &[-h, h] {
        for j in 0..=segments {
            let theta = 2.0 * pi * (j as f32) / (segments as f32);
            let (st, ct) = theta.sin_cos();
            positions.push([r * ct, r * st, z]);
            normals.push([ct, st, 0.0]);
        }
    }
    let stride = segments + 1;
    for j in 0..segments {
        let a = j;
        let b = a + stride;
        indices.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
    }
    // caps (fan), each with its own center + ring verts and flat normal
    for (z, nz) in [(h, 1.0f32), (-h, -1.0f32)] {
        let center = positions.len() as u32;
        positions.push([0.0, 0.0, z]);
        normals.push([0.0, 0.0, nz]);
        let ring_start = positions.len() as u32;
        for j in 0..=segments {
            let theta = 2.0 * pi * (j as f32) / (segments as f32);
            let (st, ct) = theta.sin_cos();
            positions.push([r * ct, r * st, z]);
            normals.push([0.0, 0.0, nz]);
        }
        for j in 0..segments {
            if nz > 0.0 {
                indices.extend_from_slice(&[center, ring_start + j, ring_start + j + 1]);
            } else {
                indices.extend_from_slice(&[center, ring_start + j + 1, ring_start + j]);
            }
        }
    }
    Mesh {
        positions,
        normals,
        indices,
    }
}

/// Triangulate a USD mesh (fan per face) and compute smooth vertex normals.
pub fn triangulate_mesh(points: &[[f32; 3]], fvi: &[i32], fvc: &[i32]) -> Option<Mesh> {
    if points.is_empty() || fvi.is_empty() || fvc.is_empty() {
        return None;
    }
    let mut indices: Vec<u32> = Vec::new();
    let mut offset = 0usize;
    for &count in fvc {
        let c = count as usize;
        if c < 3 || offset + c > fvi.len() {
            return None;
        }
        let v0 = fvi[offset];
        for k in 1..c - 1 {
            let tri = [v0, fvi[offset + k], fvi[offset + k + 1]];
            for &i in &tri {
                if i < 0 || i as usize >= points.len() {
                    return None;
                }
                indices.push(i as u32);
            }
        }
        offset += c;
    }
    if indices.is_empty() {
        return None;
    }
    let mut normals = vec![[0.0f32; 3]; points.len()];
    for tri in indices.chunks(3) {
        let a = points[tri[0] as usize];
        let b = points[tri[1] as usize];
        let c = points[tri[2] as usize];
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let n = [
            ab[1] * ac[2] - ab[2] * ac[1],
            ab[2] * ac[0] - ab[0] * ac[2],
            ab[0] * ac[1] - ab[1] * ac[0],
        ];
        for &i in tri {
            let nn = &mut normals[i as usize];
            nn[0] += n[0];
            nn[1] += n[1];
            nn[2] += n[2];
        }
    }
    for n in &mut normals {
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        if len > 1e-12 {
            n[0] /= len;
            n[1] /= len;
            n[2] /= len;
        } else {
            *n = [0.0, 0.0, 1.0];
        }
    }
    Some(Mesh {
        positions: points.to_vec(),
        normals,
        indices,
    })
}

/// Magenta unit cube used when a prim's geometry is unreadable (SPEC §7).
pub fn placeholder_mesh() -> Mesh {
    cube_mesh(1.0)
}

// --- bounds / default camera ---------------------------------------------

fn mesh_aabb(mesh: &Mesh) -> ([f64; 3], [f64; 3]) {
    let mut min = [f64::MAX; 3];
    let mut max = [f64::MIN; 3];
    for p in &mesh.positions {
        for k in 0..3 {
            let v = p[k] as f64;
            min[k] = min[k].min(v);
            max[k] = max[k].max(v);
        }
    }
    if mesh.positions.is_empty() {
        ([0.0; 3], [0.0; 3])
    } else {
        (min, max)
    }
}

fn compute_bounds(items: &[GeoItem]) -> ([f64; 3], [f64; 3]) {
    let mut min = [f64::MAX; 3];
    let mut max = [f64::MIN; 3];
    for item in items {
        let (lmin, lmax) = mesh_aabb(&item.mesh);
        for cx in [lmin[0], lmax[0]] {
            for cy in [lmin[1], lmax[1]] {
                for cz in [lmin[2], lmax[2]] {
                    let w = mat_transform_point(item.transform, [cx, cy, cz]);
                    for k in 0..3 {
                        min[k] = min[k].min(w[k]);
                        max[k] = max[k].max(w[k]);
                    }
                }
            }
        }
    }
    if items.is_empty() {
        ([0.0; 3], [0.0; 3])
    } else {
        (min, max)
    }
}

fn default_camera(bounds: ([f64; 3], [f64; 3])) -> Camera {
    let (min, max) = bounds;
    let center = [
        (min[0] + max[0]) / 2.0,
        (min[1] + max[1]) / 2.0,
        (min[2] + max[2]) / 2.0,
    ];
    let dx = max[0] - min[0];
    let dy = max[1] - min[1];
    let dz = max[2] - min[2];
    let radius = (dx * dx + dy * dy + dz * dz).sqrt() / 2.0;
    let dist = (radius * 2.5).max(1.0);
    Camera {
        position: [center[0] + dist, center[1] - dist, center[2] + dist * 0.8],
        look_at: center,
        fov_deg: 50.0,
    }
}
