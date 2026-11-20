//! A `.rusd` layer: what a document is, in both directions (v1.9).
//!
//! A `.rusd` file **is** a USD layer — the same prims, types, attribute values, reference
//! arcs and layer metadata `usda` carries — spelled as TOML tables. It is read by the
//! registered format in `openusd::rusd`, which asks this crate for the meaning ([`install`]
//! hands it [`decode`]); nothing compiles, copies or re-spells it on the way in.
//!
//! This module is the *model*: `[World.Boxes]` is a prim spec, `schema = "Xform"` its
//! type, `references = "a.rusd:/World/Box"` its reference arc, and every other key a
//! property whose **type comes from the schema** ([`crate::schema::prop_type`]) rather
//! than from the shape of the value written.
//!
//! - [`layer_to_document`] / [`data_to_document`] render a layer as that text; the encoder
//!   the registered format writes through. It writes each value as the TOML literal the
//!   profile accepts (`[x, y, z]`, `["xformOp:translate"]`, a quoted token) and leaves out
//!   every property the schema type table cannot type — a value the reader would hand back
//!   as a different type is worse than an absent one.
//! - [`layer_from_file`] opens one, through `Layer::open` like any other layer.

use openusd::sdf::{
    self, AbstractData, ChildrenKey, FieldKey, Path, SpecType, Value as SdfValue,
};

use crate::schema;
use rusd_core::syntax;

/// What [`layer_to_document`] produced, and what it had to leave out.
#[derive(Clone, Debug, Default)]
pub struct Rendered {
    /// The `.rusd` text.
    pub text: String,
    /// `<prim>.<property>` for every authored attribute the schema type table has no
    /// type for. The caller turns these into warnings: silently dropping geometry is
    /// exactly the failure this format cannot afford.
    pub skipped: Vec<String>,
}

/// How a reference's asset half is re-spelled when the layer is written (v1.9).
///
/// A generator names the layer it read a prototype from — which, for a prototype in the
/// same stack, is the **throwaway stack's own id**, a layer that dies with the pass. The
/// prim inside it is what decides which file now holds it, so both halves are passed:
/// one stack id names `/World/Boxes` in one file and, in another picture, nothing.
pub type Rewrite<'a> = &'a dyn Fn(&str, &str) -> Option<String>;

/// Render `layer` as `.rusd` text.
pub fn layer_to_document(layer: &sdf::Layer, rewrite: Rewrite<'_>) -> Result<Rendered, String> {
    data_to_document(layer.data(), rewrite)
}

