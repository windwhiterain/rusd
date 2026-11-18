//! The v1.5 client contract (`docs/PROTOCOL.md` v1.5 / v1.5.1): **an op row says
//! which op it is, what it reads and what it writes** — and the click is unchanged.
//!
//! What this file pins, none of it needing a GPU or a window:
//!
//! 1. **The op row's title is the generator's schema name** (`RusdSamplePoints`,
//!    `RusdInstances`, `RusdScatter`), taken from the wire's new `op_name`, and its
//!    subtitle reads `{input} → {output summary}` with the output elided to 20
//!    characters. Rows keep the **cook order** the layer stack already has.
//! 2. **`input` null is `(parametric)`** — and so is a field the server omitted, since
//!    v1.5.1 makes the three fields additive.
//! 3. **A pre-v1.5 server draws exactly what it used to**: no `op_name` means the
//!    anonymous identifier, no `op` metadata means no invented one, and nothing is
//!    reported as an error. The old picture must not break on the new client.
//! 4. **The click is untouched**: an op row still sends `preview{upto: <that layer id>}`
//!    — the row's *id*, not the generator name and not the output field — and a
//!    non-op row still asks `comp_node{id}` with no `preview` at all.
//! 5. **The two halves actually reach the screen**: the panel is drawn headlessly with
//!    the production widget and the painted text is read back, so a title built in
//!    `tree.rs` but never drawn would fail here.
//!
//! The preview's own accept rules (a prefix does not raise the version floor, the
//! same-version frame) are v1.4.2's and are exercised in `preview_v14.rs`; nothing
//! here touches them.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use rusd_viewer::net::ProtocolClient;
use rusd_viewer::panel::{self, TreeAction, TreePanel};
use rusd_viewer::protocol::{PROTOCOL, preview_params};
use rusd_viewer::tree::{
    CompositionTree, TreeModel, TreeNodeKind, comp_node_params, elide, flatten,
};
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
/// Nothing more is needed: what these tests assert is *which request* a click sends:
/// no answer is read, so the mock's replies only have to be well formed.
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

// --- payloads ----------------------------------------------------------------

/// The mesh -> points -> instances chain v1.5 adds, with the metadata the server
/// joins onto each op layer (`CookOp.layer_id == layers[].id`, v1.5.1).
///
/// The input of the instances op is the **points prim the sample op wrote**, which is
/// the paragraph the pipeline is: three states, three layers, one order.
fn pipeline_tree() -> Value {
    json!({
        "layers": [
            {"id": "anon:session", "kind": "session", "strength": 0},
            {"id": "mesh.usda", "kind": "root", "strength": 1},
            {"id": "op:sample_points/0", "kind": "sublayer", "strength": 2, "op": true,
             "op_name": "RusdSamplePoints",
             "input": "/World/Mesh", "output": "op:sample_points/0"},
            {"id": "op:instances/1", "kind": "sublayer", "strength": 3, "op": true,
             "op_name": "RusdInstances",
             "input": "/World/Mesh/points", "output": "op:instances/1"},
        ],
        "arcs": [
            {"id": "arc:0", "kind": "subLayer", "source_layer": "mesh.usda",
             "source_prim": "/", "target_layer": "mesh.usda", "target_prim": "/"},
        ],
        "prim_nodes": [
            {"id": "prim:/World/Mesh", "path": "/World/Mesh",
             "authored_in": ["mesh.usda"]},
        ],
        "variant_sets": [],
    })
}

/// A payload from **before v1.5**: `op: true` and nothing else on the op layer.
///
/// This is what a shipped v1.4 server sends, and the client must draw it as it always
/// did — same label, same click — rather than failing on the missing fields.
fn pre_v15_tree() -> Value {
    json!({
        "layers": [
            {"id": "anon:session", "kind": "session", "strength": 0},
            {"id": "city.rusd.cooked.usda", "kind": "root", "strength": 1},
            {"id": "rusd_city", "kind": "sublayer", "strength": 2, "op": true},
        ],
        "arcs": [],
        "prim_nodes": [],
        "variant_sets": [],
    })
}

/// The rows of a tree, flattened with their depth.
fn rows_of(tree: &Value) -> Vec<(usize, rusd_viewer::tree::TreeNode)> {
    let parsed = CompositionTree::parse(tree).expect("the payload parses");
    flatten(&rusd_viewer::tree::roots(&parsed))
}

/// The one op row an id names.
fn op_row(rows: &[(usize, rusd_viewer::tree::TreeNode)], id: &str) -> rusd_viewer::tree::TreeNode {
    rows.iter()
        .map(|(_, n)| n)
        .find(|n| n.op && n.id == id)
        .unwrap_or_else(|| panic!("no op row {id}: {:?}", rows.iter().map(|(_, n)| &n.id)))
        .clone()
}

// --- 1. the label: generator, input, output ----------------------------------

