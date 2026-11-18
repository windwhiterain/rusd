//! D14 A / D14.1 acceptance: `RusdScatter` must be **driven by input geometry**,
//! not by a parameter domain (user ruling, `docs/PROTOCOL.md` v1.4.1).
//!
//! Two input forms, two rules:
//!
//! - a **point cloud** (`UsdGeom.Points`) gives one instance per point, and each
//!   instance's composed world position is that point's — ancestors' transforms
//!   included, since the point is read in world space and written back as a local
//!   `xformOp:translate`;
//! - a **surface** (`UsdGeom.Mesh`) gives exactly `count` instances, each on the
//!   mesh — the distance from an instance to the nearest triangle is zero (to
//!   floating-point noise), which is what "area-weighted, on the surface" means.
//!
//! And one rule about what the *stage* shows: the geometry the scatter replaced is
//! hidden by a **visibility override**, not deleted — the input's own spec is still
//! in its layer, so a pipeline prefix that stops before the scatter shows it again.
//!
//! Everything here drives the real cook engine over a real document: the input
//! prims are authored in the `.rusd` file, lowered by `rusd_scene`, and the scatter
//! node is cooked by `rusd_proc` exactly as `rusd expand` does it.

use openusd::sdf::{self, Value as SdfValue};
use openusd::usd::{PrimPredicate, Stage};
use rusd_core::Value;
use rusd_core::syntax::{self, ParsedDocument};
use rusd_scene::geometry::{self, Triangle};

const POINTS: &str = include_str!("../../../examples/points_scatter.rusd");
const MESH: &str = include_str!("../../../examples/mesh_scatter.rusd");

/// Lower + cook a document the way `rusd expand` does.
fn expand(text: &str) -> (Stage, rusd_proc::CookResult) {
    let doc = syntax::parse(text).expect("the example parses");
    let stage = rusd_scene::lower(&doc).expect("the example lowers");
    let root = stage.root_layer().identifier().to_owned();
    let nodes = rusd_proc::nodes_from_doc(&doc);
    let mut engine = rusd_proc::default_registry();
    let result = engine.cook(&stage, &root, &nodes);
    (stage, result)
}

fn expand_doc(doc: &ParsedDocument) -> (Stage, rusd_proc::CookResult) {
    let stage = rusd_scene::lower(doc).expect("the document lowers");
    let root = stage.root_layer().identifier().to_owned();
    let nodes = rusd_proc::nodes_from_doc(doc);
    let mut engine = rusd_proc::default_registry();
    let result = engine.cook(&stage, &root, &nodes);
    (stage, result)
}

/// Every prim path the stage composes, in traversal order.
fn composed_paths(stage: &Stage) -> Vec<String> {
    let mut paths = Vec::new();
    let _ = stage.traverse(PrimPredicate::ALL, |p| paths.push(p.as_str().to_owned()));
    paths
}

/// A prim's composed world `xformOp:translate`, read off the **composed** stage.
fn world_translate(stage: &Stage, path: &str) -> Option<[f64; 3]> {
    let prim = stage.prim(sdf::path(path).ok()?).ok()?;
    match prim
        .attribute("xformOp:translate")
        .get::<SdfValue>()
        .ok()??
    {
        SdfValue::Vec3f(v) => Some([v[0] as f64, v[1] as f64, v[2] as f64]),
        SdfValue::Vec3d(v) => Some([v[0], v[1], v[2]]),
        _ => None,
    }
}

/// The composed value of an attribute (`visibility`, `points`, …).
fn attr(stage: &Stage, path: &str, name: &str) -> Option<SdfValue> {
    let prim = stage.prim(sdf::path(path).ok()?).ok()?;
    prim.attribute(name).get::<SdfValue>().ok()?
}

