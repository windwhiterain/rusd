//! Integration tests for `preview{upto}` (`docs/PROTOCOL.md` v1.4, D14 slice B):
//! the composition a connection's snapshots stop at, and the `op` flag the
//! composition tree uses to name the layers a preview can stop at.
//!
//! Real TCP against a server thread on an ephemeral port, and the client half is
//! the viewer's shape: it decodes a `stage_snapshot`, composes it on its own, and
//! reads geometry out of the composed stage. So "the scatter is not there" is a
//! statement about the scene the client would draw, not about a server field.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;

use openusd::ar::{Asset, AssetBuffer, ResolvedPath, Resolver, SharedBuffer};
use openusd::sdf::Layer;
use openusd::usd::{PrimPredicate, Stage};
use rusd_agent::transport::{Server, base64_decode};
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

    /// The next notification (a message with no `id`).
    fn notification(&mut self) -> Value {
        loop {
            let msg = match self.inbox.pop_front() {
                Some(m) => m,
                None => self.read_line(),
            };
            if msg.get("id").is_some_and(|id| !id.is_null()) {
                self.inbox.push_back(msg);
                continue;
            }
            return msg;
        }
    }

    /// The `stage_snapshot` and `state_changed` a mutation published, in that
    /// order (a `diagnostics` between them is skipped).
    fn mutation(&mut self) -> (Value, Value) {
        let mut snapshot = None;
        let mut state = None;
        while snapshot.is_none() || state.is_none() {
            let msg = self.notification();
            match msg["method"].as_str() {
                Some("stage_snapshot") => snapshot = Some(msg["params"].clone()),
                Some("state_changed") => state = Some(msg["params"].clone()),
                Some("diagnostics") => {}
                other => panic!("unexpected notification {other:?}: {msg}"),
            }
        }
        (snapshot.unwrap(), state.unwrap())
    }

    /// A mutation call plus the two notifications it produced.
    fn mutate(&mut self, method: &str, params: Value) -> (Value, Value, Value) {
        let result = self.call(method, params);
        let (snapshot, state) = self.mutation();
        (result, snapshot, state)
    }
}

// --- the client-side mirror (Tier A) ---------------------------------------

/// A resolver serving one layer's bytes from memory: what a viewer does with a
/// snapshot, without writing a file.
struct OneLayer {
    identifier: String,
    bytes: Arc<[u8]>,
}

impl Resolver for OneLayer {
    fn create_identifier(&self, asset_path: &str, _anchor: Option<&ResolvedPath>) -> String {
        asset_path.to_owned()
    }

    fn resolve(&self, asset_path: &str) -> Option<ResolvedPath> {
        (asset_path == self.identifier).then(|| ResolvedPath::new(asset_path))
    }

    fn resolve_for_new_asset(&self, _asset_path: &str) -> Option<ResolvedPath> {
        None
    }

    fn open_asset(&self, resolved_path: &ResolvedPath) -> std::io::Result<Box<dyn Asset>> {
        let path = resolved_path.to_string_lossy();
        if path != self.identifier {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, path));
        }
        Ok(Box::new(std::io::Cursor::new(AssetBuffer::Shared(
            SharedBuffer::new(Arc::clone(&self.bytes)),
        ))))
    }
}

/// Compose a `stage_snapshot`'s bytes the way a viewer does: a single-layer
/// stack over an in-memory resolver.
fn compose(snapshot: &Value) -> Stage {
    assert_eq!(snapshot["format"].as_str(), Some("usdc"));
    let data = snapshot["data"]
        .as_str()
        .expect("a snapshot carries base64 data");
    let bytes = base64_decode(data).expect("the snapshot data is base64");
    assert_eq!(
        &bytes[..8],
        b"PXR-USDC",
        "the snapshot is a usdc crate file, not text"
    );
    let layer = Layer::from_bytes("snapshot.usdc", bytes.clone()).expect("the snapshot decodes");
    let resolver = OneLayer {
        identifier: layer.identifier().to_owned(),
        bytes: Arc::from(bytes.into_boxed_slice()),
    };
    Stage::builder()
        .resolver(resolver)
        .open(layer.identifier())
        .expect("the snapshot composes on its own")
}

fn prim_count(stage: &Stage) -> usize {
    let mut n = 0;
    let _ = stage.traverse(PrimPredicate::ALL, |_| n += 1);
    n
}

