//! The v1.4 client contract (`docs/PROTOCOL.md` v1.4 / v1.4.1, D14 削片 B-client):
//! **op rows preview a prefix**, and the prefix is a frame the viewport draws.
//!
//! What this file pins, all of it over a real socket against a **mock server** that
//! speaks the contract itself, none of it needing a GPU or a window:
//!
//! 1. **The op row's click goes to the wire as `preview{upto: <layer>}`** — and only an
//!    op row does. A layer row, an arc row, a variant set: each still asks
//!    `comp_node{id}` and sends no `preview` at all (`docs/PROTOCOL.md` v1.4: 点非 op
//!    节点仍走 comp_node 详情，不改预览).
//! 2. **`final` is `preview{upto: null}`** — the key present and null, not an omission.
//! 3. **A prefix frame changes the geometry, not the camera.** A `stage_snapshot` whose
//!    `upto` is a cook op carries a *different* layer; the client draws it, keeps its
//!    orbit, and its `preview` says which prefix it is. Dropping the prefix's geometry or
//!    re-fitting the camera on it would both be visible in the window and neither is in
//!    the contract.
//! 4. **A prefix frame does not move the scene version**, so the next real mutation is
//!    still newer than the floor (`ClientModel::apply`'s rule, which is the subtle one:
//!    a naive `accept()` on a prefix's equal version would freeze the view for good).
//! 5. **`upto` is parsed off the wire**, `null`/absent as final, and a non-string is a
//!    diagnostic rather than a silently-ignored wrong frame.
//! 6. **The overlay is compact**: ≤ 11 pt, ≤ 460 pt wide, ≤ 25 % of the viewport, and
//!    one row when collapsed — checked over the window sizes this viewer has run at.
//!
//! The overlay's own arithmetic has its unit tests in `crate::overlay`; what is here is
//! the *bound at real sizes* and the wiring, because those are the two things an
//! integration test can see and a unit test cannot.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use rusd_viewer::compose::base64_encode;
use rusd_viewer::net::{NetCommand, NetEvent, NetWorker, ProtocolClient};
use rusd_viewer::overlay::{self, FONT_PT, MAX_AREA_FRACTION, MAX_WIDTH_PT};
use rusd_viewer::panel::{self, TreeAction, TreePanel};
use rusd_viewer::protocol::{
    self, Notification, PROTOCOL, Snapshot, Wire, preview_line, preview_params,
};
use rusd_viewer::session::ClientModel;
use rusd_viewer::tree::CompositionTree;
use serde_json::{Value, json};

// --- the mock server ---------------------------------------------------------

/// One request the mock received.
#[derive(Clone, Debug)]
struct Received {
    method: String,
    params: Value,
}

/// A server that greets, answers every request with `{version}`, and can push
/// whatever notification a test asks for.
struct Mock {
    addr: String,
    seen: Receiver<Received>,
    /// The queued answers, held so the channel stays open for the serving loop.
    /// Nothing calls it: the mock answers `{version}` unless a test needs otherwise.
    _answers: Sender<Value>,
    pushes: Sender<Value>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Mock {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr").to_string();
        let (seen_tx, seen_rx) = channel();
        let (answer_tx, answer_rx) = channel::<Value>();
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
            _answers: answer_tx,
            pushes: push_tx,
            thread: Some(t),
        }
    }

    /// Queue one notification to push after the request is answered.
    fn push(&self, value: Value) {
        let _ = self.pushes.send(value);
    }

    fn next_seen(&self) -> Received {
        self.seen
            .recv_timeout(Duration::from_secs(30))
            .expect("the server received a request")
    }

    fn next_method(&self, want: &str) -> Value {
        let r = self.next_seen();
        assert_eq!(r.method, want, "expected {want}, got {}", r.method);
        r.params
    }

    /// Wait for any request and return it — for asserting that **nothing** was sent.
    fn try_next(&self) -> Option<Received> {
        self.seen.recv_timeout(Duration::from_millis(250)).ok()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        // The serving thread parks in `recv_timeout` on the push channel and in a read
        // on the client's socket; neither wakes because this test is done, and joining
        // it would hang the harness. Detaching is what the older mocks here do — the
        // test process ends the thread with it, and nothing it writes matters then.
        self.thread.take();
    }
}

