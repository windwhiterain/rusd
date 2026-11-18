//! Visual-reference capture (`docs/PROTOCOL.md` v1.1) — the **client** half, with
//! no window and no GPU in it.
//!
//! A capture is how a human hands an agent a picture: the viewer renders the
//! frame it is showing, draws a mask of the region the user dragged, and sends
//! both plus the camera that took it as one `capture{camera, image, mask,
//! scene_version, note}` request. The server keeps the bytes and answers
//! `{id}`; the id is what a person pastes into a conversation.
//!
//! Everything here is a pure function of data the caller already holds, so the
//! three rules the protocol actually states are testable without a screen:
//!
//! - **the mask is the image's size.** A mask in another resolution cannot be
//!   laid over its image, and the server refuses it (`-32602`). Both are produced
//!   by [`encode_png`] from buffers of one frame size, and [`mask_rgba`] is
//!   given the frame's size rather than the selection's.
//! - **white is the selection, black is the rest.** One byte per channel, alpha
//!   opaque, so a reader that knows nothing about the selection still sees a
//!   picture.
//! - **the camera is this side's** (`docs/REMOTE.md` §6): the capture carries the
//!   orbit camera at the moment of the screenshot, in world space, so an agent can
//!   re-render at the same pose. Nothing here asks the server about a camera.
//!
//! Offline (`--usdc`) a capture is still possible — the frame is real even when no
//! server sent it — and it says `scene_version: 0`, because there is no version.

use egui::{Pos2, Rect};
use rusd_scene::geometry::Camera;
use serde_json::{Value, json};

use crate::camera::Orbit;
use crate::compose::base64_encode;

/// The selection, as whole frame texels: `x0..x1` × `y0..y1`, half-open, so
/// `x1 - x0` is the count and a selection that touches one texel is 1 wide.
///
/// A drag **includes the texel it ends on**, at both ends: the corners are the
/// texels the pointer touched, and a mask that dropped either would leave out a
/// pixel the user drew over. So a drag across a 200-texel frame from texel 0 to
/// texel 199 is `0..200`, and one from 0 to 100 is `0..101`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionRect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl SelectionRect {
    /// How many pixels wide (≥ 1 for a rect [`rect_from_drag`] returned).
    pub fn width(self) -> u32 {
        self.x1.saturating_sub(self.x0)
    }

    /// How many pixels tall.
    pub fn height(self) -> u32 {
        self.y1.saturating_sub(self.y0)
    }

    /// How many pixels the selection marks white.
    pub fn area(self) -> u32 {
        self.width() * self.height()
    }
}

/// The frame texels a drag covers: two viewport points mapped through the same
/// point → pixel rule a pick uses (`crate::app`'s `frame_pixels`), then turned
/// into the half-open rect a mask is drawn from.
///
/// `None` when the frame or the viewport has no area — there is then nothing to
/// map into, and a mask of 0×0 is not a mask.
pub fn rect_from_drag(
    viewport: Rect,
    start: Pos2,
    end: Pos2,
    frame: (u32, u32),
) -> Option<SelectionRect> {
    let (w, h) = frame;
    if w == 0 || h == 0 || viewport.width() <= 0.0 || viewport.height() <= 0.0 {
        return None;
    }
    let to_pixel = |p: Pos2| -> (u32, u32) {
        let fx = ((p.x - viewport.left()) / viewport.width()).clamp(0.0, 1.0);
        let fy = ((p.y - viewport.top()) / viewport.height()).clamp(0.0, 1.0);
        let x = (fx * w as f32) as u32;
        let y = (fy * h as f32) as u32;
        (x.min(w - 1), y.min(h - 1))
    };
    let (ax, ay) = to_pixel(start);
    let (bx, by) = to_pixel(end);
    let rect = SelectionRect {
        x0: ax.min(bx),
        y0: ay.min(by),
        x1: (ax.max(bx) + 1).min(w),
        y1: (ay.max(by) + 1).min(h),
    };
    if rect.width() == 0 || rect.height() == 0 {
        return None;
    }
    Some(rect)
}

/// The mask for one selection over a frame of `frame` size: RGBA8, **white inside
/// the selection** and black outside, alpha opaque.
///
/// The size is the *frame's*, never the selection's — that is the invariant the
/// server checks (`capture: the mask is WxH but the image is WxH`).
pub fn mask_rgba(frame: (u32, u32), sel: SelectionRect) -> Vec<u8> {
    let (w, h) = frame;
    let mut out = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for y in 0..h {
        for x in 0..w {
            let inside = x >= sel.x0 && x < sel.x1 && y >= sel.y0 && y < sel.y1;
            if inside {
                out.extend_from_slice(&[255, 255, 255, 255]);
            } else {
                out.extend_from_slice(&[0, 0, 0, 255]);
            }
        }
    }
    out
}

