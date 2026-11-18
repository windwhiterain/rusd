//! **What a `state_changed` makes the viewer do** — the round-two UX defect, pinned
//! over a real socket against a mock server, with no GPU and no window.
//!
//! The真窗验收 found two things stale after an `open` switched documents
//! (`mesh_scatter` → `city`):
//!
//! 1. **the composition tree still showed the *previous* document** — 3 layers and
//!    241 arc Tuft references — until 「刷新」 was clicked by hand. It was asked
//!    once per connection, so a new document's layers never arrived.
//! 2. **the camera kept the old document's fit** (`dist 23.2`, `target (5, 4.9, 0.7)`),
//!    so the new document's scene sat zoomed into a corner of the frame the other
//!    one had chosen.
//!
//! The rules that fix both live in [`rusd_viewer::view`], one copy shared by the
//! window and these tests:
//!
//! | notification | `composition_tree` | camera |
//! | --- | --- | --- |
//! | `state_changed{reason:"open"}` | asked again | **refitted** to the new document |
//! | `state_changed{reason:"patch"/"opinion"/"undo"/"redo"/"reload"}` | asked again | untouched |
//! | a `preview` (not a `state_changed` at all) | neither | neither |
//!
//! Why the refit happens on the *reason* and not on the snapshot: the contract fixes
//! the order `响应 → stage_snapshot → diagnostics → state_changed`, so by the time
//! `open` arrives the new document's scene has already been adopted — as a **later**
//! scene, which keeps the orbit. The one scene that must frame the view therefore
//! arrives before anyone knows it is a new document, and `open` is the message that
//! says so. `view::camera_for_state_change` fits from the bounds now on screen and
//! checks `version == model.version` so a reason that speaks for another scene cannot
//! fit the wrong box (it re-arms the first-fit latch instead).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use rusd_viewer::compose::base64_encode;
use rusd_viewer::net::{NetEvent, NetWorker};
use rusd_viewer::panel::TreePanel;
use rusd_viewer::protocol::{self, Notification, PROTOCOL, Wire};
use rusd_viewer::session::{ClientModel, Effect};
use rusd_viewer::view::{self, asks_tree, camera_for_scene, framing};
use serde_json::{Value, json};

// --- the mock server ---------------------------------------------------------

/// One request the mock received.
#[derive(Clone, Debug)]
struct Received {
    method: String,
    #[allow(dead_code, reason = "the params are checked by other suites")]
    params: Value,
}

/// A server that greets, answers every later request with `{version}`, and can push
/// whatever notification a test asks for.
struct Mock {
    addr: String,
    seen: Receiver<Received>,
    pushes: Sender<Value>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Mock {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr").to_string();
        let (seen_tx, seen_rx) = channel();
        let (push_tx, push_rx) = channel::<Value>();
        let t = thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            serve(stream, seen_tx, push_rx);
        });
        Self {
            addr,
            seen: seen_rx,
            pushes: push_tx,
            thread: Some(t),
        }
    }

    fn next_method(&self, want: &str) -> Received {
        let r = self
            .seen
            .recv_timeout(Duration::from_secs(30))
            .expect("the server received a request");
        assert_eq!(r.method, want, "expected {want}, got {}", r.method);
        r
    }

    /// Wait a moment for a request and return it — for asserting that **nothing**
    /// was sent. Long enough for a request to have crossed a loopback socket.
    fn try_next(&self) -> Option<Received> {
        self.seen.recv_timeout(Duration::from_millis(300)).ok()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        // The serving thread parks in a read on the client's socket and in
        // `recv_timeout` on the push channel; neither wakes because this test is done,
        // and joining it would hang the harness. Detaching is what the other suites
        // here do — the process ends the thread with it.
        self.thread.take();
    }
}

