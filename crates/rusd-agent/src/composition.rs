//! # The composition tree (`docs/PROTOCOL.md` v1.2, D13)
//!
//! The viewer's tree panel is **USD's composition tree**, not the composed prim
//! hierarchy: a flattened snapshot (Tier A) has already dropped every arc, so
//! the structure cannot be rebuilt on the client. The server reads it off the
//! authority stack instead, and it reads it from **`sdf` layer specs** —
//! `references` / `payload` / `inheritPaths` / `specializes` / `subLayers` /
//! `variantSetNames` are ordinary authored fields, so this needs no `pcp`
//! internals and does not depend on the composition engine's private types.
//!
//! What the tree answers:
//!
//! - **layers** — every layer the stage opened, with its kind and its rank in
//!   the strength order (0 strongest: the session layer first). A layer a
//!   generator cooked is marked `op`, and carries that node's `op_name`, its
//!   `input` and its `output` layer (v1.5.1). Which layers are ops is read off the
//!   **cook's own op list** (v1.5.8); every layer also carries a `description`
//!   (v1.5.9), an op's from its generator node, any other from a document layer
//!   annotation.
//! - **arcs** — the edges between them: `(source layer, source prim)` to
//!   `(target layer, target prim)`, with the arc's kind. A sublayer edge runs
//!   from `(parent layer, "/")` to `(sublayer, "/")`.
//! - **prim_nodes** — one row per composed prim, naming the layers that author
//!   a prim spec for it, strongest first: "who says what about this prim".
//! - **variant_sets** — one row per `(prim, set)`, with the variants any layer
//!   declares, the selection composition has in effect, and which layers author
//!   each variant's spec.
//!
//! ## Node ids
//!
//! `comp_node{id}` takes an id the tree printed. A **layer's** id is the layer
//! identifier itself, the same string arcs and `authored_in` use, so a client
//! correlates a row with an arc end without a lookup. The other three kinds are
//! numbered in emission order — `arc:<n>`, `prim:<path>`, `vset:<n>`. Those are
//! stable for one stage but not across a mutation, and a client refetches the
//! tree when its version moves, which is exactly when an id could have moved.
//!
//! ## What a layer itself says (`layer{id}`)
//!
//! The tree is a view of the *composition*; a clicked layer row also has to show
//! what that layer wrote. That is [`CompositionTree::layer_content`] (PROTOCOL
//! v1.5.5): one row per prim-like spec the layer authors, in the layer's own
//! order, each naming its specifier or scaffolding kind, its `typeName`, the
//! properties it authors and the references it carries. It is the layer's fact
//! content, read off one `sdf` layer — no composition, no aggregation. An id no
//! opened layer has is refused, and the refusal lists the layers there are.
//!
//! ## An arc target the stage never opened
//!
//! A reference names an asset path, and the layer it resolves to is a
//! *different* string (the resolver's canonical spelling). The composition API
//! does not hand out a stage's resolver, so the target is matched against the
//! opened layers — exact identifier, then resolved path, then file name — and
//! when nothing matches, the arc reports the asset path as authored. The tree
//! then names a target the `layers` list does not hold, which is honest: the
//! edge is in the layer, the layer is not in this stage.
//!
//! ## Strength
//!
//! Layer rank comes from `Stage::layer_stack` (the root layer stack, session
//! layers first), then the layers an arc opened
//! (`Stage::layer_identifiers`), which composition lists in the order it reached
//! them. Every layer is loaded before the rank is read: a reference's target
//! opens lazily, so the traverse is what makes the list complete rather than
//! "those reached so far".

use std::collections::BTreeMap;

use openusd::sdf::{self, ChildrenKey, FieldKey, SpecType, Value as SdfValue};
use openusd::usd::{PrimPredicate, Stage};
use rusd_proc::CookOp;
use serde_json::{Value, json};

/// The composition tree of one stage (`composition_tree{}`), and the details of
/// its nodes (`comp_node{id}`).
pub struct CompositionTree {
    layers: Vec<LayerEntry>,
    arcs: Vec<ArcEntry>,
    prims: Vec<PrimEntry>,
    vsets: Vec<VariantSetEntry>,
}

/// One input slot as the document declared it, resolved to the USD it names.
///
/// `value` is what the document wrote (`"/World/Samples/points"`, `"box.rusd:/World/…"`);
/// `asset` is that file when the slot names another layer, and `prim` is the prim the slot
/// points at — in that layer or in this stack. The list this belongs to has no fixed
/// length: an operator declares as many slots as its table has.
struct DeclaredInput {
    slot: String,
    value: String,
    asset: Option<String>,
    prim: Option<String>,
    /// **The hierarchy that input names, level by level, each with its USD schema**
    /// (`typeName`): `/World` is an `Xform`, `/World/Terrain` a `Mesh`. This *is* what the
    /// operator reads — a slot name is the document's word for it, the schema is what USD
    /// says it is.
    structure: Vec<(String, Option<String>)>,
}

fn declared_inputs(op: &CookOp, stage: &Stage) -> Vec<DeclaredInput> {
    let doc_dir = std::path::Path::new(&op.layer_path)
        .parent()
        .and_then(|p| p.parent());
    // **The operator schema is its inputs** (v1.13, the user's ruling): `{prototype,
    // points}` for an `instances` node — the prototype it instances and the cloud it
    // consumes — and `{points}` for a sampler, whose one input is the geometry it reads.
    let mut out: Vec<DeclaredInput> = Vec::new();
    if let Some(points) = op.input.as_deref() {
        out.push(input_entry("points", points, doc_dir, stage));
    }
    if matches!(op.generator.as_str(), "RusdInstances" | "RusdScatter") {
        for (_slot, value) in &op.inputs {
            out.push(input_entry("prototype", value, doc_dir, stage));
        }
    }
    out
}

/// The document's own `input` string for this operator: `"/World/Boxes"`,
/// `"volume.rusd:/World/Volume"`. The declaration is the cook's `inputs`, whose slot the
/// graph spells `input`; a document that names its slots otherwise is read by the one entry
/// that is not the implicit `points` cloud, and the last fallback is the geometry parameter
/// the node was cooked with.
fn document_input(layer: &LayerEntry) -> Option<String> {
    layer
        .inputs
        .iter()
        .find(|i| i.slot == "input")
        .or_else(|| layer.inputs.iter().rev().find(|i| i.slot != "points"))
        .or_else(|| layer.inputs.first())
        .map(|i| i.value.clone())
        .or_else(|| layer.op_input.clone())
}

/// The document's own `output` string for this operator — `usd path:/hierarchy`, spelled
/// with the file the document named (`instances.rusd:/World/Instances`).
///
/// The file part is the cook's `output_file`; a document that names no file falls back to
/// the file name of the layer it produced, which is the same file.
fn document_output(layer: &LayerEntry) -> Option<String> {
    let file = layer
        .file
        .clone()
        .unwrap_or_else(|| file_name(&layer.id).to_owned());
    if file.is_empty() {
        return None;
    }
    Some(match layer.outputs.first() {
        Some(prim) => format!("{file}:{prim}"),
        None => file,
    })
}

/// The file part of an authored `usd path:/hierarchy` spelling: `"terrain.rusd:/World/Terrain"`
/// → `"terrain.rusd"`, and a bare prim path (`"/World/Boxes"`) → `None`.
fn authored_file(value: &str) -> Option<String> {
    let i = value.rfind(':')?;
    if i == 0 || !value[i + 1..].starts_with('/') {
        return None;
    }
    Some(value[..i].to_owned())
}

/// One input of an operator schema: its name, the USD it names, and that prim's schema.
fn input_entry(
    name: &str,
    value: &str,
    doc_dir: Option<&std::path::Path>,
    stage: &Stage,
) -> DeclaredInput {
    let parsed = doc_dir.and_then(|dir| {
        rusd_scene::reference::parse(value, Some(dir.to_string_lossy().as_ref())).ok()
    });
    let prim = match &parsed {
        Some(p) => Some(p.path.as_str().to_owned()),
        None if value.starts_with('/') => Some(value.to_owned()),
        None => None,
    };
    DeclaredInput {
        slot: name.to_owned(),
        value: value.to_owned(),
        asset: parsed.as_ref().and_then(|p| p.asset.clone()),
        prim: prim.clone(),
        structure: vec![(
            name.to_owned(),
            schema_of(
                parsed.as_ref().and_then(|p| p.asset.as_deref()),
                prim.as_deref(),
                stage,
            ),
        )],
    }
}

