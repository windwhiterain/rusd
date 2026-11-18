//! v1.5 acceptance: the **two-op** pipeline, `mesh -> points -> instances`
//! (docs/PROTOCOL.md v1.5 / v1.5.1).
//!
//! The contract has three parts, and each one is a way the split could be wrong:
//!
//! 1. **The stages are real.** A prefix stopped at `root` shows the input mesh, one
//!    stopped after `RusdSamplePoints` shows the cloud it authored, and one stopped
//!    after `RusdInstances` shows the instances. Each op hides exactly what it
//!    consumed (a `visibility = "invisible"` over, never a delete), so the three
//!    states are mutually exclusive — mesh only, cloud only, instances only.
//! 2. **The cloud is on the surface.** `count` samples, every one of them on the
//!    input mesh (distance ≈ 0), not on its vertices and not in its bounding box.
//! 3. **The geometry in is world space.** Both ops read and write across the anchor's
//!    transform: moving the mesh's parent, or the ops' own parent, moves the samples
//!    and the instances by exactly that transform — once, never twice.
//!
//! Plus determinism (one seed, one result) and the budget rule (`max_prims`
//! truncates a cloud in its own order + `W_BUDGET`).
//!
//! Everything drives the real cook engine over a real document: the input prims are
//! authored in the `.rusd` text, lowered by `rusd_scene`, and cooked by `rusd_proc`
//! exactly as `rusd expand` does it.

use openusd::sdf::{self, Value as SdfValue};
use openusd::usd::{PrimPredicate, Stage};
use rusd_core::Value;
use rusd_core::syntax::{self, ParsedDocument};
use rusd_scene::geometry::{self, Triangle};

const MESH: &str = include_str!("../../../examples/mesh_scatter.rusd");

/// Lower + cook a document the way `rusd expand` does.
fn expand(text: &str) -> (Stage, rusd_proc::CookResult) {
    let doc = syntax::parse(text).expect("the example parses");
    expand_doc(&doc)
}

fn expand_doc(doc: &ParsedDocument) -> (Stage, rusd_proc::CookResult) {
    let stage = rusd_scene::lower(doc).expect("the document lowers");
    let root = stage.root_layer().identifier().to_owned();
    let nodes = rusd_proc::nodes_from_doc(doc);
    let mut engine = rusd_proc::default_registry();
    let result = engine.cook(&stage, &root, &nodes);
    (stage, result)
}

fn errors(result: &rusd_proc::CookResult) -> Vec<String> {
    result
        .diags
        .iter()
        .filter(|d| d.severity == rusd_core::Severity::Error)
        .map(|d| format!("{} {}", d.code, d.message))
        .collect()
}

/// The identifier of the output layer authored by the node whose `CookOp.id` is
/// `node` — how a test names one op's layer without assuming an order.
fn layer_of(result: &rusd_proc::CookResult, node: &str) -> String {
    result
        .ops
        .iter()
        .find(|op| op.id == node)
        .unwrap_or_else(|| {
            panic!(
                "no cooked op for {node}; ops are {:?}",
                result.ops.iter().map(|o| &o.id).collect::<Vec<_>>()
            )
        })
        .layer_id
        .clone()
}

fn composed_paths(stage: &Stage) -> Vec<String> {
    let mut paths = Vec::new();
    let _ = stage.traverse(PrimPredicate::ALL, |p| paths.push(p.as_str().to_owned()));
    paths
}

fn visibility(stage: &Stage, path: &str) -> Option<String> {
    attr(stage, path, "visibility").and_then(|v| match v {
        SdfValue::Token(t) => Some(t.as_str().to_owned()),
        _ => None,
    })
}

fn attr(stage: &Stage, path: &str, name: &str) -> Option<SdfValue> {
    let prim = stage.prim(sdf::path(path).ok()?).ok()?;
    prim.attribute(name).get::<SdfValue>().ok()?
}

/// The prims that actually render (composed, visible, with drawable geometry).
fn drawn(stage: &Stage) -> Vec<String> {
    rusd_scene::geometry::extract(stage)
        .items
        .into_iter()
        .map(|i| i.path)
        .collect()
}

fn draws(stage: &Stage, path: &str) -> bool {
    drawn(stage).iter().any(|p| p == path)
}

/// A prim's composed world `xformOp:translate`.
fn world_translate(stage: &Stage, path: &str) -> Option<[f64; 3]> {
    match attr(stage, path, "xformOp:translate")? {
        SdfValue::Vec3f(v) => Some([v[0] as f64, v[1] as f64, v[2] as f64]),
        SdfValue::Vec3d(v) => Some([v[0], v[1], v[2]]),
        _ => None,
    }
}

