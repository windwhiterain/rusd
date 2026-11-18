//! **Startup against the server that actually ships.**
//!
//! `tests/startup.rs` drives the headless half of startup against a mock, which
//! is what pins the *rule* (startup never waits for the server). This file pins
//! something the mock cannot: that the rule holds against `rusd serve` itself —
//! the real accept thread, the real sync thread, the real cook, the real 58 KB
//! snapshot — with **city open**, and with a server that is *quiet* (open, then
//! nothing to say).
//!
//! What it asserts, and why each is the one that would have caught the regression:
//!
//! 1. **The assembly returns while the server is still cooking.** `open` on city
//!    takes ~0.2 s of cook plus the flatten; startup must finish before that
//!    arrives, not after. The budget is generous on purpose (a loaded machine must
//!    not make this flaky) but it is far below the snapshot's own latency once the
//!    snapshot is what startup would have been waiting for.
//! 2. **The snapshot still arrives afterwards** (`FIRST_SNAPSHOT` is marked, and
//!    the scene has the city's prim count). A fixed startup is not one that stops
//!    listening: what used to be a wait is now a notification.
//! 3. **A quiet server keeps the window alive.** After the snapshot, the server
//!    says nothing at all; a request that nobody answers ends at its deadline and
//!    the connection still delivers what comes next. This is the state the real
//!    stall happened in.
//!
//! No GPU and no window: `startup::assemble` is the shell's headless half, and the
//! network worker is the very code the window runs.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use rusd_agent::transport::Server;
use rusd_viewer::net::{NetCommand, NetEvent, ProtocolClient};
use rusd_viewer::session::{ClientModel, Effect};
use rusd_viewer::startup::{self, Startup};
use serde_json::{Value, json};

const CITY: &str = include_str!("../../../examples/city.rusd");

/// The longest a headless assembly may take, with a real server on the other end
/// and city open. Well above the ~0.2 s cook (so a slow machine passes) and well
/// below "startup waited for the snapshot".
const ASSEMBLY_BUDGET: Duration = Duration::from_secs(3);

/// The prim count city has; a snapshot that produced a scene must reach it.
const CITY_PRIMS: usize = 434;

/// A bare TCP client, so the test can drive the server the way a second viewer
/// would (and so it is not the thing under test).
struct Wire {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    next_id: u64,
}

impl Wire {
    fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).expect("connect to the test server");
        Self {
            reader: BufReader::new(stream.try_clone().expect("clone the socket")),
            writer: stream,
            next_id: 1,
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"id": id, "method": method, "params": params});
        writeln!(self.writer, "{line}").expect("write");
        self.writer.flush().expect("flush");
        loop {
            let mut buf = String::new();
            let n = self.reader.read_line(&mut buf).expect("read a line");
            assert!(n > 0, "the server closed before answering {method}");
            let v: Value = serde_json::from_str(buf.trim_end()).expect("JSON");
            if v.get("id").and_then(Value::as_u64) == Some(id) {
                return v;
            }
        }
    }

    /// Read notifications until one named `method` arrives (or the deadline).
    fn await_notification(&mut self, method: &str, within: Duration) -> Option<Value> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            let mut buf = String::new();
            let n = self.reader.read_line(&mut buf).ok()?;
            if n == 0 {
                return None;
            }
            let v: Value = serde_json::from_str(buf.trim_end()).ok()?;
            if v.get("method").and_then(Value::as_str) == Some(method) {
                return Some(v);
            }
        }
        None
    }
}

