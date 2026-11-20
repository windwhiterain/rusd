//! The winit + egui shell: one window, one GPU device, one viewport.
//!
//! `rusd-viewer view --connect host:port [--usdc fallback]` starts here, and the
//! shape follows `docs/REMOTE.md`:
//!
//! - **The camera is this side's.** Orbit, pan and zoom move the local
//!   [`Orbit`] and ask the render thread for a frame; no message carries a
//!   camera and no message is sent because of one (§6).
//! - **Pick is this side's.** A click reads the frame's own prim-id buffer — the
//!   `R32Uint` pass `rusd_render` already draws — so selection costs no round
//!   trip (§4), and the outline is drawn by `rusd_render::annotate`.
//! - **`Stage` never leaves its thread.** A snapshot is composed where it
//!   arrived, extracted to owned geometry, and only plain data crosses to the
//!   render thread and the UI (§7).
//! - **The only message a drag sends is an `opinion`**, throttled to ≤ 30 Hz
//!   plus a final one at drag end, and it lands in the server's overrides
//!   session layer (§3, `docs/PROTOCOL.md`'s `opinion` row). The drag itself is
//!   drawn locally first, by re-baking one prim's transform, so the prim follows
//!   the pointer without waiting for the round trip.
//!
//! Threads: the UI thread draws and talks to two workers — the network thread
//! ([`crate::net::NetWorker`]) and the render thread
//! ([`crate::net::RenderWorker`]) — through channels, so neither a slow server
//! nor a slow frame can freeze the window.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Instant;

use egui_wgpu::ScreenDescriptor;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::anchor::Anchor;
use crate::camera::{Orbit, plane_hit};
use crate::net::{NetCommand, NetEvent, NetWorker, RenderCommand, RenderWorker};
use serde_json::json;

use crate::protocol::{self, Notification, OpinionOp};
use crate::session::{ClientModel, Drag, Effect, MadeScene};
use crate::startup::{self, Startup};
use crate::window::startup_window_attrs;

/// How the viewer was started: the words after `view`.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewArgs {
    /// `host:port` of the server (`127.0.0.1:8765` through an `ssh -L` tunnel,
    /// per `docs/PROTOCOL.md`).
    pub connect: String,
    /// A `.usdc` to show when the server never answers: the offline fallback.
    pub usdc: Option<String>,
    /// Draw the size/frame overlay in the viewport's top-left corner.
    ///
    /// The overlay reports the numbers that were guessed wrong three rounds
    /// running — `pixels_per_point`, the rect, the texel size, the camera's
    /// distance and target, and how many frames arrived against which one is on
    /// screen — so a screenshot answers "is the frame fresh, and is it 1:1?"
    /// without another probe.
    pub debug_ui: bool,
    /// The v1.7 control channel's address, when the caller asked for one.
    ///
    /// `None` is the default and means **no socket at all**: a viewer started
    /// without `--control` has no listening port to find, which is what makes the
    /// channel an opt-in rather than a surface every run exposes.
    pub control: Option<String>,
    /// The address the control channel actually bound, once [`crate::app::App::open_control`]
    /// has run: `--control 127.0.0.1:0` asks the OS for a port, so the request and the
    /// bound address are two different facts and only the second one can be connected to.
    ///
    /// Not a flag, so [`ViewArgs::parse`] never sets it.
    pub control_addr: Option<std::net::SocketAddr>,
}

impl ViewArgs {
    /// Parse `--connect <addr:port> [--usdc <path>]`, with `--flag=value` also
    /// accepted.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut connect = None;
        let mut usdc = None;
        let mut debug_ui = false;
        let mut control = None;
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let (flag, inline) = match a.split_once('=') {
                Some((f, v)) => (f, Some(v.to_owned())),
                None => (a.as_str(), None),
            };
            let mut take = || inline.clone().or_else(|| it.next().cloned());
            match flag {
                "--connect" => {
                    connect = take();
                    if connect.as_deref().is_none_or(str::is_empty) {
                        return Err("--connect needs an address (host:port)".to_owned());
                    }
                }
                "--usdc" => {
                    usdc = take();
                    if usdc.is_none() {
                        return Err("--usdc needs a path".to_owned());
                    }
                }
                "--control" => {
                    let addr = take();
                    let Some(addr) = addr.filter(|a| !a.is_empty()) else {
                        return Err("--control needs an address (127.0.0.1:port)".to_owned());
                    };
                    // The rule is checked here, before any socket exists: a control
                    // channel a network can reach is remote control of the user's
                    // desktop session, so a non-loopback address is a refusal at
                    // startup rather than a listener nobody notices.
                    if !crate::control::is_loopback_text(&addr) {
                        return Err(format!(
                            "--control {addr}: only a loopback address is allowed \
                             (127.0.0.1:port, [::1]:port or localhost:port)"
                        ));
                    }
                    control = Some(addr);
                }
                "--debug-ui" => debug_ui = true,
                other => return Err(format!("unexpected argument {other:?}")),
            }
        }
        let connect = connect.ok_or_else(|| "--connect <addr:port> is required".to_owned())?;
        Ok(Self {
            connect,
            usdc,
            debug_ui,
            control,
            control_addr: None,
        })
    }
}

/// Radians of orbit per point of drag.
const ORBIT_SENSITIVITY: f64 = 0.006;
/// A gizmo drag sends at most this often (`docs/PROTOCOL.md`: ≤ 30 Hz).
const GIZMO_HZ: f64 = 30.0;

/// The tags a `NetCommand::Call` is answered under, so [`Shell::on_reply`] knows
/// which of its own requests a reply belongs to.
/// The tag a `composition_tree` request is answered under: **one spelling**, in
/// `crate::view::TREE_TAG`, because the request itself is sent from there (the shell
/// and its tests share the one copy) and this `match` reads the same constant.
const TREE_TAG: &str = crate::view::TREE_TAG;
const NODE_TAG: &str = "comp_node";
const CAPTURE_TAG: &str = "capture";
/// `preview{upto}` (v1.4). Its answer is `{version}`, which the *notification* is the
/// authority for: the prefix itself arrives as a `stage_snapshot` carrying `upto`.
const PREVIEW_TAG: &str = "preview";
/// The layer table (`layer_files`) and one layer's sync (`SyncLayer`), v1.9.
const FILES_TAG: &str = "files";
const SYNC_TAG: &str = "sync";
/// `open{path}` (v1.7): the control channel's document open, answered under this tag so
/// the request's own reply is the one a script gets.
const OPEN_TAG: &str = "open";

/// What wakes the window thread.
#[derive(Debug, Clone, PartialEq)]
pub enum AppEvent {
    /// Network, frames, or a plain redraw request.
    Wake,
}

/// One pointer gesture in progress.
struct DragState {
    kind: Drag,
    /// Where the pointer was when the drag began (the gizmo's anchor).
    start: egui::Pos2,
    /// Where it was last tick (deltas are per tick).
    last: egui::Pos2,
}

/// What the control channel hands the shell once, at startup.
///
/// Two fields and no behaviour: the queue the listener fills (`Some` only when
/// `--control` was given) and the address it is bound to, which `state` reports so a
/// script that asked for port 0 can learn the port the OS chose.
#[derive(Debug, Default)]
pub struct ControlWiring {
    pub queue: Option<crate::control::Queue>,
    pub addr: Option<std::net::SocketAddr>,
}

/// Everything the window owns.
pub struct Shell {
    window: Arc<Window>,
    painter: egui_wgpu::winit::Painter,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,

    model: ClientModel,
    net: NetWorker,
    render: RenderWorker,
    orbit: Option<Orbit>,
    bounds: ([f64; 3], [f64; 3]),
    /// The last scene's composed stage, kept on this thread (`!Send`) for the
    /// gizmo's anchor reads. `None` before the first snapshot, or for a scene that
    /// carried none (the `.usdc` fallback still composes one, so in practice only
    /// an empty start).
    stage: Option<openusd::usd::Stage>,
    /// The last scene's geometry, here as well as on the render thread: the gizmo
    /// needs a prim's mesh to find its world centre, and the render thread's copy
    /// lives in a struct this thread cannot reach.
    geometry: rusd_scene::geometry::SceneGeometry,

    /// The frame on screen, with the texture it is drawn from.
    ///
    /// The texture is the **render thread's own GPU view** (`register_native_texture`),
    /// so showing a frame costs no copy (v1.10); a frame whose pixels had to be touched
    /// on the CPU (a selection's outline) carries its bytes and is uploaded instead, and
    /// `cpu_tex` keeps that handle alive.
    frame: Option<(crate::net::Frame, egui::TextureId)>,
    /// The egui texture registered over the render thread's beauty view, with the target
    /// generation it belongs to (a resize makes a new view, so a new registration).
    gpu_tex: Option<(egui::TextureId, u64)>,
    /// The upload of a frame that carried its own bytes (the annotated ones).
    cpu_tex: Option<egui::TextureHandle>,
    /// The render size the texture was made at, so a resize forces a new one.
    tex_size: (u32, u32),
    /// The viewport's geometry — the one place its rect, scale and texel size come
    /// from (`crate::viewport::ViewportSize`). Updated by the layout each frame;
    /// the render thread is told only when the texel size changes.
    viewport: crate::viewport::ViewportSize,
    /// The size the **presenter's** swapchain is configured at, in physical pixels.
    ///
    /// This is a second size on purpose, and it is not the render thread's: the
    /// render thread draws the scene into a texture of the *viewport rect's* texels,
    /// while `egui_wgpu::winit::Painter` owns the window's own back buffer. The
    /// painter configures that back buffer only in `set_window` and
    /// `on_window_resized`, and this shell was calling neither after startup — so
    /// the swapchain kept the size the window had at 1280×720 logical, and every
    /// later window size was presented by having the compositor **stretch** it
    /// (`DXGI_SCALING_STRETCH`). On a 1.75 scale that is exactly the class of defect
    /// this round was sent to fix: the whole UI drawn at one size and magnified to
    /// another, panels included.
    presented: (u32, u32),
    /// The three numbers that must agree: inner physical, scale, and the point size
    /// the pass was handed. Measured at the start of each pass, before the layout
    /// runs, so the overlay prints the platform's own numbers rather than the
    /// layout's result — the two agreeing is the whole point, and one number alone
    /// cannot show it.
    ///
    /// Its `presented_physical` is the swapchain size from **before** this pass's
    /// [`Shell::sync_presenter`] — the defect the pass inherited, not the size it
    /// installed. That is what makes a screenshot of the repairing frame show what
    /// was wrong, instead of always reading `matches yes` by construction.
    screen: crate::viewport::ScreenSpace,
    /// The bars' sizes from **this** pass: `(left, top, bottom)` in points.
    ///
    /// The layout is what knows them, and the overlay draws **inside** the layout, so
    /// they cannot be re-measured where the overlay runs. The pass records them into a
    /// local as it lays each panel out and hands them over just before the overlay
    /// asks, so the three numbers a screenshot shows are the ones on that screen.
    bars: (f32, f32, f32),
    /// A frame counter for the debug overlay: how many frames the render thread has
    /// handed over, and which one is on screen. A stale window shows a smaller
    /// `shown` than `received`, which is what makes "displaying an old frame"
    /// visible in a screenshot instead of a guess.
    frames_received: u64,
    /// Whether `--debug-ui` draws the size/frame overlay.
    debug_ui: bool,
    /// Whether the user has collapsed the overlay to one row (v1.4's overlay compact:
    /// 可折叠，键 `o` 或按钮). Only meaningful with `debug_ui`; the key and the button
    /// flip it either way, and `plan` turns it into a one-row box.
    overlay_collapsed: bool,

    drag: Option<DragState>,
    /// The prim a gizmo drag is moving, with the anchor it started from (world
    /// centre, authored local translate, parent world matrix). `None` outside a
    /// gizmo drag, or when the selection has no anchor in this scene.
    gizmo: Option<(String, Anchor)>,
    /// The drag's newest unsent local translation: flushed on the throttle or at
    /// the end.
    pending: Option<[f64; 3]>,
    throttle: protocol::Throttle,
    /// The last local translation this client actually sent, so a drag that stops
    /// moving sends nothing.
    sent: Option<[f64; 3]>,
    /// The local translation the dragged prim is currently drawn at, so an
    /// unchanged drag re-bakes nothing.
    preview: Option<[f64; 3]>,
    /// A named path to show alone once the layer that authors it has finished opening
    /// (v1.13): set by a hierarchy row's click, applied by `poll_local_open`.
    pending_render: Option<String>,

    status: String,
    fps: f64,
    last_frame_at: Instant,
    offline_usdc: Option<String>,
    /// `layer id → the file on the server` (v1.9): what a click composes **locally**, from
    /// the copy the cache already holds. Refreshed with the tree, so a re-cook shows up as a
    /// new file name without the click ever asking the server for geometry.
    layer_files: std::collections::BTreeMap<String, String>,
    /// A local open in flight (v1.9): the geometry comes back from a **worker thread**, so a
    /// 2400-prim product no longer freezes the window and the control channel while it is
    /// opened. `Stage` itself is `!Send`, so the worker hands over the geometry alone.
    opening: Option<(String, std::sync::mpsc::Receiver<Result<rusd_scene::SceneGeometry, String>>)>,
    /// The layer a click asked to see while its local copy was **not yet on disk**. The
    /// click sends a `SyncLayer` for it; when that sync lands, this is the layer to open
    /// again, so the first click after an `open` is not a click that only syncs and shows
    /// nothing (`点了节点不显示` on a fresh document). One slot: a second click replaces it.
    want_open: Option<String>,
    /// A click whose layer id a rebuild replaced: the row it was (by position among the
    /// preview rows) and the ask it wanted, so the tree that follows re-sends it with the
    /// id the new stack minted (v1.9).
    pending_preview: Option<(usize, String)>,
    /// The `host:port` this viewer was told to connect to (`--connect`), kept for the
    /// control channel's `state`: the address the viewer is *aimed* at, which is what a
    /// script needs to tell "the server is not up" from "this viewer is not aimed at it".
    connect: String,
    /// Where the control channel is listening, when `--control` opened one. Reported by
    /// `state` so a script that started the viewer with `--control 127.0.0.1:0` can learn
    /// the port the OS chose (`crate::control::Control::addr`).
    control_addr: Option<std::net::SocketAddr>,

    /// The composition-tree panel (`docs/PROTOCOL.md` v1.2/v1.3).
    tree: crate::panel::TreePanel,
    /// Whether the composition-tree panel is expanded. Collapsing it gives the width
    /// back to the viewport on the next pass — the same path a drag-resize takes.
    tree_open: bool,
    /// Whether `composition_tree{}` has been asked for this connection: asked once
    /// when the first snapshot arrives, and again on 「刷新」.
    tree_asked: bool,

    /// Whether the capture toolbar's selection mode is armed.
    capture_armed: bool,
    /// Whether 「只看选中」 is on (the user's isolation ruling): the viewport then draws
    /// only the selected prim's subtree, and nothing when nothing is selected.
    ///
    /// A **display** flag on this side alone. The wire, the composite and the scene
    /// version are untouched by it -- it is the same geometry, filtered at bake time
    /// (`crate::scene::isolate`), which is what keeps it a client-only lens.
    only_selected: bool,
    /// Where the capture drag started, in viewport points.
    capture_start: Option<egui::Pos2>,
    /// The optional note that goes into the next capture.
    capture_note: String,
    /// The last toast (a copied id or path) and when it was raised.
    toast: Option<(String, Instant)>,
    /// The clipboard payload waiting to be copied: egui copies text through the
    /// platform output of a pass, so a copy asked for in this pass is handed to
    /// `ctx.copy_text` at the **end** of it (`Shell::flush_copy`).
    pending_copy: Option<String>,

    /// The v1.7 control channel's queue, when `--control` opened one
    /// (`crate::control`). `None` is every run without the flag: nothing was ever
    /// bound, so nothing can reach this window.
    ///
    /// **Only the receiving end lives here.** The listener and one thread per
    /// connection own the sockets; a request arrives as a value plus where to write
    /// its answer, and every method below is a call to the function the widget calls.
    control: Option<crate::control::Queue>,
    /// Control requests waiting for a **deferred** answer: a method whose result is
    /// the server's reply to a request this shell sent (`open`, `preview`, and the
    /// `comp_node` half of `tree`/`select`).
    ///
    /// A short FIFO rather than one slot per method, because two connections may ask
    /// at once and the answers come back in the order the requests went out: matching
    /// by tag in send order is what keeps each caller's `id` with its own result.
    waiting: Vec<Waiting>,
}

/// One control request that cannot be answered until a reply comes back.
///
/// `awaiting` names the **reply tags** still owed — a row's click can owe two (a layer
/// row asks its `comp_node` detail *and* a preview), which is why this is a list and
/// not a flag. `results` holds what has arrived, keyed by those tags, and `shape` says
/// how to spell the answer: some methods pass the server's reply through, some wrap it
/// in what the click did.
struct Waiting {
    reply: crate::control::Replier,
    awaiting: Vec<&'static str>,
    results: BTreeMap<&'static str, Result<serde_json::Value, (i64, String)>>,
    shape: Shape,
}

