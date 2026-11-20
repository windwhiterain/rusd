//! The v1.7 control channel: the GUI's every operation as a named method on a
//! **loopback NDJSON socket**, so an agent drives the viewer without computer use
//! (`docs/PROTOCOL.md` v1.7: `view --connect <server> --control <addr>`).
//!
//! The point of the channel is what it is *not*: it is not a second viewer. A method
//! names the click it stands for, and [`crate::app::Shell::on_control`] carries it out
//! by calling the very function the widget calls — `on_tree_layer` for a layer row,
//! `ask_preview` for the preview controls, `toggle_only_selected` for 「只看选中 (i)」,
//! the pick path for `select`. Nothing here reaches into the shell's state; this
//! module parses a line and names a method, and the shell on the UI thread decides
//! what that method is. That is what keeps a script's `tree{id}` and the user's click
//! from drifting into two implementations.
//!
//! The wire is the server's own shape — one JSON object per line, `{"jsonrpc":"2.0",
//! "id":n,"result":{…}}` or `"error":{code,message}` — so a client that speaks to
//! `rusd serve` speaks to this too. Three codes are the contract's:
//! [`METHOD_NOT_FOUND`] for a method nobody has, [`INVALID_PARAMS`] for arguments a
//! method cannot use, [`NOT_CONNECTED`] for a method whose answer needs a server that
//! is not connected. [`PARSE_ERROR`] is JSON-RPC's own for a line that is not a
//! request, and [`LOCAL_IO`] is this channel's one addition (a `capture` whose file
//! could not be written): keeping it distinct is what lets `-32000` keep meaning
//! exactly what the contract says it means.
//!
//! Threads: the listener accepts, and **one thread per connection** reads lines. A
//! request travels to the UI thread over a channel with its own reply channel, and
//! the connection thread waits for that one line — so one connection's requests are
//! answered in the order it sent them, and two connections do not queue behind each
//! other. No control thread ever touches the GPU, the window or the shell; the UI
//! thread is woken through the same `EventLoopProxy` the network and render workers
//! use.
//!
//! **Off by default.** Without `--control` no socket is opened at all, and the
//! address is refused at startup unless it is a loopback one: a control channel a
//! network can reach is a remote-control API for the user's desktop session.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use serde_json::{Map, Value, json};

/// The line is not a JSON-RPC request (JSON-RPC's own code).
pub const PARSE_ERROR: i64 = -32700;
/// Nobody has that method. The message lists the ones this build has.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// The method's arguments cannot be used (missing, wrong type, an id the tree does
/// not hold, a prim the scene does not have).
pub const INVALID_PARAMS: i64 = -32602;
/// The answer needs a server and this viewer has no connection to one.
pub const NOT_CONNECTED: i64 = -32000;
/// This channel's own addition: the viewer has not got a frame or a scene yet, so the
/// answer the method promises does not exist.
///
/// Distinct from [`NOT_CONNECTED`] (there *is* a server, the picture has just not
/// arrived) and from [`LOCAL_IO`] (the picture exists and could not be written), because
/// a script's retry tells them apart: wait longer, retry, or give up.
pub const NOT_READY: i64 = -32003;
/// This channel's own addition: the operation was understood and the viewer could
/// not carry it out locally (a `capture` whose file would not write). Deliberately
/// outside the contract's three, so `-32000` never has to mean two things.
pub const LOCAL_IO: i64 = -32002;

/// Every method v1 of the channel has (`docs/PROTOCOL.md` v1.7's table).
///
/// One list, read by both the refusing side (an unknown method is answered right on
/// the connection thread) and the report, so "what exists" is not spelled twice.
pub const METHODS: [&str; 12] = [
    "state",
    "open",
    "preview",
    "tree",
    "select",
    "isolate",
    "script",
    "translate",
    "fit",
    "orbit",
    "capture",
    "shot",
];

/// Whether `method` is one of [`METHODS`].
pub fn is_known(method: &str) -> bool {
    METHODS.contains(&method)
}