/// The `typeName` of the prim an input names, when the layer (or the stage) knows it.
fn schema_of(asset: Option<&str>, prim: Option<&str>, stage: &Stage) -> Option<String> {
    let prim = prim?;
    let owned;
    let stage = match asset {
        Some(a) => {
            owned = openusd::usd::Stage::open(a).ok();
            match owned.as_ref() {
                Some(s) => s,
                None => stage,
            }
        }
        None => stage,
    };
    sdf::path(prim)
        .ok()
        .and_then(|p| stage.prim(p).ok())
        .and_then(|p| p.type_name().ok().flatten())
        .map(|t| t.as_str().to_owned())
}

/// Every level of `prim` (its ancestors and itself) with the schema it composes to.
///
/// `asset` is the layer that authors it when the input names another file; without one the
/// levels have no schema to report, and a level the layer does not define is `None`.
fn structure_of(
    asset: Option<&str>,
    prim: Option<&str>,
    stage: &Stage,
) -> Vec<(String, Option<String>)> {
    let Some(prim) = prim else {
        return Vec::new();
    };
    // A material's own layer answers for it; a prim of this stack is answered by the stack.
    let owned;
    let stage = match asset {
        Some(a) => {
            owned = openusd::usd::Stage::open(a).ok();
            match owned.as_ref() {
                Some(s) => s,
                None => stage,
            }
        }
        None => stage,
    };
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut acc = String::new();
    for part in prim.split('/').filter(|p| !p.is_empty()) {
        acc.push('/');
        acc.push_str(part);
        let schema = sdf::path(acc.as_str())
            .ok()
            .and_then(|p| stage.prim(p).ok())
            .and_then(|p| p.type_name().ok().flatten())
            .map(|t| t.as_str().to_owned());
        out.push((acc.clone(), schema));
    }
    out
}

struct LayerEntry {
    /// The layer identifier, which is also this node's id.
    id: String,
    kind: &'static str,
    strength: usize,
    /// Whether this layer is a **cook output layer** (`docs/PROTOCOL.md`
    /// v1.4): a layer the cook inserted into the root layer stack. It is read off
    /// the **cook's own op list** (v1.5.8), not guessed from the root's sublayer
    /// stack — a document may name layers of its own, and those are not ops.
    op: bool,
    /// The **generator** that wrote this layer (PROTOCOL v1.5.1), when the tree
    /// was built with the cook's [`CookOp`] list and one of its rows names this
    /// layer as its output. Always `None` on a layer that is not an op.
    op_name: Option<String>,
    /// The **anchor prim path** of the node that wrote this layer (v1.8): the
    /// generator node's own path, which is the `CookOp`'s `id`. A client that has a
    /// layer row and wants the generator node (its `comp_node`, its inputs) gets the
    /// node by name instead of guessing it from the prims the layer happens to
    /// author. Always `None` on a layer that is not an op.
    node: Option<String>,
    /// The `input_points` prim the generator read. `None` when it read no
    /// geometry at all — a parametric generator — and when no cook op answered
    /// for this layer; the wire tells those two apart by [`LayerEntry::op`] and
    /// reports both as `null`.
    op_input: Option<String>,
    /// **Every input the document declared for this operator** (v1.12): an `instances` op
    /// has two — the cloud it reads (`points`) and the prototype it instances
    /// (`prototype`) — and a row that showed one was reporting half the declaration. Each
    /// entry names the USD it points at: an **asset** (`box.rusd:/World/Prototypes/Box`)
    /// or a prim of this stack (`/World/Samples/points`).
    inputs: Vec<DeclaredInput>,
    /// **The named prim paths this node's output authors** (v1.13): the other half of the
    /// hierarchy an operator declares — its `outputs` in the document, as USD paths.
    outputs: Vec<String>,
    /// What this layer **is**, in words (PROTOCOL v1.5.9). Two sources, one field:
    /// an `op` layer takes its generator node's own `description = "..."`, and any
    /// other layer takes a document **layer annotation**
    /// (`[doc.prototypes] layer = "prototypes.usda" description = "..."`). Absent
    /// when neither says anything, and reported as `null`.
    description: Option<String>,
    /// **The file the document names for this USD**, exactly as the author spelled it
    /// (`instances.rusd`, `terrain.rusd`) — not the resolved absolute path. A row that
    /// prints a path prints this one (v1.15): the absolute spelling is the server's, the
    /// document's is the author's.
    file: Option<String>,
}

/// The cook output layers of `stage` in **cook order** (oldest first), which is
/// the order `preview{upto}` counts in (PROTOCOL v1.4). A muted layer is not in
/// the root's stack, so a superseded output layer of an earlier pass is not
/// listed.
pub fn cook_output_layers(stage: &Stage) -> Vec<String> {
    // The cook inserts its output layers under the **session** layer (v1.9: the
    // document's root layer is the user's authored file and is never written), so the
    // op layers are the session's sublayers, newest first. A stage built before that
    // change kept them under the root, and that spelling is still read when the
    // session holds none.
    let mut ids: Vec<String> = Vec::new();
    if let Some(session) = stage.session_layer() {
        let session_id = session.identifier().to_owned();
        ids = stage.sub_layers(&session_id);
        if ids.first().is_some_and(|id| *id == session_id) {
            ids.remove(0);
        }
        ids.reverse();
    }
    if ids.is_empty() {
        let root_id = stage.root_layer().identifier().to_owned();
        ids = stage.sub_layers(&root_id);
        // `sub_layers` lists the parent first.
        if ids.first().is_some_and(|id| *id == root_id) {
            ids.remove(0);
        }
        ids.reverse();
        // The stack a picture is made from carries the **document's** root layer as a
        // sublayer (v1.9: the cook writes its layers to files, nothing is inserted),
        // and that layer is the stop family's `root` — not a cook output. It is the
        // weakest sublayer, so it is the first after the reversal.
        ids.retain(|id| layer_basename(id) != "rusd_root.rusd");
    }
    ids
}

/// The file name of a layer identifier, which is how a document's layer
/// annotation (`layer = "prototypes.usda"`) names a layer the stage spells as an
/// absolute resolved path. An anonymous identifier (`anon:7:rusd_instances.usda`)
/// has no separator in it, so its whole spelling is its name.
pub(crate) fn layer_basename(id: &str) -> &str {
    let mut name = id.rsplit(['/', '\\']).next().unwrap_or(id);
    // A layer opened from another layer's bytes is tagged with that layer's own
    // identifier, and an anonymous layer's identifier is itself `anon:<n>:<file>` — so a
    // file name can carry a nested tag (`anon:31468:anon:31132:rusd_root.rusd`). The name
    // is what follows the last such tag, or `rusd_root.rusd` never matches and the layer
    // it names is treated as a cook output (v1.9).
    while let Some(rest) = name.strip_prefix("anon:") {
        match rest.split_once(':') {
            Some((_, tail)) if !tail.is_empty() => name = tail,
            _ => break,
        }
    }
    name
}

struct ArcEntry {
    id: String,
    kind: &'static str,
    source_layer: String,
    source_prim: String,
    target_layer: String,
    target_prim: String,
}

struct PrimEntry {
    id: String,
    path: String,
    authored_in: Vec<String>,
}

struct VariantSetEntry {
    id: String,
    prim: String,
    name: String,
    variants: Vec<String>,
    selection: Option<String>,
    /// Which layers author each variant's spec, strongest first.
    authored: Vec<(String, Vec<String>)>,
}

/// One opened layer as this module sees it: what the resolver resolved it to
/// (so an asset path can be matched to it) and the specs it holds.
struct LayerScan {
    id: String,
    resolved: Option<String>,
    /// Every spec in the layer, with its type — owned, so a scan outlives the
    /// `Ref` the layer was borrowed through.
    specs: Vec<(sdf::Path, SpecType)>,
}

