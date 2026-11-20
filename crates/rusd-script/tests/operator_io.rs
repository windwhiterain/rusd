//! **The operators' declarations, read back** (SPEC D15): the port tables the cook
//! checks are built from the Rust types the scripts' signatures use, and the graph a
//! script writes is the graph the parser reads.

use rusd_script::graph::{Graph, out};
use rusd_script::ops::{self, InstancesIn, InstancesParams, SamplePointsIn, SamplePointsParams};
use rusd_script::schema::{Cube, Xform};
use rusd_script::shape::{Shape, ShapeOf};
use rusd_scene::operator::PropType;

/// The `ty` a port asks for, when it asks for one prim.
fn prim_ty(shape: &Shape) -> Option<&str> {
    match shape {
        Shape::Prim(declared) => Some(&declared.ty),
        _ => None,
    }
}

#[test]
fn the_port_tables_are_the_operators_types() {
    // What the static tables used to say, now built from `Prim<Gprim>`, `Prim<Points>`,
    // `Prim<Xformable>`, `List<Prim<Untyped>>` and the street grid's nested structs.
    let inputs = ops::inputs("RusdSamplePoints");
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].name, "domain");
    assert_eq!(prim_ty(&inputs[0].shape), Some("Gprim"));

    let inputs = ops::inputs("RusdInstances");
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0].name, "points");
    assert_eq!(prim_ty(&inputs[0].shape), Some("Points"));
    assert_eq!(inputs[1].name, "prototype");
    assert_eq!(prim_ty(&inputs[1].shape), Some("Xformable"));

    // A grid reads nothing: its function takes no input struct.
    assert!(ops::inputs("RusdGrid").is_empty());

    // A sampler writes one `points` prim; an instances node writes a run of prims that
    // author no type of their own.
    let outputs = ops::outputs("RusdSamplePoints");
    assert_eq!(outputs[0].name, "points");
    assert_eq!(prim_ty(&outputs[0].shape), Some("Points"));
    let outputs = ops::outputs("RusdInstances");
    assert_eq!(outputs[0].name, "i*");
    let Shape::List(element) = &outputs[0].shape else { panic!("instances are a run") };
    assert_eq!(prim_ty(element), Some(""), "an instance's type composes through its arc");

    // A street grid writes a struct: the ground beside a run of blocks, each block a
    // run of lots. A struct in a list in a struct, however deep.
    let outputs = ops::outputs("RusdStreetGrid");
    assert_eq!(outputs[0].name, "Ground");
    assert_eq!(prim_ty(&outputs[0].shape), Some("Cube"));
    assert_eq!(outputs[1].name, "b*");
    let Shape::List(block) = &outputs[1].shape else { panic!("blocks are a run") };
    let Shape::Struct(lots) = block.as_ref() else { panic!("a block is a struct") };
    assert_eq!(lots[0].name, "lot*");
    let Shape::List(lot) = &lots[0].shape else { panic!("lots are a run") };
    assert_eq!(prim_ty(lot), Some("Cube"));
}

/// **The params a script writes are the params the TOML table declares.** Two surfaces
/// of one operator — a Rust struct and a `PropDef` table — so every entry the struct
/// emits must name a declared parameter, of the declared type.
#[test]
fn the_params_structs_agree_with_the_parameter_tables() {
    fn check(operator: &str, entries: std::collections::BTreeMap<String, rusd_core::Value>) {
        let table = rusd_scene::operator::parameters(operator).expect("a known operator");
        for (name, value) in entries {
            let def = table
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("`{operator}` declares no parameter `{name}`"));
            let ok = match (def.ty, &value) {
                (PropType::U64 | PropType::I64, rusd_core::Value::Int(_)) => true,
                (PropType::F64, rusd_core::Value::Float(_)) => true,
                (PropType::String | PropType::Token | PropType::Path | PropType::Asset, rusd_core::Value::String(_)) => true,
                (PropType::Distribution | PropType::Dict, rusd_core::Value::Dict(_)) => true,
                (PropType::Distribution, rusd_core::Value::Float(_)) => true,
                (PropType::Vec2, rusd_core::Value::Vec2(_)) => true,
                (PropType::Vec3 | PropType::Color3f, rusd_core::Value::Vec3(_)) => true,
                _ => false,
            };
            assert!(ok, "`{operator}.{name}` is {:?} in the table and {value:?} from the script", def.ty);
        }
    }
    use rusd_script::ops::{Dist, GridParams, Params, ScatterParams, StreetGridParams};
    check("RusdSamplePoints", SamplePointsParams { count: 60, seed: 11 }.entries());
    check("RusdInstances", InstancesParams { scale: Some(Dist::Uniform { min: 0.8, max: 1.4 }), max_prims: 5000, seed: 11 }.entries());
    check("RusdScatter", ScatterParams { count: 500, domain: Some(Dist::Rect { w: 100.0, h: 100.0 }), yaw: Some(Dist::Uniform { min: 0.0, max: 360.0 }), ..Default::default() }.entries());
    check("RusdGrid", GridParams::default().entries());
    check("RusdStreetGrid", StreetGridParams { height: Some(Dist::Normal { mu: 24.0, sigma: 10.0 }), zone: Some("commercial".to_owned()), ..Default::default() }.entries());
}

