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
//!   the strength order (0 strongest: the session layer first).
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
use serde_json::{Value, json};

/// The composition tree of one stage (`composition_tree{}`), and the details of
/// its nodes (`comp_node{id}`).
pub struct CompositionTree {
    layers: Vec<LayerEntry>,
    arcs: Vec<ArcEntry>,
    prims: Vec<PrimEntry>,
    vsets: Vec<VariantSetEntry>,
}

struct LayerEntry {
    /// The layer identifier, which is also this node's id.
    id: String,
    kind: &'static str,
    strength: usize,
    /// Whether this layer is a **cook output layer** (`docs/PROTOCOL.md`
    /// v1.4): a generator's authored sublayer, which a preview can stop the
    /// composition at. See [`cook_output_layer`].
    op: bool,
}

/// Whether `id` is a cook output layer of this stage: a layer the cook inserted
/// into the root layer's sublayer stack. A `.rusd` document authors no
/// `subLayers` (SPEC §4's v0 subset), so every sublayer of the root layer is one
/// the cook wrote — and none of them is a document layer, an arc target or the
/// session layer, which are the other kinds the tree knows.
///
/// The order is the cook's: the cook inserts each output layer at position 0 of
/// the root's stack, so the stack lists them newest first and **cook order is the
/// stack reversed**. [`cook_output_layers`] is the same rule, exported for the
/// preview slice.
fn cook_output_layer(id: &str, root_id: &str, root_sublayers: &[String]) -> bool {
    id != root_id && root_sublayers.iter().any(|s| s == id)
}

