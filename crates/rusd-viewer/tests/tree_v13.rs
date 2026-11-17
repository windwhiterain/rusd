//! The v1.3 client contract (`docs/PROTOCOL.md` 末节): the composition tree draws
//! **structure only** — layers, arcs, variant sets — and a prim is inspected by
//! picking it in the viewport.
//!
//! The user's ruling was that a city stack's several hundred prim rows `既撑爆面板宽度
//! 又没人读`. Two things are pinned here, through the same functions the window uses:
//!
//! 1. **No prim rows.** A tree built from a real-shaped `composition_tree` payload
//!    (with `prim_nodes` still present — the payload is unchanged) yields rows of
//!    kinds layer/arc/variant_set and **not one** `Prim`. If a prim row comes back,
//!    the panel starts listing hundreds of paths again.
//! 2. **The pick chain.** A viewport pick gives a **path**; that path must resolve
//!    through the payload's `prim_nodes` to the **id** `comp_node` takes, and the
//!    detail that comes back must be the prim's. This runs over a real socket against
//!    a mock server, because the id lookup and the `comp_node` round trip are two
//!    halves that have to meet.
//!
//! The clipboard test is kept: a click on a prim still copies its path.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use rusd_viewer::net::ProtocolClient;
use rusd_viewer::protocol::PROTOCOL;
use rusd_viewer::tree::{
    CompositionTree, TreeModel, TreeNodeKind, clipboard_for, comp_node_params, flatten,
};
use serde_json::{Value, json};

/// A `composition_tree` payload shaped like the city's, with the prim nodes a pick
/// must resolve through.
fn payload() -> Value {
    json!({
        "layers": [
            {"id": "anon:session", "kind": "session", "strength": 0},
            {"id": "city.rusd.cooked.usda", "kind": "root", "strength": 1},
            {"id": "prototypes.usda", "kind": "reference", "strength": 2},
        ],
        "arcs": [
            {"id": "arc:0", "kind": "subLayer", "source_layer": "city.rusd.cooked.usda",
             "source_prim": "/", "target_layer": "city.rusd.cooked.usda", "target_prim": "/"},
            {"id": "arc:1", "kind": "reference", "source_layer": "city.rusd.cooked.usda",
             "source_prim": "/World/City/Block_01",
             "target_layer": "prototypes.usda", "target_prim": "/Prototypes/Block"},
        ],
        // The payload keeps its prim nodes (v1.3 does not change it); the panel just
        // stops drawing them.
        "prim_nodes": [
            {"id": "prim:/", "path": "/", "authored_in": ["city.rusd.cooked.usda"]},
            {"id": "prim:/World/City/Block_01", "path": "/World/City/Block_01",
             "authored_in": ["city.rusd.cooked.usda"]},
            {"id": "prim:/Prototypes/Block", "path": "/Prototypes/Block",
             "authored_in": ["prototypes.usda"]},
            {"id": "prim:/World/City/Block_01/over", "path": "/World/City/Block_01/over",
             "authored_in": ["anon:session", "city.rusd.cooked.usda"]},
        ],
        "variant_sets": [
            {"id": "vset:0", "prim": "/Prototypes/Block", "name": "size",
             "variants": ["small", "large"], "selection": "small"},
        ],
    })
}

/// A `TreeModel` loaded with `payload()`, built the way the shell builds it.
fn model() -> TreeModel {
    let mut m = TreeModel::new();
    m.apply(Ok(payload())).expect("the contract's shape");
    m
}

/// The rows the panel would draw, flattened with their depth.
fn rows(m: &TreeModel) -> Vec<(usize, rusd_viewer::tree::TreeNode)> {
    flatten(&m.roots())
}

// --- 1. the panel draws structure only ------------------------------------

#[test]
fn the_tree_draws_no_prim_rows() {
    let rows = rows(&model());
    assert!(!rows.is_empty(), "the tree has rows to draw");
    let prim_rows: Vec<&str> = rows
        .iter()
        .filter(|(_, n)| n.kind == TreeNodeKind::Prim)
        .map(|(_, n)| n.label.as_str())
        .collect();
    assert!(
        prim_rows.is_empty(),
        "v1.3: the tree must not list prims, but it drew {prim_rows:?}"
    );
}

