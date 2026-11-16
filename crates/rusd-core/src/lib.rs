//! # rusd-core
//!
//! Shared vocabulary of the Rusd dialect (see `docs/SPEC.md`):
//!
//! - [`path`]: absolute prim paths, the addressing scheme every edit targets.
//! - [`value`]: typed property values and their canonical literal rendering.
//! - [`diag`]: machine-readable diagnostics, the feedback half of the agent loop.
//!
//! This crate is deliberately dependency-free: it is the floor everything else
//! builds on, and it must compile and test offline.

pub mod diag;
pub mod path;
pub mod value;

pub use diag::{Diagnostic, Severity};
pub use path::{PathError, PrimPath};
pub use value::Value;
