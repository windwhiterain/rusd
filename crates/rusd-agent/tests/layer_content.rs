//! Integration tests for `layer{id}` (docs/PROTOCOL.md v1.5.5): a clicked layer
//! row shows what *that layer* authors, which is the one thing the composition
//! tree cannot answer — `composition_tree` says which layers exist and how they
//! are joined, `comp_node` says what the composition makes of one prim, and
//! neither says "what did this layer write".
//!
//! Real TCP against a server thread on an ephemeral port. The stage is
//! `examples/mesh_scatter.rusd`, the three-layer pipeline (mesh → points →
//! instances), because that is the stack whose layers have visibly different
//! content: the root layer authors the input mesh and the prototype, the sample
//! op's own layer authors the cloud, and the instances op's layer authors two
//! hundred and forty specs each referencing the prototype.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};

use rusd_agent::transport::Server;
use serde_json::{Value, json};

const MESH: &str = include_str!("../../../examples/mesh_scatter.rusd");

// --- a client --------------------------------------------------------------

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

    fn result(&mut self, id: u64) -> Value {
        loop {
            let msg = match self.inbox.pop_front() {
                Some(m) => m,
                None => self.read_line(),
            };
            if msg["id"].as_u64() == Some(id) {
                if msg.get("result").is_some() {
                    return msg["result"].clone();
                }
                panic!("request {id} failed: {}", msg["error"]);
            }
            self.inbox.push_back(msg);
        }
    }

    /// The error a request failed with, refusing a success.
    fn error(&mut self, id: u64) -> Value {
        loop {
            let msg = match self.inbox.pop_front() {
                Some(m) => m,
                None => self.read_line(),
            };
            if msg["id"].as_u64() == Some(id) {
                assert!(msg.get("error").is_some(), "expected an error, got {msg}");
                return msg["error"].clone();
            }
            self.inbox.push_back(msg);
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        self.result(id)
    }

    /// The notifications a completed mutation published, up to and including its
    /// `state_changed` (a blocking read: the response came first, the rest is on
    /// its way).
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

/// A server with `mesh_scatter` open, past the notifications `open` published.
fn opened() -> (Server, Client) {
    let server = Server::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let mut client = Client::connect(server.local_addr());
    client.call("hello", json!({ "client": "test", "protocol": 1 }));
    // A subscriber, so a request that wrongly published would show up as a
    // notification the reader finds instead of the next response.
    client.call("subscribe", json!({ "client": "test" }));
    client.call("open", json!({ "content": MESH }));
    client.published();
    (server, client)
}

/// The tree's `layers` rows, strongest first.
fn tree_rows(tree: &Value) -> Vec<Value> {
    tree["layers"]
        .as_array()
        .expect("layers is an array")
        .clone()
}

/// The id of the layer row of `kind` (or, for an op layer, the one `op` flagged
/// and whose `op_name` is `generator`).
fn layer_id(tree: &Value, kind: &str, generator: Option<&str>) -> String {
    tree_rows(tree)
        .into_iter()
        .find(|row| {
            row["kind"].as_str() == Some(kind)
                && match generator {
                    None => true,
                    Some(name) => row["op_name"].as_str() == Some(name),
                }
        })
        .and_then(|row| row["id"].as_str().map(str::to_owned))
        .unwrap_or_else(|| panic!("no {kind} layer in the tree: {tree}"))
}

/// The row of `path` in a `layer` answer.
fn spec_row<'a>(layer: &'a Value, path: &str) -> &'a Value {
    layer["specs"]
        .as_array()
        .expect("specs is an array")
        .iter()
        .find(|spec| spec["path"].as_str() == Some(path))
        .unwrap_or_else(|| panic!("{path} is not a spec of the layer: {layer}"))
}

fn properties(row: &Value) -> Vec<String> {
    row["properties"]
        .as_array()
        .expect("properties is an array")
        .iter()
        .map(|p| p.as_str().unwrap_or_default().to_owned())
        .collect()
}

// --- the layer's own content ----------------------------------------------