/// Whether the composed scene holds a prim path containing `needle`.
fn has_prim(stage: &Stage, needle: &str) -> bool {
    let mut found = false;
    let _ = stage.traverse(PrimPredicate::ALL, |path| {
        if path.as_str().contains(needle) {
            found = true;
        }
    });
    found
}

/// The world bbox of the composed snapshot, via the extraction the renderer uses.
fn bounds_of(stage: &Stage) -> ([f64; 3], [f64; 3]) {
    rusd_scene::geometry::extract(stage).bounds
}

// --- fixtures --------------------------------------------------------------

/// A server on an ephemeral loopback port plus a subscribed client.
fn subscribed() -> (Server, Client) {
    let server = Server::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let mut client = Client::connect(server.local_addr());
    client.call("hello", json!({"client": "test", "protocol": 1}));
    let sub = client.call("subscribe", json!({"client": "test"}));
    assert_eq!(sub["version"].as_u64(), Some(0), "nothing is open yet");
    (server, client)
}

/// The city opened and mirrored, with the snapshot and the version it published.
fn opened() -> (Server, Client, Value, u64) {
    let (server, mut client) = subscribed();
    let (_, snapshot, state) = client.mutate("open", json!({"content": CITY}));
    let version = state["version"].as_u64().expect("a version");
    assert_eq!(snapshot["version"].as_u64(), Some(version));
    (server, client, snapshot, version)
}

/// The layer ids of a tree's `layers` rows that are cook output layers, in
/// **cook order** (the order `preview{upto}` counts in), plus the ids of the
/// rows that are not.
fn tree_layers(tree: &Value) -> (Vec<String>, Vec<String>) {
    let mut ops = Vec::new();
    let mut rest = Vec::new();
    let mut rows: Vec<&Value> = tree["layers"]
        .as_array()
        .expect("layers is an array")
        .iter()
        .collect();
    // The tree lists layers strongest first, and the cook inserts each output
    // layer at the top of the root's stack, so cook order is the reverse of the
    // order the op rows appear in.
    rows.reverse();
    for row in rows {
        let id = row["id"].as_str().expect("a layer id").to_owned();
        if row["op"].as_bool() == Some(true) {
            ops.push(id);
        } else {
            rest.push(id);
        }
    }
    (ops, rest)
}

// --- the op flag -----------------------------------------------------------

#[test]
fn the_tree_marks_the_cook_output_layers_and_nothing_else() {
    let (_server, mut client, _snapshot, _version) = opened();
    let tree = client.call("composition_tree", json!({}));

    let (ops, rest) = tree_layers(&tree);
    assert_eq!(
        ops.len(),
        2,
        "the city cooks two generators, so two output layers: {tree}"
    );
    assert!(
        ops[0].contains("rusd_city.usda"),
        "the first layer in cook order is the street grid's output: {ops:?}"
    );
    assert!(
        ops[1].contains("rusd_scatter.usda"),
        "the scatter cooks after the city, and that is the preview order: {ops:?}"
    );

    // Everything else — the session layer, the root layer — is not an op.
    for id in &rest {
        assert!(
            id.contains("rusd_session") || id.contains("rusd_root"),
            "a non-op layer is the session or root layer, got {id:?} in {rest:?}"
        );
    }

    // The flag is on the layer rows and on `comp_node`'s layer detail alike, so
    // a client does not have to fetch a detail to tell the two apart.
    let detail = client.call("comp_node", json!({"id": ops[1]}));
    assert_eq!(detail["kind_type"].as_str(), Some("layer"));
    assert_eq!(detail["op"].as_bool(), Some(true), "{detail}");
    let root = rest
        .iter()
        .find(|id| id.contains("rusd_root"))
        .expect("the root layer");
    let detail = client.call("comp_node", json!({"id": root}));
    assert_eq!(detail["op"].as_bool(), Some(false), "{detail}");
}

// --- the preview -----------------------------------------------------------

