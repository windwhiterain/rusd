//! # Visual-reference captures (`docs/PROTOCOL.md` v1.1, D13)
//!
//! A capture is how a human hands an agent a picture: the viewer renders a frame
//! plus a mask of the selected region, calls `capture`, and pastes the returned
//! id into the conversation. The server keeps the bytes so the agent can fetch
//! them back over the same protocol (`capture_get`) or list what exists
//! (`capture_list`).
//!
//! ## Where they live
//!
//! `<root>/<id>.png`, `<root>/<id>.mask.png` (when there is a mask) and
//! `<root>/<id>.meta.json`, with `root` = `<cwd>/target/captures` — under the
//! build directory, so a capture never lands in a repository by accident. The
//! directory is created on `bind`; a `bind` in a directory that already holds
//! captures reads them back into the index and carries the numbering on from
//! the highest id it finds, so ids stay unique across restarts.
//!
//! ## Ids
//!
//! `cap-<seq:04d>`, `seq` starting at 1 and increasing by one per capture. A
//! deleted capture leaves its number unused rather than reassigning it: an id
//! someone pasted into a conversation must not later mean a different picture.
//!
//! ## Sizes without decoding
//!
//! The mask must be the same size as the image — a mask in another resolution
//! cannot be overlaid, and the viewer that produced it has a bug. Both are PNGs,
//! so the size is read out of the PNG header (IHDR, bytes 16..24) rather than
//! decoded: this crate has no image decoder and does not need one to answer
//! "same size?".
//!
//! A capture is **not** a scene mutation: it does not advance `version` and
//! broadcasts nothing.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::transport::{base64_decode, base64_encode};

/// The JSON-RPC code this module refuses with (protocol §传输: invalid params).
const INVALID_PARAMS: i64 = -32602;

/// The PNG file signature, which every capture must start with.
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// A capture store rooted at one directory.
pub struct CaptureStore {
    root: PathBuf,
    /// The next `seq` to hand out. Carried on from the highest id already on
    /// disk when the store opens.
    next_seq: u64,
}

/// What one `capture` call carries, already decoded.
struct Incoming {
    camera: Value,
    image: Vec<u8>,
    mask: Option<Vec<u8>>,
    scene_version: u64,
    note: Option<String>,
}

