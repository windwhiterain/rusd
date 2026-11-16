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

#[cfg(test)]
mod tests {
    use super::*;

    const FOREST: &str = include_str!("../../../examples/forest.rusd");

    #[test]
    fn fmt_idempotent() {
        let once = canonicalize(FOREST).unwrap();
        let twice = canonicalize(&once).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn fmt_roundtrip_preserves_semantics() {
        let before = crate::syntax::parse(FOREST).unwrap();
        let formatted = canonicalize(FOREST).unwrap();
        let after = crate::syntax::parse(&formatted).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn fmt_sorts_tables_and_keys_and_normalizes_numbers() {
        let input = "[World.B]\nschema = \"Xform\"\nz = 1_000\na = 0x10\n\
                     [World.A]\nschema = \"Xform\"\n[stage]\nseed = 1\n";
        let out = canonicalize(input).unwrap();
        let stage = out.find("[stage]").unwrap();
        let a = out.find("[World.A]").unwrap();
        let b = out.find("[World.B]").unwrap();
        assert!(stage < a && a < b, "stage first, then path order:\n{out}");
        assert!(out.contains("a = 16"), "hex normalized:\n{out}");
        assert!(out.contains("z = 1000"), "underscores stripped:\n{out}");
        let bsec = &out[b..];
        assert!(
            bsec.find("a = ").unwrap() < bsec.find("z = ").unwrap(),
            "keys sorted:\n{out}"
        );
    }

    #[test]
    fn fmt_float_canonical() {
        let input = "[World.S]\nschema = \"Sphere\"\nradius = 1.50\n";
        let out = canonicalize(input).unwrap();
        assert!(out.contains("radius = 1.5"), "float normalized:\n{out}");
        let input2 = "[World.S]\nschema = \"Sphere\"\nradius = 2.0\n";
        let out2 = canonicalize(input2).unwrap();
        assert!(
            out2.contains("radius = 2.0"),
            "integral float keeps .0:\n{out2}"
        );
    }

    #[test]
    fn fmt_preserves_comment() {
        let input = "[stage]\nseed = 1 # keep me\n";
        let out = canonicalize(input).unwrap();
        assert!(out.contains("# keep me"), "comment preserved:\n{out}");
    }

    #[test]
    fn fmt_inline_table_sorted() {
        let input = "[World.F]\nschema = \"RusdScatter\"\nscale = { min = 0.8, dist = \"uniform\", max = 1.4 }\n";
        let out = canonicalize(input).unwrap();
        let d = out.find("dist = ").unwrap();
        let mx = out.find("max = ").unwrap();
        let mn = out.find("min = ").unwrap();
        assert!(d < mx && mx < mn, "inline table keys sorted:\n{out}");
    }

    #[test]
    fn parse_explicit_missing_schema_is_error() {
        let diags = crate::syntax::parse("[World]\n").unwrap_err();
        assert!(diags.iter().any(|d| d.code == "E_MISSING_SCHEMA"));
    }

    #[test]
    fn parse_implicit_parent_no_schema_error() {
        let doc =
            crate::syntax::parse("[World.Child]\nschema = \"Sphere\"\nradius = 1.0\n").unwrap();
        let world = doc
            .prims
            .iter()
            .find(|p| p.path.as_str() == "/World")
            .unwrap();
        assert_eq!(world.schema, "Xform");
    }
}
