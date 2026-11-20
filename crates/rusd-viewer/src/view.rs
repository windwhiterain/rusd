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
/// # The first fit needs only the bounds
///
/// A scene whose `geometry.camera` is `None` — an exported layer with no `Camera`
/// prim is exactly that, and so is one whose camera the extractor could not read —
/// **is still a scene**, with a bbox to frame. So the early return this function
/// used to make on `made.geometry.camera?` is gone: the fit is computed from
/// `made.bounds` alone, and the camera's presence is only consulted afterwards, for
/// the "is there anything to draw" question the caller asks separately. That early
/// return is what left `orbit` at `None` (and the gizmo, the anchors and the Fit
/// button with it) whenever a scene arrived without a camera in it.
pub fn camera_for_scene(model: &mut ClientModel, made: &MadeScene, aspect: f64) -> Option<Orbit> {
    if !model.wants_first_fit() {
        return None;
    }
    model.set_aspect(aspect);
    Some(orbit_for_bounds(
        (made.bounds.0, made.bounds.1),
        model.aspect,
    ))
}

/// The orbit that frames `bounds` in a frame of `aspect`, with no state read or
/// written: the fit itself, which every entry point below shares.
pub fn orbit_for_bounds(bounds: ([f64; 3], [f64; 3]), aspect: f64) -> Orbit {
    Orbit::from_camera(&scene::fit_camera(
        &BBox {
            min: bounds.0,
            max: bounds.1,
        },
        aspect,
    ))
}

/// Frame `bounds` now, and record that the view holds a real fit of it.
///
/// The used half of [`ClientModel::rearm_first_fit`]: after a new *document* has
/// arrived, the fit is taken and the aspect recorded in one place, so a caller
/// cannot fit without saying it did (and then keep re-fitting on every later
/// scene).
pub fn fit_to_bounds(model: &mut ClientModel, bounds: ([f64; 3], [f64; 3]), aspect: f64) -> Orbit {
    model.set_aspect(aspect);
    model.mark_framed();
    orbit_for_bounds(bounds, model.aspect)
}

/// Which `state_changed` reasons mean the **layer stack** may have changed shape,
/// so the composition tree must be re-read (`docs/PROTOCOL.md` v1.2's kind, v1.4's
/// `op` flag: a patch or an opinion can add or drop a cook op's output layer).
///
/// Every reason the contract names is listed rather than "anything but reload": a
/// server that grows a reason this build does not know would then have the tree
/// silently stop following it, and the cost of asking once too often is one
/// structural read (the panel replaces itself, the user sees no flicker). `preview`
/// is **not** a reason — it is not a mutation and advances no version — so it is
/// not here and asks for nothing.
pub fn asks_tree(reason: &str) -> bool {
    matches!(
        reason,
        "open" | "patch" | "opinion" | "undo" | "redo" | "reload"
    )
}

#[derive(Debug, Default, PartialEq)]
pub struct StateChangePlan {
    /// The camera the view must draw the scene with now, or `None` to keep the
    /// user's orbit (`docs/REMOTE.md` §6).
    pub refit: Option<Orbit>,
    /// `composition_tree{}` was sent, so the panel will soon replace its rows.
    pub asked_tree: bool,
}

/// The tag a `composition_tree` request is answered under (`app::Shell::on_reply`).
pub const TREE_TAG: &str = "composition_tree";

/// Ask `composition_tree{}` on `net` and say so on `tree`.
///
/// The one copy of "ask the tree": the shell reaches it through here for every
/// reason (`「刷新」`, the first scene, and each `state_changed`), so a test can watch
/// the wire without a window.
///
/// Rows already on screen **stay** until the answer replaces them: a
/// `state_changed` follows every mutation, and blanking the panel to a loading row on
/// each one would flicker the very thing the user is reading. A first ask (nothing on
/// screen yet) does say it is loading.
pub fn ask_tree(net: &crate::net::NetWorker, tree: &mut crate::panel::TreePanel) {
    if !matches!(tree.model, crate::tree::TreeModel::Ready(_)) {
        tree.model = crate::tree::TreeModel::Loading;
    }
    tree.note_asked(true);
    net.send(crate::net::NetCommand::Call {
        tag: TREE_TAG.to_owned(),
        method: "composition_tree".to_owned(),
        params: crate::tree::tree_params(),
    });
}

