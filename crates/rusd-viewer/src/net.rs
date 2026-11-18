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

/// The code a read that found nothing to say returns when the caller asked with a
/// deadline ([`ProtocolClient::read_within`]). It is **not** an error: the caller's
/// loop uses it to go round again and drain the commands it has been given. Every
/// other error code means the connection is over.
pub const E_WIRE_IDLE: &str = "E_WIRE_IDLE";

/// The code a request returns when its reply did not arrive before the caller's
/// deadline.
///
/// **A timeout is not a broken connection.** The peer may answer the next request
/// perfectly well, and the notifications it pushes in the meantime are still
/// wanted, so a caller reports this one request as failed and keeps its socket.
/// The alternative — parking on the channel until the server speaks — is what
/// wedged the network thread: with no deadline, one unanswered request stopped it
/// draining commands *and* stopped every `stage_snapshot` behind it, so the
/// window had no frame to draw and sat at 0% CPU.
pub const E_WIRE_TIMEOUT: &str = "E_WIRE_TIMEOUT";

/// How long a greeting (`hello` + `subscribe`) may take before the viewer gives up
/// on that server.
///
/// Longer than a city snapshot's cook-and-flatten (measured ~0.2 s) by a wide
/// margin, short enough that a server which accepts a connection and then says
/// nothing ends as one diagnostic instead of a permanently silent client. The
/// window never waits for this: the greeting runs on the network thread.
pub const GREETING_DEADLINE: Duration = Duration::from_secs(5);

/// How long one request (`opinion`, `undo`, `composition_tree`, `capture`, …) may
/// wait for its reply.
///
/// A `composition_tree` on city is a structural read of the authority stack; ten
/// seconds is far beyond it. What matters is that it is **bounded**: the tree panel
/// already knows how to degrade, and a caller that gets [`E_WIRE_TIMEOUT`] can say
/// so, while a caller parked on `recv` cannot say anything.
pub const REPLY_DEADLINE: Duration = Duration::from_secs(10);

/// The synchronous wire client: one connection, one request id counter.
///
/// ## Why a reader thread
///
/// Reading is done by a **thread of its own**, which pushes whole lines into a
/// channel; this type only writes and consumes. The reason is not throughput: a
/// client that read on the calling thread could not send while it read, so a
/// command handed to it during a quiet moment — an `undo`, a camera drag's
/// `opinion`, a `composition_tree` request — waited for the server to say something
/// unrelated first. That starved the window and deadlocked the tests.
///
/// A read timeout on the socket would have been the other way out, and it is
/// **wrong** here: a `stage_snapshot` line is ~58 KB and arrives in pieces, so a
/// timeout that fires mid-line would truncate it (or, with `read_line`, silently
/// swallow the fragments up to the next newline). A thread that blocks on the
/// socket as long as it likes and hands over complete lines loses nothing.
pub struct ProtocolClient {
    writer: TcpStream,
    /// Complete lines from the reader thread, or the reason it stopped.
    lines: std::sync::mpsc::Receiver<Result<String, String>>,
    next_id: u64,
    /// Notifications that arrived while a request was waiting for its reply
    /// ([`ProtocolClient::take_early`]).
    early: Vec<Notification>,
    /// The peer address, for messages.
    pub addr: String,
    /// How long one request's reply is waited for (`REPLY_DEADLINE` unless a test
    /// shortened it).
    reply_deadline: Duration,
}

impl ProtocolClient {
    /// Connect to `addr` (`host:port`) and answer the greeting: `hello`, then
    /// `subscribe`. Returns the client and everything the server said before the
    /// greeting was done (its `hello` reply, and any notification that raced it).
    ///
    /// The greeting is bounded by [`GREETING_DEADLINE`]: a peer that accepts the
    /// connection and then says nothing ends as a diagnostic, not as a client
    /// parked in `recv` forever.
    pub fn open(addr: &str) -> Result<(Self, Vec<Notification>), Vec<Diagnostic>> {
        Self::open_within(addr, GREETING_DEADLINE)
    }

