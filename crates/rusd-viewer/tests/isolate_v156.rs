//! 「只看选中」 (the user's isolation ruling, after v1.5.4): a switch that makes the
//! viewport draw **only the selected prim's subtree**, and the whole scene when
//! nothing is selected.
//!
//! What this file pins, none of it needing a GPU or a window:
//!
//! 1. **The filter itself** (`scene::is_in_subtree`, `scene::isolate`): exact match
//!    or a genuine subtree, on a path boundary -- `/World/City/Tufts2` is **not**
//!    under `/World/City/Tufts`. Nothing selected is the whole scene, item for item.
//! 2. **It is a lens, not a composite**: the bounds, the camera and the kept items'
//!    order come back unchanged, so flipping the switch does not re-frame the view and
//!    a pick still lands on the prim under the pointer.
//! 3. **Switching selection switches the picture**: the filter is computed from
//!    whatever `selection` says, so the rule has no state of its own to drift.
//! 4. **The render thread applies it** on a real GPU, end to end: a `View` with the
//!    flag on draws a frame whose prim-id map holds only the subtree's prims
//!    (`tests/render_ordering.rs`'s pattern, and skipped where there is no adapter).

use rusd_scene::geometry::{Camera, GeoItem, Mesh, SceneGeometry};
use rusd_viewer::scene::{is_in_subtree, isolate};

// --- fixtures ----------------------------------------------------------------

/// One tiny triangle at a path: enough to be a `GeoItem`, and nothing more.
fn item(path: &str) -> GeoItem {
    GeoItem {
        path: path.to_owned(),
        mesh: Mesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            indices: vec![0, 1, 2],
        },
        transform: [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ],
        color: [0.5, 0.5, 0.5],
    }
}

/// A scene shaped like a stacked city: a block, two tufts under it, a sibling whose
/// name *starts with* the block's, and an unrelated prim.
///
/// Each item sits at its own x, because the probe below reads a **real** frame's
/// prim-id map: five triangles stacked at the origin would draw one id and prove
/// nothing about what was filtered.
fn city() -> SceneGeometry {
    let mut geo = SceneGeometry {
        items: vec![
            item("/World/City/Block_01"),
            item("/World/City/Block_01/Tufts/i000000"),
            item("/World/City/Block_01/Tufts/i000001"),
            // The trap: a prefix match that is not a subtree.
            item("/World/City/Block_012"),
            item("/World/Other"),
        ],
        camera: Some(Camera {
            position: [0.0, -40.0, 0.0],
            look_at: [0.0, 0.0, 0.0],
            fov_deg: 50.0,
        }),
        bounds: ([-8.0, -8.0, -1.0], [8.0, 8.0, 1.0]),
        diags: Vec::new(),
    };
    // One triangle per item, side by side across the view, all in the y = 0 plane and
    // **face-on to the eye at -y**. The row is shifted left of centre because a
    // 320x240 frame is 4:3: at 50 degrees the half-width is about 3.7 units at this
    // distance, and a row centred on 0 would clip its last item out of the frame.
    for (i, it) in geo.items.iter_mut().enumerate() {
        let x = -7.0f32 + 2.0 * i as f32;
        it.mesh.positions = vec![[x, 0.0, -0.8], [x, 0.0, 0.8], [x + 1.5, 0.0, -0.8]];
        it.mesh.normals = vec![[0.0, -1.0, 0.0]; 3];
    }
    geo
}

fn paths(geo: &SceneGeometry) -> Vec<&str> {
    geo.items.iter().map(|it| it.path.as_str()).collect()
}

// --- 1. the subtree rule -----------------------------------------------------

#[test]
fn the_subtree_test_is_exact_or_below_never_a_prefix() {
    assert!(is_in_subtree("/World/City", "/World/City"), "itself");
    assert!(is_in_subtree("/World/City/Block", "/World/City"), "a child");
    assert!(
        is_in_subtree("/World/City/Block/Tufts/i0", "/World/City"),
        "a grandchild, however deep"
    );
    assert!(
        !is_in_subtree("/World/City2", "/World/City"),
        "a name that merely starts with it is not a child"
    );
    assert!(
        !is_in_subtree("/World/City_Block", "/World/City"),
        "nor is a sibling that shares the prefix"
    );
    assert!(
        !is_in_subtree("/World", "/World/City"),
        "a parent is not its own child"
    );
    // The stage: everything is under it, which is what makes `/` the pseudo-root.
    assert!(is_in_subtree("/World/City", "/"));
}

// --- 2. the filter -----------------------------------------------------------

#[test]
fn nothing_selected_is_the_whole_scene_item_for_item() {
    let geo = city();
    let all = isolate(&geo, None);
    assert_eq!(paths(&all), paths(&geo), "same items, same order");
    assert_eq!(all.bounds, geo.bounds);
    assert!(all.camera.is_some(), "and the camera is the same");
}

#[test]
fn a_selection_keeps_its_subtree_and_nothing_else() {
    let geo = city();
    let one = isolate(&geo, Some("/World/City/Block_01/Tufts/i000001"));
    assert_eq!(
        paths(&one),
        ["/World/City/Block_01/Tufts/i000001"],
        "a leaf keeps itself"
    );

    let block = isolate(&geo, Some("/World/City/Block_01"));
    assert_eq!(
        paths(&block),
        [
            "/World/City/Block_01",
            "/World/City/Block_01/Tufts/i000000",
            "/World/City/Block_01/Tufts/i000001"
        ],
        "a prim keeps its subtree, and Block_012 is not in it: {:?}",
        paths(&block)
    );

    // A prim the scene does not hold draws nothing -- and that is honest rather than
    // a fallback: the user asked for that prim.
    assert!(isolate(&geo, Some("/World/NotHere")).items.is_empty());
}