impl CompositionTree {
    /// Build the tree of `stage`, naming each cook output layer after the
    /// [`CookOp`] whose `layer_id` that layer is (PROTOCOL v1.5.1). `ops` is the
    /// latest pass's list, in cook order; a layer no row names keeps the `null`
    /// fallback rather than being left out.
    ///
    /// **`ops` is also what says which layers are ops** (v1.5.8). The previous rule
    /// — "every sublayer of the root layer is a cook output" — held only while a
    /// document could author no layers of its own; a `reference` to a material
    /// layer opens a layer no cook wrote, and it must not be labelled an op. So the
    /// question is answered by the cook's own list, and a layer it never wrote is
    /// an ordinary layer whatever its position in the stack.
    ///
    /// `descriptions` are the document's **layer annotations** (v1.5.9):
    /// `(layer name as the document spells it, text)`. A layer matches one when its
    /// identifier's **file name** is that name — the identifier itself is a
    /// resolved absolute path, and a document names `prototypes.usda`, not
    /// `C:/art/prototypes.usda`. `root` and `session` name those two layers
    /// outright.
    ///
    /// An empty `ops` is the join-fell-through case: a stage assembled by hand,
    /// whose layers the cook never wrote, reports `null` generator and input on
    /// every op row instead of being labelled with someone else's node.
    pub fn build_with_ops(
        stage: &Stage,
        ops: &[CookOp],
        descriptions: &[(String, String)],
    ) -> Self {
        let _ = stage.traverse(PrimPredicate::ALL, |_| {});
        let root_id = stage.root_layer().identifier().to_owned();
        let session_id = stage.session_layer().map(|l| l.identifier().to_owned());

        // Strength order: the root layer stack first (session layers, then the
        // root layer and its sublayers), then everything an arc opened.
        let order: Vec<String> = {
            let mut order: Vec<String> = Vec::new();
            for id in stage
                .layer_stack()
                .into_iter()
                .chain(stage.layer_identifiers())
            {
                if !order.contains(&id) {
                    order.push(id);
                }
            }
            // **The wrappers are not part of the graph** (v1.9: 这个工具的文档是合成图).
            // `rusd_composed.rusd` is the stack a snapshot is read from and
            // `rusd_cook_stack.rusd` the one a cook reads; both are how a picture is
            // assembled, not nodes the document authored, so neither gets a row. The
            // document's own lowered layer is not a row either — the composed root row
            // *is* the document in this view. A **product** is a row, and it is a file
            // now, so it is matched by the cook's own list below rather than by its name:
            // the earlier rule dropped every path-spelled `rusd_op_*` from the order, which
            // after v1.9 is the products themselves (their rows and the arcs that leave
            // them both vanished).
            order.retain(|id| {
                // **The engine's own layers are not rows** (v1.9): the assembly's root
                // (`rusd_composed.rusd`), the cook's throwaway stack, and the layer the
                // document lowers into are all how a picture is *built* — none of them is a
                // layer the document or an operator authored. `rusd_root.rusd` is not even a
                // real layer any more: a graph document is not USD.
                let base = layer_basename(id);
                base != "rusd_composed.rusd"
                    && base != "rusd_cook_stack.rusd"
                    && base != "rusd_root.rusd"
            });
            order
        };
        let scans: Vec<LayerScan> = order
            .iter()
            .map(|id| LayerScan {
                id: id.clone(),
                resolved: stage
                    .layer(id)
                    .and_then(|l| l.resolved_path().map(str::to_owned)),
                specs: layer_specs(stage, id),
            })
            .collect();

        let arcs = scan_arcs(stage, &scans);

        // A layer outside the root layer stack is there because an arc reached
        // it, and the arc that named it says what kind of edge it is. The
        // strongest edge wins when several reach the same layer.
        let mut arc_kind: BTreeMap<&str, &'static str> = BTreeMap::new();
        for arc in &arcs {
            let kind = arc_kind
                .entry(arc.target_layer.as_str())
                .or_insert(arc.kind);
            if arc_rank(arc.kind) < arc_rank(kind) {
                *kind = arc.kind;
            }
        }
        let layers: Vec<LayerEntry> = scans
            .iter()
            .enumerate()
            .map(|(strength, scan)| {
                // The join: the cook names its output layers, and this layer's
                // identifier is the key. That join is also the op test (v1.5.8): a
                // layer the cook wrote is an op, and nothing else is — a
                // `reference` to a material layer is not a generator's output just
                // because it sits in the stage.
                let joined = ops.iter().find(|o| o.layer_id == scan.id);
                let op = joined.is_some();
                let kind = if Some(&scan.id) == session_id.as_ref() {
                    "session"
                } else if scan.id == root_id {
                    "root"
                } else if op {
                    // **A product is an op, not a layer of anybody's stack** (v1.9):
                    // the root does not reference the geometry it cooked, and a cooked
                    // product is not a `sublayer` of the assembly either — it is the
                    // layer the document's generator node wrote.
                    "op"
                } else if let Some(kind) = arc_kind.get(scan.id.as_str()) {
                    kind
                } else {
                    "sublayer"
                };
                // A description comes from the generator node for an op, and from a
                // document annotation for every other layer. An op's own node is the
                // authority; an annotation is what is left.
                let description = joined.and_then(|o| o.description.clone()).or_else(|| {
                    let name = layer_basename(&scan.id);
                    descriptions
                        .iter()
                        .find(|(layer, _)| {
                            kind == "root" && layer == "root"
                                || kind == "session" && layer == "session"
                                || layer == name
                                || *layer == scan.id
                        })
                        .map(|(_, text)| text.clone())
                });
                LayerEntry {
                    id: scan.id.clone(),
                    kind,
                    strength,
                    op,
                    op_name: joined.map(|o| o.generator.clone()),
                    node: joined.map(|o| o.id.clone()),
                    op_input: joined.and_then(|o| o.input.clone()),
                    inputs: joined
                        .map(|o| declared_inputs(o, stage))
                        .unwrap_or_default(),
                    outputs: joined.map(|o| o.writes.clone()).unwrap_or_default(),
                    file: joined.and_then(|o| o.output_file.clone()),
                    description,
                }
            })
            .collect();

        // **The USD an operator read is a node too** (v1.12): the tree is a view of all the
        // USD this document involves — the products the ops wrote and the materials they
        // read. A material a sampler read (`domain = "terrain.rusd:/World/Terrain"`) is
        // opened by the cook and left behind, so no layer of the stage names it and no row
        // could show it. The entry is a **row**, not a layer of the stack: it composes
        // nothing and is labelled `reference`, and its detail is the file's own text.
        let mut layers = layers;
        // **The nodes are the source, not the rows** (v1.14): a material is read by an
        // operator, and an operator's layer may not be in the stage yet when this runs —
        // the products are appended below. Reading the declaration off the cook's own node
        // list finds every USD the document names, whether or not that layer already has a
        // row, so `terrain.rusd` gets one as readily as a product does.
        let declared: Vec<(String, String)> = ops
            .iter()
            .flat_map(|op| {
                declared_inputs(op, stage)
                    .into_iter()
                    .filter_map(|i| Some((i.asset?, i.value)))
            })
            .collect();
        for (asset, input) in declared {
            // **A product is not a material** (v1.14): this pass runs before the products
            // are appended below, so an operator whose USD input is another operator's own
            // output has to be recognised by the cook's file names rather than by the rows
            // that do not exist yet. Otherwise the product is pushed as a `reference` and
            // the product loop then skips it — a cooked file would lose its op row.
            if layers.iter().any(|l| same_file(&l.id, &asset))
                || ops.iter().any(|op| same_file(&op.layer_path, &asset))
            {
                continue;
            }
            let strength = layers.len();
            // Spelled before `asset` moves into `id`: the row prints the document's spelling.
            let file = authored_file(&input).or_else(|| Some(file_name(&asset).to_owned()));
            layers.push(LayerEntry {
                id: asset,
                kind: "reference",
                strength,
                op: false,
                op_name: None,
                node: None,
                op_input: Some(input.clone()),
                inputs: Vec::new(),
                outputs: Vec::new(),
                file,
                // No description: the row reads `<file> [reference · n]`, the same shape
                // every other material row has. What the operator said about it is the
                // file's detail (`comp_node`), not its row.
                description: None,
            });
        }

        // **Every product is a row** (v1.13): a stage holds one USD, so the products are not
        // layers of it — they are files the document wrote, each opened on its own, and the
        // tree lists them by the file they are.
        let mut layers = layers;
        for op in ops {
            if layers.iter().any(|l| same_file(&l.id, &op.layer_id)) {
                continue;
            }
            let strength = layers.len();
            layers.push(LayerEntry {
                id: op.layer_id.clone(),
                kind: "op",
                strength,
                op: true,
                op_name: Some(op.generator.clone()),
                node: Some(op.id.clone()),
                op_input: op.input.clone(),
                inputs: declared_inputs(op, stage),
                outputs: op.writes.clone(),
                file: op.output_file.clone(),
                description: op.description.clone(),
            });
        }

        // **A file's own edges are facts about that file** (v1.14): `scan_arcs` walked the
        // layers the stage opened, and a product is not one of them — so its row could
        // only say `reference: （none）` about a file that references another one from
        // every instance prim. Each file that has no scan of its own is opened as a stage
        // and read the same way; the targets are resolved by file name, because the file
        // spells them the way its author did (`boxes.rusd`, relative) and the tree names
        // them by identifier.
        let mut arcs = arcs;
        for layer in &layers {
            if scans.iter().any(|scan| scan.id == layer.id) {
                continue;
            }
            arcs.extend(file_arcs(&layer.id, &layers));
        }
        // Each file numbered its own edges from zero. An arc's id is how a client asks
        // `comp_node` about it (the index into this list), so the ids are renumbered over
        // the joined list — one numbering, no duplicates.
        for (n, arc) in arcs.iter_mut().enumerate() {
            arc.id = format!("arc:{n}");
        }

        let (prims, vsets) = scan_namespace(stage, &scans, &layers);
        CompositionTree {
            layers,
            arcs,
            prims,
            vsets,
        }    }
    /// The ids of the opened layers, strongest first: what `layer{id}` accepts,
    /// and exactly what its refusal lists. Same rows, same order, as
    /// [`CompositionTree::to_json`]'s `layers`.
    pub fn layer_ids(&self) -> Vec<String> {
        self.layers.iter().map(|l| l.id.clone()).collect()
    }

