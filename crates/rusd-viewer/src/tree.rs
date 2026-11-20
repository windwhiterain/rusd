//! The **composition tree** panel's model (`docs/PROTOCOL.md` v1.2, D13) — the
//! client half: what the server's `composition_tree{}` answer means, as a tree a
//! panel can draw and a click can address, and what `comp_node{id}`'s four
//! details look like once parsed.
//!
//! The tree is **USD's composition tree, not the composed prim hierarchy.** A
//! Tier A snapshot is flattened: every reference, payload, variant and
//! `instanceable` flag has already been baked away, so the structure cannot be
//! rebuilt from what the client holds. The server reads it off the authority
//! stack's `sdf` layer specs and pushes it; this module only reads the answer.
//!
//! ## What the roots are
//!
//! The panel's roots are the **layer stack in strength order** — the session layer
//! strongest (`strength` 0) down to the root layer and its sublayers, then the
//! layers an arc opened. An **arc** is an edge: from `(source layer, source prim)`
//! to `(target layer, target prim)`, drawn as a child of its source layer, so the
//! chain `root → reference → prototypes` is a path down the panel. A **prim node**
//! hangs under each layer that authors it (`authored_in`), which is what makes
//! "who says what about this prim" a matter of which branch you opened.
//!
//! ## Ids
//!
//! A layer's id is the layer identifier itself; the other three kinds are
//! `arc:<n>`, `prim:<path>`, `vset:<n>`, printed by the server and passed back to
//! `comp_node` unchanged. Nothing here invents an id or normalises one.
//!
//! ## Degrading
//!
//! A server from before v1.2 answers `-32601 method not found`. The panel then
//! says 「合成树不可用（server 版本）」 and draws nothing, which is why
//! [`TreeModel::Unavailable`] is a state of the model and not a panic at the call
//! site.

use std::collections::BTreeMap;

use serde_json::{Value, json};

/// The JSON-RPC code a server answers with when it has no such method.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// The JSON-RPC code a server answers with when the params are wrong.
pub const INVALID_PARAMS: i64 = -32602;

/// One named input an operator declared, as the server reported it: the slot's own name
/// (`points`, `prototype`, `domain`, …), what the document wrote, and the USD it points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredInput {
    pub slot: String,
    pub value: String,
    /// The layer the slot names, when it names one (`box.rusd:/World/Prototypes/Box`).
    pub asset: Option<String>,
    /// The prim the slot points at — in that layer, or in this stack.
    pub prim: Option<String>,
    /// **The hierarchy the input names, level by level, each with its USD schema**
    /// (`/World` an `Xform`, `/World/Terrain` a `Mesh`): what the operator reads, in USD's
    /// own terms.
    pub structure: Vec<(String, Option<String>)>,
}
/// One layer of the authority stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerNode {    /// The layer identifier, which is also this node's id.
    pub id: String,
    /// `root` / `session` / `sublayer` / `reference` / `payload` / `inherit` /
    /// `specialize` / `variant`.
    pub kind: String,
    /// Rank in the strength order: 0 is strongest (the session layer).
    pub strength: usize,
    /// Whether this layer is a **cook generator's output** (v1.4: `composition_tree`
    /// 的 layers 条目增 `op: true` 标记 cook generator 输出层，kind 仍 sublayer).
    ///
    /// An op layer is the one thing in the tree that can be previewed *up to*: the
    /// prefix is `root + the ops at or before this one + session overrides`. So this
    /// flag, and only this flag, decides whether a layer row is a preview control or
    /// a plain structural row.
    pub op: bool,
    /// The cook generator's schema name -- `RusdSamplePoints`, `RusdInstances`,
    /// `RusdScatter` -- from an op layer's `op_name` (`docs/PROTOCOL.md` v1.5).
    ///
    /// `None` when the answer carried no `op_name`: a pre-v1.5 server, or a stack the
    /// cook never built, where the contract's own fallback is a null
    /// (v1.5.1: join 缺失 => `op_name`/`input` null, `output` = 条目 id). A row whose
    /// `op_name` is `None` keeps its pre-v1.5 label.
    pub op_name: Option<String>,
    /// The geometry this op reads: the `input_points` prim path (v1.5.1), or `None`
    /// for a generator driven by its own parameters.
    ///
    /// The subtitle prints [`PARAMETRIC`] for `None`, which is the contract's word
    /// for exactly that.
    pub input: Option<String>,
    /// **Every named input** the operator declared (v1.12), by the document's own slot
    /// names — `points`, `prototype`, `domain`, or however many an operator has. Each
    /// names the USD it points at: a layer (`box.rusd`) or a prim of this stack.
    pub inputs: Vec<DeclaredInput>,
    /// **The named prim paths the operator's output authors** (v1.13): `outputs` in the
    /// document, as USD paths. Half of the hierarchy an operator declares.
    pub outputs: Vec<String>,
    /// The op's **output layer** identifier, as the server joined it to this entry
    /// (`CookOp.layer_id == layers[].id`, v1.5.1).
    ///
    /// `None` when the join is missing; the reader falls back to the layer's own id,
    /// which is the contract's fallback (`output` = 条目 id).
    pub output: Option<String>,
    /// The **prim the op is anchored at** (v1.8's `node` field), when the answer carried
    /// one: `RusdInstances` at `/World/Boxes` writes its instances there.
    ///
    /// It is what tells two layers of the same schema apart in the tree — both are named
    /// `RusdInstances`, and the prim says which one is which. `None` for a layer nobody sent
    /// one for (a pre-v1.8 server, or a stack the cook never built), and the name falls back
    /// to the bare schema name exactly as it did before the field existed.
    pub node: Option<String>,
    /// The layer's **authored description**: the `description` every layer entry now
    /// carries, from the annotation node its author wrote it on (`docs/PROTOCOL.md`).
    ///
    /// Not an op-only field: the root layer, the session layer, a document's sublayers
    /// and a material layer all send one when they have been described. A layer shown
    /// by its description is the user's ruling (「有 description 的节点，行和详情都把
    /// Description 放最前」), so the row's title and the detail's first line are this
    /// sentence, and the name derived from the identifier stands beside it.
    ///
    /// `None` when the answer carried none -- a server from before the field, or a
    /// layer nobody described -- and then the row and the header are exactly what they
    /// were before it existed.
    pub description: Option<String>,
}

/// One composition arc: an edge between two layers at two prims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArcNode {
    pub id: String,
    /// The arc's kind, one of [`ArcNode::KINDS`].
    pub kind: String,
    pub source_layer: String,
    pub source_prim: String,
    pub target_layer: String,
    pub target_prim: String,
}

impl ArcNode {
    /// The arc kinds v1.2 puts on the wire.
    pub const KINDS: [&'static str; 6] = [
        "reference",
        "payload",
        "inherit",
        "specialize",
        "subLayer",
        "variant",
    ];

    /// Whether this arc leaves the layer it starts in (i.e. whether the panel
    /// should draw it as pointing at another layer's branch).
    pub fn crosses_layers(&self) -> bool {
        self.source_layer != self.target_layer
    }
}

/// The key v1.5.4 groups arcs by: **the edge class**, with `source_prim` left out
/// (`docs/PROTOCOL.md` v1.5.4: the aggregation key is `(kind, source_layer,
/// target_layer, target_prim)`).
///
/// `source_prim` is exactly the one field that tells 240 instances of a reference
/// apart (`/World/Tufts/i000000` .. `i000239`), which is why it is not part of the
/// key: the class is what the panel draws once, and the instances are what an
/// opened row lists.
///
/// Ordered so grouping is deterministic, and so a group's place in the list is the
/// place its first arc had -- never a hash's order.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArcGroupKey {
    pub kind: String,
    pub source_layer: String,
    pub target_layer: String,
    pub target_prim: String,
}

impl ArcNode {
    /// This arc's [`ArcGroupKey`].
    pub fn group_key(&self) -> ArcGroupKey {
        ArcGroupKey {
            kind: self.kind.clone(),
            source_layer: self.source_layer.clone(),
            target_layer: self.target_layer.clone(),
            target_prim: self.target_prim.clone(),
        }
    }
}

/// One row's worth of arcs: every arc of one class that one layer authors.
///
/// **The wire is untouched.** `docs/PROTOCOL.md` v1.5.4 keeps reporting each arc
/// one per line ("wire 的 `arcs` 仍逐条上报（无损，其它 client 可自取）"), so
/// [`CompositionTree::arcs`] still holds all 240 and the panel's "242 arc" count is
/// still the true count. Grouping is a *drawing* decision, made here and nowhere
/// else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArcGroup<'a> {
    pub key: ArcGroupKey,
    /// The arcs of this class, in wire order.
    pub arcs: Vec<&'a ArcNode>,
}

impl<'a> ArcGroup<'a> {
    /// How many arcs the row stands for: the `N` of `xN`.
    pub fn count(&self) -> usize {
        self.arcs.len()
    }

