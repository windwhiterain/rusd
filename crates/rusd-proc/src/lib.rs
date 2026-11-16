//! # rusd-proc
//!
//! Procedural runtime (SPEC §5–6):
//! - [`generator`]: the Generator trait + GenContext (read-tracked composed inputs).
//! - [`cook`]: the cook engine — an implicit path-dependency DAG (D8) that wraps
//!   USD composition (D9), memoized by content hash with downstream dirty propagation.
//! - [`generators`]: built-in generators (M2: RusdGrid), default registry, expand_doc.
//!
//! City generators (StreetGrid/Building/BlockFill/Scatter) register here in M3.

pub mod cook;
pub mod generator;
pub mod generators;

pub use cook::{CookEngine, CookNode, CookResult, CookStats, nodes_from_doc};
pub use generator::{GenContext, Generator};
pub use generators::{ExpandOutput, RusdGrid, default_registry, expand_doc};