    /// The content of one opened layer (`layer{id}`, PROTOCOL v1.5.5): every spec
    /// the layer authors, in the layer's own spec order, so a clicked layer row
    /// can show what *that layer* says rather than what the composition makes of
    /// it. `None` when no opened layer has that id.
    ///
    /// A row is one **prim-like** spec: its `path`, its `spec_type`, the
    /// `type_name` it declares when it declares one, the `properties` it authors,
    /// and the `references` it carries. `spec_type` is a prim spec's **specifier**
    /// (`def` / `over` / `class`), because that is the word a layer writes, and
    /// `pseudo_root` / `variant_set` / `variant` for the scaffolding specs, which
    /// have no specifier. A property spec is folded into its owning prim's row (it
    /// is named there in `properties`) rather than listed on its own, so a layer of
    /// two hundred instances is two hundred rows and not thousands. The list is
    /// the layer's fact content and is **not** aggregated for display.
    ///
    /// Pure read: nothing about the stage moves, and the answer is a function of
    /// the layer alone, so the same layer answers the same bytes.
    pub fn layer_content(&self, stage: &Stage, id: &str) -> Option<Value> {
        let layer = self.layers.iter().find(|l| l.id == id)?;
        // A reference names an asset path, and the layer it resolves to is a
        // different string. Resolving it here by the same rule `scan_arcs` uses
        // (identifier, then resolved path, then file name) is what makes a
        // reference row and the arc it makes name one layer.
        let scans: Vec<LayerScan> = self
            .layers
            .iter()
            .map(|l| LayerScan {
                id: l.id.clone(),
                resolved: stage
                    .layer(&l.id)
                    .and_then(|guard| guard.resolved_path().map(str::to_owned)),
                specs: Vec::new(),
            })
            .collect();
        Some(op_layer_json(stage, layer, &scans))
    }

    /// The wire shape of `composition_tree{}`.
    pub fn to_json(&self) -> Value {
        json!({
            "layers": self.layers.iter().map(|l| {
                let mut row = json!({
                    "id": l.id,
                    "kind": l.kind,
                    "strength": l.strength,
                    "op": l.op,
                    // PROTOCOL v1.5.9: every layer row says what it is, in words —
                    // an op from its generator node's `description`, any other layer
                    // from a document layer annotation. `null` when neither says.
                    "description": l.description,
                });
                // PROTOCOL v1.5.1: only an op row carries the generator's name
                // and its input; the four fields are additive, so a client that
                // predates them ignores what it does not know. v1.8 adds `node`,
                // the generator node's anchor prim path — `op_name` says which
                // generator ran, `node` says which node of the document it was.
                if l.op {
                    row["op_name"] = json!(l.op_name);
                    // **The document's own words** (v1.15): the operator's `input` and
                    // `output` strings, `usd path:/hierarchy` — `input = "/World/Boxes"`,
                    // `output = "instances.rusd:/World/Instances"`. A row prints what the
                    // author wrote: the resolved absolute path is the server's business.
                    row["input"] = json!(document_input(l));
                    row["inputs"] = json!(l
                        .inputs
                        .iter()
                        .map(|i| json!({
                            "slot": i.slot,
                            "value": i.value,
                            "asset": i.asset,
                            "prim": i.prim,
                            "structure": i
                                .structure
                                .iter()
                                .map(|(path, schema)| json!({"path": path, "schema": schema}))
                                .collect::<Vec<_>>(),
                        }))
                        .collect::<Vec<_>>());
                    row["outputs"] = json!(l.outputs);
                    row["output"] = json!(document_output(l));
                    row["node"] = json!(l.node);
                }
                row
            }).collect::<Vec<_>>(),
            "arcs": self.arcs.iter().map(|a| json!({
                "id": a.id,
                "kind": a.kind,
                "source_layer": a.source_layer,
                "source_prim": a.source_prim,
                "target_layer": a.target_layer,
                "target_prim": a.target_prim,
            })).collect::<Vec<_>>(),
            "prim_nodes": self.prims.iter().map(|p| json!({
                "id": p.id,
                "path": p.path,
                "authored_in": p.authored_in,
            })).collect::<Vec<_>>(),
            "variant_sets": self.vsets.iter().map(|v| json!({
                "id": v.id,
                "prim": v.prim,
                "name": v.name,
                "variants": v.variants,
                "selection": v.selection,
            })).collect::<Vec<_>>(),
        })
    }

    /// The arcs a composed prim **itself** authors, as arc ids: the outgoing
    /// edges of this prim, read across the arcs that opened it (v1.8).
    ///
    /// An arc's `source_prim` is a path **in its own layer**, so matching it against
    /// the composed path finds an arc only when the two spellings coincide. Under a
    /// reference they do not: `/World/Clusters/i000000/i000009`'s reference to the
    /// prototype is authored by `cluster.usda` at `/World/Boxes/i000009`, so the plain
    /// comparison found nothing and a nested prim reported no outgoing edges.
    /// `Prim::prim_stack` supplies the translation, and the arc is this prim's when
    /// its source is one of the specs contributing to it.
    fn arcs_out(&self, stage: &Stage, path: &str) -> Vec<String> {
        let Ok(wanted) = sdf::path(path) else {
            return Vec::new();
        };
        let Ok(prim) = stage.prim(&wanted) else {
            return Vec::new();
        };
        let sites: Vec<(String, String)> = match prim.prim_stack() {
            Ok(sites) => sites
                .into_iter()
                .map(|s| (s.layer, s.path.as_str().to_owned()))
                .collect(),
            Err(_) => Vec::new(),
        };
        self.arcs
            .iter()
            .filter(|arc| {
                arc.source_prim == path
                    || sites
                        .iter()
                        .any(|(layer, site)| *layer == arc.source_layer && *site == arc.source_prim)
            })
            .map(|arc| arc.id.clone())
            .collect()
    }

    /// The detail of one node (`comp_node{id}`), or `None` when nothing carries
    /// that id.
    pub fn node(&self, stage: &Stage, id: &str) -> Option<Value> {
        if let Some(arc) = id
            .strip_prefix("arc:")
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| self.arcs.get(n))
        {
            return Some(arc_detail(stage, arc));
        }
        if let Some(vset) = id
            .strip_prefix("vset:")
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| self.vsets.get(n))
        {
            return Some(json!({
                "kind_type": "variant_set",
                "id": vset.id,
                "prim": vset.prim,
                "name": vset.name,
                "variants": vset.variants,
                "selection": vset.selection,
                "per_variant": vset.authored.iter().map(|(variant, layers)| json!({
                    "variant": variant,
                    "authored_in": layers,
                })).collect::<Vec<_>>(),
            }));
        }
        if let Some(path) = id.strip_prefix("prim:")
            && self.prims.iter().any(|p| p.path == path)
        {
            return Some(json!({
                "kind_type": "prim",
                "id": id,
                "path": path,
                "opinions": opinions(stage, path),
                "arcs_out": self.arcs_out(stage, path),
                "variant_sets": self.vsets.iter()
                    .filter(|v| v.prim == path)
                    .map(|v| v.id.clone())
                    .collect::<Vec<_>>(),
            }));
        }
        // A bare identifier names a layer: that is how the tree spells one.
        if let Some(layer) = self.layers.iter().find(|l| l.id == id) {
            return with_layer(stage, &layer.id, |borrow| {
                json!({
                    "kind_type": "layer",
                    "id": layer.id,
                    "identifier": layer.id,
                    "kind": layer.kind,
                    "strength": layer.strength,
                    "op": layer.op,
                    "usda_text": match borrow.export_to_string() { Ok(s) => s, Err(e) => format!("<usda export error: {e:?}>") },
                    "spec_count": borrow.data().spec_paths().len(),
                })
            });
        }
        None
    }
}

