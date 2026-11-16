//! Empirical spike for SPEC D5/D6: HOW a generator is triggered.
//!
//! Answer under test: NOT a composer callback, NOT a composer hack. A Rusd cook
//! engine sits ABOVE openusd composition (exactly like Houdini's LOP cook sits
//! above USD). It evaluates generator nodes in dependency order: read composed
//! read-only inputs -> run G -> author anonymous output layer -> insert_layer.
//! openusd pcp just composes the resulting layers (incrementally).
//!
//! This spike wires 3 nodes: A (parcel.width), B (building.height = width*2,
//! reads A's composed output), C (tree, independent). It shows:
//!   - a no-op cook pass re-cooks NOTHING (content-key memo hits);
//!   - changing A re-cooks A and downstream B, but NOT independent C.
//!
//! Run: `cargo run -p spike-openusd --bin cook_graph`

use std::collections::HashMap;
use std::time::Instant;

use openusd::sdf::{
    self, AttributeSpec, ChildrenKey, FieldKey, LayerOffset, PrimSpec, SpecType, Specifier, Value,
    Variability,
};
use openusd::usd::Stage;
use openusd::usda::TextWriter;

type Err = Box<dyn std::error::Error>;

/// Generic layer-op: author one prim with one double attribute into a new layer.
fn author_layer(tag: &str, prim: &str, ty: &str, attr: &str, val: f64) -> Result<sdf::Layer, Err> {
    let mut layer = sdf::Layer::new_anonymous(format!("{tag}.usda"));
    let attr_path = format!("{prim}.{attr}");
    layer.edit(|l| {
        let d = l.data_mut();
        PrimSpec::new(d, prim, Specifier::Def, ty)?;
        let mut a =
            AttributeSpec::new(d, attr_path.as_str(), "double", Variability::Uniform, false)?;
        a.set_default(Value::Double(val))?;
        Ok(())
    })?;
    Ok(layer)
}

fn cook_parcel(width: f64) -> Result<sdf::Layer, Err> {
    author_layer("parcel", "/World/Parcel", "Xform", "width", width)
}
fn cook_tree() -> Result<sdf::Layer, Err> {
    author_layer("tree", "/World/Tree", "Sphere", "radius", 1.0)
}
/// B reads its input from the COMPOSED scene (read-only), then produces a layer.
fn cook_building(stage: &Stage) -> Result<sdf::Layer, Err> {
    let w = stage
        .prim(sdf::path("/World/Parcel")?)?
        .attribute("width")
        .get::<f64>()?
        .unwrap_or(0.0);
    author_layer("building", "/World/Building", "Xform", "height", w * 2.0)
}

/// Minimal cook runner: content-key memo + layer replacement + cook counters.
#[derive(Default)]
struct Runner {
    memo: HashMap<String, String>,
    layers: HashMap<String, String>,
    counts: HashMap<String, u32>,
}

impl Runner {
    fn cook(
        &mut self,
        stage: &Stage,
        root_id: &str,
        node: &str,
        key: String,
        make: impl FnOnce() -> Result<sdf::Layer, Err>,
    ) -> Result<(), Err> {
        if self.memo.get(node) == Some(&key) {
            return Ok(()); // memo hit: inputs unchanged, do not re-cook
        }
        let layer = make()?;
        let id = layer.identifier().to_owned();
        stage.insert_layer(root_id, 0, layer, LayerOffset::IDENTITY)?;
        if let Some(old) = self.layers.insert(node.to_owned(), id) {
            stage.mute_layer(&old); // non-destructive: suppress superseded output
        }
        self.memo.insert(node.to_owned(), key);
        *self.counts.entry(node.to_owned()).or_insert(0) += 1;
        Ok(())
    }
}

/// One cook pass in dependency order: A -> B (reads A) ; C independent.
fn cook_pass(stage: &Stage, root_id: &str, a_width: f64, r: &mut Runner) -> Result<(), Err> {
    r.cook(stage, root_id, "A", format!("parcel:{a_width}"), || {
        cook_parcel(a_width)
    })?;
    let w = stage
        .prim(sdf::path("/World/Parcel")?)?
        .attribute("width")
        .get::<f64>()?
        .unwrap_or(0.0);
    r.cook(stage, root_id, "B", format!("building:{w}"), || {
        cook_building(stage)
    })?;
    r.cook(stage, root_id, "C", "tree:1".to_owned(), || cook_tree())?;
    Ok(())
}

fn height(stage: &Stage) -> Result<Option<f64>, Err> {
    Ok(stage
        .prim(sdf::path("/World/Building")?)?
        .attribute("height")
        .get::<f64>()?)
}

fn main() -> Result<(), Err> {
    let root_path = std::env::temp_dir().join("rusd_cook_root.usda");
    write_root(&root_path)?;
    let stage = Stage::open(root_path.to_str().unwrap())?;
    let root_id = stage.root_layer().identifier().to_owned();
    let mut r = Runner::default();

    let t0 = Instant::now();
    cook_pass(&stage, &root_id, 10.0, &mut r)?;
    let initial = t0.elapsed();
    let h1 = height(&stage)?;

    // No-op pass: same inputs -> every node memo-hits, nothing re-cooks.
    cook_pass(&stage, &root_id, 10.0, &mut r)?;
    let noop = (r.counts["A"], r.counts["B"], r.counts["C"]);

    // Change A's param -> A re-cooks, downstream B re-cooks, independent C does NOT.
    let t1 = Instant::now();
    cook_pass(&stage, &root_id, 30.0, &mut r)?;
    let recook = t1.elapsed();
    let h2 = height(&stage)?;

    println!("initial cook (A=10)       : {initial:?}, building.height = {h1:?}");
    println!(
        "no-op pass cook counts    : A={} B={} C={}   <- all 1 = memo hits, no re-cook",
        noop.0, noop.1, noop.2
    );
    println!(
        "after A=30 cook counts    : A={} B={} C={}   <- A,B=2 (downstream), C=1 (independent)",
        r.counts["A"], r.counts["B"], r.counts["C"]
    );
    println!("re-cook A+B               : {recook:?}, building.height = {h2:?}");
    println!("trigger = cook engine above pcp; no composer callback, no composer hack");

    let _ = std::fs::remove_file(&root_path);
    Ok(())
}

fn write_root(path: &std::path::Path) -> Result<(), Err> {
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
