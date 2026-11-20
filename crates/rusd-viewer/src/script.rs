//! The composition tree as **Rust-script pseudocode** — the same layers the tree
//! panel draws, read as the script that wrote them.
//!
//! A generated document is a graph of operators, and D15 writes that graph from a
//! Rust script where every operator is a function and every input a typed value.
//! This module is the reverse reading: each operator layer becomes a `let` binding
//! calling its function, each referenced USD becomes a `material(..)` binding, and a
//! slot wired to another operator's output reads as `Other.port`. It is a
//! *presentation*, not a round trip — the names are derived from the prim paths, not
//! recovered from the original script (which the document does not carry).
//!
//! Every row is still a [`TreeNodeKind::Layer`] carrying the layer's own id, so a
//! click takes the panel's existing path (`ask_for` → solo → `preview`) and renders
//! exactly the USD that variable stands for. Nothing here invents a new action.

use crate::tree::{CompositionTree, DeclaredInput, LayerNode, TreeNode, TreeNodeKind};

/// The last non-empty component of a prim path (`/World/Samples/points` → `points`).
fn leaf(path: &str) -> &str {
    path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path)
}

/// A layer id's file stem (`…/volume.rusd` → `volume`), for a material with no
/// referencing prim to take its name from.
fn file_stem(id: &str) -> String {
    let name = crate::tree::strip_anon_prefix(id);
    match name.rsplit_once('.') {
        Some((stem, _)) => stem.to_string(),
        None => name,
    }
}

/// Whether two asset identifiers name the same layer — equal, or the same file name
/// once any anonymous prefix is stripped (the wire mixes full paths and basenames).
fn same_asset(a: &str, b: &str) -> bool {
    a == b || crate::tree::strip_anon_prefix(a) == crate::tree::strip_anon_prefix(b)
}

/// The script's word for an operator schema. The five D15 operators have one each;
/// an unknown schema keeps its own name rather than guessing.
fn kind_word(op_name: Option<&str>) -> String {
    match op_name {
        Some("RusdSamplePoints") => "sample_points".to_string(),
        Some("RusdInstances") => "instances".to_string(),
        Some("RusdScatter") => "scatter".to_string(),
        Some("RusdGrid") => "grid".to_string(),
        Some("RusdStreetGrid") => "street_grid".to_string(),
        Some(other) => other.to_string(),
        None => "op".to_string(),
    }
}

/// A fresh variable name, suffixed `_2`, `_3`, … on collision.
fn alloc(base: &str, used: &mut Vec<String>) -> String {
    let base = if base.is_empty() { "v" } else { base };
    let mut name = base.to_string();
    if used.contains(&name) {
        let mut n = 2;
        while used.contains(&format!("{name}_{n}")) {
            n += 1;
        }
        name = format!("{name}_{n}");
    }
    used.push(name.clone());
    name
}

/// One binding per slot, in first-seen order. The engine may declare a slot twice
/// (a prototype that is both an asset and a stack prim); the asset reading wins,
/// because that is the USD the variable names.
fn dedup_inputs(inputs: &[DeclaredInput]) -> Vec<&DeclaredInput> {
    let mut chosen: Vec<&DeclaredInput> = Vec::new();
    for inp in inputs {
        match chosen.iter().position(|c| c.slot == inp.slot) {
            Some(pos) => {
                if chosen[pos].asset.is_none() && inp.asset.is_some() {
                    chosen[pos] = inp;
                }
            }
            None => chosen.push(inp),
        }
    }
    chosen
}