/// Author `xformOp:translate` on a prim in the stage's root layer — the same two
/// attributes an `opinion` writes, and the way a prim is moved in this pipeline
/// (`docs/PROTOCOL.md`: the surface has no `xformOp:*` property, SPEC §1).
///
/// Used to build the *input* of a geometry test: a point cloud or a mesh under a
/// translated parent, which is what makes "read in world space" a testable claim.
fn move_prim(stage: &Stage, path: &str, t: [f64; 3]) {
    let root = stage.root_layer().identifier().to_owned();
    let mut layer = stage.layer_mut(&root).expect("the root layer");
    layer
        .edit(|l| {
            let d = l.data_mut();
            let p = openusd::sdf::path(path)?;
            openusd::sdf::PrimSpec::over(d, p.clone())?;
            let tp = format!("{path}.xformOp:translate");
            let mut ta = openusd::sdf::AttributeSpec::new(
                d,
                tp.as_str(),
                "point3f",
                openusd::sdf::Variability::Uniform,
                false,
            )?;
            ta.set_default(SdfValue::Vec3f(openusd::gf::vec3f(
                t[0] as f32,
                t[1] as f32,
                t[2] as f32,
            )))?;
            let op = format!("{path}.xformOpOrder");
            let mut oa = openusd::sdf::AttributeSpec::new(
                d,
                op.as_str(),
                "token[]",
                openusd::sdf::Variability::Uniform,
                false,
            )?;
            oa.set_default(SdfValue::TokenVec(vec!["xformOp:translate".into()]))?;
            Ok(())
        })
        .expect("the prim can be moved");
}

/// The world-space triangle nearest to `p`, and the distance to it.
fn nearest_surface(p: [f64; 3], tris: &[Triangle]) -> ([f64; 3], f64) {
    let mut best = (tris[0], f64::INFINITY);
    for t in tris {
        let d = distance_to_triangle(p, *t);
        if d < best.1 {
            best = (*t, d);
        }
    }
    (geometry::triangle_centroid(best.0), best.1)
}

/// Distance from a point to a triangle's plane-projected interior: the standard
/// barycentric closest-point test (Ericson, *Real-Time Collision Detection* §5.1.5).
fn distance_to_triangle(p: [f64; 3], t: Triangle) -> f64 {
    let sub = |a: [f64; 3], b: [f64; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    let dot = |a: [f64; 3], b: [f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let ab = sub(t[1], t[0]);
    let ac = sub(t[2], t[0]);
    let ap = sub(p, t[0]);
    let d1 = dot(ab, ap);
    let d2 = dot(ac, ap);
    let closest = if d1 <= 0.0 && d2 <= 0.0 {
        t[0]
    } else {
        let bp = sub(p, t[1]);
        let d3 = dot(ab, bp);
        let d4 = dot(ac, bp);
        if d3 >= 0.0 && d4 <= d3 {
            t[1]
        } else {
            let vc = d1 * d4 - d3 * d2;
            let cp = sub(p, t[2]);
            let d5 = dot(ab, cp);
            let d6 = dot(ac, cp);
            if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
                let v = d1 / (d1 - d3);
                [
                    t[0][0] + v * ab[0],
                    t[0][1] + v * ab[1],
                    t[0][2] + v * ab[2],
                ]
            } else if d6 >= 0.0 && d5 <= d6 {
                t[2]
            } else {
                let vb = d5 * d2 - d1 * d6;
                if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
                    let w = d2 / (d2 - d6);
                    [
                        t[0][0] + w * ac[0],
                        t[0][1] + w * ac[1],
                        t[0][2] + w * ac[2],
                    ]
                } else {
                    let va = d3 * d6 - d5 * d4;
                    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
                        let w = (d4 - d3) / ((d4 - d3) + (d5 - d6));
                        [
                            t[1][0] + w * (t[2][0] - t[1][0]),
                            t[1][1] + w * (t[2][1] - t[1][1]),
                            t[1][2] + w * (t[2][2] - t[1][2]),
                        ]
                    } else {
                        let denom = 1.0 / (va + vb + vc);
                        let v = vb * denom;
                        let w = vc * denom;
                        [
                            t[0][0] + ab[0] * v + ac[0] * w,
                            t[0][1] + ab[1] * v + ac[1] * w,
                            t[0][2] + ab[2] * v + ac[2] * w,
                        ]
                    }
                }
            }
        }
    };
    let d = sub(p, closest);
    dot(d, d).sqrt()
}

