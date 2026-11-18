//! The v1.5.3 client contract (`docs/PROTOCOL.md` v1.5.3): **every prefix of the
//! composition is reachable from the tree panel, the root's included.**
//!
//! The three tiers a pipeline is read in are `root` (the input geometry and the
//! session overrides, with **no** cook op), the op rows (root plus the ops up to that
//! one), and `final` (the whole composition). v1.4 made the second tier clickable and
//! left a `final` button for the third; the first tier had **no control at all** — the
//! root layer's row was a plain structural row, so a user could not ask for the
//! input-only picture. This file pins what changed:
//!
//! 1. **The root layer's row is a preview control**: clicking it sends
//!    `preview{upto: <root layer id>}` — the layer's identifier, not a flag and not
//!    a null — and asks no `comp_node`.
//! 2. **It is marked exactly while it is the prefix in force**: [`panel::is_current_preview`]
//!    is true for it and false for the same row under another `upto`, and the drawn
//!    title gains the `≤` marker then and only then (painted, not just modelled).
//! 3. **Every other structural row is untouched**: the session layer, an arc, a variant
//!    set, and — the subtle one — an arc's leaf that *names* another layer, which is a
//!    `Layer` row with the target's id but is still clicked to inspect it.
//! 4. **`final` is still `preview{upto: null}`**, and it is a *different* request from
//!    the root row's: the root row sends an identifier, the button sends null.
//!
//! Nothing here needs a GPU or a window: the click rule is [`panel::action_for`], the
//! wire assertions go through a real `ProtocolClient` against a mock server, and the
//! marker assertion draws the production widget headlessly and reads the painted text.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use rusd_viewer::net::ProtocolClient;
use rusd_viewer::panel::{self, TreeAction, TreePanel};
use rusd_viewer::protocol::{PROTOCOL, preview_params};
use rusd_viewer::tree::{CompositionTree, TreeNode, TreeNodeKind, flatten};
use serde_json::{Value, json};

// --- the mock server ---------------------------------------------------------

/// One request the mock received.
#[derive(Clone, Debug)]
struct Received {
    method: String,
    params: Value,
}

/// A server that greets each connection and answers every request `{version}`.
///
/// What these tests assert is *which request* a click sends; no answer is read, so the
/// replies only have to be well formed.
struct Mock {
    addr: String,
    seen: Receiver<Received>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Mock {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr").to_string();
        let (seen_tx, seen_rx) = channel();
        let t = thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            serve(stream, seen_tx);
        });
        Self {
            addr,
            seen: seen_rx,
            thread: Some(t),
        }
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
        // The serving thread parks in `read_line` on the client's socket; joining it
        // would hang the harness, so it is detached as the older mocks here do.
        self.thread.take();
    }
}

