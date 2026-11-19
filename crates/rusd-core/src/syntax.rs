//! Structural parser for the `.rusd` TOML profile (SPEC §2).
//!
//! M1 spike: parse a document into a stage table plus a flat, path-sorted list
//! of prim blocks. It uses `toml_edit`'s CST so a later `fmt`/`patch` pass can
//! preserve comments and whitespace. Semantic typing (string → path/token/asset
//! coercion, schema validation) lands in M2; here every scalar keeps its TOML
//! type and `schema` is read as a plain string.

use std::collections::BTreeMap;

use toml_edit::{DocumentMut, Item, Table, Value as TomlValue};

use crate::diag::Diagnostic;
use crate::path::PrimPath;
use crate::value::Value;

/// One prim block: its path, declared schema, raw properties, and the layer its
/// `references` key names.
#[derive(Clone, Debug, PartialEq)]
pub struct PrimBlock {
    pub path: PrimPath,
    /// The schema the table declares. **Empty** means it declares none: a prim
    /// whose type is composed from the layer its `references` names (v1.5.10)
    /// must not author a `typeName` of its own. A local type is the strongest
    /// opinion there is and would win over the arc, turning the prim into an
    /// empty `Xform` with the material's geometry lost.
    pub schema: String,
    pub props: BTreeMap<String, Value>,
    /// `references = "asset:prim"` (v1.5.10): a **composition arc, not a
    /// property**. It names another layer and a prim inside it, whose spec
    /// composes underneath this prim's own; nothing about it is written as an
    /// attribute, so it never reaches the geometry layer. One string, split and
    /// resolved by `rusd_scene::reference` (asset half first, then prim half).
    pub references: Option<String>,
}

/// A **layer annotation**: a document node that says something about a layer
/// rather than about geometry.
///
/// The v1.5.7 surface spells it as a table with **no `schema`** and a `layer`
/// key, which is what tells it apart from a prim block:
///
/// ```toml
/// [doc.prototypes]
/// layer = "prototypes.usda"
/// description = "the tuft prototype asset"
/// ```
///
/// It is metadata: it authors no prim, contributes no geometry, and never enters
/// the composed stage. `layer` names the layer it is about — a file name as the
/// composition tree reports it (`prototypes.usda`, or its tail of a longer
/// identifier), or the words `root` / `session` for the two layers a stage
/// always has. `description` is the text the tree carries for that layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Annotation {
    /// The document path the annotation is spelled at (`doc.prototypes`), which is
    /// what a diagnostic names and what tells two annotations apart.
    pub node: String,
    /// The layer this note is about: a file name, `root`, or `session`.
    pub layer: String,
    /// The text `composition_tree` and `layer{id}` carry for that layer.
    pub description: String,
}

/// A parsed `.rusd` document: the `[stage]` table, all prim blocks, and the
/// layer annotations.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedDocument {
    pub stage: BTreeMap<String, Value>,
    pub prims: Vec<PrimBlock>,
    /// The directory the document was read from (v1.5.10), set by [`parse_in`]
    /// and otherwise `None`. It is what a **relative** reference or prototype
    /// asset resolves against: the root layer the adapter authors is anonymous and
    /// anchors nothing. A document handed in as text (`open{content}`) has no
    /// directory, so a relative layer is a diagnostic rather than a guess at the
    /// process's cwd.
    pub doc_dir: Option<String>,
    /// Layer annotations (`[doc.<name>]` with `layer` + `description`), sorted by
    /// node path so the surface stays deterministic. Two may name one layer; that
    /// is `E_ANNOTATION` at validation, not something this parser resolves.
    pub annotations: Vec<Annotation>,
}