/// Serve one connection: greeting for the first two requests, then a `{version}`
/// reply per request and whatever the test pushes.
fn serve(stream: TcpStream, seen: Sender<Received>, pushes: Receiver<Value>) {
    let mut out = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    let (req_tx, req_rx) = channel::<(u64, String, Value)>();
    thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
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

    for _ in 0..2 {
        let Ok((id, method, params)) = req_rx.recv() else {
            return;
        };
        let _ = seen.send(Received { method, params });
        let reply =
            json!({"id": id, "result": {"protocol": PROTOCOL, "session": "mock", "version": 0}});
        if !write_line(&mut out, &reply) {
            return;
        }
    }

    let mut version = 0u64;
    loop {
        std::thread::yield_now();
        while let Ok((id, method, params)) = req_rx.try_recv() {
            let _ = seen.send(Received { method, params });
            version += 1;
            let reply = json!({"id": id, "result": {"version": version}});
            if !write_line(&mut out, &reply) {
                return;
            }
            while let Ok(n) = pushes.try_recv() {
                if !write_line(&mut out, &n) {
                    return;
                }
            }
        }
        match pushes.recv_timeout(Duration::from_millis(20)) {
            Ok(n) => {
                if !write_line(&mut out, &n) {
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

// --- payloads ----------------------------------------------------------------

/// Two documents with **very different** extents, as `usda`, so a fit taken from the
/// wrong one is unmistakable in the numbers.
///
/// `wide` is two cubes 200 apart (`/World/A` at the origin, `/World/B` at x=200);
/// `narrow` is the single cube at the origin. That is the shape of the real defect the
/// other way round: a document framed for one extent, then a document of another
/// extent arriving under it. The sizes here are hand-written rather than cooked
/// because what is under test is the client's reaction to the reason — not `rusd-proc`.
fn wide_doc() -> Vec<u8> {
    concat!(
        "#usda 1.0\n\n",
        "def Xform \"World\"\n{\n}\n\n",
        "def Cube \"A\"\n{\n    double size = 2\n",
        "    uniform token[] xformOpOrder = [\"xformOp:translate\"]\n",
        "    uniform point3f xformOp:translate = (0, 0, 0)\n}\n\n",
        "def Cube \"B\"\n{\n    double size = 2\n",
        "    uniform token[] xformOpOrder = [\"xformOp:translate\"]\n",
        "    uniform point3f xformOp:translate = (200, 0, 0)\n}\n",
    )
    .as_bytes()
    .to_vec()
}

/// The narrow document: one cube at the origin.
fn narrow_doc() -> Vec<u8> {
    concat!(
        "#usda 1.0\n\n",
        "def Xform \"World\"\n{\n}\n\n",
        "def Cube \"A\"\n{\n    double size = 2\n",
        "    uniform token[] xformOpOrder = [\"xformOp:translate\"]\n",
        "    uniform point3f xformOp:translate = (0, 0, 0)\n}\n",
    )
    .as_bytes()
    .to_vec()
}

/// A `stage_snapshot` for `bytes` at `version`.
fn snapshot(version: u64, bytes: &[u8]) -> Value {
    json!({
        "method": "stage_snapshot",
        "params": {"version": version, "format": "usda", "data": base64_encode(bytes)},
    })
}

/// A `state_changed` naming `reason` at `version`.
fn state_changed(version: u64, reason: &str) -> Value {
    json!({"method": "state_changed", "params": {"version": version, "reason": reason}})
}

/// Feed one `stage_snapshot` and return the scene it made.
fn adopt(model: &mut ClientModel, version: u64, bytes: &[u8]) -> rusd_viewer::session::MadeScene {
    match model.apply(&notification(snapshot(version, bytes))) {
        Effect::NewScene(made) => *made,
        other => panic!("expected a scene, got {}", effect_name(&other)),
    }
}

/// Feed one `state_changed` and return what the model made of it.
fn tell(model: &mut ClientModel, version: u64, reason: &str) -> Effect {
    model.apply(&notification(state_changed(version, reason)))
}

/// One notification value, parsed the way the reader thread parses it.
fn notification(value: Value) -> Notification {
    let Wire::Notification(n) =
        protocol::parse_line(&value.to_string()).expect("the notification parses")
    else {
        panic!("not a notification: {value}");
    };
    n
}

/// An `Effect` as a word (`Effect` has no `Debug`: its `NewScene` arm holds a scene).
fn effect_name(e: &Effect) -> &'static str {
    match e {
        Effect::NewScene(_) => "NewScene",
        Effect::Status => "Status",
        Effect::StateChanged { .. } => "StateChanged",
        Effect::Nothing => "Nothing",
        Effect::Failed(_) => "Failed",
    }
}

// --- 1. the rule table, as data ----------------------------------------------

#[test]
fn every_reason_that_can_move_the_layers_asks_the_tree_and_no_other_does() {
    // The six the contract names. A tree read once per connection is what left the
    // panel showing the previous document after an `open`.
    for reason in ["open", "patch", "opinion", "undo", "redo", "reload"] {
        assert!(asks_tree(reason), "{reason} must re-read the tree");
    }
    // `preview` is not a `state_changed` (it advances no version), and a reason this
    // build does not know must not start asking on every notification.
    for reason in ["preview", "", "unknown-future-reason"] {
        assert!(!asks_tree(reason), "{reason} must not re-read the tree");
    }
}

#[test]
fn only_open_refits_the_camera() {
    // The refit is decided by the reason, and `open` is the only one that is a new
    // *document*. A patch/opinion/undo/redo/reload re-composes the same document, so
    // the user's orbit stands (REMOTE §6: 相机归 client).
    let wide = wide_doc();
    let mut model = ClientModel::new();
    let first = adopt(&mut model, 1, &wide);
    camera_for_scene(&mut model, &first, 16.0 / 9.0).expect("the first scene frames");
    for reason in ["patch", "opinion", "undo", "redo", "reload", "preview"] {
        let plan = view::camera_for_state_change(&mut model, 1, reason, 16.0 / 9.0);
        assert!(
            plan.is_none(),
            "{reason} must keep the user's orbit, got {plan:?}"
        );
    }
}

// --- 2. `open`: the tree is asked again, over the wire -----------------------

/// Take `mode`'s answer for a `composition_tree` request, the way the shell does when
/// `NetEvent::Reply` arrives: the round trip is over, so the next reason may ask again.
///
/// A test that skipped this would leave `asking` set, and the *next* reason would be
/// coalesced rather than sent — which is the behaviour under test in
/// `an_opinion_burst_asks_once_and_owes_one_more_read`, and noise in every other test.
fn settle_tree(tree: &mut TreePanel) {
    tree.apply_tree(Ok(json!({
        "layers": [],
        "arcs": [],
        "prim_nodes": [],
        "variant_sets": [],
    })));
    assert!(!tree.asking, "the round trip is over");
}

#[test]
fn an_open_asks_the_composition_tree_again() {
    // The defect, as the wire sees it: a tree asked once per connection. Here the
    // first ask is the connection's own; the second must follow the `open`.
    let mock = Mock::start();
    let worker = NetWorker::spawn(mock.addr.clone(), Box::new(|| {}));
    assert_eq!(mock.next_method("hello").method, "hello");
    assert_eq!(mock.next_method("subscribe").method, "subscribe");

    let wide = wide_doc();
    let mut model = ClientModel::new();
    let mut tree = TreePanel::new();
    adopt(&mut model, 1, &wide);
    view::ask_tree(&worker, &mut tree);
    mock.next_method("composition_tree");
    settle_tree(&mut tree);

    // The new document, in the contract's order: snapshot first, then the reason.
    adopt(&mut model, 2, &narrow_doc());
    let effect = tell(&mut model, 2, "open");
    assert!(
        matches!(effect, Effect::StateChanged { version: 2, ref reason } if reason == "open"),
        "a state_changed says which change it was, got {}",
        effect_name(&effect)
    );
    let plan = view::apply_state_change(&mut model, &mut tree, &worker, 2, "open", 16.0 / 9.0);
    assert!(plan.asked_tree, "an open re-reads the tree");
    mock.next_method("composition_tree");
}

#[test]
fn an_opinion_burst_asks_once_and_owes_one_more_read() {
    // A gizmo drag sends an opinion up to 30 times a second (`docs/PROTOCOL.md`: ≤ 30 Hz),
    // and each one is a `state_changed{reason:"opinion"}`. Asking the tree per reason
    // would put 30 structural reads of the authority stack in flight during one drag, so
    // a reason that arrives while a read is outstanding is **coalesced**: recorded, not
    // sent. Both halves matter — the tree must follow the server, and it must do so once
    // per round trip, not once per message.
    let mock = Mock::start();
    let worker = NetWorker::spawn(mock.addr.clone(), Box::new(|| {}));
    assert_eq!(mock.next_method("hello").method, "hello");
    assert_eq!(mock.next_method("subscribe").method, "subscribe");

    let mut model = ClientModel::new();
    let mut tree = TreePanel::new();
    let framed = adopt(&mut model, 1, &wide_doc());
    camera_for_scene(&mut model, &framed, 16.0 / 9.0);

    // One round trip's worth of reasons: twelve opinions, as a drag's first 400 ms.
    for version in 2..=13 {
        tell(&mut model, version, "opinion");
        let plan = view::apply_state_change(
            &mut model,
            &mut tree,
            &worker,
            version,
            "opinion",
            16.0 / 9.0,
        );
        assert!(plan.asked_tree, "every opinion may move the layers");
    }
    assert!(tree.asking, "a request is outstanding");
    assert!(
        tree.reread_pending,
        "and a read is owed, or the tree would stop following"
    );
    // Exactly one request crossed the wire, not twelve.
    assert_eq!(
        mock.try_next().map(|r| r.method),
        Some("composition_tree".to_owned()),
        "the burst sent the first read"
    );
    assert!(
        mock.try_next().is_none(),
        "and the other eleven were coalesced, not sent"
    );

    // The shell takes the owed read when the answer arrives (app::Shell::on_reply).
    settle_tree(&mut tree);
    assert!(tree.take_reread(), "one more read answers all twelve");
    assert!(!tree.take_reread(), "and only one");
}

#[test]
fn a_patch_asks_the_tree_again_and_a_preview_asks_nothing() {
    let mock = Mock::start();
    let worker = NetWorker::spawn(mock.addr.clone(), Box::new(|| {}));
    assert_eq!(mock.next_method("hello").method, "hello");
    assert_eq!(mock.next_method("subscribe").method, "subscribe");

    let wide = wide_doc();
    let mut model = ClientModel::new();
    let mut tree = TreePanel::new();
    let framed = adopt(&mut model, 1, &wide);
    camera_for_scene(&mut model, &framed, 16.0 / 9.0);

    tell(&mut model, 2, "patch");
    let plan = view::apply_state_change(&mut model, &mut tree, &worker, 2, "patch", 16.0 / 9.0);
    assert!(plan.asked_tree, "a patch may add or drop a cook op layer");
    assert!(plan.refit.is_none(), "a patch keeps the camera");
    mock.next_method("composition_tree");

    // A preview is not a mutation and raises no `state_changed`, so neither rule runs.
    // Asserted through the same entry point, with the reason a server would never send:
    // whatever arrives, neither the tree nor the camera may move.
    let plan = view::apply_state_change(&mut model, &mut tree, &worker, 2, "preview", 16.0 / 9.0);
    assert!(!plan.asked_tree, "a preview asks no tree");
    assert!(plan.refit.is_none(), "a preview keeps the camera");
    assert!(
        mock.try_next().is_none(),
        "a preview sent no request at all"
    );
}

#[test]
fn a_preview_frame_moves_neither_the_tree_nor_the_camera() {
    // The whole path for a prefix frame, as the server sends it: a `stage_snapshot`
    // carrying `upto`, and no `state_changed` afterwards (v1.4: preview 不推进 scene
    // version). The client must therefore adopt the new picture and ask nothing.
    let mock = Mock::start();
    let worker = NetWorker::spawn(mock.addr.clone(), Box::new(|| {}));
    assert_eq!(mock.next_method("hello").method, "hello");
    assert_eq!(mock.next_method("subscribe").method, "subscribe");

    let mut model = ClientModel::new();
    let mut tree = TreePanel::new();
    let first = adopt(&mut model, 1, &wide_doc());
    let orbit = camera_for_scene(&mut model, &first, 16.0 / 9.0).expect("the first scene frames");
    view::ask_tree(&worker, &mut tree);
    mock.next_method("composition_tree");
    settle_tree(&mut tree);

    // The prefix frame: the same version, a different composition.
    let prefix = json!({
        "method": "stage_snapshot",
        "params": {
            "version": 1,
            "format": "usda",
            "data": base64_encode(&narrow_doc()),
            "upto": "rusd_city",
        },
    });
    assert!(
        matches!(model.apply(&notification(prefix)), Effect::NewScene(_)),
        "a prefix frame is a picture"
    );
    assert_eq!(model.preview.as_deref(), Some("rusd_city"));
    assert!(
        camera_for_scene(&mut model, &first, 16.0 / 9.0).is_none(),
        "a prefix frame is not a first fit"
    );
    assert!(
        mock.try_next().is_none(),
        "a prefix frame asks no tree and sends nothing"
    );
    // And the orbit the user had is untouched: the prefix is the same scene, smaller.
    let (inside, _) = framing(&orbit, first.bounds, 1280, 720);
    assert!(inside, "the original fit still frames the original scene");
}

// --- 3. `open`: the camera is fitted to the new document ----------------------

#[test]
fn an_open_refits_the_camera_to_the_new_documents_bounds() {
    // The second defect: after the document switch the camera kept `dist 23.2` and
    // `target (5, 4.9, 0.7)` — the *previous* document's fit — so the new scene sat
    // zoomed into a corner. The new document's scene is adopted as a later scene (the
    // latch says so), so the fit has to come from the reason.
    let wide = wide_doc();
    let narrow = narrow_doc();
    let mut model = ClientModel::new();
    let mut tree = TreePanel::new();
    let mock = Mock::start();
    let worker = NetWorker::spawn(mock.addr.clone(), Box::new(|| {}));
    assert_eq!(mock.next_method("hello").method, "hello");
    assert_eq!(mock.next_method("subscribe").method, "subscribe");

    // Document one, framed by the connection's first fit.
    let first = adopt(&mut model, 1, &wide);
    let orbit_wide = camera_for_scene(&mut model, &first, 16.0 / 9.0).expect("first fit");
    let wide_bounds = first.bounds;

    // Document two, in the contract's order: the scene arrives first and is adopted as
    // a **later** scene, which by the camera rule keeps the orbit.
    let second = adopt(&mut model, 2, &narrow);
    assert_eq!(model.version, Some(2));
    assert!(
        camera_for_scene(&mut model, &second, 16.0 / 9.0).is_none(),
        "the snapshot alone does not frame: it is a later scene"
    );
    let narrow_bounds = second.bounds;
    assert!(
        narrow_bounds.1[0] < wide_bounds.1[0] / 4.0,
        "the two documents really differ: {wide_bounds:?} vs {narrow_bounds:?}"
    );

    // The reason is what says a new document arrived.
    let plan = view::apply_state_change(&mut model, &mut tree, &worker, 2, "open", 16.0 / 9.0);
    let refit = plan.refit.expect("an open refits the camera");
    assert_ne!(
        refit, orbit_wide,
        "the old document's fit must not be kept as the new one"
    );
    assert_eq!(
        refit.target[0],
        f64::midpoint(narrow_bounds.0[0], narrow_bounds.1[0]),
        "the fit centres on the new document's box, not the old one's"
    );
    let (inside, worst) = framing(&refit, narrow_bounds, 1280, 720);
    assert!(
        inside,
        "and the new document is framed by it: worst |NDC| {worst:.4}"
    );
    // The old document's fit was right for the old document, which is why keeping it
    // was wrong rather than merely stale.
    let (wide_inside, _) = framing(&orbit_wide, wide_bounds, 1280, 720);
    assert!(wide_inside, "the first fit framed the first document");
    mock.next_method("composition_tree");
}

#[test]
fn an_open_for_a_scene_that_has_not_arrived_arms_the_first_fit_instead() {
    // A server that sent the reason before the snapshot — or a client that dropped that
    // snapshot as stale — must not fit the bounds of the scene still on screen: those
    // belong to the old document. The fit is deferred instead, by re-arming the latch,
    // and the next scene to arrive frames as a first one would.
    let wide = wide_doc();
    let narrow = narrow_doc();
    let mut model = ClientModel::new();
    let first = adopt(&mut model, 1, &wide);
    camera_for_scene(&mut model, &first, 16.0 / 9.0).expect("first fit");

    let plan = view::camera_for_state_change(&mut model, 2, "open", 16.0 / 9.0);
    assert!(
        plan.is_none(),
        "nothing is fitted from the old document's bounds"
    );
    // Deferred, not lost: the next scene frames.
    let second = adopt(&mut model, 2, &narrow);
    let orbit = camera_for_scene(&mut model, &second, 16.0 / 9.0)
        .expect("the latch was re-armed, so the next scene frames");
    let (inside, worst) = framing(&orbit, second.bounds, 1280, 720);
    assert!(
        inside,
        "and it frames the new document: worst |NDC| {worst:.4}"
    );
    // And it is the new box, not the one that happened to be on screen.
    assert_eq!(
        orbit.target[0],
        f64::midpoint(second.bounds.0[0], second.bounds.1[0])
    );
}

#[test]
fn two_opens_in_a_row_both_refit() {
    // After the first `open` the view holds a real fit (`mark_framed`), which would make
    // the *second* `open` keep it if the rule read the latch instead of the reason. A
    // user switching documents twice must get two fits.
    let mock = Mock::start();
    let worker = NetWorker::spawn(mock.addr.clone(), Box::new(|| {}));
    assert_eq!(mock.next_method("hello").method, "hello");
    assert_eq!(mock.next_method("subscribe").method, "subscribe");

    let mut model = ClientModel::new();
    let mut tree = TreePanel::new();
    let wide = wide_doc();
    let narrow = narrow_doc();

    let first = adopt(&mut model, 1, &wide);
    camera_for_scene(&mut model, &first, 16.0 / 9.0).expect("first fit");
    let plan_a = view::apply_state_change(&mut model, &mut tree, &worker, 1, "open", 16.0 / 9.0);
    let orbit_a = plan_a.refit.expect("the first open fits");
    mock.next_method("composition_tree");
    // The round trip ends before the second document is switched to: two `open`s inside
    // one round trip are coalesced into one read on purpose (a drag's opinions are why),
    // and this test is about the camera, not about that.
    settle_tree(&mut tree);

    let second = adopt(&mut model, 2, &narrow);
    let plan_b = view::apply_state_change(&mut model, &mut tree, &worker, 2, "open", 16.0 / 9.0);
    let orbit_b = plan_b.refit.expect("the second open fits too");
    mock.next_method("composition_tree");
    assert_ne!(
        orbit_a.distance, orbit_b.distance,
        "the two documents are framed for their own extents"
    );
    assert!(orbit_b.target[0] < orbit_a.target[0] / 4.0);
    assert_eq!(
        orbit_b.target[0],
        f64::midpoint(second.bounds.0[0], second.bounds.1[0]),
        "the second fit centres on the second document"
    );
}

// --- 4. a whole sequence over one socket -------------------------------------

#[test]
fn the_window_path_a_scene_then_a_reason_reaches_the_same_two_decisions() {
    // The `NetWorker` route, which is how the window hears all of this: the snapshot and
    // the reason both come out of `drain()` as notifications, and the model's `Effect`
    // is what routes them. A `state_changed` reaching the shell as `Status` (the old
    // shape) could not act on either rule — this asserts the effect now carries the
    // reason that decides them.
    let mock = Mock::start();
    let worker = NetWorker::spawn(mock.addr.clone(), Box::new(|| {}));
    assert_eq!(mock.next_method("hello").method, "hello");
    assert_eq!(mock.next_method("subscribe").method, "subscribe");

    let mut model = ClientModel::new();
    let mut tree = TreePanel::new();

    let first = adopt(&mut model, 1, &wide_doc());
    camera_for_scene(&mut model, &first, 16.0 / 9.0).expect("first fit");
    view::ask_tree(&worker, &mut tree);
    mock.next_method("composition_tree");
    settle_tree(&mut tree);

    // The server pushes the new document, then says why (`mock.pushes` is the path a
    // real server takes, and it is what makes the ordering assertion real).
    let _ = mock.pushes.send(snapshot(2, &narrow_doc()));
    let _ = mock.pushes.send(state_changed(2, "open"));

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut saw_scene = false;
    let mut saw_open = false;
    while Instant::now() < deadline && !(saw_scene && saw_open) {
        for e in worker.drain() {
            let NetEvent::Notification(n) = e else {
                continue;
            };
            match model.apply(&n) {
                Effect::NewScene(made) => {
                    saw_scene = true;
                    // A later scene: the orbit the user has is kept (the fix's premise).
                    assert!(camera_for_scene(&mut model, &made, 16.0 / 9.0).is_none());
                }
                Effect::StateChanged { version, reason } => {
                    let plan = view::apply_state_change(
                        &mut model,
                        &mut tree,
                        &worker,
                        version,
                        &reason,
                        16.0 / 9.0,
                    );
                    assert!(plan.refit.is_some(), "an open fits the new document");
                    assert!(plan.asked_tree, "an open re-reads the tree");
                    saw_open = true;
                }
                _ => {}
            }
        }
        if !(saw_scene && saw_open) {
            thread::sleep(Duration::from_millis(10));
        }
    }
    assert!(saw_scene, "the new document reached the model");
    assert!(saw_open, "the open reached the model as its own effect");
    mock.next_method("composition_tree");
}