#[test]
fn an_op_rows_title_is_the_generator_and_its_subtitle_the_two_ends() {
    let rows = rows_of(&pipeline_tree());
    let sample = op_row(&rows, "op:sample_points/0");

    // The title carries the generator's schema name, and the v1.4 marker that says
    // this row previews rather than inspects.
    assert!(
        sample.label.starts_with("≤ RusdSamplePoints"),
        "the title must name the generator: {}",
        sample.label
    );
    assert!(
        !sample.label.contains("op:sample_points/0"),
        "the anonymous identifier is no longer the title: {}",
        sample.label
    );
    // …and the subtitle is `{input} → {output}`.
    assert_eq!(
        sample.secondary.as_deref(),
        Some("/World/Mesh → op:sample_points/0"),
        "the subtitle reads input -> output"
    );

    let instances = op_row(&rows, "op:instances/1");
    assert!(
        instances.label.starts_with("≤ RusdInstances"),
        "{}",
        instances.label
    );
    assert_eq!(
        instances.secondary.as_deref(),
        Some("/World/Mesh/points → op:instances/1"),
        "the second op reads the points the first wrote"
    );
}

#[test]
fn the_label_keeps_cook_order_and_touches_no_other_row() {
    let rows = rows_of(&pipeline_tree());
    let op_ids: Vec<&str> = rows
        .iter()
        .filter(|(_, n)| n.op)
        .map(|(_, n)| n.id.as_str())
        .collect();
    assert_eq!(
        op_ids,
        ["op:sample_points/0", "op:instances/1"],
        "rows are in the layer stack's order, which is the cook order"
    );

    // A structural row is unchanged: identifier, kind, rank, and no subtitle. Only op
    // rows gained anything, which is what keeps the panel's picture the same for
    // every other kind of node.
    let root = rows
        .iter()
        .map(|(_, n)| n)
        .find(|n| n.id == "mesh.usda")
        .expect("the root layer");
    assert_eq!(root.kind, TreeNodeKind::Layer);
    assert!(!root.op);
    assert_eq!(root.label, "mesh.usda [root · 1/4]");
    assert_eq!(root.secondary, None);
}

#[test]
fn an_output_identifier_longer_than_twenty_characters_is_elided() {
    // v1.5.1: output 摘要 = identifier 截断 ≤ 20 字符 + `…`. An op layer the cook named
    // verbosely must not stretch the row.
    let long = "a_very_long_cook_op_output_identifier";
    let tree = json!({
        "layers": [
            {"id": long, "kind": "sublayer", "strength": 0, "op": true,
             "op_name": "RusdScatter", "input": "/World/Points", "output": long},
        ],
        "arcs": [], "prim_nodes": [], "variant_sets": [],
    });
    let rows = rows_of(&tree);
    let row = op_row(&rows, long);
    let subtitle = row.secondary.clone().expect("an op row has a subtitle");
    let (input, output) = subtitle
        .split_once(" → ")
        .expect("the subtitle is `input -> output`");
    assert_eq!(input, "/World/Points");
    assert_eq!(
        output, "a_very_long_cook_op_…",
        "20 characters of the identifier, then the ellipsis"
    );
    assert_eq!(output.chars().count(), 21, "20 + the ellipsis");
    assert_eq!(elide(long), output);
    // The row's own id is still the whole identifier: the summary is the subtitle's,
    // the address is the row's.
    assert_eq!(row.id, long);
}

// --- 2. no input geometry reads as `(parametric)` ----------------------------

#[test]
fn an_op_with_no_input_reads_as_parametric() {
    // The parametric scatter's `input` is the contract's explicit null…
    let null_input = json!({
        "layers": [
            {"id": "rusd_scatter", "kind": "sublayer", "strength": 0, "op": true,
             "op_name": "RusdScatter", "input": null, "output": "rusd_scatter"},
        ],
        "arcs": [], "prim_nodes": [], "variant_sets": [],
    });
    let row = op_row(&rows_of(&null_input), "rusd_scatter");
    assert_eq!(
        row.secondary.as_deref(),
        Some("(parametric) → rusd_scatter"),
        "a null input is the contract's `(parametric)`"
    );

    // …and an omitted one is the same thing: v1.5.1's fields are additive, so absence
    // must not be an error and must not invent an input path.
    let omitted = json!({
        "layers": [
            {"id": "rusd_scatter", "kind": "sublayer", "strength": 0, "op": true,
             "op_name": "RusdScatter"},
        ],
        "arcs": [], "prim_nodes": [], "variant_sets": [],
    });
    let row = op_row(&rows_of(&omitted), "rusd_scatter");
    assert_eq!(
        row.secondary.as_deref(),
        Some("(parametric) → rusd_scatter")
    );
    assert!(row.label.starts_with("≤ RusdScatter"), "{}", row.label);
}

