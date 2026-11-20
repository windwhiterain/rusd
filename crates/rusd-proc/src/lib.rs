//! # rusd-proc
//!
//! Procedural runtime (SPEC §5–6):
//! - [`generator`]: the Generator trait + GenContext (read-tracked composed inputs).
//! - [`cook`]: the cook engine — an implicit path-dependency DAG (D8) that wraps
//!   USD composition (D9), memoized by content hash with downstream dirty propagation.
//! - [`rng`]: path-keyed deterministic ChaCha8 RNG.
//! - [`city`]: the city generator suite (D4): StreetGrid / Building / Scatter.
//! - [`pipeline`]: the v1.5 two-op pipeline, `RusdSamplePoints` (mesh → points) →
//!   `RusdInstances` (points → instances). Two generators with a real layer between
//!   them, so a pipeline prefix can show the intermediate cloud (PROTOCOL v1.5).
//! - [`deps`]: the reads/writes those ops declare to `nodes_from_doc`, which is what
//!   orders the pair and keeps incremental ≡ full.
//! - [`generators`]: RusdGrid + default registry + expand_doc.

pub mod city;
pub mod cook;
pub mod deps;
pub mod generator;
pub mod generators;
pub mod pipeline;
pub mod rng;

pub use city::{RusdBuilding, RusdScatter, RusdStreetGrid};
pub use cook::{CookEngine, CookNode, CookOp, CookResult, CookStats, compose_stack, nodes_from_doc};
pub use generator::{GenContext, Generator};
pub use generators::{ExpandOutput, RusdGrid, default_registry, expand_doc};
pub use pipeline::{RusdInstances, RusdSamplePoints, points_path};
pub use rng::Rng;
