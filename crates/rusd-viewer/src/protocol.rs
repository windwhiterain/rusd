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
    pub version: u64,
    /// `"usdc"` or `"usda"`, per [`SNAPSHOT_FORMATS`].
    pub format: String,
    /// Base64 of the flattened layer's bytes.
    pub data: String,
}

impl Snapshot {
    /// The layer bytes this snapshot carries.
    pub fn decode(&self) -> Result<Vec<u8>, Vec<Diagnostic>> {
        if !SNAPSHOT_FORMATS.contains(&self.format.as_str()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_and_subscribe_spell_the_contract() {
        assert_eq!(
            hello_line(1),
            "{\"id\":1,\"method\":\"hello\",\"params\":{\"client\":\"rusd-viewer\",\"protocol\":1}}\n"
        );
        assert_eq!(
            subscribe_line(2),
            "{\"id\":2,\"method\":\"subscribe\",\"params\":{\"client\":\"rusd-viewer\"}}\n"
        );
    }

    #[test]
    fn opinion_translate_is_an_ops_array_of_path_field_value_op() {
        let line = opinion_line(
            7,
            &[OpinionOp::set_translate_local("/World/a", [1.5, -2.0, 3.0])],
        );
        assert!(line.ends_with('\n'));
        let v: Value = serde_json::from_str(line.trim_end()).expect("one JSON object");
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "opinion");
        let ops = v["params"]["ops"].as_array().expect("ops array");
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0]["path"], "/World/a");
        assert_eq!(ops[0]["field"], "xformOp:translate");
        assert_eq!(ops[0]["op"], "set");
        assert_eq!(ops[0]["value"], json!([1.5, -2.0, 3.0]));
    }

    #[test]
    fn remove_carries_no_value() {
        let op = OpinionOp::remove("/World/a", "xformOp:translate");
        let j = op.to_json();
        assert_eq!(j["op"], "remove");
        assert!(j.get("value").is_none(), "remove has no value: {j}");
    }

    #[test]
    fn undo_and_redo_are_distinct_methods() {
        assert!(edit_line(3, false).contains("\"method\":\"undo\""));
        assert!(edit_line(3, true).contains("\"method\":\"redo\""));
    }

    #[test]
    fn a_response_with_a_result_parses() {
        let w = parse_line("{\"id\":4,\"result\":{\"version\":9}}").expect("parses");
        match w {
            Wire::Response(r) => {
                assert_eq!(r.id, 4);
                assert_eq!(r.result.expect("ok")["version"], 9);
            }
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn a_response_with_an_error_parses_into_the_error_object() {
        let w = parse_line("{\"id\":4,\"error\":{\"code\":-32602,\"message\":\"bad params\"}}")
            .expect("parses");
        match w {
            Wire::Response(r) => {
                let err = r.result.expect_err("an error response");
                assert_eq!(err.code, -32602);
                assert_eq!(err.message, "bad params");
            }
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn malformed_lines_are_diagnostics_not_panics() {
        assert_eq!(parse_line("not json").expect_err("refused").code, "E_WIRE");
        assert_eq!(
            parse_line("{\"neither\":1}").expect_err("refused").code,
            "E_WIRE_SHAPE"
        );
        assert_eq!(
            parse_line("{\"method\":\"stage_snapshot\",\"params\":{}}")
                .expect_err("refused")
                .code,
            "E_WIRE_SHAPE"
        );
    }

    #[test]
    fn a_snapshot_decodes_to_the_layer_bytes_it_carried() {
        let bytes = b"\x50\x0a\x0b\r\x00binary".to_vec();
        let snap = Snapshot {
            version: 3,
            format: "usdc".to_owned(),
            data: crate::compose::base64_encode(&bytes),
        };
        assert_eq!(snap.decode().expect("decodes"), bytes);
    }

    #[test]
    fn a_snapshot_in_a_format_that_is_not_on_the_wire_is_refused() {
        let snap = Snapshot {
            version: 3,
            format: "usdz".to_owned(),
            data: "AAAA".to_owned(),
        };
        let diags = snap.decode().expect_err("usdz is not on the wire");
        assert_eq!(diags[0].code, "E_WIRE_FORMAT");
    }

    #[test]
    fn a_snapshot_whose_data_is_not_base64_is_refused() {
        let snap = Snapshot {
            version: 3,
            format: "usdc".to_owned(),
            data: "not base64 !!".to_owned(),
        };
        assert_eq!(snap.decode().expect_err("refused")[0].code, "E_WIRE_BASE64");
    }

    #[test]
    fn the_three_protocol_notifications_parse() {
        let snap = parse_line(
            "{\"method\":\"stage_snapshot\",\"params\":{\"version\":5,\"format\":\"usdc\",\"data\":\"AA==\"}}",
        )
        .expect("parses");
        assert_eq!(
            snap,
            Wire::Notification(Notification::Snapshot(Snapshot {
                version: 5,
                format: "usdc".to_owned(),
                data: "AA==".to_owned()
            }))
        );

        let sc = parse_line(
            "{\"method\":\"state_changed\",\"params\":{\"version\":6,\"reason\":\"reload\"}}",
        )
        .expect("parses");
        assert_eq!(
            sc,
            Wire::Notification(Notification::StateChanged {
                version: 6,
                reason: "reload".to_owned()
            })
        );

        let d =
            parse_line("{\"method\":\"diagnostics\",\"params\":{\"items\":[{\"code\":\"W_X\"}]}}")
                .expect("parses");
        match d {
            Wire::Notification(Notification::Diagnostics { items }) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0]["code"], "W_X");
            }
            other => panic!("expected diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_notification_is_kept_not_refused() {
        let w = parse_line("{\"method\":\"future_thing\",\"params\":{}}").expect("parses");
        assert_eq!(
            w,
            Wire::Notification(Notification::Other {
                method: "future_thing".to_owned()
            })
        );
    }

    #[test]
    fn hello_accepts_this_protocol_and_refuses_another() {
        let mut st = ClientState::default();
        st.on_hello(&json!({"protocol":1,"session":"s1","version":4}))
            .expect("protocol 1 is ours");
        assert_eq!(st.session.as_deref(), Some("s1"));
        assert_eq!(st.max_version, Some(4));

        let mut st = ClientState::default();
        let e = st
            .on_hello(&json!({"protocol":2,"session":"s1","version":1}))
            .expect_err("protocol 2 is another build");
        assert_eq!(e.code, "E_WIRE_PROTOCOL");

        let mut st = ClientState::default();
        assert_eq!(
            st.on_hello(&json!({"session":"s1"}))
                .expect_err("no protocol number")
                .code,
            "E_WIRE_PROTOCOL"
        );
    }

    #[test]
    fn a_version_at_or_below_the_greatest_seen_is_discarded() {
        let mut st = ClientState::default();
        assert!(st.accept(2), "first version accepted");
        assert!(!st.accept(2), "the same version again is discarded");
        assert!(!st.accept(1), "an older version is discarded");
        assert!(st.accept(3), "a newer version is accepted");
        assert_eq!(st.max_version, Some(3));
        assert_eq!(st.accepted, 0, "accepted counts snapshots, not versions");
        assert_eq!(st.discarded, 2);
    }

    #[test]
    fn a_hello_version_does_not_count_as_a_snapshot_but_raises_the_floor() {
        let mut st = ClientState::default();
        st.on_hello(&json!({"protocol":1,"version":7}))
            .expect("hello");
        assert!(!st.accept(7), "a snapshot at hello's version is stale");
        assert!(st.accept(8));
        assert_eq!(st.discarded, 1);
    }

    #[test]
    fn live_needs_both_hello_and_subscribe() {
        let mut st = ClientState::default();
        assert!(!st.is_live());
        st.on_hello(&json!({"protocol":1,"version":1}))
            .expect("hello");
        assert!(!st.is_live(), "hello alone is not live");
        st.subscribed = true;
        assert!(st.is_live());
    }

    #[test]
    fn a_throttle_at_30hz_lets_at_most_one_send_per_33ms() {
        let mut t = Throttle::hz(30.0);
        assert_eq!(t.interval(), Duration::from_secs_f64(1.0 / 30.0));
        let t0 = Instant::now();
        assert!(t.ready(t0), "the first send is immediate");
        assert!(!t.ready(t0 + Duration::from_millis(16)), "half an interval");
        assert!(t.ready(t0 + Duration::from_millis(34)), "a full interval");
        assert!(
            !t.ready(t0 + Duration::from_millis(34) + Duration::from_millis(10)),
            "the interval restarts from the accepted send"
        );
        t.reset();
        assert!(
            t.ready(t0 + Duration::from_millis(40)),
            "a new drag sends at once"
        );
    }

    #[test]
    fn one_thousand_drag_events_in_one_second_send_at_most_thirty_one_opinions() {
        let mut t = Throttle::hz(30.0);
        let t0 = Instant::now();
        let mut sent = 0usize;
        // 1000 events spread over one second (1 ms apart).
        for i in 0..1000 {
            if t.ready(t0 + Duration::from_micros(i * 1000)) {
                sent += 1;
            }
        }
        assert!(
            (30..=31).contains(&sent),
            "1000 events in 1s at 30Hz send ~30 opinions, sent {sent}"
        );
    }
}
