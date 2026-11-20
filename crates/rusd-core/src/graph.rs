//! The **operator graph** document (v1.9): the thing an author writes.
//!
//! A `.ops.toml` is the document. Its vocabulary is the tool's own — `operator`,
//! `outputs`, `inputs`, and the operator's parameters — never USD's: no `schema`, no
//! `reference`, no `typeName`. It says **what runs and who reads whom**; the USD layers
//! it names (`material` entries, `asset:prim` values) are its leaves, and the stages it
//! produces are written as `.rusd` layers.
//!
//! ```toml
//! [stage]
//! default_prim = "/World"
//!
//! [material.volume]
//! prim  = "/World/Volume"
//! asset = "volume.rusd:/World/Volume"
//!
//! [operator.Samples]
//! operator    = "sample_points"
//! description = "在 Volume 盒内均匀采 60 个点"
//! outputs     = ["/World/Samples", "/World/Samples/points"]
//! count = 60
//! seed  = 11
//!
//! [operator.Samples.inputs]
//! domain = "/World/Volume"
//! ```
//!
//! This module turns that into the same [`ParsedDocument`] the rest of the engine already
//! works with — one lowerer, one cook, one composition — so the graph is a **spelling of
//! the document**, not a second engine.

use std::collections::BTreeMap;

use toml_edit::{DocumentMut, Item};

use crate::{Diagnostic, Value, syntax::{ParsedDocument, PrimBlock}};

/// What a graph operator is called in the schema registry.
fn schema_for(operator: &str) -> Option<&'static str> {
    match operator {
        "sample_points" => Some("RusdSamplePoints"),
        "instances" => Some("RusdInstances"),
        "grid" => Some("RusdGrid"),
        "street_grid" => Some("RusdStreetGrid"),
        "building" => Some("RusdBuilding"),
        "scatter" => Some("RusdScatter"),
        _ => None,
    }
}

/// How an operator's `inputs` reach the generator that runs it, by slot.
fn slot_parameter(operator: &str, slot: &str) -> Option<&'static str> {
    match (operator, slot) {
        ("sample_points", "domain") => Some("input_points"),
        ("instances", "points") => Some("input_points"),
        ("instances", "prototype") => Some("prototype"),
        ("scatter", "points") => Some("input_points"),
        ("scatter", "prototype") => Some("prototype"),
        _ => None,
    }
}

/// Whether `text` is an operator graph rather than a `.rusd` document: it has at least one
/// `[operator.<name>]` table and no `[stage]`-less prim named `operator`.
pub fn looks_like_graph(text: &str) -> bool {
    let Ok(doc) = text.parse::<DocumentMut>() else {
        return false;
    };
    doc.as_table().contains_key("operator")
}

/// Parse an operator graph into the document the engine lowers.
pub fn parse(text: &str, doc_dir: Option<&str>) -> Result<ParsedDocument, Vec<Diagnostic>> {
    let doc = text
        .parse::<DocumentMut>()
        .map_err(|e| vec![Diagnostic::error("E_PARSE", e.to_string())])?;
    let mut diags = Vec::new();
    let mut stage: BTreeMap<String, Value> = BTreeMap::new();
    let mut prims: Vec<PrimBlock> = Vec::new();

    if let Some(Item::Table(t)) = doc.get("stage") {
        for (key, item) in t.iter() {
            if let Some(v) = item.as_value().and_then(value_of) {
                stage.insert(key.to_owned(), v);
            }
        }
    }

    // **A material is an input, not a prim** (v1.9): the graph names the USD an operator
    // reads (`domain = "volume.rusd:/World/Volume"`), and the engine opens that layer to
    // read it. Nothing from it enters the document's stage, so nothing of it is drawn.
    let _ = &mut prims;

    let Some(Item::Table(operators)) = doc.get("operator") else {
        diags.push(Diagnostic::error("E_GRAPH", "the document has no [operator.*]"));
        return Err(diags);
    };
    for (name, item) in operators.iter() {
        let Item::Table(t) = item else { continue };
        let Some(kind) = t.get("operator").and_then(|v| v.as_str()) else {
            diags.push(Diagnostic::error(
                "E_GRAPH",
                format!("operator `{name}` has no `operator`"),
            ));
            continue;
        };
        let Some(schema) = schema_for(kind) else {
            diags.push(Diagnostic::error(
                "E_GRAPH",
                format!("unknown operator `{kind}` (in `{name}`)"),
            ));
            continue;
        };
        let Some(output) = t
            .get("outputs")
            .and_then(|v| v.as_array())
            .and_then(|a| a.get(0))
            .and_then(|v| v.as_str())
        else {
            diags.push(Diagnostic::error(
                "E_GRAPH",
                format!("operator `{name}` has no `outputs`"),
            ));
            continue;
        };
        let Ok(path) = crate::PrimPath::parse(output) else {
            diags.push(Diagnostic::error("E_NAME", format!("not a prim path: {output}")));
            continue;
        };
        let mut props: BTreeMap<String, Value> = BTreeMap::new();
        // The operator's own parameters: every plain value that is not one of the graph's
        // own words. `operator`, `outputs`, `inputs` and `description` are the graph's.
        for (key, item) in t.iter() {
            match key {
                "operator" | "outputs" => {}
                "inputs" => {
                    let Some(inputs) = item.as_table() else { continue };
                    for (slot, value) in inputs.iter() {
                        let Some(parameter) = slot_parameter(kind, slot) else {
                            diags.push(Diagnostic::warning(
                                "W_GRAPH_SLOT",
                                format!("operator `{name}` ignores input `{slot}`"),
                            ));
                            continue;
                        };
                        if let Some(v) = value.as_value().and_then(value_of) {
                            props.insert(parameter.to_owned(), v);
                        }
                    }
                }
                _ => {
                    if let Some(v) = item.as_value().and_then(value_of) {
                        props.insert(key.to_owned(), v);
                    }
                }
            }
        }
        prims.push(PrimBlock {
            path,
            schema: schema.to_owned(),
            props,
            references: None,
        });
    }

    if !diags.iter().any(|d| d.severity == crate::Severity::Error) {
        return Ok(ParsedDocument {
            stage,
            prims,
            doc_dir: doc_dir.map(str::to_owned),
            annotations: Vec::new(),
            graph: true,
        });
    }
    Err(diags)
}

/// A TOML value as the document's own value.
fn value_of(v: &toml_edit::Value) -> Option<Value> {
    if let Some(s) = v.as_str() {
        return Some(Value::String(s.to_owned()));
    }
    if let Some(b) = v.as_bool() {
        return Some(Value::Bool(b));
    }
    if let Some(i) = v.as_integer() {
        return Some(Value::Int(i));
    }
    if let Some(f) = v.as_float() {
        return Some(Value::Float(f));
    }
    if let Some(a) = v.as_array() {
        let items: Option<Vec<Value>> = a.iter().map(value_of).collect();
        return items.map(Value::Array);
    }
    if let Some(t) = v.as_inline_table() {
        let mut map = BTreeMap::new();
        for (k, v) in t.iter() {
            map.insert(k.to_owned(), value_of(v)?);
        }
        return Some(Value::Dict(map));
    }
    None
}
