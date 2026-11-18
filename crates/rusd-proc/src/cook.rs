//! Cook engine (SPEC §6, D8): evaluates generator nodes as an implicit DAG
//! induced by prim-path read/write dependencies. Cook wraps USD composition —
//! each node reads the composed stage, authors an output layer, and inserts it;
//! openusd pcp recomposes incrementally. Memoized by content hash; only dirty
//! nodes (params or read values changed) re-cook.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};

use openusd::sdf::LayerOffset;
use openusd::usd::Stage;
use rusd_core::{Diagnostic, PrimPath, Value};

use crate::generator::{GenContext, Generator};

/// One generator node instance: a generator schema anchored at a prim path.
#[derive(Clone, Debug)]
pub struct CookNode {
    /// Stable node id (the generator prim path string).
    pub id: String,
    /// Generator schema name (registry key).
    pub generator: String,
    /// Anchor prim path.
    pub path: PrimPath,
    pub params: BTreeMap<String, Value>,
    pub seed: u64,
    pub stage_seed: u64,
    /// Declared inputs: (prim path, attribute) read from the composed stage.
    pub reads: Vec<(String, String)>,
    /// Declared outputs: prim paths this node's output layer authors.
    pub writes: Vec<String>,
}

/// Per-pass counters proving incrementality.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CookStats {
    pub cooked: usize,
    pub reused: usize,
    pub skipped: usize,
}

/// One cooked node's output, as the composition tree reports it (PROTOCOL
/// v1.5.1): which generator ran, what geometry it read, and the layer it authored.
pub struct CookOp {
    /// Stable node id (the generator prim path string).
    pub id: String,
    /// Generator schema name (registry key).
    pub generator: String,
    /// The `input_points` parameter, when the node reads input geometry.
    pub input: Option<String>,
    /// The node's current output layer identifier (changes on every rebuild).
    pub layer_id: String,
}

/// Result of a cook pass: counters plus diagnostics (warnings/errors).
pub struct CookResult {
    pub stats: CookStats,
    pub diags: Vec<Diagnostic>,
    /// Every node with a memoized output layer after this pass, in cook order
    /// (reused nodes included). Replace wholesale after each cook; never accumulate.
    pub ops: Vec<CookOp>,
}

struct MemoEntry {
    key: u64,
    layer_id: String,
}

/// The cook engine: a generator registry plus per-node memo across passes.
#[derive(Default)]
pub struct CookEngine {
    generators: HashMap<String, Box<dyn Generator>>,
    memo: HashMap<String, MemoEntry>,
}

impl CookEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a generator type by name.
    pub fn register(&mut self, g: Box<dyn Generator>) {
        self.generators.insert(g.name().to_owned(), g);
    }

    /// Cook all nodes in dependency order, reusing memoized outputs whose
    /// content key is unchanged. Inserts new output layers and mutes superseded ones.
    pub fn cook(&mut self, stage: &Stage, root_id: &str, nodes: &[CookNode]) -> CookResult {
        let deps = derive_deps(nodes);
        let order = topo_order(nodes, &deps);
        let by_id: HashMap<&str, &CookNode> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let mut stats = CookStats::default();
        let mut diags = Vec::new();

        for id in &order {
            let node = by_id[id.as_str()];
            let key = memo_key(stage, node);
            if self.memo.get(id.as_str()).map(|m| m.key) == Some(key) {
                stats.reused += 1;
                continue;
            }
            let Some(g) = self.generators.get(&node.generator) else {
                stats.skipped += 1;
                diags.push(
                    Diagnostic::warning(
                        "W_UNKNOWN_GENERATOR",
                        format!("no generator registered for '{}'", node.generator),
                    )
                    .at_prim(node.path.as_str()),
                );
                continue;
            };
            let mut ctx =
                GenContext::new(stage, &node.path, &node.params, node.seed, node.stage_seed);
            match g.eval(&mut ctx) {
                Ok(layer) => {
                    diags.extend(ctx.take_warnings());
                    let lid = layer.identifier().to_owned();
                    if let Err(e) = stage.insert_layer(root_id, 0, layer, LayerOffset::IDENTITY) {
                        diags.push(
                            Diagnostic::error("E_COOK", e.to_string()).at_prim(node.path.as_str()),
                        );
                        continue;
                    }
                    if let Some(old) = self
                        .memo
                        .insert(id.clone(), MemoEntry { key, layer_id: lid })
                    {
                        stage.mute_layer(&old.layer_id);
                    }
                    stats.cooked += 1;
                }
                Err(d) => diags.extend(d),
            }
        }
        let ops = order
            .iter()
            .filter_map(|id| {
                let entry = self.memo.get(id.as_str())?;
                let node = by_id[id.as_str()];
                Some(CookOp {
                    id: id.clone(),
                    generator: node.generator.clone(),
                    input: node.params.get("input_points").and_then(|v| match v {
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    }),
                    layer_id: entry.layer_id.clone(),
                })
            })
            .collect();
        CookResult { stats, diags, ops }
    }
}

