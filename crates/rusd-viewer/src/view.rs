//! The view's own decisions, in one place so the window and its tests cannot
//! disagree.
//!
//! Round three fixed `scene::fit_camera` — the eight-corner, aspect-aware fit — and
//! its unit test passed while the real window still showed the city jammed into a
//! corner. The fit was right; the **view never asked it**. `adopt_scene` took a
//! scene's camera only when `orbit` was `None`, and by the time the first snapshot
//! arrived `orbit` had already been set from a fit of an empty box, so the correct
//! camera was composed, carried in the `MadeScene`, and dropped on the floor.
//!
//! A unit test of the fit cannot catch that, because the bug is one line of state
//! logic in `app.rs` that no test could reach. So the decision lives here: pure,
//! parameterised by the window's aspect, and called by `app::adopt_scene` **and** by
//! `tests/camera_wiring.rs` over a real socket. The wiring is now the thing under
//! test.

use crate::camera::Orbit;
use crate::scene::{self, BBox};
use crate::session::{ClientModel, MadeScene};

/// The camera a newly arrived scene should be drawn with, or `None` when the scene
/// must leave the current one alone.
///
/// - **The first scene of a connection frames the view**: the camera is fitted to
///   `aspect` — the window's shape *now*, not whatever the fit was computed with
///   when the worker composed the scene. This is what the window was missing.
/// - **Every later scene leaves the camera alone**: a patch, an opinion or a
///   reload must not jerk the view back to the fitted frame (REMOTE §6: the camera
///   belongs to the client). `None` is what says so; returning the scene's own
///   camera here instead would make the caller overwrite the user's orbit with it.
///
/// `None` also when the scene carries no camera at all.
pub fn camera_for_scene(model: &mut ClientModel, made: &MadeScene, aspect: f64) -> Option<Orbit> {
    // `geometry.camera` is always set by the composer, so a scene that has one can
    // be framed; a scene without one leaves the view as it is.
    made.geometry.camera?;
    if !model.wants_first_fit() {
        return None;
    }
    model.set_aspect(aspect);
    Some(Orbit::from_camera(&scene::fit_camera(
        &BBox {
            min: made.bounds.0,
            max: made.bounds.1,
        },
        model.aspect,
    )))
}

/// Whether `orbit` holds every corner of `bounds` inside a `(w, h)` frame, and the
/// worst `|NDC|` it reaches — the assertion a viewport's framing is judged by.
///
/// Lives beside [`camera_for_scene`] so the test that checks the wiring measures
/// exactly what a screenshot would show: a fit can be numerically right and still
/// be the wrong camera for the window if the wiring hands it a stale aspect.
pub fn framing(orbit: &Orbit, bounds: ([f64; 3], [f64; 3]), w: u32, h: u32) -> (bool, f64) {
    let m = crate::camera::view_proj(&orbit.camera(), bounds, w, h);
    let mut worst = 0.0f64;
    for x in [bounds.0[0], bounds.1[0]] {
        for y in [bounds.0[1], bounds.1[1]] {
            for z in [bounds.0[2], bounds.1[2]] {
                let c = [x, y, z];
                let clip = [
                    m[0] * c[0] + m[4] * c[1] + m[8] * c[2] + m[12],
                    m[1] * c[0] + m[5] * c[1] + m[9] * c[2] + m[13],
                    m[2] * c[0] + m[6] * c[1] + m[10] * c[2] + m[14],
                    m[3] * c[0] + m[7] * c[1] + m[11] * c[2] + m[15],
                ];
                if clip[3] <= 0.0 {
                    return (false, f64::INFINITY);
                }
                let ndc = [clip[0] / clip[3], clip[1] / clip[3]];
                worst = worst.max(ndc[0].abs()).max(ndc[1].abs());
            }
        }
    }
    (worst <= 1.0 - crate::scene::FIT_MARGIN + 1e-3, worst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::base64_encode;
    use crate::protocol::{Notification, Snapshot};

    const CITY: &str = include_str!("../../../examples/city.rusd");

    fn cooked_city() -> Vec<u8> {
        let doc = rusd_core::syntax::parse(CITY).expect("city parses");
        let out = rusd_proc::expand_doc(&doc).expect("city cooks");
        let flat = out.stage.flatten().expect("stage flattens");
        let format = openusd::sdf::LayerRegistry::find_by_id("usdc").expect("usdc");
        format
            .write_bytes(flat.data())
            .expect("usdc writes")
            .bytes
            .to_vec()
    }

    fn scene() -> MadeScene {
        let n = Notification::Snapshot(Snapshot {
            version: 1,
            format: "usdc".to_owned(),
            data: base64_encode(&cooked_city()),
        });
        match ClientModel::new().apply(&n) {
            crate::session::Effect::NewScene(m) => *m,
            _ => panic!("expected a scene"),
        }
    }

    #[test]
    fn the_first_scene_is_fitted_at_the_aspect_it_is_handed() {
        let made = scene();
        let mut model = ClientModel::new();
        let aspect = 1.0;
        let orbit = camera_for_scene(&mut model, &made, aspect).expect("fitted");
        assert_eq!(model.aspect, 1.0, "the fit used the aspect it was given");
        let (inside, worst) = framing(&orbit, made.bounds, 900, 900);
        assert!(inside, "square frame, worst |NDC| {worst:.4}");
    }

    #[test]
    fn a_second_scene_does_not_re_frame() {
        let made = scene();
        let mut model = ClientModel::new();
        assert!(camera_for_scene(&mut model, &made, 16.0 / 9.0).is_some());
        // `None` is the app's instruction to keep `self.orbit`: nothing about a
        // later scene moves the camera the user is using.
        assert!(
            camera_for_scene(&mut model, &made, 1.0).is_none(),
            "a second scene must not produce a camera, not even one for a new aspect"
        );
        assert_eq!(
            model.aspect,
            16.0 / 9.0,
            "and the aspect of the first fit is untouched"
        );
    }

    #[test]
    fn a_closed_connection_lets_the_next_scene_frame_again() {
        let made = scene();
        let mut model = ClientModel::new();
        camera_for_scene(&mut model, &made, 16.0 / 9.0).expect("first fit");
        model.mark_closed();
        // A reconnect's first snapshot is a first snapshot.
        assert!(camera_for_scene(&mut model, &made, 4.0 / 3.0).is_some());
        assert_eq!(model.aspect, 4.0 / 3.0, "the reconnect refit to its window");
    }
}
