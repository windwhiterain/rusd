//! Cook engine (SPEC §6, D8): evaluates generator nodes as an implicit DAG
//! induced by prim-path read/write dependencies. Cook wraps USD composition —
//! each node reads the composed stage, authors an output layer, and inserts it;
//! openusd pcp recomposes incrementally. Memoized by content hash; only dirty
//! nodes (params or read values changed) re-cook.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};

use openusd::sdf::{self, LayerOffset};
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
    /// **The input slots the document declared** (v1.12), by their own names and values —
    /// a list, because an operator may declare any number of them; `params` holds what the
    /// engine reads, this holds what the document wrote.
    pub inputs: Vec<(String, String)>,
    pub seed: u64,
    pub stage_seed: u64,
    /// Declared inputs: (prim path, attribute) read from the composed stage.
    pub reads: Vec<(String, String)>,
    /// Declared outputs: prim paths this node's output layer authors.
    pub writes: Vec<String>,
    /// The node's own `description` (v1.5.9), when the document authors one. It is
    /// **metadata about the node**, not a generator parameter: it never reaches the
    /// geometry layer, and the composition tree reports it beside the layer the
    /// node wrote (PROTOCOL v1.5.9).
    pub description: Option<String>,
    /// The directory of the document that named this node, which a relative asset
    /// parameter resolves against (v1.5.8). `None` for a document opened from text.
    pub doc_dir: Option<String>,
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
#[derive(Clone, Debug)]
pub struct CookOp {
    /// Stable node id (the generator prim path string).
    pub id: String,
    /// Generator schema name (registry key).
    pub generator: String,
    /// The `input_points` parameter, when the node reads input geometry.
    pub input: Option<String>,
    /// **Every input the document declared for this node** (v1.12), by the document's own
    /// slot names — `points`, `prototype`, `domain`, and whatever an operator names next.
    /// A two-field `input`/`prototype` would report an op with one as complete.
    pub inputs: Vec<(String, String)>,
    /// The node's current output layer identifier (changes on every rebuild).
    pub layer_id: String,
    /// The file the output layer was written to (v1.9). The layer is **not** in any
    /// stack: a composition reads this file when it needs the op's prims.
    pub layer_path: String,
    /// The node's `description`, when the document gives it one (v1.5.9). Node
    /// metadata: the tree carries it on the op row, and it is never authored into
    /// the layer.
    pub description: Option<String>,
    /// **The named prim paths this node's output authors** (v1.13), from
    /// [`CookNode::writes`]: the output half of the hierarchy the operator declares.
    pub writes: Vec<String>,
}

/// Result of a cook pass: counters plus diagnostics (warnings/errors).
pub struct CookResult {
    pub stats: CookStats,
    pub diags: Vec<Diagnostic>,
    /// Every node with a memoized output layer after this pass, in cook order
    /// (reused nodes included). Replace wholesale after each cook; never accumulate.
    pub ops: Vec<CookOp>,
}

#[derive(Clone)]
struct MemoEntry {
    key: u64,
    layer_id: String,
    /// The file this pass's output layer was written to.
    ///
    /// **The cook never inserts its layers into a live stack** (v1.9, the user's rule:
    /// 哪里都不许插入): a picture is composed by building its own stack from these
    /// files and throwing it away. That is also the only way it can work —
    /// `sdf::Layer` is not `Clone` and `Stage::insert_layer` takes it by value, so a
    /// layer that has been inserted once cannot be inserted again; a file can be read
    /// as often as a composition asks.
    layer_path: std::path::PathBuf,
}

