//! Integration tests for the D13 server methods (`docs/PROTOCOL.md` v1.1 and
//! v1.2): visual-reference captures on the wire, and the composition tree as a
//! client sees it.
//!
//! Real TCP against a server thread on an ephemeral port — these methods are the
//! wire surface, so the wire is what the tests speak. Each test names its own
//! capture directory, so no test reads another's captures.
//!
//! The arcs a reference or a variant set makes are exercised in
//! [`rusd_agent`'s own unit tests](../src/composition.rs), where a stage can be
//! authored directly: the `.rusd` document the server opens has no reference
//! syntax (SPEC §4's v0 authoring subset is sublayers, which the cook writes),
//! so a wire test cannot reach one. Here the city's stack — sublayers from the
//! cook — and the session layer are what the tree is checked against.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Instant;

use rusd_agent::transport::{Server, base64_decode, base64_encode};
use serde_json::{Value, json};

const CITY: &str = include_str!("../../../examples/city.rusd");

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

    /// Read the notifications a completed mutation published, up to and
    /// including its `state_changed`. A blocking read, because the server writes
    /// the response first and the consequences after it: once the response is
    /// in, the rest is on its way, and stopping early would leave half a
    /// snapshot line in the reader.
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

/// A private capture directory, removed when the test ends.
struct Captures {
    dir: PathBuf,
}

impl Captures {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("rusd_captures_{}_{tag}", std::process::id()));
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

/// A PNG with the given size and a body byte from `tag`, so two images of one
/// size still differ in bytes. Not a decodable image — nothing in this server
/// decodes one; the size lives in the header.
fn png(width: u32, height: u32, tag: u8) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder
    bytes.extend_from_slice(&[tag; 32]);
    bytes
}

fn camera() -> Value {
    json!({
        "position": [10.0, 20.0, 30.0],
        "look_at": [0.0, 0.0, 0.0],
        "up": [0.0, 0.0, 1.0],
        "fov_deg": 45.0,
    })
}

fn server_with(captures: &Captures) -> (Server, Client) {
    let server = Server::bind_with_captures("127.0.0.1:0", captures.path().to_path_buf())
        .expect("bind an ephemeral port");
    let mut client = Client::connect(server.local_addr());
    client.call("hello", json!({ "client": "test", "protocol": 1 }));
    // Every test here is a viewer: subscribed before anything is opened, so the
    // notifications a mutation publishes have somewhere to go. With no stage
    // open, `subscribe` pushes only its `{version}`.
    client.call("subscribe", json!({ "client": "test" }));
    (server, client)
}

/// A server with the city open, past the notifications `open` published.
fn opened_case(tag: &str) -> (Captures, Server, Client) {
    let captures = Captures::new(tag);
    let (server, mut client) = server_with(&captures);
    client.call("open", json!({ "content": CITY }));
    client.published();
    (captures, server, client)
}

// --- captures --------------------------------------------------------------

#[test]
fn a_capture_round_trips_its_bytes() {
    let captures = Captures::new("roundtrip");
    let (_server, mut client) = server_with(&captures);

    let image = png(320, 200, 0xA1);
    let mask = png(320, 200, 0x5C);
    let taken = client.call(
        "capture",
        json!({
            "camera": camera(),
            "image": base64_encode(&image),
            "mask": base64_encode(&mask),
            "scene_version": 7,
            "note": "the corner lot",
        }),
    );
    let id = taken["id"]
        .as_str()
        .expect("capture answers an id")
        .to_owned();
    assert_eq!(id, "cap-0001", "ids start at one");

    let back = client.call("capture_get", json!({ "id": id }));
    assert_eq!(back["id"].as_str(), Some(id.as_str()));
    assert_eq!(
        base64_decode(back["image"].as_str().expect("an image")).expect("image is base64"),
        image,
        "the image comes back byte for byte"
    );
    assert_eq!(
        base64_decode(back["mask"].as_str().expect("a mask")).expect("mask is base64"),
        mask,
        "the mask comes back byte for byte"
    );
    assert_eq!(back["camera"], camera(), "the camera is stored as sent");
    assert_eq!(back["scene_version"].as_u64(), Some(7));
    assert_eq!(back["note"].as_str(), Some("the corner lot"));
    assert!(
        back["created_at"].as_u64().unwrap_or(0) > 0,
        "a capture records when it was taken: {back}"
    );

    // The three files are where PROTOCOL says they are.
    for name in [
        format!("{id}.png"),
        format!("{id}.mask.png"),
        format!("{id}.meta.json"),
    ] {
        let path = captures.path().join(&name);
        assert!(path.is_file(), "{} was not written", path.display());
    }
}

