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

#[cfg(test)]
mod tests {
    use super::*;

    /// A parent that both rotates and translates: the case a translation-only
    /// fix gets wrong.
    fn rotated_parent() -> Mat {
        let r = geometry::mat_rotation_z(std::f64::consts::FRAC_PI_2); // +X → +Y
        let t = geometry::mat_translation([100.0, 50.0, 5.0]);
        geometry::mat_mul(t, r)
    }

    fn close(a: [f64; 3], b: [f64; 3], tol: f64) -> bool {
        (0..3).all(|k| (a[k] - b[k]).abs() <= tol)
    }

    #[test]
    fn a_world_target_under_a_rotated_and_scaled_parent_inverts_to_the_local_value() {
        // The general case the ruling asks for: the parent both rotates and
        // scales (and translates), so the conversion is a full inverse, not a
        // subtraction.
        let rotate = geometry::mat_rotation_z(std::f64::consts::FRAC_PI_2); // +X → +Y
        let scale = geometry::mat_scale([2.0, 2.0, 2.0]);
        let translate = geometry::mat_translation([1000.0, -500.0, 40.0]);
        let parent = geometry::mat_mul(translate, geometry::mat_mul(rotate, scale));

        // A world target the pointer reached, and the prim's own current local op.
        let start_local = [3.0, -1.0, 0.5];
        let start_world = local_to_world(parent, start_local);
        let world_target = [start_world[0] + 10.0, start_world[1] + 4.0, start_world[2]];

        let delta_world = sub3(world_target, start_world);
        let got = drag_translate(parent, start_local, delta_world).expect("invertible");

        // Independently: the prim's new world position must be the world target...
        let new_world = local_to_world(parent, got);
        assert!(
            close(new_world, world_target, 1e-9),
            "the local value reproduces the world target: {new_world:?} vs {world_target:?}"
        );
        // ...and the local value is *not* the world one (the whole point).
        assert!(
            !close(got, world_target, 1.0),
            "a local result must not look like a world position: {got:?}"
        );

        // Undoing the parent by hand gives the same answer: the delta is rotated
        // by -90° and halved, and the parent's translate never enters.
        let want_delta = [
            delta_world[1] / 2.0,
            -delta_world[0] / 2.0,
            delta_world[2] / 2.0,
        ];
        let want = [
            start_local[0] + want_delta[0],
            start_local[1] + want_delta[1],
            start_local[2] + want_delta[2],
        ];
        assert!(
            close(got, want, 1e-9),
            "hand-computed local: {got:?} vs {want:?}"
        );

        // And `world_to_local` — the absolute form the ruling names — agrees.
        let absolute = world_to_local(parent, world_target).expect("invertible");
        assert!(
            close(absolute, got, 1e-9),
            "when the prim's origin is its centre, both forms agree: {absolute:?} vs {got:?}"
        );
    }

    fn sub3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
        [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
    }

    #[test]
    fn world_to_local_undoes_a_rotation_and_a_translation() {
        let parent = rotated_parent();
        // The parent maps local (10, 20, 0) to world: rotate 90° ⇒ (-20, 10, 0),
        // then translate ⇒ (80, 60, 5).
        let local = [10.0, 20.0, 0.0];
        let world = local_to_world(parent, local);
        assert!(
            close(world, [80.0, 60.0, 5.0], 1e-9),
            "parent convention: {world:?}"
        );
        let back = world_to_local(parent, world).expect("invertible");
        assert!(close(back, local, 1e-9), "round trip: {back:?}");
    }

    #[test]
    fn a_delta_is_rotated_back_but_not_translated_back() {
        let parent = rotated_parent();
        // A world delta of +Y is +X in the parent's rotated frame; the parent's
        // (100, 50, 5) offset must not appear in the answer at all.
        let got = drag_translate(parent, [0.0; 3], [0.0, 7.0, 0.0]).expect("invertible");
        assert!(
            close(got, [7.0, 0.0, 0.0], 1e-9),
            "world +Y is local +X here, and no offset: {got:?}"
        );
    }

    #[test]
    fn the_drag_keeps_a_prim_whose_centre_is_not_its_origin_from_jumping() {
        // A prim whose local translate is (5, 5, 0) but whose visual centre sits
        // 3 units further along local X. A zero drag must produce a zero change,
        // which is only true of the delta form.
        let parent = geometry::mat_translation([100.0, 0.0, 0.0]);
        let start_local = [5.0, 5.0, 0.0];
        let got = drag_translate(parent, start_local, [0.0, 0.0, 0.0]).expect("invertible");
        assert!(
            close(got, start_local, 1e-12),
            "no movement at press: {got:?}"
        );

        // And a world nudge moves it by exactly that much in local terms.
        let got = drag_translate(parent, start_local, [2.0, -4.0, 0.0]).expect("invertible");
        assert!(close(got, [7.0, 1.0, 0.0], 1e-12), "{got:?}");
    }

    #[test]
    fn a_singular_parent_has_no_local_answer() {
        let flat = geometry::mat_scale([1.0, 0.0, 1.0]);
        assert!(world_to_local(flat, [1.0, 2.0, 3.0]).is_none());
        assert!(drag_translate(flat, [0.0; 3], [1.0, 0.0, 0.0]).is_none());
    }

    #[test]
    fn the_identity_parent_leaves_a_world_value_unchanged() {
        let got = world_to_local(geometry::IDENTITY, [1.5, -2.5, 3.25]).expect("invertible");
        assert!(close(got, [1.5, -2.5, 3.25], 1e-12), "{got:?}");
    }
}
