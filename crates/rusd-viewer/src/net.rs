//! The two worker threads the window drives: the **network** thread (TCP client
//! to the server) and the **render** thread (owner of the GPU viewport).
//!
//! Both are behind a handle and a pair of channels, so the winit/egui thread
//! never blocks on a socket read or a frame render — it collects what arrived and
//! draws.
//!
//! - [`ProtocolClient`] is the synchronous half: connect, `hello`, `subscribe`,
//!   opinion / undo / redo, and one `read_wire` per incoming line. It is what a
//!   test uses against an in-process mock server (`tests/protocol_client.rs`),
//!   with no threads and no window.
//! - [`NetWorker`] runs a `ProtocolClient` on its own thread, so a slow or dead
//!   server cannot freeze the UI.
//! - [`RenderWorker`] owns a [`crate::gpu::Viewport`] on **egui's own device**
//!   (`wgpu::Device` and `Queue` are cheap `Send` handles), receives owned
//!   geometry over a channel — never a `Stage`, which is `Rc` and `!Send`
//!   (`docs/REMOTE.md` §7) — and returns finished frames. Selection outlining
//!   and picking happen there and here respectively, both on the CPU, both
//!   local: no round trip for a camera move or a click.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;
use std::time::Duration;

use rusd_core::Diagnostic;
use rusd_render::annotate::{self, SelectMode, Selection, Style};
use rusd_scene::geometry::Camera;

use crate::camera::Orbit;
use crate::diag;
use crate::gpu::Viewport;
use crate::protocol::{self, Notification, OpinionOp, Response, Wire};
use rusd_scene::geometry::SceneGeometry;

/// The synchronous wire client: one connection, one request id counter.
pub struct ProtocolClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    next_id: u64,
    /// The peer address, for messages.
    pub addr: String,
}

impl ProtocolClient {
    /// Connect to `addr` (`host:port`) and answer the greeting: `hello`, then
    /// `subscribe`. Returns the client and everything the server said before the
    /// greeting was done (its `hello` reply, and any notification that raced it).
    pub fn open(addr: &str) -> Result<(Self, Vec<Notification>), Vec<Diagnostic>> {
        let sock_addr = addr
            .to_socket_addrs()
            .map_err(|e| vec![diag("E_CONNECT", format!("{addr}: {e}"))])?
            .next()
            .ok_or_else(|| vec![diag("E_CONNECT", format!("{addr}: resolves to no address"))])?;
        let stream = TcpStream::connect_timeout(&sock_addr, Duration::from_secs(5))
            .map_err(|e| vec![diag("E_CONNECT", format!("{addr}: {e}"))])?;
        stream
            .set_nodelay(true)
            .map_err(|e| vec![diag("E_CONNECT", format!("{addr}: {e}"))])?;
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| vec![diag("E_CONNECT", format!("{addr}: {e}"))])?,
        );
        let mut client = Self {
            reader,
            writer: stream,
            next_id: 1,
            addr: addr.to_owned(),
        };
        let mut early = Vec::new();
        let hello = client.call(&protocol::hello_line(1))?;
        match hello.result {
            Ok(result) => {
                let mut state = protocol::ClientState::default();
                state.on_hello(&result).map_err(|d| vec![d])?;
            }
            Err(e) => {
                return Err(vec![diag(
                    "E_WIRE_PROTOCOL",
                    format!("hello failed: {} (code {})", e.message, e.code),
                )]);
            }
        }
        client.next_id = 2;
        let sub = client.call(&protocol::subscribe_line(2))?;
        if let Err(e) = sub.result {
            return Err(vec![diag(
                "E_WIRE_PROTOCOL",
                format!("subscribe failed: {} (code {})", e.message, e.code),
            )]);
        }
        client.next_id = 3;
        early.extend(hello.early);
        Ok((client, early))
    }

    /// Send one line and wait for the reply to its `id`, keeping any notification
    /// that arrives first.
    fn call(&mut self, line: &str) -> Result<Call, Vec<Diagnostic>> {
        let id = parse_id(line)
            .ok_or_else(|| vec![diag("E_WIRE", "a request line with no id".to_owned())])?;
        self.writer
            .write_all(line.as_bytes())
            .map_err(|e| vec![diag("E_IO", format!("{}: write failed: {e}", self.addr))])?;
        self.writer
            .flush()
            .map_err(|e| vec![diag("E_IO", format!("{}: flush failed: {e}", self.addr))])?;
        let mut early = Vec::new();
        loop {
            match self.read_wire()? {
                Wire::Response(r) if r.id == id => {
                    return Ok(Call {
                        result: r.result,
                        early,
                    });
                }
                Wire::Response(other) => {
                    return Err(vec![diag(
                        "E_WIRE",
                        format!("expected a reply to id {id}, got a reply to {}", other.id),
                    )]);
                }
                Wire::Notification(n) => early.push(n),
            }
        }
    }

    /// Block until one line arrives (a notification or a reply) and parse it.
    pub fn read_wire(&mut self) -> Result<Wire, Vec<Diagnostic>> {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .map_err(|e| vec![diag("E_IO", format!("{}: read failed: {e}", self.addr))])?;
        if n == 0 {
            return Err(vec![diag(
                "E_IO",
                format!("{}: the server closed the connection", self.addr),
            )]);
        }
        protocol::parse_line(line.trim_end_matches(['\r', '\n'])).map_err(|d| vec![d])
    }

    /// Send `opinion{ops}`, waiting for the server's `{version}`. Any
    /// notification that raced the reply is dropped with it: a fresh snapshot
    /// follows the server's own re-composition, and the caller renders that.
    pub fn opinion(&mut self, ops: &[OpinionOp]) -> Result<Response, Vec<Diagnostic>> {
        let line = protocol::opinion_line(self.next_id, ops);
        let Call { result, .. } = self.call(&line)?;
        self.next_id += 1;
        Ok(Response {
            id: self.next_id - 1,
            result,
        })
    }

    /// Send `undo` or `redo`.
    pub fn edit(&mut self, redo: bool) -> Result<Response, Vec<Diagnostic>> {
        let line = protocol::edit_line(self.next_id, redo);
        let Call { result, .. } = self.call(&line)?;
        self.next_id += 1;
        Ok(Response {
            id: self.next_id - 1,
            result,
        })
    }
}