#[test]
fn a_capture_without_a_mask_reports_none() {
    let captures = Captures::new("nomask");
    let (_server, mut client) = server_with(&captures);
    let taken = client.call(
        "capture",
        json!({ "camera": camera(), "image": base64_encode(&png(8, 8, 1)) }),
    );
    let id = taken["id"].as_str().unwrap().to_owned();
    let back = client.call("capture_get", json!({ "id": id }));
    assert!(back["mask"].is_null(), "no mask, no mask bytes: {back}");
    assert!(back["note"].is_null());
    assert!(
        !captures.path().join(format!("{id}.mask.png")).exists(),
        "a maskless capture writes no mask file"
    );
}

#[test]
fn a_mask_of_another_size_is_invalid_params() {
    let captures = Captures::new("masksize");
    let (_server, mut client) = server_with(&captures);

    let id = client.send(
        "capture",
        json!({
            "camera": camera(),
            "image": base64_encode(&png(64, 64, 1)),
            "mask": base64_encode(&png(32, 64, 2)),
        }),
    );
    let error = client.error(id);
    assert_eq!(
        error["code"].as_i64(),
        Some(-32602),
        "not invalid params: {error}"
    );
    let message = error["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("32x64") && message.contains("64x64"),
        "the refusal names both sizes: {message}"
    );
    assert!(
        std::fs::read_dir(captures.path())
            .expect("the dir is readable")
            .next()
            .is_none(),
        "a refused capture writes nothing"
    );
}

#[test]
fn a_non_png_image_is_invalid_params() {
    let captures = Captures::new("notpng");
    let (_server, mut client) = server_with(&captures);
    let id = client.send(
        "capture",
        json!({ "camera": camera(), "image": base64_encode(b"this is not a png") }),
    );
    let error = client.error(id);
    assert_eq!(error["code"].as_i64(), Some(-32602), "{error}");
    assert!(
        error["message"]
            .as_str()
            .unwrap_or_default()
            .contains("PNG"),
        "{error}"
    );
}

#[test]
fn an_unknown_capture_id_is_invalid_params() {
    let captures = Captures::new("unknown");
    let (_server, mut client) = server_with(&captures);

    let id = client.send("capture_get", json!({ "id": "cap-9999" }));
    let error = client.error(id);
    assert_eq!(
        error["code"].as_i64(),
        Some(-32602),
        "not invalid params: {error}"
    );
    assert!(
        error["message"]
            .as_str()
            .unwrap_or_default()
            .contains("cap-9999"),
        "the refusal names the id: {error}"
    );

    // A missing `id` is the same refusal, not a server error.
    let id = client.send("capture_get", json!({}));
    assert_eq!(client.error(id)["code"].as_i64(), Some(-32602));
}

#[test]
fn capture_list_reports_one_row_per_capture() {
    let captures = Captures::new("list");
    let (_server, mut client) = server_with(&captures);

    let first = client.call(
        "capture",
        json!({
            "camera": camera(),
            "image": base64_encode(&png(8, 8, 1)),
            "scene_version": 3,
        }),
    );
    let second = client.call(
        "capture",
        json!({
            "camera": camera(),
            "image": base64_encode(&png(8, 8, 2)),
            "mask": base64_encode(&png(8, 8, 3)),
            "scene_version": 4,
            "note": "with a mask",
        }),
    );
    assert_eq!(first["id"].as_str(), Some("cap-0001"));
    assert_eq!(second["id"].as_str(), Some("cap-0002"));

    let list = client.call("capture_list", json!({}));
    let rows = list.as_array().expect("capture_list answers an array");
    assert_eq!(rows.len(), 2, "two captures, two rows: {list}");
    assert_eq!(rows[0]["id"].as_str(), Some("cap-0001"));
    assert_eq!(rows[0]["has_mask"].as_bool(), Some(false));
    assert_eq!(rows[0]["scene_version"].as_u64(), Some(3));
    assert!(rows[0]["note"].is_null(), "no note was given: {list}");
    assert_eq!(rows[1]["id"].as_str(), Some("cap-0002"));
    assert_eq!(rows[1]["has_mask"].as_bool(), Some(true));
    assert_eq!(rows[1]["note"].as_str(), Some("with a mask"));
    for row in rows {
        assert!(row["created_at"].as_u64().unwrap_or(0) > 0, "{row}");
    }
}

