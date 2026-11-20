//! The `asset:prim` spelling and the layer it names (v1.5.10).
//!
//! One spelling, three readers: a prim table's `references = "asset:prim"`, a
//! generator's `prototype = "asset:prim"` (v1.5.8), and the diagnostics that say
//! which path was looked for. The split and the resolution live here once so the
//! three agree; the prototype parameter used to carry its own copy of these rules.
//!
//! A prim-level `references` is a **composition arc, not a property**: the prim it
//! is authored on gains no attribute, nothing about it reaches the geometry
//! extractor, and the prim keeps its own place in this stage. What it does is open
//! another layer and compose the prim it names underneath this prim's spec, so the
//! type and every value come from there — a prim with `references` and no `schema`
//! composes its type from the material instead of declaring one.
//!
//! ## The split
//!
//! At a `:` whose right-hand side starts with `/` and reads as a prim path, taken
//! from the right: a Windows drive letter (`C:/art/box.usda`) and a package path
//! (`pkg.usdz[inner]`) both contain colons that are not the separator. No such
//! colon means the value is a **prim of this stage** (an internal reference), which
//! is what a prototype's same-stack spelling has always meant.
//!
//! ## The resolution
//!
//! A relative asset path resolves against `doc_dir`, the directory of the document
//! that named it: the root layer this adapter authors is anonymous and anchors
//! nothing. `doc_dir` is `None` for a document opened from its text
//! (`open{content}`), which has no directory to be relative to — the answer is then
//! a diagnostic naming the value, never a guess against the process's cwd. The
//! layer must exist: a reference to a missing layer would compose into an unresolved
//! arc, and an agent is better served by the path that was tried.

use openusd::sdf;

/// An `asset:prim` value, split into its two halves.
#[derive(Clone, Debug)]
pub struct AssetRef {
    /// The external layer, absolute and `/`-spelled; `None` for a prim of this
    /// stage (an internal reference).
    pub asset: Option<String>,
    /// The asset half **as the document wrote it** (v1.9): `../box.rusd`, or an absolute
    /// path when that is what was authored. A layer written back to disk keeps this
    /// spelling, so the file resolves from wherever it is read — the same record copied to
    /// another machine still opens, because a relative arc means "beside this file".
    pub authored: Option<String>,
    /// The prim inside that layer.
    pub path: sdf::Path,
}

/// Why a value did not name a prim.
#[derive(Clone, Debug)]
pub enum AssetRefError {
    /// The prim half is not a prim path.
    Path {
        /// The value as authored.
        value: String,
        /// What the path parser made of the prim half.
        why: String,
    },
    /// The asset half did not resolve to a layer that exists.
    Asset {
        /// The asset half as authored.
        authored: String,
        /// Why it did not resolve; for a missing layer this carries the full path
        /// that was looked for.
        why: String,
    },
}

impl AssetRefError {
    /// The prim-path failure, for a caller that wants its own wording.
    pub fn path(value: impl Into<String>, why: impl Into<String>) -> Self {
        Self::Path {
            value: value.into(),
            why: why.into(),
        }
    }
}

/// Split and resolve one `asset:prim` value. `doc_dir` is the directory of the
/// document that named it, `None` when the document was opened from its text.
pub fn parse(value: &str, doc_dir: Option<&str>) -> Result<AssetRef, AssetRefError> {
    let split = value.rfind(':').filter(|i| {
        let rest = &value[i + 1..];
        rest.starts_with('/') && sdf::path(rest).is_ok() && !value[..*i].is_empty()
    });
    let Some(split) = split else {
        let path = sdf::path(value).map_err(|e| AssetRefError::path(value, e.to_string()))?;
        return Ok(AssetRef {
            asset: None,
            authored: None,
            path,
        });
    };
    let authored = &value[..split];
    let prim = &value[split + 1..];
    let path = sdf::path(prim).map_err(|e| AssetRefError::path(value, e.to_string()))?;
    let resolved = resolve_layer(authored, doc_dir)?;
    Ok(AssetRef {
        asset: Some(resolved),
        authored: Some(authored.to_owned()),
        path,
    })
}

/// The layer an asset half names: absolute, `/`-spelled, and it must be there.
///
/// Absolute because a layer identifier has to mean the same thing from any layer
/// that reads it — the root layer this adapter authors is anonymous and anchors
/// nothing. `/`-spelled on every host because the resolver reads `\` as a
/// separator too, but the identifier it mints is the spelling the reference
/// authored, and one spelling keeps the tree and the arc agreeing.
fn resolve_layer(authored: &str, doc_dir: Option<&str>) -> Result<String, AssetRefError> {
    let as_path = std::path::Path::new(authored);
    let full = if as_path.is_absolute() {
        as_path.to_path_buf()
    } else {
        let Some(dir) = doc_dir else {
            return Err(AssetRefError::Asset {
                authored: authored.to_owned(),
                why: "the document was opened from its text, so it has no directory to \
                      resolve a relative layer against; open it by `path`, or spell the \
                      layer as an absolute path"
                    .to_owned(),
            });
        };
        std::path::Path::new(dir).join(as_path)
    };
    let full = std::path::absolute(&full).map_err(|e| AssetRefError::Asset {
        authored: authored.to_owned(),
        why: format!("cannot resolve: {e}"),
    })?;
    if !full.is_file() {
        return Err(AssetRefError::Asset {
            authored: authored.to_owned(),
            why: format!("no such layer: {}", full.display()),
        });
    }
    Ok(full.to_string_lossy().replace('\\', "/"))
}
