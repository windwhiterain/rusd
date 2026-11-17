//! The protocol client against a **mock server** that speaks `docs/PROTOCOL.md`
//! itself: a real TCP socket, NDJSON JSON-RPC, a real `hello`/`subscribe`, real
//! `stage_snapshot` / `state_changed` notifications, and real `opinion` /
//! `undo` / `redo` replies.
//!
//! What this pins, none of which needs a GPU or a window:
//!
//! 1. **The greeting** — `hello{protocol:1}` is answered and `subscribe` granted;
//!    a server speaking another protocol number is refused, not tolerated.
//! 2. **The snapshot pipeline** — a `stage_snapshot`'s base64 `usdc` decodes,
//!    composes over the single-layer mirror, and yields the cooked bbox and prim
//!    count. This is the whole Tier A path, minus pixels.
//! 3. **The discard rule** — a version at or below the greatest seen is dropped,
//!    so a late duplicate can never roll the view back.
//! 4. **The opinion loop** — a gizmo's `set` on `xformOp:translate` reaches the
//!    server with the exact shape the contract states, and its `{version}` comes
//!    back; if the server answers with a new snapshot, the client adopts it.
//! 5. **Failure is a diagnostic** — a dropped connection and a bad snapshot are
//!    reported, never a panic.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use rusd_core::Severity;
use rusd_viewer::compose::base64_encode;
use rusd_viewer::net::{NetEvent, NetWorker, ProtocolClient};
use rusd_viewer::protocol::{self, Notification, OpinionOp, PROTOCOL, Snapshot, Wire};
use rusd_viewer::session::{ClientModel, Effect, MadeScene};
use serde_json::{Value, json};

/// The cooked city `usdc` in memory: what a server puts in a snapshot.
fn cooked_city() -> Vec<u8> {
    let doc =
        rusd_core::syntax::parse(include_str!("../../../examples/city.rusd")).expect("city parses");
    let out = rusd_proc::expand_doc(&doc).expect("city cooks");
    let flat = out.stage.flatten().expect("stage flattens");
    let format = openusd::sdf::LayerRegistry::find_by_id("usdc").expect("usdc");
    format
        .write_bytes(flat.data())
        .expect("usdc writes")
        .bytes
        .to_vec()
}

/// One request the mock server received.
#[derive(Clone, Debug)]
struct Received {
    id: u64,
    method: String,
    params: Value,
}

/// The mock server: accepts one client, answers the greeting, then serves what
/// the test tells it to, recording what it was asked.
struct Mock {
    addr: String,
    seen: Receiver<Received>,
    /// Push this to make the server say something.
    tell: Sender<Say>,
    listener_thread: Option<thread::JoinHandle<()>>,
}

/// A server-side action the test asks for.
#[derive(Clone, Debug)]
enum Say {
    Notify(Value),
    Close,
}

impl Mock {
    /// Start a server that greets with `protocol` and answers every edit with an
    /// incremented version. `snapshots` are pushed to the client right after the
    /// greeting, in order.
    fn start(protocol_number: u32, snapshots: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr").to_string();
        let (seen_tx, seen_rx) = channel();
        let (say_tx, say_rx) = channel::<Say>();
        let lt = thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            serve(stream, protocol_number, snapshots, seen_tx, say_rx);
        });
        Self {
            addr,
            seen: seen_rx,
            tell: say_tx,
            listener_thread: Some(lt),
        }
    }

    fn push(&self, value: Value) {
        let _ = self.tell.send(Say::Notify(value));
    }

    fn close(&self) {
        let _ = self.tell.send(Say::Close);
    }

    fn next_seen(&self) -> Received {
        self.seen
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the server received a request")
    }

    /// The next request, asserting its method: what most tests want.
    fn next_method(&self, want: &str) -> Value {
        let r = self.next_seen();
        assert_eq!(r.method, want, "the next request is a {want}");
        r.params
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        // Never join: the connection thread may be parked in `read_line` on the
        // client's socket, which only closes when the client drops. The say
        // channel closing is what stops it soon after.
        let _ = self.tell.send(Say::Close);
        self.listener_thread.take();
    }
}

