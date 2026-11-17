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
    pub fn root_layers(&self) -> Vec<&LayerNode> {
        let mut out: Vec<&LayerNode> = self.layers.iter().collect();
        out.sort_by_key(|l| l.strength);
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
    /// The prim path this node stands for, when it stands for one: a prim node's
    /// own, an arc's *target* prim, a variant set's prim. Selecting such a row is
    /// also selecting the prim in the viewport (`docs/PROTOCOL.md` v1.2: 「点选
    /// prim 节点仍自动拷贝 prim path」).
    pub prim: Option<String>,
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

/// Build the panel's roots: one subtree per layer, **in strength order**, and
/// under each layer its arcs and the prims it authors.
///
/// The shape, in the order the panel draws it:
///
/// ```text
/// <layer id>  [kind, strength]        ← a root, strongest first (session on top)
///   ↳ arc:<n>  kind → target_layer:target_prim
///   ↳ prim:<path>  authored_in: [this layer, …]
///       ↳ vset:<n> name = selection
///           ↳ variant name
/// ```
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
    tree.root_layers()
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
            children: target_row,
        });
    }

    for prim in tree.prims_in(&layer.id) {
        let mut kids = Vec::new();
        for vset in tree.variants_of(&prim.path) {
            kids.push(TreeNode {
                id: vset.id.clone(),
                kind: TreeNodeKind::VariantSet,
                label: match &vset.selection {
                    Some(sel) => format!("{} = {sel}", vset.name),
                    None => format!("{} (none)", vset.name),
                },
                prim: Some(prim.path.clone()),
                children: vset
                    .variants
                    .iter()
                    .map(|v| TreeNode {
                        // A variant's own spec is not a `comp_node` node in v1.2 —
                        // only its *set* is. The row is therefore a label, and its
                        // id is the set's, so clicking it asks the same question
                        // the set does rather than one the server would refuse.
                        id: vset.id.clone(),
                        kind: TreeNodeKind::VariantSet,
                        label: v.clone(),
                        prim: Some(prim.path.clone()),
                        children: Vec::new(),
                    })
                    .collect(),
            });
        }
        children.push(TreeNode {
            id: prim.id.clone(),
            kind: TreeNodeKind::Prim,
            label: prim.path.clone(),
            prim: Some(prim.path.clone()),
            children: kids,
        });
    }

    TreeNode {
        id: layer.id.clone(),
        kind: TreeNodeKind::Layer,
        label: format!(
            "{} [{} · {}/{}]",
            layer.id,
            layer.kind,
            layer.strength,
            tree.layers.len()
        ),
        prim: None,
        children,
    }
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
        // Every root is a layer row, and only the session layer has a prim of its own.
        assert!(roots.iter().all(|r| r.kind == TreeNodeKind::Layer));
        assert_eq!(
            roots[0].children.len(),
            1,
            "the session layer authors one prim in this sample"
        );
        assert_eq!(roots[0].children[0].label, "/World/City/Block_01/over");
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
        assert_eq!(shared_rows, 1, "the shared prim is drawn exactly once");
        // Both arcs still exist as rows, each pointing at it.
        let arc_rows = flatten(&roots)
            .into_iter()
            .filter(|(_, n)| n.kind == TreeNodeKind::Arc)
            .count();
        assert_eq!(arc_rows, 2, "both edges are drawn");
    }

    #[test]
    fn a_prim_hangs_under_every_layer_that_authors_it_and_a_variant_set_under_its_prim() {
        let tree = CompositionTree::parse(&sample()).unwrap();
        let roots = roots(&tree);
        let session_prims: Vec<&str> = roots[0].children.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(session_prims, ["/World/City/Block_01/over"]);

        let prototype = flatten(&roots)
            .into_iter()
            .find(|(_, n)| {
                n.prim.as_deref() == Some("/Prototypes/Block") && n.kind == TreeNodeKind::Prim
            })
            .expect("the prototype's prim row");
        let vset = prototype
            .1
            .children
            .first()
            .expect("the variant set under its prim");
        assert_eq!(vset.kind, TreeNodeKind::VariantSet);
        assert_eq!(vset.label, "size = small", "the selection is in the row");
        let names: Vec<&str> = vset.children.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            names,
            ["small", "large"],
            "the variants are expandable rows"
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
    fn a_prim_rows_path_is_what_a_click_copies_and_a_layers_row_copies_nothing() {
        let tree = CompositionTree::parse(&sample()).unwrap();
        let roots = roots(&tree);
        let prim_row = &roots[0].children[0];
        let sel = TreeSelection::of(prim_row);
        assert_eq!(sel.kind, TreeNodeKind::Prim);
        assert_eq!(
            clipboard_for(&sel).as_deref(),
            Some("/World/City/Block_01/over")
        );
        let layer_row = TreeSelection::of(&roots[0]);
        assert_eq!(
            clipboard_for(&layer_row),
            None,
            "a layer identifier is not a prim path"
        );
    }

    #[test]
    fn a_selection_path_names_its_ancestors_so_a_panel_can_open_them() {
        let tree = CompositionTree::parse(&sample()).unwrap();
        let roots = roots(&tree);
        let trail = path_to(&roots, "vset:0").expect("the variant set is in the tree");
        assert_eq!(trail.first().map(String::as_str), Some("prototypes.usda"));
        assert_eq!(trail.last().map(String::as_str), Some("vset:0"));
        assert!(trail.len() >= 3, "layer → prim → variant set: {trail:?}");
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
}