    /// The arc a **single** row's id comes from: the first in wire order, so the id
    /// a click sends is deterministic, and it is the id this row sent before there
    /// were groups ([`ArcGroup::label`]'s N = 1 case).
    pub fn representative(&self) -> &'a ArcNode {
        self.arcs[0]
    }

    /// The prims the group's arcs are authored on, in wire order: the
    /// `/World/Tufts/i000000` .. list an opened row shows.
    ///
    /// One entry per arc and not deduplicated: two arcs of one class authored on one
    /// prim are two arcs the server reported, and the opened row must not hide one.
    pub fn source_prims(&self) -> Vec<&'a str> {
        self.arcs.iter().map(|a| a.source_prim.as_str()).collect()
    }

    /// The row's label: where the edge points, and `xN` when it stands for more than
    /// one arc (v1.5.4: a group of N >= 2 gets the suffix, N = 1 does not).
    ///
    /// `target` is the target layer's [`layer_display_name`], not its id: the label is
    /// read by a user, and the id (`anon:60:...`) is the wire's spelling of the same
    /// layer. The row's tooltip is where the identifier stays.
    pub fn label(&self, target: &str) -> String {
        let edge = format!(
            "{} \u{2192} {}:{}",
            self.key.kind, target, self.key.target_prim
        );
        match self.count() {
            1 => edge,
            n => format!("{edge} \u{00d7}{n}"),
        }
    }
}

/// The arcs authored in `layer`, **grouped** by [`ArcGroupKey`] (v1.5.4), in the
/// order each class first appears in the wire's list.
///
/// Grouping per layer rather than per tree is what keeps the rows where they were: an
/// arc is drawn under the layer that authors it, and two layers referencing one
/// target are two groups (their `source_layer` differs, so they were never one
/// class).
pub fn arc_groups<'a>(tree: &'a CompositionTree, layer: &str) -> Vec<ArcGroup<'a>> {
    let mut out: Vec<ArcGroup<'a>> = Vec::new();
    for arc in tree.arcs_from(layer) {
        let key = arc.group_key();
        match out.iter_mut().find(|g| g.key == key) {
            Some(group) => group.arcs.push(arc),
            None => out.push(ArcGroup {
                key,
                arcs: vec![arc],
            }),
        }
    }
    out
}

/// One composed prim, and the layers that author a spec for it, strongest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrimNode {
    /// `prim:<path>`.
    pub id: String,
    pub path: String,
    pub authored_in: Vec<String>,
}

/// One `(prim, set)` variant set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariantSetNode {
    pub id: String,
    pub prim: String,
    pub name: String,
    pub variants: Vec<String>,
    /// The variant composition has selected, when one is.
    pub selection: Option<String>,
}

/// A whole `composition_tree{}` answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompositionTree {
    /// The layer stack, strongest first (`strength` ascending).
    pub layers: Vec<LayerNode>,
    pub arcs: Vec<ArcNode>,
    pub prims: Vec<PrimNode>,
    pub variants: Vec<VariantSetNode>,
}