// --- 3. a pre-v1.5 server draws the old picture ------------------------------

#[test]
fn a_pre_v15_answer_keeps_the_anonymous_identifier_and_reports_no_error() {
    // Through `TreeModel`, the way the shell applies an answer: a payload without the
    // three new fields is a good answer, not a degraded one.
    let mut model = TreeModel::new();
    let layers = model
        .apply(Ok(pre_v15_tree()))
        .expect("an old payload still parses")
        .layers
        .len();
    assert!(!model.is_unavailable(), "no degraded sentence");
    assert_eq!(layers, 3);

    let row = op_row(&flatten(&model.roots()), "rusd_city");
    assert_eq!(
        row.label, "≤ rusd_city [sublayer · 2/3]",
        "the pre-v1.5 label, character for character"
    );
    // The ends still read, with the layer's own id standing in for the output the
    // server did not join — the contract's own fallback, not an invention.
    assert_eq!(
        row.secondary.as_deref(),
        Some("(parametric) → rusd_city"),
        "no input and no output field: the parameterised fallback and the id"
    );
    // And the row is still a preview control with the id the wire takes.
    assert_eq!(row.id, "rusd_city");
    assert!(row.op);
}

// --- 4. the click is unchanged ------------------------------------------------

#[test]
fn an_op_rows_click_still_sends_preview_upto_with_the_layer_id() {
    let mock = Mock::start();
    let mut client = connected(&mock);
    let rows = rows_of(&pipeline_tree());
    let sample = op_row(&rows, "op:sample_points/0");

    let action = panel::action_for(&sample);
    assert_eq!(
        action,
        TreeAction::Preview(Some("op:sample_points/0".to_owned())),
        "an op row previews up to itself"
    );

    // The wire carries the **layer identifier**, not the generator name the title now
    // shows and not the `output` field: `preview{upto}` is a layer id (v1.4, unchanged).
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
        json!({"upto": "op:sample_points/0"}),
        "the op layer's own id, never `RusdSamplePoints`"
    );
    assert!(mock.try_next().is_none(), "and no detail ask went with it");
}

#[test]
fn a_non_op_row_has_no_subtitle_and_still_asks_comp_node() {
    let mock = Mock::start();
    let mut client = connected(&mock);
    let rows = rows_of(&pipeline_tree());

    let arc = rows
        .iter()
        .map(|(_, n)| n)
        .find(|n| n.kind == TreeNodeKind::Arc)
        .expect("an arc row");
    assert_eq!(arc.secondary, None, "an arc row's text is unchanged");

    let action = panel::action_for(arc);
    let TreeAction::Select(sel) = action else {
        panic!("an arc row selects, not previews: {action:?}");
    };
    client
        .request("comp_node", comp_node_params(&sel.id))
        .expect("sent")
        .expect("answered");
    assert_eq!(mock.next_method("comp_node")["id"], json!(sel.id));
    assert!(mock.try_next().is_none(), "no preview was sent");
}

// --- 5. the label and the subtitle are actually painted ----------------------

/// Draw the tree panel headlessly and return every text the pass painted.
///
/// The production widget (`panel::tree_panel`), no GPU and no window: the point is
/// that a title built in the model but never drawn would not appear here.
fn painted_texts(tree: Value) -> Vec<String> {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1000.0, 700.0),
        )),
        ..Default::default()
    };
    let mut panel = TreePanel::new();
    panel.apply_tree(Ok(tree));

    let out = ctx.run_ui(raw, |root| {
        egui::CentralPanel::no_frame().show(root, |ui| {
            let _ = panel::tree_panel(ui, &panel);
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

#[test]
fn the_panel_paints_the_generator_title_and_the_ends_beside_it() {
    let texts = painted_texts(pipeline_tree());
    assert!(
        texts.iter().any(|t| t.contains("RusdSamplePoints")),
        "the generator's name must reach the screen: {texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|t| t.contains("/World/Mesh → op:sample_points/0")),
        "the subtitle must be drawn too: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("RusdInstances")),
        "and the second op's: {texts:?}"
    );
    // The panel is one row per op plus its subtitle: the ends are not folded into the
    // title, so the identifier stays readable next to the elided summary.
    assert!(
        !texts
            .iter()
            .any(|t| t.contains("RusdSamplePoints") && t.contains("→")),
        "the subtitle is a separate piece of text, not appended to the title: {texts:?}"
    );
}

#[test]
fn the_panel_paints_the_old_picture_for_a_pre_v15_answer() {
    // The same drain for a v1.4 server: the identifier is on screen and nothing
    // claims to be a generator.
    let texts = painted_texts(pre_v15_tree());
    assert!(
        texts.iter().any(|t| t.contains("rusd_city")),
        "the identifier is still the row's title: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("Rusd")),
        "no generator name is invented from a missing field: {texts:?}"
    );
}
