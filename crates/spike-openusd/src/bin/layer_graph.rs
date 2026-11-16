//! Empirical spike for SPEC D5 (Solaris-style PCG): a generator is a layer-op.
//!
//! Chain under test: generator reads (read-only) base scene → authors a NEW
//! anonymous output layer → `Stage::insert_layer` wires it into the composition
//! graph → composed value reflects the generator output. Re-running the generator
//! produces a new layer; muting the old one recomposes incrementally.
//!
//! Run: `cargo run -p spike-openusd --bin layer_graph`

use std::time::Instant;

use openusd::sdf::{
    self, AttributeSpec, ChildrenKey, FieldKey, LayerOffset, PrimSpec, SpecType, Specifier, Value,
    Variability,
};
use openusd::usd::Stage;
use openusd::usda::TextWriter;

/// A generator: pure function (params) -> new anonymous output layer.
/// It authors /World/Building with a radius; it does NOT mutate its inputs.
fn generate_building(radius: f64) -> Result<sdf::Layer, Box<dyn std::error::Error>> {
    let mut layer = sdf::Layer::new_anonymous("gen_building.usda");
    layer.edit(|l| {
        let d = l.data_mut();
        PrimSpec::new(d, "/World/Building", Specifier::Def, "Sphere")?;
        let mut attr = AttributeSpec::new(
            d,
            "/World/Building.radius",
            "double",
            Variability::Uniform,
            false,
        )?;
        attr.set_default(Value::Double(radius))?;
        Ok(())
    })?;
    Ok(layer)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root_path = std::env::temp_dir().join("rusd_pipe_root.usda");
    write_root(&root_path)?;
    let stage = Stage::open(root_path.to_str().unwrap())?;
    let root_id = stage.root_layer().identifier().to_owned();

    let _ = stage.prim(sdf::path("/World")?)?;
    let base_count = stage.indexed_count();

    // Generator v1 -> anonymous layer -> insert into composition graph.
    let t0 = Instant::now();
    let gen1 = generate_building(1.0)?;
    let gen1_id = gen1.identifier().to_owned();
    stage.insert_layer(&root_id, 0, gen1, LayerOffset::IDENTITY)?;
    let r1 = stage
        .prim(sdf::path("/World/Building")?)?
        .attribute("radius")
        .get::<f64>()?;
    let insert = t0.elapsed();
    let after_insert = stage.indexed_count();

    // Re-run generator (param changed) -> new layer -> mute old -> recompose.
    let t1 = Instant::now();
    let gen2 = generate_building(5.0)?;
    stage.insert_layer(&root_id, 0, gen2, LayerOffset::IDENTITY)?;
    stage.mute_layer(&gen1_id);
    let r2 = stage
        .prim(sdf::path("/World/Building")?)?
        .attribute("radius")
        .get::<f64>()?;
    let regen = t1.elapsed();
    let after_regen = stage.indexed_count();

    println!("base indexed_count        : {base_count}");
    println!("--- generator v1 (radius=1) ---");
    println!("composed radius           : {r1:?}   <- generator output layer composes");
    println!("generate+insert+query     : {insert:?}");
    println!("indexed_count             : {after_insert}");
    println!("--- generator v2 (radius=5), mute v1 ---");
    println!("composed radius           : {r2:?}   <- new layer overrides, old muted");
    println!("regen+insert+mute+query   : {regen:?}");
    println!("indexed_count             : {after_regen}");
    println!(
        "sublayers of root         : {:?}",
        stage.sub_layers(&root_id)
    );

    let _ = std::fs::remove_file(&root_path);
    Ok(())
}

fn write_root(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut data = sdf::Data::new();
    let root = data.create_spec(sdf::Path::abs_root(), SpecType::PseudoRoot);
    root.add(FieldKey::DefaultPrim, Value::Token("World".into()));
    root.add(
        ChildrenKey::PrimChildren,
        Value::TokenVec(vec!["World".into()]),
    );
    let world = data.create_spec(sdf::Path::new("/World")?, SpecType::Prim);
    world.add(FieldKey::Specifier, Value::Specifier(Specifier::Def));
    world.add(FieldKey::TypeName, Value::Token("Xform".into()));
    TextWriter::write_to_file(&data, path.to_str().unwrap())?;
    Ok(())
}
