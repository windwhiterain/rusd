//! The detail panel **speaks in layers** (the user's ruling, after v1.5.4): a picked
//! mesh is shown as one section per layer, headed by that layer -- its identifier, its
//! kind and its strength, and a cook op's generator name -- with the layer's own
//! specifier and attributes under it.
//!
//! Why it matters here: v1.5.4's instance rows make it possible to walk a 240-instance
//! cluster down to `/World/Tufts/i000123` and click it. What the panel then has to
//! answer is not "what does this prim have" but **"which layer says what about it"**,
//! because that is the question a composite stack is read with.
//!
//! What this file pins, none of it needing a GPU or a window:
//!
//! 1. **One section per layer, in the server's strength order** -- the section order is
//!    the composite's, not an alphabet.
//! 2. **A section's header carries the tree's own facts** for a layer the tree lists
//!    (`kind`, `strength`, and an op's `op_name`); a layer the tree does not list still
//!    gets its section, headed by its identifier alone.
//! 3. **A section's body is that layer's specifier and its attributes** -- the shape a
//!    reader checks "did the session override it?" against.
//! 4. **The panel draws the sections**, headlessly, with the production widget.

use rusd_viewer::panel::{self, TreePanel};
use rusd_viewer::tree::{
    CompositionTree, LayerSection, NodeDetail, PrimDetail, TreeNode, TreeSelection, layer_header,
};
use serde_json::{Value, json};

// --- fixtures ----------------------------------------------------------------

/// A stack with a session layer, a root layer, and a cook op's output layer -- the
/// three kinds of layer a prim's sections can be headed by.
fn stack() -> CompositionTree {
    CompositionTree::parse(&json!({
        "layers": [
            {"id": "anon:session", "kind": "session", "strength": 0},
            {"id": "tufts.usda", "kind": "root", "strength": 1},
            {"id": "anon:rusd_instances/0", "kind": "sublayer", "strength": 2, "op": true,
             "op_name": "RusdInstances",
             "input": "/World/Tufts/points", "output": "anon:rusd_instances/0"},
        ],
        "arcs": [
            {"id": "arc:0", "kind": "reference", "source_layer": "tufts.usda",
             "source_prim": "/World/Tufts/i000000", "target_layer": "tufts.usda",
             "target_prim": "/World/Tufts/i000000"},
        ],
        "prim_nodes": [
            {"id": "prim:/World/Tufts/i000000", "path": "/World/Tufts/i000000",
             "authored_in": ["tufts.usda"]},
        ],
        "variant_sets": [],
    }))
    .expect("parses")
}

/// A `comp_node` prim detail: three layers wrote it, strongest first.
fn prim_detail() -> PrimDetail {
    let value = json!({
        "kind_type": "prim",
        "id": "prim:/World/Tufts/i000000",
        "path": "/World/Tufts/i000000",
        "opinions": [
            {"layer": "anon:session", "specifier": "over",
             "fields": [{"name": "xformOp:translate", "value": [3.0, 0.0, 7.0]}]},
            {"layer": "anon:rusd_instances/0", "specifier": "def",
             "fields": [{"name": "typeName", "value": "Xform"},
                        {"name": "visibility", "value": "inherited"}]},
            {"layer": "tufts.usda", "specifier": "class",
             "fields": [{"name": "typeName", "value": "Xform"}]},
        ],
        "arcs_out": ["arc:0"],
        "variant_sets": [],
    });
    match NodeDetail::parse(&value).expect("a prim detail") {
        NodeDetail::Prim(p) => p,
        other => panic!("expected a prim, got {other:?}"),
    }
}

// --- 1. one section per layer, in the composite's order ----------------------

#[test]
fn a_prim_detail_is_one_section_per_layer_in_strength_order() {
    let tree = stack();
    let sections = prim_detail().layer_sections(Some(&tree));
    assert_eq!(
        sections
            .iter()
            .map(|s| s.layer.as_str())
            .collect::<Vec<_>>(),
        ["anon:session", "anon:rusd_instances/0", "tufts.usda"],
        "the server's order is the composite's, and it is kept"
    );
    // The same order `lines()` groups by, since the panel and the tests read one model.
    let lines = NodeDetail::Prim(prim_detail()).lines();
    let headers: Vec<&str> = lines
        .iter()
        .filter(|l| l.starts_with("\u{2500}\u{2500} "))
        .map(String::as_str)
        .collect();
    assert_eq!(headers.len(), 3, "three sections: {lines:?}");
    assert!(headers[0].contains("anon:session"), "{headers:?}");
}

#[test]
fn a_sections_header_carries_what_the_tree_knows() {
    let tree = stack();
    let sections = prim_detail().layer_sections(Some(&tree));

    // A session layer: its id, kind and rank.
    assert_eq!(
        sections[0].header, "anon:session [session \u{00b7} 0]",
        "the strongest layer's header"
    );
    // An op's output layer: the same facts **and the generator's name**, so a section
    // says which op wrote it rather than an anonymous identifier.
    assert_eq!(
        sections[1].header, "anon:rusd_instances/0 [sublayer \u{00b7} 2] RusdInstances",
        "an op layer's section says which op"
    );
    // A root layer.
    assert_eq!(sections[2].header, "tufts.usda [root \u{00b7} 1]");

    // The pure header rule, asserted on its own: a layer the tree does not list gets its
    // identifier and nothing invented.
    assert_eq!(layer_header("gone.usda", None), "gone.usda");
}