struct Call {
    result: Result<serde_json::Value, protocol::RpcError>,
    early: Vec<Notification>,
}

fn parse_id(line: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(line.trim_end()).ok()?;
    v.get("id").and_then(serde_json::Value::as_u64)
}

/// A transfer of whole objects where only the newest matters: a frame that
/// arrived while two more did is not worth drawing. Bounded by construction —
/// the slot holds one, and a producer never waits.
pub struct Latest<T> {
    slot: std::sync::Mutex<Option<T>>,
    ready: std::sync::Condvar,
}

impl<T> Default for Latest<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Latest<T> {
    pub fn new() -> Self {
        Self {
            slot: std::sync::Mutex::new(None),
            ready: std::sync::Condvar::new(),
        }
    }

    /// Replace whatever was waiting.
    pub fn put(&self, v: T) {
        let mut g = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(v);
        drop(g);
        self.ready.notify_all();
    }

    /// Take the newest, if any (never blocks).
    pub fn take(&self) -> Option<T> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Wait up to `timeout` for something to arrive, then take the newest.
    pub fn wait(&self, timeout: Duration) -> Option<T> {
        let g = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        let (mut g, _) = self
            .ready
            .wait_timeout_while(g, timeout, |slot| slot.is_none())
            .unwrap_or_else(|e| e.into_inner());
        g.take()
    }
}

/// What the UI asks the network thread to send.
#[derive(Clone, Debug, PartialEq)]
pub enum NetCommand {
    /// One batch of opinions (a gizmo drag's throttled sample, or its final one).
    Opinion(Vec<OpinionOp>),
    Undo,
    Redo,
}

/// What the network thread reports back.
#[derive(Clone, Debug)]
pub enum NetEvent {
    /// Everything the server sent that was not a reply to a command.
    Notification(Notification),
    /// An edit was answered with the server's new version.
    Applied {
        what: &'static str,
        version: Option<u64>,
    },
    /// A command failed; the connection stays up unless it was an IO error.
    Failed(Vec<Diagnostic>),
    /// The connection is over (the server closed it, or IO broke).
    Closed(Vec<Diagnostic>),
    /// `hello`+`subscribe` succeeded: the client knows it is live.
    Live { addr: String },
}