impl Waiting {
    /// A waiter that will be answered when the tags in `owed` have all come back.
    ///
    /// `owed` is what the click this waiter stands for is about to send — one tag for a
    /// row's detail, two for a layer row that also asks for a composition. It is written
    /// here rather than discovered from the replies, because a request that is never sent
    /// (nothing to ask) must not leave a waiter that nothing will ever fill.
    fn with(reply: crate::control::Replier, shape: Shape, owed: Vec<&'static str>) -> Self {
        Self {
            reply,
            awaiting: owed,
            results: BTreeMap::new(),
            shape,
        }
    }
}

/// How a control request's answer is spelled once its replies are in.
#[derive(Clone, Debug, PartialEq)]
enum Shape {
    /// `open` and `preview`: the server's own reply, as it came (`preview`'s is
    /// `{version}` — the prefix itself arrives as a `stage_snapshot`, so the reply says
    /// the request was accepted and never claims a frame).
    Server,
    /// `tree{id}`: what the click was, plus the detail it gathered.
    Row {
        id: String,
        kind: String,
        action: &'static str,
    },
    /// `select{path}`: the path, the selection, and the `comp_node` node it resolved to.
    Pick { path: String },
}

impl Shape {
    /// The answer this shape spells from what the replies said.
    fn answer(
        &self,
        results: &BTreeMap<&'static str, Result<serde_json::Value, (i64, String)>>,
    ) -> Result<serde_json::Value, (i64, String)> {
        // A refusal from the server is the answer: `-32602` for an unknown layer id is
        // exactly what a script has to see, not something this side rewrites.
        for (tag, r) in results {
            if let Err((code, message)) = r {
                return Err((*code, format!("{tag}: {message}")));
            }
        }
        match self {
            Shape::Server => Ok(results
                .values()
                .next()
                .and_then(|r| r.as_ref().ok())
                .cloned()
                .unwrap_or(serde_json::Value::Null)),
            Shape::Row { id, kind, action } => Ok(json!({
                "id": id,
                "kind": kind,
                "action": action,
                "detail": lookup(results, NODE_TAG),
                "preview": lookup(results, PREVIEW_TAG),
            })),
            Shape::Pick { path } => Ok(json!({
                "path": path,
                "selected": true,
                "node": lookup(results, NODE_TAG),
            })),
        }
    }
}

/// What one tag's reply said, or `null` when that tag was not owed.
fn lookup(
    results: &BTreeMap<&'static str, Result<serde_json::Value, (i64, String)>>,
    tag: &str,
) -> serde_json::Value {
    results
        .get(tag)
        .and_then(|r| r.as_ref().ok())
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

/// An orbit as the control channel reports it.
fn orbit_json(o: Orbit) -> serde_json::Value {
    json!({
        "yaw": o.yaw,
        "pitch": o.pitch,
        "dist": o.distance,
        "target": [o.target[0], o.target[1], o.target[2]],
    })
}

/// The word a row's click is reported as (`TreeAction` in the caller's terms).
fn action_word(action: &crate::panel::TreeAction) -> &'static str {
    match action {
        crate::panel::TreeAction::Select(_) => "select",
        crate::panel::TreeAction::Render(_) => "render",
        crate::panel::TreeAction::Layer { ask: Some(ask), .. } => ask.as_str(),
        crate::panel::TreeAction::Layer { ask: None, .. } => "layer-detail",
        crate::panel::TreeAction::PreviewSolo(_) => "preview-solo",
        crate::panel::TreeAction::Preview(_) => "preview",
        crate::panel::TreeAction::Refresh => "refresh",
        crate::panel::TreeAction::None => "none",
    }
}

impl Shell {
    /// Build the GPU stack and the two worker threads on an existing window.
    ///
    /// `startup` is the milestone recorder the caller already has: the same one
    /// that marked `window_created` marks the steps inside here, so a startup that
    /// stalls says so on the file (`RUSD_VIEWER_STARTUP_LOG`) or in its marks.
    fn new(
        window: Arc<Window>,
        args: &ViewArgs,
        proxy: EventLoopProxy<AppEvent>,
        startup: &mut Startup,
        control: ControlWiring,
    ) -> Result<Self, String> {
        let egui_ctx = egui::Context::default();
        // Before the first frame: the status bar's Chinese labels draw as tofu
        // unless a CJK face is in the atlas (see `crate::fonts`).
        let font = crate::fonts::install(&egui_ctx);
        match &font {
            Some(f) => eprintln!(
                "{{\"code\":\"N_FONT\",\"message\":\"{} bytes from {}\"}}",
                f.bytes,
                f.path.display()
            ),
            None => eprintln!(
                "{{\"code\":\"W_FONT\",\"message\":\"no CJK font found; UI text will be tofu\"}}"
            ),
        }
        let size = window.inner_size();
        let mut painter = pollster::block_on(egui_wgpu::winit::Painter::new(
            egui_ctx.clone(),
            egui_wgpu::WgpuConfiguration::default(),
            false,
            egui_wgpu::RendererOptions::default(),
        ));
        startup.mark(startup::PAINTER_READY);
        pollster::block_on(painter.set_window(egui::ViewportId::ROOT, Some(window.clone())))
            .map_err(|e| format!("GPU surface: {e}"))?;
        startup.mark(startup::SURFACE_READY);
        let render_state = painter
            .render_state()
            .ok_or_else(|| "no GPU render state after set_window".to_owned())?;
        let max_side = painter.max_texture_side();
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            None,
            max_side,
        );

        // `Window::inner_size` is in **physical pixels**, which is exactly what the
        // render thread's viewport must be: the frame is later painted into a
        // point-space rect, and one frame texel must land on one device pixel
        // (`crate::viewport::render_texel_size`, `texel_scale`). A viewport in
        // points here would be resampled by the sampler at any `pixels_per_point`
        // above 1 — the picture magnified out of its top-left corner.
        let render = RenderWorker::spawn(
            render_state.device.clone(),
            render_state.queue.clone(),
            size.width.max(1),
            size.height.max(1),
            {
                let proxy = proxy.clone();
                Box::new(move || {
                    let _ = proxy.send_event(AppEvent::Wake);
                })
            },
        );
        startup.mark(startup::RENDER_WORKER);
        let net = NetWorker::spawn(
            args.connect.clone(),
            Box::new(move || {
                let _ = proxy.send_event(AppEvent::Wake);
            }),
        );
        // **Startup does not wait for the greeting.** `NetWorker::spawn` returns as
        // soon as the thread exists; `hello`/`subscribe` and the first snapshot
        // happen on it. This mark therefore means "the socket is being worked on",
        // not "the server answered" — which is the whole point: a viewer whose
        // window waits for a snapshot is a viewer that shows nothing when the
        // server is quiet.
        startup.mark(startup::NET_WORKER);
        startup.mark(startup::SHELL_BUILT);

        let scale = window.scale_factor() as f32;
        let window_points = egui::vec2(size.width as f32, size.height as f32) / scale;

