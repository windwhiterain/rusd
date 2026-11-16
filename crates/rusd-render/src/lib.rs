//! # rusd-render
//!
//! The agent's eyes and hands (SPEC §6). Lands in M4:
//!
//! - `device`: wgpu setup (DX12 here; Vulkan/Metal elsewhere), headless-first.
//! - `passes`: beauty, prim-id (flat stable id per prim ⇒ segmentation/picking),
//!   depth, normal, wireframe.
//! - `scene_gpu`: content-hash mesh cache, GPU instancing for generator output.
//! - `capture`: deterministic offscreen render → image + metadata JSON.
//! - `viewport`: winit + egui interactive mode with hot reload.
//!
//! Error-tolerant by contract: invalid prims become magenta placeholders plus
//! diagnostics; a render call always returns, never panics the agent loop.
//!
//! wgpu/winit/egui dependencies are added at M4, not now, so the workspace
//! builds offline until then.

pub use rusd_core as core;
pub use rusd_proc as proc;
pub use rusd_scene as scene;