/// One request: its id (echoed back verbatim), its method, and its named arguments.
///
/// `params` is always an object here: a request that carried none gets `{}`, and one
/// that carried something else is refused before it reaches the shell, so no method
/// has to ask "was this a null or an object?".
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub id: Value,
    pub method: String,
    pub params: Map<String, Value>,
}

/// Where a request's answer goes: the id to echo and the connection's writer.
///
/// A **deferred** method (`open`, `preview`) hands one of these to the shell, which
/// keeps it until the server's own reply arrives and answers then
/// ([`crate::app::Shell::on_reply`]). A local method answers it in the same pass.
#[derive(Clone, Debug)]
pub struct Replier {
    pub id: Value,
    reply: Sender<String>,
}

impl Replier {
    /// A replier that writes to `reply` and echoes `id`.
    ///
    /// The listener builds one per request ([`crate::control::spawn`]'s connection thread);
    /// naming it here rather than at that one call site is what keeps the field private to
    /// this module, so no other code can answer a request that nobody asked.
    pub fn new(id: Value, reply: Sender<String>) -> Self {
        Self { id, reply }
    }

    /// A replier with nowhere to write.
    ///
    /// A placeholder only, for a holder that will never answer: the shell builds a
    /// [`Queued`] with the real channel in the same statement, and a test can exercise a
    /// method that sends nothing without a socket behind it.
    #[allow(dead_code)]
    pub fn detached() -> Self {
        let (tx, _rx) = channel();
        Self {
            id: Value::Null,
            reply: tx,
        }
    }

    /// Answer with a result object.
    pub fn ok(&self, result: Value) {
        let _ = self.reply.send(ok(&self.id, result));
    }

    /// Answer with an error.
    pub fn err(&self, code: i64, message: impl AsRef<str>) {
        let _ = self.reply.send(err(&self.id, code, message.as_ref()));
    }

    /// Answer with whichever of the two a method produced.
    pub fn answer(&self, outcome: Result<Value, (i64, String)>) {
        match outcome {
            Ok(v) => self.ok(v),
            Err((code, message)) => self.err(code, message),
        }
    }
}

/// A request the UI thread still has to carry out, with where to write what it says.
pub struct Queued {
    pub req: Request,
    pub reply: Replier,
}

/// Wake the window thread: the control threads hold a clone and call it after queueing
/// a request, so the UI thread never polls for control work.
///
/// A `Mutex` rather than a bare `Fn` because a `Wake` is shared by every connection
/// thread and one request per lock is nothing beside the round trip it is about to
/// take. The closure itself is the same `EventLoopProxy` nudge the network and render
/// workers already use (`crate::net::Wake`'s shape, made shareable).
pub type Nudge = std::sync::Arc<std::sync::Mutex<Box<dyn Fn() + Send>>>;

/// Build a [`Nudge`] from a plain closure.
pub fn nudge(f: impl Fn() + Send + 'static) -> Nudge {
    std::sync::Arc::new(std::sync::Mutex::new(Box::new(f)))
}

/// A running control channel: the address it is **actually** bound to.
///
/// Reported rather than assumed, because `--control 127.0.0.1:0` is the honest way to
/// ask for "any free port", and the caller (a script, the startup log) needs the port
/// the OS picked.
#[derive(Debug)]
pub struct Control {
    pub addr: SocketAddr,
}

