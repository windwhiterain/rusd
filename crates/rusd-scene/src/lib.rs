//! # rusd-scene
//!
//! The scene layer (SPEC §3–4):
//! - [`schema`]: typed property registry for builtin + generator schemas.
//! - [`validate`]: schema/type/required checks producing stable-code diagnostics.
//! - [`adapter`]: lower a parsed document into a composed openusd Stage (D3),
//!   query it, map composition diagnostics to Rusd JSON, and edit non-destructively
//!   via the session [`Overlay`] (provenance + undo).

pub mod adapter;
pub mod schema;
pub mod validate;

pub use adapter::{
    EditRecord, Overlay, PrimInfo, composition_diags, lower, lower_with_session, prim_count, query,
};
pub use schema::{PropType, SchemaDef, SchemaKind};
pub use validate::validate;