#[test]
fn the_tree_draws_layers_arcs_and_variant_sets() {
    let rows = rows(&model());
    let kinds: Vec<TreeNodeKind> = rows.iter().map(|(_, n)| n.kind).collect();
    assert!(
        kinds.contains(&TreeNodeKind::Layer),
        "the layers are the tree's skeleton: {kinds:?}"
    );
    assert!(
        kinds.contains(&TreeNodeKind::Arc),
        "arcs are structure: {kinds:?}"
    );
    assert!(
        kinds.contains(&TreeNodeKind::VariantSet),
        "variant sets are structure too, and stay: {kinds:?}"
    );
    for (_, node) in &rows {
        assert!(
            matches!(
                node.kind,
                TreeNodeKind::Layer | TreeNodeKind::Arc | TreeNodeKind::VariantSet
            ),
            "row {} has kind {:?}, which v1.3 does not draw",
            node.label,
            node.kind
        );
    }
}

#[test]
fn the_variant_set_row_survives_even_though_its_prim_row_does_not() {
    // The prim `/Prototypes/Block` owns a variant set. Its path row is gone, but the
    // set is structure and must still be reachable — otherwise dropping prim rows
    // would silently drop variants too.
    let rows = rows(&model());
    let vsets: Vec<&str> = rows
        .iter()
        .filter(|(_, n)| n.kind == TreeNodeKind::VariantSet)
        .map(|(_, n)| n.label.as_str())
        .collect();
    assert!(
        vsets.iter().any(|l| l.contains("size")),
        "the `size` variant set must still be drawn: {vsets:?}"
    );
    assert!(
        vsets.iter().any(|l| *l == "small" || *l == "large"),
        "and its variants: {vsets:?}"
    );
}

#[test]
fn the_layer_stack_is_drawn_from_the_strongest() {
    let tree = CompositionTree::parse(&payload()).expect("parses");
    let rows = flatten(&model().roots());
    let layers: Vec<&str> = rows
        .iter()
        .filter(|(_, n)| n.kind == TreeNodeKind::Layer)
        .map(|(_, n)| n.id.as_str())
        .collect();
    assert!(
        layers.contains(&"anon:session"),
        "the session layer (strongest) must be in the tree: {layers:?}"
    );
    assert!(
        layers.contains(&"city.rusd.cooked.usda"),
        "the root layer must be in the tree: {layers:?}"
    );
    // The report still carries its prim nodes — the panel just does not draw them.
    assert_eq!(tree.prims.len(), 4);
}

// --- 2. pick → path → id → comp_node --------------------------------------

/// A mock server that answers the greeting and `comp_node`.
struct Mock {
    addr: String,
    seen: Receiver<Value>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Mock {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr").to_string();
        let (tx, rx) = channel::<Value>();
        let thread = thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            serve(stream, tx);
        });
        Self {
            addr,
            seen: rx,
            thread: Some(thread),
        }
    }

    fn next(&self) -> Value {
        self.seen
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the server received a request")
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.thread.take();
    }
}

fn serve(stream: TcpStream, seen: Sender<Value>) {
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    for _ in 0..2 {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line.trim_end()) else {
            return;
        };
        // The greeting is recorded like any other request, so a test can assert on
        // the order it saw rather than guessing which messages went by.
        let _ = seen.send(v.clone());
        let id = v["id"].as_u64().unwrap_or(0);
        let m = v["method"].as_str().unwrap_or("");
        let reply = match m {
            "hello" => {
                json!({"id": id, "result": {"protocol": PROTOCOL, "session": "mock", "version": 1}})
            }
            "subscribe" => json!({"id": id, "result": {"version": 2}}),
            _ => json!({"id": id, "error": {"code": -32601, "message": "unexpected"}}),
        };
        if writeln!(out, "{reply}").is_err() {
            return;
        }
        let _ = out.flush();
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line.trim_end()) else {
            return;
        };
        let id = v["id"].as_u64().unwrap_or(0);
        let _ = seen.send(v.clone());
        // Answer `comp_node` with a prim detail, echoing the id it was asked for so the
        // test can prove the id travelled intact.
        let asked = v["params"]["id"].as_str().unwrap_or("").to_owned();
        let detail = json!({
            "id": id,
            "result": {
                "kind": "prim",
                "path": "/World/City/Block_01",
                "opinions": [
                    {"layer": "anon:session", "specifier": "over",
                     "fields": [{"name": "xformOp:translate", "value": [15, 12, 7]}]},
                ],
                "arcs_out": ["arc:1"],
                "variant_sets": [],
                "asked_for": asked,
            }
        });
        if writeln!(out, "{detail}").is_err() {
            return;
        }
        let _ = out.flush();
    }
}