/// Author a translate on a prim in the stage's root layer — the same two attributes
/// an `opinion` writes, and the way a prim is moved in this pipeline (the surface has
/// no `xformOp:*` property, SPEC §1).
fn move_prim(stage: &Stage, path: &str, t: [f64; 3]) {
    let root = stage.root_layer().identifier().to_owned();
    let mut layer = stage.layer_mut(&root).expect("the root layer");
    layer
        .edit(|l| {
            let d = l.data_mut();
            let p = sdf::path(path)?;
            sdf::PrimSpec::over(d, p)?;
            let tp = format!("{path}.xformOp:translate");
            let mut ta = sdf::AttributeSpec::new(
                d,
                tp.as_str(),
                "point3f",
                sdf::Variability::Uniform,
                false,
            )?;
            ta.set_default(SdfValue::Vec3f(openusd::gf::vec3f(
                t[0] as f32,
                t[1] as f32,
                t[2] as f32,
            )))?;
            let op = format!("{path}.xformOpOrder");
            let mut oa = sdf::AttributeSpec::new(
                d,
                op.as_str(),
                "token[]",
                sdf::Variability::Uniform,
                false,
            )?;
            oa.set_default(SdfValue::TokenVec(vec!["xformOp:translate".into()]))?;
            Ok(())
        })
        .expect("the prim can be moved");
}

