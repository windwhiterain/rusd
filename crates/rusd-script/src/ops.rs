//! **An operator is a Rust function** (SPEC D15). Its signature is its schema.
//!
//! Each operator below is one function. What it reads is a struct of typed ports
//! ([`SamplePointsIn`], [`InstancesIn`]); what it writes is a struct of typed ports
//! ([`Cloud`], [`InstancesOut`], [`StreetGridOut`]); what it takes beside the wires is a
//! params struct. All three are Rust types, so:
//!
//! - wiring a `Material` where a `Gprim` is asked **does not compile** — the bound is
//!   USD's `IsA`, generated from the same manifest the runtime registry walks;
//! - a parameter of the wrong type, or a missing one, **does not compile** either;
//! - the shape the cook checks at runtime is *built from these types* ([`inputs`],
//!   [`outputs`]), so there is no second table to keep in step.
//!
//! The runtime checks stay for a hand-written `.ops.toml`, which has no compiler behind
//! it. A script does not need them: by the time it runs, its wires are already typed.
//!
//! ```
//! use rusd_script::graph::{Graph, out};
//! use rusd_script::ops::{self, SamplePointsIn, SamplePointsParams};
//! use rusd_script::schema::Cube;
//!
//! let mut g = Graph::new();
//! let volume = g.material::<Cube>("volume.rusd", "/World/Volume");
//! let cloud = ops::sample_points(
//!     &mut g,
//!     out("Samples", "samples.rusd:/World/Samples"),
//!     SamplePointsIn::new(volume),
//!     SamplePointsParams { count: 60, seed: 11 },
//! );
//! // The cloud is a `Prim<Points>` in the product, and the next operator takes it.
//! let points = cloud.ports.points;
//! assert_eq!(points.path(), "/World/Samples/points");
//! ```
//!
//! A wire of the wrong class is a compile error, not a diagnostic:
//!
//! ```compile_fail
//! use rusd_script::graph::{Graph, out};
//! use rusd_script::ops::{self, SamplePointsIn, SamplePointsParams};
//! use rusd_script::schema::Xform;
//! let mut g = Graph::new();
//! let not_geometry = g.material::<Xform>("volume.rusd", "/World/Volume");
//! ops::sample_points(&mut g, out("S", "s.rusd:/World/S"),
//!                    SamplePointsIn::new(not_geometry), SamplePointsParams::default());
//! ```

use std::collections::BTreeMap;

use rusd_core::Value;

use crate::graph::{Graph, Node, Output};
use crate::ports;
use crate::prim::{Prim, Wire};
use crate::schema::{Cube, Gprim, Material, Points, Schema, Sphere, Untyped, Xform, Xformable};
use crate::shape::{List, Port, Ports, Shape, ShapeOf};

// --- What a call returns ---------------------------------------------------

/// **An operator's product**: the prim it anchors (an `Xform` in the USD it wrote) and
/// the ports it wrote under it. The ports are the operator's declared output shape; the
/// anchor is what a later operator instances when it wants the whole product.
pub struct Product<S: ShapeOf> {
    anchor: Prim<Xform>,
    pub ports: S,
}

impl<S: ShapeOf> Product<S> {
    pub fn anchor(&self) -> Prim<Xform> {
        self.anchor.clone()
    }
}

impl<S: ShapeOf> ShapeOf for Product<S> {
    fn shape() -> Shape {
        S::shape()
    }
}

/// A distribution, as the document's inline table. The three forms the generators read,
/// and a fixed number — typed here so a script cannot spell `mu` as `mean`.
#[derive(Clone, Copy, Debug)]
pub enum Dist {
    Uniform { min: f64, max: f64 },
    Normal { mu: f64, sigma: f64 },
    Rect { w: f64, h: f64 },
    Fixed(f64),
}

impl Dist {
    pub fn value(self) -> Value {
        let mut m = BTreeMap::new();
        match self {
            Dist::Uniform { min, max } => {
                m.insert("dist".to_owned(), Value::String("uniform".to_owned()));
                m.insert("min".to_owned(), Value::Float(min));
                m.insert("max".to_owned(), Value::Float(max));
            }
            Dist::Normal { mu, sigma } => {
                m.insert("dist".to_owned(), Value::String("normal".to_owned()));
                m.insert("mu".to_owned(), Value::Float(mu));
                m.insert("sigma".to_owned(), Value::Float(sigma));
            }
            Dist::Rect { w, h } => {
                m.insert("dist".to_owned(), Value::String("rect".to_owned()));
                m.insert("w".to_owned(), Value::Float(w));
                m.insert("h".to_owned(), Value::Float(h));
            }
            Dist::Fixed(v) => return Value::Float(v),
        }
        Value::Dict(m)
    }
}

