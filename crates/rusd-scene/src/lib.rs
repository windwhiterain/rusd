//! # rusd-scene
//!
//! The composed scene (SPEC §1, §3–4). Lands in M2:
//!
//! - `stage`: stage metadata, prim index, property resolution.
//! - `layer`: layer stack and opinions; session overlay with provenance.
//! - `compose`: the documented LIVERPS subset — sublayers, references,
//!   variant sets, payloads. No inherits/specializes in v0.
//! - `schema`: schema registry, typed property constraints, validation
//!   producing [`rusd_core::Diagnostic`]s.

pub use rusd_core as core;