/// The stage a node reads: the document, then the layers the earlier nodes wrote,
/// weakest first. Built for one node and dropped with the pass — v1.9 inserts nothing
/// into anything that outlives the cook (哪里都不许插入), and `sdf::Layer` is not
/// `Clone`, so the only way a later node sees an earlier one's prims is to read that
/// layer back from the file the cook wrote.
///
/// **The document is not in this stack** (v1.9): a graph document is not USD, so it
/// authors no layer. What a node reads is what earlier nodes wrote.
fn cook_stack(doc: &Stage, produced: &[std::path::PathBuf]) -> Result<Stage, String> {
    let mut builder = rusd_scene::document::stage_builder();
    if let Some(session) = doc.session_layer().map(|l| l.identifier().to_owned()) {
        builder = builder.session_layer(session);
    }
    let stack = builder
        .in_memory("rusd_cook_stack.rusd")
        .map_err(|e| e.to_string())?;
    let root = stack.root_layer().identifier().to_owned();
    for path in produced {
        // A product is a `.rusd` document now (v1.9): parsed, lowered, and handed
        // back as the layer `lower` authored under the file's own name.
        let layer = rusd_scene::document::layer_from_file(path)?;
        stack
            .insert_layer(&root, 0, layer, LayerOffset::IDENTITY)
            .map_err(|e| e.to_string())?;
    }
    Ok(stack)
}

/// The cook engine: a generator registry plus per-node memo across passes.
/// How many product files this **process** has written, so two cooks in one process never
/// mint the same path (see the `format!` in [`CookEngine::cook`]).
static NEXT_FILE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[derive(Default)]
pub struct CookEngine {
    generators: HashMap<String, Box<dyn Generator>>,
    /// Where this pass writes its products. `None` is the system temp directory; a caller
    /// that knows the document's directory sets `<doc_dir>/out`, so a product sits beside
    /// the materials it names and its arcs can be **relative** (v1.9).
    out_dir: Option<std::path::PathBuf>,
    memo: HashMap<String, MemoEntry>,
}