#[test]
fn a_preview_up_to_the_city_layer_drops_the_scatter_and_null_restores_it() {
    let (_server, mut client, full, version) = opened();
    let tree = client.call("composition_tree", json!({}));
    let (ops, _) = tree_layers(&tree);
    let city_layer = ops[0].clone();

    // The whole composition holds the scatter's instances as well as the city.
    let full_stage = compose(&full);
    let full_count = prim_count(&full_stage);
    let full_bounds = bounds_of(&full_stage);
    assert!(
        has_prim(&full_stage, "/World/StreetProps/i"),
        "the final composition holds the scatter's instances"
    );

    // Preview up to the street grid's output: the scatter cooks after it, so its
    // layer is dropped and its instances with it.
    let answer = client.call("preview", json!({"upto": city_layer}));
    assert_eq!(answer["upto"].as_str(), Some(city_layer.as_str()));
    let pushed = client.notification();
    assert_eq!(
        pushed["method"].as_str(),
        Some("stage_snapshot"),
        "{pushed}"
    );
    let snapshot = pushed["params"].clone();
    assert_eq!(
        snapshot["version"].as_u64(),
        Some(version),
        "a preview is not a scene mutation: the version must not move"
    );
    assert_eq!(
        snapshot["upto"].as_str(),
        Some(city_layer.as_str()),
        "the snapshot names the layer the preview stops after: {snapshot}"
    );
    let stage = compose(&snapshot);
    // The scatter's *anchor* prim is authored by the document, so it survives the
    // prefix; what the dropped layer contributed is the instances under it.
    assert!(
        !has_prim(&stage, "/World/StreetProps/i"),
        "the scatter's instances are not in the prefix: {} prims",
        prim_count(&stage)
    );
    assert!(
        has_prim(&stage, "/World/City"),
        "the street grid's prim is in the prefix"
    );
    let prefix_count = prim_count(&stage);
    assert!(
        prefix_count < full_count,
        "the prefix is smaller than the whole: {prefix_count} vs {full_count}"
    );
    // 475 prims whole, 175 with the scatter muted (measured on the city example);
    // assert the order of magnitude rather than the exact number so a seed change
    // in `examples/city.rusd` does not fail this test.
    assert!(
        (100..full_count).contains(&prefix_count),
        "the prefix holds the city but not the scatter's 300 instances: {prefix_count}"
    );
    let prefix_bounds = bounds_of(&stage);
    assert_ne!(
        full_bounds, prefix_bounds,
        "dropping the scatter must change the composed bbox: {full_bounds:?} vs {prefix_bounds:?}"
    );

    // A preview up to the **last** cook op is the final composition: there is
    // nothing after it to drop, so the scene is the whole one and only the
    // `upto` field distinguishes it.
    let last = ops[1].clone();
    let answer = client.call("preview", json!({"upto": last}));
    assert_eq!(answer["op_index"].as_u64(), Some(1));
    let snapshot = client.notification()["params"].clone();
    assert_eq!(snapshot["upto"].as_str(), Some(last.as_str()));
    assert_eq!(
        prim_count(&compose(&snapshot)),
        full_count,
        "stopping after the last op drops nothing"
    );

    // And back to the final composition, which says so by carrying no `upto`.
    let answer = client.call("preview", json!({"upto": Value::Null}));
    assert_eq!(answer["upto"], Value::Null);
    let snapshot = client.notification()["params"].clone();
    assert!(
        snapshot.get("upto").is_none(),
        "the final snapshot carries no `upto`: {snapshot}"
    );
    assert_eq!(snapshot["version"].as_u64(), Some(version));
    let stage = compose(&snapshot);
    assert_eq!(
        prim_count(&stage),
        full_count,
        "the whole composition is back"
    );
    // A preview must leave the authority stack exactly as it found it: the final
    // snapshot after a preview round trip is byte-identical to the one `open`
    // published, so muting and unmuting the later cook layers left no residue in
    // the composition (and the snapshot is still deterministic).
    assert_eq!(
        snapshot, full,
        "the final snapshot is the one `open` published, byte for byte"
    );
    assert!(has_prim(&stage, "/World/StreetProps/i"));

    // The version never moved through any of it.
    let hello = client.call("hello", json!({"protocol": 1}));
    assert_eq!(
        hello["version"].as_u64(),
        Some(version),
        "preview does not advance the version"
    );
}

