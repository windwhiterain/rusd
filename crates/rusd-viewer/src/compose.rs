//! Manifest → in-memory mirror → composed [`Stage`], the client half of the
//! USD-layer remote (SPEC D12 v2).
//!
//! A manifest names one root layer, an optional session layer, and any other
//! layers the stack reaches (typically the cook output a root layer sublayers),
//! each as either `usda` text or base64 `usdc` bytes. [`Stack::build`] decodes
//! every layer, hands all of them to one [`MemResolver`], and opens the root
//! with the session prepended — the exact `Stage::builder().resolver(..)
//! .session_layer(..).open(..)` call the shipped server-side path uses, only
//! over memory instead of a filesystem.
//!
//! Nothing here writes a file: the composed stage's opinions come back out of
//! the mirror, so "the mirror composes the same as the stack on disk" is a claim
//! a test can check opinion by opinion.

use std::collections::BTreeMap;

use openusd::sdf::{self, Layer};
use openusd::usd::{Prim, Stage};
use rusd_core::Diagnostic;
use serde_json::Value as Json;

use crate::diag;
use crate::memresolver::MemResolver;

/// One layer of a composite stack, as it travels over the wire.
#[derive(Clone, Debug, PartialEq)]
pub struct LayerBlob {
    /// Virtual path inside the mirror (`/root.usda`, `/cooks/city.usdc`). A
    /// relative `subLayers` entry in another layer resolves against the
    /// directory of this one.
    pub identifier: String,
    /// The storage encoding: `usda` (text) or `usdc` (crate binary).
    pub encoding: Encoding,
    /// Text for `usda`, base64 for `usdc`.
    pub data: String,
}

/// How a layer's `data` is carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// `usda` text, carried verbatim.
    Usda,
    /// `usdc` crate binary, carried base64.
    Usdc,
}

impl Encoding {
    /// The word a manifest uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Usda => "usda",
            Encoding::Usdc => "usdc",
        }
    }

    /// Parse a manifest's `encoding` word.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "usda" => Some(Encoding::Usda),
            "usdc" => Some(Encoding::Usdc),
            _ => None,
        }
    }

    /// The default virtual extension for a layer carried this way.
    pub fn extension(self) -> &'static str {
        match self {
            Encoding::Usda => "usda",
            Encoding::Usdc => "usdc",
        }
    }
}

impl LayerBlob {
    /// A layer carried as `usda` text.
    pub fn usda(identifier: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            identifier: identifier.into(),
            encoding: Encoding::Usda,
            data: text.into(),
        }
    }

    /// A layer carried as `usdc` bytes (base64 on the wire, decided here).
    pub fn usdc(identifier: impl Into<String>, bytes: &[u8]) -> Self {
        Self {
            identifier: identifier.into(),
            encoding: Encoding::Usdc,
            data: base64_encode(bytes),
        }
    }

    /// The layer's bytes, decoded from whichever encoding it carries.
    pub fn bytes(&self) -> Result<Vec<u8>, Vec<Diagnostic>> {
        match self.encoding {
            Encoding::Usda => Ok(self.data.as_bytes().to_vec()),
            Encoding::Usdc => base64_decode(&self.data).ok_or_else(|| {
                vec![diag(
                    "E_MANIFEST",
                    format!("{}: usdc data is not valid base64", self.identifier),
                )]
            }),
        }
    }
}

/// A composite stack as a client receives it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Manifest {
    /// The root layer's virtual identifier; the stage opens here.
    pub root: String,
    /// The session layer's virtual identifier, if the stack has one. It is
    /// prepended to the layer stack, so its opinions are the strongest.
    pub session: Option<String>,
    /// Every layer the stack needs, root and session included.
    pub layers: Vec<LayerBlob>,
}

