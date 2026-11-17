//! The D13 client: the composition tree, the capture, and the two copies — end to
//! end against a **mock server** over a real socket.
//!
//! What this file is for is the half of the contract a unit test cannot reach: the
//! panel's round trips. A `composition_tree{}` request that carries no params, a
//! `comp_node{id}` whose id is the one the tree printed, a `capture` whose mask is
//! the image's size, and the one failure that must **degrade** rather than break —
//! a server from before v1.2 answering `-32601`.
//!
//! The mock answers by method name, so what it is asked is the assertion: the tests
//! drive [`ProtocolClient::request`] / [`NetWorker`] exactly as the window does, and
//! read the server's side of the socket.
//!
//! No GPU, no window: everything here is data.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use rusd_viewer::capture::{
    SelectionRect, camera_json_of, capture_clipboard, capture_params, copied_toast_zh, encode_png,
    mask_rgba, png_size, prim_clipboard, rect_from_drag,
};
use rusd_viewer::net::{NetCommand, NetEvent, NetWorker, ProtocolClient};
use rusd_viewer::panel::TreePanel;
use rusd_viewer::protocol::PROTOCOL;
use rusd_viewer::tree::{
    CompositionTree, METHOD_NOT_FOUND, NodeDetail, TreeModel, TreeNodeKind, TreeSelection,
    clipboard_for, comp_node_params, flatten, tree_params,
};
use serde_json::{Value, json};

// --- the mock server ---------------------------------------------------------

/// One request the mock received.
#[derive(Clone, Debug)]
struct Received {
    method: String,
    params: Value,
}

/// What a test queues ahead of a request: the method it answers, and the result
/// (`Ok` value or `Err((code, message))`).
type Answer = (String, Result<Value, (i64, String)>);

/// A server that greets, then answers each request with whatever the test queued
/// for its method, recording every request.
struct Mock {
    addr: String,
    seen: Receiver<Received>,
    answers: Sender<Answer>,
    pushes: Sender<Value>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Mock {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr").to_string();
        let (seen_tx, seen_rx) = channel();
        let (answer_tx, answer_rx) = channel::<Answer>();
        let (push_tx, push_rx) = channel::<Value>();
        let t = thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            serve(stream, seen_tx, answer_rx, push_rx);
        });
        Self {
            addr,
            seen: seen_rx,
            answers: answer_tx,
            pushes: push_tx,
            thread: Some(t),
        }
    }

    /// Answer the next request whose method is `method` with `result`.
    ///
    /// Queued ahead of the request it answers: the mock serves from this queue
    /// without waiting, so every `answer` must be called before the request is sent.
    fn answer(&self, method: &str, result: Result<Value, (i64, String)>) {
        let _ = self.answers.send((method.to_owned(), result));
    }

    /// Push one notification to the client unprompted.
    fn push(&self, line: Value) {
        let _ = self.pushes.send(line);
    }

    fn next_seen(&self) -> Received {
        self.seen
            .recv_timeout(Duration::from_secs(30))
            .expect("the server received a request")
    }

    /// The next request after the greeting, asserting the method.
    fn next_method(&self, want: &str) -> Value {
        let r = self.next_seen();
        assert_eq!(r.method, want, "the next request is a {want}");
        r.params
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        // The raw thread parks in `read_line` on the client's socket; dropping the
        // answers' sender is what ends its serving loop soon after.
        self.thread.take();
    }
}