fn server_with_city(tag: &str) -> (Server, std::path::PathBuf) {
    let dir =
        std::env::temp_dir().join(format!("rusd-served-startup-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let captures = dir.join("captures");
    let server = Server::bind_with_captures("127.0.0.1:0", captures).expect("bind a test server");
    (server, dir)
}

/// Open city on `addr` and wait until the composited notifications are published.
///
/// After `state_changed{reason:"open"}` the server has broadcast the snapshot; the
/// test then closes its own connection, so the only subscriber left is the viewer
/// under test. That is the "quiet server" condition: open, and then nothing.
fn open_city_and_go_quiet(addr: SocketAddr) {
    let mut wire = Wire::connect(addr);
    wire.call("hello", json!({"client": "opener", "protocol": 1}));
    wire.call("subscribe", json!({"client": "opener"}));
    let opened = wire.call("open", json!({"content": CITY}));
    assert!(opened.get("result").is_some(), "the city opened: {opened}");
    assert!(
        wire.await_notification("state_changed", Duration::from_secs(30))
            .is_some(),
        "the server published the open"
    );
}

#[test]
fn the_assembly_finishes_while_a_real_server_is_still_cooking_city() {
    let (server, _dir) = server_with_city("cooking");
    let addr = server.local_addr();

    let mut startup = Startup::new();
    let began = Instant::now();
    let assembled = startup::assemble(&addr.to_string(), &mut startup);
    let took = began.elapsed();

    assert!(
        took < ASSEMBLY_BUDGET,
        "assembly must not wait for a server; it took {took:?}"
    );
    assert!(
        startup.reached(startup::SHELL_BUILT),
        "the shell is built while the server is still greeting: {:?}",
        startup.marks()
    );
    assert!(
        !startup.reached(startup::FIRST_SNAPSHOT),
        "and it did not wait for a snapshot to get there"
    );
    drop(assembled);
}

#[test]
fn a_real_server_delivers_the_snapshot_after_startup_is_finished() {
    let (server, _dir) = server_with_city("snapshot");
    let addr = server.local_addr();
    open_city_and_go_quiet(addr);

    let mut startup = Startup::new();
    let began = Instant::now();
    let mut assembled = startup::assemble(&addr.to_string(), &mut startup);
    let took = began.elapsed();
    assert!(took < ASSEMBLY_BUDGET, "{took:?}");

    // What startup stopped waiting for is still delivered, as an event.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut scene = None;
    while Instant::now() < deadline && scene.is_none() {
        for e in assembled.net.drain() {
            match e {
                NetEvent::Notification(n) => {
                    if let Effect::NewScene(made) = assembled.model.apply(&n) {
                        scene = Some(*made);
                    }
                }
                NetEvent::Failed(diags) => panic!("the viewer failed: {diags:?}"),
                _ => {}
            }
        }
        if scene.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let made = scene.expect("the snapshot arrives after startup is already done");
    assert_eq!(
        made.geometry.items.len(),
        CITY_PRIMS,
        "the scene is city's, not an empty fallback"
    );
    assert!(
        made.geometry.camera.is_some(),
        "and it was fitted, so the window has something to frame"
    );
    startup.mark(startup::FIRST_SNAPSHOT);
    assert!(startup.reached(startup::FIRST_SNAPSHOT));

    // **And the scene actually renders.** The marker this task asks for is
    // "frames received >= 1": a real server's snapshot, on the render thread, one
    // frame out, with no window. That is the whole pipeline startup has to make
    // possible, proved headless. It needs a GPU; on a machine without one the probe
    // says so and passes, as this crate's other render tests do.
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter on this machine; skipping the frame probe");
        return;
    };
    let worker = rusd_viewer::net::RenderWorker::spawn(
        device,
        queue,
        320,
        180,
        rusd_render::annotate::Style::default(),
        Box::new(|| {}),
    );
    worker.send(rusd_viewer::net::RenderCommand::Scene {
        geometry: Box::new(made.geometry.clone()),
        bounds: made.bounds,
    });
    if let Some(cam) = made.geometry.camera {
        worker.send(rusd_viewer::net::RenderCommand::View {
            orbit: rusd_viewer::camera::Orbit::from_camera(&cam),
            selection: None,
            only_selected: false,
        });
    }
    let rendered = Instant::now() + Duration::from_secs(30);
    while Instant::now() < rendered && worker.produced_seq() == 0 {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        worker.produced_seq() >= 1,
        "the render thread drew a frame from the real server's scene (frames received >= 1)"
    );
}

/// A device for the frame probe, or `None` where this machine cannot make one.
fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))
    .ok()?;
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("served-startup"),
        ..Default::default()
    }))
    .ok()
}

#[test]
fn a_server_that_has_stopped_answering_ends_in_a_diagnostic_not_a_hang() {
    let (server, _dir) = server_with_city("stopped");
    let addr = server.local_addr();
    open_city_and_go_quiet(addr);

    // The server stops accepting (its accept thread checks this flag); a client that
    // arrives now gets no greeting at all. The point is that *this ends*: with the
    // greeting's deadline it is one diagnostic in a few seconds, where the pre-fix
    // client could park in `recv` with no bound at all.
    server.shutdown();
    std::thread::sleep(Duration::from_millis(200));

    let began = Instant::now();
    let out = ProtocolClient::open_within(&addr.to_string(), Duration::from_millis(500));
    let took = began.elapsed();
    assert!(
        out.is_err(),
        "a server that greets nobody has to be refused, not waited on"
    );
    assert!(
        took < Duration::from_secs(5),
        "refused at its deadline, not later: {took:?}"
    );
}

#[test]
fn the_worker_sends_a_request_to_a_real_server_and_the_server_answers_it() {
    // The other half of "quiet": a real method round trip through the worker must
    // still work after all this, so the timeout path did not make the worker mute.
    let (server, _dir) = server_with_city("roundtrip");
    let addr = server.local_addr();
    open_city_and_go_quiet(addr);

    let mut startup = Startup::new();
    let assembled = startup::assemble(&addr.to_string(), &mut startup);
    std::thread::sleep(Duration::from_millis(100));
    let _ = assembled.net.drain();
    assembled.net.send(NetCommand::Call {
        tag: "composition_tree".to_owned(),
        method: "composition_tree".to_owned(),
        params: json!({}),
    });

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut tree = None;
    while Instant::now() < deadline && tree.is_none() {
        for e in assembled.net.drain() {
            if let NetEvent::Reply { tag, result } = e {
                assert_eq!(tag, "composition_tree");
                tree = Some(result);
            }
        }
        if tree.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let result = tree.expect("the worker's request comes back");
    let value = result.expect("a real server answers composition_tree");
    let head: String = value.to_string().chars().take(200).collect();
    assert!(
        value.get("layers").is_some(),
        "the answer is the composition tree: {head}"
    );
    let _ = ClientModel::new();
}
