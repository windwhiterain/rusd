//! The client half of D12 wire protocol v1 (`docs/PROTOCOL.md`), pure encoding
//! and decoding: no socket, no threads, no GPU, so every rule the contract
//! states is testable on its own.
//!
//! The wire is NDJSON JSON-RPC over TCP: one JSON object per line, a request
//! `{"id":n,"method":m,"params":{…}}`, a response `{"id":n,"result":{…}}` or
//! `{"id":n,"error":{"code":c,"message":s}}`, and a notification
//! `{"method":m,"params":{…}}` with no id. This module builds the requests the
//! viewer sends (`hello`, `subscribe`, `opinion`, `undo`, `redo`) and parses
//! everything the server sends back, including the two rules a client must not
//! get wrong:
//!
//! - **`version` is a monotonic u64** and the client discards any snapshot or
//!   notification whose version is not greater than the greatest it has seen
//!   ([`ClientState::accept`]).
//! - **`stage_snapshot` is a self-sufficient flattened `usdc`** carried as
//!   base64 ([`Snapshot::decode`]) — Tier A, so the client never asks for
//!   another layer and never learns an anonymous identifier.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use rusd_core::Diagnostic;
use serde_json::{Value, json};

use crate::diag;

/// The protocol number this client speaks (`hello{protocol:1}`).
pub const PROTOCOL: u32 = 1;

/// The `client` field every request carries: a stable label, not a version, so
/// two viewers of different builds are still distinguishable by eye in a log.
pub const CLIENT_NAME: &str = "rusd-viewer";

/// The snapshot encodings v1 puts on the wire. `usdz` is deliberately absent
/// (REMOTE §8: a zip container's timestamps break reproducibility).
pub const SNAPSHOT_FORMATS: [&str; 2] = ["usdc", "usda"];

/// One `{"id":n,"method":m,"params":{…}}` line, newline included.
pub fn request_line(id: u64, method: &str, params: Value) -> String {
    let mut line = json!({"id": id, "method": method, "params": params}).to_string();
    line.push('\n');
    line
}

/// The viewer's `hello`: its label and the protocol it speaks.
pub fn hello_line(id: u64) -> String {
    request_line(
        id,
        "hello",
        json!({"client": CLIENT_NAME, "protocol": PROTOCOL}),
    )
}

/// `subscribe{client}`: from the reply on, this connection receives
/// notifications.
pub fn subscribe_line(id: u64) -> String {
    request_line(id, "subscribe", json!({"client": CLIENT_NAME}))
}

/// How an opinion is applied to the server's overrides session layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Author `value` at `field` on `path`.
    Set,
    /// Drop the opinion at `field`.
    Remove,
}

impl Op {
    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Set => "set",
            Op::Remove => "remove",
        }
    }
}

/// One entry of `opinion{ops:[…]}`: where an opinion lands, on which field,
/// with which value.
///
/// `value` is the **field's Sdf value, written as-is** into the overrides session
/// layer (`docs/PROTOCOL.md` "v1 实现语义补记", ruling of 2026-11-17): the server
/// reads the composed attribute's declared type and converts the JSON into that
/// type, then authors it as the prim's own opinion. It is *not* a world-space
/// convenience the server interprets — so for an `xformOp:translate` it is the
/// **prim's local** op value, which is what keeps a prim under a transformed
/// parent inside its parent (a world value would double-apply the parent's
/// translate: the "原点加两遍" bug pattern).
///
/// The one shape this client sends is [`OpinionOp::set_translate_local`]'s — three
/// numbers for `xformOp:translate` — and turning a gizmo's world-space target into
/// that local value is this client's job (`crate::gizmo`).
#[derive(Clone, Debug, PartialEq)]
pub struct OpinionOp {
    pub path: String,
    pub field: String,
    pub value: Value,
    pub op: Op,
}

