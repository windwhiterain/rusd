//! # TCP NDJSON JSON-RPC transport (D12 v1, Tier A snapshot mirror)
//!
//! `docs/PROTOCOL.md` is the contract this module implements: one JSON-RPC 2.0
//! object per line over TCP, agent tool names reused, plus `hello` /
//! `subscribe` / `unsubscribe` / `opinion` and the server → client
//! notifications `stage_snapshot` / `state_changed` / `diagnostics`.
//!
//! ## The authority stack, and what a mutation does
//!
//! The server owns one [`Agent`] (the `.rusd` document, D11) and one live
//! `openusd` stage: the document lowered with an overrides **session layer**
//! (`rusd_scene::lower_with_session`), generator nodes cooked into it, and the
//! remembered opinions replayed into that session layer. After every mutation
//! the stack is (re)built, cooked, flattened (Tier A: the snapshot bakes assets
//! and drops arcs, so it is self-sufficient and anonymous-safe) and serialized
//! as `usdc` through `LayerRegistry::find_by_id("usdc").write_bytes`; every
//! subscriber gets the snapshot plus a `state_changed`, one monotonic `version`
//! at a time.
//!
//! ## Beyond the D12 surface
//!
//! Four groups of methods live alongside that one, each described by its own
//! section of `docs/PROTOCOL.md`:
//!
//! - [`crate::capture`] — `capture` / `capture_get` / `capture_list`, the
//!   visual references a human hands an agent (PROTOCOL v1.1). A capture is
//!   **not** a scene mutation: it does not advance `version` and broadcasts
//!   nothing.
//! - [`crate::composition`] — `composition_tree` / `comp_node` / `layer`, the
//!   composition tree the viewer's panel draws (PROTOCOL v1.2, with `layer` from
//!   v1.5.5). It is read off the authority stack's `sdf` layer specs, not off the
//!   flattened snapshot, which has already dropped every arc. `layer{id}` answers
//!   what one opened layer authors, so a clicked layer row shows its own specs
//!   rather than the composition's verdict on them. The tree is cached for the
//!   `version` it describes and dropped whenever the stage is rebuilt.
//! - `preview` here (PROTOCOL v1.4, extended by v1.5.3) — how far into the cook
//!   **one connection's** snapshots go. The state is per connection, not per
//!   session: two viewers may watch different stages of the same cook at once. A
//!   preview is not a mutation and does not move `version`; it flattens the same
//!   authority stack with the later cook output layers muted. A connection can
//!   stop at any cook op, at `null` (the whole composition) or at the **root
//!   layer** — the stop before every op, which shows the pipeline's input.
//!
//! Which mutations rebuild, and which edit in place:
//!
//! - `open` / `patch` / `undo` / `redo` / a hot **reload**: the document
//!   changed, so the root layer must be re-authored — rebuild the stage, replay
//!   the opinions, re-cook.
//! - `opinion`: the session layer is edited in place (`Stage::layer_mut`), which
//!   is what a gizmo drag sends tens of times a second; the stage survives, so
//!   the cook engine's memo still hits for every node whose inputs did not move.
//!
//! Hot reload watches the opened *file* (`notify`, no recursion) — a document
//! handed in as `content` has none — and re-reads it once the save has gone
//! quiet (`RELOAD_DEBOUNCE`). A rewrite that leaves the bytes unchanged publishes
//! nothing. REMOTE §9 G4's missing reload is an `openusd` crate gap on the client
//! mirror side; the server needs no crate reload because it rebuilds the stage
//! from the document.
//!
//! `Stage` is `Rc`-based and therefore `!Send`, so the whole session — document,
//! stage, cook memo, versions, subscriber list, the live watch — lives on one
//! dedicated sync thread and every connection talks to it through a channel
//! (REMOTE §7's hard boundary, applied to the server). The watch callback runs on
//! `notify`'s thread and only wakes this loop; the debounce is a deadline on the
//! loop's timed receive, so a pending reload never starves a request.
//!
//! ## Semantics beyond the protocol's shape
//!
//! Everything below is recorded in `docs/PROTOCOL.md`'s 「v1 实现语义补记」, which
//! the parent decision made part of the contract — none of it is a deviation:
//!
//! - **`subscribe` also pushes the current snapshot** to that connection when a
//!   stage exists, right after its `{version}` response: the protocol defines no
//!   snapshot fetch, so a late subscriber would otherwise never see the scene.
//! - **`hello.session`** is `rusd-serve/<pid>`; a protocol mismatch is refused
//!   with `-32602 invalid params`.
//! - **A mutation that cannot compose** (parse/validation errors, a failed
//!   flatten) broadcasts `diagnostics` and does not advance `version`: a
//!   `state_changed` naming a version no snapshot will carry would strand every
//!   client.
//! - **Order after one request**: its response, then `stage_snapshot`, then
//!   `diagnostics` when there are any, then `state_changed`.
//! - **`rusd serve --listen`** takes `DEFAULT_LISTEN` for a bare flag; no flag
//!   leaves the server on stdio.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use notify::Watcher as _;
use openusd::gf;
use openusd::sdf::{self, AttributeSpec, Variability};
use openusd::usd::Stage;
use rusd_core::{Diagnostic, Severity};
use rusd_proc::{CookEngine, CookOp, ExpandOutput};
use serde_json::{Value, json};

use crate::Agent;
use crate::capture::CaptureStore;
use crate::composition::CompositionTree;

/// The wire protocol version this server speaks (`hello`).
pub const PROTOCOL_VERSION: u64 = 1;

/// The bind address `rusd serve --listen` uses when the flag carries no value
/// (PROTOCOL §传输).
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8765";

/// How long after the last file event the document is re-read. A save is not
/// one write event — a truncate, the write, a rename, a metadata touch — so the
/// quiet period after the last event is what marks the save complete.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(200);

/// The preview state that stops **before** the first cook op: the stage composes
/// only the document's own layer plus the session overrides, with every cook
/// output layer muted (PROTOCOL v1.5.3).
///
/// It is spelled `-1` because the state is a position in cook order and this is
/// the position no cook reaches. That spelling is what makes the root stop
/// survive a rebuild: a cook output layer's identifier is anonymous and a
/// rebuild mints a new one, while `-1` says the same thing however many ops the
/// new cook has and whatever the root layer is now called.
const PREVIEW_ROOT: isize = -1;

/// JSON-RPC codes used here (PROTOCOL §传输).
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const PARSE_ERROR: i64 = -32700;
const SERVER_ERROR: i64 = -32000;

/// Agent tool names this transport forwards to [`Agent::call`] unchanged
/// (PROTOCOL §方法). The mutating ones are intercepted before this list.
const AGENT_METHODS: &[&str] = &[
    "open", "query", "validate", "expand", "render", "pick", "patch", "diff", "undo", "redo",
    "export", "state",
];

// --- server ---------------------------------------------------------------

/// A listening `rusd serve --listen` server: an accept thread, one reader and
/// one writer thread per connection, and the sync thread that owns the session.
pub struct Server {
    addr: SocketAddr,
    /// Held for the server's life: while one sender is alive the sync thread's
    /// receiver never closes, so the session outlives every connection.
    _keepalive: mpsc::Sender<Msg>,
    stop: Arc<AtomicBool>,
}

