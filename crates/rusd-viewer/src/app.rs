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
use std::sync::Arc;
use std::time::Instant;

use egui_wgpu::ScreenDescriptor;
use rusd_render::annotate::Style;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::anchor::Anchor;
use crate::camera::{Orbit, plane_hit};
use crate::net::{NetCommand, NetEvent, NetWorker, RenderCommand, RenderWorker};
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
}

impl ViewArgs {
    /// Parse `--connect <addr:port> [--usdc <path>]`, with `--flag=value` also
    /// accepted.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut connect = None;
        let mut usdc = None;
        let mut debug_ui = false;
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
                "--debug-ui" => debug_ui = true,
                other => return Err(format!("unexpected argument {other:?}")),
            }
        }
        let connect = connect.ok_or_else(|| "--connect <addr:port> is required".to_owned())?;
        Ok(Self {
            connect,
            usdc,
            debug_ui,
        })
    }
}

/// Radians of orbit per point of drag.
const ORBIT_SENSITIVITY: f64 = 0.006;
/// A gizmo drag sends at most this often (`docs/PROTOCOL.md`: ≤ 30 Hz).
const GIZMO_HZ: f64 = 30.0;

/// The tags a `NetCommand::Call` is answered under, so [`Shell::on_reply`] knows
/// which of its own requests a reply belongs to.
const TREE_TAG: &str = "composition_tree";
const NODE_TAG: &str = "comp_node";
const CAPTURE_TAG: &str = "capture";

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

    /// The frame on screen, with its texture.
    frame: Option<(crate::net::Frame, egui::TextureHandle)>,
    /// The render size the texture was made at, so a resize forces a new one.
    tex_size: (u32, u32),
    /// The viewport's geometry — the one place its rect, scale and texel size come
    /// from (`crate::viewport::ViewportSize`). Updated by the layout each frame;
    /// the render thread is told only when the texel size changes.
    viewport: crate::viewport::ViewportSize,
    /// A frame counter for the debug overlay: how many frames the render thread has
    /// handed over, and which one is on screen. A stale window shows a smaller
    /// `shown` than `received`, which is what makes "displaying an old frame"
    /// visible in a screenshot instead of a guess.
    frames_received: u64,
    /// Whether `--debug-ui` draws the size/frame overlay.
    debug_ui: bool,

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

    status: String,
    fps: f64,
    last_frame_at: Instant,
    offline_usdc: Option<String>,

    /// The composition-tree panel (`docs/PROTOCOL.md` v1.2).
    tree: crate::panel::TreePanel,
    /// Whether `composition_tree{}` has been asked for this connection: asked once
    /// when the first snapshot arrives, and again on 「刷新」.
    tree_asked: bool,

    /// Whether the capture toolbar's selection mode is armed.
    capture_armed: bool,
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

        let style = Style::default();
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
            style,
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
            // A placeholder until the first layout: the whole window as the rect.
            // It is replaced by `publish_viewport` before anything is drawn, and the
            // render thread's first `Resize` comes from there.
            viewport: crate::viewport::whole_window(window_points, scale),
            frames_received: 0,
            tex_size: (0, 0),
            drag: None,
            gizmo: None,
            pending: None,
            throttle: protocol::Throttle::hz(GIZMO_HZ),
            sent: None,
            preview: None,
            status: format!("connecting to {}", args.connect),
            fps: 0.0,
            last_frame_at: Instant::now(),
            offline_usdc: args.usdc.clone(),
            debug_ui: args.debug_ui,
            tree: crate::panel::TreePanel::new(),
            tree_asked: false,
            capture_armed: false,
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
                NetEvent::Live { addr: _ } => {
                    // `hello` + `subscribe` succeeded: the connection is what the
                    // status bar must now say. Without this the view kept
                    // claiming "connecting" while snapshots were arriving.
                    self.model.mark_live();
                    self.status = self.model.status_line_zh();
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
            Effect::Status => {
                self.status = self.model.status_line_zh();
                self.request_repaint();
            }
            Effect::Nothing => {}
            Effect::Failed(diags) => {
                self.status = join_diags(&diags);
                self.request_repaint();
            }
        }
    }

    /// A new scene: the **first one of a connection** frames the camera to this
    /// window's aspect (`ClientModel::wants_first_fit`), later ones keep the user's
    /// orbit; the render thread receives the geometry alone (the stage stays here,
    /// `!Send`), and a drag's local preview is dropped (the server's answer replaces
    /// it).
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
        self.status = self.model.status_line_zh();
        self.preview = None;
        self.gizmo = None;
        // The first scene of a session is when `composition_tree{}` becomes
        // answerable (the server needs an open stage). Asked once; 「刷新」 asks again.
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

    /// Ask `composition_tree{}` when the session becomes live and there is a scene.
    ///
    /// The tree is built from the server's authority stack, so it only exists once
    /// the server has opened something; asking before that would only earn a "no
    /// scene open". `tree_asked` keeps it to one request per connection (plus each
    /// 「刷新」), not one per snapshot: the tree is a structural read, and a
    /// re-composite after an opinion leaves the layers unchanged.
    fn ask_tree(&mut self) {
        if self.model.version.is_none() {
            return;
        }
        self.tree_asked = true;
        self.tree.model = crate::tree::TreeModel::Loading;
        self.net.send(NetCommand::Call {
            tag: TREE_TAG.to_owned(),
            method: "composition_tree".to_owned(),
            params: crate::tree::tree_params(),
        });
    }

    /// Ask `comp_node{id}` for a clicked row.
    fn ask_node(&mut self, id: &str) {
        self.net.send(NetCommand::Call {
            tag: NODE_TAG.to_owned(),
            method: "comp_node".to_owned(),
            params: crate::tree::comp_node_params(id),
        });
    }

    /// A `NetCommand::Call` was answered: route it by its tag.
    fn on_reply(&mut self, tag: String, result: Result<serde_json::Value, protocol::RpcError>) {
        let reply = match result {
            Ok(v) => Ok(v),
            Err(e) => Err((e.code, e.message)),
        };
        match tag.as_str() {
            TREE_TAG => {
                self.tree.apply_tree(reply);
                self.status = self.tree.model.status_zh();
            }
            NODE_TAG => self.tree.apply_node(reply),
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
    fn on_tree_select(&mut self, sel: crate::tree::TreeSelection) {
        self.ask_node(&sel.id);
        if let Some(path) = crate::panel::copy_payload(&sel) {
            self.model.selection = Some(path.clone());
            self.copy_toast(path);
            self.preview = None;
            self.gizmo = None;
            self.ask_render();
        }
        self.tree.selection = Some(sel);
        self.request_repaint();
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
        let Some(sel) = crate::panel::mask_rect(self.viewport.rect(), start, end, frame_size)
        else {
            self.status = "capture: 选区为空".to_owned();
            return;
        };
        let image_png = match crate::capture::encode_png(frame.width, frame.height, &frame.beauty) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.status = format!("capture: 编码 PNG 失败：{e}");
                return;
            }
        };
        let mask = crate::capture::mask_rgba(frame_size, sel);
        let mask_png = match crate::capture::encode_png(frame.width, frame.height, &mask) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.status = format!("capture: 编码 mask 失败：{e}");
                return;
            }
        };
        let camera = crate::capture::camera_json_of(&frame.camera);
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
            frame.width,
            frame.height,
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

        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.beauty,
        );
        let texture = self
            .egui_ctx
            .load_texture("viewport", image, egui::TextureOptions::LINEAR);
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

    /// Ask the render thread for a frame with the current camera and selection.
    fn ask_render(&mut self) {
        if let Some(orbit) = self.orbit {
            self.render.send(RenderCommand::View {
                orbit,
                selection: self.model.selection.clone(),
            });
        }
    }

    // --- UI ---------------------------------------------------------------

    fn ui(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        egui::Panel::top("status").show(root, |ui| {
            ui.horizontal(|ui| {
                ui.strong("rusd-viewer");
                ui.separator();
                // The Chinese summary is the primary line: it is what makes the
                // installed CJK face visibly right (a missing glyph shows as a
                // box right here), and it is the one a user reads. The detailed
                // ASCII line follows for diagnostics and for a viewer whose
                // font did not load.
                ui.label(self.model.status_line_zh());
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

        egui::Panel::left("composition-tree")
            .default_size(280.0)
            .min_size(180.0)
            .show(root, |ui| {
                match crate::panel::tree_panel(ui, &self.tree) {
                    crate::panel::TreeAction::Select(sel) => self.on_tree_select(sel),
                    crate::panel::TreeAction::Refresh => self.ask_tree(),
                    crate::panel::TreeAction::None => {}
                }
                ui.separator();
                crate::panel::detail_panel(ui, &self.tree);
            });

        egui::Panel::bottom("controls").show(root, |ui| {
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
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(format!("protocol {}", protocol::PROTOCOL));
                    if let Some(v) = self.model.version {
                        ui.label(format!("snapshot v{v}"));
                    }
                });
            });
        });

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
                    egui::Image::new(texture)
                        .fit_to_exact_size(size.rect().size())
                        .uv(size.uv())
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
        let before = self.model.selection.clone();
        let sel = self
            .model
            .pick_with(&frame.ids, &frame.paths, frame.width, frame.height, w, h);
        if sel != before {
            self.status = self.model.status_line();
            self.ask_render();
            self.window.request_redraw();
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
    fn pixel_is_selected(&self, pos: egui::Pos2, rect: egui::Rect) -> bool {
        let (Some(sel), Some((x, y))) =
            (self.model.selection.clone(), self.frame_pixels(rect, pos))
        else {
            return false;
        };
        let Some((frame, _)) = &self.frame else {
            return false;
        };
        let id = frame.ids[(y * frame.width + x) as usize];
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
        let lines = report.lines();
        let font = egui::FontId::monospace(12.0);
        let pad = 6.0;
        let painter = ui.painter().clone();
        let galleys: Vec<_> = lines
            .iter()
            .map(|l| painter.layout_no_wrap(l.clone(), font.clone(), egui::Color32::WHITE))
            .collect();
        let line_h = galleys.first().map(|g| g.size().y).unwrap_or(14.0).max(1.0) + 2.0;
        let width = galleys.iter().map(|g| g.size().x).fold(0.0f32, f32::max) + pad * 2.0;
        let height = line_h * lines.len() as f32 + pad * 2.0;
        let bg =
            egui::Rect::from_min_size(rect.min + egui::vec2(8.0, 8.0), egui::vec2(width, height));
        painter.rect_filled(bg, 3.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, 190));
        for (i, line) in lines.iter().enumerate() {
            // `1:1 NO` and `fresh NO` are failures: colour them so a screenshot
            // cannot be mistaken for a healthy one.
            let colour = if line.contains("NO") {
                egui::Color32::from_rgb(255, 120, 120)
            } else {
                egui::Color32::from_rgb(200, 240, 200)
            };
            painter.text(
                bg.min + egui::vec2(pad, pad + line_h * i as f32),
                egui::Align2::LEFT_TOP,
                line.clone(),
                font.clone(),
                colour,
            );
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

    fn paint(&mut self) {
        let raw = self.egui_state.take_egui_input(&self.window);
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
}

impl App {
    pub fn new(args: ViewArgs, proxy: EventLoopProxy<AppEvent>, startup: Startup) -> Self {
        Self {
            args,
            shell: None,
            proxy,
            startup,
            painted: false,
        }
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
        match Shell::new(window, &self.args, self.proxy.clone(), &mut self.startup) {
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
                    _ => {}
                }
            }
            WindowEvent::RedrawRequested => {
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
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("{{\"code\":\"E_EVENT_LOOP\",\"message\":\"{e}\"}}");
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn connect_is_required_and_usdc_is_optional() {
        let a = ViewArgs::parse(&words(&["--connect", "127.0.0.1:8765"])).expect("parses");
        assert_eq!(a.connect, "127.0.0.1:8765");
        assert_eq!(a.usdc, None);

        let b = ViewArgs::parse(&words(&["--connect=127.0.0.1:8765", "--usdc", "city.usdc"]))
            .expect("parses");
        assert_eq!(b.connect, "127.0.0.1:8765");
        assert_eq!(b.usdc.as_deref(), Some("city.usdc"));
    }

    #[test]
    fn a_missing_or_empty_connect_is_an_error_not_a_default() {
        assert!(ViewArgs::parse(&words(&[])).is_err());
        assert!(ViewArgs::parse(&words(&["--usdc", "a.usdc"])).is_err());
        assert!(ViewArgs::parse(&words(&["--connect", ""])).is_err());
        assert!(ViewArgs::parse(&words(&["--connect"])).is_err());
    }

    #[test]
    fn an_unknown_flag_is_refused_with_its_name() {
        let e = ViewArgs::parse(&words(&["--connect", "h:1", "--nope"])).expect_err("refused");
        assert!(e.contains("--nope"), "{e}");
    }
}