impl CaptureStore {
    /// Open (and create, if needed) a store at `root`, indexing the captures
    /// already there. Returns the store and the ids it found, strongest first
    /// being the newest.
    pub fn open(root: impl Into<PathBuf>) -> io::Result<CaptureStore> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let mut next_seq = 1;
        for entry in fs::read_dir(&root)?.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some(seq) = capture_seq(name) {
                next_seq = next_seq.max(seq + 1);
            }
        }
        Ok(CaptureStore { root, next_seq })
    }

    /// Every capture on disk, oldest first, as `capture_list{}`'s array.
    pub fn index(&self) -> Vec<Value> {
        let mut found: Vec<(u64, String)> = Vec::new();
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            // Only the image's own name makes a capture: its mask and its meta
            // share the number, and a stray pair of those is an incomplete
            // write, not a capture.
            let Some(seq) = capture_seq(name) else {
                continue;
            };
            if capture_file_kind(name) != Some(CaptureFile::Image) {
                continue;
            }
            found.push((seq, id_for(seq)));
        }
        found.sort();
        found.dedup();
        found
            .into_iter()
            .filter_map(|(_, id)| self.summary(&id).ok())
            .collect()
    }

    /// `capture{camera, image, mask, scene_version, note}` → `{id}`.
    pub fn capture(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let incoming = parse_capture(params)?;
        let seq = self.next_seq;
        let id = id_for(seq);
        let image = self.root.join(format!("{id}.png"));
        let mask = self.root.join(format!("{id}.mask.png"));
        let meta = self.root.join(format!("{id}.meta.json"));

        let (width, height) = write(&image, &incoming.image).map_err(|e| {
            (
                INVALID_PARAMS,
                format!("capture: cannot write {image:?}: {e}"),
            )
        })?;

        let mut record = BTreeMap::new();
        record.insert("id", json!(id));
        record.insert("camera", incoming.camera.clone());
        record.insert("scene_version", json!(incoming.scene_version));
        record.insert("created_at", json!(now_millis()));
        record.insert("width", json!(width));
        record.insert("height", json!(height));
        if let Some(note) = &incoming.note {
            record.insert("note", json!(note));
        }
        if let Some(bytes) = &incoming.mask {
            write(&mask, bytes).map_err(|e| {
                (
                    INVALID_PARAMS,
                    format!("capture: cannot write {mask:?}: {e}"),
                )
            })?;
            record.insert("has_mask", json!(true));
        } else {
            record.insert("has_mask", json!(false));
        }
        // The meta lands last: a capture whose meta is missing is one the index
        // skips, which is the right reading of a half-written capture.
        let text = serde_json::to_string_pretty(&record)
            .map_err(|e| (INVALID_PARAMS, format!("capture: cannot encode meta: {e}")))?;
        fs::write(&meta, format!("{text}\n")).map_err(|e| {
            (
                INVALID_PARAMS,
                format!("capture: cannot write {meta:?}: {e}"),
            )
        })?;

        self.next_seq = seq + 1;
        Ok(json!({ "id": id }))
    }

    /// `capture_get{id}` — the whole capture, image and mask included.
    pub fn get(&self, params: &Value) -> Result<Value, (i64, String)> {
        let id = capture_id(params, "capture_get")?;
        let meta = self.read_meta(&id)?;
        let image = fs::read(self.root.join(format!("{id}.png"))).map_err(|e| {
            (
                INVALID_PARAMS,
                format!("capture_get: {id} has no image: {e}"),
            )
        })?;
        let mask_path = self.root.join(format!("{id}.mask.png"));
        let mask = match fs::read(&mask_path) {
            Ok(bytes) => json!(base64_encode(&bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Value::Null,
            Err(e) => {
                return Err((
                    INVALID_PARAMS,
                    format!("capture_get: cannot read {mask_path:?}: {e}"),
                ));
            }
        };
        Ok(json!({
            "id": id,
            "camera": meta.get("camera").cloned().unwrap_or(Value::Null),
            "image": base64_encode(&image),
            "mask": mask,
            "scene_version": meta.get("scene_version").cloned().unwrap_or(json!(0)),
            "created_at": meta.get("created_at").cloned().unwrap_or(json!(0)),
            "note": meta.get("note").cloned().unwrap_or(Value::Null),
        }))
    }

    /// `capture_list{}` — every capture's summary, oldest first.
    pub fn list(&self, _params: &Value) -> Result<Value, (i64, String)> {
        Ok(Value::Array(self.index()))
    }

    /// One capture's summary, from its meta file.
    fn summary(&self, id: &str) -> Result<Value, (i64, String)> {
        let meta = self.read_meta(id)?;
        Ok(json!({
            "id": id,
            "created_at": meta.get("created_at").cloned().unwrap_or(json!(0)),
            "scene_version": meta.get("scene_version").cloned().unwrap_or(json!(0)),
            "note": meta.get("note").cloned().unwrap_or(Value::Null),
            "has_mask": meta.get("has_mask").cloned().unwrap_or(json!(false)),
        }))
    }

    fn read_meta(&self, id: &str) -> Result<Value, (i64, String)> {
        let path = self.root.join(format!("{id}.meta.json"));
        let text = fs::read_to_string(&path).map_err(|e| {
            (
                INVALID_PARAMS,
                format!("unknown capture {id}: cannot read {path:?} ({e})"),
            )
        })?;
        serde_json::from_str(&text).map_err(|e| {
            (
                INVALID_PARAMS,
                format!("capture {id}: its meta is not JSON: {e}"),
            )
        })
    }
}

// --- parsing ----------------------------------------------------------------

fn parse_capture(params: &Value) -> Result<Incoming, (i64, String)> {
    let camera = params.get("camera").cloned().ok_or((
        INVALID_PARAMS,
        "capture: `camera` (object) required".to_owned(),
    ))?;
    if !camera.is_object() {
        return Err((INVALID_PARAMS, "capture: `camera` must be an object".into()));
    }
    let image = match params.get("image").and_then(Value::as_str) {
        Some(text) => base64_decode(text)
            .ok_or((INVALID_PARAMS, "capture: `image` is not base64".to_owned()))?,
        None => {
            return Err((
                INVALID_PARAMS,
                "capture: `image` (base64 PNG) required".into(),
            ));
        }
    };
    let (image_width, image_height) = png_size(&image).ok_or((
        INVALID_PARAMS,
        "capture: `image` is not a PNG (no IHDR header found)".to_owned(),
    ))?;
    let mask = match params.get("mask") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => {
            let bytes = base64_decode(text)
                .ok_or((INVALID_PARAMS, "capture: `mask` is not base64".to_owned()))?;
            let (mask_width, mask_height) = png_size(&bytes).ok_or((
                INVALID_PARAMS,
                "capture: `mask` is not a PNG (no IHDR header found)".to_owned(),
            ))?;
            if (mask_width, mask_height) != (image_width, image_height) {
                return Err((
                    INVALID_PARAMS,
                    format!(
                        "capture: the mask is {mask_width}x{mask_height} but the image is \
                         {image_width}x{image_height}; a mask must be the same size as its image"
                    ),
                ));
            }
            Some(bytes)
        }
        Some(_) => {
            return Err((
                INVALID_PARAMS,
                "capture: `mask` must be base64 or null".into(),
            ));
        }
    };
    let scene_version = match params.get("scene_version") {
        None | Some(Value::Null) => 0,
        Some(v) => v.as_u64().ok_or((
            INVALID_PARAMS,
            "capture: `scene_version` must be a non-negative integer".to_owned(),
        ))?,
    };
    let note = match params.get("note") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err((INVALID_PARAMS, "capture: `note` must be a string".into())),
    };
    Ok(Incoming {
        camera,
        image,
        mask,
        scene_version,
        note,
    })
}