impl Server {
    /// Bind `addr` and start serving. `port 0` binds an ephemeral port, which
    /// [`local_addr`](Self::local_addr) then reports.
    ///
    /// Visual-reference captures go to `<cwd>/target/captures` (PROTOCOL v1.1),
    /// the build directory of whatever project the server was started in.
    pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Server> {
        let root = std::env::current_dir()?.join("target").join("captures");
        Server::bind_with_captures(addr, root)
    }

    /// [`bind`](Self::bind), with the capture directory named outright — which
    /// is what a test wants, so its captures land somewhere it can delete.
    pub fn bind_with_captures(addr: impl ToSocketAddrs, captures: PathBuf) -> io::Result<Server> {
        let captures = CaptureStore::open(captures)?;
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;

        let (tx, rx) = mpsc::channel::<Msg>();
        let name = format!("rusd-serve/{}", std::process::id());
        let sync_tx = tx.clone();
        thread::Builder::new()
            .name("rusd-sync".to_owned())
            .spawn(move || {
                let mut session = Session::new(name, sync_tx, captures);
                loop {
                    // A pending file-watch debounce turns the blocking receive
                    // into a timed one: the loop wakes on the deadline when no
                    // message arrives first, so a reload never blocks a request
                    // and a request never delays a reload forever.
                    //
                    // The deadline is checked *before* the receive, not only
                    // reported by a receive timeout: a client that keeps
                    // sending requests (a live viewer polling `state`) makes
                    // every `recv_timeout` return at once, so a reload due
                    // during that traffic would otherwise starve until the
                    // client went quiet.
                    if session.deadline_passed() {
                        session.on_deadline();
                        continue;
                    }
                    let msg = match session.next_deadline() {
                        Some(deadline) => {
                            let wait = deadline.saturating_duration_since(Instant::now());
                            match rx.recv_timeout(wait) {
                                Ok(msg) => Some(msg),
                                Err(mpsc::RecvTimeoutError::Timeout) => None,
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            }
                        }
                        None => match rx.recv() {
                            Ok(msg) => Some(msg),
                            Err(_) => break,
                        },
                    };
                    match msg {
                        Some(msg) => session.handle(msg),
                        None => session.on_deadline(),
                    }
                }
            })?;

        let stop = Arc::new(AtomicBool::new(false));
        let accept_stop = Arc::clone(&stop);
        let accept_tx = tx.clone();
        thread::Builder::new()
            .name("rusd-accept".to_owned())
            .spawn(move || {
                let mut conn = 0u64;
                while !accept_stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            // The listener is non-blocking only so that accept can
                            // notice `stop`; each connection is a blocking stream.
                            let _ = stream.set_nonblocking(false);
                            conn += 1;
                            let tx = accept_tx.clone();
                            let id = conn;
                            let _ = thread::Builder::new()
                                .name(format!("rusd-conn-{id}"))
                                .spawn(move || serve_conn(stream, id, tx));
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            })?;
        Ok(Server {
            addr,
            _keepalive: tx,
            stop,
        })
    }

    /// The address actually bound (resolved for an ephemeral port).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop accepting connections. Connections already open end when their
    /// clients close them; the sync thread ends when the last of them does.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Serve forever on `addr` (`rusd serve --listen`).
pub fn serve_tcp(addr: &str) -> Result<(), String> {
    let server = Server::bind(addr).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    eprintln!(
        "rusd serve: listening on {} (protocol {PROTOCOL_VERSION}, NDJSON JSON-RPC)",
        server.local_addr()
    );
    loop {
        thread::park();
    }
}

/// What a `--listen` flag means given the word after it: a value that is not
/// another flag is the address, and a bare flag (no word, or a flag following)
/// means [`DEFAULT_LISTEN`].
pub fn listen_target(next: Option<&str>) -> &str {
    match next {
        Some(value) if !value.starts_with('-') => value,
        _ => DEFAULT_LISTEN,
    }
}

// --- one connection -------------------------------------------------------

/// A message from a connection thread to the sync thread.
enum Msg {
    /// One parsed NDJSON request; `out` is this connection's writer channel, so
    /// the response, and any notification it is subscribed to, land in order.
    Request {
        conn: u64,
        req: Value,
        out: mpsc::Sender<String>,
    },
    /// The connection closed: forget its subscription.
    Gone { conn: u64 },
    /// The watched document file changed on disk: (re)arm the debounce.
    FileChanged,
}

/// Read NDJSON requests from `stream` until it closes, handing each to the sync
/// thread; a writer thread drains the responses and notifications back out.
fn serve_conn(stream: TcpStream, conn: u64, tx: mpsc::Sender<Msg>) {
    let Ok(mut out_stream) = stream.try_clone() else {
        return;
    };
    let (wtx, wrx) = mpsc::channel::<String>();
    let writer = thread::Builder::new()
        .name(format!("rusd-write-{conn}"))
        .spawn(move || {
            for line in wrx {
                if writeln!(out_stream, "{line}")
                    .and_then(|()| out_stream.flush())
                    .is_err()
                {
                    break;
                }
            }
        });

    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let req = match serde_json::from_str::<Value>(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = wtx.send(to_line(&err_reply(
                    Value::Null,
                    PARSE_ERROR,
                    format!("parse error: {e}"),
                )));
                continue;
            }
        };
        if tx
            .send(Msg::Request {
                conn,
                req,
                out: wtx.clone(),
            })
            .is_err()
        {
            break;
        }
    }
    let _ = tx.send(Msg::Gone { conn });
    drop(wtx);
    if let Ok(handle) = writer {
        let _ = handle.join();
    }
}

// --- the session (one sync thread) ----------------------------------------

/// A remembered override opinion: PROTOCOL's `{path, field, value, op}`.
#[derive(Clone, Debug, PartialEq)]
struct Opinion {
    path: String,
    field: String,
    value: OpinionValue,
}

#[derive(Clone, Debug, PartialEq)]
enum OpinionValue {
    Set(Value),
    Remove,
}

impl Opinion {
    /// Whether `other` addresses the same opinion slot (prim + field).
    fn same_slot(&self, other: &Opinion) -> bool {
        self.path == other.path && self.field == other.field
    }
}

/// One undoable face of the authority stack (REMOTE §3): the document source,
/// or the list of override opinions.
enum Face {
    Doc(String),
    Overrides(Vec<Opinion>),
}

/// What a pipeline pass produced: cook/validate diagnostics and whether the
/// stage composed at all.
struct Pipeline {
    diags: Vec<Diagnostic>,
    ok: bool,
}

impl Pipeline {
    fn failed(diags: Vec<Diagnostic>) -> Self {
        Pipeline { diags, ok: false }
    }
}

