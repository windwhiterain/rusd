//! Spike for SPEC D8: how the cook graph and USD composition graph relate.
//!
//! Claim under test: the cook graph is an IMPLICIT DAG induced by prim-path
//! read/write dependencies (option 3), mediated by USD composition, with cook
//! wrapping composition (option 1). A cook node's output is a LAYER (not a
//! stage). The engine derives evaluation order from read∩write intersections —
//! no hardcoded order — and memoizes by the hash of actual read values.
//!
//! Nodes: A writes /World/Parcel.width; B reads /World/Parcel.width and writes
//! /World/Building.height (= width * param); C writes /World/Tree.radius (no reads).
//! Expected: engine derives B depends on A; changing A re-cooks A and B, not C.
//!
//! Run: `cargo run -p spike-openusd --bin implicit_cook`

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};

use openusd::sdf::{
    self, AttributeSpec, ChildrenKey, FieldKey, LayerOffset, PrimSpec, SpecType, Specifier, Value,
    Variability,
};
use openusd::usd::Stage;
use openusd::usda::TextWriter;

type Err = Box<dyn std::error::Error>;

struct Node {
    id: &'static str,
    param: f64,
    reads: Vec<(&'static str, &'static str)>, // (prim path, attr) read from composed stage
    writes: Vec<&'static str>,                // prim paths authored by this node's output layer
    eval: fn(&Stage, f64) -> Result<sdf::Layer, Err>,
}

fn author(tag: &str, prim: &str, ty: &str, attr: &str, val: f64) -> Result<sdf::Layer, Err> {
    let mut layer = sdf::Layer::new_anonymous(format!("{tag}.usda"));
    let ap = format!("{prim}.{attr}");
    layer.edit(|l| {
        let d = l.data_mut();
        PrimSpec::new(d, prim, Specifier::Def, ty)?;
        let mut a = AttributeSpec::new(d, ap.as_str(), "double", Variability::Uniform, false)?;
        a.set_default(Value::Double(val))?;
        Ok(())
    })?;
    Ok(layer)
}

fn a_eval(_s: &Stage, w: f64) -> Result<sdf::Layer, Err> {
    author("parcel", "/World/Parcel", "Xform", "width", w)
}
fn b_eval(s: &Stage, mult: f64) -> Result<sdf::Layer, Err> {
    let w = read_value(s, "/World/Parcel", "width");
    author("building", "/World/Building", "Xform", "height", w * mult)
}
fn c_eval(_s: &Stage, r: f64) -> Result<sdf::Layer, Err> {
    author("tree", "/World/Tree", "Sphere", "radius", r)
}

fn read_value(stage: &Stage, prim: &str, attr: &str) -> f64 {
    let Ok(p) = sdf::path(prim) else { return 0.0 };
    let Ok(pr) = stage.prim(p) else { return 0.0 };
    pr.attribute(attr)
        .get::<f64>()
        .ok()
        .flatten()
        .unwrap_or(0.0)
}

/// Derive dependency edges: B depends on A iff B reads a prim A writes.
fn derive_deps(nodes: &[Node]) -> HashMap<&'static str, HashSet<&'static str>> {
    let writer: HashMap<&str, &str> = nodes
        .iter()
        .flat_map(|n| n.writes.iter().map(move |w| (*w, n.id)))
        .collect();
    let mut deps: HashMap<&str, HashSet<&str>> =
        nodes.iter().map(|n| (n.id, HashSet::new())).collect();
    for b in nodes {
        for (prim, _attr) in &b.reads {
            if let Some(&a) = writer.get(prim)
                && a != b.id
            {
                deps.get_mut(b.id).unwrap().insert(a);
            }
        }
    }
    deps
}

