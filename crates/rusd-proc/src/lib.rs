//! # rusd-proc
//!
//! Procedural semantics (SPEC §5). Lands in M3:
//!
//! - `rng`: ChaCha8 streams keyed by `hash(stage.seed, prim_path, local_seed)`
//!   so editing one branch never reshuffles another.
//! - `expand`: lazy, incremental, content-hash-cached expansion of generator
//!   prims into deterministic subtrees (`i000000`, ...).
//! - `generators`: RusdScatter, RusdGrid, RusdInstance first; LSystem, Terrain,
//!   Wfc later.
//! - `budget`: max_prims / max_triangles enforcement and dry-run cost estimates.

pub use rusd_core as core;
pub use rusd_scene as scene;