    /// [`ProtocolClient::open`] with the greeting's deadline named outright.
    ///
    /// A test uses this to prove that a silent server ends in
    /// [`E_WIRE_TIMEOUT`] rather than in a hung thread; the viewer passes
    /// [`GREETING_DEADLINE`].
    pub fn open_within(
        addr: &str,
        deadline: Duration,
    ) -> Result<(Self, Vec<Notification>), Vec<Diagnostic>> {
        let deadline_at = std::time::Instant::now() + deadline;
        let sock_addr = addr
            .to_socket_addrs()
            .map_err(|e| vec![diag("E_CONNECT", format!("{addr}: {e}"))])?
            .next()
            .ok_or_else(|| vec![diag("E_CONNECT", format!("{addr}: resolves to no address"))])?;
        let stream = TcpStream::connect_timeout(&sock_addr, deadline)
            .map_err(|e| vec![diag("E_CONNECT", format!("{addr}: {e}"))])?;
        stream
            .set_nodelay(true)
            .map_err(|e| vec![diag("E_CONNECT", format!("{addr}: {e}"))])?;
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| vec![diag("E_CONNECT", format!("{addr}: {e}"))])?,
        );
        let (tx, lines) = std::sync::mpsc::channel();
        // The reader thread: blocks on the socket for as long as it takes, and hands
        // over whole lines. A connection that closes ends it with one final error.
        std::thread::spawn(move || {
            let mut reader = reader;
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let _ = tx.send(Err("the server closed the connection".to_owned()));
                        return;
                    }
                    Ok(_) => {
                        if tx.send(Ok(line)).is_err() {
                            return; // nobody is reading any more
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(format!("read failed: {e}")));
                        return;
                    }
                }
            }
        });
        let mut client = Self {
            writer: stream,
            lines,
            next_id: 1,
            early: Vec::new(),
            addr: addr.to_owned(),
            reply_deadline: REPLY_DEADLINE,
        };
        let mut early = Vec::new();
        let hello = client.call_within(&protocol::hello_line(1), deadline_at)?;
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
        let sub = client.call_within(&protocol::subscribe_line(2), deadline_at)?;
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
    /// that arrives first. There is no deadline: use [`ProtocolClient::call_within`]
    /// unless the caller owns a thread it may park forever.
    #[allow(dead_code)]
    fn call(&mut self, line: &str) -> Result<Call, Vec<Diagnostic>> {
        self.call_within(line, std::time::Instant::now() + REPLY_DEADLINE)
    }

    /// Send one line and wait for the reply to its `id` **until `by`**, keeping any
    /// notification that arrives first.
    ///
    /// A reply that does not arrive before `by` returns [`E_WIRE_TIMEOUT`]. The
    /// connection stays open: the reader thread still holds the socket, and the
    /// caller keeps its reader, so a late reply is dropped as an unsolicited one
    /// and the next request works. That is the difference between a server that is
    /// slow and a client that is wedged, and the network thread must tell them
    /// apart — it is the thread everything else waits behind.
    fn call_within(&mut self, line: &str, by: std::time::Instant) -> Result<Call, Vec<Diagnostic>> {
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
            let left = by.saturating_duration_since(std::time::Instant::now());
            let wire = match self.read_within(left) {
                Ok(w) => w,
                // Nothing arrived before the deadline: this one request failed and
                // the connection lives on. A read that had time left cannot end
                // this way, so an idle answer here *is* the timeout.
                Err(diags) if diags.iter().all(|d| d.code == E_WIRE_IDLE) => {
                    // **The notifications that did arrive are kept.** A snapshot
                    // that raced the timed-out request is not the server's reply,
                    // and throwing it away would make one slow request swallow the
                    // frame it had already been sent.
                    self.early.extend(early);
                    return Err(vec![diag(
                        E_WIRE_TIMEOUT,
                        format!("{}: no reply to id {id} in time", self.addr),
                    )]);
                }
                Err(diags) => return Err(diags),
            };
            match wire {
                Wire::Response(r) if r.id == id => {
                    return Ok(Call {
                        result: r.result,
                        early,
                    });
                }
                // A reply to a request that has already timed out: its caller is
                // gone and the connection is not at fault, so it is dropped
                // rather than reported as a disagreement.
                Wire::Response(_) => continue,
                Wire::Notification(n) => early.push(n),
            }
        }
    }

    /// Block until one line arrives (a notification or a reply) and parse it.
    ///
    /// **This has no deadline and a caller on a thread others wait behind must not
    /// use it.** A request uses [`ProtocolClient::call_within`], which reads with a
    /// time limit; the network thread's idle read uses
    /// [`ProtocolClient::read_within`]. Only a test that owns its thread can park
    /// here forever. A silent server makes `read_within` return [`E_WIRE_IDLE`]
    /// instead; every other error means the connection is over. A half-read line
    /// stays in the reader thread's buffer across a timeout, so nothing is lost when
    /// the line arrives late.
    pub fn read_wire(&mut self) -> Result<Wire, Vec<Diagnostic>> {
        match self.lines.recv() {
            Ok(Ok(line)) => {
                protocol::parse_line(line.trim_end_matches(['\r', '\n'])).map_err(|d| vec![d])
            }
            Ok(Err(why)) => Err(vec![diag("E_IO", format!("{}: {why}", self.addr))]),
            // The reader thread is gone without a reason: the socket is no more.
            Err(_) => Err(vec![diag(
                "E_IO",
                format!("{}: the connection ended", self.addr),
            )]),
        }
    }

    /// Wait up to `timeout` for one line, answering [`E_WIRE_IDLE`] when the server
    /// had nothing to say.
    ///
    /// This is what lets a caller keep sending while the server is quiet: the
    /// **reading** is on its own thread ([`ProtocolClient`]), so the wait is a
    /// timeout on a channel rather than on the socket — no line can be truncated by
    /// it, and a command the caller queued in the meantime goes out on the next
    /// round.
    pub fn read_within(&mut self, timeout: Duration) -> Result<Wire, Vec<Diagnostic>> {
        match self.lines.recv_timeout(timeout) {
            Ok(Ok(line)) => {
                protocol::parse_line(line.trim_end_matches(['\r', '\n'])).map_err(|d| vec![d])
            }
            Ok(Err(why)) => Err(vec![diag("E_IO", format!("{}: {why}", self.addr))]),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(vec![diag(
                E_WIRE_IDLE,
                format!("{}: nothing arrived", self.addr),
            )]),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(vec![diag(
                "E_IO",
                format!("{}: the connection ended", self.addr),
            )]),
        }
    }

    /// Send `opinion{ops}`, waiting for the server's `{version}`. Any
    /// notification that raced the reply is dropped with it: a fresh snapshot
    /// follows the server's own re-composition, and the caller renders that.
    pub fn opinion(&mut self, ops: &[OpinionOp]) -> Result<Response, Vec<Diagnostic>> {
        let line = protocol::opinion_line(self.next_id, ops);
        let Call { result, .. } = self.call_within(&line, self.reply_deadline())?;
        self.next_id += 1;
        Ok(Response {
            id: self.next_id - 1,
            result,
        })
    }

    /// Send `undo` or `redo`.
    pub fn edit(&mut self, redo: bool) -> Result<Response, Vec<Diagnostic>> {
        let line = protocol::edit_line(self.next_id, redo);
        let Call { result, .. } = self.call_within(&line, self.reply_deadline())?;
        self.next_id += 1;
        Ok(Response {
            id: self.next_id - 1,
            result,
        })
    }

    /// The deadline a request's reply is given: [`REPLY_DEADLINE`] from now.
    ///
    /// One place, so every request a caller sends has the same bound and a test
    /// that shortens it changes them all together.
    fn reply_deadline(&self) -> std::time::Instant {
        std::time::Instant::now() + self.reply_deadline
    }

    /// Send any request and answer with the server's JSON-RPC result or error.
    ///
    /// This is what `composition_tree` / `comp_node` / `capture` go through: they
    /// are ordinary requests whose failure is part of their answer (a `-32601`
    /// from a server without the method must reach the caller, not become a
    /// diagnostic about a closed connection). Notifications that raced the reply
    /// are returned with it, so a caller that got a snapshot on the way is not
    /// made to wait for the next read.
    pub fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<Result<serde_json::Value, protocol::RpcError>, Vec<Diagnostic>> {
        let line = protocol::request_line(self.next_id, method, params);
        let Call { result, early } = self.call_within(&line, self.reply_deadline())?;
        self.next_id += 1;
        self.early.extend(early);
        Ok(result)
    }

    /// Shorten (or lengthen) how long a request's reply is waited for.
    ///
    /// A test uses a few hundred milliseconds to prove that a server which never
    /// answers ends its request as [`E_WIRE_TIMEOUT`]; the viewer keeps the
    /// default `REPLY_DEADLINE`.
    pub fn set_reply_deadline(&mut self, deadline: Duration) {
        self.reply_deadline = deadline;
    }

    /// Take any notification that arrived while a [`ProtocolClient::request`] was
    /// waiting for its reply.
    pub fn take_early(&mut self) -> Vec<Notification> {
        std::mem::take(&mut self.early)
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

/// Every diagnostic's `code: message`, joined — what a [`NetEvent::Reply`] carries
/// when the failure was this side's (a timeout has no server message of its own).
fn join_codes(diags: &[Diagnostic]) -> String {
    diags
        .iter()
        .map(|d| format!("{}: {}", d.code, d.message))
        .collect::<Vec<_>>()
        .join("; ")
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
    /// Any other request of the protocol, answered as [`NetEvent::Reply`] under
    /// the caller's own `tag`: the viewer's tree panel (`composition_tree`,
    /// `comp_node`) and its capture (`capture`) are not edits, so they must not
    /// be reported as [`NetEvent::Applied`] — a capture does not move the scene
    /// version (`docs/PROTOCOL.md` v1.1).
    ///
    /// `tag` is opaque to the worker: it goes back with the reply so the caller
    /// can tell which of its own requests this is, without a second table.
    Call {
        tag: String,
        method: String,
        params: serde_json::Value,
    },
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
    /// A [`NetCommand::Call`] was answered. `Err` is the server's JSON-RPC error,
    /// **code included**, because the tree panel has to tell `-32601 method not
    /// found` (a server from before v1.2, which degrades) from every other
    /// failure (which is reported).
    Reply {
        tag: String,
        result: Result<serde_json::Value, protocol::RpcError>,
    },
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
            // **The read below waits only briefly.** With no deadline this loop
            // parked until the server spoke, so a command handed over while the
            // server was quiet — an `undo`, a camera drag's opinion, the tree
            // request — waited for an unrelated notification. A channel timeout is
            // what lets the commands be drained every round; the socket itself is
            // read on the reader thread and is never truncated by this.
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
                    // A `Call` is not an edit: its answer goes back as `Reply`
                    // under the caller's tag. A `-32601` (a server without the
                    // method) is a *reply*, not a failure of the connection: the
                    // caller decides whether it degrades (the tree panel does).
                    let (what, sent) = match cmd {
                        NetCommand::Call {
                            tag,
                            method,
                            params,
                        } => {
                            match client.request(&method, params) {
                                Ok(result) => {
                                    let _ = ev_tx.send(NetEvent::Reply { tag, result });
                                }
                                // **A request that timed out is not a dead
                                // connection.** The reader thread still holds the
                                // socket and notifications are still arriving, so
                                // the request is reported as failed under its tag
                                // and the loop goes on. Ending the connection here
                                // is what left the window with no stream at all
                                // after one slow `composition_tree`.
                                Err(diags) if diags.iter().any(|d| d.code == E_WIRE_TIMEOUT) => {
                                    let _ = ev_tx.send(NetEvent::Reply {
                                        tag,
                                        result: Err(protocol::RpcError {
                                            code: protocol::TIMEOUT_CODE,
                                            message: join_codes(&diags),
                                        }),
                                    });
                                }
                                Err(diags) => {
                                    let _ = ev_tx.send(NetEvent::Closed(diags));
                                    wake();
                                    return;
                                }
                            }
                            // A snapshot that raced the request is delivered
                            // rather than dropped: on a timeout it may be the only
                            // thing that arrived at all.
                            for n in client.take_early() {
                                let _ = ev_tx.send(NetEvent::Notification(n));
                            }
                            continue;
                        }
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
                        Err(diags) if diags.iter().any(|d| d.code == E_WIRE_TIMEOUT) => {
                            // A gizmo sample or an undo the server did not answer in
                            // time: say so and keep the connection. The next edit is
                            // still sendable, and the snapshots the server pushes in
                            // the meantime are still delivered.
                            let _ = ev_tx.send(NetEvent::Failed(vec![diag(
                                E_WIRE_TIMEOUT,
                                format!("{what}: {}", join_codes(&diags)),
                            )]));
                        }
                        Err(diags) => {
                            let _ = ev_tx.send(NetEvent::Closed(diags));
                            wake();
                            return;
                        }
                    }
                    for n in client.take_early() {
                        let _ = ev_tx.send(NetEvent::Notification(n));
                    }
                }
                // **Wake before the blocking read.** The events of this batch — a
                // `Reply` above all — are what the UI must act on, and the read
                // below parks until the server says something else. Waking after it
                // would leave a reply sitting in the channel until an unrelated
                // notification arrived: the tree panel would stay on "请求中…" while
                // its answer was already in hand.
                if sent_any {
                    wake();
                }
                // Then whatever the server pushed. `read_within` returns after a
                // short wait even when the server says nothing, so the loop gets back
                // to the command drain above instead of starving it.
                match client.read_within(Duration::from_millis(20)) {
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
                    Err(diags) if diags.iter().all(|d| d.code == E_WIRE_IDLE) => {
                        // Nothing arrived: go round and send what the UI queued.
                        continue;
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
    /// A camera move: re-render only (usually no upload).
    ///
    /// `only_selected` is what 「只看选中」 asks for: when it is set and `selection`
    /// names a prim, the frame shows that prim's subtree and nothing else. It
    /// travels **with the camera** rather than in a command of its own because it is
    /// a display argument like the orbit -- the two arrive from one user action, and
    /// one batch of commands is one frame (`RenderWorker::spawn`'s drain), so a
    /// separate command would be a second way to ask for the same re-render.
    ///
    /// It **re-bakes** when it differs from what the GPU holds: a filter is a subset
    /// of the vertices, so it cannot be a draw-time uniform. A camera move alone
    /// still uploads nothing.
    View {
        orbit: Orbit,
        selection: Option<String>,
        only_selected: bool,
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
    /// Monotonic count of the frame this is: 1 for the first the thread ever drew.
    /// A UI that shows frame `n` while the thread has produced `m > n` is showing
    /// a stale picture, which is the one question a screenshot cannot answer.
    pub seq: u64,
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
    /// The camera this frame was **drawn with**. Carried because it is the only
    /// way to tell "the view holds the fitted camera" from "the picture on screen
    /// was drawn with an older one": the orbit the UI reports is the one it
    /// *sent*, not necessarily the one the last frame used.
    pub camera: Camera,
}

/// How many produced frames a [`RenderWorker`] remembers for a test or the debug
/// overlay. Bounded, newest last: a long session must not grow a log.
const FRAME_LOG: usize = 32;

/// The render thread: owns the [`Viewport`] and the last scene.
pub struct RenderWorker {
    cmds: Sender<RenderCommand>,
    frames: std::sync::Arc<Latest<Frame>>,
    errors: std::sync::Arc<Latest<Vec<Diagnostic>>>,
    /// Every frame the thread has produced, as `(seq, camera)`, newest last and
    /// bounded by [`FRAME_LOG`]. A test asks it which camera each frame used, and
    /// how many there were — `Latest` alone keeps one and hides the rest.
    produced: std::sync::Arc<std::sync::Mutex<Vec<(u64, Camera)>>>,
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
        let produced = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let f = frames.clone();
        let e = errors.clone();
        let p = produced.clone();
        let primes_wake = Some(wake);
        let tid = thread::spawn(move || {
            let mut viewport = Viewport::new(device, queue, width, height);
            // The last scene, kept so a preview can re-bake it with a prim moved.
            let mut scene: Option<SceneGeometry> = None;
            let mut orbit: Option<Orbit> = None;
            let mut selection: Option<String> = None;
            // What the GPU's geometry is filtered to: the prim's path when 「只看选中」
            // is on and something is selected, `None` for the whole scene. Kept beside
            // `selection` because the two together decide whether a `View` re-bakes.
            let mut isolated: Option<String> = None;
            let mut bounds: ([f64; 3], [f64; 3]) = ([0.0; 3], [0.0; 3]);
            // `dirty` says the display arguments changed since the last frame;
            // `uploaded` says the scene on the GPU is the one in `scene` (it is
            // not after a `Preview`, which re-bakes the same geometry).
            let mut dirty = false;
            let mut seq: u64 = 0;
            loop {
                // **Every pending command is applied before the frame is drawn.**
                // One batch, one render: the app sends `Scene` and then `View`
                // (`app::adopt_scene`) within microseconds, and a loop that
                // rendered between the two would draw the scene's own camera and
                // then wait for an input event to draw the fitted one — the
                // "correct fit, stale picture" the window showed with a single
                // frame on screen. Draining first makes the batch's final camera
                // the one that reaches the frame.
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
                            // **This is the render thread's own camera, and its only
                            // source besides a `View` command.** The composed scene
                            // carries a fitted camera (`scene::extract_fitted` always
                            // sets one), so the thread has a usable orbit the moment a
                            // scene lands — which is why a probe can see a plausible
                            // "fit" here even if the app never sent a `View`. That is
                            // not a second opinion about the framing: the app sends
                            // `View` right after `Scene` (`app::adopt_scene`) with the
                            // camera *it* fitted for the live window aspect, and that
                            // overwrites this one on the next iteration. The value set
                            // here exists so the very first draw after a scene is not
                            // a blank one, and so a scene that arrives before any
                            // `View` still renders something.
                            if let Some(cam) = geometry.camera {
                                orbit = Some(Orbit::from_camera(&cam));
                            }
                            scene = Some(*geometry);
                            // A new scene arrives whole (`set_scene` above), so the filter
                            // the GPU held is gone with it: the next `View` re-applies it
                            // instead of believing it is already there.
                            isolated = None;
                            dirty = true;
                        }
                        RenderCommand::View {
                            orbit: o,
                            selection: sel,
                            only_selected,
                        } => {
                            orbit = Some(o);
                            selection = sel;
                            // The filter is part of what is on the GPU, so a change to it
                            // is a re-bake; an unchanged one costs nothing, which is what
                            // keeps a plain orbit a redraw.
                            let wanted = if only_selected {
                                selection.clone()
                            } else {
                                None
                            };
                            if wanted != isolated {
                                isolated = wanted;
                                if let Some(geo) = &scene {
                                    let filter = isolated.as_deref();
                                    viewport.set_scene(&crate::scene::isolate(geo, filter));
                                }
                            }
                            dirty = true;
                        }
                        RenderCommand::Preview {
                            overrides,
                            selection: sel,
                        } => {
                            if let Some(geo) = &scene {
                                // **The isolation filter survives a drag.** A drag's
                                // preview re-bakes the geometry, and baking the whole
                                // scene here would flash every hidden prim back on
                                // screen for the length of the drag. The filter the
                                // GPU holds is `isolated`, which a `View` sets -- so a
                                // drag under 「只看选中」 re-bakes the same subtree, and
                                // the prim being dragged is in it (it is the
                                // selection).
                                let shown = crate::scene::isolate(geo, isolated.as_deref());
                                viewport.set_scene_with_translates(&shown, &overrides);
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
                // A batch that changed nothing (a timeout with no work) draws
                // nothing: an idle viewer must not burn a core.
                if !dirty {
                    continue;
                }
                // **A batch that changed something always produces a frame** —
                // the `wake` below is what gets it on screen with no input event
                // to follow. Only a camera that does not exist yet (`Scene` with
                // no camera and no `View`) postpones it: `dirty` then stays true
                // and the next `View` renders.
                let Some(o) = orbit else { continue };
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
                        seq += 1;
                        let camera = o.camera();
                        if let Ok(mut log) = p.lock() {
                            log.push((seq, camera));
                            if log.len() > FRAME_LOG {
                                log.remove(0);
                            }
                        }
                        f.put(Frame {
                            seq,
                            width: out.width,
                            height: out.height,
                            beauty: out.beauty,
                            ids: out.ids,
                            paths: out.paths,
                            millis: started.elapsed().as_secs_f64() * 1000.0,
                            camera,
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
            produced,
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

    /// The sequence number of the newest frame the thread has **produced**, taken
    /// or not. `take_frame` hides a frame nobody took; this does not, which is
    /// what lets the overlay say "14 produced / 13 shown" instead of "13/13 fresh"
    /// while a frame is still in flight.
    pub fn produced_seq(&self) -> u64 {
        self.produced
            .lock()
            .ok()
            .and_then(|l| l.last().map(|(s, _)| *s))
            .unwrap_or(0)
    }

    /// Every frame the thread produced recently, as `(seq, camera)`, oldest first
    /// and at most the last [`FRAME_LOG`] of them.
    ///
    /// This is the record a test reads to answer "which camera did the frame on
    /// screen actually use": the orbit the UI holds is the one it *sent*, and a
    /// frame drawn before that send shows an older view in every other observable
    /// channel.
    pub fn produced(&self) -> Vec<(u64, Camera)> {
        self.produced.lock().map(|l| l.clone()).unwrap_or_default()
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
