//! Validation (SPEC §3): check a parsed document against the schema registry,
//! producing stable-code JSON diagnostics. Errors (`E_*`) block lowering;
//! warnings (`W_*`) are tolerated. This is the feedback half of the agent loop.

use rusd_core::syntax::ParsedDocument;
use rusd_core::{Diagnostic, PrimPath, Value};

use crate::schema::{self, PropType};

/// Validate every prim. Returns all diagnostics (errors and warnings).
pub fn validate(doc: &ParsedDocument) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for prim in &doc.prims {
        let Some(schema) = schema::lookup(&prim.schema) else {
            diags.push(
                Diagnostic::warning(
                    "W_UNKNOWN_SCHEMA",
                    format!("unknown schema '{}'", prim.schema),
                )
                .at_prim(prim.path.as_str()),
            );
            continue;
        };

        for def in schema.props.iter().filter(|d| d.required) {
            if !prim.props.contains_key(def.name) {
                diags.push(
                    Diagnostic::error(
                        "E_REQUIRED",
                        format!("missing required property '{}'", def.name),
                    )
                    .at_prim(prim.path.as_str())
                    .at_property(def.name),
                );
            }
        }

        for (name, val) in &prim.props {
            match schema.props.iter().find(|d| d.name == name) {
                Some(def) => {
                    if !type_ok(val, def.ty) {
                        diags.push(
                            Diagnostic::error(
                                "E_TYPE",
                                format!("expected {}, got {}", def.ty.name(), val.type_name()),
                            )
                            .at_prim(prim.path.as_str())
                            .at_property(name),
                        );
                    }
                }
                None => diags.push(
                    Diagnostic::warning(
                        "W_UNKNOWN_PROP",
                        format!("property '{name}' not defined by schema '{}'", prim.schema),
                    )
                    .at_prim(prim.path.as_str())
                    .at_property(name),
                ),
            }
        }
    }
    diags
}

/// Whether a value satisfies a property type (with surface coercion rules).
fn type_ok(val: &Value, ty: PropType) -> bool {
    use PropType::*;
    match ty {
        Bool => matches!(val, Value::Bool(_)),
        I64 => matches!(val, Value::Int(_)),
        U64 => matches!(val, Value::Int(i) if *i >= 0),
        F64 => matches!(val, Value::Float(_) | Value::Int(_)),
        String => matches!(val, Value::String(_)),
        Token => matches!(val, Value::String(_) | Value::Token(_)),
        Path => match val {
            Value::Path(_) => true,
            Value::String(s) => PrimPath::parse(s).is_ok(),
            _ => false,
        },
        Asset => matches!(val, Value::String(_) | Value::Asset(_)),
        Vec2 => matches!(val, Value::Vec2(_)) || num_array(val, 2),
        Vec3 => matches!(val, Value::Vec3(_)) || num_array(val, 3),
        Vec4 => matches!(val, Value::Vec4(_)) || num_array(val, 4),
        // A `point3f[]` is an array of `[x,y,z]` arrays on the surface: `points =
        // [[0.0,0.0,0.0], [1.0,0.0,0.0]]`. An array of bare numbers is refused: three
        // numbers are one point (`vec3`), not a cloud, and silently re-shaping a flat
        // list into points would place a scatter where nobody asked.
        Vec3Array => match val {
            Value::Array(items) => {
                !items.is_empty()
                    && items
                        .iter()
                        .all(|v| matches!(v, Value::Vec3(_)) || num_array(v, 3))
            }
            _ => false,
        },
        // `int[]` / `int64[]`: an array of non-negative integers (a mesh's face
        // counts and vertex indices).
        IntArray => {
            matches!(val, Value::Array(items) if items.iter().all(|v| matches!(v, Value::Int(i) if *i >= 0)))
        }
        Color3f => matches!(val, Value::Vec3(_)) || num_array(val, 3),
        Array => matches!(val, Value::Array(_)),
        Dict => matches!(val, Value::Dict(_)),
        Distribution => match val {
            Value::Dict(m) => matches!(m.get("dist"), Some(Value::String(_))),
            _ => false,
        },
        Any => true,
    }
}

fn num_array(val: &Value, len: usize) -> bool {
    match val {
        Value::Array(items) => {
            items.len() == len
                && items
                    .iter()
                    .all(|v| matches!(v, Value::Int(_) | Value::Float(_)))
        }
        _ => false,
    }
}