impl Manifest {
    /// Parse the JSON a remote client sends: `{root, session?, layers:[{identifier,
    /// encoding, data}]}`.
    pub fn from_json(text: &str) -> Result<Self, Vec<Diagnostic>> {
        let v: Json = serde_json::from_str(text)
            .map_err(|e| vec![diag("E_MANIFEST", format!("manifest is not JSON: {e}"))])?;
        let obj = v.as_object().ok_or_else(|| {
            vec![diag(
                "E_MANIFEST",
                "manifest must be a JSON object {root, session?, layers[]}",
            )]
        })?;
        let root = obj
            .get("root")
            .and_then(Json::as_str)
            .ok_or_else(|| vec![diag("E_MANIFEST", "manifest needs a \"root\" identifier")])?
            .to_owned();
        let session = match obj.get("session") {
            None | Some(Json::Null) => None,
            Some(Json::String(s)) => Some(s.clone()),
            Some(_) => {
                return Err(vec![diag(
                    "E_MANIFEST",
                    "\"session\" must be a string identifier or null",
                )]);
            }
        };
        let list = obj
            .get("layers")
            .and_then(Json::as_array)
            .ok_or_else(|| vec![diag("E_MANIFEST", "manifest needs a \"layers\" array")])?;
        let mut layers = Vec::with_capacity(list.len());
        for (i, item) in list.iter().enumerate() {
            let l = item.as_object().ok_or_else(|| {
                vec![diag("E_MANIFEST", format!("layers[{i}] must be an object"))]
            })?;
            let identifier = l.get("identifier").and_then(Json::as_str).ok_or_else(|| {
                vec![diag(
                    "E_MANIFEST",
                    format!("layers[{i}] needs an \"identifier\""),
                )]
            })?;
            let word = l.get("encoding").and_then(Json::as_str).ok_or_else(|| {
                vec![diag(
                    "E_MANIFEST",
                    format!("layers[{i}] needs an \"encoding\" (\"usda\" or \"usdc\")"),
                )]
            })?;
            let encoding = Encoding::parse(word).ok_or_else(|| {
                vec![diag(
                    "E_MANIFEST",
                    format!("layers[{i}]: encoding \"{word}\" is neither \"usda\" nor \"usdc\""),
                )]
            })?;
            let data = l.get("data").and_then(Json::as_str).ok_or_else(|| {
                vec![diag(
                    "E_MANIFEST",
                    format!("layers[{i}] needs a \"data\" string"),
                )]
            })?;
            layers.push(LayerBlob {
                identifier: identifier.to_owned(),
                encoding,
                data: data.to_owned(),
            });
        }
        Ok(Self {
            root,
            session,
            layers,
        })
    }

    /// Render this manifest back to the JSON a client sends (deterministic:
    /// layers in their own order, keys in a fixed order).
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"root\":");
        push_json_string(&mut out, &self.root);
        out.push_str(",\"session\":");
        match &self.session {
            Some(s) => push_json_string(&mut out, s),
            None => out.push_str("null"),
        }
        out.push_str(",\"layers\":[");
        for (i, l) in self.layers.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"identifier\":");
            push_json_string(&mut out, &l.identifier);
            out.push_str(",\"encoding\":");
            push_json_string(&mut out, l.encoding.as_str());
            out.push_str(",\"data\":");
            push_json_string(&mut out, &l.data);
            out.push('}');
        }
        out.push_str("]}");
        out
    }

    /// Whether every identifier the manifest names is present in `layers`.
    pub fn check_complete(&self) -> Result<(), Vec<Diagnostic>> {
        let mut have = BTreeMap::new();
        for l in &self.layers {
            have.insert(
                crate::memresolver::canonical(&l.identifier),
                l.identifier.as_str(),
            );
        }
        let mut missing = Vec::new();
        for id in std::iter::once(&self.root).chain(self.session.as_ref()) {
            if !have.contains_key(&crate::memresolver::canonical(id)) {
                missing.push(id.clone());
            }
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(vec![diag(
                "E_MANIFEST",
                format!(
                    "manifest lacks the layer(s) it names: {}",
                    missing.join(", ")
                ),
            )])
        }
    }
}

/// A composed stage plus the mirror it was composed from.
pub struct Stack {
    /// The resolver everything was composed over.
    pub mem: MemResolver,
    /// The composed stage. `Stage` is `!Send`/`!Sync`, so it lives on the
    /// thread that built it; extract owned geometry before leaving that thread.
    pub stage: Stage,
    /// Composition diagnostics from the mirror (unresolved arcs, invalid
    /// layers, …), mapped to stable Rusd codes.
    pub diags: Vec<Diagnostic>,
}