/// Open the channel on `addr` and start accepting.
///
/// Refuses a non-loopback address outright: the caller is a desktop session's own
/// window, and a control socket a network can reach would let anyone draw its screen
/// and write files into it.
///
/// A bind failure is the caller's to report (a port already in use is a message, not a
/// crash): it comes back as the `Err` string, already naming the address.
pub fn spawn(addr: &str, queue: Sender<Queued>, wake: Nudge) -> Result<Control, String> {
    let sock = resolve_loopback(addr)?;
    let listener =
        TcpListener::bind(sock).map_err(|e| format!("--control {addr}: cannot listen: {e}"))?;
    let bound = listener
        .local_addr()
        .map_err(|e| format!("--control {addr}: {e}"))?;
    thread::Builder::new()
        .name("viewer-control".to_owned())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else { continue };
                let queue = queue.clone();
                let wake = wake.clone();
                let _ = thread::Builder::new()
                    .name("viewer-control-conn".to_owned())
                    .spawn(move || serve_conn(stream, queue, wake));
            }
        })
        .map_err(|e| format!("--control {addr}: cannot start the listener thread: {e}"))?;
    Ok(Control { addr: bound })
}

/// Read one connection's lines until it closes, answering each.
///
/// The reply is awaited **per request**, which is what keeps a connection's answers in
/// the order it asked. A deferred method (`open`, `preview`) therefore holds this
/// thread until the server answers it — bounded by the network worker's own reply
/// deadline (`crate::startup::REPLY_DEADLINE`), which is what turns a silent server
/// into a `-32001` here instead of a parked socket.
fn serve_conn(stream: TcpStream, queue: Sender<Queued>, wake: Nudge) {
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let mut out = std::io::BufWriter::new(writer);
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let req = match parse_line(&line) {
            Ok(req) => req,
            Err(reply) => {
                if write_line(&mut out, &reply).is_err() {
                    break;
                }
                continue;
            }
        };
        let (tx, rx) = channel::<String>();
        let waited = Replier::new(req.id.clone(), tx);
        if queue.send(Queued { req, reply: waited }).is_err() {
            // The window thread is gone (the window was closed): say so, rather than
            // leave a script waiting on a socket nobody will ever answer.
            let text = err(
                &Value::Null,
                NOT_CONNECTED,
                "the viewer's window has ended; nothing is listening for this request",
            );
            let _ = write_line(&mut out, &text);
            break;
        }
        if let Ok(f) = wake.lock() {
            f();
        }
        match rx.recv() {
            Ok(text) => {
                if write_line(&mut out, &text).is_err() {
                    break;
                }
            }
            // The replier was dropped without an answer: the shell went away mid-request.
            Err(_) => break,
        }
    }
}

fn write_line(out: &mut impl Write, text: &str) -> std::io::Result<()> {
    writeln!(out, "{text}")?;
    out.flush()
}

/// One line, as the channel reads it: a request to carry out, or the reply a line that
/// is not one gets.
///
/// `Err` carries the finished reply text (no newline), so a malformed line is answered
/// on the connection thread and never becomes the UI thread's problem.
pub fn parse_line(line: &str) -> Result<Request, String> {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Err(err(&Value::Null, PARSE_ERROR, format!("parse error: {e}"))),
    };
    let Some(obj) = value.as_object() else {
        return Err(err(
            &Value::Null,
            PARSE_ERROR,
            "a request must be a JSON object",
        ));
    };
    let id = obj.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return Err(err(&id, PARSE_ERROR, "`method` (string) required"));
    };
    if !is_known(method) {
        return Err(err(
            &id,
            METHOD_NOT_FOUND,
            format!(
                "unknown method: {method}; this viewer has: {}",
                METHODS.join(", ")
            ),
        ));
    }
    let params = match obj.get("params") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => {
            return Err(err(
                &id,
                INVALID_PARAMS,
                "`params` must be an object (or left out)",
            ));
        }
    };
    Ok(Request {
        id,
        method: method.to_owned(),
        params,
    })
}

/// A result line: the shape the server's own wire uses.
pub fn ok(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

/// An error line: the shape the server's own wire uses, code included.
pub fn err(id: &Value, code: i64, message: impl AsRef<str>) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.as_ref() },
    })
    .to_string()
}

/// A method's named arguments, with the method's own name in every refusal.
///
/// The refusals are the contract's `-32602`, and they name the method and the argument
/// the way the server's own methods do (`comp_node: \`id\` (string) required`), so a
/// script that guessed a name reads what to send.
pub struct Args<'a> {
    pub method: &'a str,
    pub map: &'a Map<String, Value>,
}

