//! # rusd-core
//!
//! Shared vocabulary of the Rusd dialect (see `docs/SPEC.md`):
//!
//! - [`path`]: absolute prim paths, the addressing scheme every edit targets.
//! - [`value`]: typed property values and their canonical literal rendering.
//! - [`diag`]: machine-readable diagnostics, the feedback half of the agent loop.
//! - [`syntax`]: structural parser for the `.rusd` TOML profile (toml_edit CST).
//! - [`fmt`]: canonical writer (`rusd fmt`), idempotent and comment-preserving.
//! - [`incremental`]: block-level incremental parser (SPEC §6, "incremental ≡ full").
//!
//! Path/value/diag are dependency-free; syntax/fmt/incremental pull `toml_edit` only.

pub mod diag;
pub mod fmt;
pub mod graph;
pub mod incremental;
pub mod path;
pub mod syntax;
pub mod value;

pub use diag::{Diagnostic, Severity};
pub use fmt::canonicalize;
pub use graph::parse as parse_graph;
pub use incremental::{IncrementalParser, Stats, TextBlock, text_blocks};
pub use path::{PathError, PrimPath};
pub use syntax::{Annotation, ParsedDocument, PrimBlock};
pub use value::Value;