impl Stack {
    /// Build a composite stack in memory and open its stage.
    ///
    /// Every layer is decoded first with [`Layer::from_bytes`], which sniffs
    /// content rather than trusting the `encoding` word, so a `usda` layer that
    /// actually holds crate bytes still opens. The decoded layer is then served
    /// under its manifest identifier, and the stack opens through
    /// `resolver(..) + session_layer(..) + open(..)` — the same builder calls a
    /// disk-backed open makes.
    pub fn build(manifest: &Manifest) -> Result<Self, Vec<Diagnostic>> {
        manifest.check_complete()?;
        let mut mem = MemResolver::new();
        let mut diags = Vec::new();
        for blob in &manifest.layers {
            let bytes = blob.bytes()?;
            match Layer::from_bytes(blob.identifier.clone(), bytes.clone()) {
                Ok(_) => mem.insert(&blob.identifier, bytes),
                Err(e) => {
                    // A layer that will not decode is still served verbatim, so
                    // the open reports it as the unresolved/unreadable layer it
                    // is rather than vanishing from the mirror.
                    diags.push(
                        diag("E_LAYER_DECODE", format!("{}: {}", blob.identifier, e))
                            .at_prim(blob.identifier.clone()),
                    );
                    mem.insert(&blob.identifier, bytes);
                }
            }
        }
        Self::open(mem, &manifest.root, manifest.session.as_deref(), diags)
    }

    /// Open a root layer (plus an optional session layer) over `mem`.
    pub fn open(
        mem: MemResolver,
        root: &str,
        session: Option<&str>,
        mut diags: Vec<Diagnostic>,
    ) -> Result<Self, Vec<Diagnostic>> {
        let mut builder = Stage::builder().resolver(mem.clone());
        if let Some(session) = session {
            builder = builder.session_layer(session);
        }
        let stage = builder
            .open(root)
            .map_err(|e| vec![diag("E_STAGE_OPEN", format!("{root}: {e}"))])?;
        diags.extend(rusd_scene::composition_diags(&stage));
        Ok(Self { mem, stage, diags })
    }

    /// The composed value of `attr` on `prim`, as a comparable [`sdf::Value`].
    pub fn attribute(&self, prim: &str, attr: &str) -> Option<sdf::Value> {
        let p = sdf::path(prim).ok()?;
        let pr = self.stage.prim(p).ok()?;
        pr.attribute(attr).get::<sdf::Value>().ok().flatten()
    }

    /// Every authored property on `prim`, sorted by name — the opinion list two
    /// compositions are compared over, so nothing depends on hash order.
    pub fn authored_properties(&self, prim: &str) -> Vec<(String, sdf::Value)> {
        let Ok(p) = sdf::path(prim) else {
            return Vec::new();
        };
        let Ok(pr) = self.stage.prim(p) else {
            return Vec::new();
        };
        let mut out: Vec<(String, sdf::Value)> = Vec::new();
        for a in pr.authored_attributes().unwrap_or_default() {
            if let Ok(Some(v)) = a.get::<sdf::Value>() {
                out.push((a.name().to_owned(), v));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The composed prim at `prim`, or `None` if the stage does not hold it.
    pub fn prim(&self, prim: &str) -> Option<Prim> {
        sdf::path(prim).ok().and_then(|p| self.stage.prim(p).ok())
    }

    /// Every property opinion of every prim in `subtree`, sorted, as
    /// `(prim path, property, value)` — the opinion-by-opinion view two
    /// compositions are compared over.
    pub fn opinions(&self, subtree: &str) -> Vec<(String, String, sdf::Value)> {
        let Ok(root) = sdf::path(subtree) else {
            return Vec::new();
        };
        let mut paths: Vec<String> = Vec::new();
        let _ = self.stage.traverse(openusd::usd::PrimPredicate::ALL, |p| {
            let s = p.as_str();
            if s == subtree || s.starts_with(&format!("{}/", subtree.trim_end_matches('/'))) {
                paths.push(s.to_owned());
            }
        });
        let _ = root;
        paths.sort();
        paths.dedup();
        let mut out = Vec::new();
        for path in paths {
            for (name, value) in self.authored_properties(&path) {
                out.push((path.clone(), name, value));
            }
        }
        out
    }
}

fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Standard base64 with padding (`A–Z a–z 0–9 + /`). Small enough that pulling
/// in a crate for the spike's one use is not worth the dependency.
pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 0x3f] as char);
        out.push(TABLE[(n >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// Decode standard or URL-safe base64, with or without padding. `None` on any
/// byte outside the alphabet, or on a length that cannot be a whole encoding.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let mut nibbles: Vec<u32> = Vec::with_capacity(text.len());
    let mut padding = 0usize;
    for &c in text.as_bytes() {
        match c {
            b'=' => padding += 1,
            b'\n' | b'\r' | b' ' | b'\t' => {}
            other => {
                if padding > 0 {
                    return None; // data after padding
                }
                nibbles.push(val(other)?);
            }
        }
    }
    if nibbles.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(nibbles.len() * 3 / 4);
    for chunk in nibbles.chunks(4) {
        let mut n = 0u32;
        for (i, &v) in chunk.iter().enumerate() {
            n |= v << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}