/// A buffer that can be read back after the PNG encoder has consumed it.
///
/// `png`'s `Writer` takes ownership of its sink and `finish()` does not hand it
/// back, so the bytes are collected through a shared handle instead of a
/// temporary file: a capture is written once and must not touch the disk on this
/// side (`docs/PROTOCOL.md`: the server keeps the bytes).
struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut g = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("the capture buffer was poisoned"))?;
        g.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Encode RGBA8 pixels as one PNG, in memory.
///
/// `rgba` must hold `width * height * 4` bytes; the image and its mask go through
/// this one call, which is what keeps their sizes equal by construction.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let expect = (width as usize) * (height as usize) * 4;
    if width == 0 || height == 0 {
        return Err(format!("a {width}x{height} image has no pixels"));
    }
    if rgba.len() != expect {
        return Err(format!(
            "a {width}x{height} image needs {expect} bytes of RGBA, got {}",
            rgba.len()
        ));
    }
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    {
        let mut enc = png::Encoder::new(Sink(buf.clone()), width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(rgba).map_err(|e| e.to_string())?;
        // `finish` writes IEND and flushes; without it the image is one chunk short.
        writer.finish().map_err(|e| e.to_string())?;
    }
    let bytes = buf
        .lock()
        .map_err(|_| "the capture buffer was poisoned".to_owned())?
        .clone();
    if bytes.is_empty() {
        return Err("the PNG encoder wrote nothing".to_owned());
    }
    Ok(bytes)
}

/// The size a PNG header declares (`IHDR`, bytes 16..24), without decoding it.
///
/// The same reading the server does, so a test can assert "the mask is the
/// image's size" without an image decoder on this side.
pub fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if bytes.len() < 24 || bytes[..8] != SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((w, h))
}

/// The camera a capture carries: the client's orbit camera, **world absolute**,
/// in the shape `capture{camera:{position, look_at, up, fov_deg}}` names.
///
/// `up` is `+Z`, the renderer's own look-at up vector (`rusd_render`'s
/// `camera_matrix`), so an agent that re-renders at this pose gets this picture's
/// orientation and not its mirror.
pub fn camera_json(orbit: &Orbit) -> Value {
    camera_json_of(&orbit.camera())
}

/// The same for a camera already in hand.
///
/// A capture should normally carry **the frame's own camera**
/// ([`crate::net::Frame::camera`]), not the orbit the UI currently holds: after a
/// drag or a fit the orbit has moved on, and the picture on screen — the one the
/// mask was drawn over — was taken with the older one. Sending the orbit would
/// hand an agent a pose whose pixels do not line up with the capture.
pub fn camera_json_of(cam: &Camera) -> Value {
    json!({
        "position": [cam.position[0], cam.position[1], cam.position[2]],
        "look_at": [cam.look_at[0], cam.look_at[1], cam.look_at[2]],
        "up": [0.0, 0.0, 1.0],
        "fov_deg": cam.fov_deg,
    })
}

/// One `capture` request's params, ready for the wire.
///
/// `scene_version` is the version of the scene the frame shows, or `0` when there
/// is no server version behind it (`--usdc`). A blank note is the same as no note:
/// an empty string in a capture's meta reads like a note someone forgot to type.
pub fn capture_params(
    camera: Value,
    image_png: &[u8],
    mask_png: Option<&[u8]>,
    scene_version: u64,
    note: Option<&str>,
) -> Value {
    let mut params = json!({
        "camera": camera,
        "image": base64_encode(image_png),
        "scene_version": scene_version,
    });
    if let Some(mask) = mask_png {
        params["mask"] = json!(base64_encode(mask));
    } else {
        params["mask"] = Value::Null;
    }
    match note {
        Some(n) if !n.trim().is_empty() => params["note"] = json!(n),
        _ => {}
    }
    params
}

/// What lands on the clipboard after a capture: the id, and nothing else — the
/// user pastes it into a sentence (`docs/PROTOCOL.md`: 「已拷贝 <id>」).
pub fn capture_clipboard(id: &str) -> String {
    id.to_owned()
}

/// What lands on the clipboard after a prim is picked in the tree: its prim path,
/// which is the id an agent's tools take (`prim:`'s own spelling, without the
/// `prim:` prefix).
pub fn prim_clipboard(path: &str) -> String {
    path.to_owned()
}

/// The toast line for a copy: 「已拷贝 <what>」.
pub fn copied_toast_zh(what: &str) -> String {
    format!("已拷贝 {what}")
}