impl Args<'_> {
    /// A required string.
    pub fn text(&self, key: &str) -> Result<String, (i64, String)> {
        match self.map.get(key) {
            Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
            Some(Value::String(_)) => Err(self.bad(format!("`{key}` must not be empty"))),
            _ => Err(self.bad(format!("`{key}` (string) required"))),
        }
    }

    /// An optional string; `null` is `None` (a "named or not" argument).
    pub fn opt_text(&self, key: &str) -> Result<Option<String>, (i64, String)> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(self.bad(format!("`{key}` must be a string or null"))),
        }
    }

    /// A required boolean (`false` is a value, not an absence).
    pub fn boolean(&self, key: &str) -> Result<bool, (i64, String)> {
        match self.map.get(key) {
            Some(Value::Bool(b)) => Ok(*b),
            _ => Err(self.bad(format!("`{key}` (true or false) required"))),
        }
    }

    /// A required finite number.
    pub fn number(&self, key: &str) -> Result<f64, (i64, String)> {
        match self.map.get(key) {
            Some(Value::Number(n)) => match n.as_f64() {
                Some(v) if v.is_finite() => Ok(v),
                _ => Err(self.bad(format!("`{key}` must be a finite number"))),
            },
            _ => Err(self.bad(format!("`{key}` (number) required"))),
        }
    }

    /// An optional `[x, y, z]`.
    pub fn opt_vec3(&self, key: &str) -> Result<Option<[f64; 3]>, (i64, String)> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Array(a)) if a.len() == 3 => {
                let mut out = [0.0f64; 3];
                for (i, v) in a.iter().enumerate() {
                    let Some(n) = v.as_f64().filter(|n| n.is_finite()) else {
                        return Err(self.bad(format!("`{key}` must be three finite numbers")));
                    };
                    out[i] = n;
                }
                Ok(Some(out))
            }
            Some(_) => Err(self.bad(format!("`{key}` must be `[x, y, z]`"))),
        }
    }

    /// This method's `-32602` for `why`.
    pub fn bad(&self, why: impl AsRef<str>) -> (i64, String) {
        (INVALID_PARAMS, format!("{}: {}", self.method, why.as_ref()))
    }
}

/// Refuse an address the control channel may not listen on, or pass a loopback one.
///
/// Called at startup (`crate::app::ViewArgs::parse`), before any socket exists, so a
/// typo is one message and no listener; [`spawn`] checks the same rule again because a
/// bound socket is the thing that actually matters.
pub fn resolve_loopback(addr: &str) -> Result<SocketAddr, String> {
    let sock = addr
        .to_socket_addrs()
        .map_err(|e| format!("--control {addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("--control {addr}: resolves to no address"))?;
    if !sock.ip().is_loopback() {
        return Err(format!(
            "--control {addr}: only a loopback address may carry the control channel \
             (127.0.0.1:port, [::1]:port or localhost:port); got {}",
            sock.ip()
        ));
    }
    Ok(sock)
}

/// The same rule without resolving: is this text a loopback address?
///
/// Used at parse time, where a name that needs DNS would be a surprise: `localhost` is
/// the one host name a loopback bind may be written with, and everything else must
/// already be an address.
pub fn is_loopback_text(addr: &str) -> bool {
    if let Ok(sock) = addr.parse::<SocketAddr>() {
        return sock.ip().is_loopback();
    }
    let host = match addr.rsplit_once(':') {
        Some((h, _)) => h,
        None => addr,
    };
    host.eq_ignore_ascii_case("localhost")
}

/// The receiver end of the queue, as the shell holds it.
pub type Queue = Receiver<Queued>;

#[cfg(test)]
mod scratch {
    // A `Receiver` is not `Clone` and the shell takes it once; naming the alias here
    // keeps that intent in the signature (`Option<control::Queue>`).
}