/// Distance from `p` to a triangle (Ericson, *Real-Time Collision Detection* §5.1.5).
fn distance_to_triangle(p: [f64; 3], t: Triangle) -> f64 {
    let sub = |a: [f64; 3], b: [f64; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    let dot = |a: [f64; 3], b: [f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let ab = sub(t[1], t[0]);
    let ac = sub(t[2], t[0]);
    let ap = sub(p, t[0]);
    let d1 = dot(ab, ap);
    let d2 = dot(ac, ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return dot(ap, ap).sqrt();
    }
    let bp = sub(p, t[1]);
    let d3 = dot(ab, bp);
    let d4 = dot(ac, bp);
    if d3 >= 0.0 && d4 <= d3 {
        return dot(bp, bp).sqrt();
    }
    let cp = sub(p, t[2]);
    let d5 = dot(ab, cp);
    let d6 = dot(ac, cp);
    if d6 >= 0.0 && d5 <= d6 {
        return dot(cp, cp).sqrt();
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let v = d1 / (d1 - d3);
        let q = [
            t[0][0] + v * ab[0],
            t[0][1] + v * ab[1],
            t[0][2] + v * ab[2],
        ];
        return dot(sub(p, q), sub(p, q)).sqrt();
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let w = d2 / (d2 - d6);
        let q = [
            t[0][0] + w * ac[0],
            t[0][1] + w * ac[1],
            t[0][2] + w * ac[2],
        ];
        return dot(sub(p, q), sub(p, q)).sqrt();
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let w = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        let bc = sub(t[2], t[1]);
        let q = [
            t[1][0] + w * bc[0],
            t[1][1] + w * bc[1],
            t[1][2] + w * bc[2],
        ];
        return dot(sub(p, q), sub(p, q)).sqrt();
    }
    let denom = 1.0 / (va + vb + vc);
    let v = vb * denom;
    let w = vc * denom;
    let q = [
        t[0][0] + ab[0] * v + ac[0] * w,
        t[0][1] + ab[1] * v + ac[1] * w,
        t[0][2] + ab[2] * v + ac[2] * w,
    ];
    dot(sub(p, q), sub(p, q)).sqrt()
}

/// The cloud authored by the sample op, in world space.
fn sample_points(stage: &Stage) -> Vec<[f64; 3]> {
    geometry::world_points(stage, "/World/TuftSamples/points")
        .expect("the sample op authored a readable cloud")
}

fn instance_paths(stage: &Stage) -> Vec<String> {
    composed_paths(stage)
        .into_iter()
        .filter(|p| p.starts_with("/World/Tufts/i"))
        .collect()
}

// --- 1. two ops, three prefixes, mutually exclusive states ------------------

#[test]
fn the_example_cooks_two_ops_in_pipeline_order() {
    let (_, result) = expand(MESH);
    assert!(errors(&result).is_empty(), "{:?}", errors(&result));
    let ids: Vec<&str> = result.ops.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["/World/TuftSamples", "/World/Tufts"],
        "the sample op cooks before the op that consumes its cloud"
    );
    // The metadata the composition tree joins on (v1.5.1): the generator name and the
    // `input_points` each op reads.
    assert_eq!(result.ops[0].generator, "RusdSamplePoints");
    assert_eq!(
        result.ops[0].input.as_deref(),
        Some("/World/Terrain"),
        "the sampler reads the authored mesh"
    );
    assert_eq!(result.ops[1].generator, "RusdInstances");
    assert_eq!(
        result.ops[1].input.as_deref(),
        Some("/World/TuftSamples/points"),
        "the instances op reads the cloud its upstream op authored"
    );
}

#[test]
fn the_three_prefixes_show_the_mesh_the_cloud_and_the_instances() {
    let (stage, result) = expand(MESH);
    assert!(errors(&result).is_empty(), "{:?}", errors(&result));
    let sample_layer = layer_of(&result, "/World/TuftSamples");
    let instances_layer = layer_of(&result, "/World/Tufts");

    // --- stop at root: the mesh, and nothing the ops made.
    stage.mute_layer(&sample_layer);
    stage.mute_layer(&instances_layer);
    assert!(
        draws(&stage, "/World/Terrain"),
        "the first prefix shows the input mesh: {:?}",
        drawn(&stage)
    );
    assert!(
        visibility(&stage, "/World/Terrain").is_none(),
        "with both ops out of the stack nothing hides the mesh"
    );
    assert!(
        !draws(&stage, "/World/TuftSamples/points"),
        "no cloud before the op that samples it"
    );
    assert!(
        !draws(&stage, "/World/Tufts/i000000"),
        "no instances before the op that makes them"
    );

    // --- stop after the sampler: the cloud, the mesh hidden.
    stage.unmute_layer(&sample_layer);
    assert!(
        draws(&stage, "/World/TuftSamples/points"),
        "the middle prefix shows the sampled cloud: {:?}",
        drawn(&stage)
    );
    assert_eq!(
        visibility(&stage, "/World/Terrain").as_deref(),
        Some("invisible"),
        "and the mesh it consumed is hidden"
    );
    assert!(
        !draws(&stage, "/World/Terrain"),
        "so the mesh does not draw"
    );
    assert!(
        !draws(&stage, "/World/Tufts/i000000"),
        "and no instances yet"
    );

    // --- the full stack: the instances only.
    stage.unmute_layer(&instances_layer);
    assert!(
        draws(&stage, "/World/Tufts/i000000"),
        "the last prefix shows instances: {:?}",
        drawn(&stage)
    );
    assert_eq!(
        visibility(&stage, "/World/TuftSamples/points").as_deref(),
        Some("invisible"),
        "the cloud the instances consumed is hidden in turn"
    );
    assert!(
        !draws(&stage, "/World/TuftSamples/points"),
        "so the cloud does not draw"
    );
    assert!(
        !draws(&stage, "/World/Terrain"),
        "and neither does the mesh"
    );
}

#[test]
fn each_op_hides_only_the_geometry_it_consumed() {
    // Per-op replacement (v1.5.1): the sampler hides the mesh, the instances op hides
    // the cloud — two opinions in two layers, not one op hiding everything.
    let (stage, result) = expand(MESH);
    let sample_layer = layer_of(&result, "/World/TuftSamples");
    let instances_layer = layer_of(&result, "/World/Tufts");

    let mut authored: Vec<(String, String)> = Vec::new();
    for id in [sample_layer.clone(), instances_layer.clone()] {
        let layer = stage.layer(&id).expect("the op's layer");
        for p in layer.data().spec_paths() {
            authored.push((id.clone(), p.as_str().to_owned()));
        }
    }

    let who_overrides = |target: &str| -> Vec<String> {
        // The *attribute* path, not the prim path: the sampler authors the cloud prim
        // itself (`/World/TuftSamples/points`), and the instances op re-declares that
        // prim to hide it — so the prim path is in both layers, and only the
        // `visibility` attribute distinguishes "authored it" from "overrode it".
        let attribute = format!("{target}.visibility");
        authored
            .iter()
            .filter(|(_, p)| p == &attribute)
            .map(|(id, _)| id.clone())
            .collect()
    };
    assert_eq!(
        who_overrides("/World/Terrain"),
        vec![sample_layer.clone()],
        "only the sample op hides the mesh: {authored:?}"
    );
    assert_eq!(
        who_overrides("/World/TuftSamples/points"),
        vec![instances_layer.clone()],
        "only the instances op hides the cloud: {authored:?}"
    );
    // And the inputs' own specs are untouched: replaced, not destroyed.
    let root = stage.root_layer().identifier().to_owned();
    let base = stage.layer(&root).expect("the root layer");
    assert!(
        base.data()
            .try_field(
                &sdf::path("/World/Terrain.points").unwrap(),
                sdf::FieldKey::Default.as_str(),
            )
            .ok()
            .flatten()
            .is_some(),
        "the authored mesh still has its vertices"
    );
}

// --- 2. the cloud is `count` points, on the surface ------------------------

#[test]
fn the_sample_op_writes_exactly_count_points_on_the_input_surface() {
    let (stage, result) = expand(MESH);
    assert!(errors(&result).is_empty(), "{:?}", errors(&result));
    let doc = syntax::parse(MESH).unwrap();
    let count = match doc
        .prims
        .iter()
        .find(|p| p.path.as_str() == "/World/TuftSamples")
        .expect("the sample node")
        .props
        .get("count")
    {
        Some(Value::Int(i)) => *i as usize,
        other => panic!("count is an int: {other:?}"),
    };

    let cloud = sample_points(&stage);
    assert_eq!(cloud.len(), count, "the op authors exactly `count` points");

    // The surface it sampled, read from the **root** layer: the sampler's own output
    // layer hides the mesh, so the composed read of the input would be its samples.
    let tris = {
        let sample_layer = layer_of(&result, "/World/TuftSamples");
        stage.mute_layer(&sample_layer);
        let tris = geometry::world_triangles(&stage, "/World/Terrain")
            .expect("the input mesh's triangles are readable");
        stage.unmute_layer(&sample_layer);
        tris
    };
    assert!(!tris.is_empty(), "the input mesh has surface");

    let mut worst = 0.0_f64;
    for p in &cloud {
        let d = tris
            .iter()
            .map(|t| distance_to_triangle(*p, *t))
            .fold(f64::INFINITY, f64::min);
        worst = worst.max(d);
        assert!(d < 1e-4, "a sample at {p:?} is {d} away from the surface");
    }
    eprintln!("worst distance from a sample to the surface: {worst:e}");

    // And they are not the mesh's vertices: 240 samples over 9 vertices would land on
    // at most 9 distinct spots.
    let mut distinct: Vec<[u64; 3]> = cloud
        .iter()
        .map(|p| [p[0].to_bits(), p[1].to_bits(), p[2].to_bits()])
        .collect();
    distinct.sort_unstable();
    distinct.dedup();
    assert!(
        distinct.len() > 9,
        "the samples are spread over the surface, not snapped to vertices: {} distinct",
        distinct.len()
    );
}

#[test]
fn the_instances_op_gives_one_instance_per_sample_at_its_own_position() {
    let (stage, result) = expand(MESH);
    assert!(errors(&result).is_empty(), "{:?}", errors(&result));
    let cloud = sample_points(&stage);
    let instances = instance_paths(&stage);
    assert_eq!(
        instances.len(),
        cloud.len(),
        "one instance per point, in the cloud's own order"
    );
    for (k, point) in cloud.iter().enumerate() {
        let path = format!("/World/Tufts/i{k:06}");
        let t = world_translate(&stage, &path)
            .unwrap_or_else(|| panic!("{path} has no composed translate"));
        for axis in 0..3 {
            assert!(
                (t[axis] - point[axis]).abs() < 1e-4,
                "{path}: axis {axis} is {} not {} (the point it came from)",
                t[axis],
                point[axis]
            );
        }
    }
    // An instance composes its prototype through an arc, so it is real geometry on
    // both ends rather than a copy of the prototype's numbers.
    let geo = geometry::extract(&stage);
    assert!(
        geo.items.iter().any(|i| i.path == "/World/Tufts/i000000"),
        "the instance has drawable geometry"
    );
}

// --- 3. world space in, local space out ------------------------------------

/// Cook the example with one prim in the root layer given a translate, and return the
/// stage plus the same document cooked with no moves — the baseline to compare against.
fn expand_with_move(path: Option<(&str, [f64; 3])>) -> (Stage, Stage, rusd_proc::CookResult) {
    let doc = syntax::parse(MESH).expect("the example parses");
    let (plain, _) = expand_doc(&doc);
    let stage = rusd_scene::lower(&doc).expect("the example lowers");
    if let Some((which, t)) = path {
        move_prim(&stage, which, t);
    }
    let root = stage.root_layer().identifier().to_owned();
    let nodes = rusd_proc::nodes_from_doc(&doc);
    let mut engine = rusd_proc::default_registry();
    let result = engine.cook(&stage, &root, &nodes);
    (stage, plain, result)
}

/// The input mesh's world triangles, read past the op layers that hide it.
fn input_triangles(stage: &Stage, result: &rusd_proc::CookResult) -> Vec<Triangle> {
    let sample_layer = layer_of(result, "/World/TuftSamples");
    stage.mute_layer(&sample_layer);
    let tris = geometry::world_triangles(stage, "/World/Terrain")
        .expect("the input mesh's triangles are readable");
    stage.unmute_layer(&sample_layer);
    tris
}

#[test]
fn moving_the_input_meshs_parent_moves_the_samples_and_the_instances() {
    // Raise the whole input before cooking: a sampler that read local `points` would
    // ignore this, an instance writer that wrote a world point as a local op would
    // apply it twice. The mesh is undulating, so the claim is "every vertex moved by
    // exactly +7 on z", not "everything is at z = 7".
    let (stage, plain, result) = expand_with_move(Some(("/World/Terrain", [0.0, 0.0, 7.0])));
    assert!(errors(&result).is_empty(), "{:?}", errors(&result));

    let moved_tris = input_triangles(&stage, &result);
    let plain_tris = geometry::world_triangles(&plain, "/World/Terrain").expect("triangles");
    assert_eq!(moved_tris.len(), plain_tris.len(), "same surface");
    for (m, p) in moved_tris.iter().zip(plain_tris.iter()) {
        for (mv, pv) in m.iter().zip(p.iter()) {
            for axis in 0..3 {
                let want = pv[axis] + if axis == 2 { 7.0 } else { 0.0 };
                assert!(
                    (mv[axis] - want).abs() < 1e-6,
                    "the input really is raised: {mv:?} vs {pv:?} + 7 on z"
                );
            }
        }
    }

    // The cloud sits on that raised surface — same distance-to-surface rule as before,
    // so an undulating mesh is handled rather than assumed flat.
    let cloud = sample_points(&stage);
    assert!(!cloud.is_empty(), "the sampler authored a cloud");
    let worst = cloud
        .iter()
        .map(|p| {
            moved_tris
                .iter()
                .map(|t| distance_to_triangle(*p, *t))
                .fold(f64::INFINITY, f64::min)
        })
        .fold(0.0_f64, f64::max);
    assert!(
        worst < 1e-4,
        "every sample is on the raised surface, worst distance {worst:e}"
    );
    // …and the instances are where their points are, in world space, exactly once.
    for path in instance_paths(&stage) {
        let t = world_translate(&stage, &path).unwrap();
        let want = world_translate(&plain, &path).unwrap();
        for axis in 0..3 {
            let delta = t[axis] - want[axis];
            let expect = if axis == 2 { 7.0 } else { 0.0 };
            assert!(
                (delta - expect).abs() < 1e-4,
                "{path}: axis {axis} moved by {delta}, not {expect} (applied twice?)"
            );
        }
    }
}

#[test]
fn moving_a_pipeline_anchor_is_a_change_of_frame_not_of_world_position() {
    // Each op authors its output in its **anchor's** local space, so moving an anchor
    // must change the authored numbers and leave the composed world result exactly
    // where it was. That is the same rule the scatter follows, stated so it can fail:
    // a sampler that forgot `inverse(anchor)` would move the cloud, and reading a
    // world point back as a local op would move the instances by the frame twice.
    let (stage, plain, result) = expand_with_move(Some(("/World/TuftSamples", [100.0, 0.0, 0.0])));
    assert!(errors(&result).is_empty(), "{:?}", errors(&result));

    let cloud = sample_points(&stage);
    let plain_cloud = sample_points(&plain);
    assert_eq!(cloud.len(), plain_cloud.len(), "same number of samples");
    for (a, b) in cloud.iter().zip(plain_cloud.iter()) {
        for axis in 0..3 {
            assert!(
                (a[axis] - b[axis]).abs() < 1e-4,
                "a sample moved from {b:?} to {a:?}: the anchor is a frame, not a move"
            );
        }
    }
    for path in instance_paths(&stage) {
        let t = world_translate(&stage, &path).unwrap();
        let want = world_translate(&plain, &path).unwrap();
        for axis in 0..3 {
            assert!(
                (t[axis] - want[axis]).abs() < 1e-4,
                "{path}: axis {axis} is {} not {} (the anchor frame leaked into world)",
                t[axis],
                want[axis]
            );
        }
    }
}
// --- 4. determinism and budget ---------------------------------------------

#[test]
fn one_seed_one_pipeline_result() {
    let (a, _) = expand(MESH);
    let (b, _) = expand(MESH);
    let pa = sample_points(&a);
    let pb = sample_points(&b);
    assert_eq!(pa.len(), pb.len(), "same count");
    for (x, y) in pa.iter().zip(pb.iter()) {
        assert_eq!(x, y, "the samples are identical for one seed");
    }
    let ia: Vec<_> = instance_paths(&a)
        .iter()
        .map(|p| world_translate(&a, p))
        .collect();
    let ib: Vec<_> = instance_paths(&b)
        .iter()
        .map(|p| world_translate(&b, p))
        .collect();
    assert_eq!(ia, ib, "and so are the instances");
}

#[test]
fn a_different_sample_seed_samples_different_points() {
    let (a, _) = expand(MESH);
    let doc = syntax::parse(MESH).expect("the example parses");
    let mut doc2 = doc.clone();
    doc2.prims
        .iter_mut()
        .find(|p| p.path.as_str() == "/World/TuftSamples")
        .expect("the sample node")
        .props
        .insert("seed".to_owned(), Value::Int(999));
    let (b, _) = expand_doc(&doc2);
    let pa: Vec<_> = sample_points(&a).into_iter().take(16).collect();
    let pb: Vec<_> = sample_points(&b).into_iter().take(16).collect();
    assert_ne!(pa, pb, "a different seed samples different points");
}

#[test]
fn a_cloud_over_the_instance_budget_truncates_in_order() {
    // `max_prims` is the instances op's budget: the first `max_prims` points become
    // instances, in the cloud's order, and a `W_BUDGET` says so.
    let doc = syntax::parse(MESH).expect("the example parses");
    let mut doc2 = doc.clone();
    doc2.prims
        .iter_mut()
        .find(|p| p.path.as_str() == "/World/Tufts")
        .expect("the instances node")
        .props
        .insert("max_prims".to_owned(), Value::Int(50));
    let (stage, result) = expand_doc(&doc2);
    assert!(errors(&result).is_empty(), "{:?}", errors(&result));
    assert!(
        result.diags.iter().any(|d| d.code == "W_BUDGET"),
        "a truncated cloud warns: {:?}",
        result.diags.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
    let cloud = sample_points(&stage);
    assert_eq!(cloud.len(), 240, "the cloud itself is not truncated");
    let instances = instance_paths(&stage);
    assert_eq!(instances.len(), 50, "the instances are");
    // In the cloud's order: instance k is point k, not a random subset.
    for (k, point) in cloud.iter().take(50).enumerate() {
        let t = world_translate(&stage, &format!("/World/Tufts/i{k:06}")).unwrap();
        for axis in 0..3 {
            assert!(
                (t[axis] - point[axis]).abs() < 1e-4,
                "instance {k} is not point {k}"
            );
        }
    }
}

// --- 5. the ops' diagnostic contract ---------------------------------------

#[test]
fn a_sample_op_over_a_cloud_is_a_diagnostic_not_a_silent_vertex_copy() {
    // `RusdSamplePoints` samples a surface. Given a `Points` prim it must say so
    // (`E_INPUT_GEOMETRY`) rather than fall back to "one point per vertex", which
    // would be different geometry under the same document.
    let doc = syntax::parse(
        r#"
[stage]
default_prim = "/World"
seed = 1
up_axis = "Z"

[World]
schema = "Xform"

[World.Dots]
points = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]
schema = "Points"

[World.Sampled]
count = 10
input_points = "/World/Dots"
schema = "RusdSamplePoints"
seed = 3
"#,
    )
    .expect("the document parses");
    let (stage, result) = expand_doc(&doc);
    let codes: Vec<&str> = result.diags.iter().map(|d| d.code.as_str()).collect();
    assert!(
        codes.contains(&"E_INPUT_GEOMETRY"),
        "a cloud is not a surface: {codes:?}"
    );
    // Nothing was authored in its place: the anchor prim itself is root-layer
    // scaffolding (every generator table is lowered as a prim), but the op authored
    // no cloud under it and hid nothing.
    assert!(
        geometry::world_points(&stage, "/World/Sampled/points").is_none(),
        "a failed sample op authors no cloud"
    );
    assert!(
        visibility(&stage, "/World/Dots").is_none(),
        "and hides nothing: the input it could not read is still visible"
    );
}

#[test]
fn an_instances_op_instances_the_cloud_a_mesh_authors() {
    // A `Mesh` authors a `points` array, so it *is* a readable cloud: the instances
    // op takes it and instances the vertices. That is the written rule — a cloud is a
    // cloud — and the result is not "three instances *and* the original mesh": the
    // mesh it read is hidden, so the replacement still holds.
    let doc = syntax::parse(
        r#"
[stage]
default_prim = "/World"
seed = 1
up_axis = "Z"

[World]
schema = "Xform"

[World.Plate]
faceVertexCounts = [3]
faceVertexIndices = [0, 1, 2]
points = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]
schema = "Mesh"

[World.Prototypes]
schema = "Xform"

[World.Prototypes.Dot]
radius = 0.1
schema = "Sphere"

[World.Tufts]
input_points = "/World/Plate"
prototype = "/World/Prototypes/Dot"
schema = "RusdInstances"
seed = 2
"#,
    )
    .expect("the document parses");
    let (stage, result) = expand_doc(&doc);
    assert!(errors(&result).is_empty(), "{:?}", errors(&result));
    let instances = composed_paths(&stage)
        .into_iter()
        .filter(|p| p.starts_with("/World/Tufts/i"))
        .count();
    assert_eq!(instances, 3, "one instance per authored point");
    assert_eq!(
        visibility(&stage, "/World/Plate").as_deref(),
        Some("invisible"),
        "and the mesh it read is hidden"
    );
}