struct Session {
    agent: Agent,
    session_name: String,
    version: u64,
    stage: Option<Stage>,
    session_id: Option<String>,
    cook: CookEngine,
    /// The ops of the latest cook pass (PROTOCOL v1.5.1), in cook order, each
    /// naming its generator, its `input_points` and the output layer it wrote
    /// now. Replaced **wholesale** after every cook (a rebuild mints new output
    /// layers, so the old identifiers name nothing) and never accumulated; it is
    /// what `composition_tree` joins against to label an op layer.
    ///
    /// A preview does not touch it: a prefix changes which layers a
    /// connection's snapshot stops at, not what the cook produced.
    cook_ops: Vec<CookOp>,
    ops: Vec<Opinion>,
    undo: Vec<Face>,
    redo: Vec<Face>,
    /// Subscribed connections, by connection id.
    subs: BTreeMap<u64, mpsc::Sender<String>>,
    /// Notification lines produced by the request being handled, held until
    /// that request's response is written: a client that sends `open` reads the
    /// response to `open` first, then the snapshot it caused.
    pending: Vec<Pending>,
    /// A channel back into this same sync loop, which the file-watch callback
    /// uses to wake it (that callback runs on `notify`'s own thread).
    wake: mpsc::Sender<Msg>,
    /// The document path being watched for hot reload, and the watcher that
    /// holds the registration. One watcher is kept and re-pointed when `open`
    /// names a different file, because dropping it would end the watch.
    watch: Option<Watch>,
    /// When the debounce of the last file event expires; `None` when no file
    /// event is pending.
    reload_due: Option<Instant>,
    /// Visual-reference captures (PROTOCOL v1.1): the directory they are written
    /// to and the numbering they carry on from.
    captures: CaptureStore,
    /// The composition tree (PROTOCOL v1.2), built from the authority stack's
    /// `sdf` layer specs and kept for the version it describes: `comp_node` is
    /// asked for one node at a time, after the tree that named it.
    tree: Option<(u64, CompositionTree)>,
    /// Per-connection preview state (PROTOCOL v1.4, extended by v1.5.3): how far
    /// into the cook a connection's snapshots stop. The value is a position in
    /// the **cook order** of the stage's output layers (0 is the first generator
    /// cooked), or [`PREVIEW_ROOT`] (`-1`) for the root stop, which composes no
    /// op at all; a connection absent from the map sees the final, full
    /// composition.
    ///
    /// The wire names a layer *identifier*, but cook output layers are anonymous
    /// (`anon:<n>:<tag>`) and a rebuild — `open` / `patch` / `undo` / `redo` /
    /// `reload` — mints new ones. Keeping the **position** is what lets a preview
    /// survive an edit to the document: the layer at that position is still the
    /// same generator's output, and the snapshot names the identifier the stack
    /// has now. A position the rebuilt stack no longer reaches falls back to the
    /// final composition, and the snapshot says so by carrying no `upto`. The
    /// root stop needs no such fallback: it is not a position any cook reaches,
    /// only the statement that no op is to be composed, which a rebuild cannot
    /// invalidate.
    preview: BTreeMap<u64, isize>,
}

/// What one mutation made available to its subscribers, held until the
/// request's response is written (PROTOCOL 「消息顺序」).
///
/// A snapshot is **not** one line: it depends on the connection it goes to, whose
/// preview state decides where the composition stops (PROTOCOL v1.4). So the
/// pending list names what to build and the broadcast builds it once per distinct
/// preview state.
enum Pending {
    /// A notification every subscriber gets verbatim: `diagnostics`,
    /// `state_changed`.
    Line(String),
    /// The scene at `version`. `final_line` is the whole composition, already
    /// built and so already known to flatten: a connection that asked for a
    /// preview gets a line built for its own state instead, and one that did
    /// not gets exactly this line without a second flatten.
    Snapshot { version: u64, final_line: String },
}

/// The hot-reload watch on one document path (PROTOCOL `reason:"reload"`).
struct Watch {
    /// The path `open` named — the file, not its directory, so an unrelated
    /// write beside it never wakes the session.
    path: PathBuf,
    /// Held, never read: dropping it ends the watch.
    _watcher: notify::RecommendedWatcher,
}

impl Session {
    fn new(session_name: String, wake: mpsc::Sender<Msg>, captures: CaptureStore) -> Self {
        Session {
            agent: Agent::new(),
            session_name,
            version: 0,
            stage: None,
            session_id: None,
            cook: rusd_proc::default_registry(),
            cook_ops: Vec::new(),
            ops: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            subs: BTreeMap::new(),
            pending: Vec::new(),
            wake,
            watch: None,
            reload_due: None,
            captures,
            tree: None,
            preview: BTreeMap::new(),
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Request { conn, req, out } => self.request(conn, &req, &out),
            Msg::Gone { conn } => {
                self.subs.remove(&conn);
                // A connection's preview state ends with the connection.
                self.preview.remove(&conn);
            }
            Msg::FileChanged => self.reload_due = Some(Instant::now() + RELOAD_DEBOUNCE),
        }
    }

    /// The instant the sync loop must wake at, if a file event is debouncing.
    fn next_deadline(&self) -> Option<Instant> {
        self.reload_due
    }

    /// Whether a debounce has expired with the loop not yet having seen it. The
    /// loop asks this before every receive, so a reload that falls due while
    /// requests are arriving is published then and not after the traffic stops.
    fn deadline_passed(&self) -> bool {
        self.reload_due.is_some_and(|due| Instant::now() >= due)
    }