impl OpinionOp {
    /// Author `path`'s `xformOp:translate` as `translate` — the op value in the
    /// prim's **local** space (its parent's coordinate system), not world space.
    ///
    /// A caller holding a world position must convert first
    /// (`crate::gizmo::world_to_local`), because the server writes this value as
    /// the op itself.
    pub fn set_translate_local(path: impl Into<String>, translate: [f64; 3]) -> Self {
        Self {
            path: path.into(),
            field: "xformOp:translate".to_owned(),
            value: json!([translate[0], translate[1], translate[2]]),
            op: Op::Set,
        }
    }

    /// Drop `field`'s opinion on `path`.
    pub fn remove(path: impl Into<String>, field: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            field: field.into(),
            value: Value::Null,
            op: Op::Remove,
        }
    }

    /// This op as the JSON object the wire carries.
    pub fn to_json(&self) -> Value {
        let mut obj = BTreeMap::new();
        obj.insert("path", json!(self.path));
        obj.insert("field", json!(self.field));
        obj.insert("op", json!(self.op.as_str()));
        if self.op == Op::Set {
            obj.insert("value", self.value.clone());
        }
        // BTreeMap keeps the keys in one deterministic order, so the same op
        // always spells the same line.
        Value::Object(obj.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
    }
}

/// `opinion{ops:[…]}` — the viewer's only edit, and it lands in the server's
/// overrides session layer (D12 §3), never in the document CST.
pub fn opinion_line(id: u64, ops: &[OpinionOp]) -> String {
    let ops: Vec<Value> = ops.iter().map(OpinionOp::to_json).collect();
    request_line(id, "opinion", json!({"ops": ops}))
}

/// `preview{upto}`: ask this connection to be fed the **prefix** composition up to
/// one cook op layer, or the final one.
///
/// `upto` is a cook op's output layer identifier, exactly as `composition_tree`'s
/// `layers[]` prints it (v1.4: `preview{upto: <layer identifier> | null}`, 按连接维护
/// 预览态). `None` is the final composition, i.e. the protocol's `null`.
///
/// The preview does **not** move the scene version: it is a view of the same scene, not
/// an edit of it. So the request is one of the panel's [`NetCommand::Call`]s, answered
/// with `{version}` (or an error for an unknown `upto`, `-32602`), rather than an edit
/// reported as [`crate::net::NetEvent::Applied`].
///
/// # The one place the `null` is written
///
/// `params` always carries the key, `null` included: `{"upto": null}` is "final" and
/// `{}` would be an omission the server has to guess about. A client that leaves it out
/// on the last click would leave the server's preview state where it was.
pub fn preview_line(id: u64, upto: Option<&str>) -> String {
    request_line(
        id,
        "preview",
        json!({"upto": upto.map(|u| Value::String(u.to_owned())).unwrap_or(Value::Null)}),
    )
}

/// The `params` object [`preview_line`] writes, as data — what a test asserts and
/// what [`crate::net::NetCommand::Call`] carries.
pub fn preview_params(upto: Option<&str>) -> Value {
    json!({"upto": upto.map(|u| Value::String(u.to_owned())).unwrap_or(Value::Null)})
}

/// `preview{solo}`: ask for **one layer's content and nothing else** — every other layer
/// muted, the session layer excepted ([`preview_params`]'s prefix is the other
/// composition, `preview{upto}`).
///
/// This is the material (reference / plain sublayer) row's click: an op or root row asks
/// for `upto`, which is a *cook prefix*, and a document's own sublayers are not that —
/// they are only reachable by showing them alone. Which of the two a row sends is the
/// tree's to say (`crate::tree::TreeNode::op` / `root`), never a list of layer names
/// kept anywhere else.
///
/// The reply and the snapshot follow [`preview_params`]'s rules exactly: the scene
/// arrives as a `stage_snapshot` whose `upto` names this layer, the version does not
/// move, and an unknown id is `-32602`.
pub fn preview_solo_params(layer: &str) -> Value {
    json!({"solo": layer})
}

/// `undo` / `redo`: server-side, over both edit surfaces.
pub fn edit_line(id: u64, redo: bool) -> String {
    request_line(
        id,
        if redo { "redo" } else { "undo" },
        Value::Object(Default::default()),
    )
}