impl CompositionTree {
    /// Parse `composition_tree{}`'s result. A shape that is not the contract's is
    /// an error string, never a partially filled tree: a panel drawn from half a
    /// tree would show a structure the server never asserted.
    pub fn parse(value: &Value) -> Result<Self, String> {
        if !value.is_object() {
            return Err("composition_tree: the result is not an object".to_owned());
        }
        let layers = array(value, "layers")?
            .iter()
            .map(|l| {
                Ok(LayerNode {
                    id: string(l, "id", "layers[]")?,
                    kind: string(l, "kind", "layers[]")?,
                    strength: number(l, "strength", "layers[]")? as usize,
                    // Optional and defaulting to false: a pre-v1.4 server sends no
                    // flag, and "no op rows" is exactly right for a server whose
                    // layers cannot answer `preview`.
                    op: l.get("op").and_then(Value::as_bool).unwrap_or(false),
                    // v1.5: additive op metadata, and absent means absent. A non-op
                    // layer is not supposed to send them (the contract says so), and a
                    // pre-v1.5 server sends none, so every one of the three is optional
                    // and nothing is guessed from its absence.
                    op_name: optional_string(l, "op_name"),
                    input: optional_string(l, "input"),
                    // v1.12: **every named input** the document declared for this operator.
                    // A list, so three or n slots need no change here.
                    inputs: l
                        .get("inputs")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .map(|i| DeclaredInput {
                                    slot: i
                                        .get("slot")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_owned(),
                                    value: i
                                        .get("value")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_owned(),
                                    asset: optional_string(i, "asset"),
                                    prim: optional_string(i, "prim"),
                                    structure: i
                                        .get("structure")
                                        .and_then(Value::as_array)
                                        .map(|a| {
                                            a.iter()
                                                .filter_map(|s| {
                                                    Some((
                                                        s.get("path")
                                                            .and_then(Value::as_str)?
                                                            .to_owned(),
                                                        s.get("schema")
                                                            .and_then(Value::as_str)
                                                            .map(str::to_owned),
                                                    ))
                                                })
                                                .collect()
                                        })
                                        .unwrap_or_default(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    outputs: l
                        .get("outputs")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default(),
                    output: optional_string(l, "output"),
                    // v1.8: the op's anchor prim. Optional like its two neighbours above,
                    // and nothing is guessed from its absence.
                    node: optional_string(l, "node"),
                    // Every layer may carry one, and its absence is absence: nothing
                    // is invented for a layer nobody described.
                    description: optional_string(l, "description"),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let arcs = array(value, "arcs")?
            .iter()
            .map(|a| {
                Ok(ArcNode {
                    id: string(a, "id", "arcs[]")?,
                    kind: string(a, "kind", "arcs[]")?,
                    source_layer: string(a, "source_layer", "arcs[]")?,
                    source_prim: string(a, "source_prim", "arcs[]")?,
                    target_layer: string(a, "target_layer", "arcs[]")?,
                    target_prim: string(a, "target_prim", "arcs[]")?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let prims = array(value, "prim_nodes")?
            .iter()
            .map(|p| {
                Ok(PrimNode {
                    id: string(p, "id", "prim_nodes[]")?,
                    path: string(p, "path", "prim_nodes[]")?,
                    authored_in: strings(p, "authored_in", "prim_nodes[]")?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let variants = array(value, "variant_sets")?
            .iter()
            .map(|v| {
                Ok(VariantSetNode {
                    id: string(v, "id", "variant_sets[]")?,
                    prim: string(v, "prim", "variant_sets[]")?,
                    name: string(v, "name", "variant_sets[]")?,
                    variants: strings(v, "variants", "variant_sets[]")?,
                    selection: v
                        .get("selection")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self {
            layers,
            arcs,
            prims,
            variants,
        })
    }

    /// The layers in strength order, strongest first. The wire already sends them
    /// that way; sorting by `strength` means a panel's root order is the
    /// contract's even if a server ever emits them otherwise.
    ///
    /// This is the **stack's** order, not the panel's: the panel draws op rows in
    /// cook order, which is [`CompositionTree::draw_order`].
    pub fn root_layers(&self) -> Vec<&LayerNode> {
        let mut out: Vec<&LayerNode> = self.layers.iter().collect();
        out.sort_by_key(|l| l.strength);
        out
    }

    /// The root rows in the order the panel draws them: the stack in strength order,
    /// **with each run of cook op rows reversed** (v1.5.2).
    ///
    /// # Why the op rows run the other way
    ///
    /// A cook step inserts its output layer at position 0 of the root's sublayer
    /// stack, so **the later an op cooks, the stronger its layer is** — a smaller
    /// `strength`. Cook order is therefore strength *descending* among op rows: a real
    /// server reports the mesh → points → instances chain as `RusdInstances` (2) above
    /// `RusdSamplePoints` (3), and drawn in plain strength order the panel would show
    /// the pipeline backwards.
    ///
    /// Every other layer keeps strength order (session on top, then the root and its
    /// structural sublayers, then whatever an arc opened), which is what makes the
    /// stack readable as a stack.
    ///
    /// Reversal is per **maximal run**, not one block: a server is not obliged to keep
    /// its op layers adjacent, and a run that is not contiguous still has to come out
    /// in cook order while the structural rows between runs stay where they are. A run
    /// of one is its own reversal, so a stack with no ops, or with one, is unchanged.
    pub fn draw_order(&self) -> Vec<&LayerNode> {
        let mut out = self.root_layers();
        let mut i = 0;
        while i < out.len() {
            if !out[i].op {
                i += 1;
                continue;
            }
            let start = i;
            while i < out.len() && out[i].op {
                i += 1;
            }
            out[start..i].reverse();
        }
        out
    }

    /// Every arc whose source is `layer`.
    pub fn arcs_from(&self, layer: &str) -> Vec<&ArcNode> {
        self.arcs
            .iter()
            .filter(|a| a.source_layer == layer)
            .collect()
    }

    /// The prim nodes authored in `layer`, in path order.
    pub fn prims_in(&self, layer: &str) -> Vec<&PrimNode> {
        let mut out: Vec<&PrimNode> = self
            .prims
            .iter()
            .filter(|p| p.authored_in.iter().any(|l| l == layer))
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// The variant sets of `prim`.
    pub fn variants_of(&self, prim: &str) -> Vec<&VariantSetNode> {
        self.variants.iter().filter(|v| v.prim == prim).collect()
    }

    /// The prim node at `path` — the id `comp_node` wants, for a path the user
    /// picked in the viewport (v1.3: the tree no longer lists prims, so a click is
    /// the way to name one).
    ///
    /// `None` when this composition report does not describe that prim, which is
    /// ordinary: a flattened cook output has prims the layer specs never authored,
    /// and a report may not have arrived yet. The caller says so rather than
    /// treating it as an error.
    pub fn prim_id(&self, path: &str) -> Option<&str> {
        self.prims
            .iter()
            .find(|p| p.path == path)
            .map(|p| p.id.as_str())
    }

    /// The layer that **authors** `path`: an op layer whose declared output is that path
    /// or one of its ancestors.
    ///
    /// A consumer's `visibility` over a path is authored in the **consumer's** layer — the
    /// input itself is not replaced (v1.5.1) — so showing a named path alone means
    /// composing the layer that wrote it, not the one that hides it.
    pub fn layer_owner(&self, path: &str) -> Option<String> {
        self.layers
            .iter()
            .find(|l| {
                l.outputs
                    .iter()
                    .any(|o| path == o || path.starts_with(&format!("{o}/")))
            })
            .map(|l| l.id.clone())
    }

    /// The layer `id` names, when it is one of this tree's.
    pub fn layer(&self, id: &str) -> Option<&LayerNode> {
        self.layers.iter().find(|l| l.id == id)
    }

    /// **The one name this tree shows `id` by** -- [`layer_display_name`] over its own
    /// layer list, so a caller holding a tree does not have to pass the list itself.
    pub fn display_name(&self, id: &str) -> String {
        layer_display_name(&self.layers, id)
    }
}

/// One node of the tree, as the panel draws it: what a click selects and what
/// `comp_node{id}` is asked about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeNode {
    /// The id `comp_node` takes (`<layer>`, `arc:<n>`, `prim:<path>`, `vset:<n>`).
    pub id: String,
    /// `layer` / `arc` / `prim` / `variant_set`.
    pub kind: TreeNodeKind,
    /// What the row says beside its icon.
    pub label: String,
    /// A second, dimmer piece of text on the row (v1.5: an op row's
    /// `{input} -> {output summary}`, or the display name a row titled by its
    /// description no longer shows), or `None` for every row that has none.
    ///
    /// Carried on the row, like `op`, so the panel draws the subtitle from the row it
    /// already draws rather than re-deriving which rows are ops.
    pub secondary: Option<String>,
    /// The line the row's **hover tooltip** adds under the sentence about its click.
    ///
    /// This is where a **wire identifier** lives now that a row is titled by a display
    /// name: `anon:60:rusd_instances.usda` is what a request takes and what a log line
    /// shows, and a user who needs to quote it can hover rather than read it off every
    /// row. A row whose label already is a wire spelling (a prim path, an arc id, a
    /// variant set's name) carries none.
    pub hover: Option<String>,
    /// The prim path this node stands for, when it stands for one: a prim node's
    /// own, an arc's *target* prim, a variant set's prim. Selecting such a row is
    /// also selecting the prim in the viewport (`docs/PROTOCOL.md` v1.2: 「点选
    /// prim 节点仍自动拷贝 prim path」).
    pub prim: Option<String>,
    /// Whether this row is a **preview control**: a cook op layer (v1.4), whose click
    /// asks the server for the prefix composed up to it instead of asking `comp_node`
    /// for a detail.
    ///
    /// A flag on the row rather than a check against the tree, because the rows are
    /// what the panel draws: deciding "is this a preview control?" anywhere else makes
    /// the row and the click two opinions that can disagree.
    pub op: bool,
    /// Whether this row is the **root layer's** row -- `layers[].kind == "root"`.
    ///
    /// From v1.5.3 (`docs/PROTOCOL.md`) the root row is a **preview control too**:
    /// `preview{upto: <root layer id>}` composes `root + session overrides` and **no
    /// cook op at all**, which is the input-only picture a pipeline is drawn from.
    /// It is a flag beside [`TreeNode::op`] for the same reason: the row says what a
    /// click means, and the panel decides nothing on its own.
    pub root: bool,
    /// Whether this row is the **session layer's** row -- `layers[].kind == "session"`.
    ///
    /// A third flag beside [`TreeNode::op`] and [`TreeNode::root`], and for the same
    /// reason: from v1.8 a layer row's click asks for a composition, and the session layer
    /// is the one layer whose click asks for **none** — `preview{upto: <session>}` would
    /// compose nothing at all (every other layer is stronger) and `preview{solo: <session>}`
    /// would hide the document entirely. The panel reads this flag rather than the row's
    /// label, so a change of wording cannot silently change what a click sends, and an arc's
    /// leaf naming this layer carries it too.
    pub session: bool,
    /// Children, in the order a panel draws them.
    pub children: Vec<TreeNode>,
}

/// Which of the four kinds a row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeNodeKind {
    Layer,
    Arc,
    Prim,
    VariantSet,
}

impl TreeNodeKind {
    /// The wire's own word for the kind (`kind_type` in a `comp_node` answer).
    pub fn as_str(self) -> &'static str {
        match self {
            TreeNodeKind::Layer => "layer",
            TreeNodeKind::Arc => "arc",
            TreeNodeKind::Prim => "prim",
            TreeNodeKind::VariantSet => "variant_set",
        }
    }
}

/// Build the panel's roots: one subtree per layer, in **draw order** (the stack in
/// strength order, each run of cook op rows in cook order), and under each layer its
/// arcs and its variant sets. An arc class is one row, and the prims its arcs
/// are authored on are that row's children (v1.5.4).
///
/// The shape, in the order the panel draws it (v1.3, `docs/PROTOCOL.md` 末节 --
/// **no prim rows for the prims a layer *authors*:** a stacked city has hundreds, and
/// such a prim is inspected by picking it in the viewport, which resolves its path to
/// a `prim_nodes[].id`):
///
/// ```text
/// <layer id>  [kind, strength]        <- a root, strongest first (session on top)
///   ↳ arc:<n>  kind → target_layer:target_prim          <- one edge (N = 1)
///   ↳ arc  kind → target_layer:target_prim ×N           <- N edges of one class
///       ↳ prim:/World/Tufts/i000000                     <- the prims they author on
///       ↳ prim:/World/Tufts/i000001
///       ↳ ...
///   ↳ vset:<n> name = selection       <- structure, so it stays
///       ↳ variant name
/// ```
///
/// **An edge class is one row** (v1.5.4, `docs/PROTOCOL.md`): arcs that agree on
/// `(kind, source_layer, target_layer, target_prim)` are drawn once, with `×N` when
/// there are N >= 2 of them, and **the instances are the rows under it** -- a
/// 240-instance reference cluster has to be walkable down to one mesh, which is what
/// the user asked for after the dedup. Those instance rows are the *only* prim rows
/// the tree draws, and they are there because their group was opened, not because a
/// layer authors them.
///
/// A layer row that is a **cook op** (v1.4) is drawn with `op: true` and a `≤` marker,
/// so the panel can tell a preview control from a plain structural row. From v1.5 its
/// title is the generator's schema name ([`display_name_of`]) and its subtitle reads
/// `{input} → {output summary}` ([`op_row_subtitle`]); the identifier that
/// `preview{upto}` takes is still the row's `id`, and its elided form is the subtitle's
/// second half. A row whose layer has an authored description is titled by that instead
/// ([`layer_row_label`]), which is the user's ruling for every layer the wire describes.
///
/// The stack's **root layer** row is the *other* preview control (v1.5.3,
/// “每一档前缀都要看得到”): its click previews `root + overrides`
/// and **no cook op at all**. It is flagged [`TreeNode::root`] rather than `op`,
/// because the two prefixes are not the same thing, and the panel reads the same
/// pair of flags either way.
///
/// The rows' order is **cook order** (v1.5, as v1.5.2 fixes it): the stack in strength
/// order with each run of op rows reversed ([`CompositionTree::draw_order`]), because a
/// cook step inserts its output at the top of the sublayer stack. A panel drawn in
/// plain strength order would read the pipeline backwards.
///
/// **The arc's target layer is a row that names it, not a second copy of its
/// branch.** Every layer is already a root, so nesting the target's subtree under
/// every arc that reaches it would draw the same prims once per path into the
/// layer — for the city's 434 prims under a layer reached by three arcs, that is
/// three copies of the whole thing, and the layer graph can be deeper still. The
/// row is `kind: Layer` and carries the target's own id, so clicking it asks
/// `comp_node` about exactly the same node as the root row does; the branch is one
/// click away in the roots and is not repeated.
pub fn roots(tree: &CompositionTree) -> Vec<TreeNode> {
    let order = tree.draw_order();
    // **The document's operations first, then every USD** (v1.14): the operators are the
    // document, the USDs are what it reads and writes, and the two sets are drawn one after
    // the other rather than mixed, so a reader can walk either without picking the other's
    // rows out.
    let mut out: Vec<TreeNode> = order
        .iter()
        .copied()
        .filter(|layer| layer.op)
        .map(|layer| operation_row(tree, layer))
        .collect();
    out.extend(order.iter().copied().map(|layer| usd_row(tree, layer)));
    out
}

/// What a field row prints where the thing it names has none.
const NONE: &str = "（none）";

/// **An operation row**: the document's own four fields and nothing else (v1.14, the
/// user's ruling — 「合成树里只显示每个 operation 的 description, operator, input usd
/// path/{hierarchy}, output usd path/{hierarchy}」).
///
/// `description` is the row's title, because it is the sentence a reader scans for; the
/// other three are rows under it, labelled with the document's own keys — `operator`,
/// `input`, `output`. The row keeps `op: true`, so a click on the title is still the cook
/// stop it has been since v1.4: *what the row shows* is v1.14's ruling, *what its click
/// means* is v1.4's, and neither is derived from the other.
fn operation_row(tree: &CompositionTree, layer: &LayerNode) -> TreeNode {
    TreeNode {
        id: layer.id.clone(),
        kind: TreeNodeKind::Layer,
        label: layer_description(layer).unwrap_or_else(|| display_name_of(layer)),
        prim: None,
        op: true,
        root: false,
        session: false,
        secondary: None,
        hover: Some(format!(
            "算子 {} · 层 {}",
            layer.op_name.as_deref().unwrap_or(NONE),
            layer.id
        )),
        children: vec![
            field_row(
                "operator",
                layer.op_name.clone().unwrap_or_else(|| NONE.to_owned()),
                layer.id.clone(),
                TreeNodeKind::Prim,
                None,
            ),
            input_row(tree, layer),
            output_row(layer),
        ],
    }
}

/// **The `input` row**: the one USD this operator references, spelled the way its document
/// spells it — `volume.rusd:/World/Volume`, `box.rusd:/World/Prototypes/Box` — and, where the
/// document wrote only a prim (`/World/Boxes`), that prim's own USD in front of it
/// (`boxes.rusd:/World/Boxes`).
///
/// A row that printed the bare prim said half of what the operator reads: the file is what
/// a reader has to open, and the path after the colon is where inside it the structure is.
/// The file in front is resolved through the layer that writes that prim, so the two halves
/// are the same USD by construction.
fn input_row(tree: &CompositionTree, layer: &LayerNode) -> TreeNode {
    let mut value = layer.input.clone().unwrap_or_else(|| NONE.to_owned());
    // The wire's entries carry the resolved asset and prim, which is what a click needs: a
    // row prints the document's spelling, a click takes a layer id.
    let declared = layer.inputs.iter().find(|input| input.value == value);
    let (asset, prim) = match declared {
        Some(input) => (input.asset.clone(), input.prim.clone()),
        None => (None, None),
    };
    if spelled_file(&value).is_none()
        && value.starts_with('/')
        && let Some(file) = owner_file(tree, &value)
    {
        value = format!("{file}:{value}");
    }
    match (asset, prim) {
        // A USD of its own: the click opens that file, which is the click a material row
        // already has (v1.8).
        (Some(asset), _) => field_row("input", value, asset, TreeNodeKind::Arc, None),
        // A prim of the picture: the click renders that path alone (v1.13).
        (None, Some(prim)) => {
            let mut row = field_row(
                "input",
                value,
                format!("prim:{prim}"),
                TreeNodeKind::Arc,
                Some(prim),
            );
            row.secondary = Some("render".to_owned());
            row
        }
        (None, None) => field_row("input", value, layer.id.clone(), TreeNodeKind::Prim, None),
    }
}

/// **The `output` row**: the USD this operator writes, `file:/hierarchy`, in the document's
/// own spelling (`instances.rusd:/World/Instances`). Its click opens that file alone, so the
/// product a row names is one click from the row that names it.
fn output_row(layer: &LayerNode) -> TreeNode {
    let value = layer.output.clone().unwrap_or_else(|| {
        let file = usd_file_of(layer);
        match layer.outputs.first() {
            Some(prim) => format!("{file}:{prim}"),
            None => file,
        }
    });
    field_row("output", value, layer.id.clone(), TreeNodeKind::Arc, None)
}

/// **A USD row**: the file's path (its title), then every `reference` and `sublayer` it
/// carries — and nothing else (v1.14). A file with neither has no rows under it: a
/// `reference: （none）` line says nothing a missing line does not.
///
/// The row keeps the layer's own `op`/`root`/`session` flags, so its click still means what
/// it meant: a cook stop previews up to itself, a material previews alone, the session layer
/// only shows its detail.
fn usd_row(tree: &CompositionTree, layer: &LayerNode) -> TreeNode {
    let mut children = reference_rows(tree, layer);
    children.extend(sublayer_rows(tree, layer));
    TreeNode {
        id: layer.id.clone(),
        kind: TreeNodeKind::Layer,
        label: usd_file_of(layer),
        prim: None,
        op: layer.op,
        root: is_root_layer(layer),
        session: is_session_layer(layer),
        secondary: None,
        hover: Some(format!("层 {}", layer.id)),
        children,
    }
}

/// One **reference** the layer carries: `reference: boxes.rusd:/World/Boxes ×2400`.
///
/// One row per **distinct target**, counting the prims that carry it — a 2400-instance
/// product references one USD from 2400 prims, which is one fact about the file and not
/// 2400 rows. A single reference prints the prim that carries it instead of a count.
fn reference_rows(tree: &CompositionTree, layer: &LayerNode) -> Vec<TreeNode> {
    let mut groups: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for arc in tree.arcs_from(&layer.id) {
        if arc.kind != "reference" {
            continue;
        }
        groups
            .entry((arc.target_layer.clone(), arc.target_prim.clone()))
            .or_default()
            .push(arc.source_prim.clone());
    }
    groups
        .into_iter()
        .map(|((target, prim), sources)| {
            let value = format!("{}:{}", usd_name_of(&target), prim);
            let mut row = field_row("reference", value, target.clone(), TreeNodeKind::Arc, None);
            row.secondary = Some(match sources.len() {
                1 => sources[0].clone(),
                n => format!("×{n}"),
            });
            // A target the stage never opened has no composition to show, so the row
            // states the fact and asks nothing.
            if tree.layer(&target).is_none() {
                row.kind = TreeNodeKind::Prim;
                row.id = layer.id.clone();
            }
            row
        })
        .collect()
}

/// One **sublayer** of the layer: `sublayer: C:/…/terrain.rusd`. A sublayer edge is a
/// layer-level edge (`source_prim` is `/`), so there is nothing to count and one row per
/// edge is the fact.
fn sublayer_rows(tree: &CompositionTree, layer: &LayerNode) -> Vec<TreeNode> {
    tree.arcs_from(&layer.id)
        .into_iter()
        .filter(|arc| arc.kind == "sublayer")
        .map(|arc| {
            let mut row = field_row(
                "sublayer",
                usd_name_of(&arc.target_layer),
                arc.target_layer.clone(),
                TreeNodeKind::Arc,
                None,
            );
            if tree.layer(&arc.target_layer).is_none() {
                row.kind = TreeNodeKind::Prim;
                row.id = layer.id.clone();
            }
            row
        })
        .collect()
}

/// One field row: `<field>: <value>`, and what a click on it does is decided by the id,
/// kind and `prim` it carries — a `render` row renders one path, an `arc` id opens one
/// layer, and a row with neither is a plain selection.
fn field_row(
    field: &str,
    value: impl Into<String>,
    id: String,
    kind: TreeNodeKind,
    prim: Option<String>,
) -> TreeNode {
    TreeNode {
        id,
        kind,
        label: format!("{field}: {}", value.into()),
        prim,
        op: false,
        root: false,
        session: false,
        secondary: None,
        hover: None,
        children: Vec::new(),
    }
}

/// The **file's own name**, which is what a USD row is titled by: `instances.rusd`,
/// `terrain.rusd`, `session`. Never the wire identifier — that is the `path` row's job.
/// The file this layer is, **spelled the way its document spelled it** (`instances.rusd`,
/// `terrain.rusd`): an operator's declared `output` names its own file, and a layer no
/// document named (the session layer, a stack's material) falls back to its file name.
/// The absolute identifier is the server's spelling of the same file and is not what a row
/// prints.
fn usd_file_of(layer: &LayerNode) -> String {
    if let Some(file) = layer.output.as_deref().and_then(spelled_file) {
        return file.to_owned();
    }
    match layer.kind.as_str() {
        "root" => "root".to_owned(),
        "session" => "session".to_owned(),
        _ => usd_name_of(&layer.id),
    }
}

/// [`usd_name`] for an identifier this tree may hold no layer for — a reference's or a
/// sublayer's target is named by the file an author wrote, not by a row.
/// The file part of an authored `usd path:/hierarchy` spelling: `"boxes.rusd:/World/Boxes"`
/// → `"boxes.rusd"`, and a bare prim path (`"/World/Boxes"`) → `None`. Windows drive
/// letters survive it, because the colon that counts is the one before a `/`.
fn spelled_file(value: &str) -> Option<&str> {
    let i = value.rfind(':')?;
    if i == 0 || !value[i + 1..].starts_with('/') {
        return None;
    }
    Some(&value[..i])
}

/// The USD that writes `path`: the operator whose declared output is that path or an
/// ancestor of it, spelled as its document spelled it.
///
/// This is what completes a bare input (`input = "/World/Boxes"`): the document names a
/// structure inside the picture, and the layer that wrote that structure is the USD the row
/// has to name beside it.
fn owner_file(tree: &CompositionTree, path: &str) -> Option<String> {
    tree.layers
        .iter()
        .find(|layer| {
            layer
                .outputs
                .iter()
                .any(|out| path == out || path.starts_with(&format!("{out}/")))
        })
        .map(usd_file_of)
}

fn usd_name_of(id: &str) -> String {
    let name = strip_anon_prefix(id);
    if name.is_empty() {
        id.to_owned()
    } else {
        name
    }
}

/// A layer row's **title**: its [`display name`](display_name_of), then the facts the
/// row is read with -- `[kind · rank/total]`, and `≤` for a cook op's preview control.
///
/// A layer with an authored description is titled by it instead: the user's ruling is
/// that Description comes first, so the sentence is the title and the display name
/// moves to the subtitle ([`layer_row_secondary`]). The identifier (`anon:60:...`) is in
/// neither: it is the wire's, and the row's hover tooltip is where a reader finds it.
///
/// The `≤` marker is the op row's only (v1.4); the root row's is the panel's to add,
/// and it adds it while that row *is* the prefix in force (v1.5.3), which is a fact only
/// the panel holds.
pub fn layer_row_label(tree: &CompositionTree, layer: &LayerNode) -> String {
    let rank = format!(
        "[{} \u{00b7} {}/{}]",
        layer.kind,
        layer.strength,
        tree.layers.len()
    );
    let title = match layer_description(layer) {
        Some(description) => description,
        None => display_name_of(layer),
    };
    if layer.op {
        // **The operator's schema** (v1.13) — `RusdSamplePoints`, `RusdInstances` — is part
        // of what the row says: a reader has to be able to see which operator this is.
        match &layer.op_name {
            Some(schema) => format!("\u{2264} {title} [{schema} \u{00b7} op \u{00b7} {}/{}]", layer.strength, tree.layers.len()),
            None => format!("\u{2264} {title} {rank}"),
        }
    } else {
        format!("{title} {rank}")
    }}

/// A layer row's dim second half: its display name when the description is the title,
/// and a cook op's `{input} → {output}` after that.
///
/// `None` for a row with neither a description nor an op, which is every plain
/// sublayer and (from v1.5.1) every op the server sent no metadata for.
pub fn layer_row_secondary(tree: &CompositionTree, layer: &LayerNode) -> Option<String> {
    let named = layer_description(layer).map(|_| display_name_of(layer));
    match (named, layer.op) {
        (Some(named), true) => Some(format!(
            "{named} \u{00b7} {}",
            op_row_subtitle(layer, Some(tree))
        )),
        (Some(named), false) => Some(named),
        (None, true) => Some(op_row_subtitle(layer, Some(tree))),
        (None, false) => None,
    }
}

/// The `comp_node` id of the prim at `path`.
///
/// `prim:<path>` is the server's own spelling (`rusd-agent`'s `composition_tree`
/// emits exactly this), so a row built here asks the same question a viewport pick
/// asks: a pick resolves its path through `prim_nodes`
/// ([`CompositionTree::prim_id`]), and this is that id's shape. It is **not** an
/// invented id -- the alternative at the call sites is an arc id on a row whose
/// click means "this prim", which is a different question with a different answer.
pub fn prim_node_id(path: &str) -> String {
    format!("prim:{path}")
}

/// Whether `path` is the **pseudo-root** -- `"/"`, the stage itself.
///
/// An arc reports the pseudo-root as its `source_prim` when the edge is authored
/// *at the stage* rather than at a prim (`subLayer` always is: the root layer's
/// sublayers are composed at `/`), and the stage is not a prim an instance stands
/// for. So it gets no instance row: `prim:/` would select the whole stage.
pub fn is_pseudo_root(path: &str) -> bool {
    path == "/"
}

/// The header line of a layer section in a prim's detail: the layer, and what the tree
/// knows about it.
///
/// `{display name} [{kind} · {strength}]` -- the same name and the same facts the layer's
/// own row prints, because a section and the row it came from are one layer and a reader
/// reads one name for it.
///
/// An **op's** schema name is inside that display name from v1.8 (`{op_name} {node}`, see
/// [`display_name_of`]), not appended after the bracket: the header would otherwise say
/// `RusdInstances [sublayer · 3] RusdInstances`.
///
/// The layer's **description is not repeated** here: it has its own line above this one
/// ([`NodeDetail::lines`] prints it first, per the user's ruling), so a header that also
/// carried it would say it twice.
///
/// `None` is a layer the tree does not list. Its kind and rank would have to be
/// invented, so the header is the name the identifier gives up on its own
/// ([`strip_anon_prefix`]) and nothing else: still a name and never the `anon:N:`
/// spelling, whose `N` means nothing to a reader.
pub fn layer_header(id: &str, layer: Option<&LayerNode>) -> String {
    let Some(layer) = layer else {
        return strip_anon_prefix(id);
    };
    // **No schema name appended any more** (v1.8): the display name of an op now carries
    // `{op_name} {node}`, so appending `op_name` again would say it twice. A layer whose
    // answer sent no `node` is named by the bare schema name, which is the whole of what
    // the header said before the field existed.
    format!(
        "{} [{} \u{00b7} {}]",
        display_name_of(layer),
        layer.kind,
        layer.strength
    )
}

/// Whether `layer` is the stack's **root layer** (`kind == "root"`), whose row is
/// v1.5.3's third preview control (`docs/PROTOCOL.md`: root row becomes a preview
/// control row).
///
/// Pure and public so the row's flag and the panel's click rule can be asserted
/// against the same predicate instead of each spelling `"root"` on its own.
pub fn is_root_layer(layer: &LayerNode) -> bool {
    layer.kind == "root"
}

/// Whether `layer` is the stack's **session layer** (`kind == "session"`), the one layer
/// every composition starts from.
///
/// It is the layer whose row asks for **no** composition at all (`crate::panel::ask_for`):
/// a prefix composed up to the session layer contains nothing (every other layer is
/// stronger), and a solo of it hides the document. Reading it off `kind` here and off the
/// row's flag in the panel keeps one layer with one name and one meaning.
pub fn is_session_layer(layer: &LayerNode) -> bool {
    layer.kind == "session"
}

/// The word an op row's subtitle prints where the op reads no geometry: `input`
/// null means the generator is driven by its own parameters instead
/// (`docs/PROTOCOL.md` v1.5.1).
pub const PARAMETRIC: &str = "(parametric)";

/// How many characters of an identifier an op row's subtitle shows before eliding
/// it: v1.5.1's 「output 摘要 = identifier 截断 ≤ 20 字符 + …」.
pub const OUTPUT_SUMMARY_CHARS: usize = 20;

/// An op's **schema name** when the answer carried one (`RusdSamplePoints`,
/// `RusdInstances`, `RusdScatter`), which is what [`display_name_of`] names an op layer
/// by. `None` for a layer the server built no `op_name` for, which is a pre-v1.5 server
/// or a stack the cook never built; such a row falls back to the name its identifier
/// gives up, exactly as it did before v1.5.
pub fn op_row_schema_name(layer: &LayerNode) -> Option<&str> {
    layer.op_name.as_deref()
}

/// An op row's **subtitle**: `{input} → {output name}` (v1.5.1).
///
/// `input` is `None` for a generator with no input geometry, printed as
/// [`PARAMETRIC`]; the output half names the op's output layer by the same display name
/// its own row would show ([`output_summary`]).
/// `{input} → {output name}` (v1.5.1), the output named the way its own row names it.
pub fn op_row_subtitle(layer: &LayerNode, tree: Option<&CompositionTree>) -> String {
    format!(
        "{} → {}",
        layer.input.as_deref().unwrap_or(PARAMETRIC),
        output_summary(layer, tree)
    )
}

/// An op's output layer, **named the way the rest of the viewer names it**, then elided
/// to [`OUTPUT_SUMMARY_CHARS`] characters.
///
/// The identifier the server joined (`CookOp.layer_id`, or the entry's own id when the
/// join was missing, `docs/PROTOCOL.md` v1.5.1) is what arrives; the `anon:N:` prefix is
/// dropped and, when the tree lists that layer, its display name is used -- so an op's
/// subtitle names its output the way that output's own row does (the user's ruling: one
/// layer, one name). Counted in **characters**, not bytes, so a non-ASCII name is elided
/// at a boundary a reader can see.
pub fn output_summary(layer: &LayerNode, tree: Option<&CompositionTree>) -> String {
    let out = layer.output.as_deref().unwrap_or(layer.id.as_str());
    let named = match tree.and_then(|t| t.layer(out)) {
        Some(target) => display_name_of(target),
        None => strip_anon_prefix(out),
    };
    elide(&named)
}

/// [`OUTPUT_SUMMARY_CHARS`] characters, then `…` when there was more.
pub fn elide(id: &str) -> String {
    let mut out: String = id.chars().take(OUTPUT_SUMMARY_CHARS).collect();
    if id.chars().count() > OUTPUT_SUMMARY_CHARS {
        out.push('…');
    }
    out
}

/// **The one name a layer is shown by**, wherever this viewer names it.
///
/// The user's ruling is that the same layer must read the same everywhere -- the tree
/// row, an arc's label, a detail section's header, the top bar's preview badge -- and
/// the identifier (`anon:60:rusd_instances.usda`) is the one spelling that is *not* a
/// name: it is a wire identifier, and it is kept for the tooltip and for the request
/// that takes it. So the layer is named once, here, and every drawing site asks.
///
/// The rule, in this order:
///
/// 1. a **cook op's** output layer is named by its generator schema (`RusdInstances`),
///    because that is what says which op it is;
/// 2. the stack's **root** and the **session** layer are named by their kind
///    (`root`, `session`) -- they are the two layers whose identity is a role, and
///    they have no file name that would mean anything;
/// 3. every other layer is named by its **basename without the `anon:N:` prefix**
///    (`rusd_instances.usda`), which is the file's own name and survives every
///    renumber the server's anonymous ids go through;
/// 4. an `id` no layer answers to falls back to the id itself -- a caller naming a
///    layer the tree does not list still gets a sentence rather than nothing.
///
/// Pure over the layer list, so a row, a header and the badge cannot disagree about a
/// name: they call this with the same list and the same id.
pub fn layer_display_name(layers: &[LayerNode], id: &str) -> String {
    match layers.iter().find(|l| l.id == id) {
        Some(layer) => display_name_of(layer),
        None => id.to_owned(),
    }
}

/// [`layer_display_name`] for a layer the caller already holds, so a row does not look
/// its own layer up again.
///
/// An op is named `{op_name} {node}` when the answer carried the generator's anchor path
/// (v1.8's `node`, e.g. `/World/Boxes`): two `RusdInstances` layers are told apart by the
/// prim each one reads or writes, not by their anonymous identifiers. A server from before
/// the field sends none and the name is the bare schema name, exactly as it was.
pub fn display_name_of(layer: &LayerNode) -> String {
    if layer.op
        && let Some(name) = &layer.op_name
    {
        return match &layer.node {
            Some(node) if !node.is_empty() => format!("{name} {node}"),
            _ => name.clone(),
        };
    }
    match layer.kind.as_str() {
        "root" => "root".to_owned(),
        "session" => "session".to_owned(),
        _ => strip_anon_prefix(&layer.id),
    }
}

/// `layer`'s **authored description**, collapsed to one line: what a row or a header
/// shows first when the layer has one, and `None` when it does not.
///
/// Collapsed because a description is a sentence that may be wrapped in the layer's
/// annotation node, and a row is one line: a newline in a label would break the tree's
/// own layout. The panel's detail prints it as it arrived.
pub fn layer_description(layer: &LayerNode) -> Option<String> {
    layer.description.as_deref().map(one_line)
}

/// `text` with every run of whitespace folded to one space, and the ends trimmed.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `id` without an `anon:<n>:` prefix: the layer's own file name.
///
/// The server names an anonymous layer `anon:<n>:<file>`, the `<n>` being a per-stage
/// counter that moves whenever the stack is rebuilt. The prefix therefore carries no
/// identity at all, and dropping it is what lets one layer read the same name across a
/// rebuild. An `id` with no such prefix (an authored path, a named layer) is returned
/// unchanged, as is a bare `anon:<n>` with no file behind it.
pub fn strip_anon_prefix(id: &str) -> String {
    let Some(rest) = id.strip_prefix("anon:") else {
        // A layer opened from disk is identified by its full path; the panel
        // shows the file's own name -- `\\?\C:\...\prototypes.usda` is the path,
        // not the layer.
        let last = id.rsplit(['\\', '/']).next().unwrap_or(id);
        return if last.is_empty() {
            id.to_owned()
        } else {
            last.to_owned()
        };
    };
    let Some((n, file)) = rest.split_once(':') else {
        return id.to_owned();
    };
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) || file.is_empty() {
        return id.to_owned();
    }
    file.to_owned()
}

/// One `comp_node{id}` answer, parsed into the four shapes v1.2 defines.
#[derive(Clone, Debug, PartialEq)]
pub enum NodeDetail {
    Layer(LayerDetail),
    Arc(ArcDetail),
    Prim(PrimDetail),
    VariantSet(VariantSetDetail),
}

/// A layer's detail: its identifier, its USDA text, and how many specs it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerDetail {
    pub id: String,
    pub identifier: String,
    pub kind: String,
    pub strength: usize,
    pub usda_text: String,
    pub spec_count: usize,
    /// The layer's authored description when `comp_node` carries one, which is the
    /// same sentence `composition_tree` sends for the same layer. Absent is absent.
    pub description: Option<String>,
}

/// An arc's detail: its ends and the target prim spec's authored fields.
#[derive(Clone, Debug, PartialEq)]
pub struct ArcDetail {
    pub id: String,
    pub kind: String,
    pub source_layer: String,
    pub source_prim: String,
    pub target_layer: String,
    pub target_prim: String,
    /// `{name, value}` pairs, in the order the layer authors them.
    pub target_spec_fields: Vec<NamedValue>,
}

/// A prim's detail: every layer's opinion about it, strongest layer first.
#[derive(Clone, Debug, PartialEq)]
pub struct PrimDetail {
    pub id: String,
    pub path: String,
    pub opinions: Vec<Opinion>,
    pub arcs_out: Vec<String>,
    pub variant_sets: Vec<String>,
}

impl PrimDetail {
    /// This detail as **layer sections**, layer first: one section per layer that
    /// authors a spec for the prim, in the server's strength order (the user's ruling:
    /// 详情面板一律以「层」为主语 -- 用户要看到的是 layer).
    ///
    /// `tree` supplies the header's own facts -- a layer's `kind` and `strength`, and
    /// the generator name of a cook op's output layer. A layer `comp_node` names but the
    /// tree does not list (a report that arrived before `composition_tree`, or a stack
    /// that has since changed) still gets its section, headed by its identifier alone:
    /// the opinion is a fact, and hiding it would be worse than a plain header.
    ///
    /// The sections are built by grouping `comp_node`'s own `opinions` while the server
    /// has no `layer{id}` method (v1.5.5). `opinions` is already one entry per layer and
    /// already in strength order, so grouping is a reformat: nothing is merged and
    /// nothing re-sorted, which is what keeps "讲到哪一层就是哪一层" true by construction.
    pub fn layer_sections(&self, tree: Option<&CompositionTree>) -> Vec<LayerSection> {
        self.opinions
            .iter()
            .map(|op| {
                let known = tree.and_then(|t| t.layer(&op.layer));
                LayerSection {
                    layer: op.layer.clone(),
                    header: layer_header(&op.layer, known),
                    specifier: op.specifier.clone(),
                    fields: op.fields.clone(),
                }
            })
            .collect()
    }
}

/// One layer's opinion about a prim: its specifier and the fields it authors.
#[derive(Clone, Debug, PartialEq)]
pub struct Opinion {
    pub layer: String,
    /// `def` / `over` / `class` (empty when the spec carries none).
    pub specifier: String,
    pub fields: Vec<NamedValue>,
}

/// One **layer's section** of a prim's detail (the user's ruling: 详情面板一律以「层」
/// 为主语 -- 用户要看到的是 layer).
///
/// The title is the layer -- its identifier, its kind and its strength, plus the
/// generator's name when it is a cook op's output -- and the body is what **that
/// layer** says about the prim: the spec's own type and the attributes it authors.
///
/// This is the shape the panel draws, and the shape a test asserts against, so the two
/// cannot disagree about which layer says what. While the server has no
/// `layer{id}` method (v1.5.5), the sections are built by grouping `comp_node`'s
/// `opinions` on their `layer` field -- the wire keeps its own order, and the grouping
/// only reformats it. When that method arrives, a section's fields come from it and the
/// header keeps this spelling.
#[derive(Clone, Debug, PartialEq)]
pub struct LayerSection {
    /// The layer identifier, which is also the layer row's id.
    pub layer: String,
    /// The header line: the identifier and what the tree knows about it.
    pub header: String,
    /// `def` / `over` / `class`.
    pub specifier: String,
    /// The fields this layer authors on the prim, in the server's order.
    pub fields: Vec<NamedValue>,
}

impl LayerSection {
    /// The section's body: one line per authored attribute, `spec_type name = value`.
    pub fn lines(&self) -> Vec<String> {
        self.fields
            .iter()
            .map(|f| {
                if self.specifier.is_empty() {
                    format!("{} = {}", f.name, f.value_text())
                } else {
                    format!("{} {} = {}", self.specifier, f.name, f.value_text())
                }
            })
            .collect()
    }
}

/// A variant set's detail: its variants, the selection, and who authors each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariantSetDetail {
    pub id: String,
    pub prim: String,
    pub name: String,
    pub variants: Vec<String>,
    pub selection: Option<String>,
    /// `(variant, layers that author its spec)`, in the server's order.
    pub per_variant: Vec<(String, Vec<String>)>,
}

/// One authored field: its name and its value, kept as the JSON the server sent
/// (the value's Sdf type is the server's business, `docs/PROTOCOL.md` v1.1).
#[derive(Clone, Debug, PartialEq)]
pub struct NamedValue {
    pub name: String,
    pub value: Value,
}

impl NamedValue {
    /// The value as one line of text for the panel.
    pub fn value_text(&self) -> String {
        match &self.value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }
}

impl NodeDetail {
    /// Parse a `comp_node{id}` result. `kind_type` decides which of the four it
    /// is; a value without one is an error, because guessing would mean showing
    /// a layer's text where an arc's fields belong.
    pub fn parse(value: &Value) -> Result<Self, String> {
        if !value.is_object() {
            return Err("comp_node: the result is not an object".to_owned());
        }
        let kind = string(value, "kind_type", "comp_node")?;
        match kind.as_str() {
            "layer" => Ok(NodeDetail::Layer(LayerDetail {
                id: string(value, "id", "comp_node(layer)")?,
                identifier: string(value, "identifier", "comp_node(layer)")?,
                kind: value
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                strength: value.get("strength").and_then(Value::as_u64).unwrap_or(0) as usize,
                usda_text: value
                    .get("usda_text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                spec_count: value.get("spec_count").and_then(Value::as_u64).unwrap_or(0) as usize,
                description: optional_string(value, "description"),
            })),
            "arc" => Ok(NodeDetail::Arc(ArcDetail {
                id: string(value, "id", "comp_node(arc)")?,
                kind: string(value, "kind", "comp_node(arc)")?,
                source_layer: string(value, "source_layer", "comp_node(arc)")?,
                source_prim: string(value, "source_prim", "comp_node(arc)")?,
                target_layer: string(value, "target_layer", "comp_node(arc)")?,
                target_prim: string(value, "target_prim", "comp_node(arc)")?,
                target_spec_fields: named_values(value, "target_spec_fields", "comp_node(arc)")?,
            })),
            "prim" => Ok(NodeDetail::Prim(PrimDetail {
                id: string(value, "id", "comp_node(prim)")?,
                path: string(value, "path", "comp_node(prim)")?,
                opinions: value
                    .get("opinions")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(parse_opinion).collect::<Result<Vec<_>, _>>())
                    .transpose()?
                    .unwrap_or_default(),
                arcs_out: strings(value, "arcs_out", "comp_node(prim)")?,
                variant_sets: strings(value, "variant_sets", "comp_node(prim)")?,
            })),
            "variant_set" => Ok(NodeDetail::VariantSet(VariantSetDetail {
                id: string(value, "id", "comp_node(variant_set)")?,
                prim: string(value, "prim", "comp_node(variant_set)")?,
                name: string(value, "name", "comp_node(variant_set)")?,
                variants: strings(value, "variants", "comp_node(variant_set)")?,
                selection: value
                    .get("selection")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                per_variant: value
                    .get("per_variant")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|v| {
                                if !v.is_object() {
                                    return Err(
                                        "comp_node(variant_set): per_variant[] is not an object"
                                            .to_owned(),
                                    );
                                }
                                Ok((
                                    string(v, "variant", "per_variant[]")?,
                                    strings(v, "authored_in", "per_variant[]")?,
                                ))
                            })
                            .collect::<Result<Vec<_>, String>>()
                    })
                    .transpose()?
                    .unwrap_or_default(),
            })),
            other => Err(format!(
                "comp_node: {other:?} is not one of layer/arc/prim/variant_set"
            )),
        }
    }

    /// The prim path this detail stands for, when it stands for one.
    pub fn prim(&self) -> Option<&str> {
        match self {
            NodeDetail::Layer(_) => None,
            NodeDetail::Arc(a) => Some(&a.target_prim),
            NodeDetail::Prim(p) => Some(&p.path),
            NodeDetail::VariantSet(v) => Some(&v.prim),
        }
    }

    /// The panel's lines for this detail, so what a test asserts and what the
    /// panel draws are the same strings.
    ///
    /// `tree`, when the caller has one, is what lets every line that names a layer name
    /// it the way the rest of the viewer does ([`layer_display_name`]). Without it a
    /// line still reads a name, because the identifier alone gives one up
    /// ([`strip_anon_prefix`]); what is lost is only a kind and a rank, which no line
    /// here invents.
    pub fn lines(&self, tree: Option<&CompositionTree>) -> Vec<String> {
        let name = |id: &str| match tree {
            Some(t) => layer_display_name(&t.layers, id),
            None => strip_anon_prefix(id),
        };
        match self {
            NodeDetail::Layer(l) => {
                // **The description first** when the layer has one (the user's ruling:
                // 有 description 的节点，行和详情都把 Description 放最前): the sentence its
                // author wrote says what the layer is, and the name, kind, spec count and
                // identifier follow it. An empty description is no description.
                //
                // The tree's copy stands in when this answer carried none: both are the
                // same annotation node's text, and the row is titled by it, so a detail
                // that dropped it would disagree with the row it came from.
                let description = l
                    .description
                    .as_deref()
                    .filter(|d| !d.trim().is_empty())
                    .map(str::to_owned)
                    .or_else(|| layer_description(tree.and_then(|t| t.layer(&l.id))?));
                let mut out: Vec<String> = match &description {
                    Some(d) => d.lines().map(str::to_owned).collect(),
                    None => Vec::new(),
                };
                out.push(layer_header(&l.id, tree.and_then(|t| t.layer(&l.id))));
                out.push(format!(
                    "{} specs \u{00b7} identifier {}",
                    l.spec_count, l.identifier
                ));
                let mut body: Vec<String> = l.usda_text.lines().map(str::to_owned).collect();
                out.append(&mut body);
                out
            }
            NodeDetail::Arc(a) => {
                let mut out = vec![
                    format!("arc {} ({})", a.id, a.kind),
                    format!(
                        "{}:{} \u{2192} {}:{}",
                        name(&a.source_layer),
                        a.source_prim,
                        name(&a.target_layer),
                        a.target_prim
                    ),
                    format!("target spec fields: {}", a.target_spec_fields.len()),
                ];
                for f in &a.target_spec_fields {
                    out.push(format!("  {} = {}", f.name, f.value_text()));
                }
                out
            }
            NodeDetail::Prim(p) => {
                // **By layer, layer first** (the user's ruling): the detail says which
                // *layer* says what, not which prim has opinions. Each section is headed
                // by the layer and followed by that layer's spec line and attributes.
                //
                // The header names the layer the way its row does, so a section and the
                // row it came from read as one layer. [`PrimDetail::layer_sections`] is
                // the panel's copy of this grouping, with the fuller header.
                let mut out = vec![format!("prim {}", p.path)];
                out.push(format!(
                    "{} layers author it \u{00b7} {} arcs out \u{00b7} {} variant sets",
                    p.opinions.len(),
                    p.arcs_out.len(),
                    p.variant_sets.len()
                ));
                for op in &p.opinions {
                    out.push(format!(
                        "\u{2500}\u{2500} {} [{}]",
                        name(&op.layer),
                        op.specifier
                    ));
                    for f in &op.fields {
                        out.push(format!("    {} = {}", f.name, f.value_text()));
                    }
                }
                out
            }
            NodeDetail::VariantSet(v) => {
                let mut out = vec![
                    format!("variant set {} on {}", v.name, v.prim),
                    format!("selection: {}", v.selection.as_deref().unwrap_or("(none)")),
                    format!("variants: {}", v.variants.join(", ")),
                ];
                for (variant, layers) in &v.per_variant {
                    let named: Vec<String> = layers.iter().map(|l| name(l)).collect();
                    out.push(format!("  {variant} authored in {}", named.join(", ")));
                }
                out
            }
        }
    }
}
fn parse_opinion(v: &Value) -> Result<Opinion, String> {
    Ok(Opinion {
        layer: string(v, "layer", "opinions[]")?,
        specifier: v
            .get("specifier")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        fields: named_values(v, "fields", "opinions[]")?,
    })
}

fn named_values(value: &Value, key: &str, what: &str) -> Result<Vec<NamedValue>, String> {
    let Some(v) = value.get(key) else {
        return Ok(Vec::new());
    };
    let arr = v
        .as_array()
        .ok_or_else(|| format!("{what}: \"{key}\" is not an array"))?;
    arr.iter()
        .map(|f| {
            if !f.is_object() {
                return Err(format!("{what}: \"{key}[]\" is not an object"));
            }
            Ok(NamedValue {
                name: string(f, "name", key)?,
                value: f.get("value").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

fn array(value: &Value, key: &str) -> Result<Vec<Value>, String> {
    match value.get(key) {
        Some(Value::Array(a)) => Ok(a.clone()),
        Some(_) => Err(format!("composition_tree: \"{key}\" is not an array")),
        None => Err(format!("composition_tree: no \"{key}\"")),
    }
}

fn strings(value: &Value, key: &str, what: &str) -> Result<Vec<String>, String> {
    let Some(v) = value.get(key) else {
        return Ok(Vec::new());
    };
    let arr = v
        .as_array()
        .ok_or_else(|| format!("{what}: \"{key}\" is not an array"))?;
    arr.iter()
        .map(|s| {
            s.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{what}: \"{key}[]\" is not a string"))
        })
        .collect()
}

fn string(value: &Value, key: &str, what: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("{what}: \"{key}\" (string) required"))
}

/// A string field that may be **absent**, and whose absence means `None` rather
/// than an error (v1.5: `op_name`/`input`/`output` are additive).
///
/// An explicit JSON `null` is the same as absent here, which is what the contract
/// spells for an op with no input (`input` = null). A present but non-string value
/// is also `None`: a wrong type must not cost the whole report, since every reader
/// of these three fields has a fallback (`op_name` -> the layer's own identifier,
/// `output` -> the layer's own id).
fn optional_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn number(value: &Value, key: &str, what: &str) -> Result<u64, String> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{what}: \"{key}\" (number) required"))
}

/// What the panel knows about the tree, and the three things that can go wrong.
#[derive(Clone, Debug, PartialEq)]
pub enum TreeModel {
    /// Nothing asked yet: before `subscribe`, or before the first request.
    Idle,
    /// Asked, not answered.
    Loading,
    /// A tree arrived.
    Ready(CompositionTree),
    /// The server has no `composition_tree` (`-32601`): 「合成树不可用（server 版本）」.
    Unavailable(String),
    /// The request failed for any other reason, or the answer was not the
    /// contract's shape.
    Failed(String),
}

impl TreeModel {
    /// A model before anything was asked.
    pub fn new() -> Self {
        TreeModel::Idle
    }

    /// Take a `composition_tree` reply: `Ok(value)` is the result, `Err((code,
    /// message))` the JSON-RPC error the server answered with.
    pub fn apply(
        &mut self,
        reply: Result<Value, (i64, String)>,
    ) -> Result<&CompositionTree, String> {
        match reply {
            Ok(value) => match CompositionTree::parse(&value) {
                Ok(tree) => {
                    *self = TreeModel::Ready(tree);
                    match self {
                        TreeModel::Ready(t) => Ok(t),
                        _ => unreachable!("just set"),
                    }
                }
                Err(e) => {
                    *self = TreeModel::Failed(e.clone());
                    Err(e)
                }
            },
            Err((crate::protocol::TIMEOUT_CODE, message)) => {
                // **A timeout is not a version problem.** The server speaks v1.2 and
                // simply did not answer in time (a busy cook, a slow ssh tunnel), so
                // the panel says so and can be retried with 「刷新」 — it must not
                // claim the method does not exist, which is what a shared failure
                // arm would say.
                let text = format!("合成树：服务器未在时限内应答（{message}）");
                *self = TreeModel::Failed(text.clone());
                Err(text)
            }
            Err((METHOD_NOT_FOUND, message)) => {
                let text = unavailable_text();
                *self = TreeModel::Unavailable(if message.is_empty() {
                    text
                } else {
                    format!("{text} — {message}")
                });
                Err(match self {
                    TreeModel::Unavailable(m) => m.clone(),
                    _ => unreachable!("just set"),
                })
            }
            Err((code, message)) => {
                let text = format!("composition_tree failed: {message} (code {code})");
                *self = TreeModel::Failed(text.clone());
                Err(text)
            }
        }
    }

    /// The tree, when one is loaded.
    pub fn tree(&self) -> Option<&CompositionTree> {
        match self {
            TreeModel::Ready(t) => Some(t),
            _ => None,
        }
    }

    /// The panel's one status line: what to show above (or instead of) the tree.
    pub fn status_zh(&self) -> String {
        match self {
            TreeModel::Idle => "合成树：尚未请求".to_owned(),
            TreeModel::Loading => "合成树：请求中…".to_owned(),
            TreeModel::Ready(t) => format!(
                "合成树：{} 层 · {} arc · {} prim · {} variant set",
                t.layers.len(),
                t.arcs.len(),
                t.prims.len(),
                t.variants.len()
            ),
            TreeModel::Unavailable(m) => m.clone(),
            TreeModel::Failed(m) => format!("合成树请求失败：{m}"),
        }
    }

    /// Whether the panel must show the degraded message instead of a tree.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, TreeModel::Unavailable(_) | TreeModel::Failed(_))
    }

    /// Whether a composition report has arrived and is usable — the counterpart of
    /// `is_unavailable`, for callers that must tell "no prim node for this path"
    /// (ordinary) from "no report yet" (retry).
    pub fn ready(&self) -> bool {
        matches!(self, TreeModel::Ready(_))
    }

    /// The `comp_node` id of the prim at `path`, when the report describes it. This
    /// is the v1.3 entry point: a viewport pick gives a path, and the tree turns it
    /// into the id `comp_node` takes.
    pub fn prim_id(&self, path: &str) -> Option<&str> {
        self.tree()?.prim_id(path)
    }

    /// The layer that authors `path`, when the tree knows one.
    pub fn owner_of(&self, path: &str) -> Option<String> {
        self.tree()?.layer_owner(path)
    }
    /// The roots to draw: empty unless a tree is ready.
    pub fn roots(&self) -> Vec<TreeNode> {
        match self {
            TreeModel::Ready(t) => roots(t),
            _ => Vec::new(),
        }
    }
}

impl Default for TreeModel {
    fn default() -> Self {
        Self::new()
    }
}

/// The one sentence a server without the method gets.
pub fn unavailable_text() -> String {
    "合成树不可用（server 版本）".to_owned()
}

/// The request params for both tree methods: neither takes any.
pub fn tree_params() -> Value {
    json!({})
}

/// `comp_node`'s params: the id the tree printed.
pub fn comp_node_params(id: &str) -> Value {
    json!({ "id": id })
}

/// Where a selection sits in the tree, so the panel can keep the row open and
/// highlighted across a rebuild: the id, plus every ancestor id on the path.
pub fn path_to(root: &[TreeNode], id: &str) -> Option<Vec<String>> {
    fn walk(node: &TreeNode, id: &str, trail: &mut Vec<String>) -> bool {
        trail.push(node.id.clone());
        if node.id == id {
            return true;
        }
        for child in &node.children {
            if walk(child, id, trail) {
                return true;
            }
        }
        trail.pop();
        false
    }
    for node in root {
        let mut trail = Vec::new();
        if walk(node, id, &mut trail) {
            return Some(trail);
        }
    }
    None
}

/// Every row of a tree, flattened depth first: what a test asserts against
/// without walking the shape by hand.
pub fn flatten(root: &[TreeNode]) -> Vec<(usize, TreeNode)> {
    fn walk(node: &TreeNode, depth: usize, out: &mut Vec<(usize, TreeNode)>) {
        out.push((depth, node.clone()));
        for child in &node.children {
            walk(child, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    for node in root {
        walk(node, 0, &mut out);
    }
    out
}

/// Count the rows of a tree.
pub fn row_count(root: &[TreeNode]) -> usize {
    flatten(root).len()
}

/// A selected row, as the panel and the viewport both need it: the node's id,
/// its kind, and the prim path it stands for (so a prim row also selects the
/// prim on screen).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeSelection {
    pub id: String,
    pub kind: TreeNodeKind,
    pub prim: Option<String>,
}

impl TreeSelection {
    /// Build one from a row.
    pub fn of(node: &TreeNode) -> Self {
        Self {
            id: node.id.clone(),
            kind: node.kind,
            prim: node.prim.clone(),
        }
    }
}

/// The clipboard payload a tree click produces: a prim path for a row that
/// stands for a prim, and nothing for a layer (a layer identifier is not an id an
/// agent's tools take as a prim).
pub fn clipboard_for(sel: &TreeSelection) -> Option<String> {
    sel.prim.clone()
}

/// The `(from, to)` endpoints of every arc, as a lookup from line id to ends: the
/// one place a panel decides whether an arc leaves its layer.
pub fn arc_ends(tree: &CompositionTree) -> BTreeMap<String, (String, String)> {
    tree.arcs
        .iter()
        .map(|a| {
            (
                a.source_layer.clone(),
                (a.target_layer.clone(), a.id.clone()),
            )
        })
        .collect()
}




















#[cfg(test)]
mod rows_for_the_user {
    use super::*;
    use serde_json::json;

    /// The composition tree of a two-op pipeline, as `composition_tree{}` sends it:
    /// `instances.rusd` reads `/World/Boxes` (a prim `boxes.rusd` writes) and references
    /// `boxes.rusd` from three instance prims; `boxes.rusd` reads `box.rusd` and carries no
    /// reference of its own.
    fn mock() -> CompositionTree {
        CompositionTree::parse(&json!({
            "layers": [
                {
                    "id": "/abs/out/instances.rusd",
                    "kind": "op", "strength": 0, "op": true,
                    "description": "把一簇 box 实例化到 quad 表面",
                    "op_name": "RusdInstances",
                    "input": "/World/Boxes",
                    "output": "instances.rusd:/World/Instances",
                    "inputs": [{"slot": "prototype", "value": "/World/Boxes",
                                "asset": null, "prim": "/World/Boxes", "structure": []}],
                    "outputs": ["/World/Instances"], "node": "/World/Instances"
                },
                {
                    "id": "/abs/out/boxes.rusd",
                    "kind": "op", "strength": 1, "op": true,
                    "description": "每个点放一个 box",
                    "op_name": "RusdInstances",
                    "input": "box.rusd:/World/Prototypes/Box",
                    "output": "boxes.rusd:/World/Boxes",
                    "inputs": [{"slot": "prototype", "value": "box.rusd:/World/Prototypes/Box",
                                "asset": "/abs/box.rusd", "prim": "/World/Prototypes/Box",
                                "structure": []}],
                    "outputs": ["/World/Boxes"], "node": "/World/Boxes"
                }
            ],
            "arcs": [
                {"id": "arc:0", "kind": "reference",
                 "source_layer": "/abs/out/instances.rusd", "source_prim": "/World/Instances/i000000",
                 "target_layer": "/abs/out/boxes.rusd", "target_prim": "/World/Boxes"},
                {"id": "arc:1", "kind": "reference",
                 "source_layer": "/abs/out/instances.rusd", "source_prim": "/World/Instances/i000001",
                 "target_layer": "/abs/out/boxes.rusd", "target_prim": "/World/Boxes"},
                {"id": "arc:2", "kind": "reference",
                 "source_layer": "/abs/out/instances.rusd", "source_prim": "/World/Instances/i000002",
                 "target_layer": "/abs/out/boxes.rusd", "target_prim": "/World/Boxes"}
            ],
            "prim_nodes": [],
            "variant_sets": []
        }))
        .expect("the mock is the contract's shape")
    }

    fn find<'a>(rows: &'a [TreeNode], label: &str) -> &'a TreeNode {
        rows.iter().find(|row| row.label == label).expect(label)
    }

    /// **The user's ruling** (v1.14/v1.15): an operation row shows its four fields, an
    /// operation's `input` names the USD its prim lives in, every path is spelled the way the
    /// document spells it, and a USD with neither a reference nor a sublayer has no such row.
    #[test]
    fn the_tree_shows_the_four_operator_fields_and_the_three_usd_facts() {
        let tree = mock();
        let rows = roots(&tree);

        let op = find(&rows, "把一簇 box 实例化到 quad 表面");
        let fields: Vec<&str> = op.children.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(
            fields,
            vec![
                "operator: RusdInstances",
                // The document wrote `/World/Boxes`; the row names the USD that holds it.
                "input: boxes.rusd:/World/Boxes",
                "output: instances.rusd:/World/Instances",
            ]
        );

        // A reference is one row per distinct target, counting the prims that carry it.
        let instances = find(&rows, "instances.rusd");
        assert_eq!(instances.children.len(), 1, "{:?}", instances.children);
        assert_eq!(instances.children[0].label, "reference: boxes.rusd:/World/Boxes");
        assert_eq!(instances.children[0].secondary.as_deref(), Some("×3"));

        // A USD that references nothing and has no sublayer says nothing under it.
        let boxes = find(&rows, "boxes.rusd");
        assert!(boxes.children.is_empty(), "{:?}", boxes.children);

        // No row prints the identifier's own spelling, and none prints an absent field.
        let all: Vec<String> = flatten(&rows).into_iter().map(|(_, row)| row.label).collect();
        assert!(all.iter().all(|label| !label.contains("/abs/")));
        assert!(!all.iter().any(|label| label.contains("（none）")));
    }
}