#[test]
fn the_root_layer_reports_what_the_document_authored() {
    let (_server, mut client) = opened();
    let tree = client.call("composition_tree", json!({}));
    let root = layer_id(&tree, "root", None);

    let layer = client.call("layer", json!({ "id": root }));
    assert_eq!(layer["id"].as_str(), Some(root.as_str()));
    assert_eq!(
        layer["kind"].as_str(),
        Some("root"),
        "the kind is the tree's: {layer}"
    );

    // The input mesh, as the document wrote it: a `def Mesh` with its `points`
    // (and the topology that makes it a surface rather than a cloud).
    let terrain = spec_row(&layer, "/World/Terrain");
    assert_eq!(terrain["spec_type"].as_str(), Some("def"));
    assert_eq!(terrain["type_name"].as_str(), Some("Mesh"));
    let names = properties(terrain);
    for expected in ["points", "faceVertexCounts", "faceVertexIndices"] {
        assert!(
            names.iter().any(|n| n == expected),
            "the mesh's `{expected}` is one of the properties it authors: {names:?}"
        );
    }
    // The schema a document names is the prim's `typeName`, not one of its
    // properties: the layer stores it as a field of the spec.
    assert!(
        !names.iter().any(|n| n == "schema"),
        "the schema is the type, not a property: {names:?}"
    );
    assert!(
        terrain["references"].as_array().is_some_and(Vec::is_empty),
        "the mesh references nothing: {terrain}"
    );

    // The prototype: the prim the instances reference, so the layer that holds
    // it must say so — that is what makes the instances' rows readable.
    let tuft = spec_row(&layer, "/World/Prototypes/Tuft");
    assert_eq!(tuft["spec_type"].as_str(), Some("def"));
    assert_eq!(tuft["type_name"].as_str(), Some("Sphere"));
    assert!(
        properties(tuft).iter().any(|n| n == "radius"),
        "the prototype authors its radius: {tuft}"
    );

    // The generator nodes are in the root layer too — the document declares
    // them; what they *produce* is in the op layers.
    let sample_node = spec_row(&layer, "/World/TuftSamples");
    assert_eq!(sample_node["type_name"].as_str(), Some("RusdSamplePoints"));
    assert!(
        properties(sample_node).iter().any(|n| n == "count"),
        "the node's parameters are its properties: {sample_node}"
    );
    // And no prim the cook wrote is in the root layer: a layer reports itself.
    assert!(
        layer["specs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|spec| spec["path"].as_str() != Some("/World/Tufts/i000000")),
        "a cook instance is not authored by the root layer: {layer}"
    );
}

#[test]
fn the_sample_layers_own_prims_are_the_cloud() {
    let (_server, mut client) = opened();
    let tree = client.call("composition_tree", json!({}));
    let sample = layer_id(&tree, "sublayer", Some("RusdSamplePoints"));
    assert!(
        sample.contains("rusd_sample_points"),
        "the sample op's layer is the one its generator wrote: {sample}"
    );

    let layer = client.call("layer", json!({ "id": sample }));
    let cloud = spec_row(&layer, "/World/TuftSamples/points");
    assert_eq!(cloud["spec_type"].as_str(), Some("def"));
    assert_eq!(
        cloud["type_name"].as_str(),
        Some("Points"),
        "the middle stage of the pipeline is a points prim: {layer}"
    );
    assert!(
        properties(cloud).iter().any(|n| n == "points"),
        "the cloud authors its `points`: {cloud}"
    );
    // The replacement semantics, read off the layer: the op hides the mesh it
    // consumed, in its own layer, without touching the document's spec.
    let hidden = spec_row(&layer, "/World/Terrain");
    assert_eq!(hidden["spec_type"].as_str(), Some("over"));
    assert!(
        properties(hidden).iter().any(|n| n == "visibility"),
        "the op's hide is an over with a visibility: {hidden}"
    );
    assert_eq!(
        hidden["type_name"],
        Value::Null,
        "an over declares no type of its own: {hidden}"
    );
}

#[test]
fn the_instances_layer_reports_every_instance_and_its_reference() {
    let (_server, mut client) = opened();
    let tree = client.call("composition_tree", json!({}));
    let instances = layer_id(&tree, "sublayer", Some("RusdInstances"));

    let root = layer_id(&tree, "root", None);
    let layer = client.call("layer", json!({ "id": instances }));
    let specs = layer["specs"].as_array().expect("specs is an array");
    let instance_rows: Vec<&Value> = specs
        .iter()
        .filter(|spec| {
            spec["path"]
                .as_str()
                .is_some_and(|p| p.starts_with("/World/Tufts/i"))
        })
        .collect();
    assert_eq!(
        instance_rows.len(),
        240,
        "the two-op pipeline writes one instance per sample (240): {} rows of {specs:?}",
        instance_rows.len()
    );
    // Every instance row says what it references — the prototype — and the
    // layer it lives in, resolved from the asset path the reference authored.
    for row in &instance_rows {
        let refs = row["references"]
            .as_array()
            .expect("references is an array");
        assert_eq!(refs.len(), 1, "one reference per instance: {row}");
        assert_eq!(
            refs[0]["prim"].as_str(),
            Some("/World/Prototypes/Tuft"),
            "{row}"
        );
        // The instance references the prototype **by asset path**, and the
        // answer resolves it to the layer that path opened — the root layer of
        // this stack — so a client can follow the edge without parsing paths.
        assert_eq!(
            refs[0]["layer"].as_str(),
            Some(root.as_str()),
            "the asset path resolves to the opened root layer: {row}"
        );
        // The instance's own placement is a property of its spec, and its spec
        // is a `def` — the transform is what the row is for.
        let names = properties(row);
        assert!(
            names.iter().any(|n| n == "xformOp:translate"),
            "an instance authors its translate: {row}"
        );
        assert!(
            names.iter().any(|n| n == "xformOpOrder"),
            "and the order its ops apply in: {row}"
        );
    }
    // No property spec is a row of its own: the layer's list is its prim specs,
    // not one line per attribute (240 instances, not 240 x 4).
    assert!(
        specs
            .iter()
            .all(|spec| !spec["path"].as_str().unwrap_or_default().contains('.')),
        "a property spec is folded into its prim's row: {specs:?}"
    );
    // The layer does name the sample cloud, but as the input it hides — an `over`
    // with a visibility and nothing else, never the cloud's `points`. That is the
    // replacement semantics read off the layer itself.
    let cloud = spec_row(&layer, "/World/TuftSamples/points");
    assert_eq!(cloud["spec_type"].as_str(), Some("over"));
    assert_eq!(
        properties(cloud),
        vec!["visibility".to_owned()],
        "the instances layer hides the cloud and says nothing else about it: {cloud}"
    );
}

#[test]
fn an_unknown_layer_id_is_invalid_params_naming_the_open_layers() {
    let (_server, mut client) = opened();
    let tree = client.call("composition_tree", json!({}));
    let ids: Vec<String> = tree_rows(&tree)
        .iter()
        .map(|row| row["id"].as_str().unwrap_or_default().to_owned())
        .collect();

    let id = client.send("layer", json!({ "id": "no_such_layer.usda" }));
    let error = client.error(id);
    assert_eq!(error["code"].as_i64(), Some(-32602));
    let message = error["message"].as_str().expect("a message");
    for want in &ids {
        assert!(
            message.contains(want.as_str()),
            "the refusal lists the open layers, {want} missing from {message:?}"
        );
    }

    // A missing id is the same refusal, not a different one.
    let id = client.send("layer", json!({}));
    assert_eq!(client.error(id)["code"].as_i64(), Some(-32602));
}

#[test]
fn a_layer_read_moves_no_version_and_publishes_nothing() {
    let (_server, mut client) = opened();
    let tree = client.call("composition_tree", json!({}));
    let root = layer_id(&tree, "root", None);
    let before = client.call("hello", json!({ "protocol": 1 }));

    client.call("layer", json!({ "id": root }));

    // Nothing followed the response: the next line the client reads is the one a
    // later request causes, not a notification.
    let after = client.call("hello", json!({ "protocol": 1 }));
    assert_eq!(
        after["version"].as_u64(),
        before["version"].as_u64(),
        "reading a layer is not a mutation"
    );
}

#[test]
fn one_layer_answers_the_same_bytes_twice() {
    let (_server, mut client) = opened();
    let tree = client.call("composition_tree", json!({}));
    for row in tree_rows(&tree) {
        let id = row["id"].as_str().expect("a layer id");
        let first = client.call("layer", json!({ "id": id }));
        let second = client.call("layer", json!({ "id": id }));
        assert_eq!(
            first.to_string(),
            second.to_string(),
            "the same layer answers the same list, byte for byte: {id}"
        );
        assert_eq!(
            first["id"].as_str(),
            Some(id),
            "the answer names the layer it describes"
        );
    }
}