/// Serve one connection: greeting first, then a reply per request and whatever the
/// test pushes, in that order (`docs/PROTOCOL.md` "消息顺序": 先答请求，再报后果).
fn serve(
    stream: TcpStream,
    seen: Sender<Received>,
    answers: Receiver<Value>,
    pushes: Receiver<Value>,
) {
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
            json!({"id": id, "result": {"protocol": PROTOCOL, "session": "mock", "version": 1}});
        if !write_line(&mut out, &reply) {
            return;
        }
    }

    let mut version = 1u64;
    loop {
        std::thread::yield_now();
        while let Ok((id, method, params)) = req_rx.try_recv() {
            let _ = seen.send(Received { method, params });
            version += 1;
            let reply = answers
                .try_recv()
                .map(|v| json!({"id": id, "result": v}))
                .unwrap_or_else(|_| json!({"id": id, "result": {"version": version}}));
            if !write_line(&mut out, &reply) {
                return;
            }
            // The consequences after the answer, as the contract orders them.
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

/// A connection already past the greeting.
fn connected(mock: &Mock) -> ProtocolClient {
    let (client, early) = ProtocolClient::open(&mock.addr).expect("the greeting succeeds");
    assert!(early.is_empty(), "nothing raced the greeting");
    assert_eq!(mock.next_seen().method, "hello");
    assert_eq!(mock.next_seen().method, "subscribe");
    client
}

// --- payloads ----------------------------------------------------------------

/// A `composition_tree` shaped like the city's, with the two cook op layers v1.4 adds.
///
/// `rusd_city` is the op the city's cook writes; `rusd_scatter` is the one after it, so
/// prefixing at `rusd_city` must exclude the scatter's prims — the assertion D14's
/// server-side 验收 asks for, mirrored on this side as geometry that differs.
///
/// The two ops' strengths are the **real** relation (v1.5.2): a cook step inserts its
/// output at position 0 of the sublayer stack, so `rusd_scatter`, cooking later, is the
/// **stronger** of the two (2) and `rusd_city` the weaker (3). The panel reverses the op
/// run so the rows read cook order, which is why the first op row below is `rusd_city`.
fn city_tree() -> Value {
    json!({
        "layers": [
            {"id": "anon:session", "kind": "session", "strength": 0},
            {"id": "city.rusd.cooked.usda", "kind": "root", "strength": 1},
            {"id": "rusd_scatter", "kind": "sublayer", "strength": 2, "op": true},
            {"id": "rusd_city", "kind": "sublayer", "strength": 3, "op": true},
            {"id": "prototypes.usda", "kind": "reference", "strength": 4},
        ],
        "arcs": [
            {"id": "arc:0", "kind": "reference",
             "source_layer": "city.rusd.cooked.usda", "source_prim": "/World/City/Block_01",
             "target_layer": "prototypes.usda", "target_prim": "/Prototypes/Block"},
        ],
        "prim_nodes": [
            {"id": "prim:/World/City/Block_01", "path": "/World/City/Block_01",
             "authored_in": ["city.rusd.cooked.usda"]},
            // The prim the variant set belongs to: the set is only drawn under a layer
            // that authors its prim (v1.3: layer_subtree keeps vset rows, drops prim rows).
            {"id": "prim:/Prototypes/Block", "path": "/Prototypes/Block",
             "authored_in": ["prototypes.usda"]},
        ],
        "variant_sets": [
            {"id": "vset:0", "prim": "/Prototypes/Block", "name": "size",
             "variants": ["small", "large"], "selection": "small"},
        ],
    })
}

/// A new picture's`Effect` as a word, for a failure message (`Effect` deliberately has
/// no `Debug`: its `NewScene` arm carries a whole composed scene).
fn effect_name(e: &rusd_viewer::session::Effect) -> &'static str {
    use rusd_viewer::session::Effect;
    match e {
        Effect::NewScene(_) => "NewScene",
        Effect::Status => "Status",
        Effect::StateChanged { .. } => "StateChanged",
        Effect::Nothing => "Nothing",
        Effect::Failed(_) => "Failed",
    }
}

/// A `stage_snapshot` carrying `bytes`, optionally as the prefix up to `upto`.
fn snapshot(version: u64, bytes: &[u8], upto: Option<&str>) -> Value {
    let mut params = json!({
        "version": version,
        "format": "usda",
        "data": base64_encode(bytes),
    });
    if let Some(u) = upto {
        params["upto"] = json!(u);
    }
    json!({"method": "stage_snapshot", "params": params})
}

/// Two boxes: a **final** composition (both) and a **prefix** (only the first), as
/// `usda` so the difference is visible in the text as well as the geometry.
///
/// The two layers are written by hand rather than cooked, because what this file
/// asserts is the client's handling of `upto` — cooking the city twice would test
/// `rusd-proc`, which is another agent's half of D14.
fn two_layers() -> (Vec<u8>, Vec<u8>) {
    let both = b"#usda 1.0\n\ndef Xform \"World\"\n{\n}\n\n\
                 def Cube \"A\"\n{\n    double size = 2\n\n    \
                 uniform token[] xformOpOrder = [\"xformOp:translate\"]\n    \
                 uniform point3f xformOp:translate = (0, 0, 0)\n}\n\n\
                 def Cube \"B\"\n{\n    double size = 2\n\n    \
                 uniform token[] xformOpOrder = [\"xformOp:translate\"]\n    \
                 uniform point3f xformOp:translate = (10, 0, 0)\n}\n"
        .to_vec();
    let prefix = b"#usda 1.0\n\ndef Xform \"World\"\n{\n}\n\n\
                   def Cube \"A\"\n{\n    double size = 2\n\n    \
                   uniform token[] xformOpOrder = [\"xformOp:translate\"]\n    \
                   uniform point3f xformOp:translate = (0, 0, 0)\n}\n"
        .to_vec();
    (both, prefix)
}

// --- 1. the op row's click, over the wire ------------------------------------

#[test]
fn an_op_row_click_sends_preview_upto_and_asks_no_detail() {
    let mock = Mock::start();
    let mut client = connected(&mock);
    let tree = CompositionTree::parse(&city_tree()).expect("the payload parses");
    let rows = rusd_viewer::tree::roots(&tree);

    let mut panel = TreePanel::new();
    panel.apply_tree(Ok(city_tree()));
    let op_row = rows
        .iter()
        .find(|r| r.op)
        .expect("the payload has an op layer");
    assert_eq!(
        op_row.id, "rusd_city",
        "the first op row is the op that cooked first (v1.5.2: op rows read cook order)"
    );

    let action = panel::action_for(op_row);
    assert_eq!(
        action,
        TreeAction::Preview(Some("rusd_city".to_owned())),
        "an op row previews up to itself"
    );

    // The request the shell would send, against the real mock.
    let TreeAction::Preview(upto) = action else {
        unreachable!()
    };
    client
        .request("preview", preview_params(upto.as_deref()))
        .expect("sent")
        .expect("answered");
    let params = mock.next_method("preview");
    assert_eq!(
        params,
        json!({"upto": "rusd_city"}),
        "the wire carries the layer identifier the row printed"
    );
    // And nothing asked for a detail: one request, one method.
    assert!(mock.try_next().is_none(), "no second request was sent");
}

#[test]
fn a_non_op_row_still_asks_comp_node_and_never_sends_preview() {
    let mock = Mock::start();
    let mut client = connected(&mock);
    let tree = CompositionTree::parse(&city_tree()).expect("parses");
    let rows = rusd_viewer::tree::roots(&tree);

    // Three non-control kinds: a layer, an arc, and a variant set. The layer has to
    // be one that is neither an op row nor **the root row**, because v1.5.3 makes
    // the root a preview control too (`docs/PROTOCOL.md` v1.5.3: root 行成为预览
    // 控制行); the session layer is such a row in this payload.
    let layer_row = rows
        .iter()
        .find(|r| r.kind == rusd_viewer::tree::TreeNodeKind::Layer && !r.op && !r.root)
        .expect("a plain layer root");
    let arc_row = rows
        .iter()
        .flat_map(|r| r.children.iter())
        .find(|c| c.kind == rusd_viewer::tree::TreeNodeKind::Arc)
        .expect("an arc row");
    let vset_row = rows
        .iter()
        .flat_map(|r| r.children.iter())
        .flat_map(|c| c.children.iter())
        .find(|c| c.kind == rusd_viewer::tree::TreeNodeKind::VariantSet)
        .expect("a variant set");

    for node in [layer_row, arc_row, vset_row] {
        let action = panel::action_for(node);
        let TreeAction::Select(sel) = action else {
            panic!("{} must select, not preview: {action:?}", node.label);
        };
        assert!(!node.op, "{} is not an op row", node.label);
        assert!(!node.root, "{} is not the root row", node.label);
        client
            .request("comp_node", rusd_viewer::tree::comp_node_params(&sel.id))
            .expect("sent")
            .expect("answered");
        let params = mock.next_method("comp_node");
        assert_eq!(params["id"], json!(sel.id));
    }
    // Not one `preview` in the batch.
    assert!(mock.try_next().is_none(), "no preview was sent");
}

#[test]
fn the_final_control_sends_upto_null_with_the_key_present() {
    let mock = Mock::start();
    let mut client = connected(&mock);
    let action = panel::final_action();
    assert_eq!(action, TreeAction::Preview(None));

    let TreeAction::Preview(upto) = action else {
        unreachable!()
    };
    client
        .request("preview", preview_params(upto.as_deref()))
        .expect("sent")
        .expect("answered");
    let params = mock.next_method("preview");
    assert_eq!(
        params,
        json!({"upto": null}),
        "「final」 is an explicit null, not an omitted key"
    );
    assert!(
        params.as_object().unwrap().contains_key("upto"),
        "the key must be present: {params}"
    );
}

#[test]
fn the_preview_line_spells_both_states() {
    assert_eq!(
        preview_line(7, Some("rusd_city")),
        "{\"id\":7,\"method\":\"preview\",\"params\":{\"upto\":\"rusd_city\"}}\n"
    );
    assert_eq!(
        preview_line(8, None),
        "{\"id\":8,\"method\":\"preview\",\"params\":{\"upto\":null}}\n"
    );
}

// --- 2. the panel reports the preview the row is in --------------------------

#[test]
fn the_panel_marks_the_current_prefix_and_the_final_state() {
    let mut panel = TreePanel::new();
    panel.apply_tree(Ok(city_tree()));
    // The panel reads the state from the snapshot echo, not from its own clicks.
    assert_eq!(panel.preview, None, "the final composition to start");

    let rows = rusd_viewer::tree::flatten(&panel.roots());
    let op_row = rows
        .iter()
        .map(|(_, n)| n)
        .find(|n| n.op && n.id == "rusd_city")
        .expect("the cook op's row");
    let other_op = rows
        .iter()
        .map(|(_, n)| n)
        .find(|n| n.op && n.id == "rusd_scatter")
        .expect("the scatter op's row");

    // Nothing is marked while the final composition is on screen.
    assert!(!panel::is_current_preview(op_row, panel.preview.as_deref()));

    // A prefix marks exactly the row it names — and not its neighbour.
    panel.preview = Some("rusd_city".to_owned());
    assert!(
        panel::is_current_preview(op_row, panel.preview.as_deref()),
        "the cook op's row is the one in force"
    );
    assert!(
        !panel::is_current_preview(other_op, panel.preview.as_deref()),
        "the scatter op is not, even though it is also an op row"
    );

    // And a non-op row is never marked, whatever its id: the rule needs all three
    // terms, which is why it is not a string comparison at the call site.
    let layer_row = rows
        .iter()
        .map(|(_, n)| n)
        .find(|n| !n.op && n.kind == rusd_viewer::tree::TreeNodeKind::Layer && n.id == "rusd_city");
    if let Some(n) = layer_row {
        assert!(!panel::is_current_preview(n, Some("rusd_city")));
    }
    let mut fake = op_row.clone();
    fake.kind = rusd_viewer::tree::TreeNodeKind::Arc;
    assert!(
        !panel::is_current_preview(&fake, Some("rusd_city")),
        "an arc row with an op layer's id is not a preview row"
    );
}

#[test]
fn the_panel_status_says_which_composition_is_on_screen() {
    let mut model = ClientModel::new();
    assert_eq!(model.preview_label(), "最终");
    model.preview = Some("rusd_city".to_owned());
    assert!(model.preview_label().contains("rusd_city"));
    assert!(
        model.status_line_zh().contains("预览 ≤ rusd_city"),
        "{}",
        model.status_line_zh()
    );
    assert!(
        model.status_line().contains("preview<=rusd_city"),
        "{}",
        model.status_line()
    );
    model.preview = None;
    assert_eq!(model.preview_label(), "最终");
    assert!(
        !model.status_line().contains("preview"),
        "{}",
        model.status_line()
    );
}

// --- 3. the prefix frame: geometry changes, camera does not ------------------

#[test]
fn a_prefix_frame_draws_its_own_geometry_and_keeps_the_camera() {
    let (both, prefix) = two_layers();
    let mut model = ClientModel::new();
    model.set_aspect(16.0 / 9.0);

    // The final composition first: two boxes, and it is the one that fits the camera.
    let first = model.apply(&Notification::Snapshot(Snapshot {
        version: 5,
        format: "usda".to_owned(),
        data: base64_encode(&both),
        upto: None,
    }));
    let rusd_viewer::session::Effect::NewScene(full) = first else {
        panic!("the final frame is a scene, not {}", effect_name(&first));
    };
    let full_items = full.item_count();
    let full_bounds = full.bounds;
    model.adopt(&full);
    assert!(full_items >= 2, "both boxes are there: {full_items}");
    // The first scene of a connection frames the camera; that is the fit.
    let orbit = rusd_viewer::view::camera_for_scene(&mut model, &full, 16.0 / 9.0)
        .expect("the first scene frames the view");
    assert!(orbit.distance > 0.0, "the fit is a real orbit: {orbit:?}");
    model.adopt(&full);
    assert_eq!(model.preview, None, "the final frame");

    // Now the prefix: the same scene version, a *different* composition.
    let second = model.apply(&Notification::Snapshot(Snapshot {
        version: 5,
        format: "usda".to_owned(),
        data: base64_encode(&prefix),
        upto: Some("rusd_city".to_owned()),
    }));
    let rusd_viewer::session::Effect::NewScene(part) = second else {
        panic!("a prefix frame is a scene, not {}", effect_name(&second));
    };
    assert_eq!(model.preview.as_deref(), Some("rusd_city"));
    assert!(
        part.item_count() < full_items,
        "the prefix has fewer prims ({} vs {full_items}) — the scatter is excluded",
        part.item_count()
    );
    assert_ne!(
        part.bounds, full_bounds,
        "and its geometry differs, which is what the viewport draws"
    );
    // The camera is **not** re-fitted: this is the second scene of the connection.
    assert_eq!(
        rusd_viewer::view::camera_for_scene(&mut model, &part, 16.0 / 9.0),
        None,
        "a prefix must not jerk the camera away from the user's orbit"
    );
}

#[test]
fn a_repeated_version_is_accepted_only_when_the_preview_changed() {
    // The rule behind "preview 不推进 scene version" and the discard rule meeting: a
    // prefix frame and the final frame share a version, and *both* are new pictures.
    // A naive `accept()` would drop the prefix; a blanket pass would re-draw duplicates.
    let (both, prefix) = two_layers();
    let mut model = ClientModel::new();
    let final12 = Notification::Snapshot(Snapshot {
        version: 12,
        format: "usda".to_owned(),
        data: base64_encode(&both),
        upto: None,
    });

    assert!(matches!(
        model.apply(&final12),
        rusd_viewer::session::Effect::NewScene(_)
    ));
    // The same frame again: a duplicate, discarded.
    assert!(matches!(
        model.apply(&final12),
        rusd_viewer::session::Effect::Nothing
    ));
    assert_eq!(model.state.discarded, 1);

    // The prefix at the **same version**: a new picture, accepted.
    let pref = Notification::Snapshot(Snapshot {
        version: 12,
        format: "usda".to_owned(),
        data: base64_encode(&prefix),
        upto: Some("rusd_city".to_owned()),
    });
    assert!(matches!(
        model.apply(&pref),
        rusd_viewer::session::Effect::NewScene(_)
    ));
    assert_eq!(model.preview.as_deref(), Some("rusd_city"));
    // The same prefix again: a duplicate.
    assert!(matches!(
        model.apply(&pref),
        rusd_viewer::session::Effect::Nothing
    ));

    // Back to final at the same version: a new picture again, and no re-fit requirement.
    assert!(matches!(
        model.apply(&final12),
        rusd_viewer::session::Effect::NewScene(_)
    ));
    assert_eq!(model.preview, None);

    // **And the floor did not move past 12**: a real mutation at 13 still arrives.
    let next = Notification::Snapshot(Snapshot {
        version: 13,
        format: "usda".to_owned(),
        data: base64_encode(&both),
        upto: None,
    });
    assert!(
        matches!(
            model.apply(&next),
            rusd_viewer::session::Effect::NewScene(_)
        ),
        "a prefix frame must not raise the version floor past the scene's own version"
    );
    assert_eq!(model.version, Some(13));
}

#[test]
fn a_stale_final_frame_after_a_prefix_is_discarded() {
    // The floor rule still applies to a *final* frame: a mutation older than the
    // version on screen must not roll the view back, preview or not.
    let (both, _prefix) = two_layers();
    let mut model = ClientModel::new();
    model.apply(&Notification::Snapshot(Snapshot {
        version: 9,
        format: "usda".to_owned(),
        data: base64_encode(&both),
        upto: None,
    }));
    let stale = Notification::Snapshot(Snapshot {
        version: 8,
        format: "usda".to_owned(),
        data: base64_encode(&both),
        upto: None,
    });
    assert!(matches!(
        model.apply(&stale),
        rusd_viewer::session::Effect::Nothing
    ));
    assert_eq!(model.version, Some(9), "the view did not roll back");
}

// --- 4. upto off the wire ----------------------------------------------------

#[test]
fn upto_parses_and_a_null_or_absent_one_is_the_final_composition() {
    let with = protocol::parse_line(
        "{\"method\":\"stage_snapshot\",\"params\":{\"version\":4,\"format\":\"usda\",\
         \"data\":\"AA==\",\"upto\":\"rusd_city\"}}",
    )
    .expect("parses");
    let Wire::Notification(Notification::Snapshot(s)) = with else {
        panic!("a snapshot: {with:?}");
    };
    assert_eq!(s.upto.as_deref(), Some("rusd_city"));
    assert!(s.is_prefix());

    for line in [
        // Null explicitly.
        "{\"method\":\"stage_snapshot\",\"params\":{\"version\":4,\"format\":\"usda\",\
         \"data\":\"AA==\",\"upto\":null}}",
        // Absent entirely: a pre-v1.4 server.
        "{\"method\":\"stage_snapshot\",\"params\":{\"version\":4,\"format\":\"usda\",\
         \"data\":\"AA==\"}}",
    ] {
        let w = protocol::parse_line(line).expect("parses");
        let Wire::Notification(Notification::Snapshot(s)) = w else {
            panic!("a snapshot: {w:?}");
        };
        assert_eq!(s.upto, None, "{line}");
        assert!(!s.is_prefix());
    }
}

#[test]
fn a_numeric_upto_is_a_diagnostic_not_a_silent_final_frame() {
    // `upto` is a layer identifier or null. A number here is a server bug, and treating
    // it as "final" would show a frame that is not the one the server said it made.
    let line = "{\"method\":\"stage_snapshot\",\"params\":{\"version\":4,\"format\":\"usda\",\
                \"data\":\"AA==\",\"upto\":7}}";
    let err = protocol::parse_line(line).expect_err("refused");
    assert_eq!(err.code, "E_WIRE_SHAPE", "{err:?}");
    assert!(err.message.contains("upto"), "{}", err.message);
}

#[test]
fn the_first_snapshot_at_version_zero_is_accepted() {
    // A server that never seeded a version in `hello` still has a first frame, and that
    // frame at version 0 must not read as "a duplicate of nothing" — the floor is an
    // `Option`, not `0`.
    let (both, _prefix) = two_layers();
    let mut model = ClientModel::new();
    let e = model.apply(&Notification::Snapshot(Snapshot {
        version: 0,
        format: "usda".to_owned(),
        data: base64_encode(&both),
        upto: None,
    }));
    assert!(
        matches!(e, rusd_viewer::session::Effect::NewScene(_)),
        "the first frame at version 0 is a scene, not {}",
        effect_name(&e)
    );
    assert_eq!(model.version, Some(0));
    // And an exact duplicate of it is still dropped.
    let dup = model.apply(&Notification::Snapshot(Snapshot {
        version: 0,
        format: "usda".to_owned(),
        data: base64_encode(&both),
        upto: None,
    }));
    assert!(matches!(dup, rusd_viewer::session::Effect::Nothing));
}

// --- 5. the worker's view of a preview --------------------------------------

#[test]
fn the_worker_sends_preview_and_the_model_ends_up_on_the_prefix() {
    // The whole path the window takes: a `NetCommand::Call` for `preview`, and the
    // prefix frame the server pushes afterwards reaching the model.
    let (both, prefix) = two_layers();
    let mock = Mock::start();
    let worker = NetWorker::spawn(mock.addr.clone(), Box::new(|| {}));
    assert_eq!(mock.next_seen().method, "hello");
    assert_eq!(mock.next_seen().method, "subscribe");

    worker.send(NetCommand::Call {
        tag: "preview".to_owned(),
        method: "preview".to_owned(),
        params: preview_params(Some("rusd_city")),
    });
    let params = mock.next_method("preview");
    assert_eq!(params, json!({"upto": "rusd_city"}));

    // The server's consequence: the prefix frame.
    mock.push(snapshot(3, &prefix, Some("rusd_city")));

    let mut model = ClientModel::new();
    let mut saw_scene = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline && !saw_scene {
        for e in worker.drain() {
            if let NetEvent::Notification(n) = e
                && matches!(model.apply(&n), rusd_viewer::session::Effect::NewScene(_))
            {
                saw_scene = true;
            }
        }
        if !saw_scene {
            thread::sleep(Duration::from_millis(10));
        }
    }
    assert!(saw_scene, "the prefix frame reached the model");
    assert_eq!(model.preview.as_deref(), Some("rusd_city"));
    assert!(
        model.status_line_zh().contains("预览"),
        "{}",
        model.status_line_zh()
    );
    // Keep `both` used: the final frame is what the prefix is a part of.
    assert!(both.len() > prefix.len());
}

#[test]
fn a_final_frame_after_a_prefix_clears_the_preview() {
    let (both, prefix) = two_layers();
    let mut model = ClientModel::new();
    model.apply(&Notification::Snapshot(Snapshot {
        version: 1,
        format: "usda".to_owned(),
        data: base64_encode(&prefix),
        upto: Some("rusd_city".to_owned()),
    }));
    assert_eq!(model.preview.as_deref(), Some("rusd_city"));
    // `preview{null}` answered: the server pushes the final composition.
    model.apply(&Notification::Snapshot(Snapshot {
        version: 1,
        format: "usda".to_owned(),
        data: base64_encode(&both),
        upto: None,
    }));
    assert_eq!(model.preview, None, "the label must say 最终 again");
    assert_eq!(model.preview_label(), "最终");
}

// --- 6. the overlay's bounds at the window sizes that ran -------------------

/// The window sizes the viewer has actually been measured at, in points.
const WINDOWS: [(f32, f32); 4] = [
    (1426.3, 763.4),  // 1440x800 physical at 1.75
    (1280.0, 670.0),  // the size smoke17 reported
    (823.0, 457.0),   // the size the layout defect was found at
    (2560.0, 1357.0), // the full screen at 1.0, for the other extreme
];

/// The rows the overlay draws at a real window: the report, the screen lines, and the
/// partition lines, measured by the same synthetic font the unit tests use.
fn overlay_rows(measure: impl Fn(&str) -> f32) -> Vec<egui::Vec2> {
    let screen = rusd_viewer::viewport::ScreenSpace {
        inner_physical: (2496, 1336),
        scale_factor: 1.75,
        screen_points: egui::vec2(1426.3, 763.4),
        presented_physical: (2496, 1336),
        os: None,
    };
    let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1426.3, 703.4));
    let partition = rusd_viewer::viewport::Partition {
        window: egui::vec2(1426.3, 763.4),
        left: 280.0,
        top: 26.0,
        bottom: 34.0,
        rect,
    };
    let report = rusd_viewer::viewport::OverlayReport {
        pixels_per_point: 1.75,
        rect_points: (1426.3, 703.4),
        viewport_texels: (2496, 1231),
        frame_texels: (2496, 1231),
        texels_match: true,
        frames_produced: 7,
        frames_shown: 7,
        orbit_distance: 1287.0,
        orbit_target: [116.9, 116.3, 25.0],
        drawn_distance: Some(1287.0),
        drawn_target: Some([116.9, 116.3, 25.0]),
    };
    let mut lines = vec!["composition 最终".to_owned()];
    lines.extend(report.lines());
    lines.extend(screen.lines());
    lines.extend(partition.lines());
    let max_row = overlay::row_width(egui::Rect::from_min_size(
        egui::pos2(0.0, 0.0),
        egui::vec2(1280.0, 670.0),
    ));
    lines
        .iter()
        .flat_map(|l| overlay::wrap(l, max_row, &measure))
        .map(|r| egui::Vec2::new(measure(&r), overlay::LINE_PT))
        .collect()
}