fn capture_id(params: &Value, method: &str) -> Result<String, (i64, String)> {
    match params.get("id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => Ok(id.to_owned()),
        Some(_) => Err((INVALID_PARAMS, format!("{method}: `id` must not be empty"))),
        None => Err((INVALID_PARAMS, format!("{method}: `id` (string) required"))),
    }
}

// --- the PNG header ---------------------------------------------------------

/// A PNG's pixel size from its IHDR chunk, without decoding the image.
///
/// The signature is 8 bytes, then the first chunk: a 4-byte big-endian length,
/// the 4-byte type `IHDR`, then the width (bytes 16..20) and the height
/// (20..24), both big-endian. Anything shorter or mislabelled is not a PNG.
pub fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != PNG_SIGNATURE {
        return None;
    }
    if &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    (width > 0 && height > 0).then_some((width, height))
}

/// Write one capture file, reporting the size it wrote.
fn write(path: &Path, bytes: &[u8]) -> io::Result<(u32, u32)> {
    let size =
        png_size(bytes).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a PNG"))?;
    fs::write(path, bytes)?;
    Ok(size)
}

/// `cap-0001` for seq 1.
fn id_for(seq: u64) -> String {
    format!("cap-{seq:04}")
}

/// The three kinds of file one capture owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureFile {
    Image,
    Mask,
    Meta,
}

/// Which of a capture's files `name` is, if it is one of them.
fn capture_file_kind(name: &str) -> Option<CaptureFile> {
    let rest = name.strip_prefix("cap-")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    match &rest[digits.len()..] {
        ".png" => Some(CaptureFile::Image),
        ".mask.png" => Some(CaptureFile::Mask),
        ".meta.json" => Some(CaptureFile::Meta),
        _ => None,
    }
}

/// The sequence number a file name carries, if it is one of this store's names.
///
/// `cap-0001.png` is a capture's image, `cap-0001.mask.png` its mask and
/// `cap-0001.meta.json` its meta; all three answer the same number, so the
/// numbering carries on across every file rather than only the images.
fn capture_seq(name: &str) -> Option<u64> {
    capture_file_kind(name)?;
    let rest = name.strip_prefix("cap-")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Milliseconds since the Unix epoch. A capture records when it was taken; the
/// number is written as-is, so two captures are comparable without a date
/// library.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-pixel PNG header plus a byte of padding: enough for the size read,
    /// and not a real image, which this module never needs.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = PNG_SIGNATURE.to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    #[test]
    fn png_size_reads_the_ihdr_and_rejects_non_pngs() {
        assert_eq!(png_size(&png(960, 540)), Some((960, 540)));
        assert_eq!(png_size(b"not a png at all, surely"), None);
        assert_eq!(png_size(&[]), None);
        // A truncated header is not enough to answer.
        assert_eq!(png_size(&png(960, 540)[..20]), None);
        // A zero dimension is not a picture.
        assert_eq!(png_size(&png(0, 540)), None);
        // The signature must be the PNG one.
        let mut wrong = png(4, 4);
        wrong[1] = b'X';
        assert_eq!(png_size(&wrong), None);
        // The first chunk must be IHDR.
        let mut wrong = png(4, 4);
        wrong[12..16].copy_from_slice(b"IDAT");
        assert_eq!(png_size(&wrong), None);
    }

    #[test]
    fn ids_number_from_one_and_carry_on() {
        assert_eq!(id_for(1), "cap-0001");
        assert_eq!(id_for(42), "cap-0042");
        assert_eq!(capture_seq("cap-0001.png"), Some(1));
        assert_eq!(capture_seq("cap-0042.mask.png"), Some(42));
        assert_eq!(capture_seq("cap-0007.meta.json"), Some(7));
        assert_eq!(capture_seq("notes.txt"), None);
        assert_eq!(capture_seq("cap-.png"), None);
        assert_eq!(capture_seq("cap-0001"), None);
        assert_eq!(capture_seq("cap-0001.png.tmp"), None);
        // Only a `.png` that is the image itself is a capture's image: the mask
        // and meta share the number and must not be listed as captures.
        assert_eq!(capture_file_kind("cap-0001.png"), Some(CaptureFile::Image));
        assert_eq!(
            capture_file_kind("cap-0001.mask.png"),
            Some(CaptureFile::Mask)
        );
        assert_eq!(
            capture_file_kind("cap-0001.meta.json"),
            Some(CaptureFile::Meta)
        );
    }
}
