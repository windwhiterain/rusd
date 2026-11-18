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
    /// `aspect` is the window's `width / height`: the fit needs it to hold the
    /// whole box in a frame of that shape ([`scene::fit_camera`]).
    ///
    /// The layer is registered under the snapshot's own tag, so an error names
    /// the version the caller would have to ask the server about.
    pub fn from_snapshot(snap: &Snapshot, aspect: f64) -> Result<Self, Vec<Diagnostic>> {
        let bytes = snap.decode()?;
        let tag = format!("snapshot-v{}.{}", snap.version, snap.format);
        Self::from_layer_bytes(bytes, &tag, snap.version, aspect)
    }

    /// The same for a `.usdc` read from disk — the `--usdc` fallback path, so a
    /// client with no server still has a scene to look at.
    pub fn from_usdc_file(path: &str, aspect: f64) -> Result<Self, Vec<Diagnostic>> {
        let bytes =
            std::fs::read(path).map_err(|e| vec![crate::diag("E_READ", format!("{path}: {e}"))])?;
        Self::from_layer_bytes(bytes, path, 0, aspect)
    }

    /// Decode layer bytes as one anonymous layer, compose, extract, fit.
    pub fn from_layer_bytes(
        bytes: Vec<u8>,
        tag: &str,
        version: u64,
        aspect: f64,
    ) -> Result<Self, Vec<Diagnostic>> {
        let layer = openusd::sdf::Layer::from_bytes(tag, bytes.clone())
            .map_err(|e| vec![crate::diag("E_LAYER_DECODE", format!("{tag}: {e}"))])?;
        let mut mem = crate::MemResolver::new();
        mem.insert(layer.identifier(), bytes);
        let stack = Stack::open(mem, layer.identifier(), None, Vec::new())?;
        let mut diags = stack.diags.clone();
        let geometry = scene::extract_fitted(&stack.stage, aspect);
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
    /// The window's `width / height` at which a snapshot's camera is fitted.
    ///
    /// A snapshot is composed once and used for as long as the scene stands, so
    /// the fit needs the aspect at that moment; a later resize does **not** refit
    /// (the user's orbit wins — `docs/REMOTE.md`'s "相机归 client"); the **first**
    /// scene of a connection always fits, at the window's aspect then (see
    /// `ClientModel::apply`).
    pub aspect: f64,
    /// Whether a scene has been adopted since the last connection, i.e. whether
    /// the camera is a real fit rather than absent. The first scene after a
    /// connect replaces the camera; later ones keep the user's.
    pub framed: bool,
    /// The scene on screen came from `--usdc`, not from a server: the status bar
    /// says 离线 rather than claiming a connection it does not have.
    pub offline: bool,
    /// Each prim's world bbox center in the last scene, so a gizmo drag can start
    /// from where the prim actually is. Sorted (`BTreeMap`), so nothing depends
    /// on a hash order.
    pub centers: std::collections::BTreeMap<String, [f64; 3]>,
    /// **Which composition the scene on screen is** (v1.4, D14): the cook-op layer
    /// `preview{upto}` last asked for, or `None` for the final composition.
    ///
    /// It comes from the `stage_snapshot.upto` the server sent, not from what the
    /// client asked for: a server that answers the request and then composes a
    /// prefix says which prefix that was, and this is the echo the status bar and
    /// the overlay print. A client that recorded only its own request would claim a
    /// prefix while the pixels were still the final scene.
    pub preview: Option<String>,
}