#[test]
fn a_picked_path_resolves_to_the_comp_node_id_the_server_answers_for() {
    let m = model();
    let tree = m.tree().expect("a ready tree");

    // The viewport picked this prim: a path, and nothing else.
    let picked = "/World/City/Block_01";
    // The tree turns it into the id `comp_node` takes — the v1.3 entry point.
    let id = tree
        .prim_id(picked)
        .expect("the payload lists the prim the renderer drew")
        .to_owned();
    assert_eq!(id, "prim:/World/City/Block_01");

    let mock = Mock::start();
    let (mut client, _early) = ProtocolClient::open(&mock.addr).expect("greeting");
    assert_eq!(mock.next()["method"], "hello");
    assert_eq!(mock.next()["method"], "subscribe");

    let reply = client
        .request("comp_node", comp_node_params(&id))
        .expect("comp_node answered");
    let asked = mock.next();
    assert_eq!(asked["method"], "comp_node");
    assert_eq!(
        asked["params"]["id"], "prim:/World/City/Block_01",
        "the id the pick resolved to is the id that was asked for"
    );
    let detail = reply.expect("a result");
    assert_eq!(detail["kind"], "prim");
    assert_eq!(detail["path"], "/World/City/Block_01");
    assert_eq!(detail["asked_for"], "prim:/World/City/Block_01");
    // The first opinion is the session layer's — the strongest, as the contract says.
    assert_eq!(detail["opinions"][0]["layer"], "anon:session");
}

#[test]
fn a_picked_path_the_report_does_not_list_has_no_id() {
    // Ordinary, not an error: a flattened cook output has prims no layer spec
    // authored. The app says so in the detail panel instead of asking the server for
    // an id it cannot have.
    let m = model();
    let tree = m.tree().expect("a ready tree");
    assert_eq!(tree.prim_id("/World/City/NotInTheReport"), None);
}

#[test]
fn a_tree_that_has_not_arrived_answers_no_id_at_all() {
    // The other half of the "no id" distinction: not ready yet is a different answer
    // from "ready, but this prim is not in it".
    let m = TreeModel::new();
    assert!(m.prim_id("/World/City/Block_01").is_none());
    assert!(!m.ready(), "nothing has arrived");
    assert!(!m.is_unavailable(), "and nothing has failed");
}

// --- the clipboard rule that must survive --------------------------------

#[test]
fn a_prim_selection_still_yields_its_path_for_the_clipboard() {
    // v1.1's clipboard rule is untouched: picking a prim copies its path. The row is
    // gone from the tree, so this comes from the viewport pick now.
    let sel = rusd_viewer::tree::TreeSelection {
        id: "prim:/World/City/Block_01".to_owned(),
        kind: TreeNodeKind::Prim,
        prim: Some("/World/City/Block_01".to_owned()),
    };
    assert_eq!(
        clipboard_for(&sel).as_deref(),
        Some("/World/City/Block_01"),
        "a picked prim copies its path"
    );
}

#[test]
fn a_layer_row_copies_nothing_to_the_clipboard() {
    let rows = rows(&model());
    let layer = rows
        .iter()
        .find(|(_, n)| n.kind == TreeNodeKind::Layer)
        .expect("a layer row");
    let sel = rusd_viewer::tree::TreeSelection::of(&layer.1);
    assert_eq!(sel.prim, None, "a layer row is not a prim path");
    assert_eq!(clipboard_for(&sel), None);
}
