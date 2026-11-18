//! What a gizmo drag needs to know about the selected prim, read from the
//! **composed stage** at full precision.
//!
//! `rusd_scene::geometry` hands the renderer `f32` meshes and `f64` world
//! matrices, and `MadeScene` keeps only the geometry — the stage itself is
//! `!Send` and does not travel to the render thread. A drag, though, authors a
//! value back into the prim's own `xformOp:translate`, and that value must be the
//! stage's own authored/local numbers, not anything measured off the pixels or
//! the f32 mesh:
//!
//! - the **authored local translate** (the op value the opinion will replace,
//!   which may be absent or not the last op in `xformOpOrder`),
//! - the **parent's world matrix**, the thing the world→local conversion
//!   inverts (`crate::gizmo`).
//!
//! Reading it is a handful of attribute lookups, so it happens on the thread that
//! still owns the stage, and only the plain result travels on.

use openusd::sdf::Value as SdfValue;
use openusd::usd::{Prim, Stage};
use rusd_scene::geometry::{self, Mat, SceneGeometry};

use crate::scene::BBox;

/// One prim's drag anchor.
#[derive(Clone, Debug, PartialEq)]
pub struct Anchor {
    /// The prim's rendered world box centre: where the pointer should catch it.
    pub world_center: [f64; 3],
    /// The prim's authored `xformOp:translate`, or zeros when it has none — the
    /// local op value a drag starts from.
    pub local_translate: [f64; 3],
    /// The parent prim's composed world matrix, which a world delta is undone by.
    pub parent_world: Mat,
}

impl Anchor {
    /// Read `path`'s anchor from the composed `stage`, `mesh` its local geometry
    /// under `world`.
    ///
    /// `None` when the path is not a prim of this stage.
    pub fn read(stage: &Stage, path: &str, world: Mat, mesh: &geometry::Mesh) -> Option<Self> {
        let spath = openusd::sdf::path(path).ok()?;
        let prim = stage.prim(spath).ok()?;
        // `Stage::prim` answers an unvalidated handle (`openusd` mirrors C++
        // `UsdStage::GetPrimAtPath`), so the existence check is this query's job.
        if !prim.is_valid().unwrap_or(false) {
            return None;
        }
        Some(Self {
            world_center: mesh_center_in_world(mesh, world),
            local_translate: authored_translate(&prim).unwrap_or([0.0; 3]),
            parent_world: parent_world_of(stage, &prim, world),
        })
    }

    /// A stage-free anchor, for a scene whose stage is gone (a test, or a scene
    /// that never carried one): the prim is treated as a root whose parent is the
    /// identity, and its local translate is taken as zeros.
    ///
    /// With an identity parent, world and local coincide — correct for a root
    /// prim, and the honest answer when the authoring chain is not available: a
    /// drag then writes the position the user dragged to, unchanged.
    pub fn rootless(world_center: [f64; 3], local_translate: [f64; 3]) -> Self {
        Self {
            world_center,
            local_translate,
            parent_world: geometry::IDENTITY,
        }
    }
}

/// The prim's authored `xformOp:translate`, if it has one.
fn authored_translate(prim: &Prim) -> Option<[f64; 3]> {
    match prim
        .attribute("xformOp:translate")
        .get::<SdfValue>()
        .ok()
        .flatten()?
    {
        SdfValue::Vec3f(v) => Some([f64::from(v.x), f64::from(v.y), f64::from(v.z)]),
        SdfValue::Vec3d(v) => Some([v.x, v.y, v.z]),
        SdfValue::Vec3h(_) | SdfValue::Vec3i(_) => None, // not the renderable case
        _ => None,
    }
}

/// The parent prim's world matrix.
/// The parent prim's world matrix: `own_world · inverse(own_local)`.
///
/// The composed world matrix is `parent_world · local`, so undoing the prim's own
/// local transform leaves exactly the parent chain's composition — the matrix the
/// world→local conversion has to invert. No ancestor walk is needed, and it holds
/// whatever the parent does (translate, rotate, scale).
///
/// A singular own transform (zero scale on some axis) has no inverse; then the
/// parent world is composed from the ancestry directly, which needs nothing from
/// the child.
fn parent_world_of(stage: &Stage, prim: &Prim, own_world: Mat) -> Mat {
    match crate::camera::mat_inverse(local_xform(prim)) {
        Some(inv) => geometry::mat_mul(own_world, inv),
        None => ancestors_world(stage, prim),
    }
}