// --- points: one instance per point, each at that point -------------------

#[test]
fn points_become_one_instance_each_at_the_points_own_position() {
    let (stage, result) = expand(POINTS);
    assert!(
        result
            .diags
            .iter()
            .all(|d| d.severity != rusd_core::Severity::Error),
        "the example must cook clean: {:?}",
        result.diags
    );
    // The cloud the example authors, read from the input prim rather than assumed.
    let authored = match attr(&stage, "/World/Dots", "points").expect("the input authors points") {
        SdfValue::Vec3fVec(v) => v
            .iter()
            .map(|p| [p[0] as f64, p[1] as f64, p[2] as f64])
            .collect::<Vec<_>>(),
        other => panic!("points is a point3f[]: {other:?}"),
    };
    assert_eq!(authored.len(), 5, "the example's cloud");

    let paths = composed_paths(&stage);
    let instances: Vec<&String> = paths
        .iter()
        .filter(|p| p.starts_with("/World/Spots/i"))
        .collect();
    assert_eq!(
        instances.len(),
        authored.len(),
        "one instance per point, no sampling: {instances:?}"
    );

    // Every point has an instance at exactly its own world position: the instance
    // set and the point set are the same set, in the cloud's own order.
    for (k, want) in authored.iter().enumerate() {
        let path = format!("/World/Spots/i{k:06}");
        let got =
            world_translate(&stage, &path).unwrap_or_else(|| panic!("{path} has no translate"));
        for axis in 0..3 {
            assert!(
                (got[axis] - want[axis]).abs() < 1e-6,
                "{path}: axis {axis} is {} not {}",
                got[axis],
                want[axis]
            );
        }
    }
}

#[test]
fn a_points_instance_composes_the_prototype_through_an_arc() {
    // Geometry out means real instances: each one carries the prototype's type,
    // attributes and children, not a copy of its numbers.
    let (stage, _) = expand(POINTS);
    let inst = stage
        .prim(sdf::path("/World/Spots/i000000").unwrap())
        .expect("an instance composes");
    assert_eq!(
        inst.type_name().unwrap().map(|t| t.to_string()).as_deref(),
        Some("Sphere"),
        "the instance's type comes from the prototype"
    );
    assert_eq!(
        inst.attribute("radius").get::<f64>().unwrap(),
        Some(0.6),
        "the instance's radius comes from the prototype"
    );
}

#[test]
fn the_points_prim_is_hidden_by_a_visibility_override_not_deleted() {
    let (stage, _) = expand(POINTS);
    // Composed: the input is invisible, so the full stack shows the instances alone.
    assert_eq!(
        attr(&stage, "/World/Dots", "visibility"),
        Some(SdfValue::Token("invisible".into())),
        "the scatter hides the geometry it replaced"
    );
    // Its own spec is intact: the points are still authored in the input layer.
    match attr(&stage, "/World/Dots", "points") {
        Some(SdfValue::Vec3fVec(points)) => assert_eq!(points.len(), 5),
        other => panic!("the input layer still authors its points: {other:?}"),
    }
    // And the geometry extraction honours it: the cloud draws nothing once replaced.
    let geo = geometry::extract(&stage);
    assert!(
        !geo.items.iter().any(|i| i.path == "/World/Dots"),
        "an invisible prim contributes no geometry: {:?}",
        geo.items.iter().map(|i| &i.path).collect::<Vec<_>>()
    );
}