    /// The debounce expired with no newer event: re-read the watched document
    /// and publish it as a `reload`.
    fn on_deadline(&mut self) {
        if self.reload_due.take().is_none() {
            return;
        }
        let Some(path) = self.watch.as_ref().map(|w| w.path.clone()) else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            // A save caught mid-write is not an error worth publishing: the
            // next event brings the finished file.
            return;
        };
        if self.agent.doc_text() == Some(text.as_str()) {
            return;
        }
        if let Some(before) = self.agent.doc_text().map(str::to_owned) {
            self.undo.push(Face::Doc(before));
        }
        self.redo.clear();
        self.agent.replace_doc(text);
        let pipeline = self.rebuild();
        let _ = self.after_mutation("reload", &pipeline);
        self.publish();
    }

    /// Point the hot-reload watch at `path`, replacing any earlier one.
    fn watch_doc(&mut self, path: &str) {
        if self.watch.as_ref().map(|w| w.path.as_path()) == Some(Path::new(path)) {
            return;
        }
        self.watch = None;
        self.reload_due = None;
        let wake = self.wake.clone();
        let watcher = notify::recommended_watcher(move |_event: notify::Result<notify::Event>| {
            // Any delivery is a hint to re-read; the re-read itself decides
            // whether the content changed. An error delivery counts: the
            // Windows backend reports a buffer overflow that way, and dropping
            // it would make a reload wait for the next save.
            let _ = wake.send(Msg::FileChanged);
        });
        let Ok(mut watcher) = watcher else {
            eprintln!("rusd serve: cannot watch {path} for hot reload: the OS watcher failed");
            return;
        };
        if let Err(e) = watcher.watch(Path::new(path), notify::RecursiveMode::NonRecursive) {
            eprintln!("rusd serve: cannot watch {path} for hot reload: {e}");
            return;
        }
        self.watch = Some(Watch {
            path: PathBuf::from(path),
            _watcher: watcher,
        });
    }

    /// Dispatch one request and write its JSON-RPC response to `out`.
    fn request(&mut self, conn: u64, req: &Value, out: &mpsc::Sender<String>) {
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let Some(method) = req.get("method").and_then(Value::as_str) else {
            send(
                out,
                &err_reply(id, INVALID_REQUEST, "a request needs a `method`".into()),
            );
            return;
        };
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
        if !params.is_object() {
            send(
                out,
                &err_reply(id, INVALID_PARAMS, "`params` must be an object".into()),
            );
            return;
        }
        // A `subscribe` also pushes the current snapshot, which must reach the
        // client *after* the response it is answering.
        let mut after: Option<String> = None;
        let result = match method {
            "hello" => self.hello(&params),
            "subscribe" => {
                let subscribed = self.subscribe(conn, &params, out);
                if subscribed.is_ok() {
                    let upto = self.preview_position(conn);
                    after = self.snapshot_line(self.version, upto).ok();
                }
                subscribed
            }
            "unsubscribe" => self.unsubscribe(conn, &params),
            "preview" => {
                let set = self.preview(conn, &params);
                if set.is_ok() && self.subs.contains_key(&conn) {
                    // The connection asked to see the composition stop earlier
                    // (or to see it whole): send it the scene that preview
                    // describes, after the response. The version does not move —
                    // nothing about the authority state changed — so this
                    // snapshot carries the version the connection already had.
                    //
                    // Only a subscriber gets one: snapshots are what a
                    // subscription is for, and a connection that never
                    // subscribed would not otherwise see a notification at all.
                    let upto = self.preview_position(conn);
                    after = self.snapshot_line(self.version, upto).ok();
                }
                set
            }
            "opinion" => self.opinion(&params),
            "composition_tree" => self.composition_tree(&params),
            "layer" => self.layer(&params),
            "comp_node" => self.comp_node(&params),
            "capture" => self.captures.capture(&params),
            "capture_get" => self.captures.get(&params),
            "capture_list" => self.captures.list(&params),
            "open" => self.open(&params),
            "patch" => self.patch(&params),
            "undo" => self.undo(),
            "redo" => self.redo(),
            other if AGENT_METHODS.contains(&other) => {
                self.agent.call(other, &params).map_err(server_error)
            }
            other => Err((METHOD_NOT_FOUND, format!("unknown method: {other}"))),
        };
        match result {
            Ok(result) => send(out, &ok_reply(id, result)),
            Err((code, message)) => send(out, &err_reply(id, code, message)),
        }
        // Effect, then consequence: everything this request caused reaches the
        // clients after the answer to the request itself.
        self.publish();
        if let Some(line) = after {
            let _ = out.send(line);
        }
    }

    // --- viewer methods ---------------------------------------------------

    /// `preview{upto}` (PROTOCOL v1.4, extended by v1.5.3): how far into the cook
    /// **this** connection's snapshots go. `upto` is one of
    ///
    /// - the identifier of a cook output layer, as `composition_tree`'s `layers`
    ///   lists them (the rows with `op: true`) — the snapshot stops after that
    ///   op;
    /// - the identifier of the **root layer** (the row with `kind: "root"`) — the
    ///   snapshot stops **before** every op, so the connection sees the
    ///   document's own scene plus the session overrides (PROTOCOL v1.5.3);
    /// - `null`, the final, whole composition.
    ///
    /// The prefix the connection then sees is the root layer plus the cook
    /// output layers up to and including `upto` in **cook order** — not the
    /// stack order a `layers` row lists, which is the strength order and puts
    /// the newest layer first.
    ///
    /// The state is stored as the position in cook order for the reason
    /// [`Session`]'s `preview` field gives, and the root stop is the position
    /// `-1`: no op at all. Both kinds of stop then survive a rebuild — a cook
    /// output layer's identifier is anonymous and a rebuild mints a new one,
    /// while `-1` means the same thing however many ops the new cook has and
    /// whatever the root layer is called now.
    ///
    /// An identifier that names neither a cook output layer nor the root layer
    /// of the current stage is `-32602`, naming what would have worked: asking
    /// to stop at some other layer is a client bug, not a request to see
    /// something else.
    fn preview(&mut self, conn: u64, p: &Value) -> Result<Value, (i64, String)> {
        let upto = match p.get("upto") {
            None => {
                return Err((
                    INVALID_PARAMS,
                    "preview: `upto` (a cook output layer identifier, the root layer \
                     identifier, or null) required"
                        .into(),
                ));
            }
            Some(Value::Null) => None,
            Some(Value::String(id)) => Some(id.clone()),
            Some(other) => {
                return Err((
                    INVALID_PARAMS,
                    format!("preview: `upto` must be a layer identifier or null, not {other}"),
                ));
            }
        };
        let Some(stage) = self.stage.clone() else {
            return Err(server_error("no scene open; call open first".into()));
        };
        let root_id = stage.root_layer().identifier().to_owned();
        let ops = crate::composition::cook_output_layers(&stage);
        match upto {
            None => {
                self.preview.remove(&conn);
                Ok(json!({ "upto": Value::Null }))
            }
            // The root layer: the composition stops before the first cook op.
            Some(id) if id == root_id => {
                self.preview.insert(conn, PREVIEW_ROOT);
                Ok(json!({ "upto": id, "root": true }))
            }
            Some(id) => {
                let position = ops.iter().position(|layer| *layer == id).ok_or_else(|| {
                    (
                        INVALID_PARAMS,
                        format!(
                            "preview: {id:?} is neither a cook output layer nor the root layer \
                             of this stage; the op layers are {ops:?} and the root layer is \
                             {root_id:?} (null previews the final composition)"
                        ),
                    )
                })?;
                self.preview.insert(conn, position as isize);
                Ok(json!({ "upto": id, "op_index": position }))
            }
        }
    }

    /// Where `conn`'s preview stops, as a position in cook order; `None` is the
    /// final composition, and [`PREVIEW_ROOT`] (`-1`) is the stop before every
    /// cook op. A position the current stage no longer reaches (the document
    /// changed and the cook now has fewer ops) falls back to the final
    /// composition, draining the stale state as it does.
    ///
    /// The root stop is never stale: `-1` is not a position in the current cook
    /// at all, but the statement that no op should be composed, which holds for
    /// a stage with no ops just as well as for one with three.
    fn preview_position(&mut self, conn: u64) -> Option<isize> {
        let position = *self.preview.get(&conn)?;
        if position == PREVIEW_ROOT {
            return Some(PREVIEW_ROOT);
        }
        let stage = self.stage.as_ref()?;
        let len = crate::composition::cook_output_layers(stage).len();
        if (position as usize) < len {
            Some(position)
        } else {
            self.preview.remove(&conn);
            None
        }
    }

    /// `composition_tree{}` (PROTOCOL v1.2): the composition tree of the
    /// authority stack. Built on demand and kept until the stage changes — a
    /// `comp_node` for a node the client just read must see the same tree, and
    /// the build walks every layer, so it is not something to repeat per node.
    fn composition_tree(&mut self, _p: &Value) -> Result<Value, (i64, String)> {
        let Some(stage) = self.stage.clone() else {
            return Err(server_error("no scene open; call open first".into()));
        };
        let tree = CompositionTree::build_with_ops(&stage, &self.cook_ops);
        let value = tree.to_json();
        self.tree = Some((self.version, tree));
        Ok(value)
    }

    /// `comp_node{id}`: one node's detail, out of the tree last built. A stage
    /// that moved since is rebuilt first, so a node id the client is still
    /// holding cannot address the tree of a scene that no longer exists.
    fn comp_node(&mut self, p: &Value) -> Result<Value, (i64, String)> {
        let Some(id) = p.get("id").and_then(Value::as_str) else {
            return Err((INVALID_PARAMS, "comp_node: `id` (string) required".into()));
        };
        let Some(stage) = self.stage.clone() else {
            return Err(server_error("no scene open; call open first".into()));
        };
        if self.tree.as_ref().map(|(v, _)| *v) != Some(self.version) {
            let tree = CompositionTree::build_with_ops(&stage, &self.cook_ops);
            self.tree = Some((self.version, tree));
        }
        let (_, tree) = self.tree.as_ref().expect("the tree was just built");
        tree.node(&stage, id).ok_or_else(|| {
            (
                INVALID_PARAMS,
                format!("comp_node: no node has the id {id:?}"),
            )
        })
    }

    /// `layer{id}` (PROTOCOL v1.5.5): what one opened layer authors. The id is
    /// the identifier `composition_tree`'s `layers` rows carry, so a client that
    /// clicked a layer row (or an op row, or the root row) needs no lookup.
    ///
    /// A pure read: no `version` moves and nothing is published, because the
    /// answer describes the layer the stage already holds rather than changing
    /// it. The tree is brought up to the current version first, for the same
    /// reason `comp_node` does: an id a client is still holding must not address
    /// the layer stack of a scene that no longer exists.
    ///
    /// An unknown id is `-32602` and the message lists the layers that *are*
    /// open, which is what a client needs to correct itself.
    fn layer(&mut self, p: &Value) -> Result<Value, (i64, String)> {
        let Some(id) = p.get("id").and_then(Value::as_str) else {
            return Err((INVALID_PARAMS, "layer: `id` (string) required".into()));
        };
        let Some(stage) = self.stage.clone() else {
            return Err(server_error("no scene open; call open first".into()));
        };
        if self.tree.as_ref().map(|(v, _)| *v) != Some(self.version) {
            let tree = CompositionTree::build_with_ops(&stage, &self.cook_ops);
            self.tree = Some((self.version, tree));
        }
        let (_, tree) = self.tree.as_ref().expect("the tree was just built");
        tree.layer_content(&stage, id).ok_or_else(|| {
            (
                INVALID_PARAMS,
                format!(
                    "layer: no opened layer has the id {id:?}; the layers are {}",
                    tree.layer_ids().join(", ")
                ),
            )
        })
    }

    fn hello(&self, p: &Value) -> Result<Value, (i64, String)> {
        match p.get("protocol").and_then(Value::as_u64) {
            Some(PROTOCOL_VERSION) => {}
            Some(other) => {
                return Err((
                    INVALID_PARAMS,
                    format!("this server speaks protocol {PROTOCOL_VERSION}, not {other}"),
                ));
            }
            None => return Err((INVALID_PARAMS, "hello: `protocol` (number) required".into())),
        }
        Ok(json!({
            "protocol": PROTOCOL_VERSION,
            "session": self.session_name,
            "version": self.version,
        }))
    }

    fn subscribe(
        &mut self,
        conn: u64,
        p: &Value,
        out: &mpsc::Sender<String>,
    ) -> Result<Value, (i64, String)> {
        if p.get("client").and_then(Value::as_str).is_none() {
            return Err((
                INVALID_PARAMS,
                "subscribe: `client` (string) required".into(),
            ));
        }
        self.subs.insert(conn, out.clone());
        Ok(json!({ "version": self.version }))
    }

    fn unsubscribe(&mut self, conn: u64, p: &Value) -> Result<Value, (i64, String)> {
        if p.get("client").and_then(Value::as_str).is_none() {
            return Err((
                INVALID_PARAMS,
                "unsubscribe: `client` (string) required".into(),
            ));
        }
        self.subs.remove(&conn);
        Ok(json!({ "ok": true }))
    }

    /// Author override opinions into the overrides session layer (REMOTE §3's
    /// third edit face), then re-cook the live stage and publish.
    fn opinion(&mut self, p: &Value) -> Result<Value, (i64, String)> {
        let ops = parse_opinions(p)?;
        if ops.is_empty() {
            return Ok(json!({ "version": self.version }));
        }
        let (Some(stage), Some(session_id)) = (self.stage.clone(), self.session_id.clone()) else {
            return Err(server_error("no scene open; call open first".into()));
        };
        let previous = self.ops.clone();
        for op in &ops {
            if let Err(diags) = apply_opinion(&stage, &session_id, op) {
                // The session layer is now half-edited: rebuild it from the
                // opinions that were recorded as good.
                self.rebuild();
                return Err(server_error(diags_message(&diags)));
            }
            upsert(&mut self.ops, op.clone());
        }
        self.undo.push(Face::Overrides(previous));
        self.redo.clear();
        let pipeline = self.recook();
        self.after_mutation("opinion", &pipeline)?;
        Ok(json!({ "version": self.version }))
    }

    // --- agent tools, with the mutation pipeline hung off them -------------

    fn open(&mut self, p: &Value) -> Result<Value, (i64, String)> {
        let result = self.agent.call("open", p).map_err(server_error)?;
        self.ops.clear();
        self.undo.clear();
        self.redo.clear();
        // Hot reload follows a document opened by path; a document handed in as
        // `content` has no file to watch.
        match p.get("path").and_then(Value::as_str) {
            Some(path) => self.watch_doc(path),
            None => {
                self.watch = None;
                self.reload_due = None;
            }
        }
        let pipeline = self.rebuild();
        self.after_mutation("open", &pipeline)?;
        Ok(result)
    }

    fn patch(&mut self, p: &Value) -> Result<Value, (i64, String)> {
        if p.get("ops").and_then(Value::as_array).is_none() {
            return Err((INVALID_PARAMS, "patch: `ops` array required".into()));
        }
        let before = self.agent.doc_text().map(str::to_owned);
        let result = self.agent.call("patch", p).map_err(server_error)?;
        if let Some(text) = before {
            self.undo.push(Face::Doc(text));
        }
        self.redo.clear();
        let pipeline = self.rebuild();
        self.after_mutation("patch", &pipeline)?;
        Ok(result)
    }

    fn undo(&mut self) -> Result<Value, (i64, String)> {
        let Some(face) = self.undo.pop() else {
            return Err(server_error("nothing to undo".into()));
        };
        match face {
            Face::Doc(text) => {
                if let Some(current) = self.agent.doc_text().map(str::to_owned) {
                    self.redo.push(Face::Doc(current));
                }
                self.agent.replace_doc(text);
            }
            Face::Overrides(ops) => {
                let current = std::mem::replace(&mut self.ops, ops);
                self.redo.push(Face::Overrides(current));
            }
        }
        let pipeline = self.rebuild();
        self.after_mutation("undo", &pipeline)?;
        Ok(json!({ "ok": true, "undo_depth": self.undo.len() }))
    }

    fn redo(&mut self) -> Result<Value, (i64, String)> {
        let Some(face) = self.redo.pop() else {
            return Err(server_error("nothing to redo".into()));
        };
        match face {
            Face::Doc(text) => {
                if let Some(current) = self.agent.doc_text().map(str::to_owned) {
                    self.undo.push(Face::Doc(current));
                }
                self.agent.replace_doc(text);
            }
            Face::Overrides(ops) => {
                let current = std::mem::replace(&mut self.ops, ops);
                self.undo.push(Face::Overrides(current));
            }
        }
        let pipeline = self.rebuild();
        self.after_mutation("redo", &pipeline)?;
        Ok(json!({ "ok": true, "redo_depth": self.redo.len() }))
    }

    // --- the pipeline -----------------------------------------------------

    /// Rebuild the authority stack from the document: lower with a session
    /// layer, replay the opinions, cook, and hand the stage to the agent so its
    /// own tools see the same scene the viewers do.
    fn rebuild(&mut self) -> Pipeline {
        self.agent.set_live(None);
        self.stage = None;
        self.session_id = None;
        // The tree describes the stage that was just dropped: a later
        // `comp_node` must not answer out of it.
        self.tree = None;
        // A different root layer means the memoized cook layers are not in this
        // stage: a memo hit would skip the insert and leave a hole, so the
        // content-hash cache starts over with the stage. Its op list goes with it
        // — those `layer_id`s name layers this rebuild will not reuse.
        self.cook = rusd_proc::default_registry();
        self.cook_ops.clear();

        let Some(text) = self.agent.doc_text().map(str::to_owned) else {
            return Pipeline {
                diags: Vec::new(),
                ok: false,
            };
        };
        let doc = match rusd_core::syntax::parse(&text) {
            Ok(doc) => doc,
            Err(diags) => return Pipeline::failed(diags),
        };
        let validate = rusd_scene::validate(&doc);
        if validate.iter().any(|d| d.severity == Severity::Error) {
            return Pipeline::failed(validate);
        }
        let stage = match rusd_scene::lower_with_session(&doc) {
            Ok(stage) => stage,
            Err(diags) => return Pipeline::failed(diags),
        };
        let session_id = stage.session_layer().map(|l| l.identifier().to_owned());
        if let Some(session_id) = session_id.as_deref() {
            for op in &self.ops {
                if let Err(diags) = apply_opinion(&stage, session_id, op) {
                    return Pipeline::failed(diags);
                }
            }
        }
        let root_id = stage.root_layer().identifier().to_owned();
        let nodes = rusd_proc::nodes_from_doc(&doc);
        let result = self.cook.cook(&stage, &root_id, &nodes);
        let stats = result.stats;
        let diags = result.diags;
        // The ops of *this* pass, wholesale (PROTOCOL v1.5.1): a rebuild mints
        // new output layers, so the previous list's `layer_id`s are gone. A cook
        // that failed to produce a layer for a node simply has no op for it, and
        // the tree falls back to `null` for that layer.
        self.cook_ops = result.ops;
        let prim_count = rusd_scene::prim_count(&stage);
        self.live(stage.clone(), stats, &diags, prim_count, nodes.len());
        self.stage = Some(stage);
        self.session_id = session_id;
        Pipeline { diags, ok: true }
    }

    /// Re-cook over the live stage after an in-place session-layer edit: nodes
    /// whose inputs did not move are memo hits (D6).
    fn recook(&mut self) -> Pipeline {
        let (Some(stage), Some(text)) =
            (self.stage.clone(), self.agent.doc_text().map(str::to_owned))
        else {
            return Pipeline {
                diags: Vec::new(),
                ok: false,
            };
        };
        let doc = match rusd_core::syntax::parse(&text) {
            Ok(doc) => doc,
            Err(diags) => return Pipeline::failed(diags),
        };
        let root_id = stage.root_layer().identifier().to_owned();
        let nodes = rusd_proc::nodes_from_doc(&doc);
        let result = self.cook.cook(&stage, &root_id, &nodes);
        let stats = result.stats;
        let diags = result.diags;
        // Wholesale, as in `rebuild`: memo hits keep their output layer, so this
        // pass still lists them — a reused node is an op of the cook (v1.5.1).
        self.cook_ops = result.ops;
        let prim_count = rusd_scene::prim_count(&stage);
        self.live(stage, stats, &diags, prim_count, nodes.len());
        Pipeline { diags, ok: true }
    }

    fn live(
        &mut self,
        stage: Stage,
        stats: rusd_proc::CookStats,
        diags: &[Diagnostic],
        prim_count: usize,
        node_count: usize,
    ) {
        self.agent.set_live(Some(ExpandOutput {
            stage,
            stats,
            diags: diags.to_vec(),
            prim_count,
            node_count,
        }));
    }

    /// Publish the new authority state: bump the version, and hand every
    /// subscriber the scene and the reason. Snapshots are built at broadcast
    /// time, one per distinct preview state (PROTOCOL v1.4).
    fn after_mutation(&mut self, reason: &str, pipeline: &Pipeline) -> Result<(), (i64, String)> {
        if !pipeline.ok {
            self.pending
                .push(Pending::Line(diagnostics_line(&pipeline.diags)));
            return Ok(());
        }
        let version = self.version + 1;
        // Build the final composition once here, so a flatten failure is this
        // request's error and no version is published for a scene no snapshot
        // can carry (PROTOCOL 「合成失败的 mutation」). The line is reused for
        // every subscriber that did not ask for a preview.
        let final_line = self.snapshot_line(version, None).map_err(server_error)?;
        self.version = version;
        self.pending.push(Pending::Snapshot {
            version,
            final_line,
        });
        if !pipeline.diags.is_empty() {
            self.pending
                .push(Pending::Line(diagnostics_line(&pipeline.diags)));
        }
        self.pending.push(Pending::Line(to_line(&json!({
            "method": "state_changed",
            "params": { "version": version, "reason": reason },
        }))));
        Ok(())
    }

    /// The Tier A snapshot: the stage — or, for a preview, the composition that
    /// stops at `upto` — flattened into one `usdc` layer, base64 on the wire.
    ///
    /// A preview is built by muting the cook output layers **after** `upto` in
    /// cook order, flattening, and unmuting them again. Muting is the stage's own
    /// composition control (`Stage::mute_layer`), so the prefix is composed by
    /// the same engine that composes the whole — there is no second pipeline to
    /// keep in step. The layers come back exactly as they were, so a caller's
    /// final composition is untouched by a client's preview.
    ///
    /// The root stop (`upto` is [`PREVIEW_ROOT`], `-1`) mutes **every** cook
    /// output layer, so what composes is the document's own scene plus the
    /// session overrides: the first of the prefixes PROTOCOL v1.5.3 requires to
    /// be reachable, and the one that shows the pipeline's input.
    fn snapshot_line(&self, version: u64, upto: Option<isize>) -> Result<String, String> {
        let Some(stage) = self.stage.as_ref() else {
            return Err("no scene open; nothing to snapshot".into());
        };
        let root_id = stage.root_layer().identifier().to_owned();
        let ops = crate::composition::cook_output_layers(stage);
        let muted: Vec<String> = match upto {
            // Nothing has cooked yet at the root stop, so every op layer goes.
            Some(PREVIEW_ROOT) => ops.clone(),
            Some(position) => ops.iter().skip(position as usize + 1).cloned().collect(),
            None => Vec::new(),
        };
        for id in &muted {
            stage.mute_layer(id.clone());
        }
        let flattened = stage.flatten();
        for id in &muted {
            stage.unmute_layer(id);
        }
        let flat = flattened.map_err(|e| format!("flatten failed: {e}"))?;
        let format = sdf::LayerRegistry::find_by_id("usdc")
            .ok_or("the usdc file format is not registered")?;
        let written = format
            .write_bytes(flat.data())
            .map_err(|e| format!("usdc write failed: {e}"))?;
        let mut params = json!({
            "version": version,
            "format": "usdc",
            "data": base64_encode(&written.bytes),
        });
        // `upto` is absent for the final composition (PROTOCOL v1.4): the
        // snapshot then means the whole scene, which is what a client that never
        // asked for a preview always gets. A preview names the layer it stops
        // at: the root layer for the root stop (v1.5.3), the op layer otherwise.
        match upto {
            Some(PREVIEW_ROOT) => params["upto"] = json!(root_id),
            Some(position) => {
                if let Some(id) = ops.get(position as usize) {
                    params["upto"] = json!(id);
                }
            }
            None => {}
        }
        Ok(to_line(&json!({
            "method": "stage_snapshot",
            "params": params,
        })))
    }

    /// Broadcast what the request being handled caused, and empty the pending
    /// list (PROTOCOL 「消息顺序」: response, then snapshot, then diagnostics,
    /// then `state_changed`).
    ///
    /// A snapshot is built once per **distinct preview state** among the
    /// subscribers, not once per connection: a preview is a flatten of the whole
    /// stage, and two viewers watching the same prefix must not pay for it twice.
    /// [`PREVIEW_ROOT`] is an ordinary key here — several viewers on the root
    /// stop share one flatten like any other prefix.
    fn publish(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        if pending.is_empty() {
            return;
        }
        // Which preview state each subscriber is in, resolved once: this also
        // drains a state the rebuilt cook no longer reaches.
        let conns: Vec<u64> = self.subs.keys().copied().collect();
        let mut states: BTreeMap<u64, Option<isize>> = conns
            .into_iter()
            .map(|conn| (conn, self.preview_position(conn)))
            .collect();
        // One preview line per state that some subscriber is actually in, built
        // once however many connections watch it: a preview is a flatten of the
        // whole stage, which is the one cost worth sharing here.
        let mut previews: BTreeMap<isize, String> = BTreeMap::new();
        for item in &pending {
            let Pending::Snapshot { version, .. } = item else {
                continue;
            };
            // Collected first: a failure below rewrites `states`.
            let wanted: Vec<isize> = states.values().flatten().copied().collect();
            for position in wanted {
                if previews.contains_key(&position) {
                    continue;
                }
                match self.snapshot_line(*version, Some(position)) {
                    Ok(line) => {
                        previews.insert(position, line);
                    }
                    Err(message) => {
                        // A prefix that will not flatten: those connections fall
                        // back to the final composition rather than keep a stale
                        // scene, and the failure is logged rather than published
                        // — a client's own `preview` call is where it surfaces.
                        eprintln!("rusd serve: preview at op {position}: {message}");
                        for (conn, upto) in states.iter_mut() {
                            if *upto == Some(position) {
                                *upto = None;
                                self.preview.remove(conn);
                            }
                        }
                    }
                }
            }
        }
        let mut gone: Vec<u64> = Vec::new();
        for (conn, upto) in &states {
            for item in &pending {
                let line = match item {
                    Pending::Line(line) => line.clone(),
                    Pending::Snapshot { final_line, .. } => match upto {
                        None => final_line.clone(),
                        Some(position) => match previews.get(position) {
                            Some(line) => line.clone(),
                            None => continue,
                        },
                    },
                };
                let Some(out) = self.subs.get(conn) else {
                    break;
                };
                if out.send(line).is_err() {
                    gone.push(*conn);
                    break;
                }
            }
        }
        for conn in gone {
            self.subs.remove(&conn);
            self.preview.remove(&conn);
        }
    }
}

