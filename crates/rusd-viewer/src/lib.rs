//! # rusd-viewer
//!
//! The client side of SPEC decision D12 (`docs/REMOTE.md`): the *scene* crosses
//! the network, never the pixels. A composite USD stack arrives as bytes,
//! composes in memory, renders locally, and every camera move, click and
//! selection after that costs no round trip.
//!
//! - [`memresolver`]: [`MemResolver`], an [`openusd::ar::Resolver`] serving
//!   layer bytes keyed by virtual path. Relative `subLayers` / reference paths
//!   anchor inside the mirror, so composition sees a real layer graph.
//! - [`compose`]: the manifest → mirror → stage path (`session_layer` +
//!   `open`), and the base64 the wire uses.
//! - [`scene`]: geometry extraction + a bbox-fit look-at camera (focal 35,
//!   aperture 36×20.25) and the one-shot offscreen render to PNG.
//! - [`gpu`]: the interactive viewport — the same beauty/prim-id passes as
//!   `rusd_render`, on a device created **once**, so a camera move is a redraw
//!   rather than a new GPU.
//! - [`protocol`]: D12 wire protocol v1's client half: request lines
//!   (`hello` / `subscribe` / `opinion` / `undo` / `redo`, plus any other method a
//!   panel needs), the notification parser, the monotonic-version discard rule,
//!   and the gizmo throttle.
//! - [`tree`]: the composition tree (`docs/PROTOCOL.md` v1.2) as data — the layer
//!   stack, its arcs, its prim nodes and its variant sets, the panel's row model,
//!   and the four `comp_node` details.
//! - [`capture`]: a visual-reference capture (v1.1) built on this side: the drag's
//!   rectangle, the mask at the frame's own size, the PNG encoding, the camera the
//!   frame was drawn with, and the clipboard payloads.
//! - [`panel`]: the tree and capture panels' widgets, which only draw and report —
//!   the shell carries out what they ask.

//! - [`camera`]: the client's own orbit / pan / zoom and the screen↔world
//!   mapping a pick needs.
//! - [`gizmo`]: the client's world→local conversion for a drag — an opinion's
//!   `value` is the field's own op, so a local translation is what a move means.
//! - [`anchor`]: the drag anchor read from the composed stage (authored local
//!   translate, parent world matrix, world centre).
//! - [`session`]: the snapshot pipeline — bytes → base64 → single-layer stack
//!   → fitted geometry — plus the pure [`session::ClientModel`] the view drives.
//! - [`net`]: the TCP client thread (owner of `hello`/`subscribe`/outgoing
//!   edits) and the render thread that owns the GPU.
//! - [`fonts`]: system CJK font loading, so Chinese UI text is glyphs and not
//!   tofu (the built-in egui faces are Latin-only).
//! - [`window`]: the opening rectangle — 1280×720 centred on the primary monitor.
//! - [`view`]: the view's own decisions (which scene frames the camera, and how a
//!   frame's corners are judged), so the window and its tests run one copy of the
//!   logic instead of two that can drift.
//! - [`viewport`]: where the render thread's frame lands on screen — pure rect/UV
//!   arithmetic, so the image covers the viewport 1:1 at any `pixels_per_point`.
//! - [`app`]: the winit + egui shell that ties them together (`view`).

pub mod anchor;
pub mod app;
pub mod camera;
pub mod capture;
pub mod compose;
pub mod fonts;
pub mod gizmo;
pub mod gpu;
pub mod memresolver;
pub mod net;
pub mod panel;
pub mod protocol;
pub mod scene;
pub mod session;
pub mod tree;
pub mod view;
pub mod viewport;
pub mod window;

use rusd_core::Diagnostic;

pub use compose::{LayerBlob, Manifest, Stack};
pub use memresolver::{MemResolver, RESOLVER_IDENTITY};
pub use scene::{BBox, snapshot_png};

/// Wrap one non-Rusd error as a single Rusd diagnostic with a stable code.
pub fn diag(code: &str, e: impl std::fmt::Display) -> Diagnostic {
    Diagnostic::error(code, e.to_string())
}
