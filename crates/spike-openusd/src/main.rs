//! Empirical spike for SPEC O6: does openusd recompose incrementally?
//!
//! Method: build a scene of N prims, warm the composition cache, then author a
//! single attribute-value edit and observe `Stage::indexed_count()` (how many
//! prim indices remain cached) plus timing. If invalidation is granular, a value
//! edit must NOT collapse the cache to zero, and edit+requery must be far cheaper
//! than the cold compose. Source evidence: pcp/mod.rs "Cache invalidation"
//! (mirrors C++ PcpChanges) and pcp/dependencies.rs (reverse dependency index).

use std::time::Instant;

use openusd::sdf::{self, ChildrenKey, FieldKey, SpecType, Specifier, Value};
use openusd::usd::Stage;
use openusd::usda::TextWriter;

const N: usize = 2000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let scene = std::env::temp_dir().join("rusd_spike_scene.usda");
    write_scene(&scene)?;

    let stage = Stage::open(scene.to_str().unwrap())?;

    // Cold compose: first query of each prim builds its index.
    let t0 = Instant::now();
    for i in 0..N {
        let p = format!("/World/P{i:04}");
        let prim = stage.prim(sdf::path(&p)?)?;
        let _ = prim.attribute("radius").get::<f64>()?;
    }
    let cold = t0.elapsed();
    let warm_count = stage.indexed_count();

    // Author one attribute-value edit (the common agent edit).
    let t1 = Instant::now();
    let prim = stage.prim(sdf::path("/World/P0500")?)?;
    prim.attribute("radius").set(Value::Double(2.0))?;
    let after_edit = stage.indexed_count();
    let v = stage
        .prim(sdf::path("/World/P0500")?)?
        .attribute("radius")
        .get::<f64>()?;
    let edit = t1.elapsed();
    let after_requery = stage.indexed_count();

    println!("N prims                 : {N}");
    println!("cold compose all        : {cold:?}");
    println!("indexed_count (warm)    : {warm_count}");
    println!("--- single value edit ---");
    println!("indexed_count post-edit : {after_edit}   <- stays ~N if granular, 0 if full rebuild");
    println!("edited radius readback  : {v:?}");
    println!("edit + requery          : {edit:?}");
    println!("indexed_count requery   : {after_requery}");
    println!(
        "speedup edit vs cold    : {:.1}x",
        cold.as_secs_f64() / edit.as_secs_f64().max(1e-9)
    );

    // Contrast: a structural edit (specifier/arc-class) should drop the subtree.
    let before_struct = stage.indexed_count();
    stage.remove_prim(sdf::path("/World/P0600")?)?;
    let after_struct = stage.indexed_count();
    println!("--- structural remove ---");
    println!("indexed before / after  : {before_struct} / {after_struct}");

    let _ = std::fs::remove_file(&scene);
    Ok(())
}

fn write_scene(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut data = sdf::Data::new();
    let root = data.create_spec(sdf::Path::abs_root(), SpecType::PseudoRoot);
    root.add(FieldKey::DefaultPrim, Value::Token("World".into()));
    root.add(
        ChildrenKey::PrimChildren,
        Value::TokenVec(vec!["World".into()]),
    );

    let names: Vec<String> = (0..N).map(|i| format!("P{i:04}")).collect();
    let world = data.create_spec(sdf::Path::new("/World")?, SpecType::Prim);
    world.add(FieldKey::Specifier, Value::Specifier(Specifier::Def));
    world.add(FieldKey::TypeName, Value::Token("Xform".into()));
    world.add(
        ChildrenKey::PrimChildren,
        Value::TokenVec(names.iter().map(|n| n.as_str().into()).collect()),
    );

    for n in &names {
        let p = format!("/World/{n}");
        let ps = data.create_spec(sdf::Path::new(&p)?, SpecType::Prim);
        ps.add(FieldKey::Specifier, Value::Specifier(Specifier::Def));
        ps.add(FieldKey::TypeName, Value::Token("Sphere".into()));
        ps.add(
            ChildrenKey::PropertyChildren,
            Value::TokenVec(vec!["radius".into()]),
        );
        let attr = data.create_spec(sdf::Path::new(&format!("{p}.radius"))?, SpecType::Attribute);
        attr.add(FieldKey::TypeName, Value::Token("double".into()));
        attr.add(FieldKey::Default, Value::Double(1.0));
    }

    TextWriter::write_to_file(&data, path.to_str().unwrap())?;
    Ok(())
}
