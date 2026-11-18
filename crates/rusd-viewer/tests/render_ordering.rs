//! The render thread's **command ordering**, which is the last rendering bug of
//! the M4' round: a correct first fit, on screen as a pre-fit corner.
//!
//! The evidence was a `--debug-ui` overlay that agreed with itself — `ppp 1.75`,
//! a 1280×670 rect, frame texels equal to the viewport's, `1:1 yes`, `fresh yes`,
//! and an orbit at the fit's `dist 1287.0 / target (116.9, 116.3, 25.0)` — over a
//! picture that was plainly the un-fitted one, with `frames received=1/shown=1`.
//! The orbit was right and the *frame* was old, and no counter could tell, because
//! the only camera the window reported was the one it had **sent**.
//!
//! The mechanism: `app::adopt_scene` sends `Scene` and then `View` within a batch,
//! and the render loop applied **one** command, drew, and went back to a blocking
//! wait. So the frame that reached the screen came from `Scene` alone — drawn with
//! the composed geometry's own camera — and the `View` that carried the window's
//! fit sat in the queue until the next input event arrived. An idle window never
//! had one.
//!
//! The fix is one property, asserted here twice over: **every pending command is
//! applied before a frame is drawn**, and a batch that changed anything always
//! produces a frame. The regression drives the thread the way the window does, with
//! no input at all, and checks the camera of each frame it produced.
//!
//! Both tests need a GPU (the frame is real); on a machine without one they say so
//! and pass, so the suite stays runnable.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rusd_scene::geometry::Camera;
use rusd_viewer::camera::Orbit;
use rusd_viewer::net::{RenderCommand, RenderWorker};
use rusd_viewer::session::MadeScene;

/// A device for the probe, or `None` where this machine cannot make one.
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
        label: Some("ordering-test"),
        ..Default::default()
    }))
    .ok()
}

/// The city `usdc` the parity fixtures use, from the main tree (`target/` is
/// gitignored, so a worktree looks upward for it — `RUSD_MAIN_TREE` names it
/// outright; see `tests/parity.rs`).
fn city_usdc() -> Option<Vec<u8>> {
    if let Ok(root) = std::env::var("RUSD_MAIN_TREE") {
        let p = std::path::Path::new(&root).join("target/city.usdc");
        if let Ok(b) = std::fs::read(&p) {
            return Some(b);
        }
    }
    let mut dir = std::env::current_dir().ok()?;
    for _ in 0..6 {
        let p = dir.join("target/city.usdc");
        if let Ok(b) = std::fs::read(&p) {
            return Some(b);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// A scene whose *composed* camera is deliberately **not** the fit: the fit below
/// moves the eye somewhere the scene's own camera is not, so a frame drawn from the
/// wrong one is unmistakable in the record.
fn scene_and_fit() -> Option<(MadeScene, Orbit, Orbit)> {
    let bytes = city_usdc()?;
    let made = MadeScene::from_layer_bytes(bytes, "city.usdc", 1, 16.0 / 9.0).expect("a scene");
    let composed = Orbit::from_camera(&made.geometry.camera.expect("a composed camera"));
    // The fit the window computes for its own (much narrower) rect. The aspect is
    // deliberately far from the 16:9 the scene was composed at, so the two cameras
    // are measurably different — which is what makes "which one reached the frame"
    // a question with an answer.
    let fit = Orbit::from_camera(&rusd_viewer::scene::fit_camera(
        &rusd_viewer::scene::BBox {
            min: made.bounds.0,
            max: made.bounds.1,
        },
        670.0 / 1280.0,
    ));
    assert!(
        (fit.distance - composed.distance).abs() > 1.0
            || (fit.target[0] - composed.target[0]).abs() > 1.0,
        "the two cameras must differ for this test to mean anything: \
         fit {fit:?} vs composed {composed:?}"
    );
    Some((made, composed, fit))
}

/// Whether a drawn camera is `want`, as an orbit.
fn drawn(cam: &Camera) -> Orbit {
    Orbit::from_camera(cam)
}

#[test]
fn a_scene_and_its_fitted_view_with_no_input_draw_the_fitted_camera() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter on this machine; skipping the ordering probe");
        return;
    };
    let Some((made, composed, fit)) = scene_and_fit() else {
        eprintln!("no cooked city on this machine; skipping the ordering probe");
        return;
    };

    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = wakes.clone();
    let worker = RenderWorker::spawn(
        device,
        queue,
        480,
        270,
        rusd_render::annotate::Style::default(),
        Box::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }),
    );

    // Exactly what `app::adopt_scene` does, in the same order, with nothing after
    // it: no resize, no camera move, no pointer event.
    worker.send(RenderCommand::Scene {
        geometry: Box::new(made.geometry.clone()),
        bounds: made.bounds,
    });
    worker.send(RenderCommand::View {
        orbit: fit,
        selection: None,
        only_selected: false,
    });

    // No input at all from here on: the thread must produce the fitted frame by
    // itself. A loop that renders between two commands leaves the `View` unapplied
    // and the window shows the composed camera until something else wakes it.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut frames: Vec<(u64, Orbit)> = Vec::new();
    while Instant::now() < deadline {
        frames = worker
            .produced()
            .into_iter()
            .map(|(seq, cam)| (seq, drawn(&cam)))
            .collect();
        if frames
            .iter()
            .any(|(_, o)| near(o, &fit) && !near(o, &composed))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        !frames.is_empty(),
        "the render thread produced no frame at all for Scene+View"
    );
    let fitted: Vec<&(u64, Orbit)> = frames
        .iter()
        .filter(|(_, o)| near(o, &fit) && !near(o, &composed))
        .collect();
    assert_eq!(
        fitted.len(),
        1,
        "exactly one frame is drawn with the fitted camera, and it is the last one: \
         produced {:?}, fit {:?}, composed {:?}",
        frames
            .iter()
            .map(|(s, o)| (*s, o.distance, o.target))
            .collect::<Vec<_>>(),
        (fit.distance, fit.target),
        (composed.distance, composed.target)
    );
    assert_eq!(
        frames.last().map(|(s, _)| *s),
        Some(fitted[0].0),
        "the fitted frame is the newest the thread produced, not an earlier one"
    );
    assert!(
        wakes.load(Ordering::Relaxed) >= 1,
        "the UI is woken for the frame it must show"
    );

    // And the frame handed to the UI is that one, carrying its own camera.
    let frame = worker.take_frame().expect("a frame is waiting");
    assert!(
        near(&drawn(&frame.camera), &fit) && !near(&drawn(&frame.camera), &composed),
        "the frame the UI draws was taken with the fit: {:?}",
        drawn(&frame.camera)
    );
    assert_eq!((frame.width, frame.height), (480, 270));
}

