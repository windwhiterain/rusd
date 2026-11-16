//! # rusd-core
//!
//! Shared vocabulary of the Rusd dialect (see `docs/SPEC.md`):
//!
//! - [`path`]: absolute prim paths, the addressing scheme every edit targets.
//! - [`value`]: typed property values and their canonical literal rendering.
//! - [`diag`]: machine-readable diagnostics, the feedback half of the agent loop.
//! - [`syntax`]: structural parser for the `.rusd` TOML profile (toml_edit CST).
//!
//! Path/value/diag are dependency-free; syntax pulls `toml_edit` only.

pub mod diag;
pub mod path;
pub mod syntax;
pub mod value;

pub use diag::{Diagnostic, Severity};
pub use path::{PathError, PrimPath};
pub use value::Value;
