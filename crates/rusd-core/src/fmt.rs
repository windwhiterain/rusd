//! Canonical writer for the `.rusd` TOML profile (SPEC §2) — `rusd fmt`.
//!
//! Canonical form: `[stage]` first, then prim tables sorted by path byte order;
//! keys sorted by name within each table; numbers normalized (decimal, no
//! underscores), floats always with a decimal point, strings as basic strings
//! with minimal escaping; arrays/inline tables in canonical layout. Comments are
//! preserved (toml_edit decor). The transform is idempotent and semantics-preserving.

use toml_edit::{DocumentMut, Item, Table, Value};

use crate::diag::Diagnostic;

/// Canonicalize a `.rusd` document. Returns error diagnostics on parse failure.
pub fn canonicalize(text: &str) -> Result<String, Vec<Diagnostic>> {
    let mut doc = text
        .parse::<DocumentMut>()
        .map_err(|e| vec![Diagnostic::error("E_PARSE", e.to_string())])?;
    canonicalize_table(doc.as_table_mut());
    order_tables(&mut doc);
    Ok(doc.to_string())
}

/// Normalize values and sort keys, recursively through all tables.
fn canonicalize_table(t: &mut Table) {
    let value_keys: Vec<String> = t
        .iter()
        .filter(|(_, item)| matches!(item, Item::Value(_)))
        .map(|(k, _)| k.to_owned())
        .collect();
    for (_k, item) in t.iter_mut() {
        match item {
            Item::Value(v) => {
                canonicalize_value(v);
                v.decor_mut().set_prefix(" ");
            }
            Item::Table(sub) => canonicalize_table(sub),
            Item::ArrayOfTables(aot) => {
                for sub in aot.iter_mut() {
                    canonicalize_table(sub);
                }
            }
            Item::None => {}
        }
    }
    // Canonical "key = value" spacing: space before '=' (key leaf suffix) and
    // after '=' (value prefix, set above). Comments in prefix/suffix survive.
    for k in &value_keys {
        if let Some(mut km) = t.key_mut(k) {
            km.leaf_decor_mut().set_suffix(" ");
        }
    }
    // Sorts the syntactic key-value pairs (not sub-tables); recurses dotted keys.
    t.sort_values_by(|k1, _, k2, _| k1.get().cmp(k2.get()));
}

/// Reset a value to its default (canonical) representation, preserving decor.
fn canonicalize_value(v: &mut Value) {
    match v {
        Value::String(f) => f.fmt(),
        Value::Integer(f) => f.fmt(),
        Value::Float(f) => f.fmt(),
        Value::Boolean(f) => f.fmt(),
        Value::Datetime(f) => f.fmt(),
        Value::Array(a) => {
            for el in a.iter_mut() {
                canonicalize_value(el);
            }
            a.fmt();
        }
        Value::InlineTable(t) => {
            for (_k, v) in t.iter_mut() {
                canonicalize_value(v);
            }
            t.sort_values();
            t.fmt();
        }
    }
}

/// Assign table render positions: `[stage]` first, then prim tables by path.
fn order_tables(doc: &mut DocumentMut) {
    let mut paths: Vec<Vec<String>> = Vec::new();
    collect_paths(doc.as_table(), &mut Vec::new(), &mut paths);
    paths.sort_by_key(|a| path_key(a));
    for (i, comps) in paths.iter().enumerate() {
        if let Some(t) = table_at_mut(doc.as_table_mut(), comps) {
            t.set_position(Some(i as isize));
        }
    }
}

/// Sort key: `[stage]` sorts before every prim path.
fn path_key(comps: &[String]) -> String {
    if comps.len() == 1 && comps[0] == "stage" {
        String::new()
    } else {
        format!("/{}", comps.join("/"))
    }
}

/// Collect header-table paths (skip dotted-key tables, which are properties).
fn collect_paths(t: &Table, prefix: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    for (k, item) in t.iter() {
        if let Some(sub) = item.as_table() {
            if sub.is_dotted() {
                continue;
            }
            prefix.push(k.to_owned());
            out.push(prefix.clone());
            collect_paths(sub, prefix, out);
            prefix.pop();
        }
    }
}

fn table_at_mut<'a>(t: &'a mut Table, comps: &[String]) -> Option<&'a mut Table> {
    let mut cur = t;
    for c in comps {
        cur = cur.get_mut(c.as_str())?.as_table_mut()?;
    }
    Some(cur)
}