/// **A script's graph is the document the cook reads.** The wires the script typed are
/// the wires the parser finds, by the operator's own port names.
#[test]
fn a_scripts_graph_round_trips_through_the_parser() {
    let mut g = Graph::new();
    g.default_prim("/World").seed(11);
    let volume = g.material::<Cube>("volume.rusd", "/World/Volume");
    let proto = g.material::<Cube>("box.rusd", "/World/Prototypes/Box");
    let cloud = ops::sample_points(
        &mut g,
        out("Samples", "samples.rusd:/World/Samples").desc("在 Volume 盒内采 60 个点"),
        SamplePointsIn::new(volume),
        SamplePointsParams { count: 60, seed: 11 },
    );
    let boxes = ops::instances(
        &mut g,
        out("Boxes", "boxes.rusd:/World/Boxes"),
        InstancesIn::new(cloud.ports.points, proto),
        InstancesParams { max_prims: 5000, seed: 11, ..Default::default() },
    );
    // The product's anchor is what a later operator instances whole.
    let _: rusd_script::Prim<Xform> = boxes.anchor();

    let text = g.to_ops_toml();
    let doc = rusd_core::parse_graph(&text, Some("examples")).expect("the graph parses");
    assert!(doc.graph);
    assert_eq!(doc.prims.len(), 2);

    let samples = &doc.prims[0];
    assert_eq!(samples.operator.as_deref(), Some("RusdSamplePoints"));
    assert_eq!(samples.path.as_str(), "/World/Samples");
    assert_eq!(samples.output_file.as_deref(), Some("samples.rusd"));
    assert_eq!(samples.inputs, vec![("domain".to_owned(), "volume.rusd:/World/Volume".to_owned())]);

    let boxes = &doc.prims[1];
    assert_eq!(boxes.operator.as_deref(), Some("RusdInstances"));
    // The cloud wire is explicit: the script typed it, so the document declares it,
    // and the cook does not have to guess it from node order.
    let points = boxes.inputs.iter().find(|(slot, _)| slot == "points").expect("the cloud wire");
    assert_eq!(points.1, "/World/Samples/points");
    assert_eq!(
        boxes.props.get("input_points").and_then(|v| match v {
            rusd_core::Value::String(s) => Some(s.as_str()),
            _ => None,
        }),
        Some("/World/Samples/points")
    );
}

/// **The USD nesting a script authors is a layer USD can open.** A struct of prims is a
/// prim with named children; a Vec of prims is a run of them; the ancestors are Xform.
#[test]
fn a_script_authors_usd_nesting() {
    let dir = std::env::temp_dir().join(format!("rusd_script_nesting_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut layer = rusd_script::usd::Layer::new("prototypes.rusd");
    let world = layer.xform("/World");
    let prototypes = layer.xform(&format!("{}/Prototypes", world.path()));
    // A Vec of typed prims, nested under one parent by path.
    let boxes: Vec<rusd_script::Prim<Cube>> = (0..3)
        .map(|k| layer.cube(&format!("{}/Box{k:03}", prototypes.path()), 0.6))
        .collect();
    layer.translate(&boxes[0], [0.0, 0.0, 20.0]);
    layer.write(&dir).unwrap();

    let opened = rusd_scene::document::layer_from_file(&dir.join("prototypes.rusd"))
        .expect("the layer opens as USD");
    let _ = opened;
    let stage = openusd::usd::Stage::open(dir.join("prototypes.rusd").to_str().unwrap())
        .expect("a stage over the layer");
    for prim in ["/World", "/World/Prototypes", "/World/Prototypes/Box000", "/World/Prototypes/Box002"] {
        let p = stage.prim(prim).unwrap_or_else(|e| panic!("{prim}: {e}"));
        assert!(p.is_valid().unwrap_or(false), "{prim} is authored");
    }
    let ty = stage
        .prim("/World/Prototypes/Box001")
        .unwrap()
        .type_name()
        .unwrap()
        .map(|t| t.as_str().to_owned())
        .unwrap_or_default();
    assert_eq!(ty, "Cube");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The shape a `Product` describes is its ports' shape — the anchor is the node's own
/// prim, not a port, so it never enters the declaration.
#[test]
fn a_products_shape_is_its_ports() {
    use rusd_script::ops::Cloud;
    assert_eq!(
        <rusd_script::ops::Product<Cloud> as ShapeOf>::shape(),
        Cloud::shape()
    );
}