// --- opinions -------------------------------------------------------------

/// Parse `opinion{ops:[{path, field, value, op}]}` (PROTOCOL §viewer 专用).
fn parse_opinions(p: &Value) -> Result<Vec<Opinion>, (i64, String)> {
    let ops = p
        .get("ops")
        .and_then(Value::as_array)
        .ok_or((INVALID_PARAMS, "opinion: `ops` array required".to_owned()))?;
    let mut out = Vec::with_capacity(ops.len());
    for (i, op) in ops.iter().enumerate() {
        let path = op.get("path").and_then(Value::as_str).ok_or((
            INVALID_PARAMS,
            format!("opinion: ops[{i}].path (string) required"),
        ))?;
        let field = op.get("field").and_then(Value::as_str).ok_or((
            INVALID_PARAMS,
            format!("opinion: ops[{i}].field (string) required"),
        ))?;
        let kind = op.get("op").and_then(Value::as_str).ok_or((
            INVALID_PARAMS,
            format!("opinion: ops[{i}].op (\"set\"|\"remove\") required"),
        ))?;
        let value = match kind {
            "set" => OpinionValue::Set(op.get("value").cloned().ok_or((
                INVALID_PARAMS,
                format!("opinion: ops[{i}] is a `set` and needs a `value`"),
            ))?),
            "remove" => OpinionValue::Remove,
            other => {
                return Err((
                    INVALID_PARAMS,
                    format!("opinion: ops[{i}].op is \"{other}\"; expected \"set\" or \"remove\""),
                ));
            }
        };
        out.push(Opinion {
            path: path.to_owned(),
            field: field.to_owned(),
            value,
        });
    }
    Ok(out)
}

