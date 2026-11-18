//! The render thread's pacing, which is why an idle viewer showed a handful of fps
//! and why a snapshot could sit undrawn.
//!
//! Two properties, both measured rather than argued:
//!
//! 1. **An idle viewer draws nothing and wakes nobody.** The thread parks in its
//!    command wait; the only frames that reach the UI are the ones a change asked
//!    for. A thread that polled and redrew on its own would burn a core and make
//!    the `fps` readout meaningless.
//! 2. **A scene is drawn, and wakes the UI exactly once for it.** This is the half
//!    of the wiring bug that the camera tests cannot see: a correct fit that the
//!    renderer never gets around to drawing looks exactly like a wrong fit.
//!
//! Both need a GPU. On a machine without one the test says so and passes, so the
//! suite stays runnable where `wgpu` cannot find an adapter.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

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
        label: Some("pacing-test"),
        ..Default::default()
    }))
    .ok()
}

#[test]
fn an_idle_render_thread_draws_nothing_and_wakes_nobody() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter on this machine; skipping the pacing probe");
        return;
    };
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = wakes.clone();
    let worker = RenderWorker::spawn(
        device,
        queue,
        320,
        240,
        rusd_render::annotate::Style::default(),
        Box::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }),
    );
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        wakes.load(Ordering::Relaxed),
        0,
        "an idle render thread must not wake the UI (it parked in its command wait)"
    );
    assert!(
        worker.take_frame().is_none(),
        "an idle render thread must not draw a frame by itself"
    );
}

#[test]
fn a_scene_is_drawn_and_wakes_the_ui_once() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter on this machine; skipping the pacing probe");
        return;
    };
    let Ok(bytes) = std::fs::read("C:/resource/rusd/target/city.usdc") else {
        eprintln!("no cooked city on this machine; skipping the draw probe");
        return;
    };
    let made = MadeScene::from_layer_bytes(bytes, "city.usdc", 1, 16.0 / 9.0).expect("scene");
    let orbit = Orbit::from_camera(&made.geometry.camera.expect("a fitted camera"));
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
    // The app sends the scene and then the camera on top of it.
    worker.send(RenderCommand::Scene {
        geometry: Box::new(made.geometry.clone()),
        bounds: made.bounds,
    });
    worker.send(RenderCommand::View {
        orbit,
        selection: None,
        only_selected: false,
    });

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut frame = None;
    while Instant::now() < deadline && frame.is_none() {
        frame = worker.take_frame();
        if frame.is_none() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let frame = frame.expect("a scene must be drawn");
    assert_eq!(
        (frame.width, frame.height),
        (480, 270),
        "the frame is the viewport's size"
    );
    assert!(
        frame.ids.iter().any(|&id| id != 0),
        "and it has the city in it"
    );
    assert!(
        wakes.load(Ordering::Relaxed) >= 1,
        "the UI was told a frame is ready"
    );
}