/// Render a layer's **data** as `.rusd` text. The form the registered format's encoder
/// calls, because a writer is handed data and nothing else.
pub fn data_to_document(
    data: &dyn AbstractData,
    rewrite: Rewrite<'_>,
) -> Result<Rendered, String> {
    let mut rendered = Rendered::default();
    let mut out = String::from(
        "# rusd 0.1 — a layer as a document (written by `rusd`, v1.9)\n\
         # Values only: the type of every key here comes from the schema table.\n",
    );
    // Sorted by path, which puts a prim before its children and keeps the file stable
    // across passes: the same layer renders the same bytes.
    for path in data.spec_paths() {
        let spelling = path.as_str();
        if spelling == "/" || data.spec_type(&path) != Some(SpecType::Prim) {
            // The pseudo-root holds layer metadata (`defaultPrim`, `upAxis`), and a
            // product declares none of it; a property spec is written with its prim.
            continue;
        }
        let type_name = tokens(data, &path, FieldKey::TypeName.as_str()).unwrap_or_default();
        out.push_str(&format!("\n[{}]\n", table_name(spelling)));
        // Always authored, `""` included: an explicit prim table with no `schema` key is
        // `E_MISSING_SCHEMA` to the reader, and an empty type name is the document's way
        // of saying "this prim has no type opinion of its own" — its type comes through
        // the arc it references.
        out.push_str(&format!("schema = \"{}\"\n", escape(&type_name)));
        match reference_spelling(data, &path, rewrite) {
            Some(reference) => {
                out.push_str(&format!("references = \"{}\"\n", escape(&reference)));
            }
            None => {
                if authored_reference_count(data, &path) > 1 {
                    // `.rusd` carries one `references` string per prim; a second arc is
                    // a spelling the document profile does not have.
                    rendered.skipped.push(format!("{spelling}.references[1:]"));
                }
            }
        }
        for name in property_names(data, &path) {
            let Ok(prop_path) = path.append_property(&name) else {
                rendered.skipped.push(format!("{spelling}.{name}"));
                continue;
            };
            if data.spec_type(&prop_path) != Some(SpecType::Attribute) {
                // A relationship has no spelling in the document profile either.
                rendered.skipped.push(format!("{spelling}.{name}"));
                continue;
            }
            // A declaration with no `default` authors no value, which is what the
            // document carries: it is a value format, not a schema-authoring one.
            let Some(value) = field(data, &prop_path, FieldKey::Default.as_str()) else {
                continue;
            };
            // Rule 6 of the format: a property the schema table cannot type is not
            // written, because the reader would have to guess its type from the value's
            // shape and would guess a different one. A **string** is the one shape that
            // needs no guess: a named structure under an operator's prim records the USD
            // it points at that way, and no schema table has a word for it — the name is
            // the operator's, and the value says where that structure lives.
            let typeable = schema::prop_type(&type_name, &name).is_some();
            if !typeable && !matches!(value, SdfValue::String(_)) {
                rendered.skipped.push(format!("{spelling}.{name}"));
                continue;
            }
            match literal(&value) {
                Some(text) => out.push_str(&format!("\"{}\" = {text}\n", escape(&name))),
                None => rendered.skipped.push(format!("{spelling}.{name}")),
            }
        }
    }
    rendered.text = out;
    Ok(rendered)
}

/// Read a document back as a layer, through the format that claims `.rusd` (v1.9).
///
/// `Layer::open` is the engine's own path: extension, format, decoder. There is no
/// compile step, no cache and no second spelling — a `.rusd` **is** a layer file, which is
/// what the registered format in `openusd::rusd` makes true.
pub fn layer_from_file(path: &std::path::Path) -> Result<sdf::Layer, String> {
    install();
    sdf::Layer::open(path.to_string_lossy().replace('\\', "/")).map_err(|e| e.to_string())
}

/// A document's text as a layer: written beside the caller and opened like any other
/// `.rusd`. The file is the currency — a layer is a file — so a caller holding only text
/// gets one for the length of the call.
pub fn layer_from_text(
    text: &str,
    dir: Option<&std::path::Path>,
    tag: &str,
) -> Result<sdf::Layer, String> {
    let path = match dir {
        Some(dir) => dir.join(tag),
        None => std::env::temp_dir().join(tag),
    };
    std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    let layer = layer_from_file(&path);
    let _ = std::fs::remove_file(&path);
    layer
}

/// Install the document decoder every `.rusd` read goes through (v1.9), once.
///
/// The format in `openusd::rusd` is the wire; what a document *means* is this crate's, so
/// the format asks the host for a decoder instead of guessing at TOML. Every entry point
/// that can open a layer calls this, and the first call wins.
pub fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        openusd::rusd::set_decoder(decode);
        openusd::rusd::set_encoder(|data| {
            data_to_document(data, &|_, _| None).map(|rendered| rendered.text)
        });
    });
}

/// A document's text as layer data: `parse_in` → [`crate::adapter::author_into`], the same
/// two steps [`crate::adapter::lower`] takes, with no stage around them.
fn decode(text: &str, location: &str) -> Result<sdf::LayerData, String> {
    let dir = std::path::Path::new(location)
        .parent()
        .map(|d| d.to_string_lossy().replace('\\', "/"));
    let doc = syntax::parse_in(text, dir.as_deref())
        .map_err(|diags| first_message(&diags, "parse failed"))?;
    let mut data = sdf::Data::new();
    crate::adapter::author_into(&mut data, &doc)
        .map_err(|diags| first_message(&diags, "lowering failed"))?;
    Ok(Box::new(data))
}

