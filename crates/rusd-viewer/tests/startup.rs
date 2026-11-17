//! **Startup does not wait for the server.**
//!
//! The bug this file pins, in one line: a viewer whose first window was built
//! only after its network thread had a snapshot never built a window at all when
//! the server was quiet, slow, or late — and the failure was silent, because every
//! startup step after `N_FONT` printed nothing.
//!
//! Two properties, both testable without a window, a GPU or a human:
//!
//! 1. **The assembly reaches its milestones whatever the server does.** A mock
//!    that greets and then says nothing, a mock that never answers a request, and
//!    a port with nothing behind it all leave [`startup::assemble`] returning, with
//!    `SHELL_BUILT` marked, in well under the deadline. That is the red-green of
//!    the regression: the pre-fix code waited for a greeting inside the window
//!    build, so the same mock left it un-built.
//! 2. **A request that never gets its reply ends in `E_WIRE_TIMEOUT`, and the
//!    connection survives it.** A server that answers `hello`/`subscribe` and then
//!    goes quiet must not wedge the network thread: the timeout is one failed
//!    request, the next notification still arrives, and a later request still
//!    works. The pre-fix code parked on `recv` forever in exactly this state.
//!
//! Both mocks are real TCP sockets speaking the same NDJSON the real server does —
//! this is the protocol, not a stub of it.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use rusd_viewer::net::{
    E_WIRE_TIMEOUT, NetCommand, NetEvent, NetWorker, ProtocolClient, REPLY_DEADLINE,
};
use rusd_viewer::protocol::{self, PROTOCOL, TIMEOUT_CODE};
use rusd_viewer::startup::{self, Startup, WINDOW_CREATED};
use serde_json::{Value, json};

/// The longest a *headless* assembly may take. Startup is a few map lookups and a
/// thread spawn; anything near a second means it is waiting for something.
const STARTUP_BUDGET: Duration = Duration::from_secs(2);

/// A mock server that greets, records what it is asked, and **never answers**
/// anything after `subscribe`.
///
/// This is the state the viewer stalled in: a live, gregarious server is easy; the
/// hard case is a peer that completes the handshake and then goes quiet, which is
/// what a server busy cooking city looks like from the other side of the socket.
struct Quiet {
    addr: String,
    seen: Receiver<String>,
    pushes: Sender<String>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Quiet {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr").to_string();
        let (seen_tx, seen_rx) = channel();
        let (push_tx, push_rx) = channel::<String>();
        let t = thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let Ok(mut out) = stream.try_clone() else {
                return;
            };
            let mut reader = BufReader::new(stream);
            // The greeting, answered exactly as `rusd serve` answers it.
            for _ in 0..2 {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let Ok(v) = serde_json::from_str::<Value>(line.trim_end()) else {
                    return;
                };
                let id = v["id"].as_u64().unwrap_or(0);
                let method = v["method"].as_str().unwrap_or("").to_owned();
                let _ = seen_tx.send(method.clone());
                let reply = match method.as_str() {
                    "hello" => {
                        json!({"id": id, "result": {"protocol": PROTOCOL, "session": "quiet", "version": 1}})
                    }
                    "subscribe" => json!({"id": id, "result": {"version": 2}}),
                    _ => json!({"id": id, "error": {"code": -32601, "message": "quiet mock"}}),
                };
                if writeln!(out, "{reply}").is_err() || out.flush().is_err() {
                    return;
                }
            }
            // From here on: read every request and record it, answer none. Push
            // whatever the test asks for, so "the connection still works" is
            // observable.
            thread::spawn(move || {
                let mut reader = reader;
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let m = serde_json::from_str::<Value>(line.trim_end())
                        .ok()
                        .and_then(|v| v["method"].as_str().map(str::to_owned))
                        .unwrap_or_default();
                    if seen_tx.send(m).is_err() {
                        return;
                    }
                }
            });
            for line in push_rx {
                if writeln!(out, "{line}").is_err() || out.flush().is_err() {
                    return;
                }
            }
        });
        Self {
            addr,
            seen: seen_rx,
            pushes: push_tx,
            thread: Some(t),
        }
    }

    /// The next method the client sent.
    fn next_method(&self) -> String {
        self.seen
            .recv_timeout(Duration::from_secs(10))
            .expect("the client sent a request")
    }

    /// Push one raw NDJSON line to the client.
    fn push(&self, line: Value) {
        let _ = self.pushes.send(line.to_string());
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        // The serving thread parks in `read_line`; dropping the push sender is what
        // ends its push loop, and the client's socket closing ends the reader.
        let _ = self.pushes.send(String::new());
        let _ = &self.thread;
    }
}

