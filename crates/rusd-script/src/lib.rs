//! # rusd-script — operators as Rust functions, and the scripts that call them
//!
//! **SPEC D15**: type checking moves from the runtime into the script. An operator is a
//! Rust function ([`ops`]); its signature — the Rust types of its input ports, its
//! output ports and its parameters — *is* its schema. A script is Rust code that calls
//! those functions, builds Struct / Array / Vec values out of their ports, and writes
//! the operator graph (`.ops.toml`) and the USD nesting (`.rusd`) they name.
//!
//! What that buys, concretely:
//!
//! - **A wrong wire does not compile.** A port asks for `impl Wire<Gprim>`; `Prim<Cube>`
//!   satisfies it because the generated `IsA` impls are the closure of USD's own schema
//!   manifest, and `Prim<Xform>` does not. The error is rustc's, at the call site, with
//!   the script's line number — not `E_SCHEMA` after a cook.
//! - **A prim's type is its class *and* its interfaces.** `Prim<Applied<MaterialBindingAPI,
//!   Cube>>` is a `Cube` with the API applied; a port that asks for both bounds `Wire`
//!   and [`schema::Has`].
//! - **One declaration.** The cook's runtime check reads [`ops::inputs`] / [`ops::outputs`],
//!   which are built from the same Rust types the script's signatures use. The static
//!   tables that used to sit beside them are gone.
//!
//! The runtime checks remain for a hand-written `.ops.toml`: it has no compiler behind
//! it, so the cook still asks USD what a wire is. A script does not need that answer —
//! by the time it runs, its wires are typed.
//!
//! ## The shape of a script
//!
//! ```
//! use rusd_script::graph::{Graph, out};
//! use rusd_script::ops::{self, InstancesIn, InstancesParams, SamplePointsIn, SamplePointsParams};
//! use rusd_script::schema::{Cube, Xform};
//!
//! let mut g = Graph::new();
//! g.default_prim("/World").seed(11);
//!
//! // Material: a typed handle into a USD, or a layer the script authors itself.
//! let volume = g.material::<Cube>("volume.rusd", "/World/Volume");
//! let box_proto = g.material::<Cube>("box.rusd", "/World/Prototypes/Box");
//!
//! // Two operator calls. The second takes the first's output port — a `Prim<Points>`.
//! let cloud = ops::sample_points(
//!     &mut g, out("Samples", "samples.rusd:/World/Samples"),
//!     SamplePointsIn::new(volume), SamplePointsParams { count: 60, seed: 11 },
//! );
//! let boxes = ops::instances(
//!     &mut g, out("Boxes", "boxes.rusd:/World/Boxes"),
//!     InstancesIn::new(cloud.ports.points, box_proto),
//!     InstancesParams { max_prims: 5000, seed: 11, ..Default::default() },
//! );
//!
//! // The product's anchor is an `Xform`: what a later operator instances whole.
//! let _: rusd_script::Prim<Xform> = boxes.anchor();
//! assert_eq!(g.nodes().len(), 2);
//! ```

pub mod graph;
pub mod ops;
pub mod prim;
pub mod runner;
pub mod schema;
pub mod shape;
pub mod usd;

pub use graph::{Graph, Node, Output, out};
pub use prim::{Prim, Wire};
pub use shape::{Arr, List, Ports, ShapeOf};