/// A `stage_snapshot` notification's payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// Monotonic u64; the client discards a version it has already passed.
    ///
    /// **A preview frame does not move it** (`docs/PROTOCOL.md` v1.4: `preview` 不推进
    /// scene version), so the same version can arrive twice — once as a prefix and once
    /// as the final frame. That is why the preview state, not the version, decides
    /// whether a repeated version is a new frame ([`crate::session::ClientModel::apply`]).
    pub version: u64,
    /// `"usdc"` or `"usda"`, per [`SNAPSHOT_FORMATS`].
    pub format: String,
    /// Base64 of the flattened layer's bytes.
    pub data: String,
    /// The cook-op layer this frame was composed up to, or `None` for the final
    /// composition (v1.4: `stage_snapshot` 增可选字段 `upto`，缺省 = final).
    ///
    /// It is what the viewport is showing, so the status bar and the overlay say it;
    /// the client never *derives* it, because which ops a prefix contains is the
    /// server's composition decision, not something a flattened layer can reveal.
    pub upto: Option<String>,
}

impl Snapshot {
    /// Whether this frame is a prefix (composed up to one cook op) rather than the
    /// final composition.
    pub fn is_prefix(&self) -> bool {
        self.upto.is_some()
    }

    /// The layer bytes this snapshot carries.
    pub fn decode(&self) -> Result<Vec<u8>, Vec<Diagnostic>> {
        if !SNAPSHOT_FORMATS.contains(&self.format.as_str()) {
            // (prefix frames obey the same format rule as final ones)
            return Err(vec![diag(
                "E_WIRE_FORMAT",
                format!(
                    "stage_snapshot format {:?} is not one of {:?}",
                    self.format, SNAPSHOT_FORMATS
                ),
            )]);
        }
        crate::compose::base64_decode(&self.data).ok_or_else(|| {
            vec![diag(
                "E_WIRE_BASE64",
                format!(
                    "stage_snapshot v{}: data is not valid base64 ({} chars)",
                    self.version,
                    self.data.len()
                ),
            )]
        })
    }
}

/// A JSON-RPC error object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// The code a request's failure carries when **this client** gave up waiting for
/// its reply, rather than the server refusing it.
///
/// It is outside JSON-RPC's reserved range on purpose: a panel that degrades on
/// `-32601 method not found` must not mistake a timeout for a missing method, and
/// the tree panel reports it as 「服务器未在时限内应答」 instead of 「合成树不可用
/// （server 版本）」. `tests/startup.rs` asserts both.
pub const TIMEOUT_CODE: i64 = -32001;

/// A reply to one of this client's requests.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub id: u64,
    pub result: Result<Value, RpcError>,
}

/// Everything a server sends that is not a reply to a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notification {
    /// A whole flattened stack, self-sufficient (Tier A).
    Snapshot(Snapshot),
    /// Why the server re-composed: `open` / `patch` / `opinion` / `undo` /
    /// `redo` / `reload`.
    StateChanged { version: u64, reason: String },
    /// Cook / validate diagnostics.
    Diagnostics { items: Vec<Value> },
    /// A notification this client does not interpret (the server may add one);
    /// kept, not refused, so a newer server does not break an older viewer.
    Other { method: String },
}

/// One parsed line.
#[derive(Clone, Debug, PartialEq)]
pub enum Wire {
    Response(Response),
    Notification(Notification),
}

/// Parse one NDJSON line into the request/notification it carries.
///
/// `result_large_err` is the repo's own allowance (`rusd_core::syntax` has the
/// same one): a `Diagnostic` is the error type every crate reports, and boxing it
/// here would only make the caller unwrap a box to read a code.
#[allow(clippy::result_large_err)]
pub fn parse_line(line: &str) -> Result<Wire, Diagnostic> {
    let v: Value = serde_json::from_str(line)
        .map_err(|e| diag("E_WIRE", format!("not JSON: {e}: {}", truncate(line, 160))))?;
    let obj = v.as_object().ok_or_else(|| {
        diag(
            "E_WIRE_SHAPE",
            "a wire line must be a JSON object".to_owned(),
        )
    })?;

    if let Some(id) = obj.get("id").and_then(Value::as_u64) {
        return Ok(Wire::Response(parse_response(id, obj)));
    }
    let method = obj.get("method").and_then(Value::as_str).ok_or_else(|| {
        diag(
            "E_WIRE_SHAPE",
            format!(
                "a wire line must carry \"id\" or \"method\": {}",
                truncate(line, 160)
            ),
        )
    })?;
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    Ok(Wire::Notification(parse_notification(method, &params)?))
}