/// An operator's parameters, as the document's key/value table. Only what the script
/// set is written, so the cook's own defaults stay the defaults.
pub trait Params {
    fn entries(&self) -> BTreeMap<String, Value>;
}

fn put(map: &mut BTreeMap<String, Value>, name: &str, value: Value) {
    map.insert(name.to_owned(), value);
}

fn put_opt<T>(map: &mut BTreeMap<String, Value>, name: &str, value: Option<T>, f: impl FnOnce(T) -> Value) {
    if let Some(v) = value {
        put(map, name, f(v));
    }
}

/// Record one node, and hand back the anchor it writes at.
fn node(
    g: &mut Graph,
    at: Output,
    kind: &str,
    input: Option<String>,
    wires: Vec<(String, String)>,
    params: BTreeMap<String, Value>,
) -> Prim<Xform> {
    let anchor = Prim::<Xform>::new(&at.anchor);
    g.push(Node { name: at.node.clone(), kind: kind.to_owned(), output: at, input, wires, params });
    anchor
}

// --- sample_points: a Gprim in, a point cloud out --------------------------

ports! {
    /// **What a sampler reads**: the geometry it samples. `Gprim` is the class that
    /// covers a `Cube` and a `Mesh`; which of them it can draw from is settled where the
    /// geometry is read, because a `Points` is a `Gprim` too and has neither a volume
    /// nor a surface.
    pub struct SamplePointsIn {
        domain: Prim<Gprim>,
    }
}

impl SamplePointsIn {
    pub fn new(domain: impl Wire<Gprim>) -> Self {
        Self { domain: domain.wire() }
    }
}

ports! {
    /// **What a sampler writes**: one point cloud, under the prim it anchors.
    pub struct Cloud {
        points: Prim<Points>,
    }
}

/// A sampler's parameters. `count` is how many samples; `seed` keys the stream.
#[derive(Clone, Copy, Debug)]
pub struct SamplePointsParams {
    pub count: u64,
    pub seed: u64,
}

impl Default for SamplePointsParams {
    fn default() -> Self {
        Self { count: 100, seed: 0 }
    }
}

impl Params for SamplePointsParams {
    fn entries(&self) -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        put(&mut m, "count", Value::Int(self.count as i64));
        put(&mut m, "seed", Value::Int(self.seed as i64));
        m
    }
}

/// **Sample a domain into a point cloud.** One Rust function: the domain's class is a
/// bound, the parameters are a struct, and the cloud it returns is a `Prim<Points>` in
/// the product the call named.
pub fn sample_points(
    g: &mut Graph,
    at: Output,
    input: SamplePointsIn,
    params: SamplePointsParams,
) -> Product<Cloud> {
    let wire = input.domain.reference();
    let anchor = node(
        g,
        at,
        "sample_points",
        Some(wire.clone()),
        vec![("domain".to_owned(), wire)],
        params.entries(),
    );
    let points = anchor.child("points");
    Product { anchor, ports: Cloud { points } }
}

// --- instances: a cloud and a prototype in, a run of instances out ---------

ports! {
    /// **What an instances node reads**: the cloud it copies along, and the prim it
    /// copies. The prototype is an `Xformable` — anything USD can place.
    pub struct InstancesIn {
        points: Prim<Points>,
        prototype: Prim<Xformable>,
    }
}

impl InstancesIn {
    pub fn new(points: impl Wire<Points>, prototype: impl Wire<Xformable>) -> Self {
        Self { points: points.wire(), prototype: prototype.wire() }
    }
}

