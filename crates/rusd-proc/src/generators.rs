//! Built-in generators. M2 ships `RusdGrid` (a simple parametric grid) to exercise
//! the cook engine end-to-end; the city suite (StreetGrid/Building/BlockFill/Scatter)
//! registers here in M3, and the v1.5 two-op pipeline (`RusdSamplePoints` →
//! `RusdInstances`) in v1.5.

use openusd::gf;
use openusd::sdf::{self, AttributeSpec, PrimSpec, Specifier, Value as SdfValue, Variability};
use openusd::usd::Stage;
use rusd_core::Diagnostic;
use rusd_core::syntax::ParsedDocument;

use crate::cook::{CookEngine, CookStats, nodes_from_doc};
use crate::generator::{GenContext, Generator};

/// A parametric grid of spheres: count_x × count_y at `spacing`, each `radius`.
/// Pure parametric (no composed inputs); authors children under its anchor path.
pub struct RusdGrid;

impl Generator for RusdGrid {
    fn name(&self) -> &'static str {
        "RusdGrid"
    }

    fn eval(&self, ctx: &mut GenContext) -> Result<sdf::Layer, Vec<Diagnostic>> {
        let nx = ctx.param_u64("count_x", 4);
        let ny = ctx.param_u64("count_y", 4);
        let spacing = ctx.param_f64("spacing", 2.0) as f32;
        let radius = ctx.param_f64("radius", 0.5);
        let base = ctx.path.as_str().to_owned();

        let mut layer = sdf::Layer::new_anonymous("rusd_grid.usda");
        layer
            .edit(|l| {
                let d = l.data_mut();
                PrimSpec::new(d, base.as_str(), Specifier::Def, "Xform")?;
                for i in 0..nx {
                    for j in 0..ny {
                        let p = format!("{base}/c{i}_{j}");
                        PrimSpec::new(d, p.as_str(), Specifier::Def, "Sphere")?;

                        let rp = format!("{p}.radius");
                        let mut ra = AttributeSpec::new(
                            d,
                            rp.as_str(),
                            "double",
                            Variability::Uniform,
                            false,
                        )?;
                        ra.set_default(SdfValue::Double(radius))?;

                        let tp = format!("{p}.xformOp:translate");
                        let mut ta = AttributeSpec::new(
                            d,
                            tp.as_str(),
                            "point3f",
                            Variability::Uniform,
                            false,
                        )?;
                        ta.set_default(SdfValue::Vec3f(gf::vec3f(
                            i as f32 * spacing,
                            j as f32 * spacing,
                            0.0,
                        )))?;

                        let op = format!("{p}.xformOpOrder");
                        let mut oa = AttributeSpec::new(
                            d,
                            op.as_str(),
                            "token[]",
                            Variability::Uniform,
                            false,
                        )?;
                        oa.set_default(SdfValue::TokenVec(vec!["xformOp:translate".into()]))?;
                    }
                }
                Ok(())
            })
            .map_err(|e| {
                vec![Diagnostic::error("E_GEN", e.to_string()).at_prim(ctx.path.as_str())]
            })?;
        Ok(layer)
    }
}

/// A cook engine pre-loaded with the built-in generators.
pub fn default_registry() -> CookEngine {
    let mut e = CookEngine::new();
    e.register(Box::new(RusdGrid));
    e.register(Box::new(crate::city::RusdStreetGrid));
    e.register(Box::new(crate::city::RusdBuilding));
    e.register(Box::new(crate::city::RusdScatter));
    // v1.5: the two-op pipeline. Registered as two separate generators because the
    // intermediate point cloud is a real layer, which is what makes a prefix able to
    // stop between them (docs/PROTOCOL.md v1.5).
    e.register(Box::new(crate::pipeline::RusdSamplePoints));
    e.register(Box::new(crate::pipeline::RusdInstances));
    e
}

/// The result of expanding a document: the composed stage plus cook counters.
pub struct ExpandOutput {
    pub stage: Stage,
    pub stats: CookStats,
    pub diags: Vec<Diagnostic>,
    pub prim_count: usize,
    pub node_count: usize,
}

/// Lower a document to a Stage, then cook its generator nodes (D8/D9).
/// Validation is the caller's responsibility (run `rusd_scene::validate` first).
///
/// `doc_dir` is the directory of the file the document came from (v1.5.8): a
/// relative asset path in a generator parameter resolves against it. `None` is a
/// document handed in as text, which has no directory — such a path is then a
/// diagnostic, not a guess at the process's cwd.
pub fn expand_doc(
    doc: &ParsedDocument,
    doc_dir: Option<&str>,
) -> Result<ExpandOutput, Vec<Diagnostic>> {
    let stage = rusd_scene::lower(doc)?;
    let root_id = stage.root_layer().identifier().to_owned();
    let nodes = nodes_from_doc(doc, doc_dir);
    let mut engine = default_registry();
    let result = engine.cook(&stage, &root_id, &nodes);
    let prim_count = rusd_scene::prim_count(&stage);
    Ok(ExpandOutput {
        stage,
        stats: result.stats,
        diags: result.diags,
        prim_count,
        node_count: nodes.len(),
    })
}
