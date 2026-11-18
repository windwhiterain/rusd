//! Integration tests for the cook metadata on `composition_tree`'s op rows
//! (`docs/PROTOCOL.md` v1.5.1): `op_name` / `input` / `output`, joined against
//! the latest `CookResult.ops`.
//!
//! Real TCP against a server thread on an ephemeral port, because the tree's
//! `layers` rows are the wire surface the viewer's op lines are drawn from. The
//! join is checked against **real cooks** — the `city` stack (two parametric
//! generators, so `input` is `null`) and `points_scatter` (a `RusdScatter` that
//! reads `input_points`, so `input` names a prim) — and against the rebuild a
//! `patch` forces, which mints new output layer identifiers.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};

use rusd_agent::transport::Server;
use serde_json::{Value, json};

const CITY: &str = include_str!("../../../examples/city.rusd");
const POINTS: &str = include_str!("../../../examples/points_scatter.rusd");

// --- a client --------------------------------------------------------------

/// A TCP client that keeps every line it has read, so a response and the
/// notifications around it stay in the order the server wrote them.
struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    next_id: u64,
    inbox: VecDeque<Value>,
}

impl Client {
    fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).expect("connect to the test server");
        Client {
            reader: BufReader::new(stream.try_clone().expect("clone the socket")),
            writer: stream,
            next_id: 1,
            inbox: VecDeque::new(),
        }
    }

    fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.writer, "{request}").expect("write the request");
        self.writer.flush().expect("flush the request");
        id
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).expect("read a line");
        assert!(n > 0, "the server closed the connection");
        serde_json::from_str(&line).expect("the server writes JSON")
    }

    /// The response to request `id`. A notification that arrives first is kept
    /// in the inbox, the way a real client queues what it has not consumed yet;
    /// leaving it in the socket would make the next `call` read someone else's
    /// line first.
    fn result(&mut self, id: u64) -> Value {
        let msg = self.response(id);
        if msg.get("result").is_some() {
            return msg["result"].clone();
        }
        panic!("request {id} failed: {}", msg["error"]);
    }

    /// The raw response object for request `id`, out of the inbox when it is
    /// already there:
    fn response(&mut self, id: u64) -> Value {
        if let Some(index) = self.inbox.iter().position(|m| m["id"].as_u64() == Some(id)) {
            return self.inbox.remove(index).expect("the index just found");
        }
        loop {
            let msg = self.read_line();
            if msg["id"].as_u64() == Some(id) {
                return msg;
            }
            self.inbox.push_back(msg);
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        self.result(id)
    }

    /// Read the notifications a completed mutation published, up to and
    /// including its `state_changed`.
    fn published(&mut self) {
        loop {
            let msg = match self.inbox.pop_front() {
                Some(m) => m,
                None => self.read_line(),
            };
            if msg["method"].as_str() == Some("state_changed") {
                return;
            }
        }
    }
}

// --- fixtures --------------------------------------------------------------

/// A private capture directory, removed when the test ends: the server takes one
/// at bind time whether or not a capture is ever made.
struct Captures {
    dir: PathBuf,
}