#[test]
fn the_visibility_override_survives_the_tier_a_snapshot() {
    // The viewer only ever sees the flattened snapshot (Tier A, PROTOCOL §4), so the
    // replacement has to be in it — not just in the server's stack.
    let (stage, _) = expand(POINTS);
    let flat = stage.flatten().expect("the stack flattens");
    let fmt = sdf::LayerRegistry::find_by_id("usdc").expect("usdc is registered");
    let written = fmt.write_bytes(flat.data()).expect("the snapshot writes");
    let layer = sdf::Layer::from_bytes("snapshot.usdc", written.bytes).expect("it decodes");
    let client = Stage::builder().in_memory("client_root.usda").unwrap();
    let cid = client.root_layer().identifier().to_owned();
    client
        .insert_layer(&cid, 0, layer, sdf::LayerOffset::IDENTITY)
        .expect("the snapshot composes");
    assert_eq!(
        attr(&client, "/World/Dots", "visibility"),
        Some(SdfValue::Token("invisible".into())),
        "the snapshot carries the replacement"
    );
    assert!(
        world_translate(&client, "/World/Spots/i000000").is_some(),
        "and the instances compose on the client"
    );
    let geo = geometry::extract(&client);
    assert!(
        !geo.items.iter().any(|i| i.path == "/World/Dots"),
        "a client's prefixes/extraction agrees with the server's"
    );
}

// --- the same cloud, different seed: still deterministic -------------------

#[test]
fn the_same_document_cooks_to_the_same_instances() {
    let (a, _) = expand(POINTS);
    let (b, _) = expand(POINTS);
    let ta: Vec<_> = (0..5)
        .map(|k| world_translate(&a, &format!("/World/Spots/i{k:06}")))
        .collect();
    let tb: Vec<_> = (0..5)
        .map(|k| world_translate(&b, &format!("/World/Spots/i{k:06}")))
        .collect();
    assert_eq!(ta, tb, "one document, one result");
}

// --- mesh: exactly `count` instances, all on the surface ------------------

#[test]
fn a_mesh_gives_count_instances_all_on_its_surface() {
    let (stage, result) = expand(MESH);
    assert!(
        result
            .diags
            .iter()
            .all(|d| d.severity != rusd_core::Severity::Error),
        "the mesh example must cook clean: {:?}",
        result.diags
    );
    let count = match syntax::parse(MESH)
        .unwrap()
        .prims
        .iter()
        .find(|p| p.path.as_str() == "/World/Tufts")
        .expect("the scatter node")
        .props
        .get("count")
    {
        Some(Value::Int(i)) => *i as usize,
        other => panic!("count is an int: {other:?}"),
    };

    let instances: Vec<String> = composed_paths(&stage)
        .into_iter()
        .filter(|p| p.starts_with("/World/Tufts/i"))
        .collect();
    assert_eq!(
        instances.len(),
        count,
        "a surface scatter places exactly `count` instances, got {}",
        instances.len()
    );

    let tris = geometry::world_triangles(&stage, "/World/Terrain")
        .expect("the input mesh's triangles are readable");
    assert!(!tris.is_empty(), "the input mesh has surface");

    let mut worst = 0.0_f64;
    for path in &instances {
        let p = world_translate(&stage, path)
            .unwrap_or_else(|| panic!("{path} has no composed translate"));
        let (_, d) = nearest_surface(p, &tris);
        worst = worst.max(d);
        assert!(
            d < 1e-4,
            "{path} at {p:?} is {d} away from the input surface"
        );
    }
    eprintln!("worst distance from an instance to the surface: {worst:e}");
}

