//! World → local for the gizmo: the conversion that belongs to the **client**.
//!
//! The ruling recorded in `docs/PROTOCOL.md` ("v1 实现语义补记", 2026-11-17) is
//! that an `opinion`'s `value` is the field's Sdf value **written as-is** into the
//! overrides session layer. For `xformOp:translate` that value is the prim's own
//! op — a **local** coordinate, in its parent's space. The server does not
//! interpret it as a world position, and it must not: a lot sits under a block
//! that already carries a `translate`, so a world value written into the lot's op
//! would apply the block's offset twice — the same class of mistake M6 fixed by
//! hand ("city lot 落位去除重复叠加的街区原点").
//!
//! So the drag pipeline is:
//!
//! ```text
//! pointer pixel  → ray (camera::Orbit::ray) → plane hit        (world)
//! world target   − prim's world centre = Δworld                (a vector)
//! Δlocal         = inverse(parent world) · Δworld              (linear part only)
//! new op value   = prim's authored local translate + Δlocal    (local)
//! ```
//!
//! The inverse is the **full affine inverse of the parent's world matrix**, not a
//! subtraction of its translation: a parent that rotates or scales has to be
//! undone as well, or the prim would drift whenever the block it hangs under is
//! not axis-aligned.
//!
//! The delta form matters for correctness at press: a prim's `xformOp:translate`
//! is the position of its *local origin*, which is not in general its visual
//! centre (a `[translate, scale]` cube is centred, a mesh with off-centre topology
//! is not). Adding the local delta to the authored value keeps the prim exactly
//! where it was at the moment of the press, instead of snapping it so that its
//! local origin sits under the cursor.

use rusd_scene::geometry::{self, Mat};

use crate::camera::mat_inverse;

/// The local-space translation that puts a prim `delta_world` away from where it
/// is now, given the prim's parent world matrix and its current local
/// `xformOp:translate`.
///
/// `None` when the parent's transform is singular (a zero scale), where "local"
/// has no unique answer.
pub fn drag_translate(
    parent_world: Mat,
    start_local: [f64; 3],
    delta_world: [f64; 3],
) -> Option<[f64; 3]> {
    let inv = mat_inverse(parent_world)?;
    // A delta is a vector: the inverse's translation column must not apply to it,
    // or the parent's own offset would be added a second time.
    let d = geometry::mat_transform_vec(inv, delta_world);
    Some([
        start_local[0] + d[0],
        start_local[1] + d[1],
        start_local[2] + d[2],
    ])
}

/// The prim's local coordinates for a world point — the plain conversion the
/// ruling names, and the one a caller holding an absolute world target uses.
///
/// This is `inverse(parent world) · world`. For the gizmo's *drag* use
/// [`drag_translate`] instead: it moves the prim by a delta, so a prim whose
/// geometry is not centred on its local origin does not jump when the drag starts.
pub fn world_to_local(parent_world: Mat, world: [f64; 3]) -> Option<[f64; 3]> {
    let inv = mat_inverse(parent_world)?;
    let p = geometry::mat_transform_point(inv, world);
    Some([p[0], p[1], p[2]])
}

/// The inverse of [`world_to_local`]: where a local point lands in world space.
/// Present so a test (and a future gizmo handle) can round-trip, and so the
/// convention here is checkable rather than asserted.
pub fn local_to_world(parent_world: Mat, local: [f64; 3]) -> [f64; 3] {
    geometry::mat_transform_point(parent_world, local)
}