#[test]
fn a_second_bind_reindexes_the_directory_and_carries_the_number_on() {
    let captures = Captures::new("reindex");
    {
        let (_server, mut client) = server_with(&captures);
        for tag in [1u8, 2] {
            client.call(
                "capture",
                json!({ "camera": camera(), "image": base64_encode(&png(8, 8, tag)) }),
            );
        }
    }
    // The first server is gone; a new one in the same directory must not hand
    // out cap-0001 again, which is already in someone's conversation.
    let (_server, mut client) = server_with(&captures);
    let list = client.call("capture_list", json!({}));
    assert_eq!(
        list.as_array().map(Vec::len),
        Some(2),
        "the index was rebuilt from disk: {list}"
    );
    let next = client.call(
        "capture",
        json!({ "camera": camera(), "image": base64_encode(&png(8, 8, 9)) }),
    );
    assert_eq!(
        next["id"].as_str(),
        Some("cap-0003"),
        "numbering carries on from the highest id on disk"
    );
    let first = client.call("capture_get", json!({ "id": "cap-0001" }));
    assert_eq!(first["id"].as_str(), Some("cap-0001"));
}

#[test]
fn a_capture_does_not_move_the_scene_version() {
    let captures = Captures::new("noversion");
    let (_server, mut client) = server_with(&captures);
    client.call("open", json!({ "content": CITY }));
    client.published();
    let before = client.call("hello", json!({ "client": "t", "protocol": 1 }));
    let version = before["version"].as_u64().expect("a version");
    client.call(
        "capture",
        json!({
            "camera": camera(),
            "image": base64_encode(&png(8, 8, 1)),
            "scene_version": version,
        }),
    );
    let after = client.call("hello", json!({ "client": "t", "protocol": 1 }));
    assert_eq!(
        after["version"].as_u64(),
        Some(version),
        "a capture is not a scene mutation"
    );
}

// --- the composition tree --------------------------------------------------