ports! {
    /// **What an instances node writes**: a run of instances, one per point. How many
    /// is the data's to say, so this is a `List` and not a `Vec` the script holds. Each
    /// instance authors no type of its own — its type composes through the arc.
    pub struct InstancesOut {
        #[part("i*")]
        items: List<Prim<Untyped>>,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct InstancesParams {
    pub scale: Option<Dist>,
    pub max_prims: u64,
    pub seed: u64,
}

impl Default for InstancesParams {
    fn default() -> Self {
        Self { scale: None, max_prims: 20000, seed: 0 }
    }
}

impl Params for InstancesParams {
    fn entries(&self) -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        put_opt(&mut m, "scale", self.scale, Dist::value);
        put(&mut m, "max_prims", Value::Int(self.max_prims as i64));
        put(&mut m, "seed", Value::Int(self.seed as i64));
        m
    }
}

/// **One prototype instance per point**, in the cloud's own order.
pub fn instances(
    g: &mut Graph,
    at: Output,
    input: InstancesIn,
    params: InstancesParams,
) -> Product<InstancesOut> {
    let prototype = input.prototype.reference();
    let anchor = node(
        g,
        at,
        "instances",
        Some(prototype.clone()),
        vec![
            ("points".to_owned(), input.points.reference()),
            ("prototype".to_owned(), prototype),
        ],
        params.entries(),
    );
    let items = List::new(anchor.path(), "i");
    Product { anchor, ports: InstancesOut { items } }
}

// --- scatter: a prototype in, a run of spheres out -------------------------

ports! {
    /// **What a scatter writes**: a run of spheres, one per sample.
    pub struct ScatterOut {
        #[part("i*")]
        items: List<Prim<Sphere>>,
    }
}

#[derive(Clone, Debug)]
pub struct ScatterParams {
    pub count: u64,
    pub seed: u64,
    /// The region it scatters in. Without one the scatter is parametric.
    pub domain: Option<Dist>,
    pub scale: Option<Dist>,
    pub yaw: Option<Dist>,
    /// Geometry in: a prim whose points become one instance each. Typed as the class
    /// the sampler asks for, so a `Material` cannot be wired here either.
    pub points: Option<Prim<Gprim>>,
    pub max_prims: u64,
    pub max_triangles: Option<u64>,
}

impl Default for ScatterParams {
    fn default() -> Self {
        Self {
            count: 100,
            seed: 0,
            domain: None,
            scale: None,
            yaw: None,
            points: None,
            max_prims: 20000,
            max_triangles: None,
        }
    }
}

impl Params for ScatterParams {
    fn entries(&self) -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        put(&mut m, "count", Value::Int(self.count as i64));
        put(&mut m, "seed", Value::Int(self.seed as i64));
        put_opt(&mut m, "domain", self.domain, Dist::value);
        put_opt(&mut m, "scale", self.scale, Dist::value);
        put_opt(&mut m, "yaw", self.yaw, Dist::value);
        put_opt(&mut m, "max_triangles", self.max_triangles, |v| Value::Int(v as i64));
        put(&mut m, "max_prims", Value::Int(self.max_prims as i64));
        m
    }
}

/// **Scatter a prototype** through a region, or along a cloud when one is wired.
pub fn scatter(
    g: &mut Graph,
    at: Output,
    prototype: impl Wire<Xformable>,
    params: ScatterParams,
) -> Product<ScatterOut> {
    let prototype = prototype.wire();
    let mut wires = vec![("prototype".to_owned(), prototype.reference())];
    let mut entries = params.entries();
    if let Some(points) = &params.points {
        wires.push(("points".to_owned(), points.reference()));
        put(&mut entries, "input_points", Value::String(points.reference()));
    }
    let anchor = node(
        g,
        at,
        "scatter",
        Some(prototype.reference()),
        wires,
        entries,
    );
    let items = List::new(anchor.path(), "i");
    Product { anchor, ports: ScatterOut { items } }
}

// --- grid: no input, a run of spheres out ----------------------------------