/// Compose a prim's world matrix from its ancestors' authored xforms — the
/// fallback when the prim's own transform cannot be inverted.
fn ancestors_world(stage: &Stage, prim: &Prim) -> Mat {
    let mut chain: Vec<Prim> = Vec::new();
    let mut cur = prim.parent();
    while let Some(p) = cur {
        if p.path().as_str() == "/" {
            break;
        }
        cur = p.parent();
        chain.push(p);
    }
    let _ = stage;
    // `local_xform` reads authored ops, which is exactly what the extractor's own
    // `world_xform` composes, so no other stage bookkeeping is involved.
    let mut m = geometry::IDENTITY;
    for p in chain.iter().rev() {
        m = geometry::mat_mul(m, local_xform(p));
    }
    m
}

/// The prim's local transform from its authored `xformOpOrder` — the same rule and
/// the same op set as `rusd_scene::geometry`'s own `local_xform`, repeated here
/// because that function is private to the extractor.
fn local_xform(prim: &Prim) -> Mat {
    let order = match prim
        .attribute("xformOpOrder")
        .get::<SdfValue>()
        .ok()
        .flatten()
    {
        Some(SdfValue::TokenVec(ops)) => ops,
        Some(SdfValue::StringVec(ops)) => ops.iter().map(|s| s.into()).collect(),
        _ => Vec::new(),
    };
    let mut m = geometry::IDENTITY;
    for op in order {
        let op = op.to_string();
        let om = match op.as_str() {
            "xformOp:translate" => {
                geometry::mat_translation(attr_vec3(prim, &op).unwrap_or([0.0; 3]))
            }
            "xformOp:scale" => geometry::mat_scale(attr_vec3(prim, &op).unwrap_or([1.0; 3])),
            "xformOp:rotateXYZ" => {
                let r = attr_vec3(prim, &op).unwrap_or([0.0; 3]);
                let d = std::f64::consts::PI / 180.0;
                geometry::mat_mul(
                    geometry::mat_mul(
                        geometry::mat_rotation_x(r[0] * d),
                        geometry::mat_rotation_y(r[1] * d),
                    ),
                    geometry::mat_rotation_z(r[2] * d),
                )
            }
            "xformOp:transform" => attr_matrix(prim, &op).unwrap_or(geometry::IDENTITY),
            _ => geometry::IDENTITY,
        };
        m = geometry::mat_mul(m, om);
    }
    m
}

fn attr_vec3(prim: &Prim, name: &str) -> Option<[f64; 3]> {
    match prim.attribute(name).get::<SdfValue>().ok().flatten()? {
        SdfValue::Vec3f(v) => Some([f64::from(v.x), f64::from(v.y), f64::from(v.z)]),
        SdfValue::Vec3d(v) => Some([v.x, v.y, v.z]),
        _ => None,
    }
}

fn attr_matrix(prim: &Prim, name: &str) -> Option<Mat> {
    match prim.attribute(name).get::<SdfValue>().ok().flatten()? {
        // `gf::Matrix4d` is row-major; the crate's `Mat` is column-major, exactly
        // as `rusd_scene::geometry::read_matrix` handles it.
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

/// The centre of `mesh` under `world`.
fn mesh_center_in_world(mesh: &geometry::Mesh, world: Mat) -> [f64; 3] {
    geometry::mat_transform_point(world, mesh_local_center(mesh))
}

fn mesh_local_center(mesh: &geometry::Mesh) -> [f64; 3] {
    let mut min = [f64::MAX; 3];
    let mut max = [f64::MIN; 3];
    for p in &mesh.positions {
        for k in 0..3 {
            min[k] = min[k].min(f64::from(p[k]));
            max[k] = max[k].max(f64::from(p[k]));
        }
    }
    if mesh.positions.is_empty() {
        return [0.0; 3];
    }
    [
        (min[0] + max[0]) / 2.0,
        (min[1] + max[1]) / 2.0,
        (min[2] + max[2]) / 2.0,
    ]
}

/// Convenience: a scene geometry's anchor for `path`, read from the composed
/// `stage` at full precision.
pub fn anchor_of_stage(stage: &Stage, geometry: &SceneGeometry, path: &str) -> Option<Anchor> {
    let item = geometry.items.iter().find(|i| i.path == path)?;
    Anchor::read(stage, path, item.transform, &item.mesh)
}

/// Convenience for a scene with no stage: the prim is treated as a root (see
/// [`Anchor::rootless`]), which is only exact for a prim whose parent is the
/// identity.
pub fn fallback_anchor(geometry: &SceneGeometry, path: &str) -> Option<Anchor> {
    let item = geometry.items.iter().find(|i| i.path == path)?;
    Some(Anchor::rootless(BBox::of_item(item).center(), [0.0; 3]))
}