impl CookEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Write this pass's products into `dir` (created when the first one is written).
    pub fn set_out_dir(&mut self, dir: Option<std::path::PathBuf>) {
        self.out_dir = dir;
    }

    /// Register a generator type by name.
    pub fn register(&mut self, g: Box<dyn Generator>) {
        self.generators.insert(g.name().to_owned(), g);
    }

    /// Cook all nodes in dependency order, reusing memoized outputs whose content key is
    /// unchanged, and write each new output layer to its own file.
    ///
    /// **Nothing is inserted anywhere** (v1.9: 哪里都不许插入): the products are files, and
    /// the stack a picture is made from is built by the caller ([`CookResult::ops`] names
    /// every file). That is also why there is no root-layer argument any more — a cook
    /// that inserts nothing has no stack to insert into.
    pub fn cook(&mut self, stage: &Stage, nodes: &[CookNode]) -> CookResult {
        let deps = derive_deps(nodes);
        let order = topo_order(nodes, &deps);
        let by_id: HashMap<&str, &CookNode> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let mut stats = CookStats::default();
        let mut diags = Vec::new();
        // The layers earlier nodes produced, in cook order. A node reads the composed
        // stage, and with nothing inserted anywhere (v1.9) that stage has to be built
        // again for every node: the document plus the files written so far. It is a
        // **throwaway stack** — built here, dropped at the end of the pass, never the
        // one a picture is made from.
        let mut produced: Vec<std::path::PathBuf> = Vec::new();
        // The layer ids the earlier products were written under, which is how a later
        // generator names them, paired with the file that now holds them.
        let mut produced_ids: Vec<(String, String)> = Vec::new();
        // Each earlier product's **anchor prim** and the file that holds it. A generator
        // that instances another op's product names it as `@<stack>@</World/Boxes>` — a
        // same-stack reference — and with the products in files, "the stack" is what has
        // to become the file (see the rewrite below).
        let mut produced_anchors: Vec<(String, String)> = Vec::new();

        for id in &order {
            let node = by_id[id.as_str()];
            // The stack this node reads: the document plus the layers the earlier nodes
            // wrote. It is built **before** the memo key, because the key is a hash of
            // this node's read values, and a read of another op's output only exists in
            // that stack — with nothing inserted into the document (v1.9), `/World/Boxes`
            // is a prim in a file, not in the document's stage. Keying on the document
            // stage alone made `Clusters` look like "the input is missing", so its
            // product came out as 40 empty shells instead of 40×60 boxes.
            let stack = match cook_stack(stage, &produced) {
                Ok(stack) => stack,
                Err(e) => {
                    diags.push(Diagnostic::error("E_COOK", e).at_prim(node.path.as_str()));
                    continue;
                }
            };
            let key = memo_key(&stack, node);
            let stack_root_id = Some(stack.root_layer().identifier().to_owned());
            // A memo is only a hit while the product it names is still on disk: the output
            // **is** the file (v1.9), so a cleaned `out/` has to cook again rather than
            // compose a stack out of paths that are gone. Cloned out of the map because the
            // pushes below take the engine mutably.
            let hit = self
                .memo
                .get(id.as_str())
                .filter(|m| m.key == key && m.layer_path.is_file())
                .cloned();
            if let Some(entry) = hit {
                produced.push(entry.layer_path.clone());
                produced_ids.push((
                    entry.layer_id.clone(),
                    entry.layer_path.to_string_lossy().replace('\\', "/"),
                ));
                produced_anchors.push((
                    node.path.as_str().to_owned(),
                    entry.layer_path.to_string_lossy().replace('\\', "/"),
                ));
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
            let mut ctx = GenContext::new(
                &stack,
                &node.path,
                &node.params,
                node.seed,
                node.stage_seed,
                node.doc_dir.as_deref(),
            );
            ctx.inputs = node.inputs.clone();
            match g.eval(&mut ctx) {
                Ok(layer) => {
                    diags.extend(ctx.take_warnings());
                    let lid = layer.identifier().to_owned();
                    // **Not inserted anywhere** (v1.9, the user's rule: 哪里都不许插入).
                    // The output layer is written to a file and its path is what travels;
                    // [`Self::cook`]'s caller composes a stack from these files when a
                    // picture is asked for. Nothing the document owns is touched, and no
                    // long-lived stack carries a generator's output.
                    let dir = match &self.out_dir {
                        Some(dir) => {
                            if let Err(e) = std::fs::create_dir_all(dir) {
                                diags.push(Diagnostic::error(
                                    "E_COOK",
                                    format!("cannot create {}: {e}", dir.display()),
                                ));
                                continue;
                            }
                            dir.clone()
                        }
                        None => std::env::temp_dir(),
                    };
                    let path = dir.join(format!(
                        "rusd_op_{}_{}.rusd",
                        std::process::id(),
                        // A **process-wide** counter, not this engine's: two cooks in one
                        // process (two sessions in a server, two tests in one binary) each
                        // start at zero otherwise, and the second overwrites the first's
                        // products — which reads as "input_points authors no points".
                        NEXT_FILE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    ));
                    // **Written as a `.rusd` document** (v1.9: 全用 `.rusd`). The layer's
                    // prims, their references and their values — the type of every value
                    // coming back from the schema table, not from its shape. The reference
                    // rewriting that used to be a text substitution on `usda` bytes is the
                    // writer's own now: a generator names the throwaway stack it read a
                    // prototype from, and what the reference has to name instead is the
                    // file that holds that prim.
                    let rewrite = |asset: &str, prim: &str| -> Option<String> {
                        // A **bare file name** (v1.9): products live together in `out/`, so an
                        // arc is "the file next to me" and the same directory works wherever
                        // it is copied — server, cache, another machine.
                        let name = |file: &str| {
                            file.rsplit(['/', '\\'])
                                .next()
                                .unwrap_or(file)
                                .to_owned()
                        };
                        for (id, file) in &produced_ids {
                            if asset == id {
                                return Some(name(file));
                            }
                        }
                        if let Some(anchor) = &stack_root_id
                            && asset == anchor.as_str()
                        {
                            for (p, file) in &produced_anchors {
                                if p == prim {
                                    return Some(name(file));
                                }
                            }
                            // A same-stack reference to a prim **the document itself**
                            // authors (`prototype = "/World/Prototypes/Pine"`): no product
                            // holds it, and the document's layer is in every stack this
                            // product composes into — so the reference is internal, and an
                            // empty asset is USD's spelling for that. Left as the stack id
                            // it would name a throwaway stack that no longer exists.
                            return Some(String::new());
                        }
                        None
                    };
                    let written = rusd_scene::document::layer_to_document(&layer, &rewrite)
                        .and_then(|rendered| {
                            for name in &rendered.skipped {
                                diags.push(
                                    Diagnostic::warning(
                                        "W_RUSD_WRITE",
                                        format!(
                                            "'{name}' has no type in the schema table, so the \
                                             .rusd product does not carry it"
                                        ),
                                    )
                                    .at_prim(node.path.as_str()),
                                );
                            }
                            std::fs::write(&path, rendered.text)
                                .map_err(|e| format!("cannot write {}: {e}", path.display()))
                        });
                    if let Err(e) = written {
                        diags.push(
                            Diagnostic::error("E_COOK", e).at_prim(node.path.as_str()),
                        );
                        continue;
                    }
                    self.memo.insert(
                        id.clone(),
                        MemoEntry {
                            key,
                            layer_id: lid.clone(),
                            layer_path: path.clone(),
                        },
                    );
                    produced_ids.push((
                        lid,
                        path.to_string_lossy().replace('\\', "/"),
                    ));
                    produced_anchors.push((
                        node.path.as_str().to_owned(),
                        path.to_string_lossy().replace('\\', "/"),
                    ));
                    produced.push(path);
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
                    inputs: node.inputs.clone(),
                    writes: node.writes.clone(),
                    layer_id: entry.layer_id.clone(),
                    layer_path: entry.layer_path.to_string_lossy().into_owned(),
                    description: node.description.clone(),
                })
            })
            .collect();
        CookResult { stats, diags, ops }
    }
}

