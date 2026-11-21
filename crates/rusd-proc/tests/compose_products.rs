//! The two example documents, cooked and read the way the viewer reads them.
//!
//! Both examples are **operator graphs** (`.ops.toml`): the document names what to run and
//! the one USD each operator reads and writes. Every operator writes **its own USD** —
//! `samples.rusd`, `boxes.rusd`, … — and nothing stacks them: a stage holds one USD
//! (v1.13, 不同的东西在各自的 USD), so these tests open one product at a time and assert
//! what it holds, and assert that the composed stack holds nothing of them.

use std::path::{Path, PathBuf};

use openusd::sdf::LayerOffset;
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

/// The cook writes its products into `examples/out`, and the arcs they author
/// (`../volume.rusd`) only resolve from there — so the tests that cook take turns
/// rather than racing each other over the same files. A product half-written by one
/// test is read by another as a layer missing a part, which is a schema failure that
/// says nothing about the code.
static COOK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Cook a document into `<examples>/out` and compose the stack a picture is made from.
fn cook_example(name: &str) -> Vec<rusd_proc::CookOp> {
    let _taking_turns = COOK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
    for wanted in ["/World/Samples", "/World/Samples/points"] {
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
    // **An output holds what the operator computed, not what it read** (v1.14): the
    // instances are the operator's work, and `points` / `prototype` are its inputs — they
    // stay where they were authored, named by the document.
    for unwanted in ["/World/Boxes/points", "/World/Boxes/prototype", "/World/Boxes/input"] {
        assert!(
            !paths.iter().any(|p| p == unwanted),
            "an input is not authored into the product: {unwanted} in {paths:?}"
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

/// **The operator's declared slot is checked, not assumed** (the user: operator 定义的
/// 输入输出 schema 要自动校验). A sampler declares its `domain` to be a `Gprim` — the
/// class that covers the `Cube` and the `Mesh` it can read — so an `Xform` wired there
/// is a mismatch, reported as `E_SCHEMA` against the node that declared it.
#[test]
fn an_input_of_the_wrong_schema_is_reported() {
    // An `Xform` is a transform, not geometry: nothing a sampler can draw from.
    let volume = examples().join("volume.rusd");
    let volume = volume.to_string_lossy().replace('\\', "/");
    let text = format!(
        "[operator.Bad]\noperator = \"sample_points\"\ndescription = \"bad\"\n\
         output = \"bad.rusd:/World/Bad\"\ninput = \"{volume}:/World\"\n\
         count = 4\nseed = 1\n"
    );
    let dir = std::env::temp_dir().join("rusd-schema-check");
    std::fs::create_dir_all(&dir).unwrap();
    let dir_text = dir.to_string_lossy().replace('\\', "/");
    let doc = rusd_core::syntax::parse_in(&text, Some(&dir_text)).unwrap();
    let stage = rusd_scene::lower(&doc).unwrap();
    let nodes = rusd_proc::nodes_from_doc(&doc, Some(&dir_text));
    let mut engine = rusd_proc::default_registry();
    engine.set_out_dir(Some(dir.join("out")));
    let result = engine.cook(&stage, &nodes);
    let seen: Vec<(String, String)> = result
        .diags
        .iter()
        .map(|d| (d.code.clone(), d.message.clone()))
        .collect();
    assert!(
        seen.iter().any(|(code, _)| code == "E_SCHEMA"),
        "an input of the wrong schema is E_SCHEMA: {seen:?}"
    );
}

/// **A script's graph cooks, end to end** (SPEC D15): the operators are Rust functions,
/// the graph is what their calls wrote, and the cook runs it without a diagnostic. The
/// material layers are authored by the script too, so every wire's type is true by
/// construction — nothing here is checked at runtime that rustc did not already check.
#[test]
fn a_scripts_graph_cooks_end_to_end() {
    use rusd_script::graph::{Graph, out};
    use rusd_script::ops::{self, InstancesIn, InstancesParams, SamplePointsIn, SamplePointsParams};

    let dir = std::env::temp_dir().join(format!("rusd-script-cook-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir_text = dir.to_string_lossy().replace('\\', "/");

    // The material: a box to sample, and a box to copy. Authored, so typed by construction.
    let mut volume = rusd_script::usd::Layer::new("volume.rusd");
    let domain = volume.cube("/World/Volume", 6.0);
    volume.write(&dir).unwrap();
    let mut proto = rusd_script::usd::Layer::new("box.rusd");
    let box_prim = proto.cube("/World/Prototypes/Box", 0.6);
    proto.write(&dir).unwrap();

    // The graph: two operator calls, the second wired to the first's output port.
    let mut g = Graph::new();
    g.default_prim("/World");
    let cloud = ops::sample_points(
        &mut g,
        out("Samples", "samples.rusd:/World/Samples"),
        SamplePointsIn::new(domain),
        SamplePointsParams { count: 60, seed: 11 },
    );
    let _boxes = ops::instances(
        &mut g,
        out("Boxes", "boxes.rusd:/World/Boxes"),
        InstancesIn::new(cloud.ports.points, box_prim),
        InstancesParams { max_prims: 5000, seed: 11, ..Default::default() },
    );
    g.write(&dir.join("scene.ops.toml")).unwrap();

    // Cook what the script wrote, exactly as the CLI would.
    let text = std::fs::read_to_string(dir.join("scene.ops.toml")).unwrap();
    let doc = rusd_core::syntax::parse_in(&text, Some(&dir_text)).unwrap();
    let stage = rusd_scene::lower(&doc).unwrap();
    let nodes = rusd_proc::nodes_from_doc(&doc, Some(&dir_text));
    let mut engine = rusd_proc::default_registry();
    engine.set_out_dir(Some(dir.join("out")));
    let result = engine.cook(&stage, &nodes);
    assert!(
        result.diags.iter().all(|d| d.severity != rusd_core::Severity::Error),
        "a script's graph cooks clean: {:?}",
        result.diags.iter().map(|d| d.message.clone()).collect::<Vec<_>>()
    );
    assert_eq!(result.ops.len(), 2);
    let boxes = result.ops.iter().find(|o| o.id == "/World/Boxes").unwrap();
    let paths = prims_of(boxes);
    assert_eq!(
        paths.iter().filter(|p| is_instance(p, "/World/Boxes")).count(),
        60,
        "one instance per sampled point"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// **Every example graph cooks clean.**
///
/// A `.ops.toml` names operators and the parameters each takes; the cook runs them and
/// writes one USD per operator. This is the guard that the operator vocabulary, the
/// generators and the assets the graphs name still agree — for every example, not just
/// the two the tests above spell out.
#[test]
fn every_example_graph_cooks() {
    let mut names: Vec<String> = std::fs::read_dir(examples())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".ops.toml"))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "examples/ holds graphs to cook");
    for name in names {
        let ops = cook_example(&name);
        assert!(!ops.is_empty(), "{name} cooks at least one operator");
    }
}

/// **A prototype USD cannot place is reported — and a subtype is not a mismatch** (the
/// user: 就用 USD 本身的 subtype 机制判断). An instances node copies its prototype at every
/// point, so the slot is declared `Xformable`: a class, not a type. The accepting half is
/// the example itself — `box_cluster.ops.toml` instances a `Cube`, which is not an `Xform`
/// but *is* an `Xformable`, and that cook is asserted clean above. This is the other half:
/// a `Material` is neither geometry nor a transform, so `IsA` says no where a comparison
/// against any fixed list of type names could only ever say "not in the list".
#[test]
fn a_prototype_of_the_wrong_schema_is_reported() {
    let _ = cook_example("box_cluster.ops.toml");
    // A prototype of its own, because its type is the whole subject of the test.
    let dir = std::env::temp_dir().join("rusd-prototype-check");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mat.rusd"),
        "[stage]\ndefault_prim = \"/World\"\n\n[World]\nschema = \"Xform\"\n\n\
         [World.Mat]\nschema = \"Material\"\n",
    )
    .unwrap();
    let mat = dir.join("mat.rusd").to_string_lossy().replace('\\', "/");
    // **The example's own graph with one string changed**: the prototype's file. Same
    // operators, same cloud, same order — so what differs in the answer is the type.
    let text = std::fs::read_to_string(examples().join("box_cluster.ops.toml"))
        .unwrap()
        .replace(
            "box.rusd:/World/Prototypes/Box",
            &format!("{mat}:/World/Mat"),
        );
    let dir_text = doc_dir();
    let doc = rusd_core::syntax::parse_in(&text, Some(&dir_text)).unwrap();
    let stage = rusd_scene::lower(&doc).unwrap();
    let nodes = rusd_proc::nodes_from_doc(&doc, Some(&dir_text));
    let mut engine = rusd_proc::default_registry();
    engine.set_out_dir(Some(dir.join("out")));
    let result = engine.cook(&stage, &nodes);
    let seen: Vec<(String, String)> = result
        .diags
        .iter()
        .map(|d| (d.code.clone(), d.message.clone()))
        .collect();
    assert!(
        seen.iter()
            .any(|(code, msg)| code == "E_SCHEMA" && msg.contains("prototype")),
        "a prototype that is not an Xformable is E_SCHEMA: {seen:?}"
    );
}

/// **A list of domains is one combined region** (list input): `count` is the total
/// number of samples across every domain, split by their measure — not `count` per
/// domain. Two cubes of different sizes still yield exactly `count` points, and the
/// instances node downstream copies one per point.
#[test]
fn a_list_of_domains_samples_count_total() {
    use rusd_script::graph::{Graph, out};
    use rusd_script::ops::{self, InstancesIn, InstancesParams, SamplePointsIn, SamplePointsParams};

    let dir = std::env::temp_dir().join(format!("rusd-list-domains-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir_text = dir.to_string_lossy().replace('\\', "/");

    let mut domains = rusd_script::usd::Layer::new("domains.rusd");
    let a = domains.cube("/World/A", 6.0);
    let b = domains.cube("/World/B", 2.0);
    domains.write(&dir).unwrap();
    let mut proto = rusd_script::usd::Layer::new("box.rusd");
    let box_prim = proto.cube("/World/Prototypes/Box", 0.6);
    proto.write(&dir).unwrap();

    let mut g = Graph::new();
    g.default_prim("/World");
    let cloud = ops::sample_points(
        &mut g,
        out("Samples", "samples.rusd:/World/Samples"),
        SamplePointsIn::many([a, b]),
        SamplePointsParams { count: 60, seed: 11 },
    );
    let _boxes = ops::instances(
        &mut g,
        out("Boxes", "boxes.rusd:/World/Boxes"),
        InstancesIn::new(cloud.ports.points, box_prim),
        InstancesParams { max_prims: 5000, seed: 11, ..Default::default() },
    );
    g.write(&dir.join("scene.ops.toml")).unwrap();

    let text = std::fs::read_to_string(dir.join("scene.ops.toml")).unwrap();
    let doc = rusd_core::syntax::parse_in(&text, Some(&dir_text)).unwrap();
    let stage = rusd_scene::lower(&doc).unwrap();
    let nodes = rusd_proc::nodes_from_doc(&doc, Some(&dir_text));
    let mut engine = rusd_proc::default_registry();
    engine.set_out_dir(Some(dir.join("out")));
    let result = engine.cook(&stage, &nodes);
    assert!(
        result.diags.iter().all(|d| d.severity != rusd_core::Severity::Error),
        "a list of domains cooks clean: {:?}",
        result.diags.iter().map(|d| d.message.clone()).collect::<Vec<_>>()
    );
    let boxes = result.ops.iter().find(|o| o.id == "/World/Boxes").unwrap();
    let paths = prims_of(boxes);
    assert_eq!(
        paths.iter().filter(|p| is_instance(p, "/World/Boxes")).count(),
        60,
        "count is the total across the list of domains, not per domain"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A list of clouds instances every point** (list input): an instances node wired
/// with several clouds concatenates them in the order named and copies one prototype
/// per point across the whole run — here 30 + 20 = 50.
#[test]
fn a_list_of_clouds_instances_every_point() {
    use rusd_script::graph::{Graph, out};
    use rusd_script::ops::{self, InstancesIn, InstancesParams, SamplePointsIn, SamplePointsParams};

    let dir = std::env::temp_dir().join(format!("rusd-list-clouds-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir_text = dir.to_string_lossy().replace('\\', "/");

    let mut domains = rusd_script::usd::Layer::new("domains.rusd");
    let a = domains.cube("/World/A", 6.0);
    domains.write(&dir).unwrap();
    let mut proto = rusd_script::usd::Layer::new("box.rusd");
    let box_prim = proto.cube("/World/Prototypes/Box", 0.6);
    proto.write(&dir).unwrap();

    let mut g = Graph::new();
    g.default_prim("/World");
    let c1 = ops::sample_points(
        &mut g,
        out("S1", "s1.rusd:/World/S1"),
        SamplePointsIn::new(a.clone()),
        SamplePointsParams { count: 30, seed: 1 },
    );
    let c2 = ops::sample_points(
        &mut g,
        out("S2", "s2.rusd:/World/S2"),
        SamplePointsIn::new(a),
        SamplePointsParams { count: 20, seed: 2 },
    );
    let _boxes = ops::instances(
        &mut g,
        out("Boxes", "boxes.rusd:/World/Boxes"),
        InstancesIn::many([c1.ports.points, c2.ports.points], box_prim),
        InstancesParams { max_prims: 5000, seed: 11, ..Default::default() },
    );
    g.write(&dir.join("scene.ops.toml")).unwrap();

    let text = std::fs::read_to_string(dir.join("scene.ops.toml")).unwrap();
    let doc = rusd_core::syntax::parse_in(&text, Some(&dir_text)).unwrap();
    let stage = rusd_scene::lower(&doc).unwrap();
    let nodes = rusd_proc::nodes_from_doc(&doc, Some(&dir_text));
    let mut engine = rusd_proc::default_registry();
    engine.set_out_dir(Some(dir.join("out")));
    let result = engine.cook(&stage, &nodes);
    assert!(
        result.diags.iter().all(|d| d.severity != rusd_core::Severity::Error),
        "a list of clouds cooks clean: {:?}",
        result.diags.iter().map(|d| d.message.clone()).collect::<Vec<_>>()
    );
    let boxes = result.ops.iter().find(|o| o.id == "/World/Boxes").unwrap();
    let paths = prims_of(boxes);
    assert_eq!(
        paths.iter().filter(|p| is_instance(p, "/World/Boxes")).count(),
        50,
        "one instance per point across every cloud in the list (30 + 20)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