#[test]
fn an_instances_op_with_no_cloud_is_a_diagnostic() {
    let doc = syntax::parse(
        r#"
[stage]
default_prim = "/World"
seed = 1
up_axis = "Z"

[World]
schema = "Xform"

[World.Empty]
schema = "Xform"

[World.Prototypes]
schema = "Xform"

[World.Prototypes.Dot]
radius = 0.1
schema = "Sphere"

[World.Tufts]
input_points = "/World/Empty"
prototype = "/World/Prototypes/Dot"
schema = "RusdInstances"
seed = 2
"#,
    )
    .expect("the document parses");
    let (_, result) = expand_doc(&doc);
    let codes: Vec<&str> = result.diags.iter().map(|d| d.code.as_str()).collect();
    assert!(
        codes.contains(&"E_INPUT_GEOMETRY"),
        "a prim with no `points` is not a cloud: {codes:?}"
    );
} // --- 6. the declarations that make incremental ≡ full ----------------------
//
// These are about `nodes_from_doc`, not about geometry: what each op declares it
// reads and writes is the whole dependency (D2's hard invariant). A declaration that
// is missing does not fail loudly — the node memo-hits when it should have re-cooked,
// and the scene silently keeps geometry from the old input. So each one is asserted.

#[test]
fn the_two_ops_declare_the_cloud_path_between_them() {
    let doc = syntax::parse(MESH).expect("the example parses");
    let nodes = rusd_proc::nodes_from_doc(&doc);
    let sample = nodes
        .iter()
        .find(|n| n.generator == "RusdSamplePoints")
        .expect("the sample node");
    let instances = nodes
        .iter()
        .find(|n| n.generator == "RusdInstances")
        .expect("the instances node");

    // The sample op **writes** the cloud prim: that is the path the instances op names
    // as its input, and the edge that orders them.
    assert!(
        sample
            .writes
            .iter()
            .any(|w| w == "/World/TuftSamples/points"),
        "the sampler writes its cloud: {:?}",
        sample.writes
    );
    // Each op reads the geometry in the form it consumes, plus its own anchor's world
    // transform (both author in their anchor's local space).
    let reads = |n: &rusd_proc::CookNode, prim: &str, attr: &str| {
        n.reads.iter().any(|(p, a)| p == prim && a == attr)
    };
    assert!(
        reads(sample, "/World/Terrain", rusd_proc::generator::SURFACE_READ),
        "the sampler depends on the mesh's surface: {:?}",
        sample.reads
    );
    assert!(
        reads(
            sample,
            "/World/TuftSamples",
            rusd_proc::generator::WORLD_XFORM_READ
        ),
        "and on its own anchor's frame: {:?}",
        sample.reads
    );
    assert!(
        reads(instances, "/World/TuftSamples/points", "points"),
        "the instances op depends on the cloud: {:?}",
        instances.reads
    );
    assert!(
        reads(
            instances,
            "/World/Tufts",
            rusd_proc::generator::WORLD_XFORM_READ
        ),
        "and on its own anchor's frame: {:?}",
        instances.reads
    );
    assert!(
        reads(instances, "/World/Prototypes/Tuft", "radius"),
        "and on the prototype it instances: {:?}",
        instances.reads
    );
    // The sampler reads no `points` from its input: a mesh's vertices are deliberately
    // not an input (taking them would be different geometry).
    assert!(
        !reads(sample, "/World/Terrain", "points"),
        "a surface sample does not depend on the bare vertices: {:?}",
        sample.reads
    );
}