/// The network thread: a [`ProtocolClient`] behind channels.
pub struct NetWorker {
    cmds: Sender<NetCommand>,
    events: Receiver<NetEvent>,
    tid: Option<thread::JoinHandle<()>>,
}

/// Called after every batch of worker events, so the window thread redraws
/// without polling.
pub type Wake = Box<dyn Fn() + Send>;

impl NetWorker {
    /// Connect on a new thread. The greeting happens there too, so a server that
    /// never answers does not stall the window; the result arrives as
    /// [`NetEvent::Live`] or [`NetEvent::Closed`].
    ///
    /// `wake` fires after every batch of events, so the window thread redraws
    /// without polling.
    pub fn spawn(addr: String, wake: Wake) -> Self {
        let (cmd_tx, cmd_rx) = channel();
        let (ev_tx, ev_rx) = channel();
        let tid = thread::spawn(move || {
            let (mut client, early) = match ProtocolClient::open(&addr) {
                Ok((c, early)) => (c, early),
                Err(diags) => {
                    let _ = ev_tx.send(NetEvent::Closed(diags));
                    wake();
                    return;
                }
            };
            let _ = ev_tx.send(NetEvent::Live {
                addr: client.addr.clone(),
            });
            for n in early {
                let _ = ev_tx.send(NetEvent::Notification(n));
            }
            wake();
            loop {
                // Commands first: a gizmo drag's samples are drained in order, so
                // the last position sent is the last one the server hears.
                let mut sent_any = false;
                loop {
                    let cmd = match cmd_rx.try_recv() {
                        Ok(c) => c,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return,
                    };
                    sent_any = true;
                    let (what, sent) = match cmd {
                        NetCommand::Opinion(ops) => ("opinion", client.opinion(&ops)),
                        NetCommand::Undo => ("undo", client.edit(false)),
                        NetCommand::Redo => ("redo", client.edit(true)),
                    };
                    match sent {
                        Ok(r) => match r.result {
                            Ok(v) => {
                                let _ = ev_tx.send(NetEvent::Applied {
                                    what,
                                    version: v.get("version").and_then(serde_json::Value::as_u64),
                                });
                            }
                            Err(e) => {
                                let _ = ev_tx.send(NetEvent::Failed(vec![diag(
                                    "E_RPC",
                                    format!("{what}: {} (code {})", e.message, e.code),
                                )]));
                            }
                        },
                        Err(diags) => {
                            let _ = ev_tx.send(NetEvent::Closed(diags));
                            wake();
                            return;
                        }
                    }
                }
                if sent_any {
                    wake();
                }
                // Then whatever the server pushed. This read blocks, which is
                // what parks the thread when nothing is happening.
                match client.read_wire() {
                    Ok(Wire::Notification(n)) => {
                        let _ = ev_tx.send(NetEvent::Notification(n));
                    }
                    Ok(Wire::Response(r)) => {
                        // An unmatched reply: report it rather than drop it
                        // silently, because it means the two sides disagree.
                        let _ = ev_tx.send(NetEvent::Failed(vec![diag(
                            "E_WIRE",
                            format!("an unsolicited reply to id {}", r.id),
                        )]));
                    }
                    Err(diags) => {
                        let _ = ev_tx.send(NetEvent::Closed(diags));
                        wake();
                        return;
                    }
                }
                wake();
            }
        });
        Self {
            cmds: cmd_tx,
            events: ev_rx,
            tid: Some(tid),
        }
    }

    /// Queue a command for the network thread.
    pub fn send(&self, cmd: NetCommand) {
        let _ = self.cmds.send(cmd);
    }

    /// Everything the network thread reported since the last call.
    pub fn drain(&self) -> Vec<NetEvent> {
        let mut out = Vec::new();
        while let Ok(e) = self.events.try_recv() {
            out.push(e);
        }
        out
    }

    /// Whether the network thread is still running.
    pub fn is_running(&self) -> bool {
        self.tid.is_some()
    }
}