/// Build the stack a picture is made from: the document's root layer, then the products
/// this pass wrote, weakest first — so the last op is the strongest, which is what "later
/// cooks override earlier" means (v1.9: 哪里都不许插入, so the picture is assembled here
/// and dropped when it has been read).
///
/// `sdf::Layer` is neither `Clone` nor reusable (`insert_layer` takes it by value), so
/// every layer is **re-read**: the document's from its own bytes, each product from the
/// file the cook wrote. The ops' `layer_id`s are refreshed to the identifiers the new
/// stack gives them — a `.rusd` opened by its format is named by its path, and the stack
/// settles that spelling (`\\?\C:\…` vs `C:/…` are one file written by two hands).
pub fn compose_stack(doc: &Stage, ops: &mut [CookOp]) -> Result<Stage, Vec<Diagnostic>> {
    let mut builder = Stage::builder();
    if let Some(session) = doc.session_layer().map(|l| l.identifier().to_owned()) {
        builder = builder.session_layer(session);
    }
    let composed = builder
        .in_memory("rusd_composed.rusd")
        .map_err(|e| vec![Diagnostic::error("E_COMPOSE", e.to_string())])?;
    let root = composed.root_layer().identifier().to_owned();
    // **The document is not a layer** (v1.9): a graph document is not USD, so nothing of it
    // is composed. The stage metadata it declares (`defaultPrim`, `upAxis`,
    // `metersPerUnit`) is stage metadata — it belongs to this stack, not to a layer — so it
    // is copied onto the stack's own root, and the picture is made of the products alone.
    copy_stage_metadata(&composed, &root, doc);
    for op in ops.iter_mut() {
        let path = std::path::Path::new(&op.layer_path);
        let layer = rusd_scene::document::layer_from_file(path)
            .map_err(|e| vec![Diagnostic::error("E_COOK", e)])?;
        let written = layer.identifier().to_owned();
        composed
            .insert_layer(&root, 0, layer, LayerOffset::IDENTITY)
            .map_err(|e| vec![Diagnostic::error("E_COMPOSE", e.to_string())])?;
        op.layer_id = composed
            .layer_stack()
            .into_iter()
            .find(|id| same_file(id, &written))
            .unwrap_or(written);
    }
    Ok(composed)
}

/// A layer re-read from its own bytes, for the reason [`compose_stack`] gives.