#[test]
fn the_dependency_graph_orders_the_pair_from_that_declaration_alone() {
    let doc = syntax::parse(MESH).expect("the example parses");
    let nodes = rusd_proc::nodes_from_doc(&doc);
    let deps = rusd_proc::cook::derive_deps(&nodes);
    assert!(
        deps["/World/Tufts"].contains("/World/TuftSamples"),
        "the instances op depends on the sampler through the cloud path: {deps:?}"
    );
    assert!(
        deps["/World/TuftSamples"].is_empty(),
        "the sampler depends on no node (the mesh is authored geometry)"
    );
}

#[test]
fn an_incremental_recook_equals_a_cold_cook_of_the_final_document() {
    // The pipeline's own version of "incremental ≡ full": cook, change `count`, cook
    // again — then cook the final document from scratch. Both clouds and both instance
    // sets must be identical, or a memo is stale somewhere (D2).
    let doc = syntax::parse(MESH).expect("the example parses");
    let stage = rusd_scene::lower(&doc).expect("the example lowers");
    let root = stage.root_layer().identifier().to_owned();
    let mut engine = rusd_proc::default_registry();
    let nodes = rusd_proc::nodes_from_doc(&doc);
    engine.cook(&stage, &root, &nodes);

    let mut doc2 = doc.clone();
    doc2.prims
        .iter_mut()
        .find(|p| p.path.as_str() == "/World/TuftSamples")
        .expect("the sample node")
        .props
        .insert("count".to_owned(), Value::Int(60));
    let changed = rusd_proc::nodes_from_doc(&doc2);
    let result = engine.cook(&stage, &root, &changed);
    assert!(errors(&result).is_empty(), "{:?}", errors(&result));

    let (cold, cold_result) = expand_doc(&doc2);

    let incremental_cloud = sample_points(&stage);
    let cold_cloud = sample_points(&cold);
    assert_eq!(
        incremental_cloud.len(),
        60,
        "the changed node re-cooked with the new count"
    );
    assert_eq!(
        incremental_cloud, cold_cloud,
        "incremental samples ≡ cold samples"
    );
    let incremental_instances: Vec<_> = instance_paths(&stage)
        .iter()
        .map(|p| world_translate(&stage, p))
        .collect();
    let cold_instances: Vec<_> = instance_paths(&cold)
        .iter()
        .map(|p| world_translate(&cold, p))
        .collect();
    assert_eq!(
        incremental_instances, cold_instances,
        "incremental instances ≡ cold instances"
    );
    assert_eq!(cold_result.ops.len(), 2, "the cold cook reports both ops");
}