fn parse_response(id: u64, obj: &serde_json::Map<String, Value>) -> Response {
    let result = match (obj.get("result"), obj.get("error")) {
        (Some(r), _) => Ok(r.clone()),
        (None, Some(err)) => Err(RpcError {
            code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("an error with no message")
                .to_owned(),
        }),
        (None, None) => Err(RpcError {
            code: 0,
            message: "a response with neither result nor error".to_owned(),
        }),
    };
    Response { id, result }
}

#[allow(clippy::result_large_err)]
fn parse_notification(method: &str, params: &Value) -> Result<Notification, Diagnostic> {
    match method {
        "stage_snapshot" => {
            let version = params
                .get("version")
                .and_then(Value::as_u64)
                .ok_or_else(|| diag("E_WIRE_SHAPE", "stage_snapshot needs a u64 \"version\""))?;
            let format = params
                .get("format")
                .and_then(Value::as_str)
                .ok_or_else(|| diag("E_WIRE_SHAPE", "stage_snapshot needs a \"format\""))?;
            let data = params
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| diag("E_WIRE_SHAPE", "stage_snapshot needs \"data\""))?;
            Ok(Notification::Snapshot(Snapshot {
                version,
                format: format.to_owned(),
                data: data.to_owned(),
                // v1.4's optional field: absent (or `null`) is the final composition.
                // A *present* value must be a string — a number here is a server
                // bug, and treating it as "final" would silently show the wrong frame.
                upto: match params.get("upto") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(other) => {
                        return Err(diag(
                            "E_WIRE_SHAPE",
                            format!(
                                "stage_snapshot \"upto\" must be a string or null, not {other}"
                            ),
                        ));
                    }
                },
            }))
        }
        "state_changed" => {
            let version = params
                .get("version")
                .and_then(Value::as_u64)
                .ok_or_else(|| diag("E_WIRE_SHAPE", "state_changed needs a u64 \"version\""))?;
            let reason = params
                .get("reason")
                .and_then(Value::as_str)
                .ok_or_else(|| diag("E_WIRE_SHAPE", "state_changed needs a \"reason\""))?;
            Ok(Notification::StateChanged {
                version,
                reason: reason.to_owned(),
            })
        }
        "diagnostics" => {
            let items = params
                .get("items")
                .and_then(Value::as_array)
                .map(|a| a.to_vec())
                .unwrap_or_default();
            Ok(Notification::Diagnostics { items })
        }
        other => Ok(Notification::Other {
            method: other.to_owned(),
        }),
    }
}

/// What the client knows about the server, and the two rules it enforces on
/// everything that arrives (protocol number, monotonic version).
#[derive(Clone, Debug, Default)]
pub struct ClientState {
    /// The protocol the server answered with in `hello`.
    pub protocol: Option<u32>,
    /// The server's session label, from `hello`.
    pub session: Option<String>,
    /// Whether `subscribe` was answered.
    pub subscribed: bool,
    /// The greatest version seen; `None` before the first accepted one.
    pub max_version: Option<u64>,
    /// The last `state_changed` reason seen, for the status bar and the
    /// hot-reload badge.
    pub last_reason: Option<String>,
    /// How many snapshots were accepted.
    pub accepted: u64,
    /// How many were discarded for a version already passed.
    pub discarded: u64,
}