fn serve(
    stream: TcpStream,
    seen: Sender<Received>,
    answers: Receiver<Answer>,
    pushes: Receiver<Value>,
) {
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);

    let write_line = |out: &mut TcpStream, v: &Value| -> bool {
        writeln!(out, "{v}").is_ok() && out.flush().is_ok()
    };

    // The greeting: `hello`, then `subscribe`.
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
        let _ = seen.send(Received {
            method: method.clone(),
            params: v["params"].clone(),
        });
        let reply = match method.as_str() {
            "hello" => json!({
                "id": id,
                "result": {"protocol": PROTOCOL, "session": "mock", "version": 1}
            }),
            "subscribe" => json!({"id": id, "result": {"version": 2}}),
            _ => json!({"id": id, "error": {"code": -32601, "message": "unexpected"}}),
        };
        if !write_line(&mut out, &reply) {
            return;
        }
    }

    // **A reader thread, so the serving loop can also push.** A mock that read
    // requests inline could only answer after the client spoke, so a notification a
    // test pushes to an idle client would sit in the channel — which is exactly the
    // starvation one test exists to check.
    let (req_tx, req_rx) = channel::<(u64, String, Value)>();
    thread::spawn(move || {
        let mut reader = reader;
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let trimmed = line.trim_end().to_owned();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&trimmed) else {
                return;
            };
            let id = v["id"].as_u64().unwrap_or(0);
            let method = v["method"].as_str().unwrap_or("").to_owned();
            if req_tx.send((id, method, v["params"].clone())).is_err() {
                return;
            }
        }
    });

    loop {
        // Pushes first: a notification the test queued goes out even if the client
        // never asks for anything.
        match pushes.try_recv() {
            Ok(line) => {
                if !write_line(&mut out, &line) {
                    return;
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }

        let (id, method, params) = match req_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(r) => r,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let _ = seen.send(Received {
            method: method.clone(),
            params,
        });
        // Answer from what the test queued **before** the request arrived. A method
        // no answer was queued for is answered at once with `-32601`, which is what
        // an older server does — and a mock that waited here would hang the client
        // instead of reporting the very failure one test exists to see.
        let reply = match answers.try_recv() {
            Ok((want, result)) if want == method => match result {
                Ok(value) => json!({"id": id, "result": value}),
                Err((code, message)) => {
                    json!({"id": id, "error": {"code": code, "message": message}})
                }
            },
            Ok((want, _)) => json!({
                "id": id,
                "error": {"code": -32000, "message": format!("the test queued {want}, not {method}")}
            }),
            Err(_) => json!({
                "id": id,
                "error": {"code": -32601, "message": format!("this server has no {method}")}
            }),
        };
        if !write_line(&mut out, &reply) {
            return;
        }
        let _ = out.flush();
    }
}

/// A tree answer shaped like the city stack's.
fn city_tree() -> Value {
    json!({
        "layers": [
            {"id": "anon:session", "kind": "session", "strength": 0},
            {"id": "city.rusd.cooked.usda", "kind": "root", "strength": 1},
            {"id": "prototypes.usda", "kind": "reference", "strength": 2},
        ],
        "arcs": [
            {"id": "arc:0", "kind": "reference",
             "source_layer": "city.rusd.cooked.usda", "source_prim": "/World/City/Block_01",
             "target_layer": "prototypes.usda", "target_prim": "/Prototypes/Block"},
        ],
        "prim_nodes": [
            {"id": "prim:/World/City/Block_01", "path": "/World/City/Block_01",
             "authored_in": ["city.rusd.cooked.usda", "anon:session"]},
            {"id": "prim:/Prototypes/Block", "path": "/Prototypes/Block",
             "authored_in": ["prototypes.usda"]},
        ],
        "variant_sets": [
            {"id": "vset:0", "prim": "/Prototypes/Block", "name": "size",
             "variants": ["small", "large"], "selection": "large"},
        ],
    })
}

/// A connection already through the greeting.
fn connected(mock: &Mock) -> ProtocolClient {
    let (client, early) = ProtocolClient::open(&mock.addr).expect("the greeting succeeds");
    assert!(early.is_empty(), "nothing raced the greeting");
    // Consume the greeting's two requests so the next `next_seen` is a test's own.
    let hello = mock.next_seen();
    assert_eq!(hello.method, "hello");
    let sub = mock.next_seen();
    assert_eq!(sub.method, "subscribe");
    client
}

// --- 1. the tree over the wire ----------------------------------------------

#[test]
fn composition_tree_is_asked_with_no_params_and_becomes_the_panels_rows() {
    let mock = Mock::start();
    let mut client = connected(&mock);
    mock.answer("composition_tree", Ok(city_tree()));

    let reply = client
        .request("composition_tree", tree_params())
        .expect("the request went out")
        .expect("the server answered");
    let params = mock.next_method("composition_tree");
    assert_eq!(params, json!({}), "the method takes no params");

    let mut panel = TreePanel::new();
    panel.apply_tree(Ok(reply));
    assert!(!panel.model.is_unavailable(), "{}", panel.model.status_zh());
    let rows = flatten(&panel.roots());
    // The rows, counted from the shape rather than from the tree's own idea of it:
    //   three layer roots
    //   session layer: 1 prim                                        = 1
    //   root layer:    the arc, its naming row, 1 prim               = 3
    //   prototypes:    1 prim, its variant set, the set's 2 variants = 4
    // No branch is duplicated, so no row appears twice for one reason.
    let root_layer_rows = 1 /* arc */ + 1 /* naming row */ + 1 /* prim */;
    let prototypes_rows = 1 /* prim */ + 1 /* vset */ + 2 /* variants */;
    assert_eq!(
        rows.len(),
        3 + 1 + root_layer_rows + prototypes_rows,
        "rows: {:?}",
        rows.iter().map(|(_, n)| n.id.as_str()).collect::<Vec<_>>()
    );
    let ids: Vec<&str> = rows.iter().map(|(_, n)| n.id.as_str()).collect();
    assert_eq!(
        ids[0], "anon:session",
        "the session layer is the first root"
    );
    assert!(ids.contains(&"arc:0"), "the arc is a row: {ids:?}");
    assert!(ids.contains(&"prim:/Prototypes/Block"), "{ids:?}");
    assert!(ids.contains(&"vset:0"), "{ids:?}");
    assert_eq!(
        ids.iter()
            .filter(|id| **id == "prim:/Prototypes/Block")
            .count(),
        1,
        "the prototype's prim is drawn once, under its own layer: {ids:?}"
    );
}

#[test]
fn a_comp_node_asks_with_the_id_the_tree_printed() {
    let mock = Mock::start();
    let mut client = connected(&mock);
    mock.answer("composition_tree", Ok(city_tree()));
    let tree_reply = client
        .request("composition_tree", tree_params())
        .expect("sent")
        .expect("answered");
    let _ = mock.next_method("composition_tree");
    let tree = CompositionTree::parse(&tree_reply).expect("the tree parses");

    // The prim the session layer overrides: its first opinion must be the session's.
    let prim = tree
        .prims
        .iter()
        .find(|p| p.path == "/World/City/Block_01")
        .expect("the block");
    mock.answer(
        "comp_node",
        Ok(json!({
            "kind_type": "prim",
            "id": prim.id,
            "path": prim.path,
            "opinions": [
                {"layer": "anon:session", "specifier": "over",
                 "fields": [{"name": "xformOp:translate", "value": [3.0, 0.0, 7.0]}]},
                {"layer": "city.rusd.cooked.usda", "specifier": "def",
                 "fields": [{"name": "typeName", "value": "Xform"}]},
            ],
            "arcs_out": [],
            "variant_sets": [],
        })),
    );
    let reply = client
        .request("comp_node", comp_node_params(&prim.id))
        .expect("sent")
        .expect("answered");
    let params = mock.next_method("comp_node");
    assert_eq!(params["id"], json!(prim.id), "the id goes back unchanged");

    let detail = NodeDetail::parse(&reply).expect("a prim detail");
    let lines = detail.lines();
    assert!(
        lines.iter().any(|l| l.contains("anon:session (over)")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("xformOp:translate = [3.0,0.0,7.0]")),
        "the session layer's override is the first opinion and is shown: {lines:?}"
    );
}

#[test]
fn a_server_without_the_method_degrades_and_never_fails_the_client() {
    let mock = Mock::start();
    let mut client = connected(&mock);
    // No answer queued: the mock replies `-32601` for a method it was not told about,
    // which is exactly what a pre-v1.2 server does.
    let err = client
        .request("composition_tree", tree_params())
        .expect("the request went out")
        .expect_err("the server refused");
    assert_eq!(err.code, METHOD_NOT_FOUND, "{err:?}");

    let mut panel = TreePanel::new();
    panel.apply_tree(Err((err.code, err.message)));
    assert!(panel.model.is_unavailable());
    assert!(panel.roots().is_empty(), "a degraded panel draws no rows");
    assert!(
        panel
            .model
            .status_zh()
            .contains("合成树不可用（server 版本）"),
        "{}",
        panel.model.status_zh()
    );
    // And the connection is still usable: the degrade is an answer, not a break.
    mock.answer("composition_tree", Ok(city_tree()));
    let reply = client
        .request("composition_tree", tree_params())
        .expect("the connection is still up")
        .expect("answered");
    panel.apply_tree(Ok(reply));
    assert!(!panel.model.is_unavailable());
    assert!(!panel.roots().is_empty());
}

#[test]
fn the_tree_model_starts_idle_and_a_failed_answer_is_not_a_tree() {
    let mut model = TreeModel::new();
    assert!(model.tree().is_none());
    assert!(model.status_zh().contains("尚未请求"));
    let e = model
        .apply(Ok(json!({"layers": "not an array"})))
        .expect_err("a malformed answer");
    assert!(e.contains("layers"), "{e}");
    assert!(matches!(model, TreeModel::Failed(_)));
}

// --- 2. the capture over the wire -------------------------------------------

#[test]
fn a_capture_carries_an_image_and_a_mask_of_one_size_and_comes_back_as_an_id() {
    let mock = Mock::start();
    let mut client = connected(&mock);

    // A frame the size of a small viewport, its selection a rectangle inside it.
    let (w, h) = (64u32, 48u32);
    let frame = vec![90u8; (w * h * 4) as usize];
    let sel = SelectionRect {
        x0: 10,
        y0: 6,
        x1: 34,
        y1: 30,
    };
    let image = encode_png(w, h, &frame).expect("the image encodes");
    let mask = mask_rgba((w, h), sel);
    let mask_png = encode_png(w, h, &mask).expect("the mask encodes");
    let camera = camera_json_of(&rusd_scene::geometry::Camera {
        position: [10.0, -20.0, 30.0],
        look_at: [0.0, 0.0, 0.0],
        fov_deg: 40.0,
    });
    let params = capture_params(camera, &image, Some(&mask_png), 7, Some("看这里"));

    mock.answer("capture", Ok(json!({"id": "cap-0001"})));
    let reply = client
        .request("capture", params.clone())
        .expect("sent")
        .expect("answered");
    let seen = mock.next_method("capture");

    // The contract's checks, read off the wire the test actually sent.
    let image_bytes = rusd_viewer::compose::base64_decode(seen["image"].as_str().unwrap())
        .expect("image is base64");
    let mask_bytes = rusd_viewer::compose::base64_decode(seen["mask"].as_str().unwrap())
        .expect("mask is base64");
    assert_eq!(
        png_size(&image_bytes),
        png_size(&mask_bytes),
        "the mask is the image's size — the server's one size rule"
    );
    assert_eq!(
        png_size(&image_bytes),
        Some((w, h)),
        "at the frame's own size"
    );
    assert_eq!(seen["scene_version"], json!(7));
    assert_eq!(seen["camera"]["up"], json!([0.0, 0.0, 1.0]));
    assert_eq!(seen["note"], json!("看这里"));
    assert!(
        mask_bytes.len() > 8,
        "the mask's PNG is a real image, not a placeholder"
    );

    assert_eq!(reply["id"], json!("cap-0001"));
    // What the window does with the answer: the id goes to the clipboard, toasted.
    let id = reply["id"].as_str().expect("an id");
    assert_eq!(capture_clipboard(id), "cap-0001");
    assert_eq!(copied_toast_zh(&capture_clipboard(id)), "已拷贝 cap-0001");
}

#[test]
fn an_offline_capture_says_scene_version_zero_and_still_carries_a_mask() {
    // `--usdc`: there is a frame, so there is a capture; there is no server version.
    let (w, h) = (8u32, 6u32);
    let frame = vec![200u8; (w * h * 4) as usize];
    let image = encode_png(w, h, &frame).expect("image");
    let mask_png = encode_png(
        w,
        h,
        &mask_rgba(
            (w, h),
            SelectionRect {
                x0: 1,
                y0: 1,
                x1: 4,
                y1: 4,
            },
        ),
    )
    .expect("mask");
    let params = capture_params(
        camera_json_of(&rusd_scene::geometry::Camera {
            position: [0.0, 0.0, 1.0],
            look_at: [0.0, 0.0, 0.0],
            fov_deg: 35.0,
        }),
        &image,
        Some(&mask_png),
        0,
        None,
    );
    assert_eq!(params["scene_version"], json!(0));
    assert!(params.get("note").is_none(), "no note was typed");
    assert!(params["mask"].is_string(), "the mask is still there");
}

#[test]
fn a_capture_the_server_refuses_is_reported_with_its_code() {
    let mock = Mock::start();
    let mut client = connected(&mock);
    mock.answer(
        "capture",
        Err((
            -32602,
            "capture: the mask is 8x6 but the image is 4x4; a mask must be the same \
             size as its image"
                .to_owned(),
        )),
    );
    let err = client
        .request(
            "capture",
            json!({"camera": {}, "image": "", "scene_version": 0}),
        )
        .expect("sent")
        .expect_err("refused");
    assert_eq!(err.code, -32602);
    assert!(err.message.contains("same size"), "{}", err.message);
}

// --- 3. through the worker, as the window uses it ---------------------------

#[test]
fn the_worker_delivers_tree_and_capture_replies_under_their_tags() {
    let mock = Mock::start();
    let worker = NetWorker::spawn(mock.addr.clone(), Box::new(|| {}));
    let _ = mock.next_seen(); // hello
    let _ = mock.next_seen(); // subscribe

    mock.answer("composition_tree", Ok(city_tree()));
    mock.answer("capture", Ok(json!({"id": "cap-0002"})));
    worker.send(NetCommand::Call {
        tag: "composition_tree".to_owned(),
        method: "composition_tree".to_owned(),
        params: tree_params(),
    });
    worker.send(NetCommand::Call {
        tag: "capture".to_owned(),
        method: "capture".to_owned(),
        params: json!({"camera": {}, "image": "", "scene_version": 0}),
    });

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut tree = None;
    let mut capture = None;
    while Instant::now() < deadline && (tree.is_none() || capture.is_none()) {
        for e in worker.drain() {
            match e {
                NetEvent::Reply { tag, result } => match tag.as_str() {
                    "composition_tree" => tree = Some(result.expect("the tree answered")),
                    "capture" => capture = Some(result.expect("the capture answered")),
                    other => panic!("an unexpected tag {other:?}"),
                },
                // A `Call` is not an edit: the worker must not report one as applied.
                NetEvent::Applied { what, .. } => panic!("{what} is not an edit"),
                NetEvent::Live { .. } => {}
                other => panic!("an unexpected event {other:?}"),
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    let tree = tree.expect("the tree reply arrived");
    let capture = capture.expect("the capture reply arrived");
    assert_eq!(
        CompositionTree::parse(&tree).expect("parses").layers.len(),
        3
    );
    assert_eq!(capture["id"], json!("cap-0002"));
}

#[test]
fn the_worker_reports_a_refused_call_with_its_code_not_a_closed_connection() {
    let mock = Mock::start();
    let worker = NetWorker::spawn(mock.addr.clone(), Box::new(|| {}));
    let _ = mock.next_seen();
    let _ = mock.next_seen();
    // The mock answers `-32601` for a method no answer was queued for.
    worker.send(NetCommand::Call {
        tag: "composition_tree".to_owned(),
        method: "composition_tree".to_owned(),
        params: tree_params(),
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut code = None;
    while Instant::now() < deadline && code.is_none() {
        for e in worker.drain() {
            match e {
                NetEvent::Reply { tag: _, result } => {
                    let err = result.expect_err("refused");
                    code = Some(err.code);
                }
                NetEvent::Closed(d) => panic!("a refusal is not a closed connection: {d:?}"),
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(code, Some(METHOD_NOT_FOUND), "the code reaches the caller");
}

// --- 4. a quiet server must not starve the worker --------------------------

#[test]
fn a_request_sent_while_the_server_is_quiet_still_goes_out() {
    // A mock that says nothing unless it is spoken to is the ordinary case for a
    // viewer sitting still: no snapshots, no notifications. The worker used to park
    // in a blocking read and only drain its command channel *after* the server
    // spoke, so a tree request or an undo handed to it now was never sent — and the
    // tree panel stayed on "请求中…" forever while the reply it was waiting for was
    // the answer to a request that had not left.
    let mock = Mock::start();
    let worker = NetWorker::spawn(mock.addr.clone(), Box::new(|| {}));
    let _ = mock.next_seen(); // hello
    let _ = mock.next_seen(); // subscribe

    // Let the worker get to its read and find nothing, the way an idle server looks.
    thread::sleep(Duration::from_millis(300));

    mock.answer("composition_tree", Ok(city_tree()));
    worker.send(NetCommand::Call {
        tag: "composition_tree".to_owned(),
        method: "composition_tree".to_owned(),
        params: tree_params(),
    });

    // The mock's own record is the assertion: the request must reach the wire while
    // the server says nothing.
    let seen = mock.next_seen();
    assert_eq!(
        seen.method, "composition_tree",
        "the request left the client"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut answered = false;
    while Instant::now() < deadline && !answered {
        for e in worker.drain() {
            if let NetEvent::Reply { result, .. } = e {
                assert!(result.is_ok());
                answered = true;
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(answered, "and its answer came back to the caller");
}

#[test]
fn an_idle_read_is_not_a_closed_connection() {
    // The other side of the same rule: a socket with an idle timeout returns
    // `E_WIRE_IDLE` for silence, and the worker must treat that as "go round again",
    // never as the connection ending. A viewer whose server is quiet for a while
    // must not report itself disconnected.
    let mock = Mock::start();
    let (mut client, _early) = ProtocolClient::open(&mock.addr).expect("greeting");
    let _ = mock.next_seen();
    let _ = mock.next_seen();
    let err = client
        .read_within(Duration::from_millis(30))
        .expect_err("nothing arrived");
    assert_eq!(err[0].code, rusd_viewer::net::E_WIRE_IDLE, "{err:?}");

    // And it recovers: the next line that does arrive parses normally.
    mock.answer("composition_tree", Ok(city_tree()));
    mock.push(json!({"method": "state_changed", "params": {"version": 9, "reason": "patch"}}));
    let wire = client.read_wire().expect("the notification");
    match wire {
        rusd_viewer::protocol::Wire::Notification(
            rusd_viewer::protocol::Notification::StateChanged { version, reason },
        ) => {
            assert_eq!(version, 9);
            assert_eq!(reason, "patch");
        }
        other => panic!("expected the notification, got {other:?}"),
    }
}

// --- 5. the copies ----------------------------------------------------------

#[test]
fn a_prim_row_copies_its_path_and_there_is_no_other_copy() {
    let tree = CompositionTree::parse(&city_tree()).expect("the tree parses");
    let rows = flatten(&rusd_viewer::tree::roots(&tree));
    let prim = rows
        .iter()
        .find(|(_, n)| n.kind == TreeNodeKind::Prim && n.label == "/World/City/Block_01")
        .expect("the block's prim row");
    assert_eq!(
        clipboard_for(&TreeSelection::of(&prim.1)).as_deref(),
        Some("/World/City/Block_01")
    );
    assert_eq!(
        prim_clipboard("/World/City/Block_01"),
        "/World/City/Block_01"
    );

    // An arc row stands for the prim it is authored on, so it copies that path; a
    // layer row copies nothing, because a layer identifier is not a prim path.
    let arc = rows
        .iter()
        .find(|(_, n)| n.kind == TreeNodeKind::Arc)
        .expect("an arc row");
    assert_eq!(
        clipboard_for(&TreeSelection::of(&arc.1)).as_deref(),
        Some("/World/City/Block_01")
    );
    let layer = rows
        .iter()
        .find(|(_, n)| n.kind == TreeNodeKind::Layer)
        .expect("a layer row");
    assert_eq!(clipboard_for(&TreeSelection::of(&layer.1)), None);
}

#[test]
fn a_drag_over_the_viewport_selects_the_texels_the_mask_marks() {
    // One rule, two callers: the pick rule maps a point to a texel, and the mask
    // marks the texels of the rectangle. The mask must therefore mark exactly the
    // pixels the drag covered — no more, no fewer.
    let vp = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 50.0));
    let frame = (200u32, 100u32);
    let sel = rect_from_drag(vp, egui::pos2(20.0, 10.0), egui::pos2(70.0, 40.0), frame)
        .expect("a selection");
    let mask = mask_rgba(frame, sel);
    let white_pixels = mask.chunks(4).filter(|p| p[0] == 255).count();
    assert_eq!(
        white_pixels as u32,
        sel.width() * sel.height(),
        "the mask's white area is the selection's"
    );
    assert_eq!(
        mask.len(),
        (frame.0 * frame.1 * 4) as usize,
        "and the mask is the frame's size, not the selection's"
    );
    // The texel at the drag's start point is inside, and the one a pixel outside is not.
    let at = |x: u32, y: u32| mask[((y * frame.0 + x) * 4) as usize] == 255;
    assert!(at(sel.x0, sel.y0), "the top-left texel is white");
    assert!(!at(sel.x0 - 1, sel.y0), "one texel left of it is not");
    assert!(!at(sel.x1, sel.y1 - 1), "one texel right of it is not");
}
