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

/// What a graph operator is called in the cook's registry.
///
/// The document's word (`operator = "instances"`) and this name are the same
/// vocabulary seen from two ends: the graph's, and the generator that runs it. Neither
/// is a **schema** — a schema is what a USD layer's `typeName` names, and a graph node
/// is not a prim — so the node carries this in [`PrimBlock::operator`](crate::syntax::PrimBlock)
/// and leaves `schema` empty.
fn operator_for(kind: &str) -> Option<&'static str> {
    match kind {
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
        let Some(operator) = operator_for(kind) else {
            diags.push(Diagnostic::error(
                "E_GRAPH",
                format!("unknown operator `{kind}` (in `{name}`)"),
            ));
            continue;
        };
        let Some(output) = t.get("output").and_then(|v| v.as_str()) else {
            diags.push(Diagnostic::error(
                "E_GRAPH",
                format!("operator `{name}` has no `output`"),
            ));
            continue;
        };
        let Ok(path) = crate::PrimPath::parse(match output.rfind(':') {
            Some(i) if output[i + 1..].starts_with('/') => &output[i + 1..],
            _ => output,
        }) else {
            diags.push(Diagnostic::error("E_NAME", format!("not a prim path: {output}")));
            continue;
        };
        // **The output may name its file** (`samples.rusd:/World/Samples`): one USD writen
        // per operator, named by the document instead of by a throwaway counter.
        let output_file = output
            .rfind(':')
            .filter(|i| output[i + 1..].starts_with('/') && *i > 0)
            .map(|i| output[..i].to_owned());
        let mut props: BTreeMap<String, Value> = BTreeMap::new();
        // **One USD input, one USD output** (v1.13): `input = "<file>.rusd:/World/…"` is the
        // single USD this operator references. It is not a table: a node has one input.
        let mut declared: Vec<(String, String)> = Vec::new();
        for (key, item) in t.iter() {
            match key {
                // The graph's own words — the kind, what it writes, and its one USD input,
                // which is read below.
                "operator" | "output" | "input" => {}
                _ => {
                    if let Some(v) = item.as_value().and_then(value_of) {
                        props.insert(key.to_owned(), v);
                    }
                }
            }
        }
        if let Some(input) = t.get("input").and_then(|v| v.as_str()) {
            // Which engine parameter the one USD fills is the operator's own business:
            // a sampler reads geometry from it, an instances node instances its prototype.
            let parameter = match kind {
                "sample_points" => Some("input_points"),
                "instances" | "scatter" => Some("prototype"),
                _ => None,
            };
            match parameter {
                Some(parameter) => {
                    props.insert(parameter.to_owned(), Value::String(input.to_owned()));
                    // **The declared slot is the operator's own name for its USD input**
                    // (v1.13, the user's ruling: 「USD 的 hierarchy 就是算子的 schema」):
                    // an instances node reads its `prototype` from that USD and a sampler
                    // its `domain`, so those are the names the product authors as child
                    // prims — not the document's own table key, which is the graph's word
                    // for "the one input" and not part of any schema.
                    let slot = match kind {
                        "sample_points" => "domain",
                        "instances" | "scatter" => "prototype",
                        _ => "input",
                    };
                    declared.push((slot.to_owned(), input.to_owned()));
                }
                None => diags.push(Diagnostic::warning(
                    "W_GRAPH_INPUT",
                    format!("operator `{name}` takes no USD input; `input` was ignored"),
                )),
            }
        }
        // **The declared wires** (`[operator.X.inputs]`, D15): the graph's data flow, by
        // the operator's own port names. A Rust script writes these, so the wire it
        // typed is the wire the document declares — instead of one the cook has to
        // reconstruct from node order. `input` above is the one USD material a node
        // references; these are the ports, and `slot_parameter` is which engine
        // parameter each one fills.
        if let Some(Item::Table(wires)) = t.get("inputs") {
            for (slot, item) in wires.iter() {
                let Some(value) = item.as_str() else { continue };
                match slot_parameter(kind, slot) {
                    Some(parameter) => {
                        props.insert(parameter.to_owned(), Value::String(value.to_owned()));
                        if !declared.iter().any(|(s, _)| s == slot) {
                            declared.push((slot.to_owned(), value.to_owned()));
                        }
                    }
                    None => diags.push(Diagnostic::warning(
                        "W_GRAPH_INPUT",
                        format!("operator `{name}` has no port `{slot}`; the wire was ignored"),
                    )),
                }
            }
        }
        prims.push(PrimBlock {
            path,
            // **A graph node is not a prim** (v1.9): nothing of it is authored, so it
            // declares no USD type. The operator it runs is what the cook dispatches on.
            schema: String::new(),
            operator: Some(operator.to_owned()),
            props,
            references: None,
            inputs: declared,
            output_file,
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
