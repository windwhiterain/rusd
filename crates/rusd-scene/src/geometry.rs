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
        let type_name = prim
            .type_name()
            .ok()
            .flatten()
            .map(|t| t.to_string())
            .unwrap_or_default();
        let world = world_xform(&prim, &mut cache);
        match type_name.as_str() {
            "Camera" => camera = Some(read_camera(&prim)),
            "Cube" | "Sphere" | "Cylinder" | "Mesh" => {
                let color = read_color(&prim);
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
                    _ => unreachable!(),
                };
                items.push(GeoItem {
                    path,
                    mesh,
                    transform: world,
                    color,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_then_scale_matches_usd_order() {
        // xformOpOrder = [translate, scale] => M = T * S (scale applied first).
        let t = mat_translation([3.0, 0.0, 1.0]);
        let s = mat_scale([1.0, 2.0, 3.0]);
        let m = mat_mul(t, s);
        // unit-cube corner (1,1,1) -> scale (1,2,3) -> translate (4,2,4)
        let p = mat_transform_point(m, [1.0, 1.0, 1.0]);
        assert_eq!(p, [4.0, 2.0, 4.0]);
        // opposite corner (-1,-1,-1) -> (-1,-2,-3) -> (2,-2,-2)
        let q = mat_transform_point(m, [-1.0, -1.0, -1.0]);
        assert_eq!(q, [2.0, -2.0, -2.0]);
    }

    #[test]
    fn mat_mul_identity_and_assoc() {
        let a = mat_translation([1.0, 2.0, 3.0]);
        assert_eq!(mat_mul(a, IDENTITY), a);
        assert_eq!(mat_mul(IDENTITY, a), a);
        let b = mat_scale([2.0, 2.0, 2.0]);
        let c = mat_rotation_z(0.7);
        let lhs = mat_mul(mat_mul(a, b), c);
        let rhs = mat_mul(a, mat_mul(b, c));
        for i in 0..16 {
            assert!((lhs[i] - rhs[i]).abs() < 1e-12);
        }
    }

    #[test]
    fn cube_mesh_topology() {
        let m = cube_mesh(2.0);
        assert_eq!(m.positions.len(), 24);
        assert_eq!(m.normals.len(), 24);
        assert_eq!(m.indices.len(), 36);
        // half-extent 1
        assert!(
            m.positions
                .iter()
                .all(|p| p.iter().all(|&c| c.abs() <= 1.0))
        );
    }

    #[test]
    fn sphere_mesh_radius_and_winding_count() {
        let m = sphere_mesh(1.5, 24, 16);
        assert_eq!(m.positions.len(), (24 + 1) * (16 + 1));
        assert_eq!(m.indices.len(), 24 * 16 * 6);
        for p in &m.positions {
            let len = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
            assert!((len - 1.5).abs() < 1e-4);
        }
    }

    #[test]
    fn triangulate_quad_fan_and_normals() {
        // unit quad in XY, two triangles via a 4-vertex face
        let pts = [
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ];
        let m = triangulate_mesh(&pts, &[0, 1, 2, 3], &[4]).unwrap();
        assert_eq!(m.indices.len(), 6);
        // smooth normal points +Z for a CCW XY quad
        for n in &m.normals {
            assert!(n[2] > 0.99);
        }
    }

    #[test]
    fn triangulate_rejects_bad_index() {
        let pts = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        assert!(triangulate_mesh(&pts, &[0, 1, 9], &[3]).is_none());
        assert!(triangulate_mesh(&pts, &[0, 1, 2], &[2]).is_none());
    }

    #[test]
    fn cylinder_has_caps() {
        let m = cylinder_mesh(1.0, 2.0, 24);
        // side + 2 caps, non-empty and finite
        assert!(!m.indices.is_empty());
        assert!(m.positions.iter().all(|p| p.iter().all(|c| c.is_finite())));
    }
}