#[test]
fn a_preview_keeps_the_session_overrides() {
    // PROTOCOL v1.4: the prefix is the root layer + the cook ops up to `upto` +
    // **the session overrides**. A human's gizmo drag is stronger than every
    // layer of the stack, so a prefix that dropped it would show the user a
    // scene they had already changed.
    let (_server, mut client, _full, _opened_version) = opened();
    let tree = client.call("composition_tree", json!({}));
    let (ops, _) = tree_layers(&tree);
    let city_layer = ops[0].clone();

    // The street grid's own prim: it is authored by the document, so it is in
    // the prefix whether or not its output layer is.
    let target = [4.0f32, 5.0, 6.0];
    let (_, _, state) = client.mutate(
        "opinion",
        json!({"ops": [{"path": "/World/City", "field": "xformOp:translate", "value": target, "op": "set"}]}),
    );
    let version = state["version"].as_u64().expect("a version");

    client.call("preview", json!({"upto": city_layer}));
    let pushed = client.notification();
    assert_eq!(
        pushed["method"].as_str(),
        Some("stage_snapshot"),
        "{pushed}"
    );
    let snapshot = pushed["params"].clone();
    assert_eq!(snapshot["version"].as_u64(), Some(version));
    assert_eq!(snapshot["upto"].as_str(), Some(city_layer.as_str()));

    let translate = |snapshot: &Value| {
        let stage = compose(snapshot);
        let path = openusd::sdf::path("/World/City").expect("a prim path");
        let prim = stage.prim(path).expect("the prim composes");
        match prim
            .attribute("xformOp:translate")
            .get::<openusd::sdf::Value>()
            .expect("the attribute is readable")
        {
            Some(openusd::sdf::Value::Vec3f(v)) => Some([v[0], v[1], v[2]]),
            other => panic!("the composed translate is a vec3f, got {other:?}"),
        }
    };
    assert_eq!(
        translate(&snapshot),
        Some(target),
        "the prefix carries the session override"
    );
    // And the override belongs to the session layer, so it is still there:
    // a preview neither consumes nor perturbs it.
    let hello = client.call("hello", json!({"protocol": 1}));
    assert_eq!(hello["version"].as_u64(), Some(version));
    client.call("preview", json!({"upto": Value::Null}));
    let final_snapshot = client.notification()["params"].clone();
    assert!(final_snapshot.get("upto").is_none());
    assert_eq!(
        translate(&final_snapshot),
        Some(target),
        "the final composition keeps the override after a preview"
    );
}

#[test]
fn a_preview_past_the_new_cook_falls_back_to_the_final_composition() {
    // A preview is remembered as a position in cook order, because the cook
    // output layers' identifiers are anonymous and a rebuild mints new ones.
    // When the rebuild produces a **shorter** cook than the position names,
    // there is no op left to stop after: the connection falls back to the final
    // composition, which says so by carrying no `upto`.
    let (_server, mut client, _snapshot, _version) = opened();
    let tree = client.call("composition_tree", json!({}));
    let (ops, _) = tree_layers(&tree);
    // Stop after the second op — the scatter, i.e. the last one.
    client.call("preview", json!({"upto": ops[1]}));
    let pushed = client.notification();
    assert_eq!(pushed["params"]["upto"].as_str(), Some(ops[1].as_str()));

    // Remove the scatter generator: the next cook has one op, and position 1 is
    // gone. (`remove` takes the whole prim.)
    let (_, snapshot, state) = client.mutate(
        "patch",
        json!({"ops": [{"op": "remove", "prim": "/World/StreetProps"}]}),
    );
    assert_eq!(state["reason"].as_str(), Some("patch"));
    assert!(
        snapshot.get("upto").is_none(),
        "the preview position no longer names an op, so the snapshot is the final one"
    );

    // One op is left, and it is a cook output layer the tree still marks.
    let tree = client.call("composition_tree", json!({}));
    let (ops, _) = tree_layers(&tree);
    assert_eq!(ops.len(), 1, "only the street grid cooks now: {ops:?}");
    // The state was drained with the fallback: a further mutation still gets the
    // final composition rather than resurrecting the old position.
    let (_, snapshot, _) = client.mutate("undo", json!({}));
    assert!(snapshot.get("upto").is_none(), "{snapshot}");
}

