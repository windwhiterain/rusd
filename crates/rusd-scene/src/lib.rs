//! # rusd-scene
//!
//! The scene layer (SPEC §3–4):
//! - [`schema`]: typed property registry for builtin + generator schemas.
//! - [`validate`]: schema/type/required checks producing stable-code diagnostics.
//! - [`adapter`]: lower a parsed document into a composed openusd Stage (D3),
//!   query it, and map composition diagnostics back to Rusd JSON.

pub mod adapter;
pub mod schema;
pub mod validate;

pub use adapter::{PrimInfo, composition_diags, lower, prim_count, query};
pub use schema::{PropType, SchemaDef, SchemaKind};
pub use validate::validate;
