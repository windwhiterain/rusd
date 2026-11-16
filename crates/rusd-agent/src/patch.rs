//! Document-level patch (decision D11): apply opinion operations to the `.rusd`
//! TOML source via the `toml_edit` CST, preserving comments/whitespace of untouched
//! blocks. The document stays the source of truth, so generators added here are
//! seen by the cook engine (`nodes_from_doc`).
//!
//! Ops (JSON):
//! - `{"op":"set","prim":"/World/Forest","property":"count","value":500}`
//! - `{"op":"define","prim":"/World/Forest","schema":"RusdScatter","props":{...}}`
//! - `{"op":"remove","prim":"/World/Forest"}` (whole prim)
//! - `{"op":"remove","prim":"/World/Forest","property":"count"}` (one property)

use serde_json::Value;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value as TomlValue};

/// Apply `ops` to `text`, returning the new TOML source.
pub fn apply(text: &str, ops: &[Value]) -> Result<String, String> {
    let mut doc = text
        .parse::<DocumentMut>()
        .map_err(|e| format!("toml parse: {e}"))?;
    for op in ops {
        apply_op(&mut doc, op)?;
    }
    Ok(doc.to_string())
}

fn apply_op(doc: &mut DocumentMut, op: &Value) -> Result<(), String> {
    let kind = op
        .get("op")
        .and_then(Value::as_str)
        .ok_or("op: missing `op`")?;
    let prim = op
        .get("prim")
        .and_then(Value::as_str)
        .ok_or("op: missing `prim`")?;
    match kind {
        "set" => {
            let prop = op
                .get("property")
                .and_then(Value::as_str)
                .ok_or("set: missing `property`")?;
            let value = op.get("value").ok_or("set: missing `value`")?;
            let t = prim_table(doc, prim)?;
            t.insert(prop, Item::Value(json_to_value(value)?));
        }
        "define" => {
            let schema = op
                .get("schema")
                .and_then(Value::as_str)
                .ok_or("define: missing `schema`")?;
            let t = prim_table(doc, prim)?;
            t.insert("schema", Item::Value(TomlValue::from(schema.to_owned())));
            if let Some(props) = op.get("props").and_then(Value::as_object) {
                for (k, v) in props {
                    t.insert(k, Item::Value(json_to_value(v)?));
                }
            }
        }
        "remove" => {
            if let Some(prop) = op.get("property").and_then(Value::as_str) {
                prim_table(doc, prim)?.remove(prop);
            } else {
                remove_prim(doc, prim)?;
            }
        }
        _ => return Err(format!("unknown op: {kind}")),
    }
    Ok(())
}

fn components(path: &str) -> Result<Vec<&str>, String> {
    let comps: Vec<&str> = path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if comps.is_empty() {
        return Err(format!("empty prim path: {path}"));
    }
    Ok(comps)
}

/// Navigate to (creating as needed) the table for a prim path.
fn prim_table<'a>(doc: &'a mut DocumentMut, path: &str) -> Result<&'a mut Table, String> {
    let comps = components(path)?;
    let mut t = doc.as_table_mut();
    for c in comps {
        let item = t.entry(c).or_insert(Item::Table(Table::new()));
        t = item
            .as_table_mut()
            .ok_or_else(|| format!("`{c}` is not a table"))?;
    }
    Ok(t)
}

fn remove_prim(doc: &mut DocumentMut, path: &str) -> Result<(), String> {
    let comps = components(path)?;
    let (last, parents) = comps.split_last().unwrap();
    let mut t = doc.as_table_mut();
    for c in parents {
        t = t
            .get_mut(c)
            .and_then(Item::as_table_mut)
            .ok_or_else(|| format!("`{c}` not found"))?;
    }
    t.remove(last);
    Ok(())
}

fn json_to_value(v: &Value) -> Result<TomlValue, String> {
    match v {
        Value::Bool(b) => Ok(TomlValue::from(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(TomlValue::from(i))
            } else if let Some(f) = n.as_f64() {
                Ok(TomlValue::from(f))
            } else {
                Err("unsupported number".into())
            }
        }
        Value::String(s) => Ok(TomlValue::from(s.clone())),
        Value::Array(a) => {
            let mut arr = Array::new();
            for e in a {
                arr.push(json_to_value(e)?);
            }
            Ok(TomlValue::from(arr))
        }
        Value::Object(o) => {
            let mut t = InlineTable::new();
            for (k, e) in o {
                t.insert(k.clone(), json_to_value(e)?);
            }
            Ok(TomlValue::from(t))
        }
        Value::Null => Err("null is not a rusd value".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BASE: &str = "[stage]\nseed = 1\n\n[World]\nschema = \"Xform\"\n";

    #[test]
    fn define_set_remove_roundtrip() {
        let ops = vec![json!({
            "op": "define", "prim": "/World/Box", "schema": "Cube",
            "props": {"size": 2.0}
        })];
        let out = apply(BASE, &ops).unwrap();
        assert!(out.contains("[World.Box]"));
        assert!(out.contains("schema = \"Cube\""));
        // set a property
        let out = apply(
            &out,
            &[json!({"op":"set","prim":"/World/Box","property":"size","value":4.0})],
        )
        .unwrap();
        assert!(out.contains("size = 4.0"));
        // remove the prim
        let out = apply(&out, &[json!({"op":"remove","prim":"/World/Box"})]).unwrap();
        assert!(!out.contains("[World.Box]"));
        // base preserved
        assert!(out.contains("[World]"));
    }

    #[test]
    fn preserves_comments() {
        let src = "# top comment\n[stage]\nseed = 1\n\n[World]\nschema = \"Xform\" # inline\n";
        let out = apply(
            src,
            &[json!({"op":"set","prim":"/World","property":"kind","value":"assembly"})],
        )
        .unwrap();
        assert!(out.contains("# top comment"));
        assert!(out.contains("# inline"));
    }

    #[test]
    fn remove_property() {
        let src = "[World]\nschema = \"Xform\"\nfoo = 1\n";
        let out = apply(
            src,
            &[json!({"op":"remove","prim":"/World","property":"foo"})],
        )
        .unwrap();
        assert!(!out.contains("foo"));
        assert!(out.contains("schema"));
    }
}