#[test]
fn an_unknown_upto_is_invalid_params() {
    let (_server, mut client, _snapshot, _version) = opened();
    let tree = client.call("composition_tree", json!({}));
    let (ops, rest) = tree_layers(&tree);
    let root_layer = rest
        .iter()
        .find(|id| id.contains("rusd_root"))
        .expect("the root layer")
        .clone();

    // A name nothing in the stage answers to is refused, and the message names
    // **both** kinds of stop that would have worked: the cook output layers and,
    // since PROTOCOL v1.5.3, the root layer.
    let id = client.send("preview", json!({ "upto": "anon:404:nowhere.usda" }));
    let error = client.error(id);
    assert_eq!(error["code"].as_i64(), Some(-32602), "{error}");
    let message = error["message"].as_str().unwrap_or_default().to_owned();
    assert!(
        message.contains("neither a cook output layer nor the root layer"),
        "the message says what went wrong: {error}"
    );
    assert!(
        message.contains(root_layer.as_str()),
        "the message names the root layer `{root_layer}`: {error}"
    );
    for op in &ops {
        assert!(
            message.contains(op.as_str()),
            "the message lists the op layer `{op}`: {error}"
        );
    }

    // The root layer on its own is a stop now, not an error — a separate test
    // covers what it composes — so it is not repeated here.

    // `upto` itself is required: `{}` is not "the final composition".
    let id = client.send("preview", json!({}));
    let error = client.error(id);
    assert_eq!(error["code"].as_i64(), Some(-32602), "{error}");

    // And a refused preview left the connection on the final composition: a
    // later mutation hands it a snapshot with no `upto`.
    let (_, snapshot, state) = client.mutate(
        "patch",
        json!({"ops": [{"op": "set", "prim": "/World/StreetProps", "property": "count", "value": 3}]}),
    );
    assert!(state["version"].as_u64().unwrap() > 0);
    assert!(snapshot.get("upto").is_none(), "{snapshot}");
    assert_eq!(
        ops.len(),
        2,
        "the city still cooks both generators: {ops:?}"
    );
}

// --- the root stop (PROTOCOL v1.5.3) ---------------------------------------

/// The id of the tree's `kind = "root"` layer, or the row naming `rusd_root`.
fn root_layer_of(tree: &Value) -> String {
    let rows = tree["layers"].as_array().expect("layers is an array");
    rows.iter()
        .find(|row| row["kind"].as_str() == Some("root"))
        .or_else(|| {
            rows.iter()
                .find(|row| row["id"].as_str().unwrap_or_default().contains("rusd_root"))
        })
        .and_then(|row| row["id"].as_str())
        .expect("the tree lists the root layer")
        .to_owned()
}

#[test]
fn a_preview_stopping_at_the_root_layer_shows_the_pipelines_input() {
    // The root stop PROTOCOL v1.5.3 requires to be reachable: the root layer and
    // the session overrides, with **every** cook output layer muted. What a
    // viewer sees is the document's own scene — the geometry the cook reads —
    // and none of what the generators wrote.
    let (_server, mut client, full, version) = opened();
    let tree = client.call("composition_tree", json!({}));
    let (ops, _) = tree_layers(&tree);
    let root_layer = root_layer_of(&tree);

    let answer = client.call("preview", json!({ "upto": root_layer }));
    assert_eq!(answer["upto"].as_str(), Some(root_layer.as_str()));
    assert_eq!(
        answer["root"].as_bool(),
        Some(true),
        "the answer says this stop is the root layer, not a cook op: {answer}"
    );

    let pushed = client.notification();
    assert_eq!(
        pushed["method"].as_str(),
        Some("stage_snapshot"),
        "{pushed}"
    );
    let snapshot = pushed["params"].clone();
    assert_eq!(
        snapshot["upto"].as_str(),
        Some(root_layer.as_str()),
        "the snapshot names the layer the preview stops at: {snapshot}"
    );
    assert_eq!(
        snapshot["version"].as_u64(),
        Some(version),
        "a preview is not a scene mutation: the version must not move"
    );

    let stage = compose(&snapshot);
    // Nothing a generator authored is here: the street grid's ground and the
    // scatter's instances both come from cook output layers.
    assert!(
        !has_prim(&stage, "/World/City/Ground"),
        "the street grid's output is muted at the root stop: {} prims",
        prim_count(&stage)
    );
    assert!(
        !has_prim(&stage, "/World/StreetProps/i"),
        "the scatter's instances are muted at the root stop"
    );
    // The document's own prims are, which is what makes the stop useful: they
    // are the input the pipeline reads.
    for path in ["/World", "/World/City", "/World/Prototypes"] {
        assert!(
            has_prim(&stage, path),
            "{path} is authored by the document, so it survives: {} prims",
            prim_count(&stage)
        );
    }
    let root_count = prim_count(&stage);
    let root_bounds = bounds_of(&stage);

    // The stop after the first op is strictly bigger: it holds the street grid.
    client.call("preview", json!({ "upto": ops[0].clone() }));
    let city_stage = compose(&client.notification()["params"].clone());
    assert!(
        has_prim(&city_stage, "/World/City/Ground"),
        "the first op's prefix does hold the street grid"
    );
    assert!(
        root_count < prim_count(&city_stage),
        "the root stop composes fewer prims than the first op's prefix: {root_count} vs {}",
        prim_count(&city_stage)
    );
    assert_ne!(
        root_bounds,
        bounds_of(&city_stage),
        "the missing street grid must change the composed bbox: {root_bounds:?}"
    );
    assert_ne!(
        root_bounds,
        bounds_of(&compose(&full)),
        "and it is not the final composition either"
    );

    // The version never moved through any of it.
    let hello = client.call("hello", json!({ "protocol": 1 }));
    assert_eq!(
        hello["version"].as_u64(),
        Some(version),
        "preview does not advance the version"
    );
}