#[test]
fn a_mesh_instance_is_inside_the_inputs_footprint() {
    // "On the surface" also means "inside the mesh's own extent": a sampler that
    // picked the wrong triangle (or extrapolated past one) would show up here.
    let (stage, _) = expand(MESH);
    let tris = geometry::world_triangles(&stage, "/World/Terrain").unwrap();
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    for t in &tris {
        for p in t {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
    }
    for path in composed_paths(&stage)
        .into_iter()
        .filter(|p| p.starts_with("/World/Tufts/i"))
    {
        let p = world_translate(&stage, &path).unwrap();
        for k in 0..3 {
            assert!(
                p[k] >= lo[k] - 1e-6 && p[k] <= hi[k] + 1e-6,
                "{path} {p:?} escapes the mesh box [{lo:?}, {hi:?}] on axis {k}"
            );
        }
    }
}

#[test]
fn the_mesh_is_hidden_after_the_scatter_and_intact_before_it() {
    let (stage, _) = expand(MESH);
    assert_eq!(
        attr(&stage, "/World/Terrain", "visibility"),
        Some(SdfValue::Token("invisible".into())),
        "the replaced surface is hidden"
    );
    // The spec is still there — a prefix of the pipeline sees it, which is exactly
    // what the visibility override (rather than a delete or an inactive flag) buys.
    assert!(
        attr(&stage, "/World/Terrain", "points").is_some(),
        "the input layer still authors its vertices"
    );
    let geo = geometry::extract(&stage);
    assert!(
        !geo.items.iter().any(|i| i.path == "/World/Terrain"),
        "the hidden mesh contributes no geometry"
    );
}

#[test]
fn a_mesh_scatter_is_deterministic_for_one_seed() {
    let (a, _) = expand(MESH);
    let (b, _) = expand(MESH);
    let ta: Vec<_> = (0..20)
        .map(|k| world_translate(&a, &format!("/World/Tufts/i{k:06}")))
        .collect();
    let tb: Vec<_> = (0..20)
        .map(|k| world_translate(&b, &format!("/World/Tufts/i{k:06}")))
        .collect();
    assert_eq!(ta, tb, "same seed, same scene");
}

#[test]
fn a_different_seed_moves_the_instances() {
    // The sampling is seeded: two seeds over one mesh must not be one picture, or the
    // "seed" parameter would be a lie (and `W_`-free but useless).
    let (a, _) = expand(MESH);
    let doc = syntax::parse(MESH).unwrap();
    let mut doc2 = doc.clone();
    doc2.prims
        .iter_mut()
        .find(|p| p.path.as_str() == "/World/Tufts")
        .unwrap()
        .props
        .insert("seed".to_owned(), Value::Int(999));
    let (b, _) = expand_doc(&doc2);
    let ta: Vec<_> = (0..10)
        .map(|k| world_translate(&a, &format!("/World/Tufts/i{k:06}")))
        .collect();
    let tb: Vec<_> = (0..10)
        .map(|k| world_translate(&b, &format!("/World/Tufts/i{k:06}")))
        .collect();
    assert_ne!(ta, tb, "a different seed samples different points");
}

#[test]
fn area_weighting_favours_the_larger_triangle() {
    // Two triangles: a 10×10 one and a 1×1 one. Sampling by area means the big one
    // gets ~100× the instances; sampling by *triangle count* would give half each.
    // The assertion is deliberately loose (a 3× separation, where the truth is 99×)
    // so a good sampler cannot fail it by catching a few of the small triangle's
    // instances, and a bad one cannot pass it.
    let doc = syntax::parse(
        r#"
[stage]
default_prim = "/World"
seed = 1
up_axis = "Z"

[World]
schema = "Xform"

[World.Plate]
faceVertexCounts = [3, 3]
faceVertexIndices = [0, 1, 2, 3, 4, 5]
points = [
    [0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0],
    [20.0, 0.0, 0.0], [21.0, 0.0, 0.0], [20.0, 1.0, 0.0],
]
schema = "Mesh"

[World.Prototypes]
schema = "Xform"

[World.Prototypes.Dot]
radius = 0.1
schema = "Sphere"

[World.Tufts]
count = 400
input_points = "/World/Plate"
prototype = "/World/Prototypes/Dot"
schema = "RusdScatter"
seed = 3
"#,
    )
    .expect("the document parses");
    let (stage, _) = expand_doc(&doc);
    let big = 0;
    let small = 0;
    let (mut in_big, mut in_small) = (big, small);
    for path in composed_paths(&stage)
        .into_iter()
        .filter(|p| p.starts_with("/World/Tufts/i"))
    {
        let p = world_translate(&stage, &path).unwrap();
        // The big triangle lies in x ∈ [0,10], y ∈ [0,10]; the small one at x ≈ 20.
        if p[0] < 15.0 {
            in_big += 1;
        } else {
            in_small += 1;
        }
    }
    assert_eq!(in_big + in_small, 400, "every instance is counted");
    assert!(
        in_big > in_small * 3,
        "area weighting: the 100× triangle must dominate, got {in_big} vs {in_small}"
    );
}

// --- the parametric fallback is untouched ---------------------------------

#[test]
fn without_input_points_the_scatter_still_samples_its_domain() {
    // D14 A's compatibility clause: a scatter with no input geometry keeps its
    // parameter-driven behaviour, byte for byte. The forest example is the witness.
    let text = include_str!("../../../examples/forest.rusd");
    let doc = syntax::parse(text).unwrap();
    assert!(
        !doc.prims
            .iter()
            .find(|p| p.path.as_str() == "/World/Forest")
            .unwrap()
            .props
            .contains_key("input_points"),
        "the forest scatter is parametric"
    );
    let (stage, result) = expand_doc(&doc);
    assert!(
        result
            .diags
            .iter()
            .all(|d| d.severity != rusd_core::Severity::Error)
    );
    let instances: Vec<String> = composed_paths(&stage)
        .into_iter()
        .filter(|p| p.starts_with("/World/Forest/i"))
        .collect();
    assert_eq!(instances.len(), 500, "500 parametric instances");
    // No visibility override anywhere: nothing was replaced.
    assert_eq!(attr(&stage, "/World/Prototypes/Pine", "visibility"), None);
    let geo = geometry::extract(&stage);
    assert_eq!(
        geo.items
            .iter()
            .filter(|i| i.path.starts_with("/World/Forest/i"))
            .count(),
        500,
        "and they draw"
    );
}

// --- diagnostics ----------------------------------------------------------

#[test]
fn a_bad_input_geometry_is_a_diagnostic_not_a_panic() {
    let doc = syntax::parse(
        r#"
[stage]
default_prim = "/World"
seed = 1
up_axis = "Z"

[World]
schema = "Xform"

[World.Missing]
schema = "Xform"

[World.Prototypes]
schema = "Xform"

[World.Prototypes.Dot]
radius = 0.1
schema = "Sphere"

[World.Tufts]
count = 10
input_points = "/World/Missing"
prototype = "/World/Prototypes/Dot"
schema = "RusdScatter"
seed = 3
"#,
    )
    .expect("the document parses");
    let (_stage, result) = expand_doc(&doc);
    let diag = result
        .diags
        .iter()
        .find(|d| d.code == "E_INPUT_GEOMETRY")
        .unwrap_or_else(|| panic!("a clear error, got {:?}", result.diags));
    assert!(diag.message.contains("/World/Missing"), "{diag:?}");
}

#[test]
fn a_cloud_over_budget_truncates_deterministically() {
    let doc = syntax::parse(
        r#"
[stage]
default_prim = "/World"
seed = 1
up_axis = "Z"

[World]
schema = "Xform"

[World.Dots]
points = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]]
schema = "Points"

[World.Prototypes]
schema = "Xform"

[World.Prototypes.Dot]
radius = 0.1
schema = "Sphere"

[World.Tufts]
input_points = "/World/Dots"
max_prims = 2
prototype = "/World/Prototypes/Dot"
schema = "RusdScatter"
seed = 3
"#,
    )
    .expect("the document parses");
    let (stage, result) = expand_doc(&doc);
    assert!(
        result.diags.iter().any(|d| d.code == "W_BUDGET"),
        "{:?}",
        result.diags
    );
    let instances: Vec<String> = composed_paths(&stage)
        .into_iter()
        .filter(|p| p.starts_with("/World/Tufts/i"))
        .collect();
    assert_eq!(instances.len(), 2, "the budget bounds the instances");
    // The first two points of the cloud, in its own order: i0, i1.
    assert_eq!(
        world_translate(&stage, "/World/Tufts/i000000"),
        Some([0.0, 0.0, 0.0])
    );
    assert_eq!(
        world_translate(&stage, "/World/Tufts/i000001"),
        Some([1.0, 0.0, 0.0])
    );
}

