//! The v1.5.4 client contract (`docs/PROTOCOL.md` v1.5.4, then the user's
//! follow-up ruling): **240 instance references are one row, and that row opens.**
//!
//! What this file pins, none of it needing a GPU or a window:
//!
//! 1. **The aggregation.** Arcs that agree on `(kind, source_layer, target_layer,
//!    target_prim)` are drawn **once**; `source_prim` is not part of the key, because
//!    it is the one field that tells 240 instances apart. A group of N >= 2 carries
//!    `×N`; N = 1 carries no suffix. Different keys (a different `target_prim`,
//!    `target_layer` or `kind`) are different rows.
//! 2. **The wire is untouched.** `CompositionTree::arcs` still holds every arc, so the
//!    panel's 「242 arc」 count and every other `composition_tree` consumer are
//!    unchanged. Aggregation is drawing only.
//! 3. **The row opens into its instances.** The group row's children are one row per
//!    arc, naming that arc's `source_prim`; a clicked instance row **selects that
//!    prim** with the same selection shape a viewport pick produces (id, kind, prim),
//!    so the shell's selection path is the one that already exists. N = 1 gets its one
//!    row too.
//! 4. **The group row itself is an edge and selects no prim.** Its `prim` is `None`,
//!    which is what keeps `Shell::on_tree_select` from selecting, copying and
//!    re-rendering a prim when the row that was clicked stands for a *class* of
//!    edges. Its detail ask is still the target prim (`comp_node{目标}`).
//! 5. **The pseudo-root authors no instance row.** A `subLayer` arc reports
//!    `source_prim: "/"`, which is the stage, not a mesh.
//!
//! The panel half (a collapsed group row is one line; opening it draws the
//! instances) is asserted here too, headlessly, with the production widget.

use rusd_viewer::panel::{self, TreeAction, TreePanel};
use rusd_viewer::tree::{
    self, ArcGroup, CompositionTree, TreeNode, TreeNodeKind, TreeSelection, arc_groups, flatten,
    prim_node_id,
};
use serde_json::{Value, json};

// --- payloads ----------------------------------------------------------------

/// **The shape that made this change necessary**: 240 instances of one reference.
///
/// All 240 arcs agree on `(reference, tufts.usda, prototypes.usda,
/// /Prototypes/Tuft)` and differ only in `source_prim` -- which is exactly the fact a
/// panel must not turn into 240 rows.
fn tufts_tree(n: usize) -> Value {
    let mut arcs = Vec::new();
    for i in 0..n {
        arcs.push(json!({
            "id": format!("arc:{i}"),
            "kind": "reference",
            "source_layer": "tufts.usda",
            "source_prim": format!("/World/Tufts/i{i:06}"),
            "target_layer": "prototypes.usda",
            "target_prim": "/Prototypes/Tuft",
        }));
    }
    json!({
        "layers": [
            {"id": "anon:session", "kind": "session", "strength": 0},
            {"id": "tufts.usda", "kind": "root", "strength": 1},
            {"id": "prototypes.usda", "kind": "reference", "strength": 2},
        ],
        "arcs": arcs,
        "prim_nodes": [
            {"id": "prim:/Prototypes/Tuft", "path": "/Prototypes/Tuft",
             "authored_in": ["prototypes.usda"]},
        ],
        "variant_sets": [],
    })
}

fn rows_of(tree: &Value) -> Vec<(usize, TreeNode)> {
    let parsed = CompositionTree::parse(tree).expect("the payload parses");
    flatten(&tree::roots(&parsed))
}

fn row_by_id(rows: &[(usize, TreeNode)], id: &str) -> Option<TreeNode> {
    rows.iter()
        .find(|(_, n)| n.id == id)
        .map(|(_, n)| n.clone())
}

/// The arc rows: the rows of kind `Arc`, in draw order.
fn arc_rows(rows: &[(usize, TreeNode)]) -> Vec<TreeNode> {
    rows.iter()
        .filter(|(_, n)| n.kind == TreeNodeKind::Arc)
        .map(|(_, n)| n.clone())
        .collect()
}