#[test]
fn the_root_stop_survives_a_rebuild() {
    // The root stop is remembered as **a position no cook reaches** (`-1`), not
    // as the identifier the client sent: a rebuild mints a new anonymous root
    // layer, and the stop must mean the same thing on the new stack.
    let (_server, mut client, _snapshot, _version) = opened();
    let tree = client.call("composition_tree", json!({}));
    let root_layer = root_layer_of(&tree);
    client.call("preview", json!({ "upto": root_layer }));
    let pushed = client.notification()["params"].clone();
    assert_eq!(pushed["upto"].as_str(), Some(root_layer.as_str()));

    // A document edit rebuilds the stack, and every anon identifier with it.
    let (_, snapshot, state) = client.mutate(
        "patch",
        json!({"ops": [{"op": "set", "prim": "/World/City", "property": "blocks_x", "value": 3}]}),
    );
    assert!(state["version"].as_u64().unwrap() > 0);
    assert_eq!(state["reason"].as_str(), Some("patch"));
    let rebuilt_root = snapshot["upto"].as_str().unwrap_or_default().to_owned();
    assert!(
        rebuilt_root.contains("rusd_root"),
        "the rebuilt snapshot still stops at the root layer: {snapshot}"
    );
    assert_ne!(
        rebuilt_root, root_layer,
        "the rebuild really did mint a new anonymous root layer, so the stop \
         surviving is the position and not the spelling: {root_layer} vs {rebuilt_root}"
    );
    assert!(
        !has_prim(&compose(&snapshot), "/World/City/Ground"),
        "and no cook output is composed there"
    );

    // The state itself is still in force, not just the one snapshot: a further
    // mutation (the undo re-cooks the restored document) carries it too.
    let (_, snapshot, _) = client.mutate("undo", json!({}));
    assert!(
        snapshot["upto"]
            .as_str()
            .unwrap_or_default()
            .contains("rusd_root"),
        "the root stop is remembered across rebuilds: {snapshot}"
    );
    assert!(!has_prim(&compose(&snapshot), "/World/City/Ground"));

    // The identifiers the connection is *sent* keep moving with each rebuild,
    // while what it asked for does not. This is the property `-1` buys.
    assert_ne!(
        snapshot["upto"].as_str().unwrap_or_default(),
        rebuilt_root,
        "a second rebuild mints yet another root layer: {}",
        snapshot["upto"]
    );

    // And `null` still leaves it for the final composition.
    let answer = client.call("preview", json!({ "upto": Value::Null }));
    assert_eq!(answer["upto"], Value::Null);
    let final_snapshot = client.notification()["params"].clone();
    assert!(final_snapshot.get("upto").is_none(), "{final_snapshot}");
}
// --- the root stop and the session layer -----------------------------------

