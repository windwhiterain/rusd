//! **A Rusd script** (SPEC D15): the two-stage box scatter, as Rust.
//!
//! Run it with `rusd script run examples/box_scatter.rs`; check its types without
//! running it with `rusd script check examples/box_scatter.rs`. It writes
//! `examples/box_scatter.ops.toml` — the operator graph the cook reads. The graph is a
//! product of this file, so edit this file, not the TOML.
//!
//! Every wire below is typed. `SamplePointsIn::new` asks for a `Gprim`, so the `Cube`
//! volume and the `Mesh` terrain both fit and an `Xform` would not compile;
//! `InstancesIn::new` asks for a `Points` and an `Xformable`, so the cloud port and the
//! box prototype fit and a `Material` would not. Nothing here is checked at runtime:
//! by the time this program runs, rustc has already answered.

use rusd_script::graph::{Graph, out};
use rusd_script::ops::{self, InstancesIn, InstancesParams, SamplePointsIn, SamplePointsParams};
use rusd_script::schema::{Cube, Mesh};

fn main() {
    let mut g = Graph::new();
    g.default_prim("/World");

    // Material: typed handles into the USD files beside the graph.
    let volume = g.material::<Cube>("volume.rusd", "/World/Volume");
    let terrain = g.material::<Mesh>("terrain.rusd", "/World/Terrain");
    let box_proto = g.material::<Cube>("box.rusd", "/World/Prototypes/Box");

    // Stage one: sample the box's volume, then put a box at every point.
    let samples = ops::sample_points(
        &mut g,
        out("Samples", "samples.rusd:/World/Samples").desc("在 Volume 盒内均匀采 60 个点（第一阶段）"),
        SamplePointsIn::new(volume),
        SamplePointsParams { count: 60, seed: 11 },
    );
    let boxes = ops::instances(
        &mut g,
        out("Boxes", "boxes.rusd:/World/Boxes").desc("每个点放一个 box，得到一簇 box（原型来自素材层）"),
        InstancesIn::new(samples.ports.points, box_proto),
        InstancesParams { max_prims: 5000, seed: 11, ..Default::default() },
    );

    // Stage two: sample the terrain's surface, then instance the whole first-stage
    // product at every point. The prototype is the boxes' anchor — an `Xform`.
    let surface = ops::sample_points(
        &mut g,
        out("SurfaceSamples", "surface_samples.rusd:/World/SurfaceSamples")
            .desc("在 Terrain 表面面积加权采 40 个点（第二阶段）"),
        SamplePointsIn::new(terrain),
        SamplePointsParams { count: 40, seed: 7 },
    );
    let _clusters = ops::instances(
        &mut g,
        out("Instances", "instances.rusd:/World/Instances").desc("把第一阶段的一簇 box 实例化到 quad 表面"),
        InstancesIn::new(surface.ports.points, boxes.anchor()),
        InstancesParams { max_prims: 5000, seed: 7, ..Default::default() },
    );

    let path = std::path::Path::new("examples/box_scatter.ops.toml");
    g.write(path).expect("write the operator graph");
    println!("wrote {} ({} operators)", path.display(), g.nodes().len());
}
