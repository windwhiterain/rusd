//! A `.rusd` product has to compose back into the same hierarchy it was written from.

#[cfg(test)]
mod tests {
    use openusd::sdf;
    use openusd::usd::{PrimPredicate, Stage};

    /// The document a `RusdInstances` product writes: one anchor prim with three
    /// children under it, each carrying a transform.
    const PRODUCT: &str = r#"
[World]
schema = ""

[World.Boxes]
schema = "Xform"

[World.Boxes.i000000]
schema = ""
"xformOp:translate" = [1.0, 2.0, 3.0]
"xformOpOrder" = ["xformOp:translate"]

[World.Boxes.i000001]
schema = ""
"xformOp:translate" = [4.0, 5.0, 6.0]
"xformOpOrder" = ["xformOp:translate"]

[World.Boxes.i000002]
schema = ""
"xformOp:translate" = [7.0, 8.0, 9.0]
"xformOpOrder" = ["xformOp:translate"]
"#;

    /// The document that names the op: its `/World/Boxes` is a generator spec with no
    /// children of its own, and it is lowered the same way the real document is.
    const DOC: &str = r#"
[World]
schema = "Xform"

[World.Boxes]
schema = "RusdInstances"
input_points = "/World/Samples/points"
prototype = "/World/Prototypes/Tuft"
"#;

    #[test]
    fn a_product_composes_with_its_children() {
        let layer = rusd_scene::document::layer_from_text(PRODUCT, None, "box.rusd").unwrap();
        // The layer itself has the hierarchy: the anchor's own `primChildren`.
        let data = layer.data();
        let children = data
            .try_field(&sdf::path("/World/Boxes").unwrap(), "primChildren")
            .unwrap()
            .map(|v| format!("{v:?}"))
            .unwrap_or_default();
        assert!(children.contains("i000000"), "primChildren: {children}");

        let stack = Stage::builder().in_memory("stack.usda").unwrap();
        let root = stack.root_layer().identifier().to_owned();
        stack
            .insert_layer(&root, 0, layer, sdf::LayerOffset::IDENTITY)
            .unwrap();
        let mut paths = Vec::new();
        stack
            .traverse(PrimPredicate::ALL, |p| paths.push(p.as_str().to_owned()))
            .unwrap();
        assert!(
            paths.iter().any(|p| p == "/World/Boxes/i000002"),
            "composed paths: {paths:?}"
        );
    }

    /// The same, but with the document's own spec for the anchor in the stack — the
    /// case the picture actually composes: the document says nothing about children,
    /// and the product is what has them.
    #[test]
    fn a_product_composes_over_the_documents_anchor() {
        let doc = rusd_core::syntax::parse(DOC).unwrap();
        let product = rusd_scene::document::layer_from_text(PRODUCT, None, "box.rusd").unwrap();
        let doc_stage = rusd_scene::lower(&doc).unwrap();
        let written = sdf::LayerRegistry::find_by_id("usda")
            .unwrap()
            .write_bytes(doc_stage.root_layer().data())
            .unwrap();
        let doc_layer = sdf::Layer::from_bytes("rusd_root.usda", written.bytes).unwrap();

        let stack = Stage::builder().in_memory("stack.usda").unwrap();
        let root = stack.root_layer().identifier().to_owned();
        stack
            .insert_layer(&root, 0, doc_layer, sdf::LayerOffset::IDENTITY)
            .unwrap();
        stack
            .insert_layer(&root, 0, product, sdf::LayerOffset::IDENTITY)
            .unwrap();
        let mut paths = Vec::new();
        stack
            .traverse(PrimPredicate::ALL, |p| paths.push(p.as_str().to_owned()))
            .unwrap();
        assert!(
            paths.iter().any(|p| p == "/World/Boxes/i000002"),
            "composed paths: {paths:?}"
        );
    }
}