#[test]
fn a_layer_the_tree_does_not_list_still_gets_its_section() {
    // `comp_node` can name a layer a `composition_tree` answer does not (it arrived
    // first, or the stack has changed since). The opinion is a fact, so the section is
    // drawn -- headed by the identifier, with no kind or rank guessed for it.
    let detail = prim_detail();
    let sections = detail.layer_sections(None);
    assert_eq!(sections.len(), 3, "every opinion is still a section");
    assert_eq!(sections[0].header, "anon:session");
    assert_eq!(sections[1].header, "anon:rusd_instances/0");
    assert!(
        sections.iter().all(|s| !s.header.contains('[')),
        "nothing is invented about a layer the tree does not list"
    );
}

// --- 2. a section's body is that layer's own spec ----------------------------

#[test]
fn a_sections_body_is_that_layers_specifier_and_attributes() {
    let tree = stack();
    let sections = prim_detail().layer_sections(Some(&tree));

    // The session's override: its specifier and its one attribute, and nothing else.
    assert_eq!(sections[0].specifier, "over");
    assert_eq!(
        sections[0]
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        ["xformOp:translate"],
        "a section carries its own layer's attributes, not the prim's"
    );
    assert_eq!(
        sections[0].lines(),
        ["over xformOp:translate = [3.0,0.0,7.0]"],
        "the body reads `specifier name = value`"
    );

    // The op's two attributes, the root's one: each is its own layer's list.
    assert_eq!(sections[1].lines().len(), 2);
    assert_eq!(sections[2].lines(), ["class typeName = Xform"]);

    // A spec that carries no specifier prints the name alone instead of a stray space.
    let bare = LayerSection {
        layer: "x.usda".to_owned(),
        header: "x.usda".to_owned(),
        specifier: String::new(),
        fields: vec![rusd_viewer::tree::NamedValue {
            name: "typeName".to_owned(),
            value: json!("Xform"),
        }],
    };
    assert_eq!(bare.lines(), ["typeName = Xform"]);
}

// --- 3. the panel draws it ---------------------------------------------------

/// The texts a headless pass of the detail panel painted.
fn painted_detail(detail: PrimDetail, tree: Option<CompositionTree>) -> Vec<String> {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(700.0, 600.0),
        )),
        ..Default::default()
    };
    let mut panel = TreePanel::new();
    if let Some(t) = &tree {
        let _ = panel.model.apply(Ok(json!({
            "layers": t.layers.iter().map(|l| json!({
                "id": l.id, "kind": l.kind, "strength": l.strength, "op": l.op,
                "op_name": l.op_name,
            })).collect::<Vec<Value>>(),
            "arcs": [], "prim_nodes": [], "variant_sets": [],
        })));
    }
    panel.detail = Some(NodeDetail::Prim(detail));
    let out = ctx.run_ui(raw, |root| {
        egui::CentralPanel::no_frame().show(root, |ui| {
            panel::detail_panel(ui, &panel);
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
fn the_panel_paints_one_section_per_layer_with_its_header() {
    let texts = painted_detail(prim_detail(), Some(stack()));
    for want in [
        "anon:session [session \u{00b7} 0]",
        "anon:rusd_instances/0 [sublayer \u{00b7} 2] RusdInstances",
        "tufts.usda [root \u{00b7} 1]",
    ] {
        assert!(
            texts.iter().any(|t| t.contains(want)),
            "a section's header must reach the screen ({want}): {texts:?}"
        );
    }
    assert!(
        texts.iter().any(|t| t.contains("over xformOp:translate")),
        "and its body: {texts:?}"
    );
    // The old subject line merged every layer into one count; the sectioned panel says
    // it as "how many layers wrote it", which is the same fact with a layer's noun.
    assert!(
        texts
            .iter()
            .any(|t| t.contains("\u{5c42}\u{5199}\u{8fc7}\u{5b83}")),
        "the summary counts layers: {texts:?}"
    );
}

#[test]
fn an_instance_row_click_can_reach_that_detail() {
    // The end-to-end shape v1.5.4 makes possible: a walk down to one instance, a click,
    // and a detail that speaks in layers. What the row gives the shell is the prim path
    // and the `comp_node` id; what comes back is this.
    let tree = stack();
    let rows = rusd_viewer::tree::roots(&tree);
    let instance = rows
        .iter()
        .flat_map(|r| r.children.iter())
        .flat_map(|c| c.children.iter())
        .find(|c| c.kind == rusd_viewer::tree::TreeNodeKind::Prim)
        .expect("the arc's instance row") as &TreeNode;
    let sel = TreeSelection::of(instance);
    assert_eq!(sel.prim.as_deref(), Some("/World/Tufts/i000000"));
    assert_eq!(sel.id, prim_detail().id, "the id the detail came back for");
    assert!(
        !prim_detail().layer_sections(Some(&tree)).is_empty(),
        "and the detail has layer sections to draw"
    );
}