/// Kahn topological sort over the derived dependency graph.
fn topo_order(
    nodes: &[Node],
    deps: &HashMap<&'static str, HashSet<&'static str>>,
) -> Vec<&'static str> {
    let mut indeg: HashMap<&'static str, usize> =
        nodes.iter().map(|n| (n.id, deps[n.id].len())).collect();
    let mut dependents: HashMap<&'static str, Vec<&'static str>> = HashMap::new();
    for (b, ds) in deps {
        for a in ds {
            dependents.entry(a).or_default().push(b);
        }
    }
    let mut q: VecDeque<&'static str> = nodes
        .iter()
        .filter(|n| indeg[n.id] == 0)
        .map(|n| n.id)
        .collect();
    let mut order = Vec::new();
    while let Some(id) = q.pop_front() {
        order.push(id);
        if let Some(ds) = dependents.get(id) {
            for &d in ds {
                *indeg.get_mut(d).unwrap() -= 1;
                if indeg[d] == 0 {
                    q.push_back(d);
                }
            }
        }
    }
    order
}

#[derive(Default)]
struct Memo {
    keys: HashMap<&'static str, u64>,
    layers: HashMap<&'static str, String>,
    cooked: HashMap<&'static str, u32>,
}

fn cook_pass(
    nodes: &[Node],
    stage: &Stage,
    root_id: &str,
    memo: &mut Memo,
) -> Result<Vec<&'static str>, Err> {
    let deps = derive_deps(nodes);
    let order = topo_order(nodes, &deps);
    let by_id: HashMap<&str, &Node> = nodes.iter().map(|n| (n.id, n)).collect();
    for id in &order {
        let n = by_id[id];
        // memo key = hash(param + actual composed read values)
        let mut h = DefaultHasher::new();
        n.param.to_bits().hash(&mut h);
        for (prim, attr) in &n.reads {
            read_value(stage, prim, attr).to_bits().hash(&mut h);
        }
        let key = h.finish();
        if memo.keys.get(id) == Some(&key) {
            continue; // memo hit
        }
        let layer = (n.eval)(stage, n.param)?;
        let lid = layer.identifier().to_owned();
        stage.insert_layer(root_id, 0, layer, LayerOffset::IDENTITY)?;
        if let Some(old) = memo.layers.insert(id, lid) {
            stage.mute_layer(&old);
        }
        memo.keys.insert(id, key);
        *memo.cooked.entry(id).or_insert(0) += 1;
    }
    Ok(order)
}

fn main() -> Result<(), Err> {
    let root_path = std::env::temp_dir().join("rusd_implicit_root.usda");
    write_root(&root_path)?;
    let stage = Stage::open(root_path.to_str().unwrap())?;
    let root_id = stage.root_layer().identifier().to_owned();

    let mut nodes = vec![
        Node {
            id: "A",
            param: 10.0,
            reads: vec![],
            writes: vec!["/World/Parcel"],
            eval: a_eval,
        },
        Node {
            id: "B",
            param: 2.0,
            reads: vec![("/World/Parcel", "width")],
            writes: vec!["/World/Building"],
            eval: b_eval,
        },
        Node {
            id: "C",
            param: 1.0,
            reads: vec![],
            writes: vec!["/World/Tree"],
            eval: c_eval,
        },
    ];
    let mut memo = Memo::default();

    let order = cook_pass(&nodes, &stage, &root_id, &mut memo)?;
    let deps = derive_deps(&nodes);
    let h1 = read_value(&stage, "/World/Building", "height");
    println!(
        "derived deps          : B <- {:?} (from read/write path intersection)",
        deps["B"]
    );
    println!("auto topo order       : {order:?}  (B scheduled after A, no hardcoded order)");
    println!("building.height       : {h1}  (= width 10 * 2)");
    println!(
        "cold cook counts      : A={} B={} C={}",
        memo.cooked["A"], memo.cooked["B"], memo.cooked["C"]
    );

    // no-op pass: all memo hits
    cook_pass(&nodes, &stage, &root_id, &mut memo)?;
    println!(
        "no-op cook counts     : A={} B={} C={}  (unchanged = memo hits)",
        memo.cooked["A"], memo.cooked["B"], memo.cooked["C"]
    );

    // change A's param -> A re-cooks, B re-cooks (read value changed), C does not
    nodes[0].param = 30.0;
    cook_pass(&nodes, &stage, &root_id, &mut memo)?;
    let h2 = read_value(&stage, "/World/Building", "height");
    println!(
        "after A=30 cook counts: A={} B={} C={}  (downstream B only)",
        memo.cooked["A"], memo.cooked["B"], memo.cooked["C"]
    );
    println!("building.height       : {h2}  (= width 30 * 2)");

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