// --- building ---------------------------------------------------------------

/// How strong a composition arc is, strongest first: the LIVERPS order. Used
/// only to pick one kind for a layer several arcs reach.
fn arc_rank(kind: &str) -> usize {
    match kind {
        // A cook product is not a layer-stack edge at all: it is what the generator a
        // document prim declares produced. It ranks with the stack edges because that is
        // where it sits in the picture, not because it is one.
        "generates" => 0,
        "sublayer" => 0,
        "inherit" => 1,
        "variant" => 2,
        "reference" => 3,
        "payload" => 4,
        _ => 5,
    }
}

/// Borrow `layer` for one computation and return its result owned. Every read in
/// this module goes through here: a `SpecRef` borrows the guard, so it cannot
/// outlive the call.
fn with_layer<T>(stage: &Stage, layer: &str, f: impl FnOnce(&sdf::Layer) -> T) -> Option<T> {
    let guard = stage.layer(layer)?;
    Some(f(&guard))
}

/// One `layer{id}` answer (PROTOCOL v1.5.5, extended by v1.8): what the layer
/// authors, and — for an op layer — which generator node wrote it.
///
/// `node` is the same field [`CompositionTree::to_json`] puts on an op row, so a
/// client that reached this layer from the tree and a client that asked for it by
/// id get one answer. A layer that is not an op carries no `node` at all, for the
/// reason it carries no `op_name`: nothing generated it, so there is no node to
/// name.
fn op_layer_json(stage: &Stage, layer: &LayerEntry, scans: &[LayerScan]) -> Value {
    let mut answer = json!({
        "id": layer.id,
        "kind": layer.kind,
        "description": layer.description,
        "specs": layer_spec_rows(stage, &layer.id, scans),
    });
    if layer.op {
        answer["op_name"] = json!(layer.op_name);
        answer["input"] = json!(layer.op_input);
        answer["inputs"] = json!(layer
            .inputs
            .iter()
            .map(|i| json!({
                "slot": i.slot,
                "value": i.value,
                "asset": i.asset,
                "prim": i.prim,
            }))
            .collect::<Vec<_>>());
        answer["node"] = json!(layer.node);
    }
    answer
}

/// Every spec in a layer, with its type. Read as a whole so later scans need no
/// live borrow.
fn layer_specs(stage: &Stage, layer: &str) -> Vec<(sdf::Path, SpecType)> {
    with_layer(stage, layer, |borrow| {
        let data = borrow.data();
        data.spec_paths()
            .into_iter()
            .filter_map(|path| data.spec_type(&path).map(|ty| (path, ty)))
            .collect()
    })
    .unwrap_or_default()
}