impl Captures {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("rusd_opmeta_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the capture dir");
        Captures { dir }
    }

    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Captures {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A server with `document` open and one subscribed client, past the
/// notifications `open` published.
fn opened(tag: &str, document: &str) -> (Captures, Server, Client) {
    let captures = Captures::new(tag);
    let server = Server::bind_with_captures("127.0.0.1:0", captures.path().to_path_buf())
        .expect("bind an ephemeral port");
    let mut client = Client::connect(server.local_addr());
    client.call("hello", json!({ "client": "test", "protocol": 1 }));
    client.call("subscribe", json!({ "client": "test" }));
    client.call("open", json!({ "content": document }));
    client.published();
    (captures, server, client)
}

/// The `layers` rows that carry `op: true`, in **cook order** — the order
/// `preview{upto}` counts in. The tree lists layers strongest first and the cook
/// inserts each output layer at the top of the root's stack, so cook order is
/// the reverse of the row order.
fn op_rows(tree: &Value) -> Vec<Value> {
    let mut rows: Vec<Value> = tree["layers"]
        .as_array()
        .expect("layers is an array")
        .iter()
        .filter(|l| l["op"].as_bool() == Some(true))
        .cloned()
        .collect();
    rows.reverse();
    rows
}

/// The `layers` rows that are not cook output layers.
fn other_rows(tree: &Value) -> Vec<Value> {
    tree["layers"]
        .as_array()
        .expect("layers is an array")
        .iter()
        .filter(|l| l["op"].as_bool() != Some(true))
        .cloned()
        .collect()
}

// --- the join --------------------------------------------------------------

#[test]
fn the_city_op_rows_name_their_generator_in_cook_order() {
    let (_captures, _server, mut client) = opened("city", CITY);
    let tree = client.call("composition_tree", json!({}));

    let ops = op_rows(&tree);
    assert_eq!(
        ops.len(),
        2,
        "the city cooks two generators, so two op rows: {tree}"
    );
    // Cook order: the street grid first, the scatter second.
    assert_eq!(
        ops[0]["op_name"].as_str(),
        Some("RusdStreetGrid"),
        "{ops:?}"
    );
    assert_eq!(ops[1]["op_name"].as_str(), Some("RusdScatter"), "{ops:?}");
    // Neither reads `input_points`: both are parametric, and the wire says so
    // with a `null` rather than by leaving the field out.
    for row in &ops {
        assert_eq!(row["input"], Value::Null, "parametric generator: {row}");
    }
    // `output` names the layer the generator wrote now, which is the row's own
    // id: the join key is `CookOp.layer_id == id`.
    for row in &ops {
        let id = row["id"].as_str().expect("an op row has an id");
        assert_eq!(row["output"].as_str(), Some(id), "{row}");
        assert!(
            id.contains("anon:"),
            "a cook output layer's identifier is anonymous: {row}"
        );
    }
    assert!(
        ops[0]["output"]
            .as_str()
            .is_some_and(|id| id.contains("rusd_city")),
        "the first op's output layer is the street grid's: {ops:?}"
    );
    assert!(
        ops[1]["output"]
            .as_str()
            .is_some_and(|id| id.contains("rusd_scatter")),
        "the second op's output layer is the scatter's: {ops:?}"
    );

    // The rows are not the `layers` array's order: the tree is strength order,
    // where the newest cook output layer is strongest.
    let ids: Vec<&str> = tree["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_str().unwrap_or_default())
        .collect();
    let first_op_in_row_order = ops[1]["id"].as_str().unwrap();
    let second_op_in_row_order = ops[0]["id"].as_str().unwrap();
    let a = ids
        .iter()
        .position(|id| *id == first_op_in_row_order)
        .unwrap();
    let b = ids
        .iter()
        .position(|id| *id == second_op_in_row_order)
        .unwrap();
    assert!(a < b, "the cook's second layer is listed first: {ids:?}");
}

#[test]
fn an_op_that_reads_geometry_names_the_prim_it_read() {
    let (_captures, _server, mut client) = opened("points", POINTS);
    let tree = client.call("composition_tree", json!({}));

    let ops = op_rows(&tree);
    assert_eq!(
        ops.len(),
        1,
        "points_scatter has one generator node: {tree}"
    );
    let row = &ops[0];
    assert_eq!(row["op_name"].as_str(), Some("RusdScatter"), "{row}");
    assert_eq!(
        row["input"].as_str(),
        Some("/World/Dots"),
        "the scatter's `input_points` reaches the wire: {row}"
    );
    assert_eq!(row["output"].as_str(), row["id"].as_str(), "{row}");
}

#[test]
fn a_non_op_row_carries_no_cook_metadata() {
    let (_captures, _server, mut client) = opened("nonop", CITY);
    let tree = client.call("composition_tree", json!({}));

    let others = other_rows(&tree);
    assert!(
        !others.is_empty(),
        "the city stack has a session and a root layer: {tree}"
    );
    for row in &others {
        // PROTOCOL v1.5.1: the three fields are on the op rows only. Their
        // absence — not a `null` — is what tells an older client's shape apart
        // from a parametric generator's.
        assert!(
            row.get("op_name").is_none(),
            "a non-op row has no generator: {row}"
        );
        assert!(
            row.get("input").is_none(),
            "a non-op row has no input: {row}"
        );
        assert!(
            row.get("output").is_none(),
            "a non-op row has no output: {row}"
        );
    }
}

// --- the join follows the cook across a rebuild ----------------------------

#[test]
fn a_patch_rebuild_relabels_the_new_output_layers() {
    let (_captures, _server, mut client) = opened("patch", CITY);
    let before = op_rows(&client.call("composition_tree", json!({})));

    client.call(
        "patch",
        json!({"ops": [{"op": "set", "prim": "/World/City", "property": "blocks_x", "value": 3}]}),
    );
    client.published();
    let after = op_rows(&client.call("composition_tree", json!({})));

    assert_eq!(
        after.len(),
        before.len(),
        "the same two generators cook: {before:?} then {after:?}"
    );
    for (old, new) in before.iter().zip(after.iter()) {
        // A rebuild mints fresh anonymous layers, so the joined identifier is
        // the new one — not the one the previous pass reported.
        assert_ne!(
            old["id"].as_str(),
            new["id"].as_str(),
            "a rebuild gives each layer a new identifier: {old} vs {new}"
        );
        // The generator and its input are the node's, not the layer's: they
        // survive the rebuild, and the join still lands on the new layer.
        assert_eq!(old["op_name"], new["op_name"], "{old} vs {new}");
        assert_eq!(old["input"], new["input"], "{old} vs {new}");
        assert_eq!(
            new["output"], new["id"],
            "the output field names the layer the new pass wrote: {new}"
        );
    }
}

#[test]
fn an_opinion_recook_keeps_the_op_rows_joined() {
    let (_captures, _server, mut client) = opened("opinion", CITY);
    let before = op_rows(&client.call("composition_tree", json!({})));

    // An opinion edits the session layer in place: the cook's memo hits, so the
    // output layers are reused and the op rows are the ones from the same pass.
    client.call(
        "opinion",
        json!({"ops": [{"path": "/World", "field": "xformOp:translate", "value": [1.0, 2.0, 0.0], "op": "set"}]}),
    );
    client.published();
    let after = op_rows(&client.call("composition_tree", json!({})));

    assert_eq!(
        after.len(),
        before.len(),
        "a re-cook lists every node with an output layer: {after:?}"
    );
    for (old, new) in before.iter().zip(after.iter()) {
        assert_eq!(
            old["output"], new["output"],
            "a memo hit keeps its output layer, so the join does not move: {old} vs {new}"
        );
        assert_eq!(old["op_name"], new["op_name"], "{old} vs {new}");
    }
    assert_eq!(
        after[0]["op_name"].as_str(),
        Some("RusdStreetGrid"),
        "every op still reports its generator: {after:?}"
    );
}

#[test]
fn a_preview_does_not_change_the_op_rows() {
    let (_captures, _server, mut client) = opened("preview", CITY);
    let before = op_rows(&client.call("composition_tree", json!({})));

    // A prefix changes which layers one connection's snapshots stop at; it is
    // not a cook, so the metadata the whole cook reported must not move.
    client.call("preview", json!({ "upto": before[0]["id"].clone() }));
    let after = op_rows(&client.call("composition_tree", json!({})));

    assert_eq!(after, before, "a preview re-cooks nothing");
}