#[test]
fn the_filter_does_not_re_frame_the_view() {
    // The bounds and the camera are the whole scene's, so the isolated picture is the
    // same picture with things removed -- not a zoom onto the selection.
    let geo = city();
    let one = isolate(&geo, Some("/World/City/Block_01/Tufts/i000000"));
    assert_eq!(one.bounds, geo.bounds, "the fit is the scene's");
    assert_eq!(one.camera, geo.camera, "and so is the camera");
    // The kept order is the source order, which is what keeps a re-bake's ids stable.
    let all: Vec<&str> = geo.items.iter().map(|it| it.path.as_str()).collect();
    let kept: Vec<&str> = one.items.iter().map(|it| it.path.as_str()).collect();
    let positions: Vec<usize> = kept
        .iter()
        .map(|p| {
            all.iter()
                .position(|q| q == p)
                .expect("kept items come from it")
        })
        .collect();
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "kept in source order: {positions:?}"
    );
}

#[test]
fn switching_the_selection_switches_the_picture() {
    // The rule has no state of its own: the same scene filtered by two different
    // selections is two different pictures, which is what "切换选中即切换画面" means.
    let geo = city();
    let a = isolate(&geo, Some("/World/City/Block_01/Tufts/i000000"));
    let b = isolate(&geo, Some("/World/Other"));
    assert_eq!(paths(&a), ["/World/City/Block_01/Tufts/i000000"]);
    assert_eq!(paths(&b), ["/World/Other"]);
    assert_ne!(paths(&a), paths(&b));
}

// --- 3. the render thread applies it (a real frame, where there is a GPU) ----

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
        label: Some("isolate-test"),
        ..Default::default()
    }))
    .ok()
}

#[test]
fn a_view_with_only_selected_draws_only_the_subtree() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter on this machine; skipping the isolation probe");
        return;
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use rusd_viewer::camera::Orbit;
    use rusd_viewer::net::{RenderCommand, RenderWorker};

    let geo = city();
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
    let orbit = Orbit::from_camera(&Camera {
        position: [3.0, -3.0, 3.0],
        look_at: [0.0, 0.0, 0.0],
        fov_deg: 50.0,
    });
    worker.send(RenderCommand::Scene {
        geometry: Box::new(geo.clone()),
        bounds: geo.bounds,
    });
    worker.send(RenderCommand::View {
        orbit,
        selection: None,
        only_selected: false,
    });

    /// The paths a frame's prim-id map actually holds -- what was *drawn*.
    fn drawn(worker: &RenderWorker) -> Option<Vec<String>> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Some(frame) = worker.take_frame() {
                let mut seen: Vec<String> = frame
                    .ids
                    .iter()
                    .filter(|&&id| id != 0)
                    .filter_map(|&id| frame.paths.get(id as usize - 1).cloned())
                    .collect();
                seen.sort();
                seen.dedup();
                return Some(seen);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    let whole = drawn(&worker).expect("the unfiltered frame");
    assert_eq!(
        whole,
        [
            "/World/City/Block_01",
            "/World/City/Block_01/Tufts/i000000",
            "/World/City/Block_01/Tufts/i000001",
            "/World/City/Block_012",
            "/World/Other"
        ]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>(),
        "the whole scene is drawn when nothing is selected"
    );

    worker.send(RenderCommand::View {
        orbit,
        selection: Some("/World/City/Block_01".to_owned()),
        only_selected: true,
    });
    let only = drawn(&worker).expect("the filtered frame");
    assert_eq!(
        only,
        [
            "/World/City/Block_01",
            "/World/City/Block_01/Tufts/i000000",
            "/World/City/Block_01/Tufts/i000001"
        ]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>(),
        "with the lens on, only the subtree is in the frame: {only:?}"
    );

    // **The lens is a lens**: turning it off restores the whole scene, so the switch
    // is never a one-way door.
    worker.send(RenderCommand::View {
        orbit,
        selection: Some("/World/City/Block_01".to_owned()),
        only_selected: false,
    });
    let back = drawn(&worker).expect("the restored frame");
    assert_eq!(back, whole, "off is the whole scene again");

    // **A drag must not flash the hidden prims back.** A gizmo drag's `Preview`
    // re-bakes the geometry, and it has to re-bake the *filtered* one: the prim being
    // dragged is the selection, so it is inside the filter, and everything else must
    // stay off screen for the length of the drag.
    worker.send(RenderCommand::View {
        orbit,
        selection: Some("/World/City/Block_01".to_owned()),
        only_selected: true,
    });
    let _ = drawn(&worker).expect("the filtered frame before the drag");
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("/World/City/Block_01".to_owned(), [0.0, 0.5, 0.0]);
    worker.send(RenderCommand::Preview {
        overrides,
        selection: Some("/World/City/Block_01".to_owned()),
    });
    let during = drawn(&worker).expect("the dragged frame");
    assert_eq!(
        during, only,
        "a drag under the lens re-bakes the same subtree: the hidden prims stay hidden"
    );

    // **Switching the selection switches the picture** while the switch stays on: the
    // flag travels with every `View`, and every path that changes the selection sends
    // one, so the lens simply follows.
    worker.send(RenderCommand::View {
        orbit,
        selection: Some("/World/Other".to_owned()),
        only_selected: true,
    });
    let other = drawn(&worker).expect("the re-filtered frame");
    assert_eq!(
        other,
        vec!["/World/Other".to_owned()],
        "the new selection is what is drawn now"
    );
}