#[test]
fn the_root_stop_keeps_the_session_overrides() {
    // PROTOCOL v1.5.3: the root stop = the root layer **plus** the session
    // overrides, and no cook op. The overrides belong to the session layer, which
    // is not a sublayer of the root layer, so muting the whole cook leaves them
    // in force — a human's gizmo drag is stronger than every layer of the stack.
    let (_server, mut client, _full, _opened_version) = opened();
    let tree = client.call("composition_tree", json!({}));
    let root_layer = root_layer_of(&tree);

    let target = [7.0f32, 8.0, 9.0];
    let (_, _, state) = client.mutate(
        "opinion",
        json!({"ops": [{"path": "/World/City", "field": "xformOp:translate", "value": target, "op": "set"}]}),
    );
    let version = state["version"].as_u64().expect("a version");

    client.call("preview", json!({ "upto": root_layer }));
    let snapshot = client.notification()["params"].clone();
    assert_eq!(snapshot["upto"].as_str(), Some(root_layer.as_str()));
    assert_eq!(snapshot["version"].as_u64(), Some(version));

    // The override is composed, and none of the cook's output is.
    let stage = compose(&snapshot);
    let path = openusd::sdf::path("/World/City").expect("a prim path");
    let prim = stage.prim(path).expect("the prim composes");
    let translate = match prim
        .attribute("xformOp:translate")
        .get::<openusd::sdf::Value>()
        .expect("the attribute is readable")
    {
        Some(openusd::sdf::Value::Vec3f(v)) => Some([v[0], v[1], v[2]]),
        other => panic!("the composed translate is a vec3f, got {other:?}"),
    };
    assert_eq!(
        translate,
        Some(target),
        "the root stop carries the session override"
    );
    assert!(
        !has_prim(&stage, "/World/City/Ground"),
        "and none of the street grid's own output"
    );

    // The override survives the return to the final composition: the preview
    // neither consumed nor perturbed it.
    client.call("preview", json!({ "upto": Value::Null }));
    let final_snapshot = client.notification()["params"].clone();
    assert!(final_snapshot.get("upto").is_none(), "{final_snapshot}");
    assert!(
        has_prim(&compose(&final_snapshot), "/World/City/Ground"),
        "the whole composition is back"
    );
    let hello = client.call("hello", json!({ "protocol": 1 }));
    assert_eq!(hello["version"].as_u64(), Some(version));
}
#[test]
fn each_connection_keeps_its_own_preview() {
    let (server, mut full_viewer, _snapshot, version) = opened();
    let tree = full_viewer.call("composition_tree", json!({}));
    let (ops, _) = tree_layers(&tree);
    let city_layer = ops[0].clone();
    let full_count = prim_count(&compose(&_snapshot));

    // A second viewer on the same session asks for the prefix.
    let mut prefix_viewer = Client::connect(server.local_addr());
    prefix_viewer.call("hello", json!({"client": "prefix", "protocol": 1}));
    prefix_viewer.call("subscribe", json!({"client": "prefix"}));
    let pushed = prefix_viewer.notification()["params"].clone();
    assert_eq!(
        prim_count(&compose(&pushed)),
        full_count,
        "a fresh subscriber starts from the final composition"
    );
    prefix_viewer.call("preview", json!({"upto": city_layer}));
    let prefix_snapshot = prefix_viewer.notification()["params"].clone();
    assert_eq!(
        prefix_snapshot["upto"].as_str(),
        Some(city_layer.as_str()),
        "the previewing viewer is on the prefix"
    );

    // One mutation, two different snapshots: the state belongs to the
    // connection, not to the session.
    full_viewer.call(
        "patch",
        json!({"ops": [{"op": "set", "prim": "/World/City", "property": "blocks_x", "value": 3}]}),
    );
    let (prefix_after, prefix_state) = prefix_viewer.mutation();
    let (full_after, full_state) = full_viewer.mutation();
    assert_eq!(prefix_state["version"].as_u64(), Some(version + 1));
    assert_eq!(full_state["version"].as_u64(), Some(version + 1));

    // The identifier in `upto` moves across a rebuild — the cooked layers are
    // anonymous, and a document edit mints new ones — while the preview state
    // does not: what the connection asked for was "stop after the street grid",
    // and that is what it still gets. So the check is on the generator the layer
    // belongs to, not on the spelling it had before the patch.
    assert!(
        prefix_after["upto"]
            .as_str()
            .unwrap_or_default()
            .contains("rusd_city.usda"),
        "the previewing viewer's snapshot still stops after the street grid: {}",
        prefix_after["upto"]
    );
    assert!(
        full_after.get("upto").is_none(),
        "the other viewer still gets the final composition: {}",
        full_after["version"]
    );
    let prefix_stage = compose(&prefix_after);
    assert!(!has_prim(&prefix_stage, "/World/StreetProps/i"));
    assert!(has_prim(&compose(&full_after), "/World/StreetProps/i"));
}