// --- 1. the aggregation ------------------------------------------------------

#[test]
fn two_hundred_forty_identical_references_are_one_row_carrying_x240() {
    let rows = rows_of(&tufts_tree(240));
    let arcs = arc_rows(&rows);
    assert_eq!(
        arcs.len(),
        1,
        "240 arcs of one class are one row: {:?}",
        arcs.iter().map(|a| a.label.as_str()).collect::<Vec<_>>()
    );
    let row = &arcs[0];
    assert!(
        row.label.ends_with("×240"),
        "the row says how many it stands for: {:?}",
        row.label
    );
    assert!(
        row.label.contains("/Prototypes/Tuft"),
        "and where they point: {:?}",
        row.label
    );
    // The count is the group's own, and the wire still carries all 240: the panel's
    // 「242 arc」 header counts `tree.arcs`, which nothing here changed.
    let tree = CompositionTree::parse(&tufts_tree(240)).expect("parses");
    assert_eq!(tree.arcs.len(), 240, "the wire is untouched");
    let groups = arc_groups(&tree, "tufts.usda");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].count(), 240);
}

#[test]
fn a_group_of_one_has_no_suffix() {
    // N = 1 is the pre-v1.5.4 picture exactly: one arc, one row, no `×1`.
    let rows = rows_of(&tufts_tree(1));
    let arcs = arc_rows(&rows);
    assert_eq!(arcs.len(), 1);
    assert!(
        !arcs[0].label.contains('×'),
        "a single arc says nothing about a count: {:?}",
        arcs[0].label
    );
}

#[test]
fn arcs_that_differ_in_any_key_field_are_separate_rows() {
    // One field at a time, against a base of two identical arcs each time: a
    // different `target_prim`, a different `target_layer`, and a different `kind` must
    // each split the group, because each is part of the key.
    let base = json!({
        "id": "arc:1", "kind": "reference", "source_layer": "a.usda",
        "source_prim": "/A2", "target_layer": "b.usda", "target_prim": "/B",
    });
    for (what, field, value) in [
        ("target_prim", "target_prim", json!("/Other")),
        ("target_layer", "target_layer", json!("c.usda")),
        ("kind", "kind", json!("payload")),
    ] {
        let mut second = base.clone();
        second[field] = value.clone();
        let tree = json!({
            "layers": [
                {"id": "a.usda", "kind": "root", "strength": 0},
                {"id": "b.usda", "kind": "reference", "strength": 1},
                {"id": "c.usda", "kind": "reference", "strength": 2},
            ],
            "arcs": [
                {"id": "arc:0", "kind": "reference", "source_layer": "a.usda",
                 "source_prim": "/A1", "target_layer": "b.usda", "target_prim": "/B"},
                second,
            ],
            "prim_nodes": [],
            "variant_sets": [],
        });
        let arcs = arc_rows(&rows_of(&tree));
        assert_eq!(
            arcs.len(),
            2,
            "a different {what} is a different class, so two rows: {:?}",
            arcs.iter().map(|a| a.label.as_str()).collect::<Vec<_>>()
        );
        assert!(
            arcs.iter().all(|a| !a.label.contains('×')),
            "neither of the two is a group of several: {:?}",
            arcs.iter().map(|a| a.label.as_str()).collect::<Vec<_>>()
        );
    }
}

#[test]
fn the_source_prim_is_not_part_of_the_key() {
    // The heart of it, stated as the lookup rather than through a payload: two arcs
    // whose every field but `source_prim` agrees are one group.
    let tree = CompositionTree::parse(&tufts_tree(2)).expect("parses");
    let a = &tree.arcs[0];
    let b = &tree.arcs[1];
    assert_ne!(a.source_prim, b.source_prim, "the instances differ");
    assert_eq!(a.group_key(), b.group_key(), "and nothing else does");
    // And an arc's key really is the four fields, `kind` first.
    assert_eq!(a.group_key().kind, "reference");
    assert_eq!(a.group_key().source_layer, "tufts.usda");
    assert_eq!(a.group_key().target_layer, "prototypes.usda");
    assert_eq!(a.group_key().target_prim, "/Prototypes/Tuft");
}