/// A snapshot notification, byte-identical in shape to the server's.
fn snapshot_line(version: u64) -> Value {
    json!({
        "method": "stage_snapshot",
        "params": {"version": version, "format": "usdc", "data": ""}
    })
}

#[test]
fn the_assembly_finishes_while_the_server_is_quiet() {
    let mock = Quiet::start();
    let mut startup = Startup::new();
    let began = Instant::now();
    let assembled = startup::assemble(&mock.addr, &mut startup);
    let took = began.elapsed();

    assert!(
        startup.reached(startup::NET_WORKER),
        "the network worker was started: {:?}",
        startup.marks()
    );
    assert!(
        startup.reached(startup::SHELL_BUILT),
        "the shell is built with the server not yet having said anything beyond \
         its greeting: {:?}",
        startup.marks()
    );
    assert!(
        took < STARTUP_BUDGET,
        "assembly must not wait for the server; it took {took:?}"
    );
    // The greeting did happen — on the worker thread — so the connection is live.
    assert_eq!(mock.next_method(), "hello");
    assert_eq!(mock.next_method(), "subscribe");
    drop(assembled.net.drain());
    let _ = assembled.model;
}

#[test]
fn the_assembly_finishes_when_nothing_is_listening_at_all() {
    // A closed port: `connect` fails at once, and that must be a diagnostic on the
    // worker thread, never a delay in the window build.
    let closed = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = closed.local_addr().expect("addr").to_string();
    drop(closed);

    let mut startup = Startup::new();
    let began = Instant::now();
    let assembled = startup::assemble(&addr, &mut startup);
    let took = began.elapsed();

    assert!(
        startup.reached(startup::SHELL_BUILT),
        "{:?}",
        startup.marks()
    );
    assert!(
        took < STARTUP_BUDGET,
        "no waiting for a dead server: {took:?}"
    );
    // The failure arrives as an event, not as a hang: the worker reports `Closed`
    // and the window draws with whatever it has.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut closed_event = false;
    while Instant::now() < deadline {
        for e in assembled.net.drain() {
            if matches!(e, NetEvent::Closed(_)) {
                closed_event = true;
            }
        }
        if closed_event {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(closed_event, "a dead server is reported, not waited on");
}

#[test]
fn a_greeting_with_a_deadline_ends_a_silent_peer_in_a_timeout() {
    // A port that accepts the connection and answers nothing at all — not even
    // `hello`. The greeting must end at its deadline with `E_CONNECT`/timeout
    // diagnostics rather than parking forever; this is the shape of a firewall or a
    // server whose accept thread has stalled.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let accept = thread::spawn(move || {
        // Hold the connection open, saying nothing.
        let Ok((_stream, _)) = listener.accept() else {
            return;
        };
        thread::sleep(Duration::from_millis(600));
    });

    let began = Instant::now();
    let out = ProtocolClient::open_within(&addr, Duration::from_millis(150));
    let took = began.elapsed();
    let diags = out.err().expect("a silent peer is refused");
    assert!(
        diags.iter().any(|d| d.code == E_WIRE_TIMEOUT),
        "the greeting times out: {diags:?}"
    );
    assert!(
        took < Duration::from_secs(3),
        "and it returns promptly: {took:?}"
    );
    let _ = accept.join();
}

