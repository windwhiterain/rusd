//! # rusd-viewer (spike ②)
//!
//! Client-side, headless proof of the USD-layer remote design (SPEC D12 v2):
//! a composite USD stack that exists **only in memory** — a root layer, its
//! sublayers and a session layer, all handed over as bytes — composes and
//! renders exactly like the same stack on disk.
//!
//! - [`memresolver`]: [`MemResolver`], an [`openusd::ar::Resolver`] serving
//!   layer bytes keyed by virtual path. Relative `subLayers` / reference paths
//!   anchor inside the mirror, so composition sees a real layer graph.
//! - [`scene`]: geometry extraction + a bbox-fit look-at camera (focal 35,
//!   aperture 36×20.25) and the offscreen render to PNG, sharing `rusd-cli
//!   render`'s default [`RenderConfig`] so images are comparable.
//! - [`compose`]: the manifest → mirror → stage path (`session_layer` +
//!   `open`), the shape a remote client would run.
//!
//! No network, no egui, no filesystem beyond the one `.usdc` read and the PNG
//! written: everything between is memory.

pub mod compose;
pub mod memresolver;
pub mod scene;

use rusd_core::Diagnostic;

pub use compose::{LayerBlob, Manifest, Stack};
pub use memresolver::{MemResolver, RESOLVER_IDENTITY};
pub use scene::{BBox, snapshot_png};

/// Wrap one non-Rusd error as a single Rusd diagnostic with a stable code.
pub fn diag(code: &str, e: impl std::fmt::Display) -> Diagnostic {
    Diagnostic::error(code, e.to_string())
}