#[test]
fn a_group_lists_its_instances_in_wire_order() {
    let tree = CompositionTree::parse(&tufts_tree(3)).expect("parses");
    let groups = arc_groups(&tree, "tufts.usda");
    assert_eq!(
        groups[0].source_prims(),
        [
            "/World/Tufts/i000000",
            "/World/Tufts/i000001",
            "/World/Tufts/i000002"
        ],
        "the opened row's order is the server's, not a sort of our own"
    );
    // A group is a value a test can build on its own, too: the label rule is not
    // reachable only through a payload.
    let group: ArcGroup<'_> = groups[0].clone();
    assert_eq!(group.count(), 3);
    assert_eq!(
        group.representative().id,
        "arc:0",
        "the first in wire order"
    );
}

// --- 2. the row opens -------------------------------------------------------

#[test]
fn the_group_row_opens_into_one_row_per_instance() {
    let rows = rows_of(&tufts_tree(240));
    let group = row_by_id(&rows, "prim:/Prototypes/Tuft").expect("the group row");
    assert_eq!(group.kind, TreeNodeKind::Arc, "it is still an edge");
    // The children are the naming row that says which layer the edges reach (the
    // pre-existing shape for a cross-layer arc) and then one row per arc.
    let instances: Vec<&TreeNode> = group
        .children
        .iter()
        .filter(|c| c.kind == TreeNodeKind::Prim)
        .collect();
    assert_eq!(
        instances.len(),
        240,
        "one row per arc, so the cluster is walkable down to one mesh"
    );
    let first = instances[0];
    assert_eq!(first.label, "/World/Tufts/i000000", "the prim it names");
    assert_eq!(first.id, "prim:/World/Tufts/i000000", "and the id it asks");
    assert_eq!(instances[239].label, "/World/Tufts/i000239");
}

#[test]
fn an_instance_row_selects_its_prim_the_way_a_pick_does() {
    // **The one thing the follow-up ruling asks of a click**: select that prim, by the
    // same selection shape a viewport pick produces. A pick builds
    // `{id: prim:<path>, kind: Prim, prim: <path>}` (`Shell::inspect_picked`); a row
    // that builds anything else would take a different path through the shell.
    let rows = rows_of(&tufts_tree(2));
    let group = row_by_id(&rows, "prim:/Prototypes/Tuft").expect("the group row");
    let instances: Vec<&TreeNode> = group
        .children
        .iter()
        .filter(|c| c.kind == TreeNodeKind::Prim)
        .collect();
    let instance = instances[1];
    assert_eq!(instance.label, "/World/Tufts/i000001");

    let sel = TreeSelection::of(instance);
    assert_eq!(sel.kind, TreeNodeKind::Prim);
    assert_eq!(sel.prim.as_deref(), Some("/World/Tufts/i000001"));
    assert_eq!(
        sel.id,
        prim_node_id("/World/Tufts/i000001"),
        "the id comp_node is asked for is the prim's own"
    );
    // The clipboard payload is the path, so a click on an instance copies exactly what
    // a pick copies.
    assert_eq!(
        tree::clipboard_for(&sel).as_deref(),
        Some("/World/Tufts/i000001")
    );
    // And the action is a selection, never a preview: this row is not a control.
    assert!(matches!(panel::action_for(instance), TreeAction::Select(_)));
}

#[test]
fn the_group_row_itself_selects_no_prim_but_asks_the_targets_detail() {
    // 「聚合行自身点击：仍不选 prim（它是一条边）」. `prim: None` is the whole of that:
    // `TreeSelection::prim` is what the shell's selection side effects read.
    let rows = rows_of(&tufts_tree(240));
    let group = row_by_id(&rows, "prim:/Prototypes/Tuft").expect("the group row");
    assert_eq!(
        group.prim, None,
        "an edge class is not a prim, so its click selects none"
    );
    let TreeAction::Select(sel) = panel::action_for(&group) else {
        panic!("the group row is not a preview control");
    };
    assert_eq!(sel.prim, None);
    assert_eq!(
        tree::clipboard_for(&sel),
        None,
        "nothing is copied for a row that stands for no prim"
    );
    // Its ask is the node the edges point at, which is what v1.5.4 keeps.
    assert_eq!(sel.id, "prim:/Prototypes/Tuft");
}