impl ClientState {
    /// Take `hello`'s answer: the server must speak [`PROTOCOL`]. The version
    /// it reports seeds the monotonic counter without counting as a snapshot.
    #[allow(clippy::result_large_err)]
    pub fn on_hello(&mut self, result: &Value) -> Result<(), Diagnostic> {
        let protocol = result.get("protocol").and_then(Value::as_u64);
        match protocol {
            Some(p) if p == u64::from(PROTOCOL) => {}
            Some(p) => {
                return Err(diag(
                    "E_WIRE_PROTOCOL",
                    format!(
                        "the server speaks protocol {p}, this viewer speaks protocol {PROTOCOL}"
                    ),
                ));
            }
            None => {
                return Err(diag(
                    "E_WIRE_PROTOCOL",
                    "hello answered no \"protocol\" number".to_owned(),
                ));
            }
        }
        self.protocol = Some(PROTOCOL);
        self.session = result
            .get("session")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(v) = result.get("version").and_then(Value::as_u64) {
            self.note_version(v);
        }
        Ok(())
    }

    /// Note a version without counting a snapshot (hello, or a `state_changed`
    /// that arrives before any snapshot).
    pub fn note_version(&mut self, version: u64) {
        self.max_version = Some(self.max_version.map_or(version, |m| m.max(version)));
    }

    /// Whether `version` is newer than everything seen, and if so, remember it.
    /// The false answer is the protocol's discard rule: a snapshot or
    /// notification that is not newer than the greatest version seen is
    /// dropped, so a late duplicate can never roll the view back.
    pub fn accept(&mut self, version: u64) -> bool {
        if self.max_version.is_some_and(|m| version <= m) {
            self.discarded += 1;
            return false;
        }
        self.max_version = Some(version);
        true
    }

    /// Whether a **`state_changed`** at `version` is one to act on: false only when it is
    /// older than everything seen.
    ///
    /// Not [`ClientState::accept`], and the difference is the whole point: the server
    /// sends `stage_snapshot{version:n}` and then `state_changed{version:n}` — **the same
    /// version**, that is what makes the reason describe the scene just pushed
    /// (`docs/PROTOCOL.md` 「消息顺序」). A strict `>` rule therefore dropped *every*
    /// `state_changed` on a real server: the snapshot had already raised the counter to
    /// `n`, so the reason that followed it read as a duplicate. It went unnoticed because
    /// every test of a reason fed it a version no snapshot had carried — this defect was
    /// found by the mock that sends both, in `tests/state_change_v142.rs`.
    ///
    /// An equal version is safe to take here where it is not safe for a snapshot: a
    /// `state_changed` carries no scene, so acting on it cannot roll the view back, and
    /// it does not move the counter either (`note_version` is a `max`). An **older** one
    /// still is a duplicate — a reason for a scene the view has passed says nothing about
    /// this one.
    pub fn accept_state_change(&mut self, version: u64) -> bool {
        if self.max_version.is_some_and(|m| version < m) {
            self.discarded += 1;
            return false;
        }
        self.note_version(version);
        true
    }

    /// Whether the viewer knows it is live: `hello` answered with our protocol
    /// and `subscribe` was granted.
    pub fn is_live(&self) -> bool {
        self.protocol == Some(PROTOCOL) && self.subscribed
    }
}

/// A rate limit for an interactive drag, as `docs/PROTOCOL.md` requires for a
/// gizmo: opinions go out at most this often, and the final one is the caller's
/// job (a drag end always sends, so the last position is never lost).
#[derive(Clone, Debug)]
pub struct Throttle {
    min_interval: Duration,
    last: Option<Instant>,
}

impl Throttle {
    /// A throttle that lets one call through per `1 / hz` seconds.
    pub fn hz(hz: f64) -> Self {
        let secs = if hz > 0.0 { 1.0 / hz } else { 0.0 };
        Self {
            min_interval: Duration::from_secs_f64(secs),
            last: None,
        }
    }

    /// Whether a send is allowed now; a `true` answer starts the next interval.
    pub fn ready(&mut self, now: Instant) -> bool {
        match self.last {
            Some(last) if now.duration_since(last) < self.min_interval => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }

    /// Forget the last send, so the next `ready` is immediate (a new drag).
    pub fn reset(&mut self) {
        self.last = None;
    }

    /// The interval this throttle enforces.
    pub fn interval(&self) -> Duration {
        self.min_interval
    }
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}