/// Copy the stage metadata a document declares onto the stack that is being built.
///
/// `defaultPrim`, `upAxis` and `metersPerUnit` are properties of a **stage**, not of a
/// layer: a graph document declares them without being one, so they are written where
/// they belong.
fn copy_stage_metadata(composed: &Stage, root: &str, doc: &Stage) {
    use openusd::sdf::AbstractData;
    let Some(mut layer) = composed.layer_mut(root) else {
        return;
    };
    let source = doc.root_layer();
    let data = source.data();
    let pseudo = sdf::Path::abs_root();
    let mut copy: Vec<(String, sdf::Value)> = Vec::new();
    for name in ["defaultPrim", "upAxis", "metersPerUnit"] {
        if let Ok(Some(value)) = data.try_field(&pseudo, name) {
            copy.push((name.to_owned(), value.into_owned()));
        }
    }
    let _ = layer.edit(|l| {
        let d = l.data_mut();
        for (name, value) in copy {
            d.set_field(&pseudo, &name, value);
        }
        Ok(())
    });
}
fn reload(layer: &sdf::Layer) -> Result<sdf::Layer, Vec<Diagnostic>> {
    let format = sdf::LayerRegistry::find_by_id("usda")
        .ok_or_else(|| vec![Diagnostic::error("E_COMPOSE", "the usda format is not registered")])?;
    let written = format
        .write_bytes(layer.data())
        .map_err(|e| vec![Diagnostic::error("E_COMPOSE", format!("usda write failed: {e}"))])?;
    let tag = layer
        .identifier()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("rusd_root.rusd")
        .to_owned();
    sdf::Layer::from_bytes(tag, written.bytes)
        .map_err(|e| vec![Diagnostic::error("E_COMPOSE", e.to_string())])
}

/// Whether two spellings name one file (the one comparison a path deserves).
fn same_file(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        s.strip_prefix(r"\\?\")
            .unwrap_or(s)
            .replace('\\', "/")
            .to_ascii_lowercase()
    }
    a == b || norm(a) == norm(b)
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
///
/// `doc_dir` is the directory of the document the text came from (v1.5.8): a
/// relative asset path in a parameter resolves against it, and `None` (a document
/// opened from text) makes such a path a diagnostic instead.
pub fn nodes_from_doc(
    doc: &rusd_core::syntax::ParsedDocument,
    doc_dir: Option<&str>,
) -> Vec<CookNode> {
    let mut nodes = Vec::new();
    // **An operator has one USD input** (v1.13), so an `instances` node no longer declares
    // the cloud it instances: the cloud is whatever the most recent earlier node wrote
    // under a prim named `points`. That is the data flow of the graph (`谁读谁`), and it is
    // the only thing the single-input form has to reconstruct.
    let mut upstream_points: Option<String> = None;
    for prim in &doc.prims {
        if let Some(points) = crate::deps::writes_for(prim.schema.as_str(), prim.path.as_str())
            .into_iter()
            .find(|p| p == "points" || p.ends_with("/points"))
        {
            upstream_points = Some(points);
        }
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
        let mut props = prim.props.clone();
        // The one USD input fills `prototype` on an instances node; the cloud it consumes
        // comes from the graph's data flow.
        if matches!(prim.schema.as_str(), "RusdInstances" | "RusdScatter")
            && !props.contains_key("input_points")
            && let Some(points) = &upstream_points
        {
            props.insert("input_points".to_owned(), Value::String(points.clone()));
        }
        let read_props = props.clone();
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
                    // `prototype` may be `asset:prim` (v1.5.8); the read names the
                    // prim either way.
                    reads.push((crate::deps::prototype_prim(&p), "radius".to_owned()));
                }
                if let Some(p) = prim.props.get("input_points").and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                }) {
                    // The input's geometry in **every** form a scatter knows (D14.1,
                    // extended by v1.5.8): the surface read covers a `Mesh`'s
                    // triangles, the volume read a `Cube`'s box, the points read a
                    // cloud. All are recorded, because which one a path answers with
                    // depends on the scene, not on this list — and the memo value of
                    // each is the geometry itself.
                    reads.push((p.clone(), crate::generator::SURFACE_READ.to_owned()));
                    reads.push((p.clone(), crate::generator::VOLUME_READ.to_owned()));
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
            params: read_props.clone(),
            inputs: prim.inputs.clone(),
            seed,
            stage_seed,
            reads,
            writes: crate::deps::writes_for(prim.schema.as_str(), prim.path.as_str()),
            description: prim.props.get("description").and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            }),
            doc_dir: doc_dir.map(str::to_owned),
        });
    }
    nodes
}
