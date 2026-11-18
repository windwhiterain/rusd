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

/// One prim block: its path, declared schema, and raw properties.
#[derive(Clone, Debug, PartialEq)]
pub struct PrimBlock {
    pub path: PrimPath,
    pub schema: String,
    pub props: BTreeMap<String, Value>,
}

/// A parsed `.rusd` document: the `[stage]` table and all prim blocks.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedDocument {
    pub stage: BTreeMap<String, Value>,
    pub prims: Vec<PrimBlock>,
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
    Ok(ParsedDocument { stage, prims })
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
