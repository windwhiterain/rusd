//! Snapshot → rendered view: the client's pipeline and its state machine, with
//! no socket and no window in the way.
//!
//! One `stage_snapshot` becomes a [`MadeScene`]: base64 → `usdc` bytes →
//! `Layer::from_bytes` (an anonymous single layer, Tier A — the snapshot is
//! self-sufficient) → one-layer [`crate::MemResolver`] stack → fitted geometry.
//! [`ClientModel`] then holds what the UI shows: the latest accepted version, the
//! selection, the camera, and the counters a status bar reads.
//!
//! Everything here is pure on purpose: the same functions run under a test with
//! no GPU, which is why the version-discard rule and the pick/selection flow can
//! be checked against a mock server without a window (`tests/protocol_client.rs`).

use rusd_core::{Diagnostic, Severity};
use rusd_scene::geometry::{self, SceneGeometry};

use crate::compose::Stack;
use crate::protocol::{ClientState, Notification, Snapshot};
use crate::scene::{self, BBox};

/// A composed, fitted scene from one snapshot.
pub struct MadeScene {
    /// The version the server sent it with.
    pub version: u64,
    /// Owned, renderer-ready geometry (camera replaced by the frame-fitting one).
    pub geometry: SceneGeometry,
    /// The world box, kept because the projection derives near/far from it.
    pub bounds: ([f64; 3], [f64; 3]),
    /// Every diagnostic from decoding, composition and extraction.
    pub diags: Vec<Diagnostic>,
    /// The composed stage this scene was extracted from, kept for the one reader
    /// that needs authored values rather than geometry: the gizmo's drag anchor
    /// (`crate::anchor`). `Stage` is `Rc` and `!Send`, so this scene must stay on
    /// the thread it was made on; the render thread is handed the geometry alone.
    pub stage: Option<openusd::usd::Stage>,
}

impl MadeScene {
    /// Turn one `stage_snapshot`'s bytes into geometry: decode base64, compose a
    /// one-layer stack in memory, extract, fit the camera.
    ///
    /// The layer is registered under the snapshot's own tag, so an error names
    /// the version the caller would have to ask the server about.
    pub fn from_snapshot(snap: &Snapshot) -> Result<Self, Vec<Diagnostic>> {
        let bytes = snap.decode()?;
        let tag = format!("snapshot-v{}.{}", snap.version, snap.format);
        Self::from_layer_bytes(bytes, &tag, snap.version)
    }

    /// The same for a `.usdc` read from disk — the `--usdc` fallback path, so a
    /// client with no server still has a scene to look at.
    pub fn from_usdc_file(path: &str) -> Result<Self, Vec<Diagnostic>> {
        let bytes =
            std::fs::read(path).map_err(|e| vec![crate::diag("E_READ", format!("{path}: {e}"))])?;
        Self::from_layer_bytes(bytes, path, 0)
    }

    /// Decode layer bytes as one anonymous layer, compose, extract, fit.
    pub fn from_layer_bytes(
        bytes: Vec<u8>,
        tag: &str,
        version: u64,
    ) -> Result<Self, Vec<Diagnostic>> {
        let layer = openusd::sdf::Layer::from_bytes(tag, bytes.clone())
            .map_err(|e| vec![crate::diag("E_LAYER_DECODE", format!("{tag}: {e}"))])?;
        let mut mem = crate::MemResolver::new();
        mem.insert(layer.identifier(), bytes);
        let stack = Stack::open(mem, layer.identifier(), None, Vec::new())?;
        let mut diags = stack.diags.clone();
        let geometry = scene::extract_fitted(&stack.stage);
        diags.extend(geometry.diags.iter().cloned());
        let bounds = geometry.bounds;
        Ok(Self {
            version,
            geometry,
            bounds,
            diags,
            stage: Some(stack.stage),
        })
    }

    /// The world box.
    pub fn bbox(&self) -> BBox {
        BBox {
            min: self.bounds.0,
            max: self.bounds.1,
        }
    }

    /// How many prims are renderable.
    pub fn item_count(&self) -> usize {
        self.geometry.items.len()
    }

    /// Whether anything in this snapshot is an error (the status bar shows that
    /// a scene loaded *with* a failure, rather than pretending it is clean).
    pub fn has_error(&self) -> bool {
        self.diags.iter().any(|d| d.severity == Severity::Error)
    }

