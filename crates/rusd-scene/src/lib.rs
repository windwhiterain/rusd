//! # rusd-scene
//!
//! The scene layer (SPEC §3–4). M2 step 1: schema registry + validation.
//! M2 step 2 adds the openusd Stage adapter (TOML → Stage lowering, cook engine,
//! session overlay, diagnostics mapping).

pub mod schema;
pub mod validate;

pub use schema::{PropType, SchemaDef, SchemaKind};
pub use validate::validate;