/// Remember `op` as the current opinion on its slot: a later `set` replaces the
/// earlier one and a `remove` drops it, so the override list stays one entry per
/// prim+field and replaying it reproduces the session layer.
fn upsert(ops: &mut Vec<Opinion>, op: Opinion) {
    match op.value {
        OpinionValue::Remove => ops.retain(|o| !o.same_slot(&op)),
        OpinionValue::Set(_) => match ops.iter_mut().find(|o| o.same_slot(&op)) {
            Some(slot) => *slot = op,
            None => ops.push(op),
        },
    }
}

/// Author one opinion into the overrides session layer. The session layer is the
/// strongest opinion in the stack, so it overrides both the document and the
/// cooked layers beneath it.
fn apply_opinion(stage: &Stage, session_id: &str, op: &Opinion) -> Result<(), Vec<Diagnostic>> {
    let prim = sdf::path(&op.path)
        .map_err(|e| vec![Diagnostic::error("E_OPINION", format!("{}: {e}", op.path))])?;
    let attr_path = format!("{}.{}", op.path, op.field);
    let path = sdf::path(&attr_path)
        .map_err(|e| vec![Diagnostic::error("E_OPINION", format!("{attr_path}: {e}"))])?;
    // Read the composed declaration before borrowing the session layer: the
    // session layer's RefMut is held for the whole edit below.
    let (value, type_name) = match &op.value {
        OpinionValue::Remove => (None, None),
        OpinionValue::Set(v) => {
            let ty = attribute_type(stage, &prim, &op.field).unwrap_or_else(|| infer_type(v));
            let value = json_to_sdf(v, &ty)?;
            (Some(value), Some(ty))
        }
    };
    let mut session = stage.layer_mut(session_id).ok_or_else(|| {
        vec![Diagnostic::error(
            "E_SESSION",
            "the stage has no overrides session layer",
        )]
    })?;
    session
        .edit(|l| -> Result<(), sdf::AuthoringError> {
            let d = l.data_mut();
            match value {
                None => {
                    d.erase_field(&path, "default");
                    Ok(())
                }
                Some(value) => {
                    if d.has_spec(&path) {
                        d.set_field(&path, "default", value);
                        Ok(())
                    } else {
                        let mut attr = AttributeSpec::new(
                            d,
                            path.as_str(),
                            type_name.as_deref().unwrap_or("double"),
                            Variability::Uniform,
                            false,
                        )?;
                        attr.set_default(value)?;
                        Ok(())
                    }
                }
            }
        })
        .map(|_| ())
        .map_err(|e| vec![Diagnostic::error("E_OPINION", e.to_string())])
}