    /// The prim path an id names (id 0 is background).
    pub fn path_of(&self, id: u32) -> Option<&str> {
        if id == 0 {
            None
        } else {
            self.geometry
                .items
                .get((id - 1) as usize)
                .map(|i| i.path.as_str())
        }
    }
}

/// One entry of the status bar's diagnostic list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagLine {
    pub severity: Severity,
    pub code: String,
    pub message: String,
}

impl From<&Diagnostic> for DiagLine {
    fn from(d: &Diagnostic) -> Self {
        Self {
            severity: d.severity,
            code: d.code.clone(),
            message: d.message.clone(),
        }
    }
}

/// Which gesture the pointer is currently making over the viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drag {
    /// Orbit: drag with the primary button and a modifier-free press.
    Orbit,
    /// Pan: drag with the secondary button (or primary + shift).
    Pan,
    /// Move the selected prim: drag with the primary button on the selection.
    Gizmo,
}

/// The pure client state the view drives and a test can drive the same way.
///
/// It holds no GPU handle and no socket: [`ClientModel::apply`] is one incoming
/// notification, [`ClientModel::pick`] is one click, and each returns what the
/// caller must now do (re-render, upload a scene, send an opinion, nothing), so
/// the decision is testable and the effect stays with the thread that owns it.
pub struct ClientModel {
    /// Protocol-level state: version floor, hello/subscribe, reasons.
    pub state: ClientState,
    /// The newest accepted snapshot's version, once one arrived.
    pub version: Option<u64>,
    /// The selected prim path, or `None`.
    pub selection: Option<String>,
    /// A `state_changed{reason:"reload"}` was seen: the UI shows a hot-reload
    /// badge until the next snapshot for that reload arrives.
    pub reload_badge: bool,
    /// Human-readable status lines from the last snapshot.
    pub diags: Vec<DiagLine>,
    /// Prims in the last snapshot.
    pub item_count: usize,
    /// The bbox of the last snapshot, for the status line.
    pub bbox: Option<([f64; 3], [f64; 3])>,
    /// Why the last `state_changed` said the server re-composed, for the status
    /// bar (`open` / `patch` / `opinion` / `undo` / `redo` / `reload`).
    pub last_reason: Option<String>,
    /// Each prim's world bbox center in the last scene, so a gizmo drag can start
    /// from where the prim actually is. Sorted (`BTreeMap`), so nothing depends
    /// on a hash order.
    pub centers: std::collections::BTreeMap<String, [f64; 3]>,
}

/// What an incoming notification means for the view.
///
/// No `Debug`/`PartialEq`: the `NewScene` arm carries a whole composed scene,
/// and a caller compares the *kind* (`matches!(effect, Effect::Nothing)`), which
/// is the question a UI actually asks.
pub enum Effect {
    /// A new scene: upload the geometry, render it, show the version.
    NewScene(Box<MadeScene>),
    /// The scene is unchanged; only the status changed (a reason, a diagnostic
    /// list, an ignored duplicate).
    Status,
    /// Nothing to do (a stale version, a notification this client does not show).
    Nothing,
    /// The snapshot could not be turned into a scene; the error goes in `diags`
    /// and the old scene stays on screen.
    Failed(Vec<Diagnostic>),
}

impl ClientModel {
    /// An empty model, before any server message.
    pub fn new() -> Self {
        Self {
            state: ClientState::default(),
            version: None,
            selection: None,
            reload_badge: false,
            diags: Vec::new(),
            item_count: 0,
            bbox: None,
            last_reason: None,
            centers: std::collections::BTreeMap::new(),
        }
    }

