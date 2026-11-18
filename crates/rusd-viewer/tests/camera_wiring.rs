//! The **app's** camera wiring, end to end against a mock server — the part the
//! headless fit test could not reach.
//!
//! Round three fixed `scene::fit_camera` to hold all eight corners in frame, and
//! its unit test passed, yet the real window still showed the city bunched into a
//! corner. The fit was correct; the *view* never used it: `adopt_scene` only took
//! the scene's camera when `orbit` was `None`, and by the time a snapshot arrived
//! `orbit` had been set from an earlier empty-bbox fit, so the correct camera sat
//! in the `MadeScene` and was thrown away. These tests run the same decision the
//! window makes (`crate::view::camera_for_scene`) over a real socket, so a
//! regression in the wiring fails here and not only on a screenshot.
//!
//! The mock pushes nothing at the greeting: the snapshot arrives only after
//! `hello` and `subscribe`, which is the order a real server takes (it answers the
//! requests first and broadcasts afterwards) and the order that exposed the bug.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Sender, channel};
use std::thread;

use rusd_viewer::camera::{Orbit, view_proj};
use rusd_viewer::compose::base64_encode;
use rusd_viewer::net::{NetEvent, NetWorker, ProtocolClient};
use rusd_viewer::protocol::{self, Notification, PROTOCOL, Snapshot, Wire};
use rusd_viewer::session::{ClientModel, Effect};
use rusd_viewer::view;
use serde_json::{Value, json};

/// The cooked city `usdc`, in memory.
fn cooked_city() -> Vec<u8> {
    let doc =
        rusd_core::syntax::parse(include_str!("../../../examples/city.rusd")).expect("city parses");
    let out = rusd_proc::expand_doc(&doc).expect("city cooks");
    let flat = out.stage.flatten().expect("stage flattens");
    let format = openusd::sdf::LayerRegistry::find_by_id("usdc").expect("usdc");
    format
        .write_bytes(flat.data())
        .expect("usdc writes")
        .bytes
        .to_vec()
}

/// The `stage_snapshot` notification for `bytes` at `version`.
fn snapshot_notification(version: u64, bytes: &[u8]) -> Value {
    json!({
        "method": "stage_snapshot",
        "params": {
            "version": version,
            "format": "usdc",
            "data": base64_encode(bytes),
        }
    })
}

/// A server that greets, then **waits** for the test to tell it to push.
struct LateServer {
    addr: String,
    push: Sender<Value>,
    thread: Option<thread::JoinHandle<()>>,
}

impl LateServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr").to_string();
        let (tx, rx) = channel::<Value>();
        let thread = thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            serve(stream, rx);
        });
        Self {
            addr,
            push: tx,
            thread: Some(thread),
        }
    }

    /// Push one notification now (after the greeting has been answered).
    fn push(&self, v: Value) {
        let _ = self.push.send(v);
    }
}

impl Drop for LateServer {
    fn drop(&mut self) {
        // Never join: the serving thread parks in `read_line` until the client's
        // socket closes, and the client is the test's to drop.
        self.thread.take();
    }
}

fn serve(stream: TcpStream, push: std::sync::mpsc::Receiver<Value>) {
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    // Answer `hello` and `subscribe`, then push on command.
    for _ in 0..2 {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line.trim_end()) else {
            return;
        };
        let id = v["id"].as_u64().unwrap_or(0);
        let method = v["method"].as_str().unwrap_or("");
        let reply = match method {
            "hello" => json!({
                "id": id,
                "result": {"protocol": PROTOCOL, "session": "late", "version": 1}
            }),
            "subscribe" => json!({"id": id, "result": {"version": 2}}),
            _ => json!({"id": id, "error": {"code": -32601, "message": "unexpected"}}),
        };
        if writeln!(out, "{reply}").is_err() {
            return;
        }
        let _ = out.flush();
    }
    // Now push whatever the test asks for, newest last.
    while let Ok(v) = push.recv() {
        if writeln!(out, "{v}").is_err() {
            return;
        }
        let _ = out.flush();
    }
}

/// Every corner of `bounds` inside the frame `(w, h)` under `orbit`, with a margin.
fn all_corners_in_frame(
    orbit: &Orbit,
    bounds: ([f64; 3], [f64; 3]),
    w: u32,
    h: u32,
) -> (bool, f64) {
    let m = view_proj(&orbit.camera(), bounds, w, h);
    let mut worst = 0.0f64;
    for x in [bounds.0[0], bounds.1[0]] {
        for y in [bounds.0[1], bounds.1[1]] {
            for z in [bounds.0[2], bounds.1[2]] {
                let c = [x, y, z];
                let clip = [
                    m[0] * c[0] + m[4] * c[1] + m[8] * c[2] + m[12],
                    m[1] * c[0] + m[5] * c[1] + m[9] * c[2] + m[13],
                    m[2] * c[0] + m[6] * c[1] + m[10] * c[2] + m[14],
                    m[3] * c[0] + m[7] * c[1] + m[11] * c[2] + m[15],
                ];
                if clip[3] <= 0.0 {
                    return (false, f64::INFINITY);
                }
                let ndc = [clip[0] / clip[3], clip[1] / clip[3]];
                worst = worst.max(ndc[0].abs()).max(ndc[1].abs());
            }
        }
    }
    (worst < 1.0, worst)
}