#[test]
fn a_batch_of_commands_produces_one_frame_with_the_last_camera() {
    // The same property from the other side: five camera moves and a resize that
    // arrive together must leave **one** frame, drawn with the last of them, rather
    // than one per command — a drag that outruns the renderer must not build a
    // backlog, and the first command of a batch must never be the one that gets
    // drawn.
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter on this machine; skipping the batch probe");
        return;
    };
    let Some((made, composed, fit)) = scene_and_fit() else {
        eprintln!("no cooked city on this machine; skipping the batch probe");
        return;
    };
    let worker = RenderWorker::spawn(
        device,
        queue,
        320,
        200,
        rusd_render::annotate::Style::default(),
        Box::new(|| {}),
    );
    // Let the thread park on its (empty) command wait, so everything below lands in
    // one batch rather than being applied one at a time.
    std::thread::sleep(Duration::from_millis(120));
    let before = worker.produced_seq();

    worker.send(RenderCommand::Scene {
        geometry: Box::new(made.geometry.clone()),
        bounds: made.bounds,
    });
    worker.send(RenderCommand::View {
        orbit: composed,
        selection: None,
        only_selected: false,
    });
    // A camera that is neither the scene's nor the final one: it must not survive.
    let mut mid = composed;
    mid.distance *= 0.5;
    worker.send(RenderCommand::View {
        orbit: mid,
        selection: None,
        only_selected: false,
    });
    worker.send(RenderCommand::View {
        orbit: fit,
        selection: None,
        only_selected: false,
    });
    worker.send(RenderCommand::Resize {
        width: 324,
        height: 204,
    });

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut frames = Vec::new();
    while Instant::now() < deadline {
        frames = worker.produced();
        if frames
            .iter()
            .any(|(s, c)| *s > before && near(&drawn(c), &fit))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let after: Vec<(u64, Orbit)> = frames
        .into_iter()
        .filter(|(s, _)| *s > before)
        .map(|(s, c)| (s, drawn(&c)))
        .collect();
    assert_eq!(
        after.len(),
        1,
        "one batch, one frame: {:?}",
        after
            .iter()
            .map(|(s, o)| (*s, o.distance, o.target))
            .collect::<Vec<_>>()
    );
    assert!(
        near(&after[0].1, &fit),
        "the frame is drawn with the batch's last camera, not its first: {:?}",
        after[0].1
    );
    let frame = worker.take_frame().expect("the batch's frame");
    assert_eq!(
        (frame.width, frame.height),
        (324, 204),
        "and at the batch's last size"
    );
}

/// Two orbits are the same camera to within a millimetre of distance and a
/// centimetre of target.
fn near(a: &Orbit, b: &Orbit) -> bool {
    (a.distance - b.distance).abs() < 1e-3
        && (0..3).all(|k| (a.target[k] - b.target[k]).abs() < 1e-2)
        && (a.yaw - b.yaw).abs() < 1e-3
        && (a.pitch - b.pitch).abs() < 1e-3
}