/// The stage builder every Rusd stage goes through: the default registry, with the
/// document decoder installed so a `.rusd` layer opens wherever one is named.
pub fn stage_builder() -> openusd::usd::StageBuilder {
    install();
    openusd::usd::Stage::builder()
}

fn first_message(diags: &[rusd_core::Diagnostic], fallback: &str) -> String {
    diags
        .first()
        .map(|d| d.message.clone())
        .unwrap_or_else(|| fallback.to_owned())
}

// --- reading a layer's data, one spec at a time -----------------------------

fn field(data: &dyn AbstractData, path: &Path, name: &str) -> Option<SdfValue> {
    data.try_field(path, name).ok().flatten().map(|v| v.into_owned())
}

fn tokens(data: &dyn AbstractData, path: &Path, name: &str) -> Option<String> {
    match field(data, path, name)? {
        SdfValue::Token(t) if !t.as_str().is_empty() => Some(t.as_str().to_owned()),
        _ => None,
    }
}

fn property_names(data: &dyn AbstractData, path: &Path) -> Vec<String> {
    match field(data, path, ChildrenKey::PropertyChildren.as_str()) {
        Some(SdfValue::TokenVec(v)) => v.iter().map(|t| t.as_str().to_owned()).collect(),
        _ => Vec::new(),
    }
}

/// The prim's `references` arc as one `asset:prim` string, or `None` when it authors
/// none — or when it authors more, which the document profile cannot spell.
fn reference_spelling(data: &dyn AbstractData, path: &Path, rewrite: Rewrite<'_>) -> Option<String> {
    let items = reference_items(data, path)?;
    let [only] = items.as_slice() else {
        return None;
    };
    let mut asset = only.asset_path.as_str().to_owned();
    let prim = only.prim_path.as_str().to_owned();
    if let Some(file) = rewrite(&asset, &prim) {
        asset = file;
    }
    Some(if asset.is_empty() {
        // An internal reference: a prim of this stage, which the document spells as a
        // bare prim path.
        prim
    } else {
        format!("{asset}:{prim}")
    })
}

fn authored_reference_count(data: &dyn AbstractData, path: &Path) -> usize {
    reference_items(data, path).map(|items| items.len()).unwrap_or(0)
}

/// Every reference a list op names, explicit ones first. A product authors one arc per
/// prim, so this is a defensive flattening rather than a merge of list opinions.
fn reference_items(data: &dyn AbstractData, path: &Path) -> Option<Vec<sdf::Reference>> {
    let op = match field(data, path, FieldKey::References.as_str())? {
        SdfValue::ReferenceListOp(op) => op,
        _ => return None,
    };
    let mut out: Vec<sdf::Reference> = Vec::new();
    if op.explicit {
        out.extend(op.explicit_items.iter().cloned());
    }
    out.extend(op.prepended_items.iter().cloned());
    out.extend(op.appended_items.iter().cloned());
    out.extend(op.added_items.iter().cloned());
    if !op.explicit && out.is_empty() {
        out.extend(op.explicit_items.iter().cloned());
    }
    Some(out)
}

// --- values ----------------------------------------------------------------