/// Serve one connection: the greeting, then a reply per request.
fn serve(stream: TcpStream, seen: Sender<Received>) {
    let mut out = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    let mut version = 1u64;
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
        let params = v["params"].clone();
        if seen
            .send(Received {
                method: method.clone(),
                params,
            })
            .is_err()
        {
            return;
        }
        let reply = if method == "hello" {
            json!({"id": id, "result": {"protocol": PROTOCOL, "session": "mock", "version": version}})
        } else {
            version += 1;
            json!({"id": id, "result": {"version": version}})
        };
        if writeln!(out, "{reply}").is_err() || out.flush().is_err() {
            return;
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

// --- the payloads ------------------------------------------------------------

/// The mesh -> points -> instances chain, as a live server reports it: the stack in
/// **strength order** `session(0), root(1), instances(2), sample(3)`.
///
/// The arc points at the same layer it starts in, so this stack has no cross-layer
/// leaf; [`cross_layer_tree`] is the fixture that adds one.
fn pipeline_tree() -> Value {
    json!({
        "layers": [
            {"id": "anon:session", "kind": "session", "strength": 0},
            {"id": "mesh.usda", "kind": "root", "strength": 1},
            {"id": "op:instances/1", "kind": "sublayer", "strength": 2, "op": true,
             "op_name": "RusdInstances",
             "input": "/World/Mesh/points", "output": "op:instances/1"},
            {"id": "op:sample_points/0", "kind": "sublayer", "strength": 3, "op": true,
             "op_name": "RusdSamplePoints",
             "input": "/World/Mesh", "output": "op:sample_points/0"},
        ],
        "arcs": [
            {"id": "arc:0", "kind": "subLayer", "source_layer": "mesh.usda",
             "source_prim": "/", "target_layer": "mesh.usda", "target_prim": "/"},
        ],
        "prim_nodes": [
            {"id": "prim:/World/Mesh", "path": "/World/Mesh", "authored_in": ["mesh.usda"]},
        ],
        "variant_sets": [],
    })
}

/// The same stack plus a **cross-layer reference**, so the arc draws an extra `Layer`
/// row that names the target layer and is not a root row.
fn cross_layer_tree() -> Value {
    let mut tree = pipeline_tree();
    tree["layers"]
        .as_array_mut()
        .expect("layers")
        .push(json!({"id": "prototypes.usda", "kind": "reference", "strength": 4}));
    tree["arcs"] = json!([
        {"id": "arc:0", "kind": "reference", "source_layer": "mesh.usda",
         "source_prim": "/World/Mesh", "target_layer": "prototypes.usda",
         "target_prim": "/Prototypes/Mesh"},
    ]);
    tree
}

/// The rows of a tree, flattened with their depth.
fn rows_of(tree: &Value) -> Vec<(usize, TreeNode)> {
    let parsed = CompositionTree::parse(tree).expect("the payload parses");
    flatten(&rusd_viewer::tree::roots(&parsed))
}

/// The one row an id names.
fn row_by_id(rows: &[(usize, TreeNode)], id: &str) -> TreeNode {
    rows.iter()
        .map(|(_, n)| n)
        .find(|n| n.id == id)
        .unwrap_or_else(|| {
            panic!(
                "no row {id}: {:?}",
                rows.iter().map(|(_, n)| &n.id).collect::<Vec<_>>()
            )
        })
        .clone()
}

/// A panel loaded with `tree`, the way the shell loads one.
fn panel_of(tree: Value) -> TreePanel {
    let mut panel = TreePanel::new();
    panel.apply_tree(Ok(tree));
    panel
}

// --- 1. the root row's click over the wire ----------------------------------

#[test]
fn a_root_row_click_sends_preview_upto_the_root_id_and_asks_no_detail() {
    let mock = Mock::start();
    let mut client = connected(&mock);
    let rows = rows_of(&pipeline_tree());
    let root = row_by_id(&rows, "mesh.usda");

    // The fixture really is the ordinary root row: a layer, not an op, and flagged
    // as the root by the model rather than by a string the test compares.
    assert_eq!(root.kind, TreeNodeKind::Layer);
    assert!(!root.op, "the root layer is not a cook op");
    assert!(root.root, "the model marks `kind: root` rows");

    let action = panel::action_for(&root);
    assert_eq!(
        action,
        TreeAction::Preview(Some("mesh.usda".to_owned())),
        "the root row previews up to the root layer, not to a `comp_node` detail"
    );

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
        json!({"upto": "mesh.usda"}),
        "the wire carries the root layer's identifier (v1.5.3: preview{{upto: <root id>}})"
    );
    assert!(mock.try_next().is_none(), "no comp_node ask went with it");
}

#[test]
fn the_three_tiers_are_three_different_preview_requests() {
    // The whole point of v1.5.3: root, an op, and final are each reachable, and each
    // is its own request. A client that folded the root tier into `null` would leave
    // the input-only picture unreachable, which is the bug this version fixes.
    let mock = Mock::start();
    let mut client = connected(&mock);
    let rows = rows_of(&pipeline_tree());

    let root_row = row_by_id(&rows, "mesh.usda");
    let sample_row = row_by_id(&rows, "op:sample_points/0");
    assert!(sample_row.op, "the sample op's row is an op row");

    let mut sent = Vec::new();
    for action in [
        panel::action_for(&root_row),
        panel::action_for(&sample_row),
        panel::final_action(),
    ] {
        let TreeAction::Preview(upto) = action else {
            panic!("a preview control must preview: {action:?}");
        };
        client
            .request("preview", preview_params(upto.as_deref()))
            .expect("sent")
            .expect("answered");
        sent.push(mock.next_method("preview"));
    }

    assert_eq!(
        sent,
        vec![
            json!({"upto": "mesh.usda"}),
            json!({"upto": "op:sample_points/0"}),
            json!({"upto": null}),
        ],
        "root, op, final: three distinct `upto` values, in that order"
    );
}

// --- 2. the highlight and the marker ---------------------------------------

#[test]
fn the_root_row_is_marked_exactly_while_it_is_the_prefix_in_force() {
    let rows = rows_of(&pipeline_tree());
    let root = row_by_id(&rows, "mesh.usda");
    let sample = row_by_id(&rows, "op:sample_points/0");
    let session = row_by_id(&rows, "anon:session");

    // The final composition: nothing is marked, the root row included.
    assert!(!panel::is_current_preview(&root, None));

    // On the root prefix: the root row is marked and no op row is.
    assert!(
        panel::is_current_preview(&root, Some("mesh.usda")),
        "preview == the root id ⇒ the root row is the one in force"
    );
    assert!(!panel::is_current_preview(&sample, Some("mesh.usda")));

    // On an op prefix: the reverse, so the two tiers cannot both look marked.
    assert!(!panel::is_current_preview(
        &root,
        Some("op:sample_points/0")
    ));
    assert!(panel::is_current_preview(
        &sample,
        Some("op:sample_points/0")
    ));

    // A structural row whose id is the root's string is still not a control: the rule
    // is the row's own flags, never a string comparison at the call site.
    let mut fake = root.clone();
    fake.kind = TreeNodeKind::Arc;
    assert!(
        !panel::is_current_preview(&fake, Some("mesh.usda")),
        "an arc row with the root layer's id is not a preview row"
    );
    let mut leaf = root.clone();
    leaf.root = false;
    assert!(
        !panel::is_current_preview(&leaf, Some("mesh.usda")),
        "an arc's leaf naming the root layer is not a preview row either"
    );
    // And the session layer is never one, whatever the preview says.
    assert!(!panel::is_current_preview(&session, Some("anon:session")));
}

#[test]
fn the_marker_is_drawn_on_the_root_row_only_while_it_is_the_prefix() {
    // The model's marker is only real if it reaches the screen, so the production
    // widget is drawn headlessly and the painted text read back.
    let mut panel = panel_of(pipeline_tree());
    let root_row = row_by_id(&flatten(&panel.roots()), "mesh.usda");

    // Not the prefix: the root row is drawn as its plain label.
    assert_eq!(
        panel::control_row_title(&root_row, false),
        root_row.label,
        "the final composition draws the root row unmarked"
    );
    let unmarked = painted_texts(&panel);
    assert!(
        !unmarked.iter().any(|t| t.starts_with("≤ mesh.usda")),
        "no `≤` on the root row while the final composition is on screen: {unmarked:?}"
    );

    // The prefix: the same row gains the marker, and the painted title proves it.
    panel.preview = Some("mesh.usda".to_owned());
    let marked = panel::control_row_title(&root_row, true);
    assert_eq!(marked, format!("≤ {}", root_row.label), "marker plus label");
    let painted = painted_texts(&panel);
    assert!(
        painted.iter().any(|t| t.starts_with("≤ mesh.usda")),
        "the root row must be painted with `≤` while it is the prefix: {painted:?}"
    );
    // Exactly one row carries it: the marker means "this is where you are".
    assert_eq!(
        painted
            .iter()
            .filter(|t| t.starts_with("≤ mesh.usda"))
            .count(),
        1,
        "one marked root row: {painted:?}"
    );
    // An op row keeps the marker its own label already carries — the control title
    // must not add a second one.
    let sample_row = row_by_id(&flatten(&panel.roots()), "op:sample_points/0");
    assert!(
        sample_row.label.starts_with("≤ "),
        "the op row's label carries its own marker: {}",
        sample_row.label
    );
    assert_eq!(
        panel::control_row_title(&sample_row, false),
        sample_row.label,
        "an op row's drawn title is its label, marker and all"
    );
    assert_eq!(
        panel::control_row_title(&sample_row, true),
        sample_row.label,
        "and being the current preview does not double it"
    );
}

// --- 3. the structural rows are untouched ---------------------------------

#[test]
fn every_non_control_row_still_selects_and_sends_comp_node() {
    let mock = Mock::start();
    let mut client = connected(&mock);
    let rows = rows_of(&cross_layer_tree());

    // The session layer, an arc, the arc's leaf that names another layer, and a
    // variant set. The leaf is the interesting one: it is a `Layer` row whose id is
    // another layer's, and v1.5.3 explicitly leaves it a selection.
    let session = row_by_id(&rows, "anon:session");
    let arc = row_by_id(&rows, "arc:0");
    let leaf = rows
        .iter()
        .map(|(_, n)| n)
        .find(|n| n.kind == TreeNodeKind::Layer && n.label == "→ prototypes.usda")
        .expect("the arc draws a leaf naming its target layer")
        .clone();
    assert_eq!(leaf.id, "prototypes.usda");
    assert!(!leaf.op && !leaf.root, "the leaf is neither control");

    for node in [&session, &arc, &leaf] {
        assert!(
            !panel::is_preview_control(node),
            "{} must not be a preview control",
            node.label
        );
        let action = panel::action_for(node);
        let TreeAction::Select(sel) = action else {
            panic!("{} must select, not preview: {action:?}", node.label);
        };
        client
            .request("comp_node", rusd_viewer::tree::comp_node_params(&sel.id))
            .expect("sent")
            .expect("answered");
        assert_eq!(mock.next_method("comp_node")["id"], json!(sel.id));
    }
    assert!(mock.try_next().is_none(), "not one preview was sent");
}

#[test]
fn is_preview_control_is_the_one_rule_for_both_kinds_of_control_row() {
    let rows = rows_of(&cross_layer_tree());
    let root = row_by_id(&rows, "mesh.usda");
    let sample = row_by_id(&rows, "op:sample_points/0");
    let session = row_by_id(&rows, "anon:session");
    let arc = row_by_id(&rows, "arc:0");

    assert!(panel::is_preview_control(&root), "the root row (v1.5.3)");
    assert!(panel::is_preview_control(&sample), "an op row (v1.4)");
    assert!(!panel::is_preview_control(&session));
    assert!(!panel::is_preview_control(&arc));

    // The click rule, the highlight rule and the drawing all read this one predicate,
    // so a row cannot preview in one place and select in another.
    for node in [&root, &sample] {
        let action = panel::action_for(node);
        assert!(
            matches!(&action, TreeAction::Preview(Some(id)) if id == &node.id),
            "{} previews its own id: {action:?}",
            node.label
        );
    }
}

// --- 4. the pre-v1.5.3 picture still draws --------------------------------

#[test]
fn a_stack_with_no_root_row_is_unchanged() {
    // A server that reports no `kind: root` layer (a hand-built stack, or a cook whose
    // root is a sublayer) has no root control — and nothing else changes: the op rows
    // preview, everything else selects.
    let tree = json!({
        "layers": [
            {"id": "anon:session", "kind": "session", "strength": 0},
            {"id": "mesh.usda", "kind": "sublayer", "strength": 1},
            {"id": "rusd_city", "kind": "sublayer", "strength": 2, "op": true},
        ],
        "arcs": [], "prim_nodes": [], "variant_sets": [],
    });
    let rows = rows_of(&tree);
    let sublayer = row_by_id(&rows, "mesh.usda");
    assert!(!sublayer.root, "a plain sublayer is not a root row");
    let action = panel::action_for(&sublayer);
    assert!(
        matches!(action, TreeAction::Select(_)),
        "no root row ⇒ no root control: {action:?}"
    );
    let op = row_by_id(&rows, "rusd_city");
    assert_eq!(
        panel::action_for(&op),
        TreeAction::Preview(Some("rusd_city".to_owned())),
        "and the op row still previews (v1.4)"
    );
}

#[test]
fn the_root_rows_subtree_is_still_drawn_now_that_the_row_is_a_control() {
    // Making the root row a control must not cost it its children: a control row is
    // drawn as a plain row *with its subtree always visible* (the v1.4 rule for op
    // rows, which v1.5.3 extends to the root), not as a twisty whose click would
    // mean two things. So the arcs the root layer authors are still on screen.
    let panel = panel_of(cross_layer_tree());
    let texts = painted_texts(&panel);
    assert!(
        texts.iter().any(|t| t.starts_with("mesh.usda")),
        "the root row itself is drawn: {texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|t| t.contains("reference → prototypes.usda:/Prototypes/Mesh")),
        "and the arc it authors, which is its child: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("→ prototypes.usda")),
        "and the leaf that names the target layer: {texts:?}"
    );
}
// --- drawing the production widget headlessly -----------------------------

/// Draw the tree panel with the production widget and return every text it painted.
///
/// No GPU and no window: the point is that a marker built in the model but never
/// drawn would not appear here.
fn painted_texts(panel: &TreePanel) -> Vec<String> {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1000.0, 700.0),
        )),
        ..Default::default()
    };
    let out = ctx.run_ui(raw, |root| {
        egui::CentralPanel::no_frame().show(root, |ui| {
            let _ = panel::tree_panel(ui, panel);
        });
    });
    // The font atlas is not applied anywhere here, and epaint refuses to drop a
    // `TexturesDelta` with unhandled deltas — the same `clear()` a real backend's
    // upload would follow.
    let mut delta = out.textures_delta;
    delta.clear();
    let mut texts = Vec::new();
    for clipped in &out.shapes {
        if let egui::epaint::Shape::Text(t) = &clipped.shape {
            texts.push(t.galley.text().to_owned());
        }
    }
    texts
}
