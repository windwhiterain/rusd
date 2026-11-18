//! # rusd-render
//!
//! The agent's eyes and hands (SPEC §7). M4 in progress:
//!
//! - geometry extraction lives in [`rusd_scene::geometry`] (Stage → meshes +
//!   world transforms + camera); this crate bakes/uploads it to the GPU.
//! - `device`: wgpu setup (DX12 here; Vulkan/Metal elsewhere), headless-first.
//!   Verified by `examples/wgpu_spike.rs` (RTX 3060 Ti, offscreen + readback).
//! - `passes`: beauty, prim-id (flat stable id per prim ⇒ segmentation/picking),
//!   depth, normal, wireframe.
//! - `scene_gpu`: content-hash mesh cache, GPU instancing for generator output.
//! - `capture`: deterministic offscreen render → image + metadata JSON.
//! - `viewport`: winit + egui interactive mode with hot reload.
//!
//! Error-tolerant by contract: invalid prims become magenta placeholders plus
//! diagnostics; a render call always returns, never panics the agent loop.

pub mod annotate;
pub mod capture;
pub mod render;

pub use render::{RenderConfig, RenderOutput, render, render_with};
pub use rusd_core as core;
pub use rusd_proc as proc;
pub use rusd_scene as scene;