/// Dependency edges: B depends on A iff B reads a prim that A writes.
pub fn derive_deps(nodes: &[CookNode]) -> HashMap<String, HashSet<String>> {
    let writer: HashMap<&str, &str> = nodes
        .iter()
        .flat_map(|n| n.writes.iter().map(move |w| (w.as_str(), n.id.as_str())))
        .collect();
    let mut deps: HashMap<String, HashSet<String>> = nodes
        .iter()
        .map(|n| (n.id.clone(), HashSet::new()))
        .collect();
    for b in nodes {
        for (prim, _attr) in &b.reads {
            if let Some(&a) = writer.get(prim.as_str())
                && a != b.id
            {
                deps.get_mut(&b.id).unwrap().insert(a.to_owned());
            }
        }
    }
    deps
}

/// Kahn topological sort over the derived dependency graph.
pub fn topo_order(nodes: &[CookNode], deps: &HashMap<String, HashSet<String>>) -> Vec<String> {
    let mut indeg: HashMap<&str, usize> = nodes
        .iter()
        .map(|n| (n.id.as_str(), deps[&n.id].len()))
        .collect();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (b, ds) in deps {
        for a in ds {
            dependents.entry(a.as_str()).or_default().push(b.as_str());
        }
    }
    let mut q: VecDeque<&str> = nodes
        .iter()
        .filter(|n| indeg[n.id.as_str()] == 0)
        .map(|n| n.id.as_str())
        .collect();
    let mut order = Vec::new();
    while let Some(id) = q.pop_front() {
        order.push(id.to_owned());
        if let Some(ds) = dependents.get(id) {
            for &d in ds {
                *indeg.get_mut(d).unwrap() -= 1;
                if indeg[d] == 0 {
                    q.push_back(d);
                }
            }
        }
    }
    order
}

/// Content key: generator + params + seed + actual composed read values.
fn memo_key(stage: &Stage, node: &CookNode) -> u64 {
    let mut h = DefaultHasher::new();
    node.generator.hash(&mut h);
    node.seed.hash(&mut h);
    for (name, val) in &node.params {
        name.hash(&mut h);
        format!("{val}").hash(&mut h);
    }
    for (prim, attr) in &node.reads {
        prim.hash(&mut h);
        attr.hash(&mut h);
        crate::generator::read_value_for_memo(stage, prim, attr).hash(&mut h);
    }
    h.finish()
}

/// Extract generator nodes from a parsed document. M2: writes = anchor path,
/// reads empty; per-generator read/write derivation is refined in M3.
pub fn nodes_from_doc(doc: &rusd_core::syntax::ParsedDocument) -> Vec<CookNode> {
    let mut nodes = Vec::new();
    for prim in &doc.prims {
        let Some(schema) = rusd_scene::schema::lookup(&prim.schema) else {
            continue;
        };
        if schema.kind != rusd_scene::schema::SchemaKind::Generator {
            continue;
        }
        let seed = match prim.props.get("seed") {
            Some(Value::Int(i)) if *i >= 0 => *i as u64,
            _ => 0,
        };
        let reads = match prim.schema.as_str() {
            // D14 geometry-in: a scatter that reads a point cloud depends on that
            // prim's `points` (hashing their world-space values, see
            // `read_value_for_memo`) **and on its own anchor's world transform**,
            // because its instances are written in the anchor's local space.
            "RusdScatter" => {
                let mut reads: Vec<(String, String)> = Vec::new();
                if let Some(p) = prim.props.get("prototype").and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                }) {
                    reads.push((p, "radius".to_owned()));
                }
                if let Some(p) = prim.props.get("input_points").and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                }) {
                    // The input's geometry in **either** form (D14.1): the surface
                    // read covers a `Mesh`'s triangles, the points read covers a
                    // cloud. Both are recorded, because which one a path answers
                    // with depends on the scene, not on this list — and the memo
                    // value of each is the geometry itself.
                    reads.push((p.clone(), crate::generator::SURFACE_READ.to_owned()));
                    reads.push((p, rusd_scene::geometry::POINTS_ATTR.to_owned()));
                    reads.push((
                        prim.path.as_str().to_owned(),
                        crate::generator::WORLD_XFORM_READ.to_owned(),
                    ));
                }
                reads
            }
            // v1.5 two-op pipeline (PROTOCOL v1.5.1): the pair's declarations live
            // in `deps`, because the two ops have to agree on the cloud path between
            // them (the sample op writes it, the instances op reads it).
            "RusdSamplePoints" | "RusdInstances" => {
                crate::deps::reads_for(prim.schema.as_str(), &prim.props, prim.path.as_str())
            }
            _ => Vec::new(),
        };
        let stage_seed = match doc.stage.get("seed") {
            Some(Value::Int(i)) if *i >= 0 => *i as u64,
            _ => 0,
        };
        nodes.push(CookNode {
            id: prim.path.as_str().to_owned(),
            generator: prim.schema.clone(),
            path: prim.path.clone(),
            params: prim.props.clone(),
            seed,
            stage_seed,
            reads,
            writes: crate::deps::writes_for(prim.schema.as_str(), prim.path.as_str()),
        });
    }
    nodes
}