        Ok(Self {
            window,
            painter,
            egui_ctx,
            egui_state,
            model: ClientModel::new(),
            net,
            render,
            orbit: None,
            bounds: ([0.0; 3], [0.0; 3]),
            stage: None,
            geometry: rusd_scene::geometry::SceneGeometry {
                items: Vec::new(),
                camera: None,
                bounds: ([0.0; 3], [0.0; 3]),
                diags: Vec::new(),
            },
            frame: None,
            gpu_tex: None,
            cpu_tex: None,
            // A placeholder until the first layout: the whole window as the rect.
            // It is replaced by `publish_viewport` before anything is drawn, and the
            // render thread's first `Resize` comes from there.
            viewport: crate::viewport::whole_window(window_points, scale),
            // What the presenter's swapchain was configured at: `set_window` above
            // used exactly this size, and every `Resized` since updates both.
            presented: (size.width, size.height),
            screen: crate::viewport::ScreenSpace {
                inner_physical: (size.width, size.height),
                scale_factor: scale,
                screen_points: window_points,
                presented_physical: (size.width, size.height),
                os: None,
            },
            bars: (0.0, 0.0, 0.0),
            frames_received: 0,
            tex_size: (0, 0),
            drag: None,
            gizmo: None,
            pending: None,
            throttle: protocol::Throttle::hz(GIZMO_HZ),
            sent: None,
            preview: None,
            pending_render: None,
            status: format!("connecting to {}", args.connect),
            fps: 0.0,
            last_frame_at: Instant::now(),
            offline_usdc: args.usdc.clone(),
            layer_files: std::collections::BTreeMap::new(),
            opening: None,
            want_open: None,
            pending_preview: None,
            connect: args.connect.clone(),
            // The socket, when one was asked for, is opened by `App` before the window
            // exists (`--control`), and its address is handed in here: nothing about the
            // window depends on it, and a viewer without the flag never opens one.
            control: control.queue,
            control_addr: control.addr,
            waiting: Vec::new(),
            debug_ui: args.debug_ui,
            // **Expanded by default**, because the numbers are what the overlay exists
            // for, and the area bound already keeps it to a quarter of the viewport.
            // Collapsing is the extra affordance (`o`, or the bar's button) for when
            // even that is in the way; `--debug-ui` no longer needs it to be usable.
            overlay_collapsed: false,
            tree: crate::panel::TreePanel::new(),
            tree_open: true,
            tree_asked: false,
            capture_armed: false,
            only_selected: false,
            capture_start: None,
            capture_note: String::new(),
            toast: None,
            pending_copy: None,
        })
    }

    // --- server → view ----------------------------------------------------

    /// Every notification the network thread has, applied in order.
    fn drain_net(&mut self) {
        for e in self.net.drain() {
            match e {
                NetEvent::Notification(n) => self.on_notification(n),
                NetEvent::Reconnected { addr } => {
                    // The wire is back (v1.9). The **server** owns the document, so the
                    // viewer needs to know nothing about it: `subscribe` was redone by
                    // the worker, and the server answers a new subscriber with the
                    // current scene — the snapshot arrives on its own, and the tree is
                    // asked for again. If the server itself was restarted it has no
                    // scene, and it says exactly that; re-opening the document is then
                    // the user's act, not a path this window remembered.
                    self.model.mark_live();
                    self.status = format!("reconnected to {addr}");
                    self.ask_tree();
                    self.request_repaint();
                }
                NetEvent::Live { addr: _ } => {
                    // `hello` + `subscribe` succeeded: the connection is what the
                    // status bar must now say. Without this the view kept
                    // claiming "connecting" while snapshots were arriving.
                    self.model.mark_live();
                    self.status = self
                        .model
                        .status_line_zh(self.preview_kind(), |id| self.layer_name(id));
                    self.ask_tree();
                    self.request_repaint();
                }
                NetEvent::Applied { what, version } => {
                    self.status = match version {
                        Some(v) => format!("{what} accepted → server v{v}"),
                        None => format!("{what} accepted"),
                    };
                    self.request_repaint();
                }
                NetEvent::Failed(diags) => {
                    self.status = join_diags(&diags);
                    self.request_repaint();
                }
                NetEvent::Reply { tag, result } => self.on_reply(tag, result),
                NetEvent::Closed(diags) => {
                    self.status = join_diags(&diags);
                    self.model.mark_closed();
                    // A control request whose answer was this connection's (`open`,
                    // `preview`, a row's detail) can never be answered now: say so
                    // rather than leave a script on a socket until its own timeout.
                    self.fail_control_waiters(
                        "the server connection ended before this 
                                                 request was answered",
                    );
                    // Nothing will arrive: fall back to the file the user named,
                    // if any, and say so rather than show an empty window.
                    if self.model.version.is_none()
                        && let Some(path) = self.offline_usdc.clone()
                    {
                        self.load_offline(&path);
                    }
                    self.request_repaint();
                }
            }
        }
    }

    fn on_notification(&mut self, n: Notification) {
        match self.model.apply(&n) {
            Effect::NewScene(made) => self.adopt_scene(*made),
            Effect::StateChanged { version, reason } => {
                self.on_state_changed(version, &reason);
            }
            Effect::Status => {
                self.status = self
                    .model
                    .status_line_zh(self.preview_kind(), |id| self.layer_name(id));
                self.request_repaint();
            }
            Effect::Nothing => {}
            Effect::Failed(diags) => {
                self.status = join_diags(&diags);
                self.request_repaint();
            }
        }
    }

    /// A `state_changed`: the reason decides what has to follow it.
    ///
    /// Two things can be stale when the server re-composes, and each reason says
    /// whether it is (the rules are `crate::view`'s, so the window and its tests run
    /// one copy of them):
    ///
    /// - **the composition tree** — asked again for every reason that can move the
    ///   layer stack (`crate::view::asks_tree`), because a tree read once per
    ///   connection showed the previous document's layers after an `open` and only
    ///   「刷新」 fixed it;
    /// - **the camera** — refitted for `open` alone (`crate::view::camera_for_state_change`),
    ///   whose scene is the first frame of a new document. The contract orders
    ///   `stage_snapshot` *before* `state_changed`, so the new document's scene is
    ///   already on screen (adopted as a later scene, which keeps the orbit); the fit
    ///   is taken here from the bounds now in hand, and the render thread is told.
    ///
    /// A `preview` never gets here: it is not a mutation, it advances no version, so
    /// it raises no `state_changed` at all (v1.4), and neither rule runs.
    fn on_state_changed(&mut self, version: u64, reason: &str) {
        let aspect = self.viewport_aspect();
        let plan = crate::view::apply_state_change(
            &mut self.model,
            &mut self.tree,
            &self.net,
            version,
            reason,
            aspect,
        );
        if let Some(orbit) = plan.refit {
            self.orbit = Some(orbit);
            self.render.send(RenderCommand::View {
                orbit,
                selection: self.model.selection.clone(),
                only_selected: self.only_selected,
            });
        }
        if plan.asked_tree {
            // The rows are on screen until the answer replaces them (`view::ask_tree`).
            self.tree_asked = true;
        }
        self.status = self
            .model
            .status_line_zh(self.preview_kind(), |id| self.layer_name(id));
        self.request_repaint();
    }

    /// A new scene: the **first one of a connection** frames the camera to this
    /// window's aspect (`ClientModel::wants_first_fit`), later ones keep the user's
    /// orbit; the render thread receives the geometry alone (the stage stays here,
    /// `!Send`), and a drag's local preview is dropped (the server's answer replaces
    /// it).
    ///
    /// **A prefix frame is a scene like any other**: it takes the same path, so the
    /// geometry on screen and the camera rules are the same for a `preview{upto}` as
    /// for a mutation — except that no later frame re-fits, which is exactly what a
    /// prefix needs (it is the same scene seen earlier, and the user was looking at it).
    fn adopt_scene(&mut self, made: MadeScene) {
        self.bounds = made.bounds;
        // One function decides whether this scene frames the view or keeps the
        // user's camera (`crate::view::camera_for_scene`), so the window and the
        // wiring tests cannot disagree about it. The bug this replaces: the
        // camera was only taken when `orbit` was `None`, and by the first snapshot
        // it never was.
        let aspect = self.viewport_aspect();
        if let Some(orbit) = crate::view::camera_for_scene(&mut self.model, &made, aspect) {
            self.orbit = Some(orbit);
        }
        self.model.adopt(&made);
        // The composition on screen, echoed to the panel so its op row can be marked
        // and its 「final」 button reads the state the viewport is actually in (v1.4).
        self.tree.preview = self.model.preview.clone();
        self.status = self
            .model
            .status_line_zh(self.preview_kind(), |id| self.layer_name(id));
        self.preview = None;
        self.gizmo = None;
        // The first scene of a session is when `composition_tree{}` becomes
        // answerable (the server needs an open stage). Asked once here; a
        // `state_changed` asks again when its reason can move the layers
        // (`Shell::on_state_changed`), and 「刷新」 asks on demand.
        if !self.tree_asked {
            self.ask_tree();
        }
        // The stage is kept for the gizmo's anchor reads and never leaves this
        // thread; the render thread gets the geometry.
        self.stage = made.stage;
        self.geometry = made.geometry.clone();
        self.render.send(RenderCommand::Scene {
            geometry: Box::new(made.geometry),
            bounds: made.bounds,
        });
        // The scene's own camera is now the fit, so the render thread must draw
        // with it on this command rather than waiting for a `View` that may never
        // come (an idle client sends none, which is what starved the first frame).
        if let Some(orbit) = self.orbit {
            self.render.send(RenderCommand::View {
                orbit,
                selection: self.model.selection.clone(),
                only_selected: self.only_selected,
            });
        }
        self.request_repaint();
    }

    /// The selected prim's drag anchor: its world centre, its authored local
    /// `xformOp:translate`, and the parent matrix a world delta is undone by.
    ///
    /// Read from the retained stage at full precision (`crate::anchor`), falling
    /// back to a rootless anchor when the scene has none (the `.usdc` fallback).
    fn anchor_for(&self, path: &str) -> Option<Anchor> {
        match &self.stage {
            Some(stage) => crate::anchor::anchor_of_stage(stage, &self.geometry, path),
            None => crate::anchor::fallback_anchor(&self.geometry, path),
        }
    }

    /// No server: show the `.usdc` the user named so the window is not empty.
    fn load_offline(&mut self, path: &str) {
        let aspect = self.viewport_aspect();
        // A fallback scene is not a server's: the status bar must say 离线 rather
        // than 连接中.
        self.model.offline = true;
        match MadeScene::from_usdc_file(path, aspect) {
            Ok(made) => {
                let prims = made.item_count();
                self.adopt_scene(made);
                self.status = format!("offline {path} ({prims} prims) — no server");
            }
            Err(diags) => self.status = join_diags(&diags),
        }
        self.window.request_redraw();
    }

    // --- the composition tree and the capture (D13) -----------------------

    /// Ask `composition_tree{}` for the authority stack's layers.
    ///
    /// Asked once when the first scene arrives (the server needs an open stage, so
    /// asking earlier only earns a "no scene open"), again on every `state_changed`
    /// whose reason can move the layers (`crate::view::asks_tree` — an `open` is a
    /// different document entirely, and a patch or an opinion can add or drop a cook
    /// op's output layer), and on 「刷新」. Not once per snapshot: an identical layer
    /// stack replaced under the same rows costs the pending selection and the open
    /// detail panel, which is a visible loss for no gain. `tree_asked` therefore only
    /// records that the first ask happened; it no longer gates the later ones, since
    /// a tree that stops following the server is the defect this fixes.
    fn ask_tree(&mut self) {
        if self.model.version.is_none() {
            return;
        }
        self.tree_asked = true;
        crate::view::ask_tree(&self.net, &mut self.tree);
    }

    /// Ask `comp_node{id}` for a clicked row.
    fn ask_node(&mut self, id: &str) {
        self.net.send(NetCommand::Call {
            tag: NODE_TAG.to_owned(),
            method: "comp_node".to_owned(),
            params: crate::tree::comp_node_params(id),
        });
    }

    /// Ask `preview{upto}` for a **preview control row's** click -- an op row (v1.4)
    /// or the root layer's row (v1.5.3) -- or `preview{null}` for the
    /// 「final」 control.
    ///
    /// The scene the server answers with arrives as an ordinary `stage_snapshot` whose
    /// `upto` says which prefix it is; this request only *asks*, so nothing here changes
    /// `model.preview`. That field follows the frame (`ClientModel::apply`), which is
    /// what keeps a refused `upto` (`-32602`) from leaving the status bar claiming a
    /// prefix the viewport never showed.
    ///
    /// The camera does **not** re-fit: a prefix is the same scene, smaller, and the
    /// user who asked for it was looking at it. `ClientModel::wants_first_fit` already
    /// says one fit per connection, so this falls out of `adopt_scene` rather than
    /// needing a rule of its own.
    fn ask_preview(&mut self, upto: Option<String>) {
        let label = match &upto {
            Some(u) => format!("preview ≤ {u}"),
            None => "final".to_owned(),
        };
        self.status = format!("{label}: 请求中…");
        self.net.send(NetCommand::Call {
            tag: PREVIEW_TAG.to_owned(),
            method: "preview".to_owned(),
            params: protocol::preview_params(upto.as_deref()),
        });
    }

    /// Ask `preview{solo: <layer>}` for a **material layer row's** click (v1.8): that
    /// layer's own content and nothing else, every other layer muted (the session layer
    /// excepted, so an override stays visible).
    ///
    /// The other composition a layer row can ask for, and the reason a row names which
    /// one it wants ([`crate::panel::LayerAsk`]): a reference or a plain sublayer is not a
    /// stop in the cook order, so `preview{upto}` has nothing to say about it.
    ///
    /// Everything after the request is the same as [`Shell::ask_preview`]'s: the scene
    /// arrives as an ordinary `stage_snapshot` whose `upto` names this layer, the version
    /// does not move, `model.preview` follows the frame (never the request) — which is what
    /// keeps a refused id (`-32602`) from leaving the bar claiming a composition the
    /// viewport never showed.
    fn ask_solo(&mut self, layer: String) {
        self.status = format!("solo {layer}: 请求中…");
        self.net.send(NetCommand::Call {
            tag: PREVIEW_TAG.to_owned(),
            method: "preview".to_owned(),
            params: protocol::preview_solo_params(&layer),
        });
    }

    /// A `NetCommand::Call` was answered: route it by its tag.
    ///
    /// **The control channel's waiters see the reply first** (`note_control_result`): a
    /// request that asked for this tag gets its answer before the GUI's own handling runs,
    /// and both see the same value — the reply is not consumed by either.
    fn on_reply(&mut self, tag: String, result: Result<serde_json::Value, protocol::RpcError>) {
        let reply = match result {
            Ok(v) => Ok(v),
            Err(e) => Err((e.code, e.message)),
        };
        self.note_control_result(&tag, &reply);
        match tag.as_str() {
            TREE_TAG => {
                self.tree.apply_tree(reply);
                self.status = self.tree.model.status_zh();
                // **The layer table follows the tree** (v1.9): a re-cook mints new product
                // files, so the table is re-read with every tree and the files whose blocks
                // moved are synced. A click then costs nothing — it composes from the cache.
                self.net.send(NetCommand::Call {
                    tag: FILES_TAG.to_owned(),
                    method: "layer_files".to_owned(),
                    params: serde_json::json!({}),
                });
                // A click whose layer id died with a rebuild is re-asked here, for the row in
                // the same position (v1.9): the ids move, the rows do not, so the picture
                // follows the click instead of waiting for a second one.
                if let Some((index, ask)) = self.pending_preview.take() {
                    let rows = self.preview_rows();
                    if let Some(node) = rows.get(index) {
                        let id = node.id.clone();
                        if ask == "solo" {
                            self.ask_solo(id);
                        } else {
                            self.ask_preview(Some(id));
                        }
                    }
                }
                // A reason that arrived while this read was in flight was coalesced, not
                // queued: one more read answers all of them (a gizmo drag sends opinions
                // at ≤ 30 Hz, and each one is a reason).
                if self.tree.take_reread() {
                    crate::view::ask_tree(&self.net, &mut self.tree);
                }
            }
            NODE_TAG => self.tree.apply_node(reply),
            PREVIEW_TAG => match reply {
                Ok(value) => {
                    // The prefix itself arrives as a `stage_snapshot` whose `upto` is the
                    // authority (see `ask_preview`); the reply only says the request was
                    // accepted, so this line reports it without claiming a frame.
                    let version = value
                        .get("version")
                        .and_then(serde_json::Value::as_u64)
                        .map_or_else(
                            || "accepted".to_owned(),
                            |v| format!("accepted → server v{v}"),
                        );
                    self.status = format!("preview {version}");
                }
                Err((crate::protocol::TIMEOUT_CODE, _)) => {
                    self.status = "preview: 服务器未在时限内应答".to_owned();
                }
                Err((crate::tree::METHOD_NOT_FOUND, _)) => {
                    // A server from before v1.4: the op rows it sent (it sent none) are
                    // the only preview controls, and the button says why it did nothing.
                    self.status = "preview：服务器无此方法（server 版本）".to_owned();
                }
                Err((code, message)) => {
                    // `-32602` for an unknown id (the panel keeps the previous frame, and
                    // `model.preview` still names it — which is why the field follows the
                    // snapshot rather than the request), or a `solo` a server from before
                    // v1.8 does not have. The second reads as a version note, because a
                    // material row's click is the only thing that can send it.
                    //
                    // **A stop the server no longer has falls back to the whole picture**
                    // (v1.9): a rebuild mints new layer ids, so an id the viewer still holds
                    // stops naming a layer — and a click or a reconnect then left the bar
                    // reporting a refusal while the viewport sat on the last frame.
                    if message.contains("not an opened layer") {
                        // The row's id is from before a cook (v1.9): re-read the tree and, once
                        // it lands, ask again for the row in the same position — the ids move,
                        // the rows do not.
                        self.status = "这一行的层已重建，正在取新的…".to_owned();
                        self.ask_tree();
                    } else if message.contains("solo") {
                        self.status = format!(
                            "preview{{solo}}：服务器无此参数（server 版本）——{message} \
                             (code {code})"
                        );
                    } else {
                        self.status = format!("preview failed: {message} (code {code})");
                    }
                }
            },
            CAPTURE_TAG => match reply {
                Ok(value) => match value.get("id").and_then(|v| v.as_str()) {
                    Some(id) => {
                        self.copy_toast(id.to_owned());
                        self.status = format!("capture {id}");
                    }
                    None => {
                        self.status = format!("capture: the reply has no id: {value}");
                    }
                },
                Err((code, message)) => {
                    self.status = format!("capture failed: {message} (code {code})");
                }
            },
            FILES_TAG => {
                // `layer_files` answered: remember `id → file`, and sync the ones whose blocks
                // the cache lacks. The manifest is a few hundred bytes per layer; the blocks
                // that travel are the ones that actually moved.
                self.layer_files.clear();
                if let Ok(value) = reply {
                    for entry in value
                        .get("layers")
                        .and_then(|l| l.as_array())
                        .into_iter()
                        .flatten()
                    {
                        let (Some(id), Some(file)) = (
                            entry.get("id").and_then(|v| v.as_str()),
                            entry.get("file").and_then(|v| v.as_str()),
                        ) else {
                            continue;
                        };
                        self.layer_files.insert(id.to_owned(), file.to_owned());
                        self.net.send(NetCommand::SyncLayer {
                            tag: SYNC_TAG.to_owned(),
                            path: file.to_owned(),
                        });
                    }
                }
            }
            SYNC_TAG => match reply {
                Ok(value) => {
                    self.status = format!(
                        "已同步 {} 块（本次取 {}）→ {}",
                        value.get("blocks").and_then(|v| v.as_u64()).unwrap_or(0),
                        value.get("fetched").and_then(|v| v.as_u64()).unwrap_or(0),
                        value
                            .get("local")
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                    );
                    // **A click waiting on this sync is answered now** (v1.9): it asked for a
                    // layer whose local copy was not written yet, and a sync alone shows
                    // nothing. Re-tried only when the file is there, so a sync that could
                    // not produce it does not loop.
                    if let Some(id) = self.want_open.take() {
                        let there = self
                            .layer_files
                            .get(&id)
                            .map(|remote| {
                                crate::cache::LayerCache::new(
                                    crate::cache::LayerCache::default_dir(),
                                )
                                .local_path(remote)
                                .is_file()
                            })
                            .unwrap_or(false);
                        if there {
                            self.show_layer_local(&id);
                        }
                    }
                }
                Err((code, message)) => {
                    self.want_open = None;
                    self.status = format!("同步失败：{message} (code {code})");
                }
            },
            OPEN_TAG => match reply {
                // The status line is the one thing a lost waiter could leave behind (v1.9):
                // the open whose reply this is may have had its waiter dropped when the
                // connection died, and then the line sat on 「请求中…」 while the scene was
                // already there. The answer says where the viewer is now, not where the
                // request was.
                Ok(_) => self.status = self.tree.model.status_zh(),
                Err((code, message)) => {
                    self.status = format!("open failed: {message} (code {code})")
                }
            },
            other => self.status = format!("a reply with an unknown tag {other:?}"),
        }
        self.request_repaint();
    }

    /// Put `text` on the clipboard and raise the 「已拷贝」 toast.
    ///
    /// The copy itself is deferred to the end of the pass (`flush_copy`): egui
    /// hands clipboard writes to the platform through the pass's output, and
    /// `ctx.copy_text` called mid-pass is exactly how that is spelled.
    fn copy_toast(&mut self, text: String) {
        self.status = crate::capture::copied_toast_zh(&text);
        self.pending_copy = Some(text.clone());
        self.toast = Some((crate::capture::copied_toast_zh(&text), Instant::now()));
    }

    /// Hand a pending copy to egui. Called once per pass, after the UI is built.
    fn flush_copy(&mut self, ctx: &egui::Context) {
        if let Some(text) = self.pending_copy.take() {
            ctx.copy_text(text);
        }
    }

    /// A click in the tree: select the row, ask its detail, and — when the row
    /// stands for a prim — select that prim in the viewport and copy its path.
    ///
    /// **A layer row's click is also a preview when that layer is a preview control**
    /// (`TreeAction::Preview`), and `preview` is asked *beside* the detail rather than
    /// instead of it: the user's ruling is that clicking an arc's target-layer leaf must
    /// equal clicking the layer itself, and what a layer row means is now one thing --
    /// select it, show its detail, and (for a root or op layer) compose up to it. A row
    /// that is not a control leaves the composition alone (`docs/PROTOCOL.md` v1.4: 点非
    /// op 节点仍走 comp_node 详情，不改预览).
    fn on_tree_select(&mut self, sel: crate::tree::TreeSelection) {
        self.ask_node(&sel.id);
        if let Some(path) = crate::panel::copy_payload(&sel) {
            self.model.selection = Some(path.clone());
            self.copy_toast(path);
            self.preview = None;
            self.gizmo = None;
            self.ask_render();
        }
        // **A layer click reaches that layer's row** (v1.5.8): the row may be scrolled
        // far out of sight, and an arc's target-layer leaf is a click on the layer, so
        // the panel brings the layer's own first-level row into view on the next pass.
        if sel.kind == crate::tree::TreeNodeKind::Layer {
        }
        self.tree.selection = Some(sel);
        self.request_repaint();
    }

    /// A **layer row** was clicked: the one click a layer has, whatever drew it.
    ///
    /// The detail first — a layer row always shows that layer's `comp_node` — then the
    /// jump to the layer's own row, then the composition the row asks for
    /// ([`crate::panel::ask_for`]): `preview{upto}` for a cook op or the root layer,
    /// `preview{solo}` for a material (reference / plain sublayer) layer, nothing for the
    /// session layer. Asking both halves is the user's ruling made executable: 点
    /// reference 的层叶子 == 点这个 layer 的本身
    /// (`docs/PROTOCOL.md` v1.5.5), so neither half may be skipped for either row.
    ///
    /// The preview goes through [`Shell::ask_preview`] / [`Shell::ask_solo`] like the
    /// 「final」 button does, so a refused id (`-32602`) is reported the same way rather
    /// than leaving the bar claiming a composition the viewport never showed.
    /// A click on a row of the **hierarchy an operator declares** (v1.13): show *only*
    /// that named path.
    ///
    /// The path is first resolved to the layer that **authored** it. A consumer hides what
    /// it reads with a `visibility` opinion in its **own** layer — the input itself is not
    /// replaced (v1.5.1) — so drawing an input path from the consumer's stack would draw
    /// nothing. Composing its author's layer and then filtering to the path is what shows
    /// the structure the document declared.
    fn on_tree_render(&mut self, selection: crate::tree::TreeSelection) {
        let Some(path) = selection.prim.clone() else {
            return;
        };
        // **The clicked row is the selected one**: a `Render` row lit nothing before, so a
        // click on an operator's `input` looked like it had not landed.
        self.tree.selection = Some(selection);
        let shown = self.model.preview.clone();
        match self.tree.model.owner_of(&path) {
            Some(owner) if Some(owner.clone()) != shown => {
                self.pending_render = Some(path);
                self.show_layer_local(&owner);
                self.window.request_redraw();
            }
            _ => self.render_alone(path),
        }
    }

    /// Select `path` and turn 「只看选中」 on: the same two-state change the filter makes,
    /// applied to one named path instead of to whatever the pointer picked.
    fn render_alone(&mut self, path: String) {
        self.model.selection = Some(path.clone());
        self.status = self.model.status_line();
        if !self.only_selected {
            self.toggle_only_selected();
        } else {
            self.ask_render();
            self.copy_toast(path);
        }
        self.window.request_redraw();
    }

    fn on_tree_layer(
        &mut self,
        selection: crate::tree::TreeSelection,
        ask: Option<crate::panel::LayerAsk>,
    ) {
        let id = selection.id.clone();
        // **Remember which row this was, by position among the preview rows** (v1.9): a
        // rebuild mints new layer ids, so the id in hand dies with the pass — but the rows
        // themselves are in the same order, so a refused id is re-asked for the row in that
        // position instead of making the user click twice.
        if let Some(ask) = &ask {
            let rows = self.preview_rows();
            let index = rows.iter().position(|n| n.id == id);
            let word = match ask {
                crate::panel::LayerAsk::Upto => "upto",
                crate::panel::LayerAsk::Solo => "solo",
            };
            self.pending_preview = index.map(|i| (i, word.to_owned()));
        }
        self.on_tree_select(selection);
        // **This click is answered locally** (v1.9), so no pending retry is left behind: that
        // marker belongs to the server-side preview path, and leaving it set here re-opened
        // the layer on every tree refresh — while the sync was rewriting the very file.
        self.pending_preview = None;
        // **A different USD is a different picture** (v1.13): the selection and 「只看选中」
        // name paths of the USD that was open, so they are cleared before another one is
        // composed — otherwise the filter asks for a path this file does not have (the
        // render after clicking terrain.rusd then a boxes row failed on
        // `/World/Samples/points`).
        if self.only_selected {
            self.toggle_only_selected();
        }
        self.model.selection = None;
        self.pending_render = None;
        match ask {
            Some(_) => self.show_layer_local(&id),
            None => {}
        }
    }

    /// Show one layer from the **local** copy: open the file the cache holds and adopt it.
    fn show_layer_local(&mut self, id: &str) {
        let Some(remote) = self.layer_files.get(id).cloned() else {
            self.status = format!("这一层没有文件（内存层）：{id}");
            return;
        };
        let local = crate::cache::LayerCache::new(crate::cache::LayerCache::default_dir())
            .local_path(&remote);
        if !local.is_file() {
            // The sync about to run is for **this** click: when it lands, open the layer
            // (the `SYNC_TAG` arm), or a click made while the first sync is still writing
            // the file would only ever sync and never show anything.
            self.status = format!("本地还没有这一层，正在同步：{}", local.display());
            self.want_open = Some(id.to_owned());
            self.net.send(NetCommand::SyncLayer {
                tag: SYNC_TAG.to_owned(),
                path: remote,
            });
            return;
        }
        let opened = local.to_string_lossy().replace('\\', "/");
        // **Opened on a worker thread** (v1.9): `Stage` is `!Send`, so the thread opens the
        // file, extracts, and hands the geometry back; the window keeps drawing and the
        // control channel keeps answering while a 2400-prim product is read.
        self.status = format!("正在本地打开 {}…", local.display());
        let (tx, rx) = std::sync::mpsc::channel();
        let path_for_thread = opened.clone();
        // Every remote layer this session knows, so the product's arcs can be pointed at the
        // copies the cache holds (v1.9): a product names its materials by the **server's**
        // paths, and those do not exist here.
        let _known: usize = self.layer_files.len();
        std::thread::spawn(move || {
            rusd_scene::document::install();
            // **Nothing is rewritten** (v1.9): the product's arcs are relative (`../box.rusd`,
            // `rusd_op_…_2.rusd`), and the cache mirrors the layout, so the file opens as it
            // is. A resolver is still installed for a path this machine happens to lack a
            // byte-identical copy of.
            let resolver = crate::cache::CacheResolver::new(crate::cache::LayerCache::default_dir());
            let opened = openusd::usd::Stage::builder()
                .resolver(resolver)
                .open(path_for_thread.as_str());
            let result = match opened {
                Ok(stage) => Ok(rusd_scene::geometry::extract(&stage)),
                Err(e) => Err(format!("本地打开失败：{e}")),
            };
            let _ = tx.send(result);
        });
        self.opening = Some((id.to_owned(), rx));
    }

    /// Take a local open that finished, if one did. Called once a frame.
    fn poll_local_open(&mut self) {
        let Some((id, rx)) = &self.opening else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(geometry)) => {
                let id = id.clone();
                let bounds = geometry.bounds;
                let made = crate::session::MadeScene {
                    version: self.model.version.unwrap_or(0),
                    geometry,
                    bounds,
                    diags: Vec::new(),
                    stage: None,
                };
                // **The file the click opened is a new document, so it frames itself**
                // (v1.9). The layer can sit anywhere and be any size — a point cloud at
                // z≈0 after a box cluster at z≈20 — and keeping the previous framing
                // leaves it entirely outside the frustum, which reads as "点了节点不显示".
                // The server-snapshot path already re-arms the fit when the composition
                // changes (`session.rs`); a local open never went through it.
                self.model.rearm_first_fit();
                self.adopt_scene(made);
                self.model.preview = Some(id);
                self.status = "本地打开完成".to_owned();
                // A hierarchy row asked to show one path: now that its author's layer is
                // the one composed, filter to that path.
                if let Some(path) = self.pending_render.take() {
                    self.render_alone(path);
                }                self.opening = None;
                self.request_repaint();
            }
            Ok(Err(message)) => {
                self.status = message;
                self.opening = None;
                self.request_repaint();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // **The worker is still reading the file, and nothing else will wake the
                // window.** An idle egui app draws no frame until an input arrives, and this
                // loop installs no egui repaint callback, so `request_repaint_after` alone
                // would schedule a frame nothing ever wakes for (measured: the status sat at
                // 「正在本地打开…」 for fifteen silent seconds). The pair `request_repaint`
                // is the only thing that actually draws — the same pair every other wake in
                // this shell uses — so a local open keeps painting until its geometry lands.
                self.request_repaint();
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.status = "本地打开失败：线程结束".to_owned();
                self.opening = None;
            }
        }
    }

    #[allow(dead_code)]
    fn show_layer_local_blocking(&mut self, id: &str) {
        let Some(remote) = self.layer_files.get(id).cloned() else {
            return;
        };
        let local = crate::cache::LayerCache::new(crate::cache::LayerCache::default_dir())
            .local_path(&remote);
        let opened = local.to_string_lossy().replace('\\', "/");
        // The viewer reads `.rusd` layers itself now, so it needs the document decoder
        // registered in **this** process (v1.9): the format is openusd's, the meaning is
        // rusd-scene's, and one call installs it.
        rusd_scene::document::install();
        match openusd::usd::Stage::open(opened.as_str()) {
            Ok(stage) => {
                let geometry = rusd_scene::geometry::extract(&stage);
                let bounds = geometry.bounds;
                let made = crate::session::MadeScene {
                    version: self.model.version.unwrap_or(0),
                    geometry,
                    bounds,
                    diags: Vec::new(),
                    stage: Some(stage),
                };
                self.model.rearm_first_fit();
                self.adopt_scene(made);
                self.model.preview = Some(id.to_owned());
                self.status = format!("本地打开 {}", local.display());
                self.request_repaint();
            }
            Err(e) => self.status = format!("本地打开失败：{e}"),
        }
    }

    /// The rows a click composes something for, in the order the panel draws them.
    fn preview_rows(&self) -> Vec<crate::tree::TreeNode> {
        crate::panel::rows(&self.tree)
            .into_iter()
            .map(|(_, node)| node)
            .filter(|node| {
                matches!(
                    crate::panel::action_for(node),
                    crate::panel::TreeAction::Layer { ask: Some(_), .. }
                )
            })
            .collect()
    }

    /// Take the capture the user just framed: the frame's own beauty image, a mask
    /// of the region at that size, and the camera the frame was drawn with.
    ///
    /// Everything here is local: the picture is the one on screen (so it costs no
    /// render), the mask is built from the same rectangle a pick would use, and the
    /// camera is [`crate::net::Frame::camera`] — what the pixels actually show, not
    /// the orbit the UI has since moved to.
    ///
    /// Offline (`--usdc`) this still works and says `scene_version: 0`: the frame is
    /// real even when no server produced it (`docs/PROTOCOL.md` v1.1 验收).
    fn take_capture(&mut self, start: egui::Pos2, end: egui::Pos2) {
        let Some((frame, _)) = self.frame.as_ref() else {
            self.status = "capture: 还没有画面（等第一帧）".to_owned();
            return;
        };
        let frame_size = (frame.width, frame.height);
        let (frame_width, frame_height, camera_json) =
            (frame.width, frame.height, crate::capture::camera_json_of(&frame.camera));
        let Some(sel) = crate::panel::mask_rect(self.viewport.rect(), start, end, frame_size)
        else {
            self.status = "capture: 选区为空".to_owned();
            return;
        };
        // A capture is the one thing that needs the pixels on the CPU (v1.10), so the
        // frame is read back here — once, on the GPU's own copy of the picture on screen.
        let Some((beauty, _ids)) = self.render.grab() else {
            self.status = "capture: 显卡没有交回像素".to_owned();
            return;
        };
        let image_png = match crate::capture::encode_png(frame_width, frame_height, &beauty) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.status = format!("capture: 编码 PNG 失败：{e}");
                return;
            }
        };
        let mask = crate::capture::mask_rgba(frame_size, sel);
        let mask_png = match crate::capture::encode_png(frame_width, frame_height, &mask) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.status = format!("capture: 编码 mask 失败：{e}");
                return;
            }
        };
        let camera = camera_json;
        let params = crate::capture::capture_params(
            camera,
            &image_png,
            Some(&mask_png),
            self.model.version.unwrap_or(0),
            Some(self.capture_note.as_str()),
        );
        self.net.send(NetCommand::Call {
            tag: CAPTURE_TAG.to_owned(),
            method: "capture".to_owned(),
            params,
        });
        self.capture_armed = false;
        self.capture_start = None;
        self.status = format!(
            "capture 已发送（{}×{} 帧，选区 {}×{}）",
            frame_width,
            frame_height,
            sel.width(),
            sel.height()
        );
        self.request_repaint();
    }

    // --- render thread → view --------------------------------------------

    /// Take the newest frame, if one arrived, and upload it as an egui texture.
    fn drain_frames(&mut self) {
        if let Some(diags) = self.render.take_error() {
            self.status = join_diags(&diags);
        }
        let Some(frame) = self.render.take_frame() else {
            return;
        };
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame_at).as_secs_f64();
        if dt > 0.0 {
            self.fps = if self.fps == 0.0 {
                1.0 / dt
            } else {
                0.75 * self.fps + 0.25 / dt
            };
        }
        self.last_frame_at = now;

        let texture = if frame.beauty.is_empty() {
            // **The picture is already on the GPU** (v1.10): register the render thread's
            // own beauty view as an egui texture and paint it. No copy, no upload, and the
            // registration is done once per target generation (a resize is the only thing
            // that makes a new view).
            match self.register_view(&frame) {
                Some(id) => id,
                None => {
                    // No render state to register with (a headless build): fall back to
                    // showing nothing rather than a texture that does not exist.
                    self.frames_received += 1;
                    return;
                }
            }
        } else {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [frame.width as usize, frame.height as usize],
                &frame.beauty,
            );
            self.cpu_tex = Some(self.egui_ctx.load_texture(
                "viewport-cpu",
                image,
                egui::TextureOptions::LINEAR,
            ));
            self.cpu_tex.as_ref().expect("just set").id()
        };
        self.tex_size = (frame.width, frame.height);
        self.frames_received += 1;
        self.frame = Some((frame, texture));
        // **The upload only becomes visible on the *next* pass.** `load_texture`
        // registers the image with the texture manager, and the manager's deltas are
        // handed to the painter at the end of the pass that is running now — a pass
        // whose widget tree was built *before* this texture existed. So a frame that
        // arrives while a redraw is in flight is uploaded but not drawn: the window
        // keeps showing the previous one. Repainting once here is what actually puts
        // the new frame on screen, and it is the difference the debug overlay shows
        // as `shown == received` instead of `shown < received`.
        self.request_repaint();
    }

    /// Paint the viewport from the frame's texture, GPU view or upload alike.
    fn viewport_image(texture: egui::TextureId, size: egui::Vec2, uv: egui::Rect) -> egui::Image<'static> {
        egui::Image::new(egui::load::SizedTexture::new(texture, size)).uv(uv)
    }

    /// Register the render thread's beauty view with egui, once per target generation.
    fn register_view(&mut self, frame: &crate::net::Frame) -> Option<egui::TextureId> {
        let state = self.painter.render_state()?;
        if let Some((id, generation)) = self.gpu_tex {
            if generation == frame.generation {
                return Some(id);
            }
            state.renderer.write().free_texture(&id);
            self.gpu_tex = None;
        }
        let id = state.renderer.write().register_native_texture(
            &state.device,
            &frame.view,
            wgpu::FilterMode::Linear,
        );
        self.gpu_tex = Some((id, frame.generation));
        Some(id)
    }

    /// Ask the render thread for a frame with the current camera and selection.
    fn ask_render(&mut self) {
        if let Some(orbit) = self.orbit {
            self.render.send(RenderCommand::View {
                orbit,
                selection: self.model.selection.clone(),
                only_selected: self.only_selected,
            });
        }
    }

    /// Flip 「只看选中」: the viewport draws the selected prim's subtree, or the whole
    /// scene when nothing is selected.
    ///
    /// **It asks for a frame and nothing else.** No request goes to the server, no
    /// version moves, and the camera is untouched: the filter is applied to the
    /// geometry this client already holds (`crate::scene::isolate`, through the render
    /// thread's re-bake), which is what makes it a lens rather than a composite.
    ///
    /// **A new selection is a new picture** even while the switch stays on: every path
    /// that changes `model.selection` ends in `ask_render`, and that command carries
    /// the flag, so the render thread filters to whatever is selected now. This method
    /// therefore has nothing to do about selection -- only about the switch.
    fn toggle_only_selected(&mut self) {
        self.only_selected = !self.only_selected;
        self.status = match (self.only_selected, self.model.selection.is_some()) {
            (true, true) => "只看选中：开".to_owned(),
            (true, false) => "只看选中：开（无选中，仍画整场景）".to_owned(),
            (false, _) => "只看选中：关".to_owned(),
        };
        self.ask_render();
        self.window.request_redraw();
    }

    // --- the control channel (v1.7) ---------------------------------------

    /// Carry out every control request queued since the last pass, in order.
    ///
    /// One pass drains the queue, so two requests that arrived together are applied to
    /// one state and the frame after them shows both. Nothing here blocks: a method
    /// whose answer needs the server *sends* its request and leaves a [`Waiting`]
    /// behind, and [`Shell::note_control_result`] answers it when the reply lands.
    fn drain_control(&mut self) {
        let Some(queue) = self.control.as_ref() else {
            return;
        };
        let mut batch = Vec::new();
        while let Ok(q) = queue.try_recv() {
            batch.push(q);
        }
        if batch.is_empty() {
            return;
        }
        for q in batch {
            self.on_control(q);
        }
        // One control pass can move the camera, the selection or the frame: ask for the
        // picture that shows it, the way a click does.
        self.request_repaint();
    }

    /// One request: pick the method, carry it out, and answer — now, or on the reply.
    ///
    /// Every arm below calls the function the widget calls. That is the whole design of
    /// v1.7 (`docs/PROTOCOL.md`: 所有方法走和 GUI 事件同一条路径): a control method is
    /// a **name for a click**, never a second implementation of one.
    fn on_control(&mut self, q: crate::control::Queued) {
        let crate::control::Queued { req, reply } = q;
        let args = crate::control::Args {
            method: &req.method,
            map: &req.params,
        };
        // The two halves of the table, told apart by what the answer *is*: a **deferred**
        // method's result is a reply from the server, so it leaves a waiter behind and
        // returns `None`; a **local** one reads state this shell already holds and answers
        // in this pass. Each deferred arm takes a clone of the replier, so the waiter it
        // keeps and the refusal it may return are the same answer channel.
        let immediate: Option<Result<serde_json::Value, (i64, String)>> = match req.method.as_str()
        {
            "open" => self.control_open(&args, reply.clone()),
            "preview" => self.control_preview(&args, reply.clone()),
            "tree" => self.control_tree(&args, reply.clone()),
            "select" => self.control_select(&args, reply.clone()),
            "state" => Some(Ok(self.control_state())),
            "isolate" => self.control_isolate(&args),
            "translate" => self.control_translate(&args),
            "fit" => self.control_fit(),
            "orbit" => self.control_orbit(&args),
            "capture" | "shot" => self.control_capture(&args),
            // Unreachable: the connection thread refuses an unknown method before the
            // request is queued (`crate::control::parse_line`). Answered rather than
            // ignored so a change of that rule cannot leave a caller waiting.
            other => Some(Err((
                crate::control::METHOD_NOT_FOUND,
                format!("unknown method: {other}"),
            ))),
        };
        if let Some(outcome) = immediate {
            reply.answer(outcome);
        }
    }

    /// `state`: the whole of what the GUI is showing, in one object.
    ///
    /// **Always answerable** (the contract says so): it reads fields the shell already
    /// holds and touches no socket, so a script can ask before the server has answered —
    /// and a viewer that never connected says so, in a shape a script can branch on.
    fn control_state(&self) -> serde_json::Value {
        use crate::tree::TreeModel;
        let tree = match &self.tree.model {
            TreeModel::Idle => json!({ "state": "idle" }),
            TreeModel::Loading => json!({ "state": "loading" }),
            TreeModel::Ready(t) => json!({
                "state": "ready",
                "layers": t.layers.len(),
                "arcs": t.arcs.len(),
                "prims": t.prims.len(),
                "variants": t.variants.len(),
            }),
            TreeModel::Unavailable(m) => json!({ "state": "unavailable", "message": m }),
            TreeModel::Failed(m) => json!({ "state": "failed", "message": m }),
        };
        json!({
            "connected": self.model.state.is_live(),
            "server": self.connect,
            "control": self.control_addr.map(|a| a.to_string()),
            "version": self.model.version,
            "preview": self.model.preview,
            "selection": self.model.selection,
            "isolate": self.only_selected,
            "prims": self.model.item_count,
            "offline": self.model.offline,
            "tree": tree,
            "orbit": self.orbit.map(orbit_json),
            "frame": self.frame.as_ref().map(|(f, _)| {
                json!({ "seq": f.seq, "width": f.width, "height": f.height, "millis": f.millis,
                        "rewritten": f.rewritten, "slabs": f.slabs, "instances": f.instances,
                        "draws": f.draws, "matrix_bytes": f.matrix_bytes })
            }),
            "status": self.status,
        })
    }

    /// What a method that needs the server answers when there is none.
    fn need_server(&self) -> Result<(), (i64, String)> {
        if self.model.state.is_live() {
            Ok(())
        } else {
            Err((
                crate::control::NOT_CONNECTED,
                "not connected: this viewer has no live server connection (start it with \
                 --connect and wait for `state` to say connected)"
                    .to_owned(),
            ))
        }
    }

    /// `open{path}`: the document open, forwarded to the server, answered with the
    /// server's own reply.
    ///
    /// The scene, the tree and the camera all follow as they do for an `open` a user
    /// caused: the reply arrives on the network worker, `state_changed{reason:"open"}`
    /// after it re-reads the tree and refits (`crate::view`). `{"path": …}` is the
    /// server's own parameter (`rusd-agent`'s `open`), so nothing is translated.
    fn control_open(
        &mut self,
        args: &crate::control::Args<'_>,
        reply: crate::control::Replier,
    ) -> Option<Result<serde_json::Value, (i64, String)>> {
        let path = match args.text("path") {
            Ok(p) => p,
            Err(e) => return Some(Err(e)),
        };
        if let Err(e) = self.need_server() {
            return Some(Err(e));
        }
        // The waiter owes the tag this request sends: `note_control_result` answers a
        // waiter whose `awaiting` holds the tag that just landed, so a waiter built with
        // the empty `new` never matched anything and the control caller waited forever
        // (the work happened, the click worked, only the answer was lost).
        self.waiting.push(Waiting::with(reply, Shape::Server, vec![OPEN_TAG]));
        self.status = format!("open {path}: 请求中…");
        self.send_net(NetCommand::Call {
            tag: OPEN_TAG.to_owned(),
            method: "open".to_owned(),
            params: json!({ "path": path }),
        });
        None
    }

    /// `preview{upto}` / `preview{solo}`: the preview controls' click, in the two forms
    /// the wire has.
    ///
    /// `upto: <layer id>` is a **prefix** (v1.4/v1.5.3 — the op and root rows' click),
    /// `upto: null` is 「final」, and `solo: <layer id>` is the material (reference /
    /// plain sublayer) row's click, which shows that layer's content and nothing else.
    /// Exactly one of the two keys may be given, and `upto` must be **spelled** as null
    /// for 「final」 rather than omitted: the server keeps preview state per connection,
    /// so an omission would leave it where it was (`crate::protocol::preview_params`).
    fn control_preview(
        &mut self,
        args: &crate::control::Args<'_>,
        reply: crate::control::Replier,
    ) -> Option<Result<serde_json::Value, (i64, String)>> {
        let has_upto = args.map.contains_key("upto");
        let has_solo = args.map.contains_key("solo");
        let params = match (has_upto, has_solo) {
            (true, false) => match args.opt_text("upto") {
                Ok(upto) => protocol::preview_params(upto.as_deref()),
                Err(e) => return Some(Err(e)),
            },
            (false, true) => match args.text("solo") {
                Ok(id) => protocol::preview_solo_params(&id),
                Err(e) => return Some(Err(e)),
            },
            (true, true) => {
                return Some(Err(args.bad(
                    "give `upto` or `solo`, not both: they are two different compositions",
                )));
            }
            (false, false) => {
                return Some(Err(args.bad(
                    "`upto` (a layer id, or null for the final composition) or `solo` \
                     (a layer id) is required",
                )));
            }
        };
        if let Err(e) = self.need_server() {
            return Some(Err(e));
        }
        self.waiting.push(Waiting::with(reply, Shape::Server, vec![PREVIEW_TAG]));
        self.send_net(NetCommand::Call {
            tag: PREVIEW_TAG.to_owned(),
            method: "preview".to_owned(),
            params,
        });
        None
    }

    /// `tree{id}`: the click on that row — or, with no `id`, the rows themselves.
    ///
    /// The click half is `action_for`'s answer carried out by the arms the panel's
    /// [`crate::panel::TreeAction`] takes (`on_tree_layer`, `on_tree_select`,
    /// `ask_preview`), so a script naming a row gets the same detail, the same selection
    /// and the same preview a user's click gets — including the two-ask shape a layer
    /// row has.
    ///
    /// The listing half is what makes a script possible at all: it prints the rows with
    /// their ids, and those ids are exactly the strings this method and `comp_node` take.
    fn control_tree(
        &mut self,
        args: &crate::control::Args<'_>,
        reply: crate::control::Replier,
    ) -> Option<Result<serde_json::Value, (i64, String)>> {
        let id = match args.opt_text("id") {
            Ok(v) => v.filter(|s| !s.is_empty()),
            Err(e) => return Some(Err(e)),
        };
        let rows = crate::tree::flatten(&self.tree.model.roots());
        let Some(id) = id else {
            // No id: the tree as rows. Read from the panel's model — the copy the GUI is
            // drawing — because a listing that could disagree with the screen would make
            // `tree{id}` and the click two trees.
            let rows: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|(depth, node)| {
                    json!({
                        "id": node.id,
                        "kind": node.kind.as_str(),
                        "label": node.label,
                        "depth": depth,
                        "op": node.op,
                        "root": node.root,
                        "session": node.session,
                        // Which composition this row's click asks for, in the words the
                        // channel's own `preview` method takes: a script reads this and
                        // sends `preview{upto}` / `preview{solo}` rather than re-deriving
                        // the rule from the flags it can see.
                        "preview_ask": match crate::panel::ask_for(&node) {
                            Some(crate::panel::LayerAsk::Upto) => "upto",
                            Some(crate::panel::LayerAsk::Solo) => "solo",
                            None => "none",
                        },
                        "prim": node.prim,
                        "rows_below": node.children.len(),
                    })
                })
                .collect();
            return Some(Ok(json!({
                "state": if self.tree.model.ready() { "ready" } else { "not_ready" },
                "status": self.tree.model.status_zh(),
                "preview": self.model.preview,
                // Which composition the frame on screen is, in the same words a row's
                // `preview_ask` uses: `upto` for a cook prefix, `solo` for a bare layer, and
                // `final` when nothing is muted.
                "composition": match (self.model.preview.is_some(), self.preview_kind()) {
                    (false, _) => "final",
                    (true, Some(crate::panel::LayerAsk::Solo)) => "solo",
                    (true, _) => "upto",
                },
                "rows": rows,
            })));
        };
        if let Err(e) = self.need_server() {
            return Some(Err(e));
        }
        let Some((_, node)) = rows.iter().find(|(_, n)| n.id == id) else {
            let known: Vec<&str> = rows.iter().map(|(_, n)| n.id.as_str()).collect();
            let shown: Vec<&str> = known.iter().take(40).copied().collect();
            let more = if known.len() > shown.len() {
                format!(
                    " … and {} more (ask tree{} for the whole list)",
                    known.len() - shown.len(),
                    ""
                )
            } else {
                String::new()
            };
            return Some(Err(args.bad(format!(
                "no row with id {id:?} in the composition tree; it has: {}{more}",
                shown.join(", ")
            ))));
        };
        let row = node.clone();
        // **A row naming a USD the document declared is a link, not a node of the
        // composition** (v1.12). The file opens locally, and the server never opened that
        // layer, so the `comp_node` this click would owe can only be refused — every click
        // on such a row answered an error right after the picture had already changed.
        let is_link = match &self.tree.model {
            crate::tree::TreeModel::Ready(t) => t.layer(&row.id).is_none(),
            _ => true,
        };
        if is_link {
            let action = crate::panel::action_for(&row);
            let word = action_word(&action).to_owned();
            match action {
                // A row of the operator's declared hierarchy renders its path alone; it
                // is not a file, so nothing is opened here.
                crate::panel::TreeAction::Render(sel) => self.on_tree_render(sel),
                crate::panel::TreeAction::Select(sel) => {
                    self.on_tree_select(sel);
                    self.show_layer_local(&row.id);
                }
                _ => self.show_layer_local(&row.id),
            }
            self.request_repaint();
            return Some(Ok(json!({
                "action": word,
                "id": row.id,
                "detail": serde_json::Value::Null,
            })));
        }
        // What a click on this row means — the panel's own rule, not a copy of it.
        let action = crate::panel::action_for(&row);
        let shape = Shape::Row {
            id: row.id.clone(),
            kind: row.kind.as_str().to_owned(),
            action: action_word(&action),
        };
        // Which reply tags that click will owe. A row's click asks its `comp_node`
        // detail, and that is **all** it can owe: the composition half of a layer click is
        // served locally now (v1.9 — `show_layer_local` opens the cached file and mutes
        // nothing on the server), so a `PREVIEW_TAG` in this list would be a reply no
        // request was sent for and the waiter would never be answered. A plain `Select`
        // asks its detail alone; whether it has a prim node at all was settled above.
        let owed = vec![NODE_TAG];
        // The replier is in hand **before** the click runs: `on_tree_*` sends its request
        // on the spot, and that request's reply is what this one waits for.
        self.waiting.push(Waiting::with(reply, shape, owed));
        match action {
            crate::panel::TreeAction::Select(sel) => self.on_tree_select(sel),
            crate::panel::TreeAction::Render(sel) => self.on_tree_render(sel),
            crate::panel::TreeAction::Layer { selection, ask } => {
                self.on_tree_layer(selection, ask)
            }
            crate::panel::TreeAction::Preview(upto) => self.ask_preview(upto),
            crate::panel::TreeAction::PreviewSolo(layer) => self.ask_solo(layer),
            // `action_for` never returns these for a row that exists (`Refresh` is the
            // panel's button and `None` is "no click"), so nothing was sent: the waiter
            // has no tag to wait for and `note_control_result` would never see it.
            crate::panel::TreeAction::Refresh | crate::panel::TreeAction::None => {
                let w = self.waiting.pop();
                if let Some(w) = w {
                    w.reply.ok(json!({
                        "id": row.id,
                        "kind": row.kind.as_str(),
                        "label": row.label,
                        "action": "none",
                        "note": "this row's click asks for nothing",
                    }));
                }
            }
        }
        None
    }

    /// `select{path}`: the viewport pick of a prim named by path.
    ///
    /// The pick path's own second half, with no pixel involved: the selection,
    /// `ask_render` for the outline, and `inspect_picked` for the per-layer detail —
    /// exactly what `Shell::click` does once it has resolved a pixel to a path.
    ///
    /// A path the scene does not hold is `-32602` naming the miss: a selection that
    /// outlines nothing is worse than a refusal a script can read.
    ///
    /// **The clipboard is not written.** A GUI pick copies the path onto the desktop
    /// clipboard (`on_tree_select`'s precedent); a script doing the same would overwrite
    /// whatever the user had there, so the control path leaves it alone — the path is
    /// in the answer instead.
    fn control_select(
        &mut self,
        args: &crate::control::Args<'_>,
        reply: crate::control::Replier,
    ) -> Option<Result<serde_json::Value, (i64, String)>> {
        let path = match args.text("path") {
            Ok(p) => p,
            Err(e) => return Some(Err(e)),
        };
        if let Err(e) = self.need_server() {
            return Some(Err(e));
        }
        // A path is selectable when **either** side knows it: the frame on screen (an
        // outline follows a selection there) or the composition report (`prim_nodes`, which
        // is what the scene tree draws and what a per-layer detail needs). Requiring the
        // frame alone refused a scene-tree row whose prim the last render did not contain —
        // a report and a frame are two answers about the same stack, and they need not
        // arrive in the same breath.
        let in_scene = self.model.centers.contains_key(&path);
        let in_report = self.tree.model.prim_id(&path).is_some();
        if !in_scene && !in_report {
            return Some(Err(args.bad(format!(
                "no prim at {path:?}: the scene on screen has {} prims and the composition \
                 report lists none by that path",
                self.model.item_count
            ))));
        }
        let shape = Shape::Pick { path: path.clone() };
        let row = self.tree.model.prim_id(&path).map(str::to_owned);
        let Some(node_id) = row else {
            // A prim the composition tree does not describe: ordinary for a flattened cook
            // output (a city's instances are such prims). The selection still happens —
            // that is what a viewport pick does too — and the detail panel says why it has
            // no per-layer view instead of going quiet (`inspect_picked`'s own words).
            self.select_path(&path);
            return Some(Ok(json!({
                "path": path,
                "selected": true,
                "node": serde_json::Value::Null,
                "detail": serde_json::Value::Null,
                "note": "the composition tree lists no node for this path (a cook \
                         output), so there is no per-layer detail to show",
            })));
        };
        self.waiting.push(Waiting::with(reply, shape, vec![NODE_TAG]));
        self.select_path(&path);
        self.ask_node(&node_id);
        let _ = node_id;
        None
    }

    /// Select a prim the way a viewport pick does: the selection, the outline's
    /// re-render, the per-layer detail, and the status line.
    ///
    /// One function for both entry points — [`Shell::click`] resolves a pixel to a path
    /// and calls this; `select{path}` has the path already and calls the same thing. The
    /// copy to the clipboard is **not** here: a click copies the path for the user, a
    /// control request reports it in its answer instead, and putting that decision at the
    /// two callers is what keeps a script from overwriting the desktop clipboard.
    fn select_path(&mut self, path: &str) {
        self.model.selection = Some(path.to_owned());
        self.tree.detail = None;
        self.tree.detail_error = None;
        self.ask_render();
        self.inspect_picked(path);
        self.status = self
            .model
            .status_line_zh(self.preview_kind(), |id| self.layer_name(id));
        self.window.request_redraw();
    }

    /// `isolate{on}`: the 「只看选中 (i)」 switch, asked for as a state.
    ///
    /// `{on: true}` is the key `i` pressed once from off, `{on: false}` once from on;
    /// asking for the state it is already in does nothing. The work is
    /// [`Shell::toggle_only_selected`] — the same function the key and the button call.
    fn control_isolate(
        &mut self,
        args: &crate::control::Args<'_>,
    ) -> Option<Result<serde_json::Value, (i64, String)>> {
        let on = match args.boolean("on") {
            Ok(b) => b,
            Err(e) => return Some(Err(e)),
        };
        if self.only_selected != on {
            self.toggle_only_selected();
        }
        Some(Ok(json!({
            "isolate": self.only_selected,
            "selection": self.model.selection,
        })))
    }

    /// `translate{path, to:[x,y,z]}`: **one gizmo-drag tick, without the gesture**
    /// (v1.11).
    ///
    /// It is the call `drag_gizmo` makes while the pointer moves: the prim is drawn at a
    /// **local** translate, locally, and the server is told nothing (a drag that ends
    /// sends the opinion; this method does not, so a script can move a prim without
    /// authoring anything). What it costs is what a drag costs, and `state.frame` says
    /// so: `matrix_bytes: 64` (one model matrix) and `rewritten: 0` (no vertex touched).
    fn control_translate(
        &mut self,
        args: &crate::control::Args<'_>,
    ) -> Option<Result<serde_json::Value, (i64, String)>> {
        let path = match args.text("path") {
            Ok(p) => p,
            Err(e) => return Some(Err(e)),
        };
        let to = match args.opt_vec3("to") {
            Ok(Some(v)) => v,
            Ok(None) => return Some(Err(args.bad("`to` ([x, y, z]) required"))),
            Err(e) => return Some(Err(e)),
        };
        if !self.model.centers.contains_key(&path) {
            return Some(Err(args.bad(format!(
                "no prim at {path:?}: the scene on screen has {} prims and none by that path",
                self.model.item_count
            ))));
        }
        let mut overrides = BTreeMap::new();
        overrides.insert(path.clone(), to);
        // The selection is the shell's own: a move must not change what is picked.
        self.render.send(RenderCommand::Preview {
            overrides,
            selection: self.model.selection.clone(),
        });
        self.request_repaint();
        Some(Ok(json!({ "path": path, "to": to })))
    }

    /// `fit`: the Fit button.
    fn control_fit(&mut self) -> Option<Result<serde_json::Value, (i64, String)>> {
        if self.model.bbox.is_none() {
            return Some(Err((
                crate::control::NOT_READY,
                "fit: no scene has arrived yet, so there is nothing to frame".to_owned(),
            )));
        }
        self.fit();
        Some(Ok(json!({
            "orbit": self.orbit.map(orbit_json),
            "camera": self.orbit.map(|o| crate::capture::camera_json(&o)),
        })))
    }

    /// `orbit{yaw, pitch, dist, target?}`: the camera, set outright.
    ///
    /// Not a gesture but the state a gesture ends in: a drag adds a delta to `yaw` /
    /// `pitch`, the wheel scales `dist`, and this asks for the resulting numbers. The
    /// pitch is clamped the way [`crate::camera::Orbit`] clamps a drag (the poles are
    /// degenerate), and a non-positive `dist` is refused rather than quietly replaced.
    ///
    /// A viewer with no orbit yet starts from the fit a gesture would have started from,
    /// so a script can set a camera before the user has touched anything.
    fn control_orbit(
        &mut self,
        args: &crate::control::Args<'_>,
    ) -> Option<Result<serde_json::Value, (i64, String)>> {
        let yaw = args.number("yaw");
        let pitch = args.number("pitch");
        let dist = args.number("dist");
        let target = args.opt_vec3("target");
        if let Err(e) = yaw {
            return Some(Err(e));
        }
        if let Err(e) = pitch {
            return Some(Err(e));
        }
        if let Err(e) = dist {
            return Some(Err(e));
        }
        if let Err(e) = target {
            return Some(Err(e));
        }
        let (Ok(yaw), Ok(pitch), Ok(dist), Ok(target)) = (yaw, pitch, dist, target) else {
            return Some(Err(args.bad("`yaw`, `pitch` and `dist` are required")));
        };
        if dist <= 0.0 {
            return Some(Err(args.bad(format!(
                "`dist` must be positive (got {dist}); it is the eye-to-target distance"
            ))));
        }
        let start = self.orbit.or_else(|| {
            self.model
                .bbox
                .map(|(min, max)| crate::view::orbit_for_bounds((min, max), self.viewport_aspect()))
        });
        let Some(mut orbit) = start else {
            return Some(Err((
                crate::control::NOT_READY,
                "orbit: no scene has arrived yet, so there is no camera to move".to_owned(),
            )));
        };
        orbit.yaw = yaw;
        orbit.pitch = pitch.clamp(-crate::camera::PITCH_LIMIT, crate::camera::PITCH_LIMIT);
        orbit.distance = dist;
        if let Some(t) = target {
            orbit.target = t;
        }
        self.orbit = Some(orbit);
        self.ask_render();
        self.window.request_redraw();
        Some(Ok(json!({
            "orbit": orbit_json(orbit),
            "camera": crate::capture::camera_json(&orbit),
        })))
    }

    /// `capture{path}` / `shot{path}`: **the frame on screen**, written as a PNG.
    ///
    /// The reader's half of 「选区截图」 with no selection: the pixels are the ones the
    /// viewport drew (`crate::net::Frame::beauty`, outline included), so this costs no
    /// render and cannot disagree with what a screenshot of the window would show. The
    /// encoding is `crate::capture`'s — the one the wire's captures already use.
    fn control_capture(
        &mut self,
        args: &crate::control::Args<'_>,
    ) -> Option<Result<serde_json::Value, (i64, String)>> {
        let path = match args.text("path") {
            Ok(p) => p,
            Err(e) => return Some(Err(e)),
        };
        let Some((frame, _)) = self.frame.as_ref() else {
            return Some(Err((
                crate::control::NOT_READY,
                "capture: no frame yet (ask again once the viewport has drawn one)".to_owned(),
            )));
        };
        let png = {
            // The pixels come from the render thread on request (v1.10); the frame that is
            // on screen is the one in its textures, so what is read back is what is shown.
            let Some((beauty, _ids)) = self.render.grab() else {
                return Some(Err((
                    crate::control::NOT_READY,
                    "capture: the GPU did not hand back the frame".to_owned(),
                )));
            };
            match crate::capture::encode_png(frame.width, frame.height, &beauty) {
                Ok(bytes) => bytes,
                Err(e) => {
                    return Some(Err((
                        crate::control::NOT_READY,
                        format!("capture: PNG encoding failed: {e}"),
                    )));
                }
            }
        };
        let written = std::path::Path::new(&path);
        if let Some(dir) = written.parent().filter(|d| !d.as_os_str().is_empty())
            && !dir.is_dir()
        {
            return Some(Err((
                crate::control::LOCAL_IO,
                format!("capture: no directory {}", dir.display()),
            )));
        }
        if let Err(e) = std::fs::write(written, &png) {
            return Some(Err((
                crate::control::LOCAL_IO,
                format!("capture: cannot write {path}: {e}"),
            )));
        }
        let absolute = std::path::absolute(written)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.clone());
        self.status = format!("capture: {absolute} ({} bytes)", png.len());
        Some(Ok(json!({
            "path": absolute,
            "width": frame.width,
            "height": frame.height,
            "bytes": png.len(),
            "frame": frame.seq,
            "version": self.model.version,
            "camera": crate::capture::camera_json_of(&frame.camera),
        })))
    }

    /// Feed one server reply to the control request that asked for it.
    ///
    /// Called from [`Shell::on_reply`] before the GUI's own handling, with the tag the
    /// reply came under. **The oldest waiter that owes this tag gets it**: two
    /// connections may both have an `open` in flight, the server answers in send order,
    /// and pairing by order is what keeps each caller's `id` with its own result. When
    /// the last tag a waiter owes is filled, the control reply goes out.
    ///
    /// Costs one length check when no control channel is open.
    fn note_control_result(
        &mut self,
        tag: &str,
        result: &Result<serde_json::Value, (i64, String)>,
    ) {
        if self.waiting.is_empty() {
            return;
        }
        let name = match tag {
            OPEN_TAG => OPEN_TAG,
            PREVIEW_TAG => PREVIEW_TAG,
            NODE_TAG => NODE_TAG,
            _ => return,
        };
        let Some(i) = self.waiting.iter().position(|w| w.awaiting.contains(&name)) else {
            return;
        };
        self.waiting[i].results.insert(name, result.clone());
        self.waiting[i].awaiting.retain(|t| *t != name);
        if !self.waiting[i].awaiting.is_empty() {
            return;
        }
        let w = self.waiting.remove(i);
        w.reply.answer(w.shape.answer(&w.results));
    }

    /// Answer every control request still waiting: the connection ended, so their
    /// replies can never arrive and a script must not be left on a socket until its own
    /// timeout. Called from the `NetEvent::Closed` arm of [`Shell::drain_net`].
    fn fail_control_waiters(&mut self, message: &str) {
        let waiting = std::mem::take(&mut self.waiting);
        for w in waiting {
            w.reply.err(crate::control::NOT_CONNECTED, message);
        }
    }

    // --- UI ---------------------------------------------------------------

    fn ui(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        // The bars' sizes are recorded as the pass lays them out, because the
        // overlay draws *inside* the layout and cannot re-measure them there.
        let mut bars = (0.0f32, 0.0f32, 0.0f32);
        let top_bar = egui::Panel::top("status").show(root, |ui| {
            ui.horizontal(|ui| {
                ui.strong("rusd-viewer");
                ui.separator();
                // The Chinese summary is the primary line: it is what makes the
                // installed CJK face visibly right (a missing glyph shows as a
                // box right here), and it is the one a user reads. The detailed
                // ASCII line follows for diagnostics and for a viewer whose
                // font did not load.
                ui.label(
                    self.model
                        .status_line_zh(self.preview_kind(), |id| self.layer_name(id)),
                );
                ui.separator();
                ui.label(&self.status);
                ui.separator();
                ui.label(format!("{:.0} fps", self.fps));
                if let Some((f, _)) = &self.frame {
                    ui.label(format!("{:.1} ms/frame", f.millis));
                }
                if self.model.reload_badge {
                    ui.separator();
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 200, 60),
                        "⟳ 热重载 reloaded on server",
                    );
                }
                if let Some((text, at)) = &self.toast
                    && crate::panel::toast_visible(Instant::now(), *at)
                {
                    ui.separator();
                    ui.colored_label(egui::Color32::from_rgb(120, 220, 140), text.clone());
                }
            });
        });
        // `Panel::show` returns the rect it took: the top bar's height, the left
        // panel's width and the controls bar's height are the three terms the
        // partition identity is made of. They are recorded here because the overlay
        // draws inside the layout, where re-measuring them is impossible.
        bars.1 = top_bar.response.rect.height();

        // The composition-tree panel: **resizable and collapsible**, and when either
        // happens the viewport re-derives its rect from `ui.available_size()` on the
        // next pass — the panels' width is an input to the viewport's size, never a
        // constant (`tests/panel_layout.rs` pins the partition). A resize does **not**
        // re-fit the camera (REMOTE §6: the camera is the client's; only the first
        // scene and the Fit button fit).
        let left_bar = if self.tree_open {
            egui::Panel::left("composition-tree")
                .resizable(true)
                .default_size(280.0)
                .min_size(180.0)
                .show(root, |ui| {
                    ui.horizontal(|ui| {
                        if ui.small_button("⟨ 收起").clicked() {
                            self.tree_open = false;
                        }
                    });
                    // The list of what the panel holds: the composition tree (layers, arcs,
                    // The detail gets its own bottom section inside the panel: the tree is
                    // a full-height scroll area, so a detail drawn *after* it lands below
                    // the visible rect and is never seen (v1.5.5 lesson: the feature
                    // existed, the pixels did not). Carve the detail first, then the tree
                    // takes the rest.
                    egui::Panel::bottom("tree-detail")
                        .resizable(true)
                        .default_size(240.0)
                        .min_size(64.0)
                        .show(ui, |ui| {
                            crate::panel::detail_panel(ui, &self.tree);
                        });
                    match crate::panel::tree_panel(ui, &mut self.tree) {
                        crate::panel::TreeAction::Select(sel) => self.on_tree_select(sel),
                        crate::panel::TreeAction::Render(sel) => self.on_tree_render(sel),
                        crate::panel::TreeAction::Layer { selection, ask } => {
                            self.on_tree_layer(selection, ask)
                        }
                        crate::panel::TreeAction::Preview(upto) => self.ask_preview(upto),
                        crate::panel::TreeAction::PreviewSolo(layer) => self.ask_solo(layer),
                        crate::panel::TreeAction::Refresh => self.ask_tree(),
                        crate::panel::TreeAction::None => {}
                    }
                })
        } else {
            // Collapsed: a strip that reopens it. The viewport takes the width back on
            // the next pass, which is the same path a drag-resize takes.
            egui::Panel::left("composition-tree-collapsed")
                .resizable(false)
                .exact_size(24.0)
                .show(root, |ui| {
                    if ui.small_button("⟩").on_hover_text("展开合成树").clicked() {
                        self.tree_open = true;
                    }
                })
        };
        bars.0 = left_bar.response.rect.width();

        let bottom_bar = egui::Panel::bottom("controls").show(root, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Undo").clicked() {
                    self.send_net(NetCommand::Undo);
                }
                if ui.button("Redo").clicked() {
                    self.send_net(NetCommand::Redo);
                }
                ui.separator();
                if ui.button("Fit").clicked() {
                    self.fit();
                }
                ui.separator();
                if let crate::panel::CaptureAction::ToggleArmed =
                    crate::panel::capture_toolbar(ui, self.capture_armed, &mut self.capture_note)
                {
                    self.capture_armed = !self.capture_armed;
                    self.capture_start = None;
                }
                ui.separator();
                // The overlay's collapse button (v1.4's overlay compact: 可折叠，键 `o`
                // 或按钮). Only drawn with `--debug-ui`: without it there is no overlay,
                // and a button that toggles an invisible state is worse than none.
                // `o` is the key that does the same, and the label names it so the
                // keyboard path is discoverable from the picture.
                if self.debug_ui {
                    let label = if self.overlay_collapsed {
                        "overlay: 1 行 (o)"
                    } else {
                        "overlay: 全展开 (o)"
                    };
                    if ui
                        .small_button(label)
                        .on_hover_text("折叠/展开 --debug-ui 叠层（键 o）")
                        .clicked()
                    {
                        self.overlay_collapsed = !self.overlay_collapsed;
                    }
                    ui.separator();
                }
                match self.model.selection.clone() {
                    Some(sel) => {
                        ui.label(format!("selected {sel}"));
                        if ui.small_button("deselect").clicked() {
                            self.model.selection = None;
                            self.ask_render();
                            self.window.request_redraw();
                        }
                    }
                    None => {
                        ui.label("click a prim · LMB orbit · RMB pan · wheel zoom · drag a selected prim to move it");
                    }
                }
                ui.separator();
                // 「只看选中 (i)」: one switch, and the label says the key so the
                // keyboard path is discoverable from the picture (the overlay's `o`
                // button sets the same precedent). Green while it is on, so whether
                // the viewport is filtered is visible without clicking anything.
                {
                    let text = if self.only_selected {
                        "只看选中 (i) ✓"
                    } else {
                        "只看选中 (i)"
                    };
                    let button = egui::Button::new(text).fill(if self.only_selected {
                        egui::Color32::from_rgb(40, 80, 60)
                    } else {
                        egui::Color32::TRANSPARENT
                    });
                    if ui
                        .add(button)
                        .on_hover_text("只画选中的 prim（含其子树）；无选中时画整场景")
                        .clicked()
                    {
                        self.toggle_only_selected();
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(format!("protocol {}", protocol::PROTOCOL));
                    if let Some(v) = self.model.version {
                        ui.label(format!("snapshot v{v}"));
                    }
                });
            });
        });
        bars.2 = bottom_bar.response.rect.height();

        egui::CentralPanel::no_frame().show(root, |ui| {
            let avail = ui.available_size();
            // The rect is allocated from the layout; its size, scale and texel size
            // come back from one source (`crate::viewport::ViewportSize`), which is
            // what the render thread is told to render at and what the camera is
            // fitted to. Nothing here re-derives a second size.
            let (rect, resp) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
            let size = crate::viewport::ViewportSize::new(rect, ui.pixels_per_point());
            self.publish_viewport(size);
            match &self.frame {
                Some((_frame, texture)) => {
                    // The frame was rendered at `size.texels()`; paint it into
                    // `size.rect()` with the whole texture, so one texel is one
                    // device pixel whatever the scale.
                    Self::viewport_image(*texture, size.rect().size(), size.uv())
                        .fit_to_exact_size(size.rect().size())
                        .paint_at(ui, size.rect());
                }
                None => {
                    ui.painter()
                        .rect_filled(rect, 0.0, egui::Color32::from_gray(24));
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "waiting for a stage_snapshot…",
                        egui::FontId::proportional(18.0),
                        egui::Color32::from_gray(170),
                    );
                }
            }
            self.handle_input(ui, &resp, rect);
            // The overlay's partition terms: the three bars recorded above and this
            // pass's rect, handed over before the overlay asks for them.
            self.bars = bars;
            if self.debug_ui {
                self.paint_debug_overlay(ui, rect);
            }
        });
        let _ = ctx;
    }

    /// Frame the scene again (the fitted camera, as the first snapshot built it).
    fn fit(&mut self) {
        let Some(bounds) = self.model.bbox else {
            return;
        };
        // Fit the window as it is now: "Fit" is the user asking for a fresh
        // framing, which is the one moment a resize is allowed to matter.
        let aspect = self.viewport_aspect();
        self.model.set_aspect(aspect);
        self.orbit = Some(Orbit::from_camera(&crate::scene::fit_camera(
            &crate::scene::BBox {
                min: bounds.0,
                max: bounds.1,
            },
            aspect,
        )));
        self.ask_render();
        self.window.request_redraw();
    }

    /// Record the laid-out viewport and, when its texel size changed, tell the
    /// render thread to render at exactly that.
    ///
    /// This is the join that was missing: the render size used to come from
    /// `window.inner_size()` (the whole window, including the title bar and the
    /// panels' height) while the image was painted into the viewport rect. The rect
    /// is smaller than the window and has a different aspect, so the frame was
    /// resampled into it — the city reduced to a magnified corner. Now both sides
    /// read `ViewportSize`: one rect, one scale, one texel size.
    fn publish_viewport(&mut self, size: crate::viewport::ViewportSize) {
        if size.texels() == self.viewport.texels() {
            self.viewport = size;
            return;
        }
        self.viewport = size;
        let (w, h) = size.texels();
        self.render.send(RenderCommand::Resize {
            width: w,
            height: h,
        });
    }

    /// The aspect the camera is fitted to: the **viewport rect's**, not the
    /// window's. The rect is what the picture is drawn into, so it is the shape the
    /// frame must be framed for.
    fn viewport_aspect(&self) -> f64 {
        self.viewport.aspect()
    }

    /// What a **layer identifier is called** on screen: the tree's one
    /// [`display name`](crate::tree::layer_display_name) when a tree is loaded, and the
    /// identifier less its `anon:N:` prefix when none is yet (before the first
    /// `composition_tree` answer there is no layer list to ask, and the prefix carries no
    /// identity anyway). Every place the shell prints a layer uses this -- the status
    /// bar's preview badge, the overlay's `composition` line -- so a badge cannot name a
    /// layer differently from the row the user clicked.
    fn layer_name(&self, id: &str) -> String {
        match self.tree.model.tree() {
            Some(tree) => tree.display_name(id),
            None => crate::tree::strip_anon_prefix(id),
        }
    }

    /// Every prim path the composition report lists, in the wire's own order.
    ///
    /// Which composition the layer the viewport is showing names, as the **tree** says:
    /// a cook prefix for an op or root layer, a solo for any other document layer, and
    /// `None` when the tree cannot tell (not arrived yet, or the id is not one of its
    /// layers — a badge then keeps the prefix's wording, which is what every pre-v1.8
    /// frame was).
    ///
    /// One reader for the status bar and the overlay, so the two cannot word the same frame
    /// differently; the rule itself is [`crate::panel::ask_for_layer`], the same one the row's
    /// click uses.
    fn preview_kind(&self) -> Option<crate::panel::LayerAsk> {
        let id = self.model.preview.as_deref()?;
        let tree = self.tree.model.tree()?;
        crate::panel::ask_for_layer(tree.layer(id)?)
    }

    /// Pointer input over the viewport: orbit, pan, zoom, pick, and the gizmo.
    ///
    /// In capture-selection mode a drag is not an orbit: it frames the region to
    /// capture, and the orbit waits until the mode is left. Taking the picture and
    /// moving the view at once would send an agent a mask over pixels that have
    /// since changed.
    fn handle_input(&mut self, ui: &egui::Ui, resp: &egui::Response, rect: egui::Rect) {
        if self.capture_armed {
            self.handle_capture_input(ui, resp, rect);
            return;
        }
        if resp.drag_started() {
            let Some(pos) = resp.interact_pointer_pos() else {
                return;
            };
            let secondary = ui.input(|i| i.pointer.secondary_down());
            // A press on the selected prim's own pixels is a gizmo drag; every
            // other press orbits (right button pans).
            let kind = if secondary {
                Drag::Pan
            } else if self.pixel_is_selected(pos, rect) {
                Drag::Gizmo
            } else {
                Drag::Orbit
            };
            if kind == Drag::Gizmo {
                if let Some(sel) = self.model.selection.clone()
                    && let Some(anchor) = self.anchor_for(&sel)
                {
                    self.gizmo = Some((sel, anchor));
                    self.throttle.reset();
                    self.preview = None;
                    self.pending = None;
                } else {
                    self.gizmo = None;
                }
            }
            self.drag = Some(DragState {
                kind,
                start: pos,
                last: pos,
            });
        }

        if let Some(drag) = self.drag.as_mut() {
            if resp.dragged() {
                let Some(pos) = resp.interact_pointer_pos() else {
                    return;
                };
                let (dx, dy) = (pos.x - drag.last.x, pos.y - drag.last.y);
                drag.last = pos;
                let start = drag.start;
                match drag.kind {
                    Drag::Orbit => {
                        if let Some(o) = self.orbit.as_mut() {
                            o.orbit(f64::from(dx), f64::from(dy), ORBIT_SENSITIVITY);
                        }
                        self.ask_render();
                        self.window.request_redraw();
                    }
                    Drag::Pan => {
                        if let Some(o) = self.orbit.as_mut() {
                            o.pan(f64::from(dx), f64::from(dy), f64::from(rect.height()));
                        }
                        self.ask_render();
                        self.window.request_redraw();
                    }
                    Drag::Gizmo => self.drag_gizmo(start, pos, rect),
                }
            }
            if resp.drag_stopped() {
                if self.drag.as_ref().is_some_and(|d| d.kind == Drag::Gizmo) {
                    self.flush_opinion(true);
                }
                self.drag = None;
                self.gizmo = None;
                self.preview = None;
                self.pending = None;
            }
        }

        if resp.clicked()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            self.click(pos, rect);
        }

        if resp.hovered() {
            let scroll = ui.input(|i| f64::from(i.smooth_scroll_delta().y) / 50.0);
            if scroll != 0.0 {
                if let Some(o) = self.orbit.as_mut() {
                    o.zoom(scroll);
                }
                self.ask_render();
                self.window.request_redraw();
            }
        }
    }

    /// A click: pick locally and, if the selection changed, re-render it.
    fn click(&mut self, pos: egui::Pos2, rect: egui::Rect) {
        let Some((w, h)) = self.frame_pixels(rect, pos) else {
            return;
        };
        let Some((frame, _)) = &self.frame else {
            return;
        };
        // **One texel, not a frame** (v1.10): the id buffer stays on the GPU and the
        // render thread answers with the pixel under the pointer.
        let Some(id) = self.render.pick(w, h) else {
            return;
        };
        let picked = if id == 0 {
            None
        } else {
            frame.paths.get((id - 1) as usize).cloned()
        };
        let before = self.model.selection.clone();
        self.model.selection = picked;
        let sel = self.model.selection.clone();
        if sel != before {
            self.status = self.model.status_line();
            self.ask_render();
            // v1.3: the tree no longer lists prims, so picking one in the viewport
            // is the way to inspect it — picked path → `prim_nodes[].id` →
            // `comp_node{id}`. A path the composition report does not describe is
            // ordinary (a flattened cook output has prims no layer spec authored),
            // so it is said plainly rather than treated as a failure.
            if let Some(path) = self.model.selection.clone() {
                // v1.3: picking a prim in the viewport copies its path, same as a tree
                // click. **The copy stays on this path and not in `select_path`**: the
                // control channel's `select{path}` reports the path in its answer
                // instead, and a script must not overwrite the desktop clipboard.
                self.copy_toast(path.clone());
                self.inspect_picked(&path);
            } else {
                // Clicking the background clears the selection; the detail panel
                // should stop describing the prim that was there.
                self.tree.detail = None;
                self.tree.detail_error = None;
                self.tree.selection = None;
            }
            self.window.request_redraw();
        }
    }

    /// Inspect the prim the user picked in the viewport: look its `comp_node` id up
    /// in the composition tree's `prim_nodes`, then ask for its detail.
    fn inspect_picked(&mut self, path: &str) {
        match self.tree.model.prim_id(path) {
            Some(id) => {
                let id = id.to_owned();
                self.tree.selection = Some(crate::tree::TreeSelection {
                    id: id.clone(),
                    kind: crate::tree::TreeNodeKind::Prim,
                    prim: Some(path.to_owned()),
                    depth: None,
                });
                self.ask_node(&id);
            }
            None => {
                // No `comp_node` node for this path. Say why, and leave the previous
                // detail cleared so the panel cannot be read as describing this prim.
                self.tree.detail = None;
                self.tree.selection = None;
                self.tree.detail_error = Some(if self.tree.model.is_unavailable() {
                    "合成树不可用：无法检视该 prim（server 版本）".to_owned()
                } else if self.tree.model.ready() {
                    format!("合成树未列出 {path}（可能来自 cook 展开），无法给出逐层 opinions")
                } else {
                    "合成树尚未到达：稍后重试".to_owned()
                });
            }
        }
    }

    /// The frame pixel a viewport point lands on.
    fn frame_pixels(&self, rect: egui::Rect, pos: egui::Pos2) -> Option<(u32, u32)> {
        let (frame, _) = self.frame.as_ref()?;
        let fx = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let fy = ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
        let x = (fx * frame.width as f32).min((frame.width - 1) as f32) as u32;
        let y = (fy * frame.height as f32).min((frame.height - 1) as f32) as u32;
        Some((x, y))
    }

    /// Selection-mode input: a drag frames a region, and letting go takes the
    /// capture. Nothing here moves the camera or picks a prim — the region is the
    /// whole output of the gesture.
    fn handle_capture_input(&mut self, ui: &egui::Ui, resp: &egui::Response, rect: egui::Rect) {
        if resp.drag_started()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            self.capture_start = Some(pos);
        }
        if let (Some(start), Some(pos)) = (self.capture_start, resp.interact_pointer_pos())
            && resp.dragged()
        {
            crate::panel::paint_capture_rect(ui, start, pos);
        }
        if resp.drag_stopped() {
            let start = self.capture_start.take();
            let end = resp.interact_pointer_pos();
            match (start, end) {
                (Some(s), Some(e)) => self.take_capture(s, e),
                _ => {
                    self.status = "capture: 选区取消".to_owned();
                }
            }
        }
        // A single click is not a region; say so rather than capturing a pixel the
        // user did not mean to pick.
        if resp.clicked() {
            self.status = "capture: 拖一个矩形（不是点）".to_owned();
        }
        let _ = rect;
    }

    /// Whether a viewport point lands on the current selection's pixels.
    ///
    /// It asks the GPU for **one texel** (v1.11): a frame carries no id buffer any more
    /// (v1.10 stopped copying it back), so this used to index an empty `Vec` — a press on
    /// a selected prim, which is exactly the press that starts a drag, panicked the
    /// window. The `Pick` round trip is the same one a click makes, so both agree about
    /// what is under the pointer.
    fn pixel_is_selected(&self, pos: egui::Pos2, rect: egui::Rect) -> bool {
        let Some(sel) = self.model.selection.clone() else {
            return false;
        };
        let Some((x, y)) = self.frame_pixels(rect, pos) else {
            return false;
        };
        let Some((frame, _)) = &self.frame else {
            return false;
        };
        let Some(id) = self.render.pick(x, y) else {
            return false;
        };
        id != 0 && frame.paths.get((id - 1) as usize) == Some(&sel)
    }

    /// One gizmo tick: the pointer's hit on the plane through the prim's centre
    /// becomes a **world** delta, which is converted to the prim's **local**
    /// `xformOp:translate` (`crate::gizmo`) before it is drawn or sent.
    ///
    /// The conversion is what the ruling puts on this side: the server writes an
    /// opinion's value as the field's op, so a world value would double-apply a
    /// translated (or rotated, or scaled) parent. Both the local preview and the
    /// wire carry the **local** number, so what the user sees and what the server
    /// authors are the same value.
    fn drag_gizmo(&mut self, start: egui::Pos2, pos: egui::Pos2, rect: egui::Rect) {
        let (Some((sel, anchor)), Some(orbit)) = (self.gizmo.clone(), self.orbit) else {
            return;
        };
        let (w, h) = (f64::from(rect.width()), f64::from(rect.height()));
        let plane_normal = [0.0, 0.0, 1.0];
        let origin = anchor.world_center;

        let ray_at = |p: egui::Pos2| {
            orbit.ray(
                f64::from(p.x - rect.left()),
                f64::from(p.y - rect.top()),
                w,
                h,
                self.bounds,
            )
        };
        let Some(press_hit) = plane_hit(ray_at(start), origin, plane_normal) else {
            return;
        };
        let Some(hit) = plane_hit(ray_at(pos), origin, plane_normal) else {
            return;
        };
        let delta_world = [hit[0] - press_hit[0], hit[1] - press_hit[1], 0.0];
        // World target → the prim's own local op value. The delta form keeps a
        // prim whose visual centre is not its local origin from jumping at press.
        let Some(new_local) =
            crate::gizmo::drag_translate(anchor.parent_world, anchor.local_translate, delta_world)
        else {
            self.status = format!("drag {sel}: the parent transform is not invertible");
            return;
        };

        // Preview: one prim, one re-bake, so the pointer never outruns the image.
        if self.preview != Some(new_local) {
            self.preview = Some(new_local);
            let mut overrides = BTreeMap::new();
            overrides.insert(sel.clone(), new_local);
            self.render.send(RenderCommand::Preview {
                overrides,
                selection: Some(sel.clone()),
            });
            self.window.request_redraw();
        }

        self.pending = Some(new_local);
        self.status = format!(
            "drag {sel} → local ({:.2}, {:.2}, {:.2})",
            new_local[0], new_local[1], new_local[2]
        );
        if self.throttle.ready(Instant::now()) {
            self.flush_opinion(false);
        }
    }

    /// Draw the `--debug-ui` overlay in the viewport's top-left corner.
    ///
    /// Plain shapes and text, no widgets: it must not take clicks from the
    /// viewport, and it must be readable in a screenshot. The text comes from
    /// [`crate::viewport::OverlayReport::lines`] so what a test asserts and what a
    /// screenshot shows are the same strings.
    ///
    /// **Size lives in [`crate::overlay`], not here** (v1.4's overlay compact): this
    /// function measures, wraps and draws, and the font, width and area bounds are the
    /// module's constants, so the box a screenshot shows and the box a test asserts
    /// cannot drift apart. Collapsing (`o`, or the tree panel's button) is one row.
    fn paint_debug_overlay(&self, ui: &egui::Ui, rect: egui::Rect) {
        let vp = self.viewport;
        let frame_texels = self
            .frame
            .as_ref()
            .map(|(f, _)| (f.width, f.height))
            .unwrap_or((0, 0));
        let orbit = self.orbit;
        // `produced` counts what the render thread made, `shown` what this window
        // drew; `drawn_*` is the camera of the frame in hand — the one the pixels on
        // screen were actually taken with.
        let drawn = self.frame.as_ref().map(|(f, _)| f.camera);
        let report = crate::viewport::OverlayReport {
            pixels_per_point: vp.pixels_per_point(),
            rect_points: (vp.rect().width(), vp.rect().height()),
            viewport_texels: vp.texels(),
            frame_texels,
            texels_match: frame_texels.0 > 0 && vp.is_pixel_exact(frame_texels),
            frames_produced: self.render.produced_seq(),
            // Only one texture is ever on screen, so the frame in hand is the one
            // drawn this pass `drain_frames` ran before `paint`.
            frames_shown: self.frames_received,
            orbit_distance: orbit.map_or(0.0, |o| o.distance),
            orbit_target: orbit.map_or([0.0; 3], |o| o.target),
            drawn_distance: drawn.map(|c| crate::camera::Orbit::from_camera(&c).distance),
            drawn_target: drawn.map(|c| c.look_at),
        };
        let mut lines = report.lines();
        // The composition the viewport is showing (v1.4's preview), first line: it is
        // the one fact a prefix preview changes, and a screenshot of a prefix must say
        // so without being compared against another screenshot.
        lines.insert(
            0,
            format!(
                "composition {}",
                self.model
                    .preview_label_kind(self.preview_kind(), |id| self.layer_name(id))
            ),
        );
        // The two groups that name a DPI defect as a number rather than a story:
        // what the platform said against what the contract expects, and whether the
        // panels and the viewport partition the window. A screenshot of a settled
        // window then answers both questions on its own.
        lines.extend(self.screen.lines());
        if let Some(fix) = self.presenter_fix_line() {
            lines.push(fix);
        }
        lines.extend(
            crate::viewport::Partition {
                // The **authority**: the OS's `GetClientRect / dpi`, falling back to
                // winit's division only where no probe exists. Round 10 built this
                // from `expected_points()` — winit's own numbers — so the identity held
                // between two agreeing conspirators while the viewport reached past the
                // screen edge.
                window: self.screen.authoritative_points(),
                left: self.bars.0,
                top: self.bars.1,
                bottom: self.bars.2,
                rect: vp.rect(),
            }
            .lines(),
        );
        let painter = ui.painter().clone();
        let font = egui::FontId::monospace(crate::overlay::FONT_PT);
        // The wrap width comes from the **viewport** (`overlay::row_width`), the same
        // number the box is sized by: a fixed 460 pt wrap on a 300 pt viewport would
        // draw rows wider than the box they sit in.
        let max_row = crate::overlay::row_width(rect);
        // Wrap every line to the box's width, then measure the rows. `layout_no_wrap`
        // is the measurement the wrap rule itself uses, so what fits is what is drawn.
        let rows: Vec<String> = lines
            .iter()
            .flat_map(|l| {
                crate::overlay::wrap(l, max_row, |s| {
                    painter
                        .layout_no_wrap(s.to_owned(), font.clone(), egui::Color32::WHITE)
                        .size()
                        .x
                })
            })
            .collect();
        let measured: Vec<egui::Vec2> = rows
            .iter()
            .map(|r| {
                painter
                    .layout_no_wrap(r.clone(), font.clone(), egui::Color32::WHITE)
                    .size()
            })
            .collect();
        let plan = crate::overlay::plan(rect, &measured, self.overlay_collapsed);
        if plan.rect == egui::Rect::NOTHING {
            return;
        }
        let bg = plan.rect.translate(rect.min.to_vec2());
        painter.rect_filled(bg, 3.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, 190));
        let draw = |row: usize, text: &str| {
            // `1:1 NO` and `fresh NO` are failures: colour them so a screenshot
            // cannot be mistaken for a healthy one. Rows that are part of a wrapped
            // line inherit their line's colour, which is why the check is on the
            // row's own text.
            let colour = if text.contains("NO") {
                egui::Color32::from_rgb(255, 120, 120)
            } else {
                egui::Color32::from_rgb(200, 240, 200)
            };
            painter.text(
                bg.min + egui::vec2(crate::overlay::PAD_PT, crate::overlay::row_y(row)),
                egui::Align2::LEFT_TOP,
                text.to_owned(),
                font.clone(),
                colour,
            );
        };
        for r in plan.rows() {
            match r.input {
                Some(idx) => draw(r.row, &rows[idx]),
                None => draw(
                    r.row,
                    &crate::overlay::marker_text(plan.hidden, plan.collapsed),
                ),
            }
        }
    }

    /// Send the drag's newest **local** translation (throttled samples, and the
    /// final one which is never throttled away).
    fn flush_opinion(&mut self, final_sample: bool) {
        let Some(t) = self.pending else { return };
        if !final_sample && t == self.last_sent() {
            return;
        }
        let Some((sel, _)) = self.gizmo.clone() else {
            return;
        };
        self.pending = None;
        self.record_sent(t);
        self.send_net(NetCommand::Opinion(vec![OpinionOp::set_translate_local(
            sel, t,
        )]));
    }

    fn last_sent(&self) -> [f64; 3] {
        self.sent.unwrap_or([f64::NAN; 3])
    }

    fn record_sent(&mut self, t: [f64; 3]) {
        self.sent = Some(t);
    }

    fn send_net(&mut self, cmd: NetCommand) {
        self.net.send(cmd);
        self.window.request_redraw();
    }

    // --- paint ------------------------------------------------------------

    /// Wake the UI and the GPU: a redraw now, and egui's own repaint request.
    ///
    /// Both halves are needed. `request_redraw` gets a frame drawn; without egui's
    /// `request_repaint` an immediate-mode UI only redraws when the event loop
    /// happens to wake, which is what left the window at a handful of fps between
    /// events while a snapshot was waiting to be shown. This is called on every
    /// scene, notification and input-driven change, so a fresh snapshot is shown
    /// on the next frame and an idle viewer simply sits still.
    fn request_repaint(&mut self) {
        self.egui_ctx.request_repaint();
        self.window.request_redraw();
    }

    /// Measure the window's screen space and, when the platform's own numbers
    /// disagree with the one equation an egui layout relies on, put the right point
    /// size into the pass **before** the layout runs.
    ///
    /// `screen_points == inner_physical / scale_factor`, once — that is the whole
    /// contract, and [`crate::viewport::ScreenSpace::agreement`] names which way it
    /// is broken when it is (`pixels as points`, `scaled twice`, or a ratio that is
    /// not a whole scale and which this therefore does not touch).
    ///
    /// The correction is deliberately **conditional**. Both states this round
    /// measured (`2240×1216 / 1.75 = 1280.0` and `2560×1272 / 1.75 = 1462.9`) are
    /// the correct row, so an unconditional override would have rewritten a layout
    /// that was already right and hidden the real defect behind its own arithmetic.
    /// When the ratio is `1.000` this method changes nothing but the record.
    ///
    /// It also refreshes the presenter's back buffer to the window's size, which is
    /// the other half of the same class of defect and is described on
    /// [`Shell::presented`].
    fn correct_screen_space(&mut self, mut raw: egui::RawInput) -> egui::RawInput {
        let inner = self.window.inner_size();
        let scale = self.window.scale_factor() as f32;
        // What egui-winit put in the pass, so the ratio is measured rather than
        // assumed. The `None` case is a zero-sized window (minimized); there is
        // nothing to correct then, and egui skips the pass itself.
        let screen_points = raw.screen_rect.map_or(egui::Vec2::ZERO, |r| r.size());
        // Built with the size the pass **inherited**; `sync_presenter` below may
        // change `self.presented`, which leaves this record as the defect to report.
        //
        // `os` is the second opinion: `inner_size()` **is** `GetClientRect`, so a
        // comparison among winit's own numbers proves only that winit agrees with
        // itself, which is exactly what round 10's overlay did while the viewport was
        // wider than the screen.
        self.screen = crate::viewport::ScreenSpace {
            inner_physical: (inner.width, inner.height),
            scale_factor: scale,
            screen_points,
            presented_physical: self.presented,
            os: crate::platform_metrics::probe(&self.window).and_then(|m| {
                Some(crate::viewport::OsView {
                    client_px: m.client_px?,
                    scale: m.os_scale()?,
                    window_px: m.window_px?,
                    awareness: m.awareness.unwrap_or("n/a"),
                    monitor_px: m.monitor_px.map(|(_, _, w, h)| (w, h)),
                })
            }),
        };
        if let Some(want) = self.screen.correction() {
            // The rect egui lays out in, replaced by the size the window's own
            // numbers imply. `raw.screen_rect` is the whole input: no offset, no
            // clip rect, so this is the one field a correction may touch.
            raw.screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, want));
        }
        // The scale has to be corrected **with** the rect, not after it: egui derives
        // `pixels_per_point = zoom_factor * native_pixels_per_point`, and the presenter
        // is handed that same product. Correcting the point space against the OS while
        // leaving the scale at winit's would make the layout agree with the OS and the
        // rendering disagree with the layout — a `texels != rect x ppp` mismatch, which
        // is the class of defect round 9 already had to unpick. One authority for both.
        if let Some(os) = self.screen.os {
            raw.viewports
                .entry(egui::ViewportId::ROOT)
                .or_default()
                .native_pixels_per_point = Some(os.scale);
        }
        self.sync_presenter(inner);
        raw
    }

    /// Keep the presenter's swapchain the size of the window it presents.
    ///
    /// `egui_wgpu::winit::Painter` owns the window's back buffer and reconfigures it
    /// only when told: `set_window` once, then `on_window_resized` per resize. This
    /// shell called neither after startup, so the swapchain stayed at the size the
    /// window was created with; a later window size was presented by the compositor
    /// **stretching** that back buffer (`DXGI_SCALING_STRETCH`, the only scaling
    /// mode `wgpu-hal` requests on DX12), which magnifies everything drawn into it —
    /// panels included — by `new / old`. egui-winit's own integrations call this
    /// from the same `Resized` arm that this one skipped.
    ///
    /// Done here rather than in the event arm so the size is taken once, from the
    /// window as it is at draw time, and so a scale change (which arrives as a
    /// `ScaleFactorChanged` and a `Resized` together) cannot leave the two disagreeing.
    fn sync_presenter(&mut self, inner: winit::dpi::PhysicalSize<u32>) {
        if (inner.width, inner.height) == self.presented {
            return;
        }
        let (Some(w), Some(h)) = (NonZeroU32::new(inner.width), NonZeroU32::new(inner.height))
        else {
            // A zero-sized window (minimized): the old back buffer is the correct
            // thing to keep, and the next real size will sync it.
            return;
        };
        self.painter.on_window_resized(egui::ViewportId::ROOT, w, h);
        self.presented = (inner.width, inner.height);
    }

    /// The overlay line for a swapchain this pass **repaired**, or nothing.
    ///
    /// `self.screen.presented_physical` is deliberately the size from *before* the
    /// sync, so [`crate::viewport::ScreenSpace::lines`] reports the defect the pass
    /// inherited rather than the size it just installed. This adds the one thing that
    /// line cannot say: that the pass also fixed it, so a screenshot of the repairing
    /// frame reads "stale, and resized this pass" instead of looking like a live fault.
    fn presenter_fix_line(&self) -> Option<String> {
        let was = self.screen.presented_physical;
        (was != self.presented).then(|| {
            format!(
                "swapchain resized this pass {}x{} → {}x{} px",
                was.0, was.1, self.presented.0, self.presented.1
            )
        })
    }

    fn paint(&mut self) {
        self.poll_local_open();
        let raw = self.egui_state.take_egui_input(&self.window);
        let raw = self.correct_screen_space(raw);
        let ctx = self.egui_ctx.clone();
        let full = ctx.run_ui(raw, |root| self.ui(root));
        // A copy asked for during the pass goes out with the pass's own platform
        // output; doing it here — after the widgets ran — is what makes one pass's
        // click and its clipboard write the same frame.
        self.flush_copy(&ctx);
        self.egui_state
            .handle_platform_output(&self.window, full.platform_output);
        let clipped = ctx.tessellate(full.shapes, full.pixels_per_point);
        let size = self.window.inner_size();
        let _screen = ScreenDescriptor {
            size_in_pixels: [size.width, size.height],
            pixels_per_point: full.pixels_per_point,
        };
        let mut textures = full.textures_delta;
        self.painter.paint_and_update_textures(
            egui::ViewportId::ROOT,
            full.pixels_per_point,
            [0.02, 0.02, 0.03, 1.0],
            &clipped,
            &mut textures,
            vec![],
            &self.window,
        );
        // **A pass that applied nothing must not panic on the way out** (v1.9).
        // `epaint::TexturesDelta` asserts on drop that every delta was applied, and a
        // pass whose surface is not presentable (the window occluded, the swapchain lost,
        // the first frame still warming up) returns from `paint_and_update_textures`
        // without taking them. The panic then killed the whole viewer — from the outside,
        // a script's next call found the control port refused and every row after it went
        // unmeasured. Clearing what the pass did not take is what the assertion asks for.
        textures.clear();
    }
}