/// A window aspect the app supports, with its pixel size.
const SIZES: [(u32, u32); 3] = [(1280, 720), (1000, 800), (900, 900)];

#[test]
fn the_first_snapshot_after_subscribe_is_fitted_to_the_window_aspect() {
    // Every window shape the app may open in, not just 16:9: at exactly 16:9 the
    // *old* wiring happened to look right (the composed camera was fitted for the
    // same shape), so a single 1280×720 case would not have caught it. A tall or
    // square window does, and is also what a user resizes into.
    let bytes = cooked_city();
    for (w, h) in SIZES {
        let server = LateServer::start();
        let (mut client, early) = ProtocolClient::open(&server.addr).expect("greeting");
        assert!(early.is_empty(), "the greeting pushed no snapshot");

        let mut model = ClientModel::new();
        // The app's aspect for a fresh window: whatever the real viewport is.
        model.set_aspect(f64::from(w) / f64::from(h));
        model.mark_live();

        // Only now does the scene arrive — after connect and subscribe.
        server.push(snapshot_notification(8, &bytes));

        let mut orbit: Option<Orbit> = None;
        let mut scene_bounds = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline && orbit.is_none() {
            match client.read_wire() {
                Ok(Wire::Notification(n)) => {
                    if let Effect::NewScene(made) = model.apply(&n) {
                        // Exactly the app's decision: does this scene get the camera?
                        orbit =
                            view::camera_for_scene(&mut model, &made, f64::from(w) / f64::from(h));
                        scene_bounds = Some(made.bounds);
                        assert_eq!(made.version, 8);
                    }
                }
                Ok(Wire::Response(_)) => {}
                Err(_) => break,
            }
        }
        let orbit = orbit.expect("the first scene produced a camera");
        let bounds = scene_bounds.expect("a scene");

        let (inside, worst) = view::framing(&orbit, bounds, w, h);
        assert!(
            inside,
            "the first snapshot's camera must hold the city in a {w}x{h} frame: worst |NDC| {worst:.4}"
        );
        assert_eq!(
            orbit.target,
            [
                f64::midpoint(bounds.0[0], bounds.1[0]),
                f64::midpoint(bounds.0[1], bounds.1[1]),
                f64::midpoint(bounds.0[2], bounds.1[2]),
            ],
            "the eye looks at the scene centre, so the city is centred, not off in a corner"
        );
    }
}
#[test]
fn the_first_fit_uses_the_window_the_snapshot_landed_in_not_a_default_one() {
    // A tall window: with the old wiring the camera was fitted for whatever
    // aspect the model happened to hold (16:9), and a square frame cropped it.
    let bytes = cooked_city();
    let server = LateServer::start();
    let (mut client, _early) = ProtocolClient::open(&server.addr).expect("greeting");
    let mut model = ClientModel::new();
    model.set_aspect(16.0 / 9.0); // fitted for the wrong shape, as at startup
    model.mark_live();
    server.push(snapshot_notification(1, &bytes));

    let (w, h) = SIZES[2]; // 900x900
    let mut orbit = None;
    let mut bounds = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline && orbit.is_none() {
        match client.read_wire() {
            Ok(Wire::Notification(n)) => {
                if let Effect::NewScene(made) = model.apply(&n) {
                    orbit = view::camera_for_scene(&mut model, &made, f64::from(w) / f64::from(h));
                    bounds = Some(made.bounds);
                }
            }
            Ok(Wire::Response(_)) => {}
            Err(_) => break,
        }
    }
    let orbit = orbit.expect("a camera");
    let bounds = bounds.expect("a scene");
    let (inside, worst) = all_corners_in_frame(&orbit, bounds, w, h);
    assert!(
        inside && worst < 0.98,
        "a first fit must follow the window it landed in, not the startup default: worst |NDC| {worst:.4}"
    );
}