impl Drop for NetWorker {
    fn drop(&mut self) {
        // Dropping `cmds` ends the thread's command loop; a blocking read ends
        // when the socket closes.
        if let Some(tid) = self.tid.take() {
            let _ = tid;
        }
    }
}

/// What the UI asks the render thread to do.
pub enum RenderCommand {
    /// A new scene: bake and upload (the camera is not part of it).
    ///
    /// Only the geometry crosses: a `MadeScene` also holds its composed `Stage`,
    /// which is `Rc` and `!Send` — it stays on the thread that made it, for the
    /// gizmo's anchor reads (`crate::anchor`), and the render thread never sees it.
    Scene {
        geometry: Box<SceneGeometry>,
        bounds: ([f64; 3], [f64; 3]),
    },
    /// A camera move: re-render only (no upload).
    View {
        orbit: Orbit,
        selection: Option<String>,
    },
    /// A gizmo drag's local preview: re-bake with these prims moved, then draw.
    /// It is the one command that re-uploads geometry, and it exists so a drag
    /// never waits for the server's answer to follow the pointer.
    Preview {
        overrides: std::collections::BTreeMap<String, [f64; 3]>,
        selection: Option<String>,
    },
    /// Resize the render targets.
    Resize { width: u32, height: u32 },
}

/// One finished frame, as the UI needs it.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// RGBA8, gamma-encoded (sRGB target), with the selection outline drawn.
    pub beauty: Vec<u8>,
    /// Per-pixel prim ids, for a local pick.
    pub ids: Vec<u32>,
    /// id → prim path.
    pub paths: Vec<String>,
    /// How long this frame took to draw and read back.
    pub millis: f64,
}

/// The render thread: owns the [`Viewport`] and the last scene.
pub struct RenderWorker {
    cmds: Sender<RenderCommand>,
    frames: std::sync::Arc<Latest<Frame>>,
    errors: std::sync::Arc<Latest<Vec<Diagnostic>>>,
    tid: Option<thread::JoinHandle<()>>,
}