/// The pseudocode rows for a composition tree: material bindings first (in layer
/// order), then operator bindings in dependency order (a reader after its source).
pub fn rows(tree: &CompositionTree) -> Vec<TreeNode> {
    let ops: Vec<&LayerNode> = tree.layers.iter().filter(|l| l.op).collect();
    let materials: Vec<&LayerNode> = tree
        .layers
        .iter()
        .filter(|l| !l.op && l.kind != "session" && l.kind != "root")
        .collect();

    let mut used: Vec<String> = Vec::new();

    // Material bindings: name from the prim a slot references, schema from that
    // slot's structure, falling back to the file stem when nothing references it.
    let mut mats: Vec<(String, String, String, &LayerNode)> = Vec::new();
    for &m in &materials {
        let referring = ops
            .iter()
            .flat_map(|o| o.inputs.iter())
            .find(|inp| inp.asset.as_deref().is_some_and(|a| same_asset(a, &m.id)));
        let (prim, schema) = match referring {
            Some(inp) => (
                inp.prim.clone().unwrap_or_else(|| "/".to_string()),
                inp.structure.last().and_then(|(_, s)| s.clone()).unwrap_or_default(),
            ),
            None => ("/".to_string(), String::new()),
        };
        let base = referring
            .and_then(|i| i.prim.as_deref())
            .map(|p| leaf(p).to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| file_stem(&m.id));
        let var = alloc(&base, &mut used);
        mats.push((var, schema, prim, m));
    }

    // Operator variable names, from the anchor prim's leaf.
    let op_vars: Vec<String> = ops
        .iter()
        .map(|o| {
            let base = leaf(o.node.as_deref().unwrap_or(""));
            let base = if base.is_empty() {
                o.op_name.clone().unwrap_or_else(|| "op".to_string())
            } else {
                base.to_string()
            };
            alloc(&base, &mut used)
        })
        .collect();

    // Every output prim → (operator index, is it the anchor). A stack input resolves
    // against this: the anchor reads as the bare variable, a port as `var.port`.
    let outputs: Vec<(String, usize, bool)> = ops
        .iter()
        .enumerate()
        .flat_map(|(i, o)| {
            o.outputs.iter().enumerate().map(move |(j, p)| (p.clone(), i, j == 0))
        })
        .collect();

    let token = |inp: &DeclaredInput| -> String {
        if let Some(a) = &inp.asset {
            for (var, _, _, m) in &mats {
                if same_asset(a, &m.id) {
                    return var.clone();
                }
            }
        }
        if let Some(p) = &inp.prim {
            for (out, i, anchor) in &outputs {
                if p == out || p.starts_with(&format!("{out}/")) {
                    let v = &op_vars[*i];
                    if p == out && *anchor {
                        return v.clone();
                    }
                    return format!("{v}.{}", leaf(p));
                }
            }
            return p.clone();
        }
        if inp.value.is_empty() {
            "?".to_string()
        } else {
            inp.value.clone()
        }
    };

    // Dependency order: an operator that reads another's output comes after it.
    let n = ops.len();
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n];
    for i in 0..n {
        for inp in &ops[i].inputs {
            if inp.asset.is_some() {
                continue;
            }
            if let Some(p) = &inp.prim {
                for (out, j, _) in &outputs {
                    if *j != i && (p == out || p.starts_with(&format!("{out}/"))) && !deps[i].contains(j) {
                        deps[i].push(*j);
                    }
                }
            }
        }
    }
    let mut emitted = vec![false; n];
    let mut order: Vec<usize> = Vec::new();
    while order.len() < n {
        let mut progressed = false;
        for i in 0..n {
            if !emitted[i] && deps[i].iter().all(|j| emitted[*j]) {
                emitted[i] = true;
                order.push(i);
                progressed = true;
                break;
            }
        }
        if !progressed {
            // A cycle (the engine would not cook one) still draws, in layer order.
            for i in 0..n {
                if !emitted[i] {
                    emitted[i] = true;
                    order.push(i);
                }
            }
        }
    }

    let mut rows = Vec::new();
    for (var, schema, prim, m) in &mats {
        let file = crate::tree::strip_anon_prefix(&m.id);
        let label = if schema.is_empty() {
            format!("let {var} = material(\"{file}\", \"{prim}\")")
        } else {
            format!("let {var}: {schema} = material(\"{file}\", \"{prim}\")")
        };
        rows.push(TreeNode {
            id: m.id.clone(),
            kind: TreeNodeKind::Layer,
            label,
            secondary: Some(format!("// {}", m.kind)),
            hover: Some(m.id.clone()),
            prim: Some(prim.clone()),
            op: false,
            root: false,
            session: false,
            children: Vec::new(),
        });
    }
    for &i in &order {
        let o = ops[i];
        let var = &op_vars[i];
        let kw = kind_word(o.op_name.as_deref());
        let args: Vec<String> = dedup_inputs(&o.inputs)
            .iter()
            .map(|inp| format!("{} = {}", inp.slot, token(inp)))
            .collect();
        let label = match o.output_schemas.first().and_then(|s| s.clone()) {
            Some(s) => format!("let {var}: {s} = {kw}({})", args.join(", ")),
            None => format!("let {var} = {kw}({})", args.join(", ")),
        };
        let ports: Vec<String> = o
            .outputs
            .iter()
            .enumerate()
            .skip(1)
            .map(|(j, p)| {
                let s = o.output_schemas.get(j).and_then(|x| x.clone()).unwrap_or_default();
                if s.is_empty() {
                    leaf(p).to_string()
                } else {
                    format!("{}: {s}", leaf(p))
                }
            })
            .collect();
        let mut secondary = format!("// → {}", o.output.as_deref().unwrap_or("?"));
        if !ports.is_empty() {
            secondary.push_str(&format!(" · {}", ports.join(", ")));
        }
        rows.push(TreeNode {
            id: o.id.clone(),
            kind: TreeNodeKind::Layer,
            label,
            secondary: Some(secondary),
            hover: Some(o.id.clone()),
            prim: o.node.clone(),
            op: true,
            root: false,
            session: false,
            children: Vec::new(),
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::{ask_for, LayerAsk};

    /// Two materials and two operators, shaped exactly like the wire's answer for
    /// `box_scatter.ops.toml` — including the duplicate `prototype` slot.
    fn fixture() -> CompositionTree {
        let value: serde_json::Value = serde_json::from_str(
            r#"{
              "layers": [
                {"id":"C:/x/volume.rusd","kind":"reference","strength":0,"op":false},
                {"id":"C:/x/box.rusd","kind":"reference","strength":1,"op":false},
                {"id":"C:/x/out/samples.rusd","kind":"op","strength":2,"op":true,
                 "op_name":"RusdSamplePoints","node":"/World/Samples",
                 "output":"samples.rusd:/World/Samples",
                 "outputs":["/World/Samples","/World/Samples/points"],
                 "output_schemas":["Xform","Points"],
                 "inputs":[{"slot":"domain","value":"volume.rusd:/World/Volume",
                   "asset":"C:/x/volume.rusd","prim":"/World/Volume",
                   "structure":[{"path":"Volume","schema":"Xform"},{"path":"Volume","schema":"Cube"}]}]},
                {"id":"C:/x/out/boxes.rusd","kind":"op","strength":3,"op":true,
                 "op_name":"RusdInstances","node":"/World/Boxes",
                 "output":"boxes.rusd:/World/Boxes",
                 "outputs":["/World/Boxes"],"output_schemas":["Xform"],
                 "inputs":[
                   {"slot":"points","value":"/World/Samples/points","asset":null,
                    "prim":"/World/Samples/points",
                    "structure":[{"path":"Samples","schema":"Xform"},{"path":"points","schema":"Points"}]},
                   {"slot":"prototype","value":"box.rusd:/World/Prototypes/Box",
                    "asset":"C:/x/box.rusd","prim":"/World/Prototypes/Box",
                    "structure":[{"path":"Prototypes","schema":"Xform"},{"path":"Box","schema":"Cube"}]},
                   {"slot":"prototype","value":"/World/Samples/points","asset":null,
                    "prim":"/World/Samples/points",
                    "structure":[{"path":"Samples","schema":"Xform"},{"path":"points","schema":"Points"}]}
                 ]}
              ],
              "arcs": [], "prim_nodes": [], "variant_sets": []
            }"#,
        )
        .expect("fixture json");
        CompositionTree::parse(&value).expect("fixture parses")
    }

    #[test]
    fn pseudocode_names_the_usd_and_wires_the_stack() {
        let tree = fixture();
        let rows = rows(&tree);
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "let Volume: Cube = material(\"volume.rusd\", \"/World/Volume\")",
                "let Box: Cube = material(\"box.rusd\", \"/World/Prototypes/Box\")",
                "let Samples: Xform = sample_points(domain = Volume)",
                "let Boxes: Xform = instances(points = Samples.points, prototype = Box)",
            ]
        );
        // Every row is a layer carrying its own id, and a click solos it — the same
        // ask the tree panel uses, so the variable renders the USD it names.
        for r in &rows {
            assert_eq!(r.kind, TreeNodeKind::Layer);
            assert!(matches!(ask_for(r), Some(LayerAsk::Solo)));
            assert!(tree.layers.iter().any(|l| l.id == r.id));
        }
        assert_eq!(
            rows.iter().map(|r| r.op).collect::<Vec<_>>(),
            vec![false, false, true, true]
        );
    }

    #[test]
    fn a_duplicate_slot_keeps_the_asset_reading() {
        let tree = fixture();
        let rows = rows(&tree);
        let boxes = rows.iter().find(|r| r.label.starts_with("let Boxes")).unwrap();
        assert!(boxes.label.contains("prototype = Box"));
        assert!(!boxes.label.contains("prototype = Samples"));
    }

    #[test]
    fn an_operator_with_no_materials_still_reads_as_a_call() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"layers":[{"id":"g.rusd","kind":"op","strength":0,"op":true,
               "op_name":"RusdGrid","node":"/World/Grid","output":"g.rusd:/World/Grid",
               "outputs":["/World/Grid"],"output_schemas":["Xform"],"inputs":[]}],
               "arcs":[],"prim_nodes":[],"variant_sets":[]}"#,
        )
        .unwrap();
        let tree = CompositionTree::parse(&value).unwrap();
        let rows = rows(&tree);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "let Grid: Xform = grid()");
    }
}