// --- geometry in respects world transforms --------------------------------

#[test]
fn points_are_read_in_world_space_and_written_back_as_local_ops() {
    // The input sits under a translated block; the scatter's anchor is translated
    // too. An instance's composed world position must still be the point's, i.e.
    // exactly one parent translation applies — writing a world value as a local op
    // would apply it twice (the "原点加两遍" bug class).
    let doc = syntax::parse(
        r#"
[stage]
default_prim = "/World"
seed = 1
up_axis = "Z"

[World]
schema = "Xform"

[World.Block]
schema = "Xform"

[World.Block.Dots]
points = [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]]
schema = "Points"

[World.Prototypes]
schema = "Xform"

[World.Prototypes.Dot]
radius = 0.1
schema = "Sphere"

[World.Anchor]
schema = "Xform"

[World.Anchor.Tufts]
input_points = "/World/Block/Dots"
prototype = "/World/Prototypes/Dot"
schema = "RusdScatter"
seed = 3
"#,
    )
    .expect("the document parses");
    // Both parents are moved in the stage: the surface syntax has no `xformOp:*`
    // property (SPEC §1), and these two transforms are the whole point of the test.
    let stage = rusd_scene::lower(&doc).expect("the document lowers");
    move_prim(&stage, "/World/Block", [100.0, 0.0, 0.0]);
    move_prim(&stage, "/World/Anchor", [0.0, 50.0, 0.0]);
    let root = stage.root_layer().identifier().to_owned();
    let nodes = rusd_proc::nodes_from_doc(&doc);
    let mut engine = rusd_proc::default_registry();
    let result = engine.cook(&stage, &root, &nodes);
    let stage = &stage;
    assert!(
        result
            .diags
            .iter()
            .all(|d| d.severity != rusd_core::Severity::Error),
        "{:?}",
        result.diags
    );
    // The world positions: the block's translate applied once to each authored point.
    let want = [[101.0, 2.0, 3.0], [104.0, 5.0, 6.0]];
    let geo = geometry::extract(stage);
    for (k, w) in want.iter().enumerate() {
        let path = format!("/World/Anchor/Tufts/i{k:06}");
        let item = geo
            .items
            .iter()
            .find(|i| i.path == path)
            .unwrap_or_else(|| {
                panic!(
                    "{path} has no geometry; items are {:?}",
                    geo.items.iter().map(|i| &i.path).collect::<Vec<_>>()
                )
            });
        // The item's world transform's translation column is the instance's world spot.
        let t = &item.transform;
        for axis in 0..3 {
            assert!(
                (t[12 + axis] - w[axis]).abs() < 1e-4,
                "{path}: axis {axis} is {} not {} (parent translate applied twice?)",
                t[12 + axis],
                w[axis]
            );
        }
    }
}