impl RenderWorker {
    /// Start the render thread on `device`/`queue` — egui's own, handed over as
    /// cheap `Send` handles, so the viewport's textures live on the device that
    /// presents them and no frame is copied between GPUs.
    ///
    /// `wake` fires whenever a frame is ready, so the window thread redraws
    /// without polling.
    pub fn spawn(
        device: wgpu::Device,
        queue: wgpu::Queue,
        width: u32,
        height: u32,
        style: Style,
        wake: Wake,
    ) -> Self {
        let (cmd_tx, cmd_rx) = channel::<RenderCommand>();
        let frames = std::sync::Arc::new(Latest::new());
        let errors = std::sync::Arc::new(Latest::new());
        let f = frames.clone();
        let e = errors.clone();
        let primes_wake = Some(wake);
        let tid = thread::spawn(move || {
            let mut viewport = Viewport::new(device, queue, width, height);
            // The last scene, kept so a preview can re-bake it with a prim moved.
            let mut scene: Option<SceneGeometry> = None;
            let mut orbit: Option<Orbit> = None;
            let mut selection: Option<String> = None;
            let mut bounds: ([f64; 3], [f64; 3]) = ([0.0; 3], [0.0; 3]);
            let mut dirty = false;
            loop {
                // Commands may pile up while a frame is in flight; every one is
                // applied, and only the last camera survives, so a fast drag
                // never builds a backlog of stale views.
                let mut got = false;
                loop {
                    let cmd = if got {
                        match cmd_rx.try_recv() {
                            Ok(c) => c,
                            Err(_) => break,
                        }
                    } else {
                        match cmd_rx.recv_timeout(Duration::from_millis(50)) {
                            Ok(c) => c,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    };
                    got = true;
                    match cmd {
                        RenderCommand::Scene {
                            geometry,
                            bounds: b,
                        } => {
                            viewport.set_scene(&geometry);
                            bounds = b;
                            if let Some(cam) = geometry.camera {
                                orbit = Some(Orbit::from_camera(&cam));
                            }
                            scene = Some(*geometry);
                            dirty = true;
                        }
                        RenderCommand::View {
                            orbit: o,
                            selection: sel,
                        } => {
                            orbit = Some(o);
                            selection = sel;
                            dirty = true;
                        }
                        RenderCommand::Preview {
                            overrides,
                            selection: sel,
                        } => {
                            if let Some(geo) = &scene {
                                viewport.set_scene_with_translates(geo, &overrides);
                                selection = sel;
                                dirty = true;
                            }
                        }
                        RenderCommand::Resize { width, height } => {
                            viewport.resize(width, height);
                            dirty = true;
                        }
                    }
                }
                if !got && !dirty {
                    continue;
                }
                let Some(o) = orbit else { continue };
                if !dirty {
                    continue;
                }
                let cfg = rusd_render::RenderConfig {
                    width: viewport.size().0,
                    height: viewport.size().1,
                    ..Default::default()
                };
                let started = std::time::Instant::now();
                match viewport.render(&o.camera(), bounds, &cfg) {
                    Ok(mut out) => {
                        if let Some(sel) = &selection {
                            annotate::apply_selection(
                                &mut out,
                                &[Selection {
                                    path: sel.clone(),
                                    color: [255, 60, 60],
                                    mode: SelectMode::Both,
                                }],
                                &style,
                            );
                        }
                        f.put(Frame {
                            width: out.width,
                            height: out.height,
                            beauty: out.beauty,
                            ids: out.ids,
                            paths: out.paths,
                            millis: started.elapsed().as_secs_f64() * 1000.0,
                        });
                        if let Some(wake) = &primes_wake {
                            wake();
                            // The proxy is a one-shot nudge; keep the closure for
                            // the next frame (a proxy clone is cheap, so this is
                            // not a resource that runs out).
                            let _ = wake;
                        }
                        dirty = false;
                    }
                    Err(diags) => {
                        e.put(diags);
                        if let Some(wake) = &primes_wake {
                            wake();
                        }
                        dirty = false;
                    }
                }
            }
        });
        Self {
            cmds: cmd_tx,
            frames,
            errors,
            tid: Some(tid),
        }
    }

    /// Queue a command (never blocks: the channel is unbounded and the consumer
    /// coalesces).
    pub fn send(&self, cmd: RenderCommand) {
        let _ = self.cmds.send(cmd);
    }

    /// The newest finished frame, if one arrived since the last call.
    pub fn take_frame(&self) -> Option<Frame> {
        self.frames.take()
    }

    /// The newest render failure, if any.
    pub fn take_error(&self) -> Option<Vec<Diagnostic>> {
        self.errors.take()
    }

    /// Whether the render thread is still running.
    pub fn is_running(&self) -> bool {
        self.tid.is_some()
    }
}

/// A camera as the render thread wants it (a plain value; `Camera` itself is a
/// `rusd_scene` type and is what the projection takes).
pub fn camera_of(o: &Orbit) -> Camera {
    o.camera()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn latest_keeps_only_the_newest() {
        let l = Latest::new();
        assert!(l.take().is_none());
        l.put(1);
        l.put(2);
        assert_eq!(l.take(), Some(2), "the newest replaces the waiting one");
        assert!(l.take().is_none());
    }

    #[test]
    fn latest_waits_until_something_arrives_or_the_timeout_expires() {
        let l = Arc::new(Latest::new());
        let started = std::time::Instant::now();
        assert!(l.wait(Duration::from_millis(20)).is_none());
        assert!(started.elapsed() >= Duration::from_millis(20));

        let putter = {
            let l = l.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                l.put(7);
            })
        };
        assert_eq!(l.wait(Duration::from_secs(2)), Some(7));
        putter.join().expect("putter");
    }

    #[test]
    fn a_request_id_is_read_back_from_its_line() {
        assert_eq!(parse_id(&protocol::opinion_line(9, &[])), Some(9));
        assert_eq!(parse_id("not json"), None);
    }

    #[test]
    fn a_camera_survives_the_orbit_round_trip() {
        use crate::scene::{BBox, fit_camera};
        let cam = fit_camera(&BBox {
            min: [-1.0, -1.0, 0.0],
            max: [1.0, 1.0, 2.0],
        });
        let back = camera_of(&Orbit::from_camera(&cam));
        for k in 0..3 {
            assert!((cam.position[k] - back.position[k]).abs() < 1e-9);
        }
    }
}