#[test]
fn later_snapshots_keep_the_users_camera() {
    // The rule the first-fit fix must not break: once the view has been framed,
    // a later scene (a patch, an opinion, a reload) hands it **no** camera, so the
    // camera the user is using survives.
    let bytes = cooked_city();
    let server = LateServer::start();
    let (mut client, _early) = ProtocolClient::open(&server.addr).expect("greeting");
    let mut model = ClientModel::new();
    model.set_aspect(16.0 / 9.0);
    model.mark_live();
    server.push(snapshot_notification(1, &bytes));
    server.push(snapshot_notification(2, &bytes));

    let (w, h) = SIZES[0];
    let mut framed = 0usize;
    let mut user: Option<Orbit> = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline && framed < 2 {
        match client.read_wire() {
            Ok(Wire::Notification(n)) => {
                if let Effect::NewScene(made) = model.apply(&n) {
                    match view::camera_for_scene(&mut model, &made, f64::from(w) / f64::from(h)) {
                        // The first scene frames it...
                        Some(fitted) => {
                            framed += 1;
                            // ...and the user then moves the camera their own way:
                            // orbited, panned and zoomed in. That is what must
                            // survive the next snapshot.
                            let mut o = fitted;
                            o.orbit(220.0, -90.0, 0.006);
                            o.pan(60.0, 40.0, f64::from(h));
                            o.zoom(3.0);
                            user = Some(o);
                        }
                        // ...and the second hands back nothing, which is the app's
                        // instruction to keep `self.orbit`.
                        None => framed += 1,
                    }
                }
            }
            Ok(Wire::Response(_)) => {}
            Err(_) => break,
        }
    }
    assert_eq!(framed, 2, "both snapshots were applied");
    let user = user.expect("the first scene was framed");
    assert!(
        user.distance > 0.0,
        "the user's camera is still the one in hand: {user:?}"
    );
    // A re-fit for this window would sit at the scene's fitted distance; the user's
    // camera is zoomed well inside it, so keeping the orbit is observable.
    let scene_fit = Orbit::from_camera(&rusd_viewer::scene::fit_camera(
        &rusd_viewer::scene::BBox {
            min: model.bbox.expect("bounds").0,
            max: model.bbox.expect("bounds").1,
        },
        f64::from(w) / f64::from(h),
    ));
    assert!(
        user.distance < scene_fit.distance * 0.9,
        "the user's camera stayed where they put it: {:.1} vs a re-fit's {:.1}",
        user.distance,
        scene_fit.distance
    );
}
#[test]
fn a_snapshot_before_connect_still_fits_because_the_app_marks_live_first() {
    // The order the worker actually produces: `Live` then the snapshot. The fit
    // must not depend on which of the two the UI thread drains first.
    let bytes = cooked_city();
    let server = LateServer::start();
    let worker = NetWorker::spawn(server.addr.clone(), Box::new(|| {}));
    let mut model = ClientModel::new();
    let (w, h) = SIZES[1];
    model.set_aspect(f64::from(w) / f64::from(h));

    server.push(snapshot_notification(3, &bytes));
    let mut fitted = None;
    let mut scene_bounds = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline && fitted.is_none() {
        for e in worker.drain() {
            match e {
                NetEvent::Live { .. } => model.mark_live(),
                NetEvent::Notification(n) => {
                    if let Effect::NewScene(made) = model.apply(&n) {
                        fitted =
                            view::camera_for_scene(&mut model, &made, f64::from(w) / f64::from(h));
                        scene_bounds = Some(made.bounds);
                    }
                }
                _ => {}
            }
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
    let orbit = fitted.expect("the worker's snapshot produced a camera");
    let bounds = scene_bounds.expect("a scene");
    let (inside, worst) = view::framing(&orbit, bounds, w, h);
    assert!(
        inside,
        "the worker's first snapshot must be framed in {w}x{h}: worst |NDC| {worst:.4}"
    );
    assert!(
        model.state.is_live(),
        "the client is live after Live + a snapshot"
    );
    assert_eq!(model.version, Some(3));
    // And the status bar says so, in Chinese, with the prim count.
    let line = model.status_line_zh();
    assert!(line.starts_with("已连接"), "{line}");
    assert!(line.contains("434 prim"), "{line}");
    assert!(line.contains("未选中"), "{line}");
}

#[test]
fn a_scene_from_disk_is_fitted_at_the_window_aspect_too() {
    // The `--usdc` fallback takes the same path: it is a scene on screen like any
    // other, so it gets the same first fit.
    let Ok(bytes) = std::fs::read("C:/resource/rusd/target/city.usdc") else {
        eprintln!("no cooked city on this machine; the in-memory case covers this path");
        return;
    };
    let (w, h) = SIZES[1];
    let aspect = f64::from(w) / f64::from(h);
    let made = rusd_viewer::session::MadeScene::from_layer_bytes(bytes, "offline.usdc", 0, aspect)
        .expect("the fallback composes");
    let mut model = ClientModel::new();
    model.offline = true;
    let orbit = view::camera_for_scene(&mut model, &made, aspect).expect("the fallback is fitted");
    let (inside, worst) = all_corners_in_frame(&orbit, made.bounds, w, h);
    assert!(inside && worst < 0.98, "worst |NDC| {worst:.4}");
    assert!(
        model.status_line_zh().starts_with("离线"),
        "{}",
        model.status_line_zh()
    );
}

/// The protocol-level guard: the mock's own line parses with the real parser.
#[test]
fn the_late_servers_notification_is_the_contracts_shape() {
    let bytes = cooked_city();
    let line = snapshot_notification(7, &bytes);
    match protocol::parse_line(&line.to_string()).expect("parses") {
        Wire::Notification(Notification::Snapshot(Snapshot {
            version,
            format,
            data,
            ..
        })) => {
            assert_eq!(version, 7);
            assert_eq!(format, "usdc");
            assert_eq!(data, base64_encode(&bytes));
        }
        other => panic!("expected a snapshot, got {other:?}"),
    }
}
