//! Integration test for the D12 v1 transport (`rusd serve --listen`,
//! `docs/PROTOCOL.md`): a real TCP client against a server thread bound to an
//! ephemeral port.
//!
//! The client half is deliberately the viewer's shape — it decodes the base64
//! `stage_snapshot`, composes the bytes with [`openusd::sdf::Layer`] over an
//! in-memory resolver, and reads geometry out of the composed stage — so the
//! test proves the snapshot is self-sufficient (Tier A) rather than trusting the
//! server's own diagnostics.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;

use openusd::ar::{Asset, AssetBuffer, ResolvedPath, Resolver, SharedBuffer};
use openusd::sdf::{self, Layer};
use openusd::usd::Stage;
use rusd_agent::transport::{Server, base64_decode};
use serde_json::{Value, json};

const CITY: &str = include_str!("../../../examples/city.rusd");

// --- a client ------------------------------------------------------------

/// A TCP client that keeps every line it has read, so assertions can look at
/// both responses and notifications in the order the server wrote them.
struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    next_id: u64,
    /// Lines read but not yet consumed, oldest first.
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

    /// The response to `id`, taking it from the inbox or the wire. Notifications
    /// met on the way are kept, not dropped.
    fn result(&mut self, id: u64) -> Value {
        loop {
            let msg = match self.inbox.pop_front() {
                Some(m) => m,
                None => self.read_line(),
            };
            if msg["id"].as_u64() == Some(id) && msg.get("result").is_some() {
                return msg["result"].clone();
            }
            if msg["id"].as_u64() == Some(id) {
                panic!("request {id} failed: {}", msg["error"]);
            }
            self.inbox.push_back(msg);
        }
    }

    /// The next notification (a message with no `id`).
    fn notification(&mut self) -> Value {
        match self.inbox.pop_front() {
            Some(msg) if msg.get("id").is_none_or(Value::is_null) => msg,
            Some(msg) => {
                self.inbox.push_front(msg);
                let msg = self.read_line();
                assert!(
                    msg.get("id").is_none_or(Value::is_null),
                    "expected a notification, got {msg}"
                );
                msg
            }
            None => {
                let msg = self.read_line();
                assert!(
                    msg.get("id").is_none_or(Value::is_null),
                    "expected a notification, got {msg}"
                );
                msg
            }
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        self.result(id)
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

// --- the client-side mirror (Tier A) -------------------------------------

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

/// What a client keeps: the newest snapshot's `version` and the stage composed
/// from it.
struct Mirror {
    version: u64,
    stage: Option<Stage>,
}

impl Mirror {
    fn new() -> Self {
        Mirror {
            version: 0,
            stage: None,
        }
    }

    /// Compose a `stage_snapshot`'s params. Returns `false` — leaving the
    /// mirror untouched — when the snapshot is not newer than what it holds
    /// (PROTOCOL §规则: drop a `version` not greater than the largest seen).
    fn apply(&mut self, snapshot: &Value) -> bool {
        let version = snapshot["version"]
            .as_u64()
            .expect("a snapshot has a version");
        if version <= self.version && self.stage.is_some() {
            return false;
        }
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
        let layer =
            Layer::from_bytes("snapshot.usdc", bytes.clone()).expect("the snapshot decodes");
        let resolver = OneLayer {
            identifier: layer.identifier().to_owned(),
            bytes: Arc::from(bytes.into_boxed_slice()),
        };
        let stage = Stage::builder()
            .resolver(resolver)
            .open(layer.identifier())
            .expect("the snapshot composes on its own");
        self.version = version;
        self.stage = Some(stage);
        true
    }

    fn stage(&self) -> &Stage {
        self.stage.as_ref().expect("a snapshot was applied")
    }

    /// World-space bbox of the composed snapshot, via the same extraction the
    /// renderer uses.
    fn bounds(&self) -> ([f64; 3], [f64; 3]) {
        rusd_scene::geometry::extract(self.stage()).bounds
    }

    /// A prim's composed `xformOp:translate`, if the snapshot has that prim.
    fn translate(&self, prim: &str) -> Option<[f32; 3]> {
        let path = sdf::path(prim).ok()?;
        let p = self.stage().prim(path).ok()?;
        match p.attribute("xformOp:translate").get::<sdf::Value>().ok()? {
            Some(sdf::Value::Vec3f(v)) => Some([v[0], v[1], v[2]]),
            _ => None,
        }
    }

    fn prim_count(&self) -> usize {
        let mut n = 0;
        let _ = self
            .stage()
            .traverse(openusd::usd::PrimPredicate::ALL, |_| n += 1);
        n
    }
}

fn bounds_differ(a: ([f64; 3], [f64; 3]), b: ([f64; 3], [f64; 3]), tol: f64) -> bool {
    (0..3).any(|k| (a.0[k] - b.0[k]).abs() > tol || (a.1[k] - b.1[k]).abs() > tol)
}

/// A server on an ephemeral loopback port plus a subscribed client.
fn subscribed() -> (Server, Client, Mirror) {
    let server = Server::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let mut client = Client::connect(server.local_addr());
    client.call("hello", json!({"client": "test", "protocol": 1}));
    let sub = client.call("subscribe", json!({"client": "test"}));
    assert_eq!(sub["version"].as_u64(), Some(0), "nothing is open yet");
    (server, client, Mirror::new())
}

/// A city opened and mirrored, with the version it published.
fn opened() -> (Server, Client, Mirror, u64) {
    let (server, mut client, mut mirror) = subscribed();
    let (_, snapshot, state) = client.mutate("open", json!({"content": CITY}));
    assert!(mirror.apply(&snapshot), "the first snapshot is accepted");
    assert_eq!(state["version"].as_u64(), Some(mirror.version));
    let version = mirror.version;
    (server, client, mirror, version)
}

// --- tests ----------------------------------------------------------------

#[test]
fn hello_reports_the_protocol_and_the_session() {
    let (_server, mut client, _mirror) = subscribed();
    let hello = client.call("hello", json!({"client": "test", "protocol": 1}));
    assert_eq!(hello["protocol"].as_u64(), Some(1));
    assert!(
        hello["session"]
            .as_str()
            .unwrap_or_default()
            .starts_with("rusd-serve/")
    );
    // A protocol mismatch is refused rather than served.
    let id = client.send("hello", json!({"client": "old", "protocol": 0}));
    let msg = client.read_line();
    assert_eq!(msg["id"].as_u64(), Some(id));
    assert_eq!(msg["error"]["code"].as_i64(), Some(-32602), "{msg}");
}

#[test]
fn unknown_methods_and_malformed_lines_are_json_rpc_errors() {
    let (_server, mut client, _mirror) = subscribed();
    let id = client.send("teleport", json!({}));
    let msg = client.read_line();
    assert_eq!(msg["error"]["code"].as_i64(), Some(-32601), "{msg}");
    assert_eq!(msg["id"].as_u64(), Some(id));

    writeln!(client.writer, "{{not json").unwrap();
    client.writer.flush().unwrap();
    let msg = client.read_line();
    assert_eq!(msg["error"]["code"].as_i64(), Some(-32700), "{msg}");
    assert!(msg["id"].is_null());

    // The connection survives both, and still answers.
    let hello = client.call("hello", json!({"protocol": 1}));
    assert_eq!(hello["protocol"].as_u64(), Some(1));
}

#[test]
fn open_publishes_a_self_sufficient_usdc_snapshot() {
    let (server, _client, mirror, _v1) = opened();

    // The snapshot carries the cooked city: hundreds of prims, a real bbox.
    let (min, max) = mirror.bounds();
    assert!(max[0] > min[0] && max[1] > min[1], "bbox {min:?}..{max:?}");
    assert!(
        mirror.prim_count() > 400,
        "the snapshot holds the cooked city, got {}",
        mirror.prim_count()
    );

    // A late subscriber gets the current snapshot on subscribing, not only on
    // the next mutation: the protocol defines no snapshot fetch.
    let mut late = Client::connect(server.local_addr());
    late.call("hello", json!({"client": "late", "protocol": 1}));
    let sub = late.call("subscribe", json!({"client": "late"}));
    assert_eq!(sub["version"].as_u64(), Some(mirror.version));
    let mut late_mirror = Mirror::new();
    let snapshot = late.notification()["params"].clone();
    assert!(late_mirror.apply(&snapshot));
    assert_eq!(late_mirror.version, mirror.version);
}

#[test]
fn patch_bumps_the_version_and_the_client_side_bbox() {
    let (_server, mut client, mut mirror, v1) = opened();
    let before = mirror.bounds();

    // Halve the block grid: the city's extent shrinks, so the composed bbox
    // must move.
    let (applied, snapshot, state) = client.mutate(
        "patch",
        json!({"ops": [{"op": "set", "prim": "/World/City", "property": "blocks_x", "value": 3}]}),
    );
    assert_eq!(applied["applied"].as_u64(), Some(1));
    let v2 = snapshot["version"].as_u64().unwrap();
    assert_eq!(v2, v1 + 1, "one mutation, one version");
    assert!(mirror.apply(&snapshot));
    assert_eq!(state["reason"].as_str(), Some("patch"));
    assert_eq!(state["version"].as_u64(), Some(v2));

    let after = mirror.bounds();
    assert!(
        bounds_differ(before, after, 1.0),
        "the patch must change the composed bbox: {before:?} -> {after:?}"
    );
    assert!(
        after.1[0] < before.1[0],
        "a 3-block city is narrower in X: {before:?} -> {after:?}"
    );
}

#[test]
fn opinion_override_and_undo_reach_the_snapshot() {
    let (_server, mut client, mut mirror, v1) = opened();

    // Pick a real lot out of the cooked city, so the test does not depend on
    // which lots the seed happened to occupy.
    let query = client.call("query", json!({}));
    let lot = query["prims"]
        .as_array()
        .expect("query lists prims")
        .iter()
        .filter_map(|p| p["path"].as_str())
        .find(|p| p.contains("/City/") && p.contains("/lot"))
        .map(str::to_owned)
        .expect("the cooked city has building lots");
    let original = mirror.translate(&lot).expect("the lot has a translate");

    let target = [1234.0, 567.0, 8.0];
    let (opinion, snapshot, state) = client.mutate(
        "opinion",
        json!({"ops": [{
            "path": lot,
            "field": "xformOp:translate",
            "value": target,
            "op": "set"
        }]}),
    );
    assert_eq!(opinion["version"].as_u64(), Some(v1 + 1));
    assert!(mirror.apply(&snapshot));
    assert_eq!(state["reason"].as_str(), Some("opinion"));
    assert_eq!(
        mirror.translate(&lot),
        Some(target),
        "the session-layer opinion overrode the cooked translate"
    );

    // Undo restores the previous overrides (none), so the cooked value is back.
    let (undone, snapshot, state) = client.mutate("undo", json!({}));
    assert_eq!(undone["ok"].as_bool(), Some(true));
    assert!(mirror.apply(&snapshot));
    assert_eq!(state["reason"].as_str(), Some("undo"));
    assert_eq!(
        mirror.translate(&lot),
        Some(original),
        "undo restored the cooked value"
    );

    // Redo puts the opinion back.
    let (_, snapshot, state) = client.mutate("redo", json!({}));
    assert!(mirror.apply(&snapshot));
    assert_eq!(state["reason"].as_str(), Some("redo"));
    assert_eq!(mirror.translate(&lot), Some(target));

    // The overrides are not lost when the document is rebuilt: a later patch
    // replays them onto the new stage.
    let (_, snapshot, _) = client.mutate(
        "patch",
        json!({"ops": [{"op": "set", "prim": "/World/City", "property": "occupancy", "value": 0.5}]}),
    );
    assert!(mirror.apply(&snapshot));
    assert_eq!(
        mirror.translate(&lot),
        Some(target),
        "a document mutation replays the session-layer overrides"
    );
}

#[test]
fn versions_are_monotonic_and_a_stale_snapshot_is_dropped() {
    let (_server, mut client, mut mirror, v1) = opened();

    let (_, second, _) = client.mutate(
        "opinion",
        json!({"ops": [{"path": "/World", "field": "xformOp:translate", "value": [1.0, 2.0, 3.0], "op": "set"}]}),
    );
    let v2 = second["version"].as_u64().unwrap();
    assert!(v2 > v1, "versions increase: {v1} then {v2}");
    assert!(mirror.apply(&second));
    let bounds = mirror.bounds();

    // Replaying an older snapshot — a reordered or duplicated notification —
    // must not roll the client back.
    let stale = json!({"version": v1, "format": "usdc", "data": ""});
    assert!(!mirror.apply(&stale), "a stale version is dropped");
    assert_eq!(mirror.version, v2);
    assert_eq!(
        mirror.bounds(),
        bounds,
        "the dropped snapshot changed nothing"
    );
    // Replaying the current one is a no-op too.
    assert!(!mirror.apply(&second));
}

#[test]
fn unsubscribe_stops_the_notifications() {
    let (_server, mut client, mirror, _v1) = opened();

    let done = client.call("unsubscribe", json!({"client": "test"}));
    assert_eq!(done["ok"].as_bool(), Some(true));

    // A mutation still answers, but nothing is broadcast to this client: the
    // version moves on the server, and the very next line the client reads is
    // the response to its own call.
    let id = client.send(
        "opinion",
        json!({"ops": [{"path": "/World", "field": "xformOp:translate", "value": [9.0, 9.0, 9.0], "op": "set"}]}),
    );
    let msg = client.read_line();
    assert_eq!(msg["id"].as_u64(), Some(id), "no snapshot arrived: {msg}");
    assert!(msg.get("result").is_some());
    let version = msg["result"]["version"].as_u64().unwrap();
    assert_eq!(version, mirror.version + 1, "the server did advance");
}

#[test]
fn a_tool_call_fails_cleanly_before_open() {
    let (_server, mut client, _mirror) = subscribed();
    let id = client.send("query", json!({}));
    let msg = client.read_line();
    assert_eq!(msg["id"].as_u64(), Some(id));
    assert_eq!(msg["error"]["code"].as_i64(), Some(-32000), "{msg}");
    assert!(
        msg["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("open"),
        "{msg}"
    );
    // An opinion with no stage is an error too, not a silent version bump.
    let id = client.send(
        "opinion",
        json!({"ops": [{"path": "/World", "field": "a", "value": 1.0, "op": "set"}]}),
    );
    let msg = client.read_line();
    assert_eq!(msg["id"].as_u64(), Some(id));
    assert_eq!(msg["error"]["code"].as_i64(), Some(-32000), "{msg}");
}
