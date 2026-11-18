//! # rusd-proc
//!
//! Procedural runtime (SPEC §5–6):
//! - [`generator`]: the Generator trait + GenContext (read-tracked composed inputs).
//! - [`cook`]: the cook engine — an implicit path-dependency DAG (D8) that wraps
//!   USD composition (D9), memoized by content hash with downstream dirty propagation.
//! - [`rng`]: path-keyed deterministic ChaCha8 RNG.
//! - [`city`]: the city generator suite (D4): StreetGrid / Building / Scatter.
//! - [`generators`]: RusdGrid + default registry + expand_doc.

pub mod city;
pub mod cook;
pub mod generator;
pub mod generators;
pub mod rng;

pub use city::{RusdBuilding, RusdScatter, RusdStreetGrid};
pub use cook::{CookEngine, CookNode, CookOp, CookResult, CookStats, nodes_from_doc};
pub use generator::{GenContext, Generator};
pub use generators::{ExpandOutput, RusdGrid, default_registry, expand_doc};
pub use rng::Rng;