fn join_diags(diags: &[rusd_core::Diagnostic]) -> String {
    diags
        .iter()
        .map(|d| format!("{}: {}", d.code, d.message))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The winit application: builds the shell on `resumed`, then forwards events.
pub struct App {
    args: ViewArgs,
    shell: Option<Shell>,
    proxy: EventLoopProxy<AppEvent>,
    /// The startup milestones: marks the shell made, and the file they go to.
    startup: Startup,
    /// Whether the first frame has been marked (`FIRST_PAINT`).
    painted: bool,
    /// The control channel's receiving end, once opened (`--control`).
    ///
    /// Held here rather than in the shell because the socket must exist **before** the
    /// window does: a script started the viewer and began asking, and a request that
    /// arrived while the GPU was still warming up used to have nowhere to go. The shell
    /// receives it from `resumed` ([`ControlWiring`]), so from the first pass on it is the
    /// shell that drains it.
    control: Option<crate::control::Queue>,
}

impl App {
    pub fn new(args: ViewArgs, proxy: EventLoopProxy<AppEvent>, startup: Startup) -> Self {
        Self {
            args,
            shell: None,
            proxy,
            startup,
            painted: false,
            control: None,
        }
    }

    /// Open the control channel, when `--control` asked for one.
    ///
    /// Called once, before `EventLoop::run_app`: an address that cannot be bound is the
    /// caller's to report, and a viewer that refused it prints one line and exits rather
    /// than opening a window nobody can drive. **Without the flag this does nothing at
    /// all** — no socket, no thread, nothing listening.
    ///
    /// The nudge handed to the listener threads is the same `EventLoopProxy` the network
    /// and render workers use, so a request wakes the window thread exactly as a frame or
    /// a notification does; nothing polls.
    pub fn open_control(&mut self, wake: crate::control::Nudge) -> Result<(), String> {
        let Some(addr) = self.args.control.clone() else {
            return Ok(());
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let control = crate::control::spawn(&addr, tx, wake)?;
        eprintln!(
            "{{\"code\":\"N_CONTROL\",\"message\":\"control channel on {}\"}}",
            control.addr
        );
        self.args.control_addr = Some(control.addr);
        self.control = Some(rx);
        Ok(())
    }
}

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.shell.is_some() {
            return;
        }
        let attrs = match startup_window_attrs(event_loop) {
            Ok(a) => a,
            Err(msg) => {
                eprintln!("{{\"code\":\"E_WINDOW\",\"message\":\"{msg}\"}}");
                event_loop.exit();
                return;
            }
        };
        let window = match event_loop.create_window(attrs) {
            Ok(w) => {
                self.startup.mark(startup::WINDOW_CREATED);
                Arc::new(w)
            }
            Err(e) => {
                eprintln!("{{\"code\":\"E_WINDOW\",\"message\":\"{e}\"}}");
                event_loop.exit();
                return;
            }
        };
        let wiring = ControlWiring {
            queue: self.control.take(),
            addr: self.args.control_addr,
        };
        match Shell::new(
            window,
            &self.args,
            self.proxy.clone(),
            &mut self.startup,
            wiring,
        ) {
            Ok(shell) => {
                shell.window.request_redraw();
                self.shell = Some(shell);
            }
            Err(e) => {
                eprintln!("{{\"code\":\"E_GPU_INIT\",\"message\":\"{e}\"}}");
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: AppEvent) {
        if let Some(shell) = self.shell.as_mut() {
            // Control requests first: a script that asked for something sees the effect of
            // it in the frame this event's redraw paints, and `drain_net` below may then
            // answer an earlier request's waiter with a reply that has just arrived.
            shell.drain_control();
            shell.drain_net();
            shell.drain_frames();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(shell) = self.shell.as_mut() else {
            return;
        };
        match &event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
                return;
            }
            WindowEvent::Resized(size) => {
                // **No render resize here.** The renderer's framebuffer is the
                // *viewport rect's* device pixels, not the window's — the rect is
                // smaller (the status bar and controls take their height) and has a
                // different aspect. Sizing it from the window here was the second
                // source of the size, and it is what made a correctly framed frame
                // land in the wrong shape. The next layout pass publishes the right
                // size through `Shell::publish_viewport`.
                if size.width > 0 && size.height > 0 {
                    shell.window.request_redraw();
                }
                return;
            }
            WindowEvent::KeyboardInput {
                event:
                    winit::event::KeyEvent {
                        state: ElementState::Pressed,
                        logical_key,
                        ..
                    },
                ..
            } => {
                use winit::keyboard::{Key, NamedKey};
                match logical_key {
                    Key::Named(NamedKey::Escape) => {
                        event_loop.exit();
                        return;
                    }
                    Key::Character(c) if c.as_str() == "z" => shell.send_net(NetCommand::Undo),
                    Key::Character(c) if c.as_str() == "y" => shell.send_net(NetCommand::Redo),
                    Key::Character(c) if c.as_str() == "f" => shell.fit(),
                    // `i` is 「只看选中」: the viewport draws only the selected prim's
                    // subtree. It flips the lens and nothing else -- no request, no
                    // version, and the camera stays where the user put it.
                    Key::Character(c) if c.as_str() == "i" => shell.toggle_only_selected(),
                    // `o` collapses/expands the `--debug-ui` overlay (v1.4's overlay
                    // compact). It does nothing without `--debug-ui`: the overlay is
                    // not drawn then, so a toggle would be a key that silently
                    // changes an invisible state.
                    Key::Character(c) if c.as_str() == "o" && shell.debug_ui => {
                        shell.overlay_collapsed = !shell.overlay_collapsed;
                        shell.window.request_redraw();
                    }
                    _ => {}
                }
            }
            WindowEvent::RedrawRequested => {
                shell.drain_control();
                shell.drain_net();
                shell.drain_frames();
                shell.paint();
                // The first frame is the marker that the window is usable: a banner
                // that sees `shell_built` but never `first_paint` was built and never
                // drawn, which is exactly the state the silent startup was stuck in.
                if !self.painted {
                    self.painted = true;
                    self.startup.mark(startup::FIRST_PAINT);
                }
                return;
            }
            _ => {}
        }
        let _ = shell.egui_state.on_window_event(&shell.window, &event);
        shell.window.request_redraw();
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.shell = None;
    }
}

/// Run the viewer window: 0 on a clean exit, 1 if the window or GPU failed.
///
/// Every failure prints one `{"code":…,"message":…}` line on stderr before
/// returning, so a caller (a tunnel session, a test harness) sees why the window
/// never appeared instead of a bare exit code. The one case this cannot report
/// is an environment with no window station at all: on Windows, winit aborts the
/// process while building the event loop (session 0 / non-interactive service
/// sessions do this), which no `Result` here can intercept.
pub fn run(args: ViewArgs) -> i32 {
    let mut startup = Startup::from_env();
    let event_loop = match EventLoop::<AppEvent>::with_user_event().build() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{{\"code\":\"E_EVENT_LOOP\",\"message\":\"{e}\"}}");
            return 1;
        }
    };
    startup.mark(startup::EVENT_LOOP);
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = App::new(args, proxy, startup);
    // **The control channel opens before the window does.** A script starts the viewer
    // and asks at once; a socket created in `resumed` would refuse those first requests
    // while the GPU warms up. A bind that fails ends the run here, with one line, rather
    // than leaving a window up that nothing can drive.
    {
        let wake_proxy = app.proxy.clone();
        let wake = crate::control::nudge(move || {
            let _ = wake_proxy.send_event(AppEvent::Wake);
        });
        if let Err(e) = app.open_control(wake) {
            eprintln!("{{\"code\":\"E_CONTROL\",\"message\":\"{e}\"}}");
            return 2;
        }
    }
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("{{\"code\":\"E_EVENT_LOOP\",\"message\":\"{e}\"}}");
        return 1;
    }
    0
}