#[test]
fn a_request_the_server_never_answers_times_out_and_the_connection_lives() {
    let mock = Quiet::start();
    let (mut client, _early) = ProtocolClient::open(&mock.addr).expect("the greeting succeeds");
    assert_eq!(mock.next_method(), "hello");
    assert_eq!(mock.next_method(), "subscribe");

    // A reply deadline short enough for a test, exercised through the same path
    // the window uses.
    client.set_reply_deadline(Duration::from_millis(200));
    let began = Instant::now();
    let err = client
        .request("composition_tree", json!({}))
        .expect_err("no reply, so no result");
    let took = began.elapsed();
    assert!(
        err.iter().any(|d| d.code == E_WIRE_TIMEOUT),
        "the request times out: {err:?}"
    );
    assert!(took >= Duration::from_millis(150), "{took:?}");
    assert!(took < Duration::from_secs(3), "{took:?}");
    assert_eq!(mock.next_method(), "composition_tree");

    // **The connection survived it.** A snapshot the server pushes afterwards is
    // delivered, and a later request still goes out — the two things that were
    // impossible while the client parked on `recv`.
    mock.push(snapshot_line(9));
    let mut saw_snapshot = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !saw_snapshot {
        if let Ok(rusd_viewer::protocol::Wire::Notification(protocol::Notification::Snapshot(s))) =
            client.read_within(Duration::from_millis(100))
        {
            saw_snapshot = s.version == 9;
        }
    }
    assert!(
        saw_snapshot,
        "a notification pushed after the timeout arrives"
    );

    client.set_reply_deadline(Duration::from_secs(5));
    // Nothing answers requests in this mock, so the next one also times out —
    // which is the point: the *socket* is still good.
    let again = client.request("comp_node", json!({"id": "prim:/city"}));
    assert!(again.is_err(), "still unanswered");
    assert_eq!(mock.next_method(), "comp_node");
}

#[test]
fn the_worker_reports_a_timeout_as_a_failed_request_not_a_closed_connection() {
    let mock = Quiet::start();
    let worker = NetWorker::spawn(mock.addr.clone(), Box::new(|| {}));
    assert_eq!(mock.next_method(), "hello");
    assert_eq!(mock.next_method(), "subscribe");
    // Give the worker a moment to report `Live`, then ask for something the server
    // will never answer.
    thread::sleep(Duration::from_millis(50));
    worker.send(NetCommand::Call {
        tag: "composition_tree".to_owned(),
        method: "composition_tree".to_owned(),
        params: json!({}),
    });

    // The default deadline is `REPLY_DEADLINE`; the assertion here is about the
    // *kind* of event, so wait long enough for it and no longer.
    let mut reply = None;
    let mut closed = false;
    let deadline = Instant::now() + REPLY_DEADLINE + Duration::from_secs(2);
    while Instant::now() < deadline {
        for e in worker.drain() {
            match e {
                NetEvent::Reply { result, .. } => reply = Some(result),
                NetEvent::Closed(_) => closed = true,
                _ => {}
            }
        }
        if reply.is_some() || closed {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !closed,
        "one unanswered request must not be reported as a dead connection"
    );
    let result = reply.expect("the request is answered, as a failure");
    let err = match result {
        Err(e) => e,
        Ok(v) => panic!("a timeout is a failure, got a result: {v}"),
    };
    assert_eq!(
        err.code, TIMEOUT_CODE,
        "the code says this side gave up, not that the method is missing"
    );
}

#[test]
fn the_marks_are_ordered_and_never_claim_a_window_a_headless_run_does_not_build() {
    let mut startup = Startup::new();
    startup::assemble("127.0.0.1:1", &mut startup);
    let names: Vec<&str> = startup.marks().iter().map(|m| m.name).collect();
    assert!(
        !names.contains(&WINDOW_CREATED),
        "a headless assembly builds no window: {names:?}"
    );
    // Ordering: the worker is up before "the shell is built" is claimed.
    let net = names.iter().position(|n| *n == startup::NET_WORKER);
    let shell = names.iter().position(|n| *n == startup::SHELL_BUILT);
    assert!(net.is_some() && shell.is_some(), "{names:?}");
    assert!(
        net < shell,
        "the worker precedes the finished shell: {names:?}"
    );
}

#[test]
fn a_timeout_code_is_not_mistaken_for_a_missing_method() {
    // The panel decorates failures differently, and the difference is the *code*:
    // `-32601` means "this server predates the method", a timeout means "it did not
    // answer in time". They must not collide.
    assert_ne!(
        TIMEOUT_CODE,
        rusd_viewer::tree::METHOD_NOT_FOUND,
        "a timed-out request must not read as a missing method"
    );
    // And the tree panel says the right thing about each.
    let mut model = rusd_viewer::tree::TreeModel::default();
    let e = model
        .apply(Err((TIMEOUT_CODE, "no reply in time".to_owned())))
        .expect_err("there is no tree");
    assert!(
        e.contains("时限"),
        "the failure says the server did not answer in time: {e}"
    );
    assert!(
        !e.contains("server 版本"),
        "and does not claim the server is too old to have the method: {e}"
    );
    assert!(
        model.status_zh().contains("时限"),
        "the status line carries the same reading: {}",
        model.status_zh()
    );
}
