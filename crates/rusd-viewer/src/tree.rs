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

/// One layer of the authority stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerNode {
    /// The layer identifier, which is also this node's id.
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
    /// The op's **output layer** identifier, as the server joined it to this entry
    /// (`CookOp.layer_id == layers[].id`, v1.5.1).
    ///
    /// `None` when the join is missing; the reader falls back to the layer's own id,
    /// which is the contract's fallback (`output` = 条目 id).
    pub output: Option<String>,
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
                    output: optional_string(l, "output"),
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

    /// The layer `id` names, when it is one of this tree's.
    pub fn layer(&self, id: &str) -> Option<&LayerNode> {
        self.layers.iter().find(|l| l.id == id)
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
    /// `{input} -> {output summary}`), or `None` for every row that has none.
    ///
    /// Carried on the row, like `op`, so the panel draws the subtitle from the row it
    /// already draws rather than re-deriving which rows are ops.
    pub secondary: Option<String>,
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
/// arcs and its variant sets.
///
/// The shape, in the order the panel draws it (v1.3, `docs/PROTOCOL.md` 末节 —
/// **no prim rows**: a stacked city has hundreds, and a prim is inspected by picking
/// it in the viewport, which resolves its path to a `prim_nodes[].id`):
///
/// ```text
/// <layer id>  [kind, strength]        ← a root, strongest first (session on top)
///   ↳ arc:<n>  kind → target_layer:target_prim
///   ↳ vset:<n> name = selection       ← structure, so it stays
///       ↳ variant name
/// ```
///
/// A layer row that is a **cook op** (v1.4) is drawn with `op: true` and a `≤` marker,
/// so the panel can tell a preview control from a plain structural row. From v1.5 its
/// title is the generator's schema name ([`op_row_title`]) and its subtitle reads
/// `{input} → {output summary}` ([`op_row_subtitle`]); the identifier that
/// `preview{upto}` takes is still the row's `id`, and its elided form is the subtitle's
/// second half.
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
    tree.draw_order()
        .into_iter()
        .map(|layer| layer_subtree(tree, layer))
        .collect()
}