/// A synthetic font: six points a character.
fn six_pt(s: &str) -> f32 {
    s.chars().count() as f32 * 6.0
}

#[test]
fn the_overlay_obeys_its_three_bounds_at_every_window_that_has_run() {
    let rows = overlay_rows(six_pt);
    assert!(
        rows.len() > 13,
        "the real rows wrap into more than 13: {}",
        rows.len()
    );
    for (w, h) in WINDOWS {
        let viewport = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h));
        let plan = overlay::plan(viewport, &rows, false);
        assert_eq!(plan.font_pt, FONT_PT, "{w}x{h}: ≤ 11 pt");
        assert!(plan.font_pt <= 11.0 + 1e-4, "the contract's font bound");
        assert!(
            plan.rect.width() <= MAX_WIDTH_PT + 1e-4,
            "{w}x{h}: {} pt wide, over the 460 pt bound",
            plan.rect.width()
        );
        let frac = plan.area_fraction(viewport);
        assert!(
            frac <= MAX_AREA_FRACTION + 1e-6,
            "{w}x{h}: the overlay covers {:.1}% of the viewport, over the 25% bound",
            frac * 100.0
        );
        assert!(plan.within_bounds(viewport));
        // Whatever is not drawn is counted, never silently dropped.
        assert_eq!(plan.shown + plan.hidden, rows.len(), "{w}x{h}: {plan:?}");
    }
}