#[test]
fn a_single_edge_keeps_the_id_and_selection_it_had() {
    // v1.5.4: 点击语义不变. A group of one is the pre-existing row, id and all -- the
    // arc's own `arc:<n>`, selecting the prim it is authored on -- so a stack with no
    // 240-way cluster is byte-for-byte the picture it was.
    let rows = rows_of(&tufts_tree(1));
    let arc = row_by_id(&rows, "arc:0").expect("the arc's own id");
    assert_eq!(arc.prim.as_deref(), Some("/World/Tufts/i000000"));
    let sel = TreeSelection::of(&arc);
    assert_eq!(sel.id, "arc:0");
    assert_eq!(
        tree::clipboard_for(&sel).as_deref(),
        Some("/World/Tufts/i000000")
    );
    // It has its instance row too, so N = 1 is expandable as the ruling says.
    let instances: Vec<&TreeNode> = arc
        .children
        .iter()
        .filter(|c| c.kind == TreeNodeKind::Prim)
        .collect();
    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0].label, "/World/Tufts/i000000");
}

#[test]
fn the_pseudo_root_authors_no_instance_row() {
    // A `subLayer` edge is authored at the stage, and `prim:/` would select everything.
    let tree = json!({
        "layers": [
            {"id": "root.usda", "kind": "root", "strength": 0},
            {"id": "a.usda", "kind": "sublayer", "strength": 1},
        ],
        "arcs": [
            {"id": "arc:0", "kind": "subLayer", "source_layer": "root.usda",
             "source_prim": "/", "target_layer": "root.usda", "target_prim": "/"},
        ],
        "prim_nodes": [],
        "variant_sets": [],
    });
    let rows = rows_of(&tree);
    let arc = row_by_id(&rows, "arc:0").expect("the sublayer edge");
    assert!(
        arc.children.iter().all(|c| c.kind != TreeNodeKind::Prim),
        "the stage is not an instance: {:?}",
        arc.children
            .iter()
            .map(|c| c.label.as_str())
            .collect::<Vec<_>>()
    );
}

// --- 3. the panel draws it collapsed, and opening it draws the rows ----------

/// The texts a headless pass of the production widget painted.
fn painted(tree: Value) -> Vec<String> {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1000.0, 700.0),
        )),
        ..Default::default()
    };
    let mut panel = TreePanel::new();
    panel.apply_tree(Ok(tree));
    let out = ctx.run_ui(raw, |root| {
        egui::CentralPanel::no_frame().show(root, |ui| {
            let _ = panel::tree_panel(ui, &panel);
        });
    });
    let mut delta = out.textures_delta;
    delta.clear();
    out.shapes
        .iter()
        .filter_map(|c| match &c.shape {
            egui::epaint::Shape::Text(t) => Some(t.galley.text().to_owned()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_panel_paints_one_row_for_the_cluster_and_none_of_its_instances() {
    let texts = painted(tufts_tree(240));
    let group: Vec<&String> = texts.iter().filter(|t| t.contains("×240")).collect();
    assert_eq!(group.len(), 1, "the cluster is one painted row: {texts:?}");
    // Not one instance path is painted: the row is collapsed, which is the point of
    // aggregating it in the first place.
    assert!(
        !texts.iter().any(|t| t.contains("/World/Tufts/i000")),
        "no instance is painted until the row is opened: {texts:?}"
    );
}

#[test]
fn the_panel_paints_a_single_edge_exactly_as_before() {
    // The pre-v1.5.4 picture for a stack with no cluster: the edge's own label, with
    // no count on it.
    let texts = painted(tufts_tree(1));
    assert!(
        texts
            .iter()
            .any(|t| t.contains("reference → prototypes.usda:")),
        "the edge is painted: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains('×')),
        "and nothing says ×1: {texts:?}"
    );
}