/// The cook output layers of `stage` in **cook order** (oldest first), which is
/// the order `preview{upto}` counts in (PROTOCOL v1.4). A muted layer is not in
/// the root's stack, so a superseded output layer of an earlier pass is not
/// listed.
pub fn cook_output_layers(stage: &Stage) -> Vec<String> {
    let root_id = stage.root_layer().identifier().to_owned();
    let mut ids = stage.sub_layers(&root_id);
    // `sub_layers` lists the parent first.
    if ids.first().is_some_and(|id| *id == root_id) {
        ids.remove(0);
    }
    ids.reverse();
    ids
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
    /// Build the tree of `stage`.
    pub fn build(stage: &Stage) -> Self {
        let _ = stage.traverse(PrimPredicate::ALL, |_| {});
        let root_id = stage.root_layer().identifier().to_owned();
        let session_id = stage.session_layer().map(|l| l.identifier().to_owned());

        // Strength order: the root layer stack first (session layers, then the
        // root layer and its sublayers), then everything an arc opened.
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

        let root_sublayers = stage.sub_layers(&root_id);
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
                let kind = if Some(&scan.id) == session_id.as_ref() {
                    "session"
                } else if scan.id == root_id {
                    "root"
                } else if let Some(kind) = arc_kind.get(scan.id.as_str()) {
                    kind
                } else {
                    "sublayer"
                };
                LayerEntry {
                    id: scan.id.clone(),
                    kind,
                    strength,
                    op: cook_output_layer(&scan.id, &root_id, &root_sublayers),
                }
            })
            .collect();

        let (prims, vsets) = scan_namespace(stage, &scans, &layers);
        CompositionTree {
            layers,
            arcs,
            prims,
            vsets,
        }
    }

    /// The wire shape of `composition_tree{}`.
    pub fn to_json(&self) -> Value {
        json!({
            "layers": self.layers.iter().map(|l| json!({
                "id": l.id,
                "kind": l.kind,
                "strength": l.strength,
                "op": l.op,
            })).collect::<Vec<_>>(),
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
                "opinions": opinions(stage, &self.layers, path),
                "arcs_out": self.arcs.iter()
                    .filter(|a| a.source_prim == path)
                    .map(|a| a.id.clone())
                    .collect::<Vec<_>>(),
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
                    "usda_text": borrow.export_to_string().unwrap_or_default(),
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
fn opinions(stage: &Stage, layers: &[LayerEntry], path: &str) -> Vec<Value> {
    let Ok(wanted) = sdf::path(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for layer in layers {
        // `with_layer` answers `None` for a layer the stage does not hold, and the
        // closure answers `None` for a layer that authors no spec for this prim.
        let spec = with_layer(
            stage,
            &layer.id,
            |borrow| -> Option<(Vec<Value>, &'static str)> {
                let data = borrow.data();
                let at = data.spec_paths().into_iter().find(|candidate| {
                    candidate == &wanted
                        || (matches!(
                            data.spec_type(candidate),
                            Some(SpecType::Prim | SpecType::Variant)
                        ) && candidate.strip_all_variant_selections() == wanted)
                })?;
                let specifier = match data
                    .try_field(&at, FieldKey::Specifier.as_str())
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
                for name in data.list_fields(&at).unwrap_or_default() {
                    if BOOKKEEPING.contains(&name.as_str()) || name == FieldKey::Specifier.as_str()
                    {
                        continue;
                    }
                    let Some(value) = data.try_field(&at, &name).ok().flatten() else {
                        continue;
                    };
                    fields.push(json!({ "name": name, "value": sdf_to_json(&value) }));
                }
                // A property spec is an opinion on its prim: the override a
                // gizmo writes is one of these, not a field of the prim spec.
                for property in property_names(data, &at) {
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
            "layer": layer.id,
            "specifier": specifier,
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
                        arcs.push(ArcEntry {
                            id: format!("arc:{}", arcs.len()),
                            kind: "sublayer",
                            source_layer: scan.id.clone(),
                            source_prim: "/".to_owned(),
                            target_layer: resolve_target(scans, &asset),
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
    if let Some(scan) = scans.iter().find(|s| s.id == asset) {
        return scan.id.clone();
    }
    if let Some(scan) = scans.iter().find(|s| s.resolved.as_deref() == Some(asset)) {
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

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

// --- prims and variant sets -------------------------------------------------

fn scan_namespace(
    stage: &Stage,
    scans: &[LayerScan],
    layers: &[LayerEntry],
) -> (Vec<PrimEntry>, Vec<VariantSetEntry>) {
    // Who authors each prim, strongest first. A spec inside a variant is an
    // opinion on the prim once its selections are stripped, so both spellings
    // land on one key.
    let mut authored: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for scan in scans {
        for (path, ty) in &scan.specs {
            if !matches!(ty, SpecType::Prim | SpecType::Variant) {
                continue;
            }
            let key = path.strip_all_variant_selections().as_str().to_owned();
            let list = authored.entry(key).or_default();
            if !list.iter().any(|id| id == &scan.id) {
                list.push(scan.id.clone());
            }
        }
    }

    let mut paths: Vec<String> = Vec::new();
    let _ = stage.traverse(PrimPredicate::ALL, |p| paths.push(p.as_str().to_owned()));
    paths.retain(|p| !p.is_empty() && p != "/");
    paths.sort();
    paths.dedup();
    let prims: Vec<PrimEntry> = paths
        .into_iter()
        .map(|path| {
            let key = sdf::path(&path)
                .map(|p| p.strip_all_variant_selections().as_str().to_owned())
                .unwrap_or_else(|_| path.clone());
            PrimEntry {
                id: format!("prim:{path}"),
                authored_in: authored.get(&key).cloned().unwrap_or_default(),
                path,
            }
        })
        .collect();

    // Variant sets, one row per `(prim, set)`: the variants any layer declares,
    // in the order the strongest declaring layer lists them, plus who authors
    // each variant's spec.
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
            "prepend": op.prepended_items.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
            "add": op.added_items.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
            "append": op.appended_items.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
            "delete": op.deleted_items.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
        }),
        S::StringListOp(op) => json!({
            "explicit": op.explicit,
            "prepend": op.prepended_items,
            "add": op.added_items,
            "append": op.appended_items,
            "delete": op.deleted_items,
        }),
        S::PathListOp(op) => json!({
            "explicit": op.explicit,
            "prepend": op.prepended_items.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            "add": op.added_items.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            "append": op.appended_items.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            "delete": op.deleted_items.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        }),
        S::ReferenceListOp(op) => json!({
            "explicit": op.explicit,
            "prepend": op.prepended_items.iter().map(reference_json).collect::<Vec<_>>(),
            "add": op.added_items.iter().map(reference_json).collect::<Vec<_>>(),
            "append": op.appended_items.iter().map(reference_json).collect::<Vec<_>>(),
            "delete": op.deleted_items.iter().map(reference_json).collect::<Vec<_>>(),
        }),
        S::PayloadListOp(op) => json!({
            "explicit": op.explicit,
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
#[cfg(test)]
mod tests {
    //! The `.rusd` surface cannot author a reference or a variant set: SPEC §4
    //! fixes the v0 authoring subset at sublayers, and what the cook writes is
    //! sublayers. So the arcs those make are exercised here, on a stage built
    //! straight out of `sdf` — which is also the only place this module can be
    //! tested without a socket.

    use std::path::Path;

    use openusd::ar::DefaultResolver;
    use openusd::usd::Stage;

    use super::*;

    /// A scratch directory for authored stage files, removed when the test ends.
    struct StageDir {
        dir: std::path::PathBuf,
    }

    impl StageDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rusd_comp_{}_{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create the stage dir");
            StageDir { dir }
        }

        fn path(&self) -> &Path {
            &self.dir
        }

        /// Open `root.usda` with a resolver that searches this directory, so
        /// `@./proto.usda@` resolves.
        fn open(&self) -> Stage {
            let root = self
                .path()
                .join("root.usda")
                .to_string_lossy()
                .replace('\\', "/");
            Stage::builder()
                .resolver(DefaultResolver::with_search_paths([self
                    .path()
                    .to_path_buf()]))
                .open(&root)
                .expect("open the authored stage")
        }
    }

    impl Drop for StageDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// `proto.usda`: one `Sphere` prim at `/P`.
    fn write_asset(dir: &Path) {
        let mut data = sdf::Data::new();
        let root = data.create_spec(sdf::Path::abs_root(), SpecType::PseudoRoot);
        root.add(FieldKey::DefaultPrim, SdfValue::Token("P".into()));
        // The text writer walks the child lists, so a prim the pseudo-root does
        // not name is not written at all.
        root.add(
            ChildrenKey::PrimChildren,
            SdfValue::TokenVec(vec!["P".into()]),
        );
        let prim = data.create_spec(sdf::path("/P").unwrap(), SpecType::Prim);
        prim.add(
            FieldKey::Specifier,
            SdfValue::Specifier(sdf::Specifier::Def),
        );
        prim.add(FieldKey::TypeName, SdfValue::Token("Sphere".into()));
        openusd::usda::TextWriter::write_to_file(&data, dir.join("proto.usda"))
            .expect("write the asset");
    }

    /// `root.usda`: one `Xform` at `/X`, referencing `/P` in `proto.usda` when
    /// asked.
    fn write_root(dir: &Path, with_reference: bool) {
        let mut data = sdf::Data::new();
        let root = data.create_spec(sdf::Path::abs_root(), SpecType::PseudoRoot);
        root.add(FieldKey::DefaultPrim, SdfValue::Token("X".into()));
        root.add(
            ChildrenKey::PrimChildren,
            SdfValue::TokenVec(vec!["X".into()]),
        );
        let prim = data.create_spec(sdf::path("/X").unwrap(), SpecType::Prim);
        prim.add(
            FieldKey::Specifier,
            SdfValue::Specifier(sdf::Specifier::Def),
        );
        prim.add(FieldKey::TypeName, SdfValue::Token("Xform".into()));
        if with_reference {
            prim.add(
                FieldKey::References,
                SdfValue::ReferenceListOp(sdf::ListOp {
                    prepended_items: vec![sdf::Reference {
                        asset_path: "./proto.usda".to_owned(),
                        prim_path: sdf::path("/P").unwrap(),
                        layer_offset: sdf::LayerOffset::IDENTITY,
                        custom_data: Default::default(),
                    }],
                    ..Default::default()
                }),
            );
        }
        openusd::usda::TextWriter::write_to_file(&data, dir.join("root.usda"))
            .expect("write the root layer");
    }

    #[test]
    fn a_reference_becomes_an_arc_between_named_layers_and_prims() {
        let scratch = StageDir::new("reference");
        write_asset(scratch.path());
        write_root(scratch.path(), true);
        let stage = scratch.open();
        let tree = CompositionTree::build(&stage).to_json();

        let layers: Vec<&str> = tree["layers"]
            .as_array()
            .expect("layers")
            .iter()
            .filter_map(|l| l["id"].as_str())
            .collect();
        let root_id = layers
            .iter()
            .find(|id| id.contains("root.usda"))
            .unwrap_or_else(|| panic!("the root layer is listed: {layers:?}"))
            .to_string();
        let target = layers
            .iter()
            .find(|id| id.contains("proto.usda"))
            .unwrap_or_else(|| panic!("the referenced layer is opened and listed: {layers:?}"))
            .to_string();

        let arc = tree["arcs"]
            .as_array()
            .expect("arcs")
            .iter()
            .find(|a| a["kind"].as_str() == Some("reference"))
            .unwrap_or_else(|| panic!("a reference arc: {tree}"))
            .clone();
        assert_eq!(arc["source_layer"].as_str(), Some(root_id.as_str()));
        assert_eq!(arc["source_prim"].as_str(), Some("/X"));
        assert_eq!(
            arc["target_layer"].as_str(),
            Some(target.as_str()),
            "the asset path resolves to the loaded layer's identifier: {arc}"
        );
        assert_eq!(arc["target_prim"].as_str(), Some("/P"));

        // The referenced layer is weaker than the layer that references it.
        let strength = |id: &str| {
            tree["layers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|l| l["id"].as_str() == Some(id))
                .and_then(|l| l["strength"].as_u64())
                .unwrap_or_else(|| panic!("{id} has a strength"))
        };
        assert!(
            strength(&root_id) < strength(&target),
            "the referencing layer comes first"
        );
    }

    #[test]
    fn an_arc_detail_carries_its_target_spec_fields() {
        let scratch = StageDir::new("arcdetail");
        write_asset(scratch.path());
        write_root(scratch.path(), true);
        let stage = scratch.open();
        let tree = CompositionTree::build(&stage);
        let json = tree.to_json();
        let id = json["arcs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["kind"].as_str() == Some("reference"))
            .and_then(|a| a["id"].as_str())
            .expect("a reference arc id")
            .to_owned();

        let detail = tree.node(&stage, &id).expect("the arc has a detail");
        assert_eq!(detail["kind_type"].as_str(), Some("arc"));
        assert_eq!(detail["kind"].as_str(), Some("reference"));
        assert_eq!(detail["target_prim"].as_str(), Some("/P"));
        let fields = detail["target_spec_fields"].as_array().expect("fields");
        let type_name = fields
            .iter()
            .find(|f| f["name"].as_str() == Some("typeName"))
            .unwrap_or_else(|| panic!("the target spec's typeName: {detail}"));
        assert_eq!(type_name["value"].as_str(), Some("Sphere"));
        let specifier = fields
            .iter()
            .find(|f| f["name"].as_str() == Some("specifier"))
            .expect("the target spec's specifier");
        assert_eq!(specifier["value"].as_str(), Some("def"));

        // An id nothing answers to is not a detail.
        assert!(tree.node(&stage, "arc:9999").is_none());
        assert!(tree.node(&stage, "prim:/Nope").is_none());
        assert!(tree.node(&stage, "vset:9999").is_none());
    }

    #[test]
    fn a_variant_set_reports_its_variants_selection_and_authors() {
        let scratch = StageDir::new("variant");
        write_asset(scratch.path());
        write_root(scratch.path(), false);

        // Author a `look` variant set on `/X`: `red` (a Sphere) selected, `blue`
        // (a Cube) beside it. Variant scaffolding is `sdf`'s, not the document's.
        let root = scratch.path().join("root.usda");
        let mut layer =
            sdf::Layer::open(root.to_string_lossy().as_ref()).expect("open the root layer");
        layer
            .edit(|l| {
                let d = l.data_mut();
                let prim = sdf::path("/X")?;
                d.set_field(
                    &prim,
                    FieldKey::VariantSetNames.as_str(),
                    SdfValue::StringListOp(sdf::ListOp {
                        prepended_items: vec!["look".to_owned()],
                        ..Default::default()
                    }),
                );
                let mut selections = std::collections::HashMap::new();
                selections.insert("look".to_owned(), "red".to_owned());
                d.set_field(
                    &prim,
                    FieldKey::VariantSelection.as_str(),
                    SdfValue::VariantSelectionMap(selections),
                );
                d.set_field(
                    &prim,
                    ChildrenKey::VariantSetChildren.as_str(),
                    SdfValue::TokenVec(vec!["look".into()]),
                );
                let vset = sdf::path("/X{look=}")?;
                d.create_spec(vset.clone(), SpecType::VariantSet);
                d.set_field(
                    &vset,
                    ChildrenKey::VariantChildren.as_str(),
                    SdfValue::TokenVec(vec!["red".into(), "blue".into()]),
                );
                for (name, ty) in [("red", "Sphere"), ("blue", "Cube")] {
                    let variant = sdf::path(format!("/X{{look={name}}}"))?;
                    d.create_spec(variant.clone(), SpecType::Variant);
                    d.set_field(
                        &variant,
                        ChildrenKey::PrimChildren.as_str(),
                        SdfValue::TokenVec(vec!["c".into()]),
                    );
                    let child = sdf::path(format!("/X{{look={name}}}c"))?;
                    d.create_spec(child.clone(), SpecType::Prim);
                    d.set_field(
                        &child,
                        FieldKey::Specifier.as_str(),
                        SdfValue::Specifier(sdf::Specifier::Def),
                    );
                    d.set_field(
                        &child,
                        FieldKey::TypeName.as_str(),
                        SdfValue::Token(ty.into()),
                    );
                }
                Ok(())
            })
            .expect("author the variant set");
        layer.save().expect("save the root layer");

        let stage = scratch.open();
        let tree = CompositionTree::build(&stage);
        let json = tree.to_json();
        let sets = json["variant_sets"].as_array().expect("variant_sets");
        assert_eq!(sets.len(), 1, "one set: {json}");
        let set = &sets[0];
        assert_eq!(set["prim"].as_str(), Some("/X"));
        assert_eq!(set["name"].as_str(), Some("look"));
        let variants: Vec<&str> = set["variants"]
            .as_array()
            .expect("variants")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(variants, vec!["red", "blue"], "{set}");
        assert_eq!(set["selection"].as_str(), Some("red"));

        // One edge per variant, from the prim to the variant spec.
        let variant_arcs: Vec<&Value> = json["arcs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["kind"].as_str() == Some("variant"))
            .collect();
        assert_eq!(variant_arcs.len(), 2, "{json}");
        assert!(
            variant_arcs
                .iter()
                .any(|a| a["target_prim"].as_str() == Some("/X{look=red}")),
            "{variant_arcs:?}"
        );

        let detail = tree
            .node(&stage, set["id"].as_str().unwrap())
            .expect("set detail");
        assert_eq!(detail["kind_type"].as_str(), Some("variant_set"));
        let per = detail["per_variant"].as_array().expect("per_variant");
        assert_eq!(per.len(), 2, "{detail}");
        for row in per {
            assert_eq!(
                row["authored_in"].as_array().map(Vec::len),
                Some(1),
                "each variant spec lives in the root layer: {row}"
            );
        }

        // The prim's node names the variant set.
        let prim = tree.node(&stage, "prim:/X").expect("the prim node");
        assert_eq!(
            prim["variant_sets"].as_array().map(Vec::len),
            Some(1),
            "{prim}"
        );
    }

    #[test]
    fn an_inherit_names_a_prim_in_the_same_layer() {
        let scratch = StageDir::new("inherit");
        write_asset(scratch.path());
        write_root(scratch.path(), false);
        let root = scratch.path().join("root.usda");
        let mut layer =
            sdf::Layer::open(root.to_string_lossy().as_ref()).expect("open the root layer");
        layer
            .edit(|l| {
                let d = l.data_mut();
                let class = sdf::path("/Base")?;
                d.create_spec(class.clone(), SpecType::Prim);
                d.set_field(
                    &class,
                    FieldKey::Specifier.as_str(),
                    SdfValue::Specifier(sdf::Specifier::Class),
                );
                let x = sdf::path("/X")?;
                d.set_field(
                    &x,
                    FieldKey::InheritPaths.as_str(),
                    SdfValue::PathListOp(sdf::ListOp {
                        prepended_items: vec![class.clone()],
                        ..Default::default()
                    }),
                );
                Ok(())
            })
            .expect("author the inherit");
        layer.save().expect("save the root layer");

        let stage = scratch.open();
        let tree = CompositionTree::build(&stage).to_json();
        let arc = tree["arcs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["kind"].as_str() == Some("inherit"))
            .unwrap_or_else(|| panic!("an inherit arc: {tree}"))
            .clone();
        assert_eq!(arc["source_prim"].as_str(), Some("/X"));
        assert_eq!(arc["target_prim"].as_str(), Some("/Base"));
        assert_eq!(
            arc["source_layer"], arc["target_layer"],
            "an inherit names a prim in the same layer stack: {arc}"
        );
    }

    #[test]
    fn two_builds_of_one_stage_agree_byte_for_byte() {
        let scratch = StageDir::new("shape");
        write_asset(scratch.path());
        write_root(scratch.path(), true);
        let stage = scratch.open();
        let a = CompositionTree::build(&stage).to_json();
        let b = CompositionTree::build(&stage).to_json();
        assert_eq!(a, b, "the tree is deterministic: no map order reaches it");
    }

    #[test]
    fn sdf_values_keep_their_shape_on_the_wire() {
        assert_eq!(sdf_to_json(&SdfValue::Bool(true)), json!(true));
        assert_eq!(sdf_to_json(&SdfValue::Int(3)), json!(3));
        assert_eq!(sdf_to_json(&SdfValue::Double(1.5)), json!(1.5));
        assert_eq!(sdf_to_json(&SdfValue::String("x".into())), json!("x"));
        assert_eq!(
            sdf_to_json(&SdfValue::Token("Xform".into())),
            json!("Xform")
        );
        assert_eq!(
            sdf_to_json(&SdfValue::TokenVec(vec!["a".into(), "b".into()])),
            json!(["a", "b"])
        );
        assert_eq!(
            sdf_to_json(&SdfValue::Specifier(sdf::Specifier::Def)),
            json!("def")
        );
        assert_eq!(
            sdf_to_json(&SdfValue::Vec3f(openusd::gf::vec3f(1.0, 2.0, 3.0))),
            json!([1.0, 2.0, 3.0])
        );
        assert_eq!(
            sdf_to_json(&SdfValue::VariantSelectionMap(
                [("look".to_owned(), "red".to_owned())]
                    .into_iter()
                    .collect()
            )),
            json!({ "look": "red" })
        );
        let mut dictionary = std::collections::HashMap::new();
        dictionary.insert("b".to_owned(), SdfValue::Int(2));
        dictionary.insert("a".to_owned(), SdfValue::Int(1));
        assert_eq!(
            sdf_to_json(&SdfValue::Dictionary(dictionary)),
            json!({ "a": 1, "b": 2 }),
            "a dictionary goes out sorted, not in hash order"
        );
        // A value with no JSON shape is named, not silently dropped.
        assert!(sdf_to_json(&SdfValue::Float(f32::MIN)).is_number());
        assert!(sdf_to_json(&SdfValue::Quatf(openusd::gf::Quatf::IDENTITY)).is_string());
    }

    #[test]
    fn an_asset_path_finds_the_layer_it_names() {
        let scans = vec![
            LayerScan {
                id: "anon:1:rusd_root.usda".into(),
                resolved: None,
                specs: Vec::new(),
            },
            LayerScan {
                id: "C:/assets/proto.usda".into(),
                resolved: Some("C:/assets/proto.usda".into()),
                specs: Vec::new(),
            },
        ];
        assert_eq!(
            resolve_target(&scans, "C:/assets/proto.usda"),
            "C:/assets/proto.usda",
            "an exact identifier is itself"
        );
        assert_eq!(
            resolve_target(&scans, "./proto.usda"),
            "C:/assets/proto.usda",
            "a relative asset path finds the layer by file name"
        );
        assert_eq!(
            resolve_target(&scans, "./missing.usda"),
            "./missing.usda",
            "an asset nothing answers to stays as authored"
        );
        assert_eq!(file_name("a/b\\c.usda"), "c.usda");
    }

    /// A stage whose root layer sublayers `layers`, inserted the way the cook
    /// inserts one: at position 0, so the newest output layer is strongest.
    fn cooked_stack(layers: &[&str]) -> Stage {
        let stage = Stage::builder()
            .in_memory("root.usda")
            .expect("in-memory stage");
        let root_id = stage.root_layer().identifier().to_owned();
        for tag in layers {
            stage
                .insert_layer(
                    &root_id,
                    0,
                    sdf::Layer::new_anonymous(tag),
                    sdf::LayerOffset::IDENTITY,
                )
                .expect("insert the cooked layer");
        }
        stage
    }

    #[test]
    fn cook_output_layers_are_listed_in_cook_order_not_stack_order() {
        // Cooked `city` first, then `scatter`: the cook inserts each at position
        // 0 of the root's stack, so the stack reads `root, scatter, city` while
        // the cook ran `city` then `scatter`. `preview{upto}` counts in cook
        // order, so that is what this answers.
        let stage = cooked_stack(&["rusd_city.usda", "rusd_scatter.usda"]);
        let ops = cook_output_layers(&stage);
        assert_eq!(ops.len(), 2, "{ops:?}");
        assert!(ops[0].contains("rusd_city.usda"), "{ops:?}");
        assert!(ops[1].contains("rusd_scatter.usda"), "{ops:?}");

        let root_id = stage.root_layer().identifier().to_owned();
        let root_sublayers = stage.sub_layers(&root_id);
        assert!(
            !cook_output_layer(&root_id, &root_id, &root_sublayers),
            "the root layer is the document, not a cook output"
        );
        for id in &ops {
            assert!(
                cook_output_layer(id, &root_id, &root_sublayers),
                "each cooked sublayer is an op: {id}"
            );
        }
        // A layer the stage holds but the root does not sublayer — a session
        // layer, an arc target — is not one either.
        assert!(!cook_output_layer(
            "C:/assets/proto.usda",
            &root_id,
            &root_sublayers
        ));
    }

    #[test]
    fn the_tree_flags_the_cook_output_layers() {
        let stage = cooked_stack(&["rusd_city.usda", "rusd_scatter.usda"]);
        let tree = CompositionTree::build(&stage).to_json();
        let rows: Vec<(String, bool)> = tree["layers"]
            .as_array()
            .expect("layers")
            .iter()
            .map(|l| {
                (
                    l["id"].as_str().unwrap_or_default().to_owned(),
                    l["op"].as_bool().unwrap_or(false),
                )
            })
            .collect();
        let ops: Vec<&String> = rows
            .iter()
            .filter(|(_, op)| *op)
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ops.len(), 2, "both cooked layers are ops: {rows:?}");
        assert!(
            rows.iter().any(|(id, op)| !op && id.contains("root.usda")),
            "the root layer is not an op: {rows:?}"
        );
        // The flag survives to `comp_node`'s layer detail.
        let tree = CompositionTree::build(&stage);
        let detail = tree
            .node(&stage, ops[0])
            .expect("the op layer has a detail");
        assert_eq!(detail["op"].as_bool(), Some(true), "{detail}");
    }
}