/// Parse a `.rusd` document. Returns all error diagnostics on failure.
pub fn parse(text: &str) -> Result<ParsedDocument, Vec<Diagnostic>> {
    let doc = match text.parse::<DocumentMut>() {
        Ok(d) => d,
        Err(e) => {
            return Err(vec![Diagnostic::error("E_PARSE", e.to_string())]);
        }
    };

    let mut diags = Vec::new();
    let mut stage = BTreeMap::new();
    let mut prims = Vec::new();
    let mut annotations = Vec::new();

    for (key, item) in doc.as_table().iter() {
        if key == "stage" {
            if let Some(t) = item.as_table() {
                collect_props(t, "", &mut stage, &mut diags, "/stage");
            } else {
                diags.push(
                    Diagnostic::error("E_STAGE", "[stage] must be a table").at_prim("/stage"),
                );
            }
            continue;
        }
        // An annotation container is a table whose content is **only** annotations:
        // a leaf (`layer` and no `schema`) or a namespace of them (
        // `[doc.prototypes]` makes an implicit `doc` holding `prototypes`). It
        // describes layers instead of defining geometry, so none of it reaches the
        // stage and none of its ancestors become prims.
        if let Item::Table(t) = item
            && !t.is_dotted()
            && is_annotation_container(t)
        {
            collect_annotations(t, &format!("/{key}"), &mut annotations, &mut diags);
            continue;
        }
        match item {
            Item::Table(t) if !t.is_dotted() => {
                let path = PrimPath::root().join(key);
                match path {
                    Ok(p) => walk_prim(t, p, &mut prims, &mut diags),
                    Err(e) => diags.push(
                        Diagnostic::error("E_NAME", e.to_string()).at_prim(format!("/{key}")),
                    ),
                }
            }
            Item::ArrayOfTables(_) => {
                diags.push(
                    Diagnostic::error(
                        "E_AOT",
                        "array of tables is not allowed; multiplicity belongs to generators",
                    )
                    .at_prim(format!("/{key}")),
                );
            }
            _ => {
                diags.push(
                    Diagnostic::error(
                        "E_TOPLEVEL",
                        "bare top-level keys are not allowed; only [stage] and prim tables",
                    )
                    .at_prim(format!("/{key}")),
                );
            }
        }
    }

    if !diags.is_empty() {
        return Err(diags);
    }
    prims.sort_by(|a, b| a.path.cmp(&b.path));
    annotations.sort_by(|a, b| a.node.cmp(&b.node));
    Ok(ParsedDocument {
        stage,
        prims,
        doc_dir: None,
        annotations,
    })
}

/// Parse a document that came from a file, remembering the directory it lives
/// in: that is what a relative `references`/`prototype` asset resolves against
/// (v1.5.10). The directory is not touched, only joined against after the fact.
pub fn parse_in(text: &str, doc_dir: Option<&str>) -> Result<ParsedDocument, Vec<Diagnostic>> {
    let mut doc = parse(text)?;
    doc.doc_dir = doc_dir.map(str::to_owned);
    Ok(doc)
}

/// An annotation leaf: a `layer` and no `schema`. The `schema` test is what tells
/// an annotation apart from a prim that happens to author a `layer` property.
fn annotation_leaf(t: &Table) -> bool {
    t.get("schema").is_none() && t.get("layer").is_some()
}

/// Whether `t` holds nothing but annotations: a leaf, or a namespace of them.
fn is_annotation_container(t: &Table) -> bool {
    if annotation_leaf(t) {
        return true;
    }
    let mut any = false;
    for (_, item) in t.iter() {
        match item {
            Item::Table(child) if !child.is_dotted() && is_annotation_container(child) => {
                any = true;
            }
            _ => return false,
        }
    }
    any
}

/// Collect every annotation below an annotation container, labelling each with
/// the path the document spells it at (`/doc/prototypes`), which is what a
/// diagnostic names.
fn collect_annotations(
    table: &Table,
    prim: &str,
    out: &mut Vec<Annotation>,
    diags: &mut Vec<Diagnostic>,
) {
    if annotation_leaf(table) {
        collect_annotation(table, prim.to_owned(), out, diags);
        return;
    }
    for (key, item) in table.iter() {
        if let Item::Table(child) = item {
            collect_annotations(child, &format!("{prim}/{key}"), out, diags);
        }
    }
}

/// Read one layer annotation: `layer` (required, a string) and `description`
/// (optional, a string). Any other key is refused rather than silently ignored —
/// an annotation is metadata, and a typo in it would otherwise never show.
fn collect_annotation(
    table: &Table,
    prim: String,
    out: &mut Vec<Annotation>,
    diags: &mut Vec<Diagnostic>,
) {
    let mut layer: Option<String> = None;
    let mut description: Option<String> = None;
    for (key, item) in table.iter() {
        match (key, item) {
            ("layer", Item::Value(v)) => match v.as_str() {
                Some(s) => layer = Some(s.to_owned()),
                None => diags.push(
                    Diagnostic::error("E_ANNOTATION", "layer must be a string")
                        .at_prim(&prim)
                        .at_property("layer"),
                ),
            },
            ("description", Item::Value(v)) => match v.as_str() {
                Some(s) => description = Some(s.to_owned()),
                None => diags.push(
                    Diagnostic::error("E_ANNOTATION", "description must be a string")
                        .at_prim(&prim)
                        .at_property("description"),
                ),
            },
            ("layer" | "description", _) => diags.push(
                Diagnostic::error("E_ANNOTATION", format!("{key} must be a string"))
                    .at_prim(&prim)
                    .at_property(key),
            ),
            (other, _) => diags.push(
                Diagnostic::error(
                    "E_ANNOTATION",
                    format!("unexpected key '{other}' in a layer annotation"),
                )
                .at_prim(&prim)
                .at_property(other),
            ),
        }
    }
    let Some(layer) = layer else {
        diags.push(
            Diagnostic::error("E_ANNOTATION", "layer annotation needs `layer`").at_prim(&prim),
        );
        return;
    };
    out.push(Annotation {
        node: prim.trim_start_matches('/').replace('/', "."),
        layer,
        description: description.unwrap_or_default(),
    });
}

