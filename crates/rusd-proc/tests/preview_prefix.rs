//! D14 A / D14.1 acceptance — the second half: what a **pipeline prefix** sees.
//!
//! The contract's point is not just "instances appear"; it is that the input
//! geometry is *replaced without being destroyed*, so stopping the pipeline before
//! the scatter still shows it. The server implements a prefix by composing only
//! `root + the cook sublayers up to a chosen one + session overrides`
//! (`docs/PROTOCOL.md` v1.4 `preview{upto}`); this crate can check the same property
//! without a server, by composing the stack and then muting the scatter's own output
//! layer — which is exactly what "stop before this op" means in the layer stack.
//!
//! Every assertion here is about composed state: visibility, geometry extraction,
//! and where the instances are.

use openusd::sdf;
use openusd::usd::Stage;
use rusd_core::syntax;

const POINTS: &str = include_str!("../../../examples/points_scatter.rusd");
const MESH: &str = include_str!("../../../examples/mesh_scatter.rusd");

/// Lower + cook, returning the stage and the identifier of each cook output layer,
/// in the order the cook inserted them (strongest sublayer first, as the stack
/// reports it).
fn cooked(text: &str) -> (Stage, Vec<String>) {
    let doc = syntax::parse(text).expect("the example parses");
    let stage = rusd_scene::lower(&doc).expect("the example lowers");
    let root = stage.root_layer().identifier().to_owned();
    let nodes = rusd_proc::nodes_from_doc(&doc);
    let mut engine = rusd_proc::default_registry();
    let result = engine.cook(&stage, &root, &nodes);
    assert!(
        result
            .diags
            .iter()
            .all(|d| d.severity != rusd_core::Severity::Error),
        "the example cooks clean: {:?}",
        result.diags
    );
    let ops: Vec<String> = stage
        .layer_stack()
        .into_iter()
        .filter(|id| *id != root)
        .collect();
    (stage, ops)
}

fn visibility(stage: &Stage, path: &str) -> Option<String> {
    let prim = stage.prim(sdf::path(path).ok()?).ok()?;
    match prim.attribute("visibility").get::<sdf::Value>().ok()?? {
        sdf::Value::Token(t) => Some(t.as_str().to_owned()),
        _ => None,
    }
}

fn draws(stage: &Stage, path: &str) -> bool {
    rusd_scene::geometry::extract(stage)
        .items
        .iter()
        .any(|i| i.path == path)
}

// --- points ---------------------------------------------------------------

#[test]
fn a_prefix_before_the_scatter_shows_the_cloud_and_the_full_stack_shows_the_instances() {
    let (stage, ops) = cooked(POINTS);
    assert_eq!(ops.len(), 1, "one generator node: {ops:?}");

    // Full stack: the cloud is replaced by its instances.
    assert!(draws(&stage, "/World/Spots/i000000"), "instances draw");
    assert_eq!(
        visibility(&stage, "/World/Dots").as_deref(),
        Some("invisible"),
        "the full stack hides the cloud"
    );
    assert!(!draws(&stage, "/World/Dots"), "and draws none of it");

    // Prefix: mute the op layer the scatter wrote, i.e. stop the stack before it.
    // The cloud is visible again — because nothing ever deleted it.
    stage.mute_layer(&ops[0]);
    assert!(
        visibility(&stage, "/World/Dots").is_none(),
        "with the scatter's layer muted, no visibility opinion is in strength"
    );
    assert!(draws(&stage, "/World/Dots"), "the prefix shows the cloud");
    assert!(
        !draws(&stage, "/World/Spots/i000000"),
        "and no instances, because the node that makes them is not cooking"
    );

    // And back: unmuting restores the full stack exactly.
    stage.unmute_layer(&ops[0]);
    assert_eq!(
        visibility(&stage, "/World/Dots").as_deref(),
        Some("invisible"),
        "unmuting brings the replacement back"
    );
    assert!(draws(&stage, "/World/Spots/i000000"));
}

// --- mesh -----------------------------------------------------------------

#[test]
fn a_prefix_before_a_surface_scatter_shows_the_surface() {
    let (stage, ops) = cooked(MESH);
    assert_eq!(ops.len(), 1, "one generator node: {ops:?}");

    assert!(draws(&stage, "/World/Tufts/i000000"), "instances draw");
    assert_eq!(
        visibility(&stage, "/World/Terrain").as_deref(),
        Some("invisible"),
        "the full stack hides the surface"
    );
    assert!(!draws(&stage, "/World/Terrain"), "and draws none of it");

    stage.mute_layer(&ops[0]);
    assert!(
        visibility(&stage, "/World/Terrain").is_none(),
        "a prefix has no visibility opinion to obey"
    );
    assert!(
        draws(&stage, "/World/Terrain"),
        "the prefix shows the surface"
    );
    assert!(!draws(&stage, "/World/Tufts/i000000"));

    stage.unmute_layer(&ops[0]);
    assert_eq!(
        visibility(&stage, "/World/Terrain").as_deref(),
        Some("invisible")
    );
}

#[test]
fn the_inputs_own_layer_still_authors_its_geometry_after_cooking() {
    // "Replaced, not destroyed": the spec's own values are untouched by the
    // override, so anything reading the input layer (a prefix, an exporter, a
    // diff) still sees the authored geometry.
    let (stage, _) = cooked(MESH);
    let doc = syntax::parse(MESH).unwrap();
    let root = stage.root_layer().identifier().to_owned();
    let layer = stage.layer(&root).expect("the root layer");
    let _ = doc;
    let specifier = layer.data().try_field(
        &sdf::path("/World/Terrain").unwrap(),
        sdf::FieldKey::Specifier.as_str(),
    );
    let specifier = specifier.ok().flatten().map(|v| v.into_owned());
    assert!(
        matches!(specifier, Some(sdf::Value::Specifier(sdf::Specifier::Def))),
        "the input prim is still a `def` in its own layer: {specifier:?}"
    );
    // The points are still authored there too.
    let points = layer.data().try_field(
        &sdf::path("/World/Terrain.points").unwrap(),
        sdf::FieldKey::Default.as_str(),
    );
    assert!(
        points.ok().flatten().is_some(),
        "the input layer still authors its vertices"
    );
}

// --- mesh wins over its own vertices --------------------------------------

#[test]
fn a_mesh_input_is_sampled_as_a_surface_not_as_its_vertices() {
    // A `Mesh` authors `points`, so a reader that took the cloud path would place
    // one instance per *vertex* (9 here) instead of `count` on the surface (240).
    // That difference is what this asserts.
    let (stage, _) = cooked(MESH);
    let vertices = match stage
        .prim(sdf::path("/World/Terrain").unwrap())
        .unwrap()
        .attribute("points")
        .get::<sdf::Value>()
        .unwrap()
        .unwrap()
    {
        sdf::Value::Vec3fVec(v) => v.len(),
        other => panic!("the mesh authors vertices: {other:?}"),
    };
    let mut n = 0;
    let _ = stage.traverse(openusd::usd::PrimPredicate::ALL, |p| {
        if p.as_str().starts_with("/World/Tufts/i") {
            n += 1;
        }
    });
    assert_eq!(vertices, 9, "the example's mesh has 9 vertices");
    assert_eq!(
        n, 240,
        "the surface is sampled `count` times, not per vertex"
    );
}