/// The composed attribute's declared type, if the stack already has one.
fn attribute_type(stage: &Stage, prim: &sdf::Path, field: &str) -> Option<String> {
    let prim = stage.prim(prim.clone()).ok()?;
    prim.attribute(field)
        .type_name()
        .ok()
        .flatten()
        .map(|t| t.as_str().to_owned())
}

/// The type to declare for an attribute no layer has yet.
fn infer_type(v: &Value) -> String {
    match v {
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "double",
        Value::Number(_) => "int",
        Value::String(_) => "string",
        Value::Array(items) => {
            if triple(v).is_some() {
                "vector3f"
            } else if items.iter().all(Value::is_string) {
                "string[]"
            } else {
                "double[]"
            }
        }
        _ => "double",
    }
    .to_owned()
}

/// Convert an opinion's JSON value to the USD value its declared type needs.
fn json_to_sdf(v: &Value, ty: &str) -> Result<sdf::Value, Vec<Diagnostic>> {
    let bad = |m: String| vec![Diagnostic::error("E_OPINION", m)];
    match ty {
        "point3f" | "vector3f" | "float3" | "normal3f" | "color3f" | "texCoord3f" => {
            let a = triple(v).ok_or_else(|| bad(format!("{ty} needs three numbers")))?;
            Ok(sdf::Value::Vec3f(gf::vec3f(a[0], a[1], a[2])))
        }
        "double" => Ok(sdf::Value::Double(
            v.as_f64()
                .ok_or_else(|| bad(format!("{ty} needs a number")))?,
        )),
        "float" => Ok(sdf::Value::Float(
            v.as_f64()
                .ok_or_else(|| bad(format!("{ty} needs a number")))? as f32,
        )),
        "int" => Ok(sdf::Value::Int(
            v.as_i64()
                .ok_or_else(|| bad(format!("{ty} needs an integer")))? as i32,
        )),
        "int64" => Ok(sdf::Value::Int64(
            v.as_i64()
                .ok_or_else(|| bad(format!("{ty} needs an integer")))?,
        )),
        "uint" => Ok(sdf::Value::Uint(
            v.as_u64()
                .ok_or_else(|| bad(format!("{ty} needs an integer")))? as u32,
        )),
        "uint64" => Ok(sdf::Value::Uint64(
            v.as_u64()
                .ok_or_else(|| bad(format!("{ty} needs an integer")))?,
        )),
        "bool" => Ok(sdf::Value::Bool(
            v.as_bool()
                .ok_or_else(|| bad(format!("{ty} needs a boolean")))?,
        )),
        "string" => Ok(sdf::Value::String(
            v.as_str()
                .ok_or_else(|| bad(format!("{ty} needs a string")))?
                .to_owned(),
        )),
        "token" => Ok(sdf::Value::Token(
            v.as_str()
                .ok_or_else(|| bad(format!("{ty} needs a string")))?
                .into(),
        )),
        "asset" => Ok(sdf::Value::AssetPath(
            v.as_str()
                .ok_or_else(|| bad(format!("{ty} needs a string")))?
                .into(),
        )),
        "double[]" | "float[]" | "int[]" | "int64[]" | "string[]" | "token[]" => {
            let items = v
                .as_array()
                .ok_or_else(|| bad(format!("{ty} needs an array")))?;
            array_to_sdf(items, ty)
        }
        other => Err(bad(format!(
            "an opinion cannot author a `{other}` attribute (supported: point3f, vector3f, \
             float3, normal3f, color3f, double, float, int, int64, uint, uint64, bool, string, \
             token, asset, and their arrays)"
        ))),
    }
}