    /// Take one notification: the protocol's version rule first, then whatever
    /// the notification means.
    pub fn apply(&mut self, n: &Notification) -> Effect {
        match n {
            Notification::Snapshot(snap) => {
                if !self.state.accept(snap.version) {
                    return Effect::Nothing; // v ≤ the greatest seen: discarded
                }
                self.state.accepted += 1;
                self.version = Some(snap.version);
                self.last_reason = None;
                match MadeScene::from_snapshot(snap) {
                    Ok(made) => {
                        self.adopt(&made);
                        Effect::NewScene(Box::new(made))
                    }
                    Err(diags) => {
                        self.diags = diags.iter().map(DiagLine::from).collect();
                        Effect::Failed(diags)
                    }
                }
            }
            Notification::StateChanged { version, reason } => {
                if !self.state.accept(*version) {
                    return Effect::Nothing;
                }
                self.state.last_reason = Some(reason.clone());
                self.last_reason = Some(reason.clone());
                if reason == "reload" {
                    self.reload_badge = true;
                }
                Effect::Status
            }
            Notification::Diagnostics { items } => {
                self.diags = items
                    .iter()
                    .map(|v| DiagLine {
                        severity: match v.get("severity").and_then(|s| s.as_str()) {
                            Some("error") => Severity::Error,
                            Some("warning") => Severity::Warning,
                            _ => Severity::Note,
                        },
                        code: v
                            .get("code")
                            .and_then(|c| c.as_str())
                            .unwrap_or("N_WIRE")
                            .to_owned(),
                        message: v
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("")
                            .to_owned(),
                    })
                    .collect();
                Effect::Status
            }
            Notification::Other { .. } => Effect::Nothing,
        }
    }

    /// Record what a scene contains, and make sure the selection still names a
    /// prim in it: a selection that survived a snapshot change would outline
    /// nothing.
    pub fn adopt(&mut self, made: &MadeScene) {
        self.item_count = made.item_count();
        self.bbox = Some(made.bounds);
        self.diags = made.diags.iter().map(DiagLine::from).collect();
        self.centers = made
            .geometry
            .items
            .iter()
            .map(|it| (it.path.clone(), BBox::of_item(it).center()))
            .collect();
        if let Some(sel) = &self.selection
            && !self.centers.contains_key(sel)
        {
            self.selection = None;
        }
    }

    /// Where a prim was in the last scene: the anchor a gizmo drag starts from.
    pub fn prim_center(&self, path: &str) -> Option<[f64; 3]> {
        self.centers.get(path).copied()
    }

    /// A click at pixel `(x, y)` by prim id: select the prim under it, or clear
    /// the selection if it is background. The view uses [`ClientModel::pick_with`]
    /// (ids and the id→path table together); this is the id-only form, kept
    /// because a caller that already has the path table needs neither.
    pub fn pick_id(&mut self, ids: &[u32], width: u32, x: u32, y: u32) -> u32 {
        if x >= width {
            return 0;
        }
        ids.get((y * width + x) as usize).copied().unwrap_or(0)
    }

    /// The click path the view uses: ids and the id → path table together, so a
    /// selection is the frame's own answer, not a second lookup.
    pub fn pick_with(
        &mut self,
        ids: &[u32],
        paths: &[String],
        width: u32,
        height: u32,
        x: u32,
        y: u32,
    ) -> Option<String> {
        if width == 0 || height == 0 || x >= width || y >= height {
            return self.selection.clone();
        }
        let id = ids.get((y * width + x) as usize).copied().unwrap_or(0);
        self.selection = if id == 0 {
            None
        } else {
            paths.get((id - 1) as usize).cloned()
        };
        self.selection.clone()
    }

    /// Whether the hot-reload badge should be cleared: a snapshot arrived for the
    /// reload that raised it.
    pub fn clear_reload(&mut self) {
        self.reload_badge = false;
    }

    /// The status bar's one-line summary.
    pub fn status_line(&self) -> String {
        let conn = if self.state.is_live() {
            "live"
        } else {
            "connecting"
        };
        let version = match self.version {
            Some(v) => format!("v{v}"),
            None => "no snapshot".to_owned(),
        };
        let prims = format!("{} prims", self.item_count);
        let sel = match &self.selection {
            Some(p) => format!("sel {p}"),
            None => "no selection".to_owned(),
        };
        let reason = match &self.last_reason {
            Some(r) => format!("({r})"),
            None => String::new(),
        };
        format!("{conn} · {version} · {prims} · {sel} {reason}")
            .trim_end()
            .to_owned()
    }
}