/// Recursively walk a prim table: `schema` is special, dotted keys become
/// namespaced properties, non-dotted sub-tables are child prims.
fn walk_prim(
    table: &Table,
    path: PrimPath,
    prims: &mut Vec<PrimBlock>,
    diags: &mut Vec<Diagnostic>,
) {
    let mut props = BTreeMap::new();
    let mut schema: Option<String> = None;
    let mut references: Option<String> = None;

    for (key, item) in table.iter() {
        match item {
            Item::Value(v) if key == "schema" => match v.as_str() {
                Some(s) => schema = Some(s.to_owned()),
                None => diags.push(
                    Diagnostic::error("E_SCHEMA", "schema must be a string")
                        .at_prim(path.as_str())
                        .at_property("schema"),
                ),
            },
            Item::Value(v) if key == "references" => match v.as_str() {
                Some(s) => references = Some(s.to_owned()),
                None => diags.push(
                    Diagnostic::error(
                        "E_REFERENCE",
                        "references must be one string: \"asset:prim\"",
                    )
                    .at_prim(path.as_str())
                    .at_property("references"),
                ),
            },
            Item::Value(v) => match convert(v) {
                Ok(val) => {
                    props.insert(key.to_owned(), val);
                }
                Err(d) => diags.push(d.at_prim(path.as_str()).at_property(key)),
            },
            Item::Table(t) if t.is_dotted() => {
                collect_props(t, key, &mut props, diags, path.as_str());
            }
            Item::Table(t) => {
                let child = path.join(key);
                match child {
                    Ok(p) => walk_prim(t, p, prims, diags),
                    Err(e) => diags.push(
                        Diagnostic::error("E_NAME", e.to_string())
                            .at_prim(format!("{}/{key}", path.as_str())),
                    ),
                }
            }
            Item::ArrayOfTables(_) => diags.push(
                Diagnostic::error(
                    "E_AOT",
                    "array of tables is not allowed; multiplicity belongs to generators",
                )
                .at_prim(format!("{}/{key}", path.as_str())),
            ),
            Item::None => {}
        }
    }

    let schema = match schema {
        Some(s) => s,
        None if references.is_some() => {
            // v1.5.10: a prim that names a `references` arc declares no schema of
            // its own. That is not an omission — a local `typeName` would outrank
            // the arc and empty the prim — it is the point of the key: the type is
            // composed from the material layer.
            String::new()
        }
        None if table.is_implicit() => "Xform".to_owned(),
        None => {
            diags.push(
                Diagnostic::error(
                    "E_MISSING_SCHEMA",
                    "explicit prim table must declare schema",
                )
                .at_prim(path.as_str()),
            );
            "Xform".to_owned()
        }
    };
    prims.push(PrimBlock {
        path,
        schema,
        props,
        references,
    });
}

/// Flatten a dotted-key table into namespaced property names (`budget.max_prims`).
fn collect_props(
    table: &Table,
    prefix: &str,
    out: &mut BTreeMap<String, Value>,
    diags: &mut Vec<Diagnostic>,
    prim: &str,
) {
    for (key, item) in table.iter() {
        let name = if prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{prefix}.{key}")
        };
        match item {
            Item::Value(v) => match convert(v) {
                Ok(val) => {
                    out.insert(name, val);
                }
                Err(d) => diags.push(d.at_prim(prim).at_property(&name)),
            },
            Item::Table(t) if t.is_dotted() => collect_props(t, &name, out, diags, prim),
            _ => diags.push(
                Diagnostic::error("E_PROP", "unsupported property shape")
                    .at_prim(prim)
                    .at_property(&name),
            ),
        }
    }
}

/// Convert a TOML value to a Rusd value, keeping the TOML type (coercion is M2).
#[allow(clippy::result_large_err)]
fn convert(v: &TomlValue) -> Result<Value, Diagnostic> {
    if let Some(s) = v.as_str() {
        return Ok(Value::String(s.to_owned()));
    }
    if let Some(i) = v.as_integer() {
        return Ok(Value::Int(i));
    }
    if let Some(f) = v.as_float() {
        return Ok(Value::Float(f));
    }
    if let Some(b) = v.as_bool() {
        return Ok(Value::Bool(b));
    }
    if let Some(arr) = v.as_array() {
        let mut items = Vec::new();
        for el in arr.iter() {
            items.push(convert(el)?);
        }
        return Ok(Value::Array(items));
    }
    if let Some(it) = v.as_inline_table() {
        let mut map = BTreeMap::new();
        for (k, v) in it.iter() {
            map.insert(k.to_owned(), convert(v)?);
        }
        return Ok(Value::Dict(map));
    }
    if v.as_datetime().is_some() {
        return Err(Diagnostic::error(
            "E_DATETIME",
            "datetime literals are not part of the Rusd profile",
        ));
    }
    Err(Diagnostic::error("E_VALUE", "unsupported TOML value"))
}