fn array_to_sdf(items: &[Value], ty: &str) -> Result<sdf::Value, Vec<Diagnostic>> {
    let bad = |m: String| vec![Diagnostic::error("E_OPINION", m)];
    let nums = |f: fn(&Value) -> Option<f64>| -> Result<Vec<f64>, Vec<Diagnostic>> {
        items
            .iter()
            .map(|v| f(v).ok_or_else(|| bad(format!("{ty} needs an array of numbers"))))
            .collect()
    };
    match ty {
        "double[]" => Ok(sdf::Value::DoubleVec(nums(Value::as_f64)?)),
        "float[]" => Ok(sdf::Value::FloatVec(
            nums(Value::as_f64)?.into_iter().map(|n| n as f32).collect(),
        )),
        "int[]" => Ok(sdf::Value::IntVec(
            items
                .iter()
                .map(|v| {
                    v.as_i64()
                        .map(|n| n as i32)
                        .ok_or_else(|| bad(format!("{ty} needs an array of integers")))
                })
                .collect::<Result<_, _>>()?,
        )),
        "int64[]" => Ok(sdf::Value::Int64Vec(
            items
                .iter()
                .map(|v| {
                    v.as_i64()
                        .ok_or_else(|| bad(format!("{ty} needs an array of integers")))
                })
                .collect::<Result<_, _>>()?,
        )),
        "string[]" => Ok(sdf::Value::StringVec(
            items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| bad(format!("{ty} needs an array of strings")))
                })
                .collect::<Result<_, _>>()?,
        )),
        "token[]" => Ok(sdf::Value::TokenVec(
            items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(Into::into)
                        .ok_or_else(|| bad(format!("{ty} needs an array of strings")))
                })
                .collect::<Result<_, Vec<Diagnostic>>>()?,
        )),
        other => Err(bad(format!("unhandled array type {other}"))),
    }
}

fn triple(v: &Value) -> Option<[f32; 3]> {
    let a = v.as_array()?;
    if a.len() != 3 {
        return None;
    }
    Some([
        a[0].as_f64()? as f32,
        a[1].as_f64()? as f32,
        a[2].as_f64()? as f32,
    ])
}

// --- JSON-RPC and wire helpers -------------------------------------------

fn ok_reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err_reply(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn server_error(message: String) -> (i64, String) {
    (SERVER_ERROR, message)
}

fn diagnostics_line(diags: &[Diagnostic]) -> String {
    to_line(&json!({
        "method": "diagnostics",
        "params": { "items": crate::diags_json(diags) },
    }))
}

fn diags_message(diags: &[Diagnostic]) -> String {
    serde_json::to_string(&crate::diags_json(diags)).unwrap_or_else(|_| "diagnostics".to_owned())
}

fn to_line(v: &Value) -> String {
    serde_json::to_string(v).expect("a JSON value always serializes")
}

fn send(out: &mpsc::Sender<String>, v: &Value) {
    let _ = out.send(to_line(v));
}

// --- base64 (the `stage_snapshot` payload) --------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode `bytes` as standard base64 with padding, the wire form of a
/// `stage_snapshot`'s `data`.
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b1 = u32::from(chunk[0]);
        let b2 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b3 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b1 << 16) | (b2 << 8) | b3;
        out.push(char::from(B64[(n >> 18) as usize & 63]));
        out.push(char::from(B64[(n >> 12) as usize & 63]));
        out.push(if chunk.len() > 1 {
            char::from(B64[(n >> 6) as usize & 63])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(B64[n as usize & 63])
        } else {
            '='
        });
    }
    out
}

/// Decode standard base64, tolerating whitespace; `None` if it is not base64.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut pad = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'=' => {
                pad += 1;
                continue;
            }
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => B64.iter().position(|&c| c == byte)? as u32,
        };
        if pad > 0 {
            return None;
        }
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    if bits >= 6 || pad > 2 {
        return None;
    }
    Some(out)
}