/// The rows of `layer{id}` (PROTOCOL v1.5.5): one row per spec the layer
/// authors, in the layer's own spec order (`spec_paths` is sorted, so the same
/// layer answers the same list, byte for byte).
///
/// A row is a **prim-like** spec: the pseudo-root, a prim spec, a variant set or
/// a variant. A property spec is an opinion *on* its owning prim — the prim's row
/// lists it by name in `properties` — so it is not a row of its own: that is what
/// keeps a layer of two hundred instances and their transforms a list of two
/// hundred lines rather than thousands.
///
/// A row's `spec_type` is a prim spec's **specifier** (`def` / `over` /
/// `class`), because that is the word a layer writes, and `pseudo_root` /
/// `variant_set` / `variant` for the scaffolding specs, which have no specifier.
/// `type_name` is the type a prim or variant spec declares, `null` when it
/// declares none.
///
/// `references` names the layer a reference points at by the same rule the arcs
/// use ([`resolve_target`]): the opened layer the asset resolves to when there is
/// one, the asset path as authored when there is not.
fn layer_spec_rows(stage: &Stage, layer: &str, scans: &[LayerScan]) -> Vec<Value> {
    with_layer(stage, layer, |borrow| {
        let data = borrow.data();
        // Read every field owned while the one borrow is held: a `Ref` of the
        // layer cannot be taken twice, and a row outlives the guard.
        let specs: Vec<(sdf::Path, SpecType, Option<SdfValue>, Option<SdfValue>)> = data
            .spec_paths()
            .into_iter()
            .filter_map(|path| {
                let ty = data.spec_type(&path)?;
                let field = |key: &str| {
                    data.try_field(&path, key)
                        .ok()
                        .flatten()
                        .map(|v| v.into_owned())
                };
                Some((
                    path.clone(),
                    ty,
                    field(FieldKey::Specifier.as_str()),
                    field(FieldKey::TypeName.as_str()),
                ))
            })
            .filter(|(_, ty, _, _)| {
                matches!(
                    ty,
                    SpecType::PseudoRoot
                        | SpecType::Prim
                        | SpecType::VariantSet
                        | SpecType::Variant
                )
            })
            .collect();
        let properties: Vec<Vec<String>> = specs
            .iter()
            .map(|(path, _, _, _)| property_names(data, path))
            .collect();
        let references: Vec<Vec<(String, String)>> = specs
            .iter()
            .map(|(path, ty, _, _)| match ty {
                SpecType::Prim | SpecType::Variant => data
                    .try_field(path, FieldKey::References.as_str())
                    .ok()
                    .flatten()
                    .map(|v| v.into_owned())
                    .map(|v| {
                        reference_list(&v)
                            .into_iter()
                            .map(|(asset, prim)| {
                                (resolve_target(scans, &asset), prim.as_str().to_owned())
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                _ => Vec::new(),
            })
            .collect();
        let mut rows: Vec<Value> = Vec::with_capacity(specs.len());
        for (index, (path, ty, specifier, type_name)) in specs.into_iter().enumerate() {
            let spec_type = match ty {
                SpecType::PseudoRoot => "pseudo_root",
                SpecType::VariantSet => "variant_set",
                SpecType::Variant => "variant",
                _ => match specifier {
                    Some(SdfValue::Specifier(sdf::Specifier::Def)) => "def",
                    Some(SdfValue::Specifier(sdf::Specifier::Over)) => "over",
                    Some(SdfValue::Specifier(sdf::Specifier::Class)) => "class",
                    _ => "prim",
                },
            };
            let type_name = match type_name {
                Some(SdfValue::Token(token)) if ty != SpecType::PseudoRoot => {
                    Value::String(token.as_str().to_owned())
                }
                _ => Value::Null,
            };
            rows.push(json!({
                "path": path.as_str(),
                "spec_type": spec_type,
                "type_name": type_name,
                "properties": properties[index],
                "references": references[index].iter().map(|(layer, prim)| json!({
                    "layer": layer,
                    "prim": prim,
                })).collect::<Vec<_>>(),
            }));
        }
        rows
    })
    .unwrap_or_default()
}

/// One field of one spec, owned.
fn field_of(stage: &Stage, layer: &str, path: &sdf::Path, key: &str) -> Option<SdfValue> {
    with_layer(stage, layer, |borrow| {
        borrow
            .data()
            .try_field(path, key)
            .ok()
            .flatten()
            .map(|v| v.into_owned())
    })
    .flatten()
}

/// A spec's authored fields, in authored order, as wire pairs. The specifier is
/// reported apart (`specifier`), not as a field.
fn spec_fields(stage: &Stage, layer: &str, path: &str) -> Vec<Value> {
    let Ok(path) = sdf::path(path) else {
        return Vec::new();
    };
    with_layer(stage, layer, |borrow| {
        let data = borrow.data();
        if data.spec_type(&path).is_none() {
            return Vec::new();
        }
        data.list_fields(&path)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|name| {
                let value = data.try_field(&path, &name).ok().flatten()?;
                Some(json!({ "name": name, "value": sdf_to_json(&value) }))
            })
            .collect()
    })
    .unwrap_or_default()
}

/// The spec type a layer authors at `path`, if any.
fn spec_type_of(stage: &Stage, layer: &str, path: &sdf::Path) -> Option<SpecType> {
    with_layer(stage, layer, |borrow| borrow.data().spec_type(path)).flatten()
}

/// Which layers a composed prim **composes from**, strongest first: the layers
/// whose specs contribute to it, in the order composition applies them.
///
/// This is `Prim::prim_stack`, not a scan of each layer's own spec list, and the
/// difference is the whole point (v1.8): under a reference, the contributing spec
/// sits in the target layer at a **different path**. `/World/Clusters/i000000/i000009`
/// is authored as `/World/Boxes/i000009` inside `cluster.usda`, and that spec is
/// itself under a reference to `prototypes.usda`'s `/World/Prototypes/Box`. Only
/// the composed stack knows that; a layer's own `spec_paths` never mentions the
/// stage path at all.
///
/// A layer outside the stage is simply not in the answer, and a prim no layer
/// authors answers empty — the two are not distinguished, because a client can
/// tell neither apart in the tree either.
fn authored_in(stage: &Stage, path: &str) -> Vec<String> {
    let Ok(path) = sdf::path(path) else {
        return Vec::new();
    };
    let Ok(prim) = stage.prim(&path) else {
        return Vec::new();
    };
    let Ok(sites) = prim.prim_stack() else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for site in sites {
        if !out.contains(&site.layer) {
            out.push(site.layer);
        }
    }
    out
}

/// One prim's opinions, strongest layer first: what the inspector shows for a
/// prim node.
///
/// An opinion is a spec's own fields — the prim spec's `typeName`, `kind`, and
/// so on — plus one row per property the spec authors, named by the property and
/// carrying its default. An override a gizmo wrote lives on a child attribute
/// spec, so reading only the prim spec's fields would report the session layer as
/// saying nothing about a prim it moved. Child-list bookkeeping
/// (`primChildren`, `propertyChildren`, `variantSetChildren`) and the specifier
/// are left out: they describe the layer's structure, and the specifier is
/// reported apart.
///
/// Each row is read **at the path the spec actually occupies in its own layer**
/// (v1.8), which under a reference is not the composed path. That is what makes a
/// nested asset's prim detail say something: the spec for
/// `/World/Clusters/i000000/i000009` lives in `cluster.usda` at
/// `/World/Boxes/i000009`, so reading `cluster.usda` at the composed path would find
/// nothing and report an empty opinion list. `Prim::prim_stack` pairs each
/// contributing layer with its own path, which is exactly the translation needed.
fn opinions(stage: &Stage, path: &str) -> Vec<Value> {
    let Ok(wanted) = sdf::path(path) else {
        return Vec::new();
    };
    let Ok(prim) = stage.prim(&wanted) else {
        return Vec::new();
    };
    let Ok(sites) = prim.prim_stack() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for site in sites {
        let spec = with_layer(
            stage,
            &site.layer,
            |borrow| -> Option<(Vec<Value>, &'static str)> {
                let data = borrow.data();
                let at = &site.path;
                let specifier = match data
                    .try_field(at, FieldKey::Specifier.as_str())
                    .ok()
                    .flatten()
                    .map(|v| v.into_owned())
                {
                    Some(SdfValue::Specifier(sdf::Specifier::Def)) => "def",
                    Some(SdfValue::Specifier(sdf::Specifier::Over)) => "over",
                    Some(SdfValue::Specifier(sdf::Specifier::Class)) => "class",
                    _ => "",
                };
                let mut fields = Vec::new();
                for name in data.list_fields(at).unwrap_or_default() {
                    if BOOKKEEPING.contains(&name.as_str()) || name == FieldKey::Specifier.as_str()
                    {
                        continue;
                    }
                    let Some(value) = data.try_field(at, &name).ok().flatten() else {
                        continue;
                    };
                    fields.push(json!({ "name": name, "value": sdf_to_json(&value) }));
                }
                // A property spec is an opinion on its prim: the override a
                // gizmo writes is one of these, not a field of the prim spec.
                for property in property_names(data, at) {
                    let Ok(property_path) = at.append_property(property.as_str()) else {
                        continue;
                    };
                    let default = data
                        .try_field(&property_path, FieldKey::Default.as_str())
                        .ok()
                        .flatten();
                    let value = match &default {
                        Some(value) => sdf_to_json(value),
                        // A property with no default still exists: say so, rather
                        // than dropping the row and implying the layer is silent.
                        None => Value::Null,
                    };
                    fields.push(json!({ "name": property, "value": value }));
                }
                Some((fields, specifier))
            },
        );
        let Some(Some((fields, specifier))) = spec else {
            continue;
        };
        out.push(json!({
            "layer": site.layer,
            "specifier": specifier,
            // The spec's path in its own layer, which differs from the composed
            // path under a reference. Reported so a client can tell a nested asset's
            // opinion from a same-namespace one without re-deriving the arc chain.
            "path": site.path.as_str(),
            "fields": fields,
        }));
    }
    out
}

/// The child-list fields a layer keeps to describe its own structure. They say
/// nothing about the prim's composed value, so an opinion does not report them.
const BOOKKEEPING: &[&str] = &["primChildren", "propertyChildren", "variantSetChildren"];

/// The property names a prim or variant spec lists as children.
fn property_names(data: &dyn sdf::AbstractData, at: &sdf::Path) -> Vec<String> {
    match data
        .try_field(at, ChildrenKey::PropertyChildren.as_str())
        .ok()
        .flatten()
        .map(|v| v.into_owned())
    {
        Some(SdfValue::TokenVec(names)) => names.iter().map(|t| t.as_str().to_owned()).collect(),
        _ => Vec::new(),
    }
}

/// One arc's detail: its ends plus the target prim spec's authored fields, which
/// is what a viewer shows when an arc is clicked.
fn arc_detail(stage: &Stage, arc: &ArcEntry) -> Value {
    json!({
        "kind_type": "arc",
        "id": arc.id,
        "kind": arc.kind,
        "source_layer": arc.source_layer,
        "source_prim": arc.source_prim,
        "target_layer": arc.target_layer,
        "target_prim": arc.target_prim,
        "target_spec_fields": spec_fields(stage, &arc.target_layer, &arc.target_prim),
    })
}

// --- arcs -------------------------------------------------------------------

/// The edges one USD file carries, read by opening that file as a stage of its own.
///
/// A file an operator wrote is not a layer of the stage the tree was built from, so
/// [`scan_arcs`] never saw it. Its references are still facts about it — `instances.rusd`
/// references `boxes.rusd` from every instance prim — and a row that said otherwise would
/// be reporting the stage's reach as the file's content.
///
/// The arcs have the shape the stage's arcs have, with `source_layer` rewritten to the file
/// that carries them (a freshly opened stage's root identifier is a resolved spelling of
/// the same file). A target is resolved against `layers` — exactly, then by file name —
/// because the file writes it the way an author did (`boxes.rusd`) and the tree names the
/// layers by identifier.
fn file_arcs(path: &str, layers: &[LayerEntry]) -> Vec<ArcEntry> {
    let Ok(stage) = openusd::usd::Stage::open(path) else {
        return Vec::new();
    };
    let Some(root) = stage.layer_stack().into_iter().next() else {
        return Vec::new();
    };
    let scans = vec![LayerScan {
        id: root.clone(),
        resolved: Some(path.to_owned()),
        specs: layer_specs(&stage, &root),
    }];
    scan_arcs(&stage, &scans)
        .into_iter()
        .map(|mut arc| {
            arc.source_layer = path.to_owned();
            let target = &arc.target_layer;
            if let Some(layer) = layers.iter().find(|l| {
                same_file(&l.id, target) || file_name(&l.id) == file_name(target)
            }) {
                arc.target_layer = layer.id.clone();
            }
            arc
        })
        .collect()
}

fn scan_arcs(stage: &Stage, scans: &[LayerScan]) -> Vec<ArcEntry> {
    let mut arcs: Vec<ArcEntry> = Vec::new();
    for scan in scans {
        for (path, ty) in &scan.specs {
            match ty {
                // A layer's sublayers: the layer-level edge of the stack.
                SpecType::PseudoRoot => {
                    let Some(SdfValue::StringVec(paths)) =
                        field_of(stage, &scan.id, path, FieldKey::SubLayers.as_str())
                    else {
                        continue;
                    };
                    for asset in paths {
                        let target = resolve_target(scans, &asset);
                        // **USD graph, and no edge of the tool's own making** (v1.9). The
                        // composed root is how a picture is assembled, not a node the
                        // document wrote: its sublayers are the document's lowered layer
                        // (that row *is* the root) and the cook's products, and the boxes
                        // in those products are what the document's generator nodes
                        // instantiated — they are the op rows beside the root, not
                        // references it makes. Drawing them as arcs said the root
                        // referenced the geometry it cooked (the user's question: root
                        // 为什么要 reference？不就是实例化出来的 boxes 吗).
                        if target.ends_with("rusd_root.rusd")
                            || target
                                .rsplit(['/', '\\'])
                                .next()
                                .is_some_and(|base| base.starts_with("rusd_op_"))
                        {
                            continue;
                        }
                        let kind = "sublayer";
                        arcs.push(ArcEntry {
                            id: format!("arc:{}", arcs.len()),
                            kind,
                            source_layer: scan.id.clone(),
                            source_prim: "/".to_owned(),
                            target_layer: target,
                            target_prim: "/".to_owned(),
                        });
                    }
                }
                // A prim's arcs. Inherits and specializes name a prim in the
                // same layer stack; a reference and a payload name a layer.
                SpecType::Prim | SpecType::Variant => {
                    if let Some(listed) =
                        field_of(stage, &scan.id, path, FieldKey::InheritPaths.as_str())
                    {
                        for target in path_list(&listed) {
                            arcs.push(ArcEntry {
                                id: format!("arc:{}", arcs.len()),
                                kind: "inherit",
                                source_layer: scan.id.clone(),
                                source_prim: path.as_str().to_owned(),
                                target_layer: scan.id.clone(),
                                target_prim: target.as_str().to_owned(),
                            });
                        }
                    }
                    if let Some(listed) =
                        field_of(stage, &scan.id, path, FieldKey::References.as_str())
                    {
                        for (asset, prim) in reference_list(&listed) {
                            arcs.push(ArcEntry {
                                id: format!("arc:{}", arcs.len()),
                                kind: "reference",
                                source_layer: scan.id.clone(),
                                source_prim: path.as_str().to_owned(),
                                target_layer: resolve_target(scans, &asset),
                                target_prim: prim.as_str().to_owned(),
                            });
                        }
                    }
                    if let Some(listed) =
                        field_of(stage, &scan.id, path, FieldKey::Payload.as_str())
                    {
                        for (asset, prim) in payload_list(&listed) {
                            arcs.push(ArcEntry {
                                id: format!("arc:{}", arcs.len()),
                                kind: "payload",
                                source_layer: scan.id.clone(),
                                source_prim: path.as_str().to_owned(),
                                target_layer: resolve_target(scans, &asset),
                                target_prim: prim.as_str().to_owned(),
                            });
                        }
                    }
                    if let Some(listed) =
                        field_of(stage, &scan.id, path, FieldKey::Specializes.as_str())
                    {
                        for target in path_list(&listed) {
                            arcs.push(ArcEntry {
                                id: format!("arc:{}", arcs.len()),
                                kind: "specialize",
                                source_layer: scan.id.clone(),
                                source_prim: path.as_str().to_owned(),
                                target_layer: scan.id.clone(),
                                target_prim: target.as_str().to_owned(),
                            });
                        }
                    }
                }
                // A variant is a branch of its prim: one edge per variant, so
                // the tree can hang the unselected ones beside the selected one.
                SpecType::VariantSet => {
                    let (Some(set), Some(prim)) = (path.variant_set_name(), path.parent()) else {
                        continue;
                    };
                    let Some(SdfValue::TokenVec(names)) =
                        field_of(stage, &scan.id, path, ChildrenKey::VariantChildren.as_str())
                    else {
                        continue;
                    };
                    for name in names {
                        let Ok(variant) = prim.append_variant_selection(set, name.as_str()) else {
                            continue;
                        };
                        arcs.push(ArcEntry {
                            id: format!("arc:{}", arcs.len()),
                            kind: "variant",
                            source_layer: scan.id.clone(),
                            source_prim: prim.as_str().to_owned(),
                            target_layer: scan.id.clone(),
                            target_prim: variant.as_str().to_owned(),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    arcs
}

/// The layer an asset path names: the opened layer with that exact identifier,
/// else one the resolver spelled differently, else the path as authored.
fn resolve_target(scans: &[LayerScan], asset: &str) -> String {
    // Spelling-insensitive: a layer's identifier and an arc's authored asset path are the
    // same file written by two hands — the stage holds the resolved spelling
    // (`\\?\C:\…\prototypes.rusd`) and the document the one it authored
    // (`C:/…/prototypes.rusd`). Compared as strings, they never match, and every material
    // layer then lost its arc: the row fell back to `sublayer` and the edge pointed at a
    // layer that is not in the list (v1.9).
    if let Some(scan) = scans.iter().find(|s| same_file(&s.id, asset)) {
        return scan.id.clone();
    }
    if let Some(scan) = scans
        .iter()
        .find(|s| s.resolved.as_deref().is_some_and(|r| same_file(r, asset)))
    {
        return scan.id.clone();
    }
    let file = file_name(asset);
    if !file.is_empty() {
        for scan in scans {
            if file_name(scan.resolved.as_deref().unwrap_or(&scan.id)) == file {
                return scan.id.clone();
            }
        }
    }
    asset.to_owned()
}

/// Whether two spellings name one file: `\\?\` dropped, separators unified, case folded.
fn same_file(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        s.strip_prefix(r"\\?\")
            .unwrap_or(s)
            .replace('\\', "/")
            .to_ascii_lowercase()
    }
    a == b || norm(a) == norm(b)
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

// --- prims and variant sets -------------------------------------------------

fn scan_namespace(
    stage: &Stage,
    scans: &[LayerScan],
    layers: &[LayerEntry],
) -> (Vec<PrimEntry>, Vec<VariantSetEntry>) {
    let mut paths: Vec<String> = Vec::new();
    let _ = stage.traverse(PrimPredicate::ALL, |p| paths.push(p.as_str().to_owned()));
    paths.retain(|p| !p.is_empty() && p != "/");
    paths.sort();
    paths.dedup();
    let prims: Vec<PrimEntry> = paths
        .into_iter()
        .map(|path| PrimEntry {
            authored_in: authored_in(stage, &path),
            id: format!("prim:{path}"),
            path,
        })
        .collect();

    // Variant sets, one row per `(prim, set)`: the variants any layer declares,
    // in the order the strongest declaring layer lists them, plus who authors
    // each variant's spec.
    //
    // Variant specs live in the layers that declare them, and a variant set is
    // declared on a prim spec — so the declaring layer is always one that authors
    // the prim itself. A layer scan is therefore the right source here in a way it
    // is not for `authored_in`: a variant set is an authored fact about a layer,
    // while which layers a prim *composes from* crosses arcs.
    let mut found: Vec<(String, String)> = Vec::new();
    let mut declared: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for scan in scans {
        for (path, ty) in &scan.specs {
            if *ty != SpecType::VariantSet {
                continue;
            }
            let (Some(set), Some(prim)) = (path.variant_set_name(), path.parent()) else {
                continue;
            };
            let key = (prim.as_str().to_owned(), set.to_owned());
            let entry = declared.entry(key.clone()).or_default();
            if let Some(SdfValue::TokenVec(names)) =
                field_of(stage, &scan.id, path, ChildrenKey::VariantChildren.as_str())
            {
                for name in names {
                    let name = name.as_str().to_owned();
                    if !entry.contains(&name) {
                        entry.push(name);
                    }
                }
            }
            if !found.contains(&key) {
                found.push(key);
            }
        }
    }
    let vsets: Vec<VariantSetEntry> = found
        .into_iter()
        .enumerate()
        .map(|(index, (prim, name))| {
            let variants = declared
                .remove(&(prim.clone(), name.clone()))
                .unwrap_or_default();
            let authored = variants
                .iter()
                .map(|variant| {
                    let spec = sdf::path(&prim)
                        .ok()
                        .and_then(|p| p.append_variant_selection(&name, variant).ok());
                    let by: Vec<String> = match &spec {
                        Some(spec) => layers
                            .iter()
                            .filter(|layer| {
                                spec_type_of(stage, &layer.id, spec) == Some(SpecType::Variant)
                            })
                            .map(|layer| layer.id.clone())
                            .collect(),
                        None => Vec::new(),
                    };
                    (variant.clone(), by)
                })
                .collect();
            VariantSetEntry {
                id: format!("vset:{index}"),
                selection: composed_selection(stage, &prim, &name),
                prim,
                name,
                variants,
                authored,
            }
        })
        .collect();
    (prims, vsets)
}

/// The selection composition has in effect for `set` on `prim`.
fn composed_selection(stage: &Stage, prim: &str, set: &str) -> Option<String> {
    let path = sdf::path(prim).ok()?;
    let prim = stage.prim(path).ok()?;
    prim.variant_sets()
        .get_all_variant_selections()
        .ok()?
        .into_iter()
        .find(|(name, _)| name == set)
        .map(|(_, selection)| selection)
}

// --- list-op fields ---------------------------------------------------------

/// A path list-op's contributing items in strength order: prepended first, then
/// explicit (which replaces the weaker list entirely), then added, then appended.
/// Deleted items contribute nothing.
fn path_list(value: &SdfValue) -> Vec<sdf::Path> {
    let SdfValue::PathListOp(op) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    out.extend(op.prepended_items.iter().cloned());
    if op.explicit {
        out.extend(op.explicit_items.iter().cloned());
    }
    out.extend(op.added_items.iter().cloned());
    out.extend(op.appended_items.iter().cloned());
    out
}

fn reference_list(value: &SdfValue) -> Vec<(String, sdf::Path)> {
    let SdfValue::ReferenceListOp(op) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    out.extend(op.prepended_items.iter().cloned());
    if op.explicit {
        out.extend(op.explicit_items.iter().cloned());
    }
    out.extend(op.added_items.iter().cloned());
    out.extend(op.appended_items.iter().cloned());
    out.into_iter()
        .map(|r| (r.asset_path, r.prim_path))
        .collect()
}

fn payload_list(value: &SdfValue) -> Vec<(String, sdf::Path)> {
    let SdfValue::PayloadListOp(op) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    out.extend(op.prepended_items.iter().cloned());
    if op.explicit {
        out.extend(op.explicit_items.iter().cloned());
    }
    out.extend(op.added_items.iter().cloned());
    out.extend(op.appended_items.iter().cloned());
    out.into_iter()
        .map(|p| (p.asset_path, p.prim_path))
        .collect()
}

// --- values on the wire -----------------------------------------------------

/// An `sdf` value as JSON. A value with no direct JSON shape is written as its
/// `Debug` form rather than an invented one: the panel shows what the layer
/// says, and a value it cannot render is still named.
pub fn sdf_to_json(value: &SdfValue) -> Value {
    use SdfValue as S;
    match value {
        S::None | S::Opaque | S::ValueBlock | S::Value => Value::Null,
        S::Bool(v) => json!(v),
        S::BoolVec(v) => json!(v),
        S::Uchar(v) => json!(v),
        S::UcharVec(v) => json!(v),
        S::Int(v) => json!(v),
        S::IntVec(v) => json!(v),
        S::Uint(v) => json!(v),
        S::UintVec(v) => json!(v),
        S::Int64(v) => json!(v),
        S::Int64Vec(v) => json!(v),
        S::Uint64(v) => json!(v),
        S::Uint64Vec(v) => json!(v),
        S::Float(v) => json!(v),
        S::FloatVec(v) => json!(v),
        S::Double(v) => json!(v),
        S::DoubleVec(v) => json!(v),
        S::String(v) => json!(v),
        S::StringVec(v) => json!(v),
        S::Token(v) => json!(v.as_str()),
        S::TokenVec(v) => json!(v.iter().map(|t| t.as_str().to_owned()).collect::<Vec<_>>()),
        S::AssetPath(v) => json!(v.as_str()),
        S::AssetPathVec(v) => json!(v.iter().map(|p| p.as_str().to_owned()).collect::<Vec<_>>()),
        S::Vec2f(v) => json!([v[0], v[1]]),
        S::Vec2d(v) => json!([v[0], v[1]]),
        S::Vec2i(v) => json!([v[0], v[1]]),
        S::Vec3f(v) => json!([v[0], v[1], v[2]]),
        S::Vec3d(v) => json!([v[0], v[1], v[2]]),
        S::Vec3i(v) => json!([v[0], v[1], v[2]]),
        S::Vec4f(v) => json!([v[0], v[1], v[2], v[3]]),
        S::Vec4d(v) => json!([v[0], v[1], v[2], v[3]]),
        S::Vec4i(v) => json!([v[0], v[1], v[2], v[3]]),
        S::Matrix4d(m) => json!(m.0.to_vec()),
        S::Specifier(s) => json!(match s {
            sdf::Specifier::Def => "def",
            sdf::Specifier::Over => "over",
            sdf::Specifier::Class => "class",
        }),
        S::Variability(v) => json!(match v {
            sdf::Variability::Varying => "varying",
            sdf::Variability::Uniform => "uniform",
        }),
        S::PathVec(paths) => json!(
            paths
                .iter()
                .map(|p| p.as_str().to_owned())
                .collect::<Vec<_>>()
        ),
        S::VariantSelectionMap(map) => {
            let sorted: BTreeMap<&str, &str> =
                map.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            json!(sorted)
        }
        S::Dictionary(map) => {
            let sorted: BTreeMap<&str, Value> = map
                .iter()
                .map(|(k, v)| (k.as_str(), sdf_to_json(v)))
                .collect();
            json!(sorted)
        }
        S::TokenListOp(op) => json!({
            "explicit": op.explicit,
            // The items of an `explicit` list op are its *explicit_items*, not `add`:
            // an authored `references = @x.usda@` is `explicit([x])`, so dropping this
            // field reported every authored arc as an empty list (v1.8).
            "explicit_items": op.explicit_items.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
            "prepend": op.prepended_items.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
            "add": op.added_items.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
            "append": op.appended_items.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
            "delete": op.deleted_items.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
        }),
        S::StringListOp(op) => json!({
            "explicit": op.explicit,
            "explicit_items": op.explicit_items,
            "prepend": op.prepended_items,
            "add": op.added_items,
            "append": op.appended_items,
            "delete": op.deleted_items,
        }),
        S::PathListOp(op) => json!({
            "explicit": op.explicit,
            "explicit_items": op.explicit_items.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            "prepend": op.prepended_items.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            "add": op.added_items.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            "append": op.appended_items.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            "delete": op.deleted_items.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        }),
        S::ReferenceListOp(op) => json!({
            "explicit": op.explicit,
            "explicit_items": op.explicit_items.iter().map(reference_json).collect::<Vec<_>>(),
            "prepend": op.prepended_items.iter().map(reference_json).collect::<Vec<_>>(),
            "add": op.added_items.iter().map(reference_json).collect::<Vec<_>>(),
            "append": op.appended_items.iter().map(reference_json).collect::<Vec<_>>(),
            "delete": op.deleted_items.iter().map(reference_json).collect::<Vec<_>>(),
        }),
        S::PayloadListOp(op) => json!({
            "explicit": op.explicit,
            "explicit_items": op.explicit_items.iter().map(payload_json).collect::<Vec<_>>(),
            "prepend": op.prepended_items.iter().map(payload_json).collect::<Vec<_>>(),
            "add": op.added_items.iter().map(payload_json).collect::<Vec<_>>(),
            "append": op.appended_items.iter().map(payload_json).collect::<Vec<_>>(),
            "delete": op.deleted_items.iter().map(payload_json).collect::<Vec<_>>(),
        }),
        S::Payload(p) => payload_json(p),
        S::Relocates(pairs) => json!(
            pairs
                .iter()
                .map(|(from, to)| json!({"source": from.as_str(), "target": to.as_str()}))
                .collect::<Vec<_>>()
        ),
        S::TimeSamples(samples) => json!(
            samples
                .iter()
                .map(|(time, v)| json!({"time": time, "value": sdf_to_json(v)}))
                .collect::<Vec<_>>()
        ),
        S::LayerOffsetVec(offsets) => json!(
            offsets
                .iter()
                .map(|o| json!({"offset": o.offset, "scale": o.scale}))
                .collect::<Vec<_>>()
        ),
        other => json!(format!("{other:?}")),
    }
}

fn reference_json(r: &sdf::Reference) -> Value {
    json!({ "asset": r.asset_path, "path": r.prim_path.as_str() })
}

fn payload_json(p: &sdf::Payload) -> Value {
    json!({ "asset": p.asset_path, "path": p.prim_path.as_str() })
}