/// Ask the tree **unless a request is already in flight**; if one is, record that
/// another read is owed ([`TreePanel::take_reread`]).
///
/// This is what a `state_changed` goes through, and the coalescing is the point: a gizmo
/// drag emits an opinion up to 30 times a second, each one a `state_changed{reason:
/// "opinion"}`, and a `composition_tree` is a structural read of the whole authority
/// stack. Asking per notification would put 30 of those in flight during one drag.
/// Coalescing keeps both halves of the requirement — the tree follows the server, and it
/// follows it once per round trip instead of once per message.
fn ask_tree_coalesced(net: &crate::net::NetWorker, tree: &mut crate::panel::TreePanel) {
    if tree.asking {
        tree.reread_pending = true;
        return;
    }
    ask_tree(net, tree);
}

/// The scene's bounds as the **screen** has them: the world box of the last scene
/// adopted, which is the box a fit drawn now must frame.
///
/// Reading it from the model rather than taking it as an argument is what keeps a
/// caller from fitting a box that is not the one being shown (and it removes the
/// `&mut model` / `model.bbox` aliasing every caller would otherwise hit). `None`
/// when no scene has arrived: there is nothing to frame.
fn on_screen(model: &ClientModel) -> Option<([f64; 3], [f64; 3])> {
    model.bbox
}

/// Carry out a `state_changed`: raise the tree request and fit the camera where the
/// reason calls for it.
///
/// This is the whole reaction to the notification, with the pieces it needs handed in
/// (`model`, `tree`, `net`) and nothing from the window or the GPU — which is what
/// lets the integration test drive the same code over a socket and see both the
/// request it sends and the camera it returns. `Shell::on_state_changed` only takes
/// the answer and does the rendering.
///
/// The two rules are [`asks_tree`] and [`camera_for_state_change`]; a `preview` is not
/// a `state_changed` at all (it advances no version), so it arrives here never and
/// moves neither.
pub fn apply_state_change(
    model: &mut ClientModel,
    tree: &mut crate::panel::TreePanel,
    net: &crate::net::NetWorker,
    version: u64,
    reason: &str,
    aspect: f64,
) -> StateChangePlan {
    let refit = camera_for_state_change(model, version, reason, aspect);
    let asks = asks_tree(reason);
    if asks {
        ask_tree_coalesced(net, tree);
    }
    StateChangePlan {
        refit,
        asked_tree: asks,
    }
}

/// The camera a `state_changed` makes the view fit, or `None` to keep the user's.
///
/// Only **`open`** refits, and only `open` is a new *document*: its scene is the
/// first frame of a new thing, and the previous document's fit means nothing for
/// it (a city framed at 23 units, then a single-mesh scatter document: the second
/// lands in a corner of the first's frame). A patch, an opinion, an undo, a redo
/// and a reload re-compose the *same* document, so the user's orbit wins — the
/// rule `camera_for_scene` already states (`docs/REMOTE.md` §6: 相机归 client).
///
/// # Why it compares versions
///
/// The contract fixes the order `响应 → stage_snapshot → diagnostics →
/// state_changed`, so the new document's snapshot has **already been adopted** by
/// the time this reason arrives — through `camera_for_scene`, which says "not the
/// first scene, keep the orbit". The fit therefore has to be taken here, from the
/// bounds now on screen, and `version == model.version` is the test that the scene
/// this reason speaks for is the one being shown. When they differ (a server that
/// sent the reason first, or a client that dropped the snapshot as stale) nothing
/// is fitted: the latch is re-armed instead and the next scene to arrive fits as a
/// first one would.
pub fn camera_for_state_change(
    _model: &mut ClientModel,
    _version: u64,
    _reason: &str,
    _aspect: f64,
) -> Option<Orbit> {
    // **Nothing refits** (the user: 「为什么不彻底删掉跳转？」 — so: 彻底). A scene change
    // is another picture of the same window, and moving the camera under the user's hand
    // is never what they asked for. The camera moves when the user moves it — dragging the
    // view, or Fit / `ctl.py fit`. The one framing left is the very first scene of a
    // connection, and only because the window has no camera of its own before that
    // (`crate::view::camera_for_scene`).
    None
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