/// One layer's branch: its arcs and the prims it authors.
fn layer_subtree(tree: &CompositionTree, layer: &LayerNode) -> TreeNode {
    let mut children: Vec<TreeNode> = Vec::new();

    for arc in tree.arcs_from(&layer.id) {
        // A leaf naming the target layer (see `roots`): the edge is visible, and
        // the branch it points at is not copied.
        let target_row = if arc.crosses_layers() {
            vec![TreeNode {
                id: arc.target_layer.clone(),
                kind: TreeNodeKind::Layer,
                label: format!("→ {}", arc.target_layer),
                prim: None,
                op: false,
                root: false,
                secondary: None,
                children: Vec::new(),
            }]
        } else {
            Vec::new()
        };
        children.push(TreeNode {
            id: arc.id.clone(),
            kind: TreeNodeKind::Arc,
            label: format!("{} → {}:{}", arc.kind, arc.target_layer, arc.target_prim),
            prim: Some(arc.source_prim.clone()),
            op: false,
            root: false,
            secondary: None,
            children: target_row,
        });
    }

    // **No prim rows.** v1.3 (user ruling, `docs/PROTOCOL.md`): a city stack has
    // hundreds of prims and listing them ``既撑爆面板宽度又没人读`` — the tree is
    // about structure (layers, arcs, variants). A prim is inspected by picking it in
    // the viewport, which turns the picked path into `prim_nodes[].id` and asks
    // `comp_node{id}` (`CompositionTree::prim_id`). The payload still *carries*
    // `prim_nodes`, and the variant sets below are keyed by the prim they belong to,
    // so the rows a prim owns are still reachable — only the path rows are gone.
    for prim in tree.prims_in(&layer.id) {
        let kids: Vec<TreeNode> = tree
            .variants_of(&prim.path)
            .into_iter()
            .map(|vset| TreeNode {
                id: vset.id.clone(),
                kind: TreeNodeKind::VariantSet,
                label: match &vset.selection {
                    Some(sel) => format!("{} = {sel}", vset.name),
                    None => format!("{} (none)", vset.name),
                },
                prim: Some(prim.path.clone()),
                op: false,
                root: false,
                secondary: None,
                children: vset
                    .variants
                    .iter()
                    .map(|v| TreeNode {
                        // A variant's own spec is not a `comp_node` node — only its
                        // *set* is. The row is therefore a label, and its id is the
                        // set's, so clicking it asks the same question the set does
                        // rather than one the server would refuse.
                        id: vset.id.clone(),
                        kind: TreeNodeKind::VariantSet,
                        label: v.clone(),
                        prim: Some(prim.path.clone()),
                        op: false,
                        root: false,
                        secondary: None,
                        children: Vec::new(),
                    })
                    .collect(),
            })
            .collect();
        if !kids.is_empty() {
            children.extend(kids);
        }
    }

    TreeNode {
        id: layer.id.clone(),
        kind: TreeNodeKind::Layer,
        label: if layer.op {
            // A preview control (v1.4): the row's title is what says *which* op this
            // is, and v1.5 makes that the generator's own schema name instead of an
            // anonymous layer identifier; `≤` is the marker that says clicking it
            // previews *up to* here rather than inspecting it. The identifier is not
            // lost: it is the row's subtitle's second half (`op_row_subtitle`), and
            // it is what `preview{upto}` takes.
            format!(
                "≤ {} [{} · {}/{}]",
                op_row_title(layer),
                layer.kind,
                layer.strength,
                tree.layers.len()
            )
        } else {
            format!(
                "{} [{} · {}/{}]",
                layer.id,
                layer.kind,
                layer.strength,
                tree.layers.len()
            )
        },
        prim: None,
        op: layer.op,
        root: is_root_layer(layer),
        secondary: layer.op.then(|| op_row_subtitle(layer)),
        children,
    }
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

/// The word an op row's subtitle prints where the op reads no geometry: `input`
/// null means the generator is driven by its own parameters instead
/// (`docs/PROTOCOL.md` v1.5.1).
pub const PARAMETRIC: &str = "(parametric)";

/// How many characters of an identifier an op row's subtitle shows before eliding
/// it: v1.5.1's 「output 摘要 = identifier 截断 ≤ 20 字符 + …」.
pub const OUTPUT_SUMMARY_CHARS: usize = 20;

/// An op row's **title**: the generator's schema name (`RusdSamplePoints`,
/// `RusdInstances`, `RusdScatter`), or the layer's own identifier when the server
/// sent no `op_name`.
///
/// The fallback is the pre-v1.5 label, so a server from before this field existed
/// draws exactly the rows it used to (`docs/PROTOCOL.md` v1.5.1: `op_name` null =>
/// 回退 anon identifier).
pub fn op_row_title(layer: &LayerNode) -> &str {
    layer.op_name.as_deref().unwrap_or(layer.id.as_str())
}

/// An op row's **subtitle**: `{input} → {output summary}` (v1.5.1).
///
/// `input` is `None` for a generator with no input geometry, printed as
/// [`PARAMETRIC`]; the output half is [`output_summary`] of the op's output layer.
pub fn op_row_subtitle(layer: &LayerNode) -> String {
    format!(
        "{} → {}",
        layer.input.as_deref().unwrap_or(PARAMETRIC),
        output_summary(layer)
    )
}

/// An op's output identifier, elided to [`OUTPUT_SUMMARY_CHARS`] characters.
///
/// The output layer's own id is the fallback when the server's join to
/// `CookOp.layer_id` was missing (`docs/PROTOCOL.md` v1.5.1: join 缺失 =>
/// `output` = 条目 id). Counted in **characters**, not bytes, so a non-ASCII
/// identifier is elided at a boundary a reader can see.
pub fn output_summary(layer: &LayerNode) -> String {
    elide(layer.output.as_deref().unwrap_or(layer.id.as_str()))
}

/// [`OUTPUT_SUMMARY_CHARS`] characters, then `…` when there was more.
pub fn elide(id: &str) -> String {
    let mut out: String = id.chars().take(OUTPUT_SUMMARY_CHARS).collect();
    if id.chars().count() > OUTPUT_SUMMARY_CHARS {
        out.push('…');
    }
    out
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

/// One layer's opinion about a prim: its specifier and the fields it authors.
#[derive(Clone, Debug, PartialEq)]
pub struct Opinion {
    pub layer: String,
    /// `def` / `over` / `class` (empty when the spec carries none).
    pub specifier: String,
    pub fields: Vec<NamedValue>,
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
    pub fn lines(&self) -> Vec<String> {
        match self {
            NodeDetail::Layer(l) => {
                let mut out = vec![
                    format!("layer {}", l.identifier),
                    format!(
                        "{} · strength {} · {} specs",
                        l.kind, l.strength, l.spec_count
                    ),
                ];
                let mut body: Vec<String> = l.usda_text.lines().map(str::to_owned).collect();
                out.append(&mut body);
                out
            }
            NodeDetail::Arc(a) => {
                let mut out = vec![
                    format!("arc {} ({})", a.id, a.kind),
                    format!(
                        "{}:{} → {}:{}",
                        a.source_layer, a.source_prim, a.target_layer, a.target_prim
                    ),
                    format!("target spec fields: {}", a.target_spec_fields.len()),
                ];
                for f in &a.target_spec_fields {
                    out.push(format!("  {} = {}", f.name, f.value_text()));
                }
                out
            }
            NodeDetail::Prim(p) => {
                let mut out = vec![
                    format!("prim {}", p.path),
                    format!(
                        "{} opinions · {} arcs out · {} variant sets",
                        p.opinions.len(),
                        p.arcs_out.len(),
                        p.variant_sets.len()
                    ),
                ];
                for o in &p.opinions {
                    out.push(format!("  {} ({})", o.layer, o.specifier));
                    for f in &o.fields {
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
                    out.push(format!("  {variant} authored in {}", layers.join(", ")));
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
mod tests {
    use super::*;

    /// A small stack shaped like the city's: a session layer, the root, a
    /// sublayer, and a reference into a prototypes layer with a variant set.
    fn sample() -> Value {
        json!({
            "layers": [
                {"id": "anon:session", "kind": "session", "strength": 0},
                {"id": "city.rusd.cooked.usda", "kind": "root", "strength": 1},
                {"id": "prototypes.usda", "kind": "reference", "strength": 2},
            ],
            "arcs": [
                {"id": "arc:0", "kind": "subLayer", "source_layer": "city.rusd.cooked.usda",
                 "source_prim": "/", "target_layer": "city.rusd.cooked.usda",
                 "target_prim": "/"},
                {"id": "arc:1", "kind": "reference", "source_layer": "city.rusd.cooked.usda",
                 "source_prim": "/World/City/Block_01",
                 "target_layer": "prototypes.usda", "target_prim": "/Prototypes/Block"},
            ],
            "prim_nodes": [
                {"id": "prim:/", "path": "/", "authored_in": ["city.rusd.cooked.usda"]},
                {"id": "prim:/World/City/Block_01", "path": "/World/City/Block_01",
                 "authored_in": ["city.rusd.cooked.usda"]},
                {"id": "prim:/Prototypes/Block", "path": "/Prototypes/Block",
                 "authored_in": ["prototypes.usda"]},
                {"id": "prim:/World/City/Block_01/over", "path": "/World/City/Block_01/over",
                 "authored_in": ["anon:session", "city.rusd.cooked.usda"]},
            ],
            "variant_sets": [
                {"id": "vset:0", "prim": "/Prototypes/Block", "name": "size",
                 "variants": ["small", "large"], "selection": "small"},
            ],
        })
    }

    #[test]
    fn the_wire_answer_parses_into_a_tree() {
        let tree = CompositionTree::parse(&sample()).expect("the contract's shape");
        assert_eq!(tree.layers.len(), 3);
        assert_eq!(tree.arcs.len(), 2);
        assert_eq!(tree.prims.len(), 4);
        assert_eq!(tree.variants.len(), 1);
        assert_eq!(tree.layers[0].kind, "session");
        assert_eq!(tree.arcs[1].kind, "reference");
        assert!(tree.arcs[1].crosses_layers());
        assert!(!tree.arcs[0].crosses_layers());
        assert_eq!(tree.variants[0].selection.as_deref(), Some("small"));
    }

    #[test]
    fn the_roots_are_the_layer_stack_strongest_first() {
        let tree = CompositionTree::parse(&sample()).unwrap();
        let roots = roots(&tree);
        let ids: Vec<&str> = roots.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            ["anon:session", "city.rusd.cooked.usda", "prototypes.usda"],
            "the session layer is the strongest, so it is the first root"
        );
        // Every root is a layer row, and v1.3 draws no prim rows under them.
        assert!(roots.iter().all(|r| r.kind == TreeNodeKind::Layer));
        // (v1.3) The session layer authors a prim in this sample, but the prim's row
        // is not drawn — only the layers, arcs and variant sets are. The session
        // branch therefore has no children of its own here.
        assert_eq!(
            roots[0].children.len(),
            0,
            "v1.3: the session layer's prim is inspected by picking it, not listed"
        );
    }

    #[test]
    fn an_arc_is_a_child_of_its_source_layer_pointing_at_its_target() {
        let tree = CompositionTree::parse(&sample()).unwrap();
        let root_branch = &roots(&tree)[1];
        let arc = root_branch
            .children
            .iter()
            .find(|c| c.kind == TreeNodeKind::Arc && c.id == "arc:1")
            .expect("the reference arc is under the layer that authors it");
        assert_eq!(
            arc.label, "reference → prototypes.usda:/Prototypes/Block",
            "an arc row says where it points"
        );
        assert_eq!(
            arc.prim.as_deref(),
            Some("/World/City/Block_01"),
            "clicking an arc is also selecting the prim it is authored on"
        );
        // The target is a row **naming** the layer, not a copy of its branch: the
        // layer is already a root, and the city's 434 prims must not be drawn once
        // per path into it.
        let target = arc.children.first().expect("the target layer's row");
        assert_eq!(target.id, "prototypes.usda");
        assert_eq!(target.kind, TreeNodeKind::Layer);
        assert!(target.children.is_empty(), "a naming row, not a subtree");
        assert_eq!(target.label, "→ prototypes.usda");
    }

    #[test]
    fn a_branch_reached_by_many_arcs_is_drawn_once() {
        // Two layers reference the same target. The prims under it must appear once,
        // not once per arc: the tree is a panel over 434 prims, and repeating a
        // layer's whole branch per incoming edge is how it stops being usable.
        let many = json!({
            "layers": [
                {"id": "root.usda", "kind": "root", "strength": 0},
                {"id": "a.usda", "kind": "reference", "strength": 1},
                {"id": "b.usda", "kind": "reference", "strength": 2},
                {"id": "shared.usda", "kind": "reference", "strength": 3},
            ],
            "arcs": [
                {"id": "arc:0", "kind": "reference", "source_layer": "a.usda",
                 "source_prim": "/A", "target_layer": "shared.usda", "target_prim": "/S"},
                {"id": "arc:1", "kind": "reference", "source_layer": "b.usda",
                 "source_prim": "/B", "target_layer": "shared.usda", "target_prim": "/S"},
            ],
            "prim_nodes": [
                {"id": "prim:/S", "path": "/S", "authored_in": ["shared.usda"]},
            ],
            "variant_sets": [],
        });
        let tree = CompositionTree::parse(&many).unwrap();
        let roots = roots(&tree);
        let shared_rows = flatten(&roots)
            .into_iter()
            .filter(|(_, n)| n.id == "prim:/S")
            .count();
        // v1.3 draws no prim rows at all, so the shared prim contributes none — the
        // property this test protects (a shared target is not repeated per incoming
        // edge) now holds trivially for prims, and still has to hold for the arcs.
        assert_eq!(shared_rows, 0, "v1.3 draws no prim rows");
        // Both arcs still exist as rows, each pointing at it.
        let arc_rows = flatten(&roots)
            .into_iter()
            .filter(|(_, n)| n.kind == TreeNodeKind::Arc)
            .count();
        assert_eq!(arc_rows, 2, "both edges are drawn");
    }

    #[test]
    fn the_variant_set_stays_even_though_its_prim_row_does_not() {
        // v1.3: prim rows are gone from the tree (`docs/PROTOCOL.md` 末节), but a
        // variant set is *structure* and must still be drawn and expandable —
        // otherwise dropping prim rows would silently drop variants too.
        let tree = CompositionTree::parse(&sample()).unwrap();
        let roots = roots(&tree);
        let rows = flatten(&roots);
        assert!(
            !rows.iter().any(|(_, n)| n.kind == TreeNodeKind::Prim),
            "no prim rows at all"
        );
        let vset = rows
            .into_iter()
            .find(|(_, n)| n.kind == TreeNodeKind::VariantSet)
            .expect("the variant set is drawn");
        assert_eq!(vset.1.label, "size = small", "the selection is in the row");
        assert_eq!(
            vset.1
                .children
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            ["small", "large"],
            "the variants are expandable rows"
        );
        assert_eq!(
            vset.1.prim.as_deref(),
            Some("/Prototypes/Block"),
            "and the row still knows which prim it belongs to, for the detail panel"
        );
    }

    #[test]
    fn a_reference_cycle_in_the_layer_graph_does_not_recurse_forever() {
        let cyclic = json!({
            "layers": [
                {"id": "a.usda", "kind": "root", "strength": 0},
                {"id": "b.usda", "kind": "reference", "strength": 1},
            ],
            "arcs": [
                {"id": "arc:0", "kind": "reference", "source_layer": "a.usda",
                 "source_prim": "/A", "target_layer": "b.usda", "target_prim": "/B"},
                {"id": "arc:1", "kind": "reference", "source_layer": "b.usda",
                 "source_prim": "/B", "target_layer": "a.usda", "target_prim": "/A"},
            ],
            "prim_nodes": [],
            "variant_sets": [],
        });
        let tree = CompositionTree::parse(&cyclic).unwrap();
        let roots = roots(&tree);
        // Every layer is a root and an arc's target is a naming row, so a cycle in
        // the layer graph costs two arc rows with one layer row under each — never a
        // walk that follows the cycle.
        let rows = flatten(&roots);
        assert_eq!(
            rows.len(),
            6,
            "two layer roots, two arcs, two target rows: {rows:?}"
        );
        assert!(row_count(&roots) < 10);
    }

    #[test]
    fn a_tree_that_is_not_the_contracts_shape_is_refused_whole() {
        let missing = json!({"layers": [], "arcs": [], "prim_nodes": []});
        let e = CompositionTree::parse(&missing).expect_err("no variant_sets");
        assert!(e.contains("variant_sets"), "{e}");

        let wrong = json!({"layers": [{"id": "x", "kind": "root"}], "arcs": [],
                           "prim_nodes": [], "variant_sets": []});
        let e = CompositionTree::parse(&wrong).expect_err("a layer with no strength");
        assert!(e.contains("strength"), "{e}");

        assert!(CompositionTree::parse(&json!([1, 2, 3])).is_err());
    }

    #[test]
    fn a_method_not_found_answer_degrades_instead_of_failing() {
        let mut model = TreeModel::new();
        let e = model
            .apply(Err((METHOD_NOT_FOUND, "method not found".to_owned())))
            .expect_err("there is no tree");
        assert!(e.contains("合成树不可用"), "{e}");
        assert!(model.is_unavailable());
        assert!(model.tree().is_none());
        assert!(model.roots().is_empty(), "a degraded panel draws no rows");
        assert!(
            model.status_zh().contains("server 版本"),
            "{}",
            model.status_zh()
        );
        assert!(
            model.status_zh().contains("method not found"),
            "{}",
            model.status_zh()
        );
    }

    #[test]
    fn another_error_is_reported_with_its_code() {
        let mut model = TreeModel::new();
        let e = model
            .apply(Err((INVALID_PARAMS, "no scene open".to_owned())))
            .expect_err("no tree");
        assert!(e.contains("-32602") || e.contains("no scene open"), "{e}");
        assert!(matches!(model, TreeModel::Failed(_)));
        assert!(model.is_unavailable());
    }

    #[test]
    fn a_good_answer_leaves_the_model_ready_and_the_row_counts_agree() {
        let mut model = TreeModel::new();
        let tree = model.apply(Ok(sample())).expect("the tree parses");
        assert_eq!(tree.layers.len(), 3);
        assert!(model.tree().is_some());
        assert!(!model.is_unavailable());
        assert!(model.status_zh().contains("3 层"), "{}", model.status_zh());
        let roots = model.roots();
        assert_eq!(roots.len(), 3, "one root per layer");
        assert!(row_count(&roots) > roots.len());
    }

    #[test]
    fn a_layers_row_copies_nothing_and_a_picked_prim_copies_its_path() {
        // v1.3: no prim rows, so the clipboard's prim source is the viewport pick.
        // What must not change is the rule itself (v1.1): a prim copies its path, a
        // layer identifier copies nothing.
        let tree = CompositionTree::parse(&sample()).unwrap();
        let roots = roots(&tree);
        let layer_row = TreeSelection::of(&roots[0]);
        assert_eq!(
            clipboard_for(&layer_row),
            None,
            "a layer identifier is not a prim path"
        );
        // The picked prim arrives as a selection with a prim path on it.
        let picked = TreeSelection {
            id: tree
                .prim_id("/World/City/Block_01/over")
                .unwrap()
                .to_owned(),
            kind: TreeNodeKind::Prim,
            prim: Some("/World/City/Block_01/over".to_owned()),
        };
        assert_eq!(
            clipboard_for(&picked).as_deref(),
            Some("/World/City/Block_01/over")
        );
    }

    #[test]
    fn a_selection_path_names_its_ancestors_so_a_panel_can_open_them() {
        let tree = CompositionTree::parse(&sample()).unwrap();
        let roots = roots(&tree);
        let trail = path_to(&roots, "vset:0").expect("the variant set is in the tree");
        assert_eq!(trail.first().map(String::as_str), Some("prototypes.usda"));
        assert_eq!(trail.last().map(String::as_str), Some("vset:0"));
        // v1.3: layer → variant set, with no prim row in between.
        assert_eq!(trail.len(), 2, "layer → variant set: {trail:?}");
        assert_eq!(path_to(&roots, "nope"), None);
    }

    #[test]
    fn the_layer_details_text_is_the_panels_text() {
        let detail = NodeDetail::parse(&json!({
            "kind_type": "layer",
            "id": "city.usda",
            "identifier": "city.usda",
            "kind": "root",
            "strength": 1,
            "usda_text": "#usda 1.0\n(\n)\n\ndef Xform \"World\"\n{\n}\n",
            "spec_count": 3,
        }))
        .expect("a layer detail");
        let NodeDetail::Layer(l) = &detail else {
            panic!("expected a layer, got {detail:?}");
        };
        assert_eq!(l.spec_count, 3);
        assert_eq!(detail.prim(), None);
        let lines = detail.lines();
        assert_eq!(lines[0], "layer city.usda");
        assert!(
            lines.iter().any(|l| l.contains("def Xform")),
            "the usda text is shown: {lines:?}"
        );
    }

    #[test]
    fn an_arc_detail_lists_the_target_specs_fields() {
        let detail = NodeDetail::parse(&json!({
            "kind_type": "arc",
            "id": "arc:1",
            "kind": "reference",
            "source_layer": "city.usda",
            "source_prim": "/World/City/Block_01",
            "target_layer": "prototypes.usda",
            "target_prim": "/Prototypes/Block",
            "target_spec_fields": [
                {"name": "typeName", "value": "Xform"},
                {"name": "xformOp:translate", "value": [1.0, 2.0, 0.0]},
            ],
        }))
        .expect("an arc detail");
        let NodeDetail::Arc(a) = &detail else {
            panic!("expected an arc, got {detail:?}");
        };
        assert_eq!(a.target_spec_fields.len(), 2);
        assert_eq!(a.target_spec_fields[1].value_text(), "[1.0,2.0,0.0]");
        assert_eq!(detail.prim(), Some("/Prototypes/Block"));
    }

    #[test]
    fn a_prim_detail_keeps_the_opinions_in_the_servers_strength_order() {
        let detail = NodeDetail::parse(&json!({
            "kind_type": "prim",
            "id": "prim:/World/City/Block_01",
            "path": "/World/City/Block_01",
            "opinions": [
                {"layer": "anon:session", "specifier": "over",
                 "fields": [{"name": "xformOp:translate", "value": [3.0, 0.0, 7.0]}]},
                {"layer": "city.rusd.cooked.usda", "specifier": "def",
                 "fields": [{"name": "typeName", "value": "Xform"}]},
            ],
            "arcs_out": ["arc:1"],
            "variant_sets": ["vset:0"],
        }))
        .expect("a prim detail");
        let NodeDetail::Prim(p) = &detail else {
            panic!("expected a prim, got {detail:?}");
        };
        assert_eq!(
            p.opinions[0].layer, "anon:session",
            "the session layer is strongest"
        );
        assert_eq!(p.opinions[0].specifier, "over");
        assert_eq!(p.opinions[1].specifier, "def");
        let lines = detail.lines();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("xformOp:translate = [3.0,0.0,7.0]")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_variant_set_detail_shows_its_selection_and_per_variant_layers() {
        let detail = NodeDetail::parse(&json!({
            "kind_type": "variant_set",
            "id": "vset:0",
            "prim": "/Prototypes/Block",
            "name": "size",
            "variants": ["small", "large"],
            "selection": "large",
            "per_variant": [
                {"variant": "small", "authored_in": ["prototypes.usda"]},
                {"variant": "large", "authored_in": ["anon:session", "prototypes.usda"]},
            ],
        }))
        .expect("a variant set detail");
        let NodeDetail::VariantSet(v) = &detail else {
            panic!("expected a variant set, got {detail:?}");
        };
        assert_eq!(v.selection.as_deref(), Some("large"));
        assert_eq!(v.per_variant[1].1.len(), 2);
        let lines = detail.lines();
        assert!(
            lines.iter().any(|l| l.contains("selection: large")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("large authored in anon:session")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_detail_of_an_unknown_kind_is_refused() {
        let e = NodeDetail::parse(&json!({"kind_type": "nonsense", "id": "x"}))
            .expect_err("unknown kind");
        assert!(e.contains("nonsense"), "{e}");
        assert!(NodeDetail::parse(&json!("a string")).is_err());
    }

    #[test]
    fn the_two_requests_spell_the_contract() {
        assert_eq!(tree_params(), json!({}));
        let line = crate::protocol::request_line(9, "comp_node", comp_node_params("prim:/World"));
        assert_eq!(
            line,
            "{\"id\":9,\"method\":\"comp_node\",\"params\":{\"id\":\"prim:/World\"}}\n"
        );
    }

    #[test]
    fn the_arc_end_lookup_is_keyed_by_source_layer() {
        let tree = CompositionTree::parse(&sample()).unwrap();
        let ends = arc_ends(&tree);
        let (target, arc_id) = ends.get("city.rusd.cooked.usda").expect("the root's arc");
        assert_eq!(target, "prototypes.usda");
        assert_eq!(
            arc_id, "arc:1",
            "the later arc replaces the earlier one for one source"
        );
    }

    // --- v1.5: an op row says which op it is, what it reads, what it writes ----

    /// A stack with the two ops v1.5 puts in the mesh -> points -> instances chain:
    /// the sample reads a mesh, the instances op reads the points the first wrote.
    ///
    /// The strengths are the **real** ones (v1.5.2): a cook step inserts its output at
    /// position 0 of the root's sublayer stack, so the later op cooks, the *stronger*
    /// it is — `RusdInstances` at 2 above `RusdSamplePoints` at 3, exactly as a live
    /// server reports it. Drawn in plain strength order the chain would read backwards.
    fn op_sample() -> Value {
        json!({
            "layers": [
                {"id": "anon:session", "kind": "session", "strength": 0},
                {"id": "mesh.usda", "kind": "root", "strength": 1},
                {"id": "op:instances/1", "kind": "sublayer", "strength": 2,
                 "op": true, "op_name": "RusdInstances",
                 "input": "/World/Mesh/points", "output": "op:instances/1"},
                {"id": "op:sample_points/0", "kind": "sublayer", "strength": 3,
                 "op": true, "op_name": "RusdSamplePoints",
                 "input": "/World/Mesh", "output": "op:sample_points/0"},
            ],
            "arcs": [],
            "prim_nodes": [],
            "variant_sets": [],
        })
    }

    #[test]
    fn an_op_layer_carries_its_generator_and_its_ends() {
        let tree = CompositionTree::parse(&op_sample()).expect("parses");
        let sample = tree.layer("op:sample_points/0").expect("the sample op");
        assert!(sample.op, "still a preview control (v1.4)");
        assert_eq!(sample.op_name.as_deref(), Some("RusdSamplePoints"));
        assert_eq!(sample.input.as_deref(), Some("/World/Mesh"));
        assert_eq!(sample.output.as_deref(), Some("op:sample_points/0"));
        // A non-op layer sends none of the three, and reads as none: absence is not
        // an error and nothing is invented (the contract says the field is off them).
        let root = tree.layer("mesh.usda").expect("the root layer");
        assert_eq!(root.op_name, None);
        assert_eq!(root.input, None);
        assert_eq!(root.output, None);
    }

    #[test]
    fn an_op_rows_title_is_the_generator_and_its_subtitle_the_two_ends() {
        let tree = CompositionTree::parse(&op_sample()).unwrap();
        let rows = roots(&tree);
        let by_id = |id: &str| {
            rows.iter()
                .find(|r| r.id == id)
                .cloned()
                .unwrap_or_else(|| panic!("no row {id}: {:?}", rows.iter().map(|r| &r.id)))
        };

        let sample = by_id("op:sample_points/0");
        assert!(
            sample.label.starts_with("≤ RusdSamplePoints"),
            "{}",
            sample.label
        );
        assert_eq!(
            sample.secondary.as_deref(),
            Some("/World/Mesh → op:sample_points/0"),
            "the subtitle reads input -> output"
        );
        // The identifier is what the click sends, and the click still sends it: the
        // label change did not touch the wire (v1.4 unchanged).
        assert_eq!(sample.id, "op:sample_points/0");

        // A structural row has no subtitle at all.
        assert_eq!(by_id("mesh.usda").secondary, None);
        assert_eq!(by_id("anon:session").secondary, None);
    }

    #[test]
    fn an_op_with_no_input_reads_as_parametric() {
        let tree = CompositionTree::parse(&json!({
            "layers": [
                {"id": "city.rusd.cooked.usda", "kind": "root", "strength": 0},
                // The parametric scatter: `input` is the contract's explicit null.
                {"id": "rusd_scatter", "kind": "sublayer", "strength": 1, "op": true,
                 "op_name": "RusdScatter", "input": null, "output": "rusd_scatter"},
            ],
            "arcs": [], "prim_nodes": [], "variant_sets": [],
        }))
        .expect("parses");
        let row = roots(&tree).into_iter().find(|r| r.op).expect("the op row");
        assert_eq!(
            row.secondary.as_deref(),
            Some("(parametric) → rusd_scatter"),
            "a null input is the contract's `(parametric)`"
        );
        // And an omitted `input` is the same thing, not an error: v1.5.1's join is
        // additive, so a missing field falls back exactly like a null one.
        assert_eq!(
            op_row_subtitle(&LayerNode {
                id: "op".to_owned(),
                kind: "sublayer".to_owned(),
                strength: 1,
                op: true,
                op_name: Some("RusdScatter".to_owned()),
                input: None,
                output: None,
            }),
            "(parametric) → op"
        );
    }

    #[test]
    fn a_pre_v15_answer_keeps_the_old_label_and_no_subtitle() {
        // The whole point of tying this to the field's presence: a server that does
        // not send `op_name` draws the rows it always drew, identifier and all.
        let old = json!({
            "layers": [
                {"id": "anon:session", "kind": "session", "strength": 0},
                {"id": "rusd_city", "kind": "sublayer", "strength": 1, "op": true},
            ],
            "arcs": [], "prim_nodes": [], "variant_sets": [],
        });
        let tree = CompositionTree::parse(&old).expect("parses");
        let row = roots(&tree).into_iter().find(|r| r.op).expect("the op row");
        assert_eq!(
            row.label, "≤ rusd_city [sublayer · 1/2]",
            "the pre-v1.5 label, character for character"
        );
        assert_eq!(
            row.secondary.as_deref(),
            Some("(parametric) → rusd_city"),
            "the ends still read: the layer's own id is the output fallback"
        );
        assert_eq!(row.id, "rusd_city");
    }

    #[test]
    fn an_output_identifier_is_elided_at_twenty_characters() {
        assert_eq!(elide("short"), "short");
        let twenty = "0123456789abcdefghij";
        assert_eq!(twenty.chars().count(), OUTPUT_SUMMARY_CHARS);
        assert_eq!(elide(twenty), twenty, "exactly the budget is not elided");
        assert_eq!(
            elide("0123456789abcdefghijk"),
            "0123456789abcdefghij…",
            "one over the budget gets the ellipsis"
        );
        // Counted in characters, not bytes: a multibyte identifier is cut on a
        // character boundary and the result is still valid text.
        let wide = "\u{89e3}\u{6790}\u{5c42}".repeat(10);
        let cut = elide(&wide);
        assert_eq!(cut.chars().count(), OUTPUT_SUMMARY_CHARS + 1);
        assert!(cut.ends_with('…'), "{cut}");
    }

    #[test]
    fn the_output_half_is_the_servers_output_and_the_layer_id_when_it_is_missing() {
        let mut layer = LayerNode {
            id: "anon:op:7".to_owned(),
            kind: "sublayer".to_owned(),
            strength: 2,
            op: true,
            op_name: Some("RusdInstances".to_owned()),
            input: Some("/World/Mesh/points".to_owned()),
            output: Some("a_really_long_output_layer_identifier".to_owned()),
        };
        assert_eq!(
            op_row_subtitle(&layer),
            "/World/Mesh/points → a_really_long_output…"
        );
        layer.output = None;
        assert_eq!(
            op_row_subtitle(&layer),
            "/World/Mesh/points → anon:op:7",
            "a missing join falls back to the entry's own id (v1.5.1)"
        );
        assert_eq!(op_row_title(&layer), "RusdInstances");
        layer.op_name = None;
        assert_eq!(
            op_row_title(&layer),
            "anon:op:7",
            "no generator name falls back to the identifier"
        );
    }

    #[test]
    fn a_wrongly_typed_op_field_does_not_cost_the_whole_report() {
        // `op_name` as a number is a server bug, not a reason to draw no tree: the
        // three fields are additive and each has a fallback, so the row falls back.
        let tree = CompositionTree::parse(&json!({
            "layers": [
                {"id": "op_layer", "kind": "sublayer", "strength": 0, "op": true,
                 "op_name": 7, "input": ["/World/Mesh"], "output": null},
            ],
            "arcs": [], "prim_nodes": [], "variant_sets": [],
        }))
        .expect("the report still parses");
        let layer = tree.layer("op_layer").expect("the layer");
        assert_eq!(layer.op_name, None);
        assert_eq!(layer.input, None);
        assert_eq!(layer.output, None);
        assert!(layer.op);
    }

    #[test]
    fn the_rows_are_in_cook_order_and_the_op_metadata_does_not_reorder_them() {
        // v1.5.2: 行序 = cook 顺序 = op 行 strength **降序** (a cook step inserts its
        // output at the top of the sublayer stack, so the later op is the stronger
        // one). The structural rows keep strength order; only the op run turns.
        //
        // The wire order is the fixture's (`instances` before `sample`); the panel's
        // is the reverse inside the op run, which is the chain's own direction.
        let tree = CompositionTree::parse(&op_sample()).unwrap();
        let rows = roots(&tree);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "anon:session",
                "mesh.usda",
                "op:sample_points/0",
                "op:instances/1"
            ],
            "the session and root stay on top in strength order; the op rows read
             mesh -> points -> instances, i.e. strength descending"
        );
        // Said in the words the panel uses: the titles top to bottom are the chain.
        let titles: Vec<String> = rows
            .iter()
            .filter(|r| r.op)
            .map(|r| op_row_title(tree.layer(&r.id).expect("a row is a layer")).to_owned())
            .collect();
        assert_eq!(titles, ["RusdSamplePoints", "RusdInstances"]);
    }

    #[test]
    fn a_run_of_ops_is_reversed_in_place_and_structural_rows_do_not_move() {
        // The contract (v1.5.2) says the op rows are reversed **per run**, not as one
        // block: a server is not obliged to keep the ops adjacent, and a structural
        // row between two runs must stay where strength puts it.
        let split = json!({
            "layers": [
                {"id": "session", "kind": "session", "strength": 0},
                {"id": "op:late", "kind": "sublayer", "strength": 1, "op": true},
                {"id": "shared.usda", "kind": "sublayer", "strength": 2},
                {"id": "op:mid", "kind": "sublayer", "strength": 3, "op": true},
                {"id": "op:early", "kind": "sublayer", "strength": 4, "op": true},
                {"id": "protos.usda", "kind": "reference", "strength": 5},
            ],
            "arcs": [], "prim_nodes": [], "variant_sets": [],
        });
        let tree = CompositionTree::parse(&split).unwrap();
        let ids: Vec<String> = roots(&tree).into_iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            [
                "session",
                "op:late",
                "shared.usda",
                "op:early",
                "op:mid",
                "protos.usda",
            ],
            "each run reverses by itself: the one-row run is unchanged and the
             two-row run turns, with the structural rows untouched"
        );
        // And the stack itself is still readable in strength order — the reversal is
        // the panel's, not a change to what the model says the stack is.
        assert_eq!(
            tree.root_layers()
                .iter()
                .map(|l| l.id.as_str())
                .collect::<Vec<_>>(),
            [
                "session",
                "op:late",
                "shared.usda",
                "op:mid",
                "op:early",
                "protos.usda"
            ],
            "`root_layers` keeps the stack in strength order"
        );
    }

    #[test]
    fn a_stack_with_no_ops_or_one_op_is_not_reordered() {
        let no_ops = json!({
            "layers": [
                {"id": "session", "kind": "session", "strength": 0},
                {"id": "root.usda", "kind": "root", "strength": 1},
                {"id": "protos.usda", "kind": "reference", "strength": 2},
            ],
            "arcs": [], "prim_nodes": [], "variant_sets": [],
        });
        let tree = CompositionTree::parse(&no_ops).unwrap();
        assert_eq!(
            roots(&tree).into_iter().map(|r| r.id).collect::<Vec<_>>(),
            ["session", "root.usda", "protos.usda"]
        );

        let one_op = json!({
            "layers": [
                {"id": "root.usda", "kind": "root", "strength": 0},
                {"id": "rusd_city", "kind": "sublayer", "strength": 1, "op": true},
            ],
            "arcs": [], "prim_nodes": [], "variant_sets": [],
        });
        let tree = CompositionTree::parse(&one_op).unwrap();
        assert_eq!(
            roots(&tree).into_iter().map(|r| r.id).collect::<Vec<_>>(),
            ["root.usda", "rusd_city"],
            "a run of one is its own reversal"
        );
    }

    #[test]
    fn only_the_root_layer_row_carries_the_root_flag() {
        // v1.5.3 (`docs/PROTOCOL.md`): the row of the layer whose `kind` is `root` is a
        // preview control, and the flag is what says so. Every other row is not one:
        // op rows have their own flag, and an arc's leaf that *names* a layer is a
        // `Layer` row that is still a selection.
        let tree = CompositionTree::parse(&json!({
            "layers": [
                {"id": "anon:session", "kind": "session", "strength": 0},
                {"id": "mesh.usda", "kind": "root", "strength": 1},
                {"id": "anon:rusd_sample_points/0", "kind": "sublayer", "strength": 2,
                 "op": true, "op_name": "RusdSamplePoints",
                 "input": "/World/Mesh", "output": "anon:rusd_sample_points/0"},
                {"id": "prototypes.usda", "kind": "reference", "strength": 3},
            ],
            "arcs": [
                {"id": "arc:0", "kind": "reference", "source_layer": "mesh.usda",
                 "source_prim": "/World/Mesh", "target_layer": "prototypes.usda",
                 "target_prim": "/Prototypes/Mesh"},
            ],
            "prim_nodes": [],
            "variant_sets": [],
        }))
        .expect("parses");
        let rows = roots(&tree);
        let by_id = |id: &str| {
            rows.iter()
                .find(|r| r.id == id)
                .unwrap_or_else(|| panic!("no row {id}"))
        };

        assert!(by_id("mesh.usda").root, "the root layer's row");
        assert!(
            !by_id("anon:session").root,
            "a session layer is not the root"
        );
        assert!(
            !by_id("anon:rusd_sample_points/0").root,
            "an op layer is a control for its own reason, not this one"
        );
        assert!(
            !by_id("prototypes.usda").root,
            "a reference layer is not the root layer"
        );
        // The arc's leaf names the target layer, so it repeats that layer's id -- and it
        // is still not the root row: the flag is the tree's, not a string comparison.
        let leaf = rows
            .iter()
            .flat_map(|r| r.children.iter())
            .flat_map(|c| c.children.iter())
            .find(|c| c.label == "→ prototypes.usda")
            .expect("the arc's leaf naming its target");
        assert_eq!(leaf.kind, TreeNodeKind::Layer);
        assert!(!leaf.op && !leaf.root, "the leaf is a plain layer row");

        // And the predicate is the contract's word for it, not a second opinion.
        assert!(is_root_layer(
            tree.layer("mesh.usda").expect("the root layer")
        ));
        assert!(!is_root_layer(
            tree.layer("anon:session").expect("the session layer")
        ));
    }
}