impl Default for ClientModel {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract geometry from a composed stage and fit the camera — the one call
/// `MadeScene` and the `--usdc` fallback share.
pub fn extract_fitted(stage: &openusd::usd::Stage) -> SceneGeometry {
    geometry::extract(stage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::base64_encode;
    use rusd_proc;

    const CITY: &str = include_str!("../../../examples/city.rusd");

    /// The cooked city `usdc`, in memory: what a server would put in a snapshot.
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

    fn snapshot(version: u64, bytes: &[u8]) -> Notification {
        Notification::Snapshot(Snapshot {
            version,
            format: "usdc".to_owned(),
            data: base64_encode(bytes),
        })
    }

    #[test]
    fn a_snapshot_becomes_a_fitted_scene_with_the_cooked_box() {
        let made = MadeScene::from_snapshot(&match snapshot(4, &cooked_city()) {
            Notification::Snapshot(s) => s,
            _ => unreachable!(),
        })
        .expect("the snapshot composes");
        assert_eq!(made.version, 4);
        assert_eq!(made.item_count(), 434, "city's 434 renderable prims");
        let b = made.bbox();
        assert!(
            (b.min[0] + 210.262254).abs() < 1e-3,
            "bbox min x {}",
            b.min[0]
        );
        assert!((b.max[0] - 444.0).abs() < 1e-3, "bbox max x {}", b.max[0]);
        assert!(made.geometry.camera.is_some(), "a camera was fitted");
        assert!(made.paths_include("/World/City"), "kitchen-sink present");
    }

    impl MadeScene {
        fn paths_include(&self, p: &str) -> bool {
            self.geometry.items.iter().any(|i| i.path.starts_with(p))
        }
    }

    #[test]
    fn the_model_accepts_a_newer_version_and_discards_a_stale_one() {
        let bytes = cooked_city();
        let mut m = ClientModel::new();
        let e = m.apply(&snapshot(5, &bytes));
        assert!(matches!(e, Effect::NewScene(_)), "first snapshot accepted");
        assert_eq!(m.version, Some(5));
        assert_eq!(m.state.accepted, 1);

        let e = m.apply(&snapshot(5, &bytes));
        assert!(
            matches!(e, Effect::Nothing),
            "the same version is discarded"
        );
        let e = m.apply(&snapshot(4, &bytes));
        assert!(
            matches!(e, Effect::Nothing),
            "an older version is discarded"
        );
        assert_eq!(m.version, Some(5), "the view did not roll back");
        assert_eq!(m.state.discarded, 2);

        let e = m.apply(&snapshot(6, &bytes));
        assert!(matches!(e, Effect::NewScene(_)));
        assert_eq!(m.version, Some(6));
    }

    #[test]
    fn a_snapshot_that_is_not_a_layer_fails_without_clearing_the_view() {
        let mut m = ClientModel::new();
        m.apply(&snapshot(1, &cooked_city()));
        let bad = Notification::Snapshot(Snapshot {
            version: 2,
            format: "usdc".to_owned(),
            data: base64_encode(b"not a usdc file at all"),
        });
        let effect = m.apply(&bad);
        match &effect {
            Effect::Failed(diags) => {
                assert_eq!(diags[0].code, "E_LAYER_DECODE");
            }
            _ => panic!("expected Failed"),
        }
        assert_eq!(m.version, Some(2), "the version moved past the failed one");
        assert_eq!(
            m.item_count, 434,
            "the previous scene is still what the view has"
        );
        assert_eq!(m.diags[0].code, "E_LAYER_DECODE");
    }

    #[test]
    fn a_reload_reason_raises_the_badge_and_the_next_snapshot_clears_it() {
        let bytes = cooked_city();
        let mut m = ClientModel::new();
        m.apply(&snapshot(1, &bytes));
        assert!(!m.reload_badge);

        let e = m.apply(&Notification::StateChanged {
            version: 2,
            reason: "reload".to_owned(),
        });
        assert!(matches!(e, Effect::Status));
        assert!(m.reload_badge, "a reload shows the badge");
        assert_eq!(m.last_reason.as_deref(), Some("reload"));

        m.apply(&snapshot(3, &bytes));
        assert_eq!(m.last_reason, None, "a snapshot answers the reload");
        m.clear_reload();
        assert!(!m.reload_badge, "the badge is dismissible by the view");
    }

    #[test]
    fn a_reason_that_is_not_reload_does_not_raise_the_badge() {
        let mut m = ClientModel::new();
        m.apply(&Notification::StateChanged {
            version: 1,
            reason: "patch".to_owned(),
        });
        assert!(!m.reload_badge);
        assert_eq!(m.last_reason.as_deref(), Some("patch"));
    }

    #[test]
    fn diagnostics_notifications_replace_the_status_lines() {
        let mut m = ClientModel::new();
        let e = m.apply(&Notification::Diagnostics {
            items: vec![
                serde_json::json!({"code":"W_X","message":"warned","severity":"warning"}),
                serde_json::json!({"code":"E_Y","message":"broke","severity":"error"}),
            ],
        });
        assert!(matches!(e, Effect::Status));
        assert_eq!(m.diags.len(), 2);
        assert_eq!(m.diags[0].severity, Severity::Warning);
        assert_eq!(m.diags[1].severity, Severity::Error);
        assert_eq!(m.diags[1].code, "E_Y");
    }

    #[test]
    fn a_click_selects_the_prims_path_and_background_clears_it() {
        let mut m = ClientModel::new();
        let paths = vec!["/World/a".to_owned(), "/World/b".to_owned()];
        // 3 wide, 2 tall: row 0 is (0,0)=bg (1,0)=bg (2,0)=id2;
        // row 1 is (0,1)=id1 (1,1)=bg (2,1)=bg.
        let ids = [0u32, 0, 2, 1, 0, 0];
        assert_eq!(
            m.pick_with(&ids, &paths, 3, 2, 2, 0).as_deref(),
            Some("/World/b")
        );
        assert_eq!(
            m.pick_with(&ids, &paths, 3, 2, 0, 1).as_deref(),
            Some("/World/a")
        );
        assert_eq!(
            m.pick_with(&ids, &paths, 3, 2, 0, 0),
            None,
            "background clears"
        );
        assert!(m.selection.is_none());
    }

    #[test]
    fn a_click_outside_the_frame_leaves_the_selection_alone() {
        let mut m = ClientModel::new();
        let paths = vec!["/World/a".to_owned()];
        let ids = [1u32, 0, 0, 0];
        m.pick_with(&ids, &paths, 2, 2, 0, 0);
        assert_eq!(m.selection.as_deref(), Some("/World/a"));
        assert_eq!(
            m.pick_with(&ids, &paths, 2, 2, 9, 9).as_deref(),
            Some("/World/a"),
            "a click outside the viewport is not a click on background"
        );
    }

    #[test]
    fn a_selection_that_the_new_snapshot_dropped_is_cleared() {
        let bytes = cooked_city();
        let mut m = ClientModel::new();
        m.apply(&snapshot(1, &bytes));
        m.selection = Some("/World/City/b0_0/lot0_0".to_owned());
        m.apply(&snapshot(2, &bytes));
        assert_eq!(
            m.selection.as_deref(),
            Some("/World/City/b0_0/lot0_0"),
            "a prim the new snapshot still has stays selected"
        );
        m.selection = Some("/World/Gone".to_owned());
        m.apply(&snapshot(3, &bytes));
        assert!(m.selection.is_none(), "a vanished prim is deselected");
    }

    #[test]
    fn the_status_line_names_connection_version_and_selection() {
        let mut m = ClientModel::new();
        assert!(
            m.status_line().contains("connecting"),
            "{}",
            m.status_line()
        );
        m.state
            .on_hello(&serde_json::json!({"protocol":1,"version":1}))
            .expect("hello");
        m.state.subscribed = true;
        m.apply(&snapshot(2, &cooked_city()));
        m.selection = Some("/World/x".to_owned());
        let s = m.status_line();
        assert!(s.contains("live"), "{s}");
        assert!(s.contains("v2"), "{s}");
        assert!(s.contains("434 prims"), "{s}");
        assert!(s.contains("sel /World/x"), "{s}");
    }

    #[test]
    fn the_usdc_fallback_composes_the_same_scene_as_a_snapshot_of_it() {
        let bytes = cooked_city();
        let snap = match snapshot(1, &bytes) {
            Notification::Snapshot(s) => s,
            _ => unreachable!(),
        };
        let a = MadeScene::from_snapshot(&snap).expect("snapshot path");
        let b = MadeScene::from_layer_bytes(bytes, "fallback.usdc", 0).expect("file path");
        assert_eq!(a.item_count(), b.item_count());
        assert_eq!(a.bounds, b.bounds);
    }

    #[test]
    fn an_id_names_its_path_and_zero_is_background() {
        let snap = match snapshot(1, &cooked_city()) {
            Notification::Snapshot(s) => s,
            _ => unreachable!(),
        };
        let made = MadeScene::from_snapshot(&snap).expect("composes");
        assert_eq!(made.path_of(0), None);
        assert_eq!(
            made.path_of(1),
            Some(made.geometry.items[0].path.as_str()),
            "id 1 is the first prim in path order"
        );
        assert_eq!(made.path_of(u32::MAX), None);
    }
}