#[test]
fn moving_the_input_re_cooks_both_ops() {
    // A declared `surface` / `points` read hashes the **world** geometry, so a moved
    // input invalidates the nodes that read it. If it hashed the authored local
    // `points` instead, this pair would memo-hit and leave the instances behind.
    let doc = syntax::parse(MESH).expect("the example parses");
    let stage = rusd_scene::lower(&doc).expect("the example lowers");
    let root = stage.root_layer().identifier().to_owned();
    let nodes = rusd_proc::nodes_from_doc(&doc);
    let mut engine = rusd_proc::default_registry();
    let first = engine.cook(&stage, &root, &nodes);
    assert_eq!(first.stats.cooked, 2, "a cold cook runs both");

    // Same document, nothing moved: both reused.
    let again = engine.cook(&stage, &root, &nodes);
    assert_eq!(again.stats.reused, 2, "an unchanged document reuses both");

    // Now raise the mesh's parent. Both ops depend on it — the sampler through the
    // surface it sampled, the instances through the cloud it reads — so both rebuild.
    move_prim(&stage, "/World/Terrain", [0.0, 0.0, 5.0]);
    let moved = engine.cook(&stage, &root, &nodes);
    assert_eq!(
        moved.stats.cooked, 2,
        "moving the input re-cooks the pair (cooked {}, reused {})",
        moved.stats.cooked, moved.stats.reused
    );
    // And the result really moved.
    for path in instance_paths(&stage) {
        let t = world_translate(&stage, &path).unwrap();
        assert!(t[2] > 5.0, "{path} did not follow the raised input: {t:?}");
    }
}
