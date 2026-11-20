//! **Struct, Array, Vec become USD nesting** (SPEC D15).
//!
//! Run with `rusd script run examples/nesting.rs`. It authors a material layer
//! (`examples/nesting.rusd`) whose hierarchy is the Rust values below — a struct of
//! prims is a prim with named children, a `Vec` is a numbered run, an array is exactly
//! N — and an operator graph (`examples/nesting.ops.toml`) that instances one of them.
//!
//! Every handle is typed by the call that made it: `layer.cube` is a `Prim<Cube>`, so
//! the scatter's prototype slot (`Xformable`) takes it, and a `Material` would not
//! compile there.

use rusd_script::graph::{Graph, out};
use rusd_script::ops::{self, GridParams, ScatterParams};
use rusd_script::prim::Prim;
use rusd_script::schema::{Cube, Sphere};
use rusd_script::usd::Layer;

/// A struct of prims: the USD it becomes is a prim with these three named children.
struct Gate {
    left: Prim<Cube>,
    right: Prim<Cube>,
    lintel: Prim<Cube>,
}

fn main() {
    let mut layer = Layer::new("nesting.rusd");
    let world = layer.xform("/World");

    // A struct: three cubes under one parent, named by the struct's fields.
    let gate = Gate {
        left: layer.cube(&format!("{}/Gate/left", world.path()), 1.0),
        right: layer.cube(&format!("{}/Gate/right", world.path()), 1.0),
        lintel: layer.cube(&format!("{}/Gate/lintel", world.path()), 1.0),
    };
    layer.translate(&gate.left, [-2.0, 0.0, 1.0]);
    layer.translate(&gate.right, [2.0, 0.0, 1.0]);
    layer.translate(&gate.lintel, [0.0, 0.0, 2.5]);
    layer.scale(&gate.lintel, [5.0, 1.0, 0.5]);

    // A Vec: a run of spheres, one per element, nested under one parent.
    let markers: Vec<Prim<Sphere>> = (0..5)
        .map(|k| {
            let p = layer.sphere(&format!("{}/Markers/m{k:03}", world.path()), 0.3);
            layer.translate(&p, [k as f64 * 1.5 - 3.0, 4.0, 0.3]);
            p
        })
        .collect();

    // An array: exactly four, no more and no fewer — the length is the type's.
    let corners: [Prim<Sphere>; 4] = std::array::from_fn(|k| {
        let p = layer.sphere(&format!("{}/Corners/c{k:03}", world.path()), 0.25);
        let (x, y) = [(0.0, 0.0), (6.0, 0.0), (6.0, 6.0), (0.0, 6.0)][k];
        layer.translate(&p, [x, y, 0.25]);
        p
    });
    assert_eq!(markers.len(), 5);
    assert_eq!(corners.len(), 4);

    // The operator graph: a grid of spheres, and a scatter whose prototype is the
    // gate's lintel — a `Cube` from the layer above, wired across the asset boundary.
    let mut g = Graph::new();
    g.default_prim("/World").seed(7);
    let _grid = ops::grid(
        &mut g,
        out("Grid", "nesting_grid.rusd:/World/Grid").desc("5×5 的球体网格"),
        GridParams { count_x: 5, count_y: 5, spacing: 2.0, radius: 0.4, seed: 7 },
    );
    let lintel = g.material::<Cube>("nesting.rusd", gate.lintel.path());
    let _props = ops::scatter(
        &mut g,
        out("Props", "nesting_props.rusd:/World/Props").desc("沿矩形域撒 lintel 的副本"),
        lintel,
        ScatterParams {
            count: 24,
            domain: Some(rusd_script::ops::Dist::Rect { w: 30.0, h: 30.0 }),
            max_prims: 24,
            seed: 7,
            ..Default::default()
        },
    );

    let dir = std::path::Path::new("examples");
    layer.write(dir).expect("write the material layer");
    g.write(&dir.join("nesting.ops.toml")).expect("write the operator graph");
    println!("wrote examples/nesting.rusd and examples/nesting.ops.toml");
}
