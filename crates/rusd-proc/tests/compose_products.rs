//! The two example documents, cooked and read the way the viewer reads them.
//!
//! Both examples are **operator graphs** (`.ops.toml`): the document names what to run and
//! the one USD each operator reads and writes. Every operator writes **its own USD** —
//! `samples.rusd`, `boxes.rusd`, … — and nothing stacks them: a stage holds one USD
//! (v1.13, 不同的东西在各自的 USD), so these tests open one product at a time and assert
//! what it holds, and assert that the composed stack holds nothing of them.

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
fn cook_example(name: &str) -> Vec<rusd_proc::CookOp> {
    let dir = examples();
    let dir_text = doc_dir();
    let text = std::fs::read_to_string(dir.join(name)).unwrap();
    let doc = rusd_core::syntax::parse_in(&text, Some(&dir_text)).unwrap();
    let stage = rusd_scene::lower(&doc).unwrap();
    let nodes = rusd_proc::nodes_from_doc(&doc, Some(&dir_text));
    let mut engine = rusd_proc::default_registry();
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
    // **Nothing is stacked** (v1.13): the stage the composition hands out holds no product.
    let mut composed = rusd_proc::compose_stack(&stage, &mut ops).unwrap();
    assert_eq!(
        paths_of(&composed).len(),
        0,
        "a stage holds one USD; the products are separate files"
    );
    let _ = &mut composed;
    ops
}

/// One product's own prims, read as its own stage.
fn prims_of(op: &rusd_proc::CookOp) -> Vec<String> {
    let layer = rusd_scene::document::layer_from_file(Path::new(&op.layer_path)).unwrap();
    let stack = Stage::builder().in_memory("check.rusd").unwrap();
    let root = stack.root_layer().identifier().to_owned();
    stack
        .insert_layer(&root, 0, layer, LayerOffset::IDENTITY)
        .unwrap();
    paths_of(&stack)
}

/// Whether `path` is one of `base`'s instance prims (`i` + six digits).
fn is_instance(path: &str, base: &str) -> bool {
    let leaf = path.rsplit('/').next().unwrap_or("");
    path.starts_with(&format!("{base}/"))
        && path.split('/').count() == base.split('/').count() + 1
        && leaf.len() == 7
        && leaf.starts_with('i')
        && leaf[1..].chars().all(|c| c.is_ascii_digit())
}

#[test]
fn segment_one_cooks_a_usd_per_operator() {
    let ops = cook_example("box_cluster.ops.toml");
    assert_eq!(ops.len(), 2, "segment one is two operators");
    let samples = ops.iter().find(|o| o.id == "/World/Samples").unwrap();
    let boxes = ops.iter().find(|o| o.id == "/World/Boxes").unwrap();
    assert!(
        samples.layer_path.ends_with("samples.rusd"),
        "the document names the file: {}",
        samples.layer_path
    );
    assert!(
        boxes.layer_path.ends_with("boxes.rusd"),
        "the document names the file: {}",
        boxes.layer_path
    );
    // The sampler's own USD: its output prim, the cloud it computed, and the one named
    // structure it declares — `domain`, the USD it reads.
    let paths = prims_of(samples);
    for wanted in ["/World/Samples", "/World/Samples/points", "/World/Samples/domain"] {
        assert!(paths.iter().any(|p| p == wanted), "{wanted} in {paths:?}");
    }
    // The instances' USD: its output prim, one prim per instance, and its one input.
    let paths = prims_of(boxes);
    assert_eq!(
        paths
            .iter()
            .filter(|p| is_instance(p, "/World/Boxes"))
            .count(),
        60,
        "one instance prim per point"
    );
    // An `instances` operator reads `{points, prototype}`, so both are named structures
    // under the prim it wrote.
    for wanted in ["/World/Boxes/points", "/World/Boxes/prototype"] {
        assert!(
            paths.iter().any(|p| p == wanted),
            "{wanted} in {paths:?}"
        );
    }
    assert!(
        !paths.iter().any(|p| p.starts_with("/World/Samples")),
        "another operator's things are not in this USD: {paths:?}"
    );
}

#[test]
fn segment_two_cooks_four_usds() {
    let ops = cook_example("box_scatter.ops.toml");
    assert_eq!(ops.len(), 4, "both segments cook their own two nodes");
    let instances = ops.iter().find(|o| o.id == "/World/Instances").unwrap();
    assert!(instances.layer_path.ends_with("instances.rusd"));
    let paths = prims_of(instances);
    assert_eq!(
        paths
            .iter()
            .filter(|p| is_instance(p, "/World/Instances"))
            .count(),
        40,
        "one instance prim per surface sample"
    );
}

/// The arcs a product authors are **relative** (v1.9): the material it instances is
/// `../box.rusd` — the file beside the `out/` it sits in. An absolute path would pin the
/// picture to one machine and one process's temp directory.
#[test]
fn a_product_names_its_inputs_relatively() {
    let ops = cook_example("box_cluster.ops.toml");
    let boxes = ops.iter().find(|op| op.id == "/World/Boxes").unwrap();
    let text = std::fs::read_to_string(&boxes.layer_path).unwrap();
    assert!(
        text.contains("../box.rusd:/World/Prototypes/Box"),
        "the prototype arc is the relative ../box.rusd:\n{text}"
    );
    assert!(
        !text.contains(&doc_dir()),
        "no product names the document's absolute directory:\n{text}"
    );
}

/// The cook writes one file per node and inserts nothing: every product is a `.rusd`.
#[test]
fn a_product_is_a_layer_file() {
    let ops = cook_example("box_cluster.ops.toml");
    for op in &ops {
        let path = Path::new(&op.layer_path);
        let layer = rusd_scene::document::layer_from_file(path).unwrap();
        assert!(layer.identifier().ends_with(".rusd"));
        let stack = Stage::builder().in_memory("check.rusd").unwrap();
        let root = stack.root_layer().identifier().to_owned();
        stack
            .insert_layer(&root, 0, layer, LayerOffset::IDENTITY)
            .unwrap();
        assert!(paths_of(&stack).iter().any(|p| p.starts_with(&op.id)));
    }
}