ports! {
    /// **What a grid writes**: a run of spheres, one per cell.
    pub struct GridOut {
        #[part("c*")]
        cells: List<Prim<Sphere>>,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct GridParams {
    pub count_x: u64,
    pub count_y: u64,
    pub spacing: f64,
    pub radius: f64,
    pub seed: u64,
}

impl Default for GridParams {
    fn default() -> Self {
        Self { count_x: 5, count_y: 5, spacing: 2.0, radius: 0.4, seed: 0 }
    }
}

impl Params for GridParams {
    fn entries(&self) -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        put(&mut m, "count_x", Value::Int(self.count_x as i64));
        put(&mut m, "count_y", Value::Int(self.count_y as i64));
        put(&mut m, "spacing", Value::Float(self.spacing));
        put(&mut m, "radius", Value::Float(self.radius));
        put(&mut m, "seed", Value::Int(self.seed as i64));
        m
    }
}

/// **A grid of spheres.** It reads nothing: it grows out of its parameters, which is
/// why its function takes no input struct at all.
pub fn grid(g: &mut Graph, at: Output, params: GridParams) -> Product<GridOut> {
    let anchor = node(g, at, "grid", None, Vec::new(), params.entries());
    let cells = List::new(anchor.path(), "c");
    Product { anchor, ports: GridOut { cells } }
}

// --- street_grid: no input, a struct of blocks out -------------------------

ports! {
    /// **A block**: a run of lots, each a cube. A struct inside a list inside a struct.
    pub struct Block {
        #[part("lot*")]
        lots: List<Prim<Cube>>,
    }
}

ports! {
    /// **What a street grid writes**: the ground, and a run of blocks. The nesting is
    /// the schema — `Ground` beside `b*`, and every block holding `lot*`.
    pub struct StreetGridOut {
        #[part("Ground")]
        ground: Prim<Cube>,
        #[part("b*")]
        blocks: List<Block>,
    }
}

#[derive(Clone, Debug)]
pub struct StreetGridParams {
    pub blocks_x: u64,
    pub blocks_y: u64,
    pub block_size: [f64; 2],
    pub street_width: f64,
    pub avenue_every: Option<u64>,
    pub avenue_width: Option<f64>,
    pub jitter: Option<f64>,
    pub block_prototype: Option<Prim<Xformable>>,
    pub lot_width: f64,
    pub margin: f64,
    pub occupancy: f64,
    pub height: Option<Dist>,
    pub floor_height: f64,
    pub zone: Option<String>,
    pub ground_material: Option<Prim<Material>>,
    pub max_prims: u64,
    pub seed: u64,
}

impl Default for StreetGridParams {
    fn default() -> Self {
        Self {
            blocks_x: 4,
            blocks_y: 4,
            block_size: [60.0, 60.0],
            street_width: 12.0,
            avenue_every: None,
            avenue_width: None,
            jitter: None,
            block_prototype: None,
            lot_width: 18.0,
            margin: 4.0,
            occupancy: 0.9,
            height: None,
            floor_height: 3.2,
            zone: None,
            ground_material: None,
            max_prims: 20000,
            seed: 0,
        }
    }
}

impl Params for StreetGridParams {
    fn entries(&self) -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        put(&mut m, "blocks_x", Value::Int(self.blocks_x as i64));
        put(&mut m, "blocks_y", Value::Int(self.blocks_y as i64));
        put(&mut m, "block_size", Value::Vec2(self.block_size));
        put(&mut m, "street_width", Value::Float(self.street_width));
        put_opt(&mut m, "avenue_every", self.avenue_every, |v| Value::Int(v as i64));
        put_opt(&mut m, "avenue_width", self.avenue_width, Value::Float);
        put_opt(&mut m, "jitter", self.jitter, Value::Float);
        put_opt(&mut m, "block_prototype", self.block_prototype.as_ref(), |p| {
            Value::String(p.reference())
        });
        put(&mut m, "lot_width", Value::Float(self.lot_width));
        put(&mut m, "margin", Value::Float(self.margin));
        put(&mut m, "occupancy", Value::Float(self.occupancy));
        put_opt(&mut m, "height", self.height, Dist::value);
        put(&mut m, "floor_height", Value::Float(self.floor_height));
        put_opt(&mut m, "zone", self.zone.as_ref(), |z| Value::String(z.clone()));
        put_opt(&mut m, "ground_material", self.ground_material.as_ref(), |p| {
            Value::String(p.reference())
        });
        put(&mut m, "max_prims", Value::Int(self.max_prims as i64));
        put(&mut m, "seed", Value::Int(self.seed as i64));
        m
    }
}

/// **A street grid**: blocks and lots, heights sampled per zone. Its output is a struct
/// of ports, and the USD the cook writes nests exactly as that struct does.
pub fn street_grid(g: &mut Graph, at: Output, params: StreetGridParams) -> Product<StreetGridOut> {
    let anchor = node(g, at, "street_grid", None, Vec::new(), params.entries());
    let ground = anchor.child("Ground");
    let blocks = List::new(anchor.path(), "b");
    Product { anchor, ports: StreetGridOut { ground, blocks } }
}

// --- The declarations, read back for the runtime ---------------------------

/// **The ports `operator` reads**, built from the Rust types of its input struct. The
/// cook asks this of a hand-written `.ops.toml`, whose wires no compiler checked.
pub fn inputs(operator: &str) -> Vec<Port> {
    match operator {
        "RusdSamplePoints" => SamplePointsIn::ports(),
        "RusdInstances" => InstancesIn::ports(),
        _ => Vec::new(),
    }
}

/// **The parts `operator` writes** under the prim it anchors, built from the Rust types
/// of its output struct.
pub fn outputs(operator: &str) -> Vec<Port> {
    match operator {
        "RusdSamplePoints" => Cloud::ports(),
        "RusdInstances" => InstancesOut::ports(),
        "RusdScatter" => ScatterOut::ports(),
        "RusdGrid" => GridOut::ports(),
        "RusdStreetGrid" => StreetGridOut::ports(),
        _ => Vec::new(),
    }
}

/// One input port of `operator`, by name.
pub fn input(operator: &str, name: &str) -> Option<Port> {
    inputs(operator).into_iter().find(|port| port.name == name)
}

/// The graph word a script calls an operator by, and the registry name the cook knows.
/// The two spellings of one operator, in one place.
pub fn registry_name(kind: &str) -> Option<&'static str> {
    match kind {
        "sample_points" => Some("RusdSamplePoints"),
        "instances" => Some("RusdInstances"),
        "grid" => Some("RusdGrid"),
        "street_grid" => Some("RusdStreetGrid"),
        "scatter" => Some("RusdScatter"),
        _ => None,
    }
}

/// Whether `T` is a concrete type a prim may author — the compile-time half of the
/// registry's `is_concrete_type`, for a script that wants to ask.
pub fn concrete<T: Schema>() -> bool {
    T::CONCRETE
}