#[test]
fn mesh_triangles_are_read_in_world_space() {
    let doc = syntax::parse(
        r#"
[stage]
default_prim = "/World"
seed = 1
up_axis = "Z"

[World]
schema = "Xform"

[World.Block]
schema = "Xform"

[World.Block.Plate]
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
count = 20
input_points = "/World/Block/Plate"
prototype = "/World/Prototypes/Dot"
schema = "RusdScatter"
seed = 3
"#,
    )
    .expect("the document parses");
    let stage = rusd_scene::lower(&doc).expect("the document lowers");
    move_prim(&stage, "/World/Block", [0.0, 0.0, 7.0]);
    let root = stage.root_layer().identifier().to_owned();
    let nodes = rusd_proc::nodes_from_doc(&doc);
    let mut engine = rusd_proc::default_registry();
    engine.cook(&stage, &root, &nodes);
    let stage = &stage;
    let tris = geometry::world_triangles(stage, "/World/Block/Plate").expect("triangles");
    for t in &tris {
        for p in t {
            assert!(
                (p[2] - 7.0).abs() < 1e-9,
                "the block's translate is in the triangle: {p:?}"
            );
        }
    }
    // And the instances land on that raised surface, not on z = 0.
    for path in composed_paths(stage)
        .into_iter()
        .filter(|p| p.starts_with("/World/Tufts/i"))
    {
        let p = world_translate(stage, &path).unwrap();
        assert!(
            (p[2] - 7.0).abs() < 1e-4,
            "{path} is not on the raised plate: {p:?}"
        );
    }
}