#[test]
fn collapsed_at_a_real_window_is_one_short_row_and_the_way_back_is_named() {
    let rows = overlay_rows(six_pt);
    let viewport = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1280.0, 670.0));
    let expanded = overlay::plan(viewport, &rows, false);
    let collapsed = overlay::plan(viewport, &rows, true);
    assert!(collapsed.collapsed);
    assert_eq!(collapsed.shown, 1);
    assert!(
        collapsed.rect.height() <= overlay::LINE_PT + 2.0 * overlay::PAD_PT + 1e-3,
        "one row: {}",
        collapsed.rect.height()
    );
    assert!(
        collapsed.rect.height() * 4.0 < expanded.rect.height(),
        "collapsing must give the picture back: {} vs {}",
        collapsed.rect.height(),
        expanded.rect.height()
    );
    assert!(overlay::marker_text(12, true).contains("o"));
    assert!(overlay::marker_text(12, false).contains("o"));
    assert!(collapsed.within_bounds(viewport));
}

#[test]
fn the_overlay_write_its_composition_line_first_so_a_prefix_screenshot_says_so() {
    // The overlay's first row is the composition, by construction in `app`, and the
    // label it comes from is `ClientModel::preview_label` — this pins the pair.
    let mut model = ClientModel::new();
    let final_row = format!("composition {}", model.preview_label());
    assert_eq!(final_row, "composition 最终");
    model.preview = Some("rusd_city".to_owned());
    let prefix_row = format!("composition {}", model.preview_label());
    assert_eq!(prefix_row, "composition 预览 ≤ rusd_city");
    // Both fit the box: a prefix's label is the longer one, and it must not wrap into
    // a second row that a screenshot would read as a separate fact.
    assert!(six_pt(&prefix_row) <= MAX_WIDTH_PT - 2.0 * overlay::PAD_PT);
}