/// What an incoming notification means for the view.
///
/// No `Debug`/`PartialEq`: the `NewScene` arm carries a whole composed scene,
/// and a caller compares the *kind* (`matches!(effect, Effect::Nothing)`), which
/// is the question a UI actually asks.
pub enum Effect {
    /// The server said it re-composed, and this is why.
    ///
    /// A variant of its own rather than `Status`, because the reason is not a
    /// label: it is what decides whether the shell must do something. An `open`
    /// is a **new document** — its scene is the first frame of one, so the camera
    /// is fitted to it as it would be for a first connection, and the composition
    /// tree is re-read because the layers are another document's. The other
    /// reasons re-read the tree (the layers may have gained a cook op) and leave
    /// the camera alone. `Status` could not say any of that, and inferring it from
    /// `ClientModel::last_reason` afterwards would have fired on a following
    /// `diagnostics`, which leaves that field standing.
    StateChanged { version: u64, reason: String },
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
            aspect: 16.0 / 9.0,
            framed: false,
            offline: false,
            centers: std::collections::BTreeMap::new(),
            preview: None,
        }
    }

    /// The client is connected: `hello` was answered with our protocol and
    /// `subscribe` was granted.
    ///
    /// This is what turns the status line from `connecting` into a live one — the
    /// app never set it before, so a viewer that had been receiving snapshots for
    /// minutes still claimed to be connecting. A dropped connection clears it.
    pub fn mark_live(&mut self) {
        self.state.protocol = Some(crate::protocol::PROTOCOL);
        self.state.subscribed = true;
    }

    /// The connection ended: the status line must stop claiming to be live, and
    /// the next scene to arrive counts as a fresh first fit.
    pub fn mark_closed(&mut self) {
        self.state.subscribed = false;
        self.framed = false;
    }

    /// Whether a scene's camera should replace the current orbit: the first one
    /// of a connection does (that is the fit), later ones do not (the user's
    /// orbit wins).
    pub fn wants_first_fit(&mut self) -> bool {
        if self.framed {
            return false;
        }
        self.framed = true;
        true
    }

    /// Call the next scene a first one again, without fitting anything yet.
    ///
    /// An `open` is a **new document**: its scene has a camera of its own, so the
    /// fit of the previous document must not be kept. This is the armed half of
    /// that decision — it says "the next scene frames the view" — and a caller that
    /// already holds the scene fits it straight away with
    /// [`ClientModel::mark_framed`] instead. Both halves live here so the rule is
    /// one field (`framed`) and not a second flag beside it.
    pub fn rearm_first_fit(&mut self) {
        self.framed = false;
    }

    /// Say the view holds a real fit, so a later scene keeps it (the used half of
    /// [`ClientModel::rearm_first_fit`]).
    pub fn mark_framed(&mut self) {
        self.framed = true;
    }

    /// Set the aspect a following snapshot will be fitted at. The view calls this
    /// from the window's real size before the snapshot arrives.
    pub fn set_aspect(&mut self, aspect: f64) {
        if aspect.is_finite() && aspect > 0.0 {
            self.aspect = aspect;
        }
    }

    /// Take one notification: the protocol's version rule first, then whatever
    /// the notification means.
    pub fn apply(&mut self, n: &Notification) -> Effect {
        match n {
            Notification::Snapshot(snap) => {
                // **A preview frame does not move the version** (`docs/PROTOCOL.md`
                // v1.4: preview 不推进 scene version), so the discard rule cannot be
                // the only way in: a prefix frame and the final frame of the same
                // version are both new *pictures*, and a repeated version is
                // legitimate wherever the composition on screen changed.
                //
                // So a snapshot is stale when it is **older than the floor**, or when it
                // is at the floor and shows the composition already on screen. Nothing
                // else is dropped — and nothing at the floor raises it either, which is
                // the half that matters: a prefix that raised the floor to its own
                // version would make the next real mutation at that same version look
                // stale, and the view would freeze for good.
                //
                // The floor is an `Option`, not `unwrap_or(0)`: with no floor yet, a
                // snapshot at **version 0** is the first one, not a duplicate of
                // nothing (`hello` normally seeds a version, but a server need not).
                let stale = match self.state.max_version {
                    None => false,
                    Some(floor) => {
                        snap.version < floor || (snap.version == floor && snap.upto == self.preview)
                    }
                };
                if stale {
                    self.state.discarded += 1;
                    return Effect::Nothing;
                }
                self.state.note_version(snap.version);
                self.state.accepted += 1;
                self.version = Some(snap.version);
                self.last_reason = None;
                match MadeScene::from_snapshot(snap, self.aspect) {
                    Ok(made) => {
                        self.preview = snap.upto.clone();
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
                // `accept_state_change`, not `accept`: the server pushes the snapshot and
                // the reason at the **same** version (see that method).
                if !self.state.accept_state_change(*version) {
                    return Effect::Nothing;
                }
                self.state.last_reason = Some(reason.clone());
                self.last_reason = Some(reason.clone());
                if reason == "reload" {
                    self.reload_badge = true;
                }
                Effect::StateChanged {
                    version: *version,
                    reason: reason.clone(),
                }
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

    /// The status bar's Chinese summary: the CJK text the font work exists to
    /// draw, and the line the UI shows.
    ///
    /// `已连接 · v8 · 434 prim · 未选中` — connection, snapshot version, prim count,
    /// selection. The separator is a middle dot rather than `·`'s ASCII cousin so
    /// the line is visibly one script; every character here is CJK except the
    /// middots and `v8`, which is what makes it a real test of the font (a tofu
    /// box would be unmistakable next to the ASCII-only line below).
    pub fn status_line_zh(&self) -> String {
        let conn = if self.offline {
            "离线"
        } else if self.state.is_live() {
            "已连接"
        } else {
            // Before `subscribe` is granted the viewer is still connecting, whatever
            // `hello` said.
            "连接中"
        };
        let version = match self.version {
            Some(v) => format!("v{v}"),
            None => "无快照".to_owned(),
        };
        let prims = format!("{} prim", self.item_count);
        let sel = match &self.selection {
            Some(p) => format!("选中 {p}"),
            None => "未选中".to_owned(),
        };
        let mut line = format!("{conn} · {version} · {prims} · {sel}");
        if let Some(upto) = &self.preview {
            line.push_str(&format!(" · 预览 ≤ {upto}"));
        }
        if let Some(r) = &self.last_reason {
            line.push_str(&format!(" · {}", reason_zh(r)));
        }
        line
    }

    /// The composition on screen, as the status bar and the overlay say it: 「最终」 for
    /// the final composition and 「预览 ≤ <layer>」 for a prefix (v1.4).
    ///
    /// Both states are printed rather than only the prefix: a user who clicked an op row
    /// needs to see which prefix they are looking at, and a user who clicked 「final」
    /// needs to see that it took effect. `ClientModel::preview` is the one source (the
    /// server's echoed `upto`), so this cannot disagree with what is on screen.
    pub fn preview_label(&self) -> String {
        match &self.preview {
            Some(upto) => format!("预览 ≤ {upto}"),
            None => "最终".to_owned(),
        }
    }

    /// The status bar's one-line summary (ASCII; the logs and the tests read it).
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
        let preview = match &self.preview {
            Some(upto) => format!("preview<={upto}"),
            None => String::new(),
        };
        format!("{conn} · {version} · {prims} · {sel} {preview} {reason}")
            .replace("  ", " ")
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

/// The Chinese word for a `state_changed` reason.
fn reason_zh(reason: &str) -> &'static str {
    match reason {
        "open" => "已打开",
        "patch" => "已补丁",
        "opinion" => "已覆盖",
        "undo" => "已撤销",
        "redo" => "已重做",
        "reload" => "已热重载",
        // A reason this build does not name: 'update' keeps the line Chinese.
        _ => "已更新",
    }
}
