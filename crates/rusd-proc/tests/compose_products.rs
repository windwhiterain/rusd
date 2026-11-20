//! The two example documents, cooked and composed the way the viewer composes them.
//!
//! Both examples are **operator graphs** (`.ops.toml`): the document names what to run
//! and who reads whom, the cook writes one `.rusd` layer per operator into `<examples>/out`,
//! and the stack a picture is made from is the document root plus those files.
//!
//! The numbers are the documents' acceptance ones (v1.9): `box_cluster.ops.toml` is
//! segment one (volume → 60 boxes, 66 prims) and `box_scatter.ops.toml` is both segments in
//! one graph (60 boxes + 40 clusters of them, 2551 prims). Where the old `.rusd` examples
//! counted a prim per container the document itself authored, an operator graph authors
//! none: the counted prims are the operators' outputs alone.

use std::path::{Path, PathBuf};

use openusd::sdf::{self, LayerOffset};
use openusd::usd::{PrimPredicate, Stage};

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

fn paths_of(stage: &Stage) -> Vec<String> {
    let mut paths = Vec::new();
    let _ = stage.traverse(PrimPredicate::ALL, |p| paths.push(p.as_str().to_owned()));
    paths
}

/// The document's directory as the pipeline spells it (forward slashes, so the products and
/// the arcs written into them are the same strings on either platform).
fn doc_dir() -> String {
    examples().to_string_lossy().replace('\\', "/")
}

/// Cook a document into `<examples>/out` and compose the stack a picture is made from.
fn compose_example(name: &str) -> (Vec<String>, Vec<rusd_proc::CookOp>) {
    let dir = examples();
    let dir_text = doc_dir();
    let text = std::fs::read_to_string(dir.join(name)).unwrap();
    let doc = rusd_core::syntax::parse_in(&text, Some(&dir_text)).unwrap();
    let stage = rusd_scene::lower(&doc).unwrap();
    let nodes = rusd_proc::nodes_from_doc(&doc, Some(&dir_text));
    let mut engine = rusd_proc::default_registry();
    // Products land in `<doc_dir>/out` (v1.9): the arcs a product authors are relative
    // (`../box.rusd`, a bare `rusd_op_*.rusd`), so they only resolve from there.
    engine.set_out_dir(Some(dir.join("out")));
    let result = engine.cook(&stage, &nodes);
    assert!(
        result.diags.iter().all(|d| d.severity != rusd_core::Severity::Error),
        "cook diags: {:?}",
        result.diags.iter().map(|d| d.message.clone()).collect::<Vec<_>>()
    );
    let mut ops = result.ops;
    for op in &ops {
        assert!(
            op.layer_path.ends_with(".rusd"),
            "a product is a .rusd layer: {}",
            op.layer_path
        );
        let path = Path::new(&op.layer_path);
        assert!(path.is_file(), "{} is a file", op.layer_path);
        assert!(
            path.parent().map(|p| p.ends_with("out")).unwrap_or(false),
            "a product sits in the document's out/: {}",
            op.layer_path
        );
    }
    let composed = rusd_proc::compose_stack(&stage, &mut ops).unwrap();
    (paths_of(&composed), ops)
}

#[test]
fn segment_one_composes_its_boxes() {
    let (paths, ops) = compose_example("box_cluster.ops.toml");
    assert_eq!(ops.len(), 2, "segment one is two operators");
    assert_eq!(paths.len(), 66, "composed prims: {paths:?}");
    // **One USD input, one USD output** (v1.13): the product authors the node's one input
    // as a child prim of the prim it wrote, and nothing else.
    for schema in ["/World/Samples/input", "/World/Boxes/input"] {
        assert!(
            paths.iter().any(|p| p == schema),
            "{schema} is a prim: {paths:?}"
        );
    }
    // 60 instances of `box.rusd`'s prototype, each one prim.
    let inst = stack_prim("box_cluster.ops.toml", "/World/Boxes/i000000");
    assert_eq!(inst, Some("Cube".to_owned()), "the box is its prototype's type");
}

#[test]
fn segment_two_composes_its_clusters() {
    let (paths, ops) = compose_example("box_scatter.ops.toml");
    assert_eq!(ops.len(), 4, "both segments cook their own two nodes");
    assert_eq!(paths.len(), 2551, "composed prims: {}", paths.len());
    // One cluster is one box cluster: 60 boxes behind a same-stack reference to the
    // `/World/Boxes` layer the same graph cooked.
    let under_one_cluster = paths
        .iter()
        .filter(|p| p.starts_with("/World/Clusters/i000000/"))
        .count();
    assert_eq!(
        under_one_cluster, 61,
        "one cluster composes 60 boxes plus the cluster's own input"
    );
    let inst = stack_prim("box_scatter.ops.toml", "/World/Boxes/i000000");
    assert_eq!(inst, Some("Cube".to_owned()), "the box is its prototype's type");
}

/// The type a prim composes to, in `name`'s own stack.
fn stack_prim(name: &str, path: &str) -> Option<String> {
    let dir = examples();
    let dir_text = doc_dir();
    let text = std::fs::read_to_string(dir.join(name)).unwrap();
    let doc = rusd_core::syntax::parse_in(&text, Some(&dir_text)).unwrap();
    let stage = rusd_scene::lower(&doc).unwrap();
    let nodes = rusd_proc::nodes_from_doc(&doc, Some(&dir_text));
    let mut engine = rusd_proc::default_registry();
    engine.set_out_dir(Some(dir.join("out")));
    let mut ops = engine.cook(&stage, &nodes).ops;
    let composed = rusd_proc::compose_stack(&stage, &mut ops).unwrap();
    let prim = composed.prim(sdf::path(path).unwrap()).unwrap();
    prim.type_name().ok().flatten().map(|t| t.to_string())
}

/// The arcs a product authors are **relative** (v1.9): the material it names is
/// `../box.rusd` — the file beside the `out/` it sits in — and a product another operator
/// of the same graph wrote is a bare file name. An absolute path would pin the picture to
/// one machine and one process's temp directory.
#[test]
fn a_product_names_its_inputs_relatively() {
    let (_paths, ops) = compose_example("box_cluster.ops.toml");
    let boxes = ops
        .iter()
        .find(|op| op.id == "/World/Boxes")
        .expect("the boxes operator cooked");
    let text = std::fs::read_to_string(&boxes.layer_path).unwrap();
    assert!(
        text.contains("../box.rusd"),
        "the prototype arc is the relative ../box.rusd:\n{text}"
    );
    assert!(
        !text.contains(&doc_dir()),
        "no product names the document's absolute directory:\n{text}"
    );
}

/// The cook writes one file per node and inserts nothing: the stack is assembled from
/// those files, and every layer of it is a `.rusd`.
#[test]
fn a_product_is_a_layer_file() {
    let (_paths, ops) = compose_example("box_cluster.ops.toml");
    for op in &ops {
        let path = Path::new(&op.layer_path);
        let layer = rusd_scene::document::layer_from_file(path).unwrap();
        assert!(layer.identifier().ends_with(".rusd"));
        // Re-read into a stack, the product keeps its prims.
        let stack = Stage::builder().in_memory("check.rusd").unwrap();
        let root = stack.root_layer().identifier().to_owned();
        stack
            .insert_layer(&root, 0, layer, LayerOffset::IDENTITY)
            .unwrap();
        assert!(paths_of(&stack).iter().any(|p| p.starts_with(&op.id)));
    }
}