/// The server's side of one connection: NDJSON in, NDJSON out. It greets, then
/// pushes whatever the test asks for and answers every request with a new
/// version, until the test says close.
fn serve(
    stream: TcpStream,
    protocol_number: u32,
    snapshots: Vec<Value>,
    seen: Sender<Received>,
    say: Receiver<Say>,
) {
    let write_half = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut out = write_half;
    // A clone the close path can shut down: the reader thread holds its own, so
    // returning from this function alone would leave the socket open and the
    // client would never see the connection end.
    let shutdown = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut version = 0u64;

    // One thread reads requests, so this one can push notifications in between.
    let (req_tx, req_rx) = channel::<(u64, String, Value)>();
    let reader = BufReader::new(stream);
    thread::spawn(move || {
        let mut line = String::new();
        let mut reader = reader;
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let Ok(v) = serde_json::from_str::<Value>(line.trim_end()) else {
                return;
            };
            let id = v["id"].as_u64().unwrap_or(0);
            let method = v["method"].as_str().unwrap_or("").to_owned();
            if req_tx.send((id, method, v["params"].clone())).is_err() {
                return;
            }
        }
    });

    let write_line = |out: &mut TcpStream, v: &Value| -> bool {
        writeln!(out, "{v}").is_ok() && out.flush().is_ok()
    };

    // Answer the greeting first: hello, then subscribe.
    for _ in 0..2 {
        let Ok((id, method, params)) = req_rx.recv() else {
            return;
        };
        let _ = seen.send(Received {
            id,
            method: method.clone(),
            params,
        });
        let reply = match method.as_str() {
            "hello" => {
                version = 1;
                json!({"id": id, "result": {"protocol": protocol_number, "session": "mock", "version": version}})
            }
            "subscribe" => {
                version += 1;
                json!({"id": id, "result": {"version": version}})
            }
            _ => json!({"id": id, "error": {"code": -32601, "message": "not yet"}}),
        };
        if !write_line(&mut out, &reply) {
            return;
        }
    }

    // The snapshots this mock opens with, newest last.
    for snap in snapshots {
        if !write_line(&mut out, &snap) {
            return;
        }
    }

    // Then serve: notifications the test asks for, and replies to edits.
    loop {
        match say.recv_timeout(std::time::Duration::from_millis(10)) {
            Ok(Say::Notify(v)) => {
                if !write_line(&mut out, &v) {
                    return;
                }
            }
            Ok(Say::Close) => {
                let _ = shutdown.shutdown(std::net::Shutdown::Both);
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
        while let Ok((id, method, params)) = req_rx.try_recv() {
            let _ = seen.send(Received { id, method, params });
            version += 1;
            let reply = json!({"id": id, "result": {"version": version}});
            if !write_line(&mut out, &reply) {
                return;
            }
        }
    }
}

/// The `stage_snapshot` notification for `bytes` at `version`.
fn snapshot_notification(version: u64, bytes: &[u8]) -> Value {
    json!({
        "method": "stage_snapshot",
        "params": {
            "version": version,
            "format": "usdc",
            "data": base64_encode(bytes),
        }
    })
}

// --- 1. the greeting ------------------------------------------------------

#[test]
fn the_client_says_hello_and_subscribes_before_anything_else() {
    let mock = Mock::start(PROTOCOL, vec![]);
    let (client, early) = ProtocolClient::open(&mock.addr).expect("greeting succeeds");
    assert!(early.is_empty(), "no notification raced the greeting");
    assert_eq!(client.addr, mock.addr);

    let hello = mock.next_seen();
    assert_eq!(hello.method, "hello", "hello comes first");
    assert_eq!(hello.id, 1, "the first request is id 1");
    assert_eq!(hello.params["client"], "rusd-viewer");
    assert_eq!(hello.params["protocol"], PROTOCOL);
    let subscribe = mock.next_method("subscribe");
    assert_eq!(subscribe["client"], "rusd-viewer");
}

#[test]
fn a_snapshot_pushed_mid_session_reaches_the_model() {
    // The server pushed nothing at the greeting; the client asked for it
    // later, which is what a change on the server side looks like.
    let bytes = cooked_city();
    let mock = Mock::start(PROTOCOL, vec![]);
    let (mut client, early) = ProtocolClient::open(&mock.addr).expect("greeting");
    assert!(early.is_empty());

    mock.push(snapshot_notification(4, &bytes));
    let mut model = ClientModel::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut scene = false;
    while std::time::Instant::now() < deadline && !scene {
        match client.read_wire() {
            Ok(Wire::Notification(n)) => {
                if let Effect::NewScene(made) = model.apply(&n) {
                    assert_eq!(made.version, 4, "the version the server pushed");
                    assert_eq!(made.item_count(), 434);
                    scene = true;
                }
            }
            Ok(Wire::Response(_)) => {}
            Err(_) => break,
        }
    }
    assert!(scene, "the pushed snapshot arrived and composed");
}
#[test]
fn a_server_speaking_another_protocol_is_refused() {
    let mock = Mock::start(PROTOCOL + 1, vec![]);
    let err = ProtocolClient::open(&mock.addr)
        .err()
        .expect("another protocol is refused");
    assert_eq!(err[0].code, "E_WIRE_PROTOCOL");
    assert!(
        err[0].message.contains("protocol 2") && err[0].message.contains("protocol 1"),
        "the message names both numbers: {}",
        err[0].message
    );
}

// --- 2. the snapshot pipeline ---------------------------------------------

#[test]
fn a_pushed_snapshot_composes_to_the_cooked_bbox_and_prim_count() {
    let bytes = cooked_city();
    let mock = Mock::start(PROTOCOL, vec![snapshot_notification(2, &bytes)]);
    let (mut client, early) = ProtocolClient::open(&mock.addr).expect("greeting");

    // The snapshot may arrive in the greeting's window or right after it.
    let mut notif = early;
    while notif.is_empty() {
        match client.read_wire().expect("a wire line") {
            Wire::Notification(n) => notif.push(n),
            Wire::Response(r) => panic!("unexpected reply {}", r.id),
        }
    }
    let Notification::Snapshot(snap) = &notif[0] else {
        panic!("expected a snapshot, got {:?}", notif[0]);
    };
    assert_eq!(snap.version, 2);
    assert_eq!(snap.format, "usdc");

    let made = MadeScene::from_snapshot(snap).expect("the snapshot composes");
    assert_eq!(made.item_count(), 434, "city's prims");
    let b = made.bbox();
    assert!(
        (b.min[0] + 210.262254).abs() < 1e-3,
        "bbox min x {}",
        b.min[0]
    );
    assert!((b.max[0] - 444.0).abs() < 1e-3, "bbox max x {}", b.max[0]);
    assert!(!made.has_error(), "no error diagnostics: {:?}", made.diags);
}

#[test]
fn the_worker_threads_snapshot_into_a_model_the_ui_could_show() {
    let bytes = cooked_city();
    let mock = Mock::start(PROTOCOL, vec![snapshot_notification(2, &bytes)]);
    let worker = NetWorker::spawn(mock.addr.clone(), Box::new(|| {}));

    let mut model = ClientModel::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut got_scene = false;
    while std::time::Instant::now() < deadline && !got_scene {
        for e in worker.drain() {
            match e {
                NetEvent::Live { .. } => {}
                NetEvent::Notification(n) => {
                    if let Effect::NewScene(made) = model.apply(&n) {
                        assert_eq!(made.item_count(), 434);
                        got_scene = true;
                    }
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(got_scene, "the worker delivered the snapshot");
    assert_eq!(model.version, Some(2));
    assert_eq!(model.item_count, 434);
    assert!(
        model.status_line().contains("434 prims"),
        "{}",
        model.status_line()
    );
}

// --- 3. the discard rule ---------------------------------------------------

#[test]
fn a_stale_snapshot_from_the_server_is_discarded_and_the_newest_wins() {
    let bytes = cooked_city();
    // The server sends v3, then a late duplicate v3, then an older v2.
    let mock = Mock::start(
        PROTOCOL,
        vec![
            snapshot_notification(3, &bytes),
            snapshot_notification(3, &bytes),
            snapshot_notification(2, &bytes),
        ],
    );
    let (mut client, early) = ProtocolClient::open(&mock.addr).expect("greeting");

    let mut model = ClientModel::new();
    let mut applied = Vec::new();
    for n in early {
        if let Effect::NewScene(made) = model.apply(&n) {
            applied.push(made.version);
        }
    }
    while applied.is_empty() {
        match client.read_wire().expect("a wire line") {
            Wire::Notification(n) => {
                if let Effect::NewScene(made) = model.apply(&n) {
                    applied.push(made.version);
                }
            }
            Wire::Response(_) => {}
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        match client.read_wire() {
            Ok(Wire::Notification(n)) => {
                if let Effect::NewScene(made) = model.apply(&n) {
                    applied.push(made.version);
                }
            }
            Ok(Wire::Response(_)) => {}
            Err(_) => break,
        }
        if model.state.discarded >= 2 {
            break;
        }
    }

    assert_eq!(applied, vec![3], "only the first v3 is a new scene");
    assert_eq!(model.version, Some(3), "the view never rolled back to v2");
    assert_eq!(model.state.discarded, 2, "both stale lines were refused");
}

#[test]
fn a_state_changed_carrying_a_stale_version_is_discarded() {
    let bytes = cooked_city();
    let mock = Mock::start(
        PROTOCOL,
        vec![
            snapshot_notification(5, &bytes),
            json!({"method":"state_changed","params":{"version":4,"reason":"reload"}}),
        ],
    );
    let (mut client, _early) = ProtocolClient::open(&mock.addr).expect("greeting");
    let mut model = ClientModel::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline && model.state.discarded == 0 {
        match client.read_wire() {
            Ok(Wire::Notification(n)) => {
                model.apply(&n);
            }
            Ok(Wire::Response(_)) => {}
            Err(_) => break,
        }
    }
    assert!(
        !model.reload_badge,
        "a reload at v4 cannot be newer than the v5 snapshot already seen"
    );
}

// --- 4. the opinion loop ---------------------------------------------------

#[test]
fn a_gizmo_opinion_reaches_the_server_as_a_translate_set() {
    let mock = Mock::start(PROTOCOL, vec![]);
    let (mut client, _early) = ProtocolClient::open(&mock.addr).expect("greeting");
    let _ = mock.next_seen(); // hello
    let _ = mock.next_seen(); // subscribe

    let reply = client
        .opinion(&[OpinionOp::set_translate(
            "/World/City/b0_0/lot0_0",
            [15.0, 12.0, 7.0],
        )])
        .expect("opinion accepted");
    let received = mock.next_seen();
    assert_eq!(received.method, "opinion");
    let ops = received.params["ops"].as_array().expect("ops array");
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["path"], "/World/City/b0_0/lot0_0");
    assert_eq!(ops[0]["field"], "xformOp:translate");
    assert_eq!(ops[0]["op"], "set");
    assert_eq!(ops[0]["value"], json!([15.0, 12.0, 7.0]));

    let answered = reply.result.expect("opinion answered with a result");
    assert_eq!(answered["version"], 3, "the reply carries the new version");
}

#[test]
fn undo_and_redo_are_separate_requests_the_server_sees_by_name() {
    let mock = Mock::start(PROTOCOL, vec![]);
    let (mut client, _early) = ProtocolClient::open(&mock.addr).expect("greeting");
    let _ = mock.next_seen();
    let _ = mock.next_seen();

    client.edit(false).expect("undo accepted");
    assert_eq!(mock.next_seen().method, "undo");
    client.edit(true).expect("redo accepted");
    assert_eq!(mock.next_seen().method, "redo");
}

#[test]
fn the_worker_sends_an_opinion_and_reports_the_servers_version() {
    let mock = Mock::start(PROTOCOL, vec![]);
    let worker = NetWorker::spawn(mock.addr.clone(), Box::new(|| {}));
    let _ = mock.next_seen();
    let _ = mock.next_seen();

    worker.send(rusd_viewer::net::NetCommand::Opinion(vec![
        OpinionOp::set_translate("/World/a", [1.0, 2.0, 3.0]),
    ]));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut applied = None;
    let mut seen_opinion = None;
    while std::time::Instant::now() < deadline && applied.is_none() {
        if seen_opinion.is_none()
            && let Ok(r) = mock.seen.recv_timeout(std::time::Duration::from_millis(50))
        {
            assert_eq!(r.clone().method, "opinion");
            assert_eq!(r.params["ops"][0]["value"], json!([1.0, 2.0, 3.0]));
            seen_opinion = Some(r.method);
        }
        for e in worker.drain() {
            if let NetEvent::Applied { what, version } = e {
                assert_eq!(what, "opinion");
                assert!(version.is_some(), "the server's version came back");
                applied = Some(version);
            }
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(seen_opinion.is_some(), "the server received the opinion");
    assert!(applied.is_some(), "the worker reported the applied version");
}

// --- 5. failure is a diagnostic -------------------------------------------

#[test]
fn a_server_that_closes_the_connection_is_reported_not_panicked() {
    let mock = Mock::start(PROTOCOL, vec![]);
    let (mut client, _early) = ProtocolClient::open(&mock.addr).expect("greeting");
    let _ = mock.next_seen();
    let _ = mock.next_seen();
    mock.close();
    let err = client
        .read_wire()
        .expect_err("the closed socket is an error");
    assert_eq!(err[0].code, "E_IO");
    assert!(err[0].message.contains("closed"), "{}", err[0].message);
}

#[test]
fn a_snapshot_that_is_not_a_layer_is_a_diagnostic_and_the_old_scene_stays() {
    let bytes = cooked_city();
    let good = snapshot_notification(2, &bytes);
    let bad = json!({
        "method": "stage_snapshot",
        "params": {"version": 3, "format": "usdc", "data": base64_encode(b"nope")}
    });
    let mock = Mock::start(PROTOCOL, vec![good, bad]);
    let (mut client, early) = ProtocolClient::open(&mock.addr).expect("greeting");

    let mut model = ClientModel::new();
    let mut scenes = 0usize;
    let mut failures = 0usize;
    let drain =
        |model: &mut ClientModel, n: &Notification, scenes: &mut usize, failures: &mut usize| {
            match model.apply(n) {
                Effect::NewScene(_) => *scenes += 1,
                Effect::Failed(d) => {
                    assert_eq!(d[0].code, "E_LAYER_DECODE");
                    *failures += 1;
                }
                _ => {}
            }
        };
    for n in &early {
        drain(&mut model, n, &mut scenes, &mut failures);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline && failures == 0 {
        match client.read_wire() {
            Ok(Wire::Notification(n)) => drain(&mut model, &n, &mut scenes, &mut failures),
            Ok(Wire::Response(_)) => {}
            Err(_) => break,
        }
    }
    assert_eq!(scenes, 1, "the good snapshot loaded");
    assert_eq!(failures, 1, "the bad one was reported");
    assert_eq!(
        model.item_count, 434,
        "the good scene is still the one shown"
    );
    assert_eq!(model.diags[0].severity, Severity::Error);
}

#[test]
fn connecting_to_a_closed_port_is_a_diagnostic_not_a_panic() {
    // A port nothing listens on: bind, note it, then drop the listener.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    drop(listener);
    let err = ProtocolClient::open(&addr).err().expect("refused");
    assert_eq!(err[0].code, "E_CONNECT");
    assert!(err[0].message.contains(&addr), "{}", err[0].message);
}

#[test]
fn the_mock_servers_wire_lines_are_the_contracts_shape() {
    // Guard against the harness drifting from the protocol: the client parses
    // what the mock writes with the real parser.
    let bytes = cooked_city();
    let line = snapshot_notification(7, &bytes);
    let parsed = protocol::parse_line(&line.to_string()).expect("the mock's line parses");
    match parsed {
        Wire::Notification(Notification::Snapshot(Snapshot {
            version,
            format,
            data,
        })) => {
            assert_eq!(version, 7);
            assert_eq!(format, "usdc");
            assert_eq!(data, base64_encode(&bytes));
        }
        other => panic!("expected a snapshot, got {other:?}"),
    }
}