/// The layer ids of a tree, strongest first.
fn layer_ids(tree: &Value) -> Vec<String> {
    tree["layers"]
        .as_array()
        .expect("layers is an array")
        .iter()
        .map(|l| l["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn layer_kinds(tree: &Value) -> Vec<String> {
    tree["layers"]
        .as_array()
        .expect("layers is an array")
        .iter()
        .map(|l| l["kind"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn prim_node<'a>(tree: &'a Value, path: &str) -> &'a Value {
    tree["prim_nodes"]
        .as_array()
        .expect("prim_nodes is an array")
        .iter()
        .find(|p| p["path"].as_str() == Some(path))
        .unwrap_or_else(|| panic!("{path} is not a prim node of the tree"))
}

#[test]
fn the_city_stack_lists_its_layers_and_its_cook_sublayers() {
    let (_captures, _server, mut client) = opened_case("tree-city");
    let tree = client.call("composition_tree", json!({}));

    let layers = layer_ids(&tree);
    let kinds = layer_kinds(&tree);
    assert!(
        layers.len() >= 4,
        "the city stack is a session layer, the root layer and a cook layer per \
         generator: {layers:?}"
    );
    assert_eq!(
        kinds[0], "session",
        "the session layer is strongest: {kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|k| *k == "root").count(),
        1,
        "exactly one root layer: {kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|k| *k == "session").count(),
        1,
        "exactly one session layer: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "sublayer"),
        "the cooked generator layers join as sublayers: {kinds:?}"
    );
    // Ordinals are the strength order, with no gaps and no repeats.
    for (index, layer) in tree["layers"].as_array().unwrap().iter().enumerate() {
        assert_eq!(layer["strength"].as_u64(), Some(index as u64), "{layer}");
    }

    // The cook attaches its output layers below the root layer, so the tree
    // carries one edge per cooked layer, with the root layer at its strong end.
    let root_id = kinds
        .iter()
        .position(|k| k == "root")
        .map(|i| layers[i].clone())
        .expect("a root layer");
    let arcs = tree["arcs"].as_array().expect("arcs is an array");
    let sublayer_arcs: Vec<&Value> = arcs
        .iter()
        .filter(|a| a["kind"].as_str() == Some("sublayer"))
        .collect();
    assert!(
        !sublayer_arcs.is_empty(),
        "a cooked stack has sublayer edges: {arcs:?}"
    );
    for arc in &sublayer_arcs {
        assert_eq!(arc["source_layer"].as_str(), Some(root_id.as_str()));
        assert_eq!(arc["source_prim"].as_str(), Some("/"));
        assert_eq!(arc["target_prim"].as_str(), Some("/"));
        assert!(
            layers.contains(&arc["target_layer"].as_str().unwrap_or_default().to_owned()),
            "a sublayer edge points at a layer of the list: {arc}"
        );
    }
}

#[test]
fn a_prim_node_names_its_authors_strongest_first() {
    let (_captures, _server, mut client) = opened_case("tree-authors");
    let tree = client.call("composition_tree", json!({}));
    let layers = layer_ids(&tree);
    let root_id = tree["layers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["kind"].as_str() == Some("root"))
        .and_then(|l| l["id"].as_str())
        .expect("a root layer")
        .to_owned();

    // The document authors /World/City and the cook re-authors it: both say
    // something, the root layer first.
    let city = prim_node(&tree, "/World/City");
    let authored: Vec<&str> = city["authored_in"]
        .as_array()
        .expect("authored_in is an array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        authored.len() >= 2,
        "/World/City is authored by the document and by its generator's output: {city}"
    );
    assert_eq!(
        authored[0], root_id,
        "the root layer is the strongest author of /World/City: {city}"
    );
    for pair in authored.windows(2) {
        let first = layers.iter().position(|l| l == pair[0]).expect("a layer");
        let second = layers.iter().position(|l| l == pair[1]).expect("a layer");
        assert!(
            first < second,
            "authored_in is strongest first: {authored:?}"
        );
    }

    // A prim no generator rewrites is authored by the document alone.
    let prototype = prim_node(&tree, "/World/Prototypes/Tree");
    assert_eq!(
        prototype["authored_in"].as_array().map(Vec::len),
        Some(1),
        "{prototype}"
    );
    assert_eq!(
        prototype["authored_in"][0].as_str(),
        Some(root_id.as_str()),
        "{prototype}"
    );

    // The composed namespace is there, not only the authored pieces: a cooked
    // lot and a scattered prop are in the tree.
    let paths: Vec<&str> = tree["prim_nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["path"].as_str())
        .collect();
    assert!(
        paths
            .iter()
            .any(|p| p.contains("/City/") && p.contains("/lot")),
        "the cooked lots are prim nodes"
    );
    assert!(
        paths.iter().any(|p| p.contains("/StreetProps/i")),
        "the scattered props are prim nodes"
    );
    // Node ids are unique, and a prim's id is the path.
    let mut ids: Vec<&str> = tree["prim_nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["id"].as_str())
        .collect();
    let count = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), count, "prim node ids are unique");
    assert!(ids.contains(&"prim:/World/City"));
}

#[test]
fn a_prim_node_reports_its_layer_opinions_strongest_first() {
    let (_captures, _server, mut client) = opened_case("tree-opinions");
    let tree = client.call("composition_tree", json!({}));
    let layers = layer_ids(&tree);

    let detail = client.call("comp_node", json!({ "id": "prim:/World/City" }));
    assert_eq!(detail["path"].as_str(), Some("/World/City"));
    let opinions = detail["opinions"]
        .as_array()
        .expect("a prim node has opinions");
    assert!(
        opinions.len() >= 2,
        "the document and its cook layer both author /World/City: {detail}"
    );
    let mut last = 0;
    for opinion in opinions {
        let id = opinion["layer"].as_str().expect("a layer id");
        let at = layers.iter().position(|l| l == id).expect("a known layer");
        assert!(at >= last, "opinions are in strength order: {detail}");
        last = at;
        assert!(
            !opinion["fields"].as_array().expect("fields").is_empty(),
            "an opinion with no fields says nothing: {opinion}"
        );
    }
    // The root layer's opinion carries the document's own fields.
    let root_opinion = opinions
        .iter()
        .find(|o| {
            o["layer"].as_str()
                == Some(
                    layers
                        .iter()
                        .find(|l| l.contains("rusd_root"))
                        .unwrap_or(&layers[1])
                        .as_str(),
                )
        })
        .or_else(|| opinions.last())
        .expect("an opinion");
    let names: Vec<&str> = root_opinion["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert!(
        names.contains(&"typeName"),
        "a prim spec carries its typeName: {root_opinion}"
    );

    // The prim's arcs out and its variant sets are the tree's own rows.
    assert!(detail["arcs_out"].is_array(), "{detail}");
    assert!(detail["variant_sets"].is_array(), "{detail}");
}

#[test]
fn a_layer_node_carries_its_usda_text() {
    let (_captures, _server, mut client) = opened_case("tree-layer");
    let tree = client.call("composition_tree", json!({}));
    let root_id = tree["layers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["kind"].as_str() == Some("root"))
        .and_then(|l| l["id"].as_str())
        .expect("a root layer")
        .to_owned();

    let detail = client.call("comp_node", json!({ "id": root_id }));
    assert_eq!(detail["identifier"].as_str(), Some(root_id.as_str()));
    let text = detail["usda_text"].as_str().expect("the layer's text");
    assert!(
        text.starts_with("#usda 1.0"),
        "the detail is the layer's usda form: {text}"
    );
    assert!(
        text.contains("World") && text.contains("City"),
        "the root layer authors the document's prims: {text}"
    );
    assert!(
        detail["spec_count"].as_u64().unwrap_or(0) >= 5,
        "the root layer holds the document's prims: {detail}"
    );
    // The session layer's text is a layer too, and it is nearly empty until an
    // opinion lands in it.
    let session = layer_ids(&tree)[0].clone();
    let detail = client.call("comp_node", json!({ "id": session }));
    assert_eq!(detail["kind"].as_str(), Some("session"));
    assert!(detail["spec_count"].as_u64().is_some(), "{detail}");
}

#[test]
fn a_covered_prim_puts_the_session_layer_first() {
    let (_captures, _server, mut client) = opened_case("tree-override");
    let tree = client.call("composition_tree", json!({}));
    let session = layer_ids(&tree)[0].clone();

    // Cover a real lot, so the strongest opinion on it is the session layer's
    // rather than the cook layer's.
    let query = client.call("query", json!({}));
    let lot = query["prims"]
        .as_array()
        .expect("query lists prims")
        .iter()
        .filter_map(|p| p["path"].as_str())
        .find(|p| p.contains("/City/") && p.contains("/lot"))
        .map(str::to_owned)
        .expect("the cooked city has building lots");
    client.call(
        "opinion",
        json!({"ops": [{
            "path": lot,
            "field": "xformOp:translate",
            "value": [1.0, 2.0, 3.0],
            "op": "set",
        }]}),
    );
    client.published();

    // The tree a client refetches after the version moved names the session
    // layer as an author of the covered prim.
    let tree = client.call("composition_tree", json!({}));
    let node = prim_node(&tree, &lot);
    assert_eq!(
        node["authored_in"][0].as_str(),
        Some(session.as_str()),
        "the overrides session layer is the strongest author: {node}"
    );

    let detail = client.call("comp_node", json!({ "id": format!("prim:{lot}") }));
    let opinions = detail["opinions"]
        .as_array()
        .expect("a prim node has opinions");
    assert!(
        !opinions.is_empty(),
        "the covered lot has opinions: {detail}"
    );
    assert_eq!(
        opinions[0]["layer"].as_str(),
        Some(session.as_str()),
        "the overrides session layer is the strongest opinion: {detail}"
    );
    assert_eq!(
        opinions[0]["specifier"].as_str(),
        Some("over"),
        "the session layer's opinion is an over: {detail}"
    );
    let translate = opinions[0]["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .find(|f| f["name"].as_str() == Some("xformOp:translate"))
        .unwrap_or_else(|| panic!("the opinion carries the field: {detail}"));
    assert_eq!(translate["value"], json!([1.0, 2.0, 3.0]));
}

#[test]
fn an_unknown_comp_node_id_is_invalid_params() {
    let (_captures, _server, mut client) = opened_case("tree-unknown");
    for id in [
        "arc:9999",
        "prim:/World/Nothing",
        "vset:9999",
        "no-such-layer.usda",
    ] {
        let request = client.send("comp_node", json!({ "id": id }));
        let error = client.error(request);
        assert_eq!(
            error["code"].as_i64(),
            Some(-32602),
            "{id} should be invalid params: {error}"
        );
    }
    let request = client.send("comp_node", json!({}));
    assert_eq!(client.error(request)["code"].as_i64(), Some(-32602));
}

#[test]
fn both_tree_methods_need_an_open_scene() {
    let captures = Captures::new("tree-closed");
    let (_server, mut client) = server_with(&captures);
    for method in ["composition_tree", "comp_node"] {
        let params = if method == "comp_node" {
            json!({ "id": "prim:/World" })
        } else {
            json!({})
        };
        let request = client.send(method, params);
        let error = client.error(request);
        assert_eq!(error["code"].as_i64(), Some(-32000), "{method}: {error}");
    }
}

#[test]
fn the_tree_and_node_agree_on_the_stage_they_describe() {
    // A `comp_node` after a mutation must not answer out of the tree of the
    // scene that mutation replaced: a lot that exists only after the patch must
    // resolve, and a prim path that is gone must refuse.
    let (_captures, _server, mut client) = opened_case("tree-stale");
    let first = client.call("composition_tree", json!({}));
    let lots_before: Vec<String> = first["prim_nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["path"].as_str())
        .filter(|p| p.contains("/City/") && p.contains("/lot"))
        .map(str::to_owned)
        .collect();
    assert!(!lots_before.is_empty());

    // Re-cook with a narrower grid: the namespace changes.
    client.call(
        "patch",
        json!({"ops": [{"op": "set", "prim": "/World/City", "property": "blocks_x", "value": 2}]}),
    );
    client.published();

    let second = client.call("composition_tree", json!({}));
    let lots_after: Vec<String> = second["prim_nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["path"].as_str())
        .filter(|p| p.contains("/City/") && p.contains("/lot"))
        .map(str::to_owned)
        .collect();
    assert!(
        lots_after.len() < lots_before.len(),
        "a 2-block city has fewer lots than a 6-block one: {} -> {}",
        lots_before.len(),
        lots_after.len()
    );
    // A lot that the narrower grid dropped is gone from the tree, and asking
    // for it by id is refused rather than answered out of the old tree.
    if let Some(gone) = lots_before.iter().find(|p| !lots_after.contains(p)) {
        let request = client.send("comp_node", json!({ "id": format!("prim:{gone}") }));
        assert_eq!(
            client.error(request)["code"].as_i64(),
            Some(-32602),
            "{gone} was dropped by the re-cook"
        );
    }
    // And a lot the new tree holds resolves.
    let present = lots_after.first().expect("the 2-block city still has lots");
    let detail = client.call("comp_node", json!({ "id": format!("prim:{present}") }));
    assert_eq!(detail["path"].as_str(), Some(present.as_str()));
}

#[test]
fn a_tree_build_is_not_a_slow_path() {
    // The tree walks every layer of the stack, and a viewer refetches it on
    // every version: it must not cost a second. The city's biggest layer holds
    // ~500 prims; the budget leaves room for a debug build on a busy machine.
    let (_captures, _server, mut client) = opened_case("tree-timing");
    let start = Instant::now();
    let tree = client.call("composition_tree", json!({}));
    let took = start.elapsed();
    let prims = tree["prim_nodes"].as_array().map(Vec::len).unwrap_or(0);
    assert!(prims > 100, "the city composes many prims: {prims}");
    assert!(
        took < std::time::Duration::from_secs(2),
        "a tree build of {prims} prims took {took:?}"
    );
}