/// A `sdf::Value` as the TOML literal the document profile accepts, or `None` when the
/// profile has no spelling for it (a matrix, a quaternion, a time sample).
fn literal(value: &SdfValue) -> Option<String> {
    let text = match value {
        SdfValue::Bool(b) => if *b { "true" } else { "false" }.to_owned(),
        SdfValue::Int(i) => i.to_string(),
        SdfValue::Int64(i) => i.to_string(),
        SdfValue::Uint(u) => u.to_string(),
        SdfValue::Uint64(u) => u.to_string(),
        SdfValue::Float(f) => number(*f as f64),
        SdfValue::Double(d) => number(*d),
        SdfValue::String(s) => quoted(s),
        // A token is a bare word in `.rusd`, so it is written as the string whose
        // **type the schema decides** — the same rule that keeps the reader from
        // guessing: `visibility = "invisible"` is a token because `visibility` is one.
        SdfValue::Token(t) => quoted(t.as_str()),
        SdfValue::AssetPath(a) => quoted(a.as_str()),
        SdfValue::Vec2f(v) => list(&[v.x as f64, v.y as f64]),
        SdfValue::Vec2d(v) => list(&[v.x, v.y]),
        SdfValue::Vec3f(v) => list(&[v.x as f64, v.y as f64, v.z as f64]),
        SdfValue::Vec3d(v) => list(&[v.x, v.y, v.z]),
        SdfValue::Vec2fVec(items) => nested(items.iter().map(|v| [v.x as f64, v.y as f64])),
        SdfValue::Vec2dVec(items) => nested(items.iter().map(|v| [v.x, v.y])),
        SdfValue::Vec3fVec(items) => {
            nested(items.iter().map(|v| [v.x as f64, v.y as f64, v.z as f64]))
        }
        SdfValue::Vec3dVec(items) => nested(items.iter().map(|v| [v.x, v.y, v.z])),
        SdfValue::FloatVec(items) => list(&items.iter().map(|v| *v as f64).collect::<Vec<_>>()),
        SdfValue::DoubleVec(items) => list(items),
        SdfValue::IntVec(items) => flat(items.iter().map(|v| v.to_string())),
        SdfValue::Int64Vec(items) => flat(items.iter().map(|v| v.to_string())),
        SdfValue::UintVec(items) => flat(items.iter().map(|v| v.to_string())),
        SdfValue::Uint64Vec(items) => flat(items.iter().map(|v| v.to_string())),
        SdfValue::StringVec(items) => flat(items.iter().map(|v| quoted(v))),
        SdfValue::TokenVec(items) => flat(items.iter().map(|t| quoted(t.as_str()))),
        SdfValue::AssetPathVec(items) => flat(items.iter().map(|a| quoted(a.as_str()))),
        _ => return None,
    };
    Some(text)
}

fn number(v: f64) -> String {
    if v.is_nan() {
        "nan".to_owned()
    } else if v.is_infinite() {
        if v > 0.0 { "inf" } else { "-inf" }.to_owned()
    } else if v == v.trunc() && v.abs() < 1e15 {
        // An integral float keeps its `.0`, or the reader would see an integer and the
        // schema would be asked to coerce a type nobody wrote (the same rule
        // `rusd_core::Value`'s `Display` follows).
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

fn list(items: &[f64]) -> String {
    flat(items.iter().map(|v| number(*v)))
}

fn nested<I, const N: usize>(items: I) -> String
where
    I: IntoIterator<Item = [f64; N]>,
{
    flat(items.into_iter().map(|v| list(&v)))
}

fn flat<I>(items: I) -> String
where
    I: IntoIterator<Item = String>,
{
    let mut out = String::from("[");
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&item);
    }
    out.push(']');
    out
}

fn quoted(s: &str) -> String {
    format!("\"{}\"", escape(s))
}

/// TOML basic-string escaping, the same set `rusd_core::Value` prints.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// A prim path as a TOML table name: `[World.Boxes.i000000]`, quoting any component
/// that is not a bare TOML key (a USD name never is not, but a path is not one name).
fn table_name(spelling: &str) -> String {
    spelling
        .trim_start_matches('/')
        .split('/')
        .map(key_component)
        .collect::<Vec<_>>()
        .join(".")
}

fn key_component(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        name.to_owned()
    } else {
        quoted(name)
    }
}