#[test]
fn three_viewers_on_three_stops_get_three_snapshots() {
    // `publish` builds one snapshot per **distinct** preview state, and the root
    // stop is an ordinary state in that set (PROTOCOL v1.5.3): two viewers both
    // on `-1` share one flatten, and the one on the final composition must still
    // get its own. A mutation is where all three are told at once.
    let (server, mut full_viewer, _snapshot, version) = opened();
    let tree = full_viewer.call("composition_tree", json!({}));
    let root_layer = root_layer_of(&tree);

    let mut root_a = Client::connect(server.local_addr());
    root_a.call("hello", json!({ "client": "root-a", "protocol": 1 }));
    root_a.call("subscribe", json!({ "client": "root-a" }));
    let _ = root_a.notification();
    root_a.call("preview", json!({ "upto": root_layer }));
    let first = root_a.notification()["params"].clone();
    assert_eq!(first["upto"].as_str(), Some(root_layer.as_str()));

    let mut root_b = Client::connect(server.local_addr());
    root_b.call("hello", json!({ "client": "root-b", "protocol": 1 }));
    root_b.call("subscribe", json!({ "client": "root-b" }));
    let _ = root_b.notification();
    root_b.call("preview", json!({ "upto": root_layer }));
    let second = root_b.notification()["params"].clone();
    assert_eq!(second["upto"].as_str(), Some(root_layer.as_str()));

    // One mutation: three connections, three stops.
    full_viewer.call(
        "patch",
        json!({"ops": [{"op": "set", "prim": "/World/City", "property": "blocks_x", "value": 3}]}),
    );
    let (a_after, a_state) = root_a.mutation();
    let (b_after, b_state) = root_b.mutation();
    let (full_after, full_state) = full_viewer.mutation();
    assert_eq!(a_state["version"].as_u64(), Some(version + 1));
    assert_eq!(b_state["version"].as_u64(), Some(version + 1));
    assert_eq!(full_state["version"].as_u64(), Some(version + 1));

    // Both root-stopped viewers see the same root composition, with no cook
    // output in it, and the third sees the whole scene.
    for after in [&a_after, &b_after] {
        assert!(
            after["upto"]
                .as_str()
                .unwrap_or_default()
                .contains("rusd_root"),
            "a root-stopped viewer keeps its stop: {after}"
        );
        assert!(!has_prim(&compose(after), "/World/City/Ground"));
    }
    assert_eq!(
        a_after, b_after,
        "two viewers on the same stop share the one snapshot"
    );
    assert!(
        full_after.get("upto").is_none(),
        "the viewer that never previewed still gets the final composition: {full_after}"
    );
    assert!(has_prim(&compose(&full_after), "/World/City/Ground"));

    // And the scene each of them holds is what its own stop composes: a later
    // snapshot request from the full viewer does not move the others. The patch
    // rebuilt the stack and minted new anonymous layer ids, so the op to stop at
    // is the one the tree names **now**.
    let tree = full_viewer.call("composition_tree", json!({}));
    let (ops, _) = tree_layers(&tree);
    full_viewer.call("preview", json!({ "upto": ops[0].clone() }));
    let op_snapshot = full_viewer.notification()["params"].clone();
    assert_eq!(op_snapshot["upto"].as_str(), Some(ops[0].as_str()));
    assert!(has_prim(&compose(&op_snapshot), "/World/City/Ground"));
    assert!(
        !has_prim(&compose(&op_snapshot), "/World/StreetProps/i"),
        "the op stop holds the street grid but not the scatter"
    );

    // The root-stopped viewers were not touched by that: a notification to one
    // connection is not a state change for the others, and there was none to
    // send — a preview moves no version.
    let hello = root_a.call("hello", json!({ "protocol": 1 }));
    assert_eq!(
        hello["version"].as_u64(),
        Some(version + 1),
        "the version is the patch's, not the preview's"
    );
}
#[test]
fn a_preview_from_an_unsubscribed_connection_sends_no_snapshot() {
    // A snapshot is what a subscription is for. A connection that asked to
    // preview without subscribing gets its `preview` response and nothing else:
    // pushing a scene to a connection that never asked for notifications would
    // make the wire depend on whether a client happens to read it.
    let (server, mut subscriber, _snapshot, _version) = opened();
    let tree = subscriber.call("composition_tree", json!({}));
    let (ops, _) = tree_layers(&tree);

    // A second connection that only says hello: not a subscriber.
    let mut observer = Client::connect(server.local_addr());
    observer.call("hello", json!({"client": "observer", "protocol": 1}));

    let answer = observer.call("preview", json!({"upto": ops[0]}));
    assert_eq!(answer["upto"].as_str(), Some(ops[0].as_str()));

    // Nothing follows the response: the next line the observer can read is the
    // one a later request causes, not a snapshot.
    let id = observer.send("hello", json!({"protocol": 1}));
    let msg = observer.read_line();
    assert_eq!(
        msg["id"].as_u64(),
        Some(id),
        "the next line is the answer to the next request, so no snapshot was pushed: {msg}"
    );
}
