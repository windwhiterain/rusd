//! # rusd-scene
//!
//! The scene layer (SPEC §3–4):
//! - [`schema`]: typed property registry for USD schemas — what a prim's `typeName`
//!   names, and what its attributes are.
//! - [`operator`]: the operator vocabulary — what a graph node runs and what parameters
//!   it takes. A generator is an operator, not a schema; nothing here reaches a layer.
//! - [`validate`]: schema/type/required checks producing stable-code diagnostics.
//! - [`adapter`]: lower a parsed document into a composed openusd Stage (D3),
//!   query it, map composition diagnostics to Rusd JSON, and edit non-destructively
//!   via the session [`Overlay`] (provenance + undo).
//! - [`document`]: a layer as a `.rusd` document, and back (v1.9) — what a cooking
//!   product is written as and re-read from.

pub mod adapter;
pub mod document;
pub mod geometry;
pub mod operator;
pub mod reference;
pub mod schema;
pub mod validate;

pub use adapter::{
    EditRecord, Overlay, PrimInfo, composition_diags, lower, lower_with_session, prim_count, query,
};
pub use geometry::{Camera, GeoItem, Mat, Mesh, SceneGeometry, extract};
pub use schema::{PropType, SchemaDef};
pub use validate::validate;
